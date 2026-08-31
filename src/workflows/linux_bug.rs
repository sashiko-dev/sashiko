// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Standalone Linux kernel Bug Pipeline.
//!
//! Processes candidate Linux kernel bugs individually through:
//! 1. Dedicated single-issue verification and High/Critical severity calibration.
//! 2. Subsystem & file-localized fast vector candidate retrieval (Top N = 20).
//! 3. LLM deduplication confirmation against known Linux kernel bugs.
//! 4. Standalone LKML-style defect description generation for newly discovered bugs.
//! 5. Database persistence and review linking.

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tracing::info;

use crate::ai::session::{LlmSession, SessionRunner, ValidationError};
use crate::ai::vector_search::{
    DEFAULT_SIMILARITY_THRESHOLD, DEFAULT_TOP_CANDIDATES, extract_bug_vector, find_top_candidates,
};
use crate::ai::{AiProvider, AiResponse, AiResponseFormat, AiTool};
use crate::db::{Bug, Database, NewBug, Severity};
use crate::toolbox::ToolBox;

/// Input payload representing a candidate Linux kernel defect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BugInput {
    pub problem: String,
    pub reasoning: String,
    pub locations: Option<Value>,
    #[serde(default)]
    pub subsystems: Vec<String>,
    pub source_files: Vec<String>,
    pub commit_sha: Option<String>,
    pub patchset_id: Option<i64>,
    pub patch_id: Option<i64>,
    pub baseline_sha: Option<String>,
}

/// The result of processing a candidate Linux kernel bug through the pipeline.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BugOutcome {
    /// The candidate bug was discarded (invalid, false positive, or Low/Medium severity).
    Discarded {
        reason: String,
        logs: Option<String>,
    },
    /// The bug was confirmed as an identical duplicate of a known Linux kernel bug.
    Duplicate {
        existing_bug: Bug,
        reasoning: String,
        logs: Option<String>,
    },
    /// The bug was confirmed as a newly discovered Linux kernel bug.
    NewlyDiscovered { bug: Bug },
}

impl std::fmt::Display for BugOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BugOutcome::NewlyDiscovered { bug } => {
                write!(
                    f,
                    "newly discovered bug {} ({}) [severity: {}]",
                    bug.id, bug.bugid, bug.severity
                )
            }
            BugOutcome::Duplicate {
                existing_bug,
                reasoning,
                ..
            } => {
                write!(
                    f,
                    "duplicate of bug {} ({}) - {}",
                    existing_bug.id, existing_bug.bugid, reasoning
                )
            }
            BugOutcome::Discarded { reason, .. } => {
                write!(f, "discarded - {}", reason)
            }
        }
    }
}

impl std::fmt::Debug for BugOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BugOutcome::Discarded { reason, logs } => f
                .debug_struct("Discarded")
                .field("reason", reason)
                .field(
                    "logs",
                    &logs.as_ref().map(|l| format!("<{} bytes>", l.len())),
                )
                .finish(),
            BugOutcome::Duplicate {
                existing_bug,
                reasoning,
                logs,
            } => f
                .debug_struct("Duplicate")
                .field("existing_bug_id", &existing_bug.id)
                .field("existing_bug_bugid", &existing_bug.bugid)
                .field("reasoning", reasoning)
                .field(
                    "logs",
                    &logs.as_ref().map(|l| format!("<{} bytes>", l.len())),
                )
                .finish(),
            BugOutcome::NewlyDiscovered { bug } => f
                .debug_struct("NewlyDiscovered")
                .field("id", &bug.id)
                .field("bugid", &bug.bugid)
                .field("status", &bug.status)
                .field("problem", &bug.problem)
                .field("severity", &bug.severity)
                .finish(),
        }
    }
}

// ---------------------------------------------------------------------------
// 1. Verification Session
// ---------------------------------------------------------------------------
// 1. Verification Session
// ---------------------------------------------------------------------------

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct VerificationJson {
    pub verification_reasoning: String,
    pub is_false_positive: bool,
    pub refutation_evidence: Option<String>,
    pub impact_severity: Option<String>,
    pub relevant_code_locations: Option<Value>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct NormalizationJson {
    pub canonical_title: String,
    pub canonical_description: String,
    pub affected_source_files: Vec<String>,
    #[serde(default)]
    pub affected_symbols: Option<Vec<String>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct DedupJson {
    pub is_duplicate: bool,
    pub duplicate_of_id: Option<i64>,
    pub reasoning: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct TracingJson {
    pub introducing_commit_sha: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct SeverityJson {
    pub severity: String,
    pub severity_explanation: String,
}

struct VerifySession<'a> {
    title: &'a str,
    description: &'a str,
    subsystem: &'a str,
    affected_files: &'a [String],
    locations: Option<&'a Value>,
    master_sha: String,
    tools: Option<Arc<ToolBox>>,
    context_tag: Option<String>,
    prefetched_context: String,
}

#[async_trait]
impl LlmSession for VerifySession<'_> {
    type Output = VerificationJson;

    fn system_prompt(&self) -> String {
        let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();
        format!(
            "Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references strictly on this current date.\n\n\
            You are an expert Linux kernel maintainer. Your task is to rigorously verify a candidate Linux kernel defect or vulnerability against the top-of-trunk of Linus Torvalds' main Linux kernel tree.\n\
            Use available tools (git_read_files, git_grep, git_blame, git_log, git_show, git_diff) to inspect the mainline codebase, verify call chains, and confirm whether this defect exists.\n\n\
            CRITICAL VALIDATION FILTER: You must assess if the bug is genuine. Do not give the code the benefit of the doubt. To mark an issue as a false positive (is_false_positive=true), you must find concrete proof in the local codebase that the described conditions are impossible, unreachable, or already safely handled. If you cannot prove it is false, verify the code locations and provide your step-by-step reasoning in verification_reasoning."
        )
    }

    fn initial_user_prompt(&self) -> String {
        let loc_str = self
            .locations
            .and_then(|v| serde_json::to_string_pretty(v).ok())
            .unwrap_or_else(|| "[]".to_string());

        let prefetch_block = if self.prefetched_context.is_empty() {
            String::new()
        } else {
            format!(
                "\n<pre_fetched_context>\nThe following context was automatically pre-fetched from mainline at commit `{}`. It contains the source code around the reported locations.\nIf this context is sufficient to verify the defect, render your verdict directly without redundant tool calls.\n\n{}\n</pre_fetched_context>\n",
                self.master_sha, self.prefetched_context
            )
        };

        let files_str = if self.affected_files.is_empty() {
            String::new()
        } else {
            format!("Affected Files: {}\n", self.affected_files.join(", "))
        };

        format!(
            "Candidate Defect to Verify:
Title: {title}
Subsystem: {subsystem}
{files_str}Description:
{description}
Locations:
{locations}
{prefetch_block}
Task:
1. Verify the problem against the mainline code shown above and top-of-trunk of Linus's main tree (commit `{master_sha}`). IMPORTANT: Use this exact `{master_sha}` SHA in any tool calls instead of `HEAD` or `master` to check the actual top-of-trunk.
2. Scope your verification to the relevant functions and code blocks. Do not wander across unrelated drivers or files.
3. Determine if the issue is a genuine, reachable defect in the codebase.
4. If the defect is hallucinated, or a false positive that you can prove based on the code is impossible or safely handled, set \"is_false_positive\": true, provide concrete proof in \"refutation_evidence\", and summarize in \"verification_reasoning\".
5. If it is a confirmed bug, set \"is_false_positive\": false, \"refutation_evidence\": null, provide your step-by-step proof in \"verification_reasoning\", carry forward and refine the verified code locations in \"relevant_code_locations\", and optionally suggest an \"impact_severity\" (\"Low\", \"Medium\", \"High\", \"Critical\", or \"Unknown\").

EFFICIENCY LIMIT REQUIREMENT: Limit your investigation to the core defect. Do not trace unneeded macro definitions or unrelated history. You have a strict limit on tool calls; be extremely efficient instead of wandering the history.

Return ONLY a valid JSON object matching this schema:
{{
  \"verification_reasoning\": \"1. Call chain... 2. Condition...\",
  \"is_false_positive\": false,
  \"refutation_evidence\": null,
  \"impact_severity\": \"High\",
  \"relevant_code_locations\": [ {{\"file\": \"path/to/file.c\", \"function_or_symbol\": \"function_name\", \"line\": 123}} ]
}}",
            master_sha = self.master_sha,
            title = self.title,
            subsystem = self.subsystem,
            description = self.description,
            locations = loc_str,
            prefetch_block = prefetch_block,
        )
    }

    fn tools(&self) -> Option<Vec<AiTool>> {
        self.tools.as_ref().map(|t| t.get_declarations_generic())
    }

    async fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        if let Some(ref tools) = self.tools {
            tools.call(name, args).await
        } else {
            bail!("Tool execution requested but no toolbox available");
        }
    }

    fn response_format(&self) -> Option<AiResponseFormat> {
        Some(AiResponseFormat::Json { schema: None })
    }

    fn context_tag(&self) -> Option<String> {
        self.context_tag.clone()
    }

    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
        let text = response.content.as_deref().unwrap_or("");
        let parsed: VerificationJson = crate::workflow::output::parse_json_from_text(text)
            .map_err(|e| ValidationError::FormatViolation(e.to_string()))?;
        Ok(parsed)
    }
}

// ---------------------------------------------------------------------------
// 2. Normalization Session
// ---------------------------------------------------------------------------

struct NormalizeSession<'a> {
    problem: &'a str,
    reasoning: &'a str,
    locations: &'a str,
    master_sha: &'a str,
    maintainers_hint: Option<String>,
    tools: Option<Arc<ToolBox>>,
    context_tag: Option<String>,
}

#[async_trait]
impl LlmSession for NormalizeSession<'_> {
    type Output = NormalizationJson;

    fn system_prompt(&self) -> String {
        format!(
            "You are an expert Linux kernel maintainer and technical editor. Your role is to normalize a candidate Linux kernel defect into canonical form.\n\
            You must standardize the defect's title, describe the technical substance, and identify the verified affected source files and symbols.\n\
            The target codebase is Linus Torvalds' mainline Linux kernel tree at top-of-trunk commit `{master_sha}`.\n\
            Use available tools (git_read_files, git_log, git_grep) to inspect the codebase at `{master_sha}`. Specifically:\n\
            - Use git_read_files with revision: \"{master_sha}\" or git_grep to inspect source code and identify affected files and symbols.\n\
            - Use git_log with range: \"{master_sha}\" on affected files to observe the conventional subsystem commit prefix used by maintainers (e.g. 'btrfs:', 'net:', 'mm:', 'drm/i915:').",
            master_sha = self.master_sha
        )
    }

    fn initial_user_prompt(&self) -> String {
        let hint_section = self
            .maintainers_hint
            .as_ref()
            .map(|h| format!("\n{}\n", h))
            .unwrap_or_default();

        format!(
            "Candidate Bug Details:
Original Problem: {problem}
Reasoning: {reasoning}
Reported Locations:
{locations}
{hint}
Target Mainline Commit: {master_sha}

Task:
1. Determine the conventional subsystem commit prefix for this defect (e.g. 'btrfs', 'net', 'net/sched', 'bpf', 'drm/i915', 'sched', 'mm'). Use git_log on the affected file(s) at revision '{master_sha}' to observe the standard commit prefix used by kernel maintainers.
2. Formulate a canonical title matching Linux kernel patch conventions: '<subsystem_prefix>: <defect or broken invariant in function_name()>' (strict limit of under 80 characters, NO backticks, NO markdown).
   - CRITICAL: This is a bug report title describing an existing defect, NOT a patch or commit title. Do NOT use patch/fix action verbs like 'fix', 'resolve', 'prevent', 'avoid', or 'handle'. State the defect directly (e.g. 'btrfs: use-after-free in btrfs_cleanup_ordered_extents()' or 'iommu/rockchip: array compaction flaw in rk_iommu_probe()', NEVER 'iommu/rockchip: fix array compaction flaw in rk_iommu_probe()').
3. Provide a detailed, structured canonical description:
   - Trigger / Preconditions: Specific conditions, inputs, or states required to trigger the defect. If reproducible only under special circumstances (e.g. on a 32-bit machine, specific architecture, or configuration), highlight it first (e.g. 'On a 32-bit architecture...').
   - Call Chain / Execution Path: Detail the complete chain of events/calls (e.g. func_a() -> func_b() -> func_c()) leading up to the problem.
   - Failure Mechanism: Detail the exact root cause and how the fault or resource corruption occurs.
   - Impact: Consequence of the failure (e.g. UAF, memory leak, deadlock, null pointer dereference, crash).
4. Verify and list the affected source files and symbols in the mainline tree at commit '{master_sha}'.

Return ONLY a valid JSON object matching this schema:
{{
  \"canonical_title\": \"btrfs: use-after-free in btrfs_cleanup_ordered_extents()\",
  \"canonical_description\": \"Trigger / Preconditions: ...\\nFailure Mechanism: ...\\nImpact: ...\",
  \"affected_source_files\": [\"fs/btrfs/ordered-data.c\"],
  \"affected_symbols\": [\"btrfs_cleanup_ordered_extents\"]
}}",
            problem = self.problem,
            reasoning = self.reasoning,
            locations = self.locations,
            hint = hint_section,
            master_sha = self.master_sha
        )
    }

    fn tools(&self) -> Option<Vec<AiTool>> {
        self.tools.as_ref().map(|t| t.get_declarations_generic())
    }

    async fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        if let Some(ref tools) = self.tools {
            tools.call(name, args).await
        } else {
            bail!("Tool execution requested but no toolbox available");
        }
    }

    fn response_format(&self) -> Option<AiResponseFormat> {
        Some(AiResponseFormat::Json { schema: None })
    }

    fn context_tag(&self) -> Option<String> {
        self.context_tag.clone()
    }

    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
        let text = response.content.as_deref().unwrap_or("");
        let parsed: NormalizationJson = crate::workflow::output::parse_json_from_text(text)
            .map_err(|e| ValidationError::FormatViolation(e.to_string()))?;
        let title = parsed.canonical_title.trim();
        if title.is_empty() {
            return Err(ValidationError::FormatViolation(
                "canonical_title cannot be empty".into(),
            ));
        }
        if parsed.affected_source_files.is_empty() {
            return Err(ValidationError::FormatViolation(
                "affected_source_files cannot be empty".into(),
            ));
        }

        let (prefix, desc) = if let Some((p, rest)) = title.split_once(':') {
            (p.trim(), rest.trim().to_ascii_lowercase())
        } else {
            return Err(ValidationError::FormatViolation(format!(
                "canonical_title must follow the format '<subsystem_prefix>: <defect description>'. Got: '{}'",
                title
            )));
        };

        if prefix.is_empty() {
            return Err(ValidationError::FormatViolation(
                "subsystem prefix in canonical_title cannot be empty".into(),
            ));
        }

        const FORBIDDEN_PREFIXES: &[&str] = &[
            "fix ",
            "fixes ",
            "fixing ",
            "resolve ",
            "resolves ",
            "resolving ",
            "prevent ",
            "prevents ",
            "preventing ",
            "avoid ",
            "avoids ",
            "avoiding ",
        ];
        if FORBIDDEN_PREFIXES
            .iter()
            .any(|forbidden| desc.starts_with(forbidden))
        {
            return Err(ValidationError::FormatViolation(format!(
                "canonical_title must describe the defect rather than a patch/fix; do not use patch action verbs like 'fix', 'prevent', or 'avoid' after the subsystem prefix. Got: '{}'",
                title
            )));
        }

        Ok(parsed)
    }
}

/// Extracts the subsystem or prefix before the first colon in a canonical title.
pub fn extract_title_prefix(title: &str) -> &str {
    if let Some((prefix, _)) = title.split_once(':') {
        prefix.trim()
    } else {
        title.trim()
    }
}

/// Extracts coarse directory-based subsystem prefixes (e.g. "fs/btrfs" or "net/core") from file paths.
pub fn extract_directory_subsystems(files: &[String]) -> Vec<String> {
    let mut subs = Vec::new();
    for file in files {
        let parts: Vec<&str> = file.split('/').collect();
        let sub = if parts.len() >= 2 {
            format!("{}/{}", parts[0], parts[1])
        } else if !parts.is_empty() && !parts[0].is_empty() {
            parts[0].to_string()
        } else {
            continue;
        };
        if !subs.contains(&sub) {
            subs.push(sub);
        }
    }
    if subs.is_empty() {
        vec!["kernel".to_string()]
    } else {
        subs
    }
}

// ---------------------------------------------------------------------------
// 3. Deduplication Confirmation Session
// ---------------------------------------------------------------------------

struct DedupSession<'a> {
    candidate_problem: &'a str,
    candidate_locations: Option<&'a Value>,
    candidate_subsystems: &'a [String],
    known_candidates: &'a [Bug],
    context_tag: Option<String>,
}

#[async_trait]
impl LlmSession for DedupSession<'_> {
    type Output = DedupJson;

    fn system_prompt(&self) -> String {
        "You are an expert Linux kernel maintainer responsible for defect tracking and deduplication.\n\
        You will compare a newly verified Linux kernel bug against a list of known Linux kernel bugs in the codebase.\n\
        Determine if the newly verified bug is an identical duplicate (describing the same root cause in the same code path/function) of one of the candidate bugs.\n\
        IMPORTANT: Bugs that have the same root cause but different consequences (e.g. wrong synchronization leads to a data race which might look like a memory leak or use-after-free crash) should be considered a duplicate and be merged. Rule of thumb: if fixing one issue will resolve the other issue, it's the same bug.\n\
        Output raw JSON only."
            .to_string()
    }

    fn initial_user_prompt(&self) -> String {
        let loc_str = self
            .candidate_locations
            .map(|v| {
                let mut stripped = v.clone();
                if let Some(arr) = stripped.as_array_mut() {
                    for obj in arr {
                        if let Some(map) = obj.as_object_mut() {
                            map.remove("line");
                        }
                    }
                }
                serde_json::to_string_pretty(&stripped).unwrap_or_else(|_| "[]".to_string())
            })
            .unwrap_or_else(|| "[]".to_string());

        let mut known_list = String::new();
        for bug in self.known_candidates {
            let files_str = bug
                .source_files
                .as_ref()
                .map(|f| f.join(", "))
                .unwrap_or_default();
            let subs_str = if bug.subsystems.is_empty() {
                "unknown".to_string()
            } else {
                bug.subsystems.join(", ")
            };
            known_list.push_str(&format!(
                "- Bug ID {}: [BugID: {}] [Severity: {}] [Subsystems: {}]\n  Problem: {}\n  Affected Files: {}\n\n",
                bug.id,
                bug.bugid,
                bug.severity.as_str(),
                subs_str,
                bug.problem,
                files_str
            ));
        }

        let cand_subs = if self.candidate_subsystems.is_empty() {
            "unknown".to_string()
        } else {
            self.candidate_subsystems.join(", ")
        };

        format!(
            "Newly Verified Linux Kernel Bug:\n\
            Problem: {}\n\
            Subsystems: {}\n\
            Locations:\n{}\n\n\
            Candidate Known Bugs in Database:\n\
            {}\n\
            Task:\n\
            Determine if the newly verified bug is an identical duplicate of ANY of the candidate bugs listed above.\n\
            - Root cause matching: Bugs that have the same root cause but different consequences (e.g. wrong synchronization leads to a data race which might look like a memory leak or use-after-free crash) should be considered a duplicate and be merged.\n\
            - Rule of thumb: If fixing one issue will resolve the other issue, it's the same bug.\n\
            - If it matches a candidate bug, set \"is_duplicate\": true, set \"duplicate_of_id\": <ID of matched bug>, and explain in \"reasoning\".\n\
            - If it is a distinct or newly discovered issue, set \"is_duplicate\": false, \"duplicate_of_id\": null, and explain in \"reasoning\".\n\n\
            Return ONLY a valid JSON object matching:\n\
            {{\n\
              \"is_duplicate\": true,\n\
              \"duplicate_of_id\": 12,\n\
              \"reasoning\": \"Both describe the same missing unlock in foo_cleanup()\"\n\
            }}",
            self.candidate_problem, cand_subs, loc_str, known_list
        )
    }

    fn response_format(&self) -> Option<AiResponseFormat> {
        Some(AiResponseFormat::Json { schema: None })
    }

    fn context_tag(&self) -> Option<String> {
        self.context_tag.clone()
    }

    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
        let text = response.content.as_deref().unwrap_or("");
        let parsed: DedupJson = crate::workflow::output::parse_json_from_text(text)
            .map_err(ValidationError::FormatViolation)?;

        if parsed.is_duplicate {
            if let Some(dup_id) = parsed.duplicate_of_id {
                if !self.known_candidates.iter().any(|b| b.id == dup_id) {
                    return Err(ValidationError::FormatViolation(format!(
                        "duplicate_of_id {} is not among candidate IDs {:?}",
                        dup_id,
                        self.known_candidates
                            .iter()
                            .map(|b| b.id)
                            .collect::<Vec<_>>()
                    )));
                }
            } else {
                return Err(ValidationError::FormatViolation(
                    "is_duplicate is true but duplicate_of_id was null or omitted".into(),
                ));
            }
        }

        Ok(parsed)
    }
}

// ---------------------------------------------------------------------------
// 4. Tracing Session (Enrichment)
// ---------------------------------------------------------------------------

struct TracingSession<'a> {
    input: &'a BugInput,
    master_sha: String,
    verification_reasoning: String,
    relevant_locations: String,
    tools: Option<Arc<ToolBox>>,
    context_tag: Option<String>,
}

#[async_trait]
impl LlmSession for TracingSession<'_> {
    type Output = TracingJson;

    fn system_prompt(&self) -> String {
        "You are an expert Linux kernel maintainer. Your task is to determine the exact commit that introduced a verified kernel defect.\n\
        Use available tools (git_blame, git_log, git_diff, git_show, git_read_files) to inspect history backwards and confirm which commit actually introduced the buggy logic rather than just refactoring lines.\n\
        EFFICIENCY LIMIT REQUIREMENT: You have a strict limit on tool calls; be extremely efficient instead of wandering the history.".to_string()
    }

    fn initial_user_prompt(&self) -> String {
        format!(
            "Verified Vulnerability:
Problem: {problem}
Reasoning: {reasoning}
Verification Evidence: {ver_reasoning}
Relevant Code Locations:
{locations}

Task:
1. Use `git_blame`, `git_log`, `git_diff`, and `git_show` to determine the exact commit that introduced the problem. Set \"introducing_commit_sha\" to the exact 40-character commit SHA, or null if you cannot conclusively determine it within a reasonable number of queries. Use the provided `{master_sha}` in your tool calls instead of `HEAD`.

Return ONLY a valid JSON object matching this schema:
{{
  \"introducing_commit_sha\": \"abc123456789012345678901234567890123456789\"
}}",
            master_sha = self.master_sha,
            problem = self.input.problem,
            reasoning = self.input.reasoning,
            ver_reasoning = self.verification_reasoning,
            locations = self.relevant_locations
        )
    }

    fn tools(&self) -> Option<Vec<AiTool>> {
        self.tools.as_ref().map(|t| t.get_declarations_generic())
    }

    async fn call_tool(
        &mut self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value> {
        if let Some(ref tools) = self.tools {
            tools.call(name, args).await
        } else {
            bail!("Tool execution requested but no toolbox available");
        }
    }

    fn response_format(&self) -> Option<AiResponseFormat> {
        Some(AiResponseFormat::Json { schema: None })
    }

    fn context_tag(&self) -> Option<String> {
        self.context_tag.clone()
    }

    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
        let text = response.content.as_deref().unwrap_or("");
        let parsed: TracingJson = crate::workflow::output::parse_json_from_text(text)
            .map_err(|e| ValidationError::FormatViolation(e.to_string()))?;

        #[allow(clippy::collapsible_if)]
        if let Some(tb) = &self.tools {
            if let Some(sha) = &parsed.introducing_commit_sha {
                let output = std::process::Command::new("git")
                    .current_dir(tb.get_worktree_path())
                    .args(["cat-file", "-e", &format!("{}^{{commit}}", sha)])
                    .output()
                    .map_err(|e| ValidationError::FormatViolation(e.to_string()))?;

                if !output.status.success() {
                    return Err(ValidationError::FormatViolation(format!(
                        "The SHA '{}' provided for introducing_commit_sha is invalid or not a commit.",
                        sha
                    )));
                }
            }
        }

        Ok(parsed)
    }
}

// ---------------------------------------------------------------------------
// 5. Severity & Impact Estimation Session (Enrichment)
// ---------------------------------------------------------------------------

struct SeveritySession<'a> {
    canonical_title: &'a str,
    canonical_description: &'a str,
    locations: &'a str,
    context_tag: Option<String>,
}

#[async_trait]
impl LlmSession for SeveritySession<'_> {
    type Output = SeverityJson;

    fn system_prompt(&self) -> String {
        format!(
            "{}\n\nAssess the severity and impact of a verified defect in the Linux kernel following the severity definitions and calibration guidance above.\n\
            Output raw JSON only matching the schema.",
            crate::prompt_bundle::kernel_severity_guide()
        )
    }

    fn initial_user_prompt(&self) -> String {
        format!(
            "Verified Linux Kernel Defect:
Title: {title}
Description:
{description}
Code Locations:
{locations}

Task:
Assess the severity of this defect and provide an explanation following the severity levels and calibration guidance above.
State your reasoning (consequence, triggering path, reachability) at the start of severity_explanation so the label is auditable.

Return ONLY a valid JSON object matching:
{{
  \"severity\": \"Low\" | \"Medium\" | \"High\" | \"Critical\" | \"Unknown\",
  \"severity_explanation\": \"Explain consequence, triggering path, reachability, attack prerequisites, required privileges, and blast radius...\"
}}",
            title = self.canonical_title,
            description = self.canonical_description,
            locations = self.locations,
        )
    }

    fn response_format(&self) -> Option<AiResponseFormat> {
        Some(AiResponseFormat::Json { schema: None })
    }

    fn context_tag(&self) -> Option<String> {
        self.context_tag.clone()
    }

    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
        let text = response.content.as_deref().unwrap_or("");
        let parsed: SeverityJson = crate::workflow::output::parse_json_from_text(text)
            .map_err(|e| ValidationError::FormatViolation(e.to_string()))?;
        let _ = Severity::from_str(&parsed.severity);
        Ok(parsed)
    }
}

// ---------------------------------------------------------------------------
// 6. Standalone Description Generation Session (Enrichment)
// ---------------------------------------------------------------------------

struct ReportSession<'a> {
    problem: &'a str,
    severity: &'a str,
    canonical_description: &'a str,
    severity_explanation: &'a str,
    locations: Option<&'a Value>,
    introduced_in_commit: Option<&'a str>,
    tools: Option<Arc<ToolBox>>,
    context_tag: Option<String>,
    prefetched_context: String,
}

#[async_trait]
impl LlmSession for ReportSession<'_> {
    type Output = String;

    fn system_prompt(&self) -> String {
        r#"You are an expert Linux kernel maintainer generating a comprehensive, standalone technical description for a defect discovered in the Linux kernel codebase.
Generate a technically thorough, precise, and objective explanation of the problem, suitable for upstream LKML submission.

CRITICAL RULES:
1. No titles, headers, or subject lines. NEVER start with "Defect Report:", "Report:", "Issue:", "Description:", or the bug title. Start immediately with the technical description of the defect.
2. Lead with the symptom and broken invariant:
   - The very first sentence must state what goes wrong, in which function/subsystem, and under what condition.
   - If the problem is reproducible only under special circumstances (e.g. on a 32-bit architecture, specific config options, or specific hardware), highlight it first at the very beginning of the opening sentence (e.g. "On a 32-bit architecture, ...").
3. Anti-Lecture Directive (Maintainer Audience):
   - Assume the reader is an experienced Linux kernel maintainer.
   - Do NOT explain basic kernel mechanics (how RCU works, what spinlocks do, what workqueues or slab caches are). Focus strictly on the broken contract or invariant in this code.
4. Conciseness and technical precision:
   - Keep the entire description concise: ideally 1 to 2 cohesive paragraphs.
   - Do NOT output vertical ASCII call-trees (e.g. func_a() -> func_b() -> func_c()). Refer to callers or flow inline within prose (e.g. "when called from func_a()").
   - Clarify the precise root cause and failure mechanism (e.g. memory leak on error path, use-after-free, deadlock, null pointer dereference, race condition, integer overflow).
   - State the immediate technical consequence directly (e.g. memory leak, panic, use-after-free, deadlock). Avoid generic security hyperbole.
   - Argue once: do NOT include conversational filler ("While reviewing...", "I noticed that...", "In the Linux kernel..."), defensive rationalizations, or concluding summaries.
5. No fix recommendations, patches, or remediation advice. Do NOT suggest how to resolve the issue or how to write a patch. Describe ONLY the bug itself.
6. Adaptive kernel code snippet guidelines:
   - Snippets are optional: If the defect is an interface contract violation, an unhandled state transition, or an architectural mismatch that is clear from prose alone, OMIT code snippets entirely. Do not force code when prose suffices.
   - Paired actions rule (allocations and releases, locks, refcounts): For memory leaks or paired resource lifecycle bugs, the snippet MUST include BOTH the allocation or acquisition site (e.g. kzalloc or mutex_lock) AND the error or exit path where release was missed. Never show only the exit path without showing what was allocated or acquired. Larger snippets (10-20 lines) are justified and encouraged here to capture full context.
   - Targeted snippets (4-8 lines): Use for localized expressions, boundary errors, or direct dereferences before null checks.
   - Formatting: When a code snippet is used, format it as:
     // <filepath>:<start_line>-<end_line>
     return_type func_name(args)
     {
     	< ... >
     	some_code();
     	< ... >
     }
   - Preserve EXACT verbatim indentation (tabs/spaces) from the source code. Indent the < ... > (or <...>) omission marker to match surrounding block level.
   - Do NOT mention raw line numbers in prose; refer to function names or the snippet header instead.
7. Strict Caret (^^^^^) highlighting rules:
   - Carets are overused and must be used with extreme discipline. In most reports, NO carets should be used.
   - Carets may ONLY point to an existing defective code token or operator that is visibly present in the code (e.g. an unsigned variable compared with < 0, an inverted relational operator, or an off-by-one boundary).
   - NEVER use carets to point at a missing thing (e.g. NEVER point carets at 'goto out;' or 'return err;' or a blank line to say "missing kfree()"). Absence of a function call cannot be highlighted with carets.
   - Comments on code lines or caret lines must NEVER cause total line width (including indentation) to exceed 75 characters. If an explanation is needed, place it on a separate comment line or explain it in the prose below the snippet.
8. Concurrency, Race Conditions, and Deadlocks (LKML timeline style):
   - For race conditions, deadlocks, lock order inversions, or multi-CPU concurrency issues—and ONLY when it clearly improves clarity—you may illustrate the temporal sequence of events using a clean multi-column timeline across the involved CPUs.
   - LKML format: Format columns using whitespace separation and dashed underlines (e.g. CPU 0 and CPU 1). Do NOT draw an ASCII table with vertical borders ('|'), crosses ('+'), or markdown table grids.
   - Choose whatever representation best explains the specific problem: prose only, code snippet only, multi-CPU timeline, or a combination if needed to ground the concurrency trace in code.
   - Do NOT use multi-column timelines for non-concurrency defects (single-threaded leaks, null pointer dereferences on error paths, missing validation).
9. Do NOT use backticks (`) to quote any names (variables, functions, symbols, or files). For function names, use func() format.
10. Do NOT use markdown code fences (```) or quote marks ('>').
11. Format all text paragraphs and comment lines hard-wrapped at 75 characters per line (LKML standard: 72-75 columns). Do not wrap code lines or multi-column diagrams.

EXAMPLES:

Example 1 (Resource / memory leak with paired allocation and error exit):

In parse_durable_handle_context(), if ksmbd_extract_sharename() fails after
allocating the durable handle buffer, the function returns an error code
without freeing the allocated buffer:

    // fs/smb/server/smb2pdu.c:2450-2475
    static int parse_durable_handle_context(...)
    {
    	struct ksmbd_file *fp;
    	< ... >
    	fp = kzalloc(sizeof(*fp), GFP_KERNEL);
    	if (!fp)
    		return -ENOMEM;
    	< ... >
    	rc = ksmbd_extract_sharename(share_name, ...);
    	if (rc) {
    		status.ret = KSMBD_TREE_CONN_STATUS_ERROR;
    		return rc;
    	}
    	< ... >
    }

The allocated fp structure is abandoned on the early return path without
calling ksmbd_fd_put() or kfree(), leading to a permanent kernel memory
leak whenever an invalid sharename is received.

Example 2 (Race condition multi-column timeline across CPUs):

In xc5000_release(), cancel_delayed_work() does not wait for timer_sleep
to finish if it is already executing, racing with priv deallocation:

    CPU 0 (release thread)              CPU 1 (delayed worker)
    ----------------------              ----------------------
    xc5000_release()
      cancel_delayed_work()
      kfree(priv);
                                        xc5000_do_timer_sleep()
                                          priv->timer_active = 0; // UAF

This causes xc5000_do_timer_sleep() to dereference freed memory when tuner
shutdown coincides with an expiring sleep timer.

Example 3 (Broken invariant described in pure prose without a code snippet):

In bpf_sk_lookup_assign(), assigning a TCP listening socket to an SKB
fails to verify that the socket's network namespace matches the incoming
packet's network namespace.

When a BPF program attaches to a cgroup in a non-root network namespace and
performs a socket lookup across namespaces, the function assigns a foreign
namespace listener to the SKB. Subsequent TCP input processing in the local
namespace assumes all associated sockets reside within the packet's own
netns, resulting in cross-netns socket leaks and routing invariant
violations.

Example 4 (Type mismatch / signedness with targeted carets and short comment):

On a 32-bit architecture, size_t is 32-bit and an integer overflow occurs
when calculating the allocation size in snd_pcm_hw_params():

    // sound/core/pcm_native.c:450-475
    static int snd_pcm_hw_params(...)
    {
    	< ... >
    	size = params->periods * params->period_bytes;
    	       ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
    	       // overflows 32-bit size_t
    	< ... >
    }

Because params->periods and params->period_bytes are controlled by
userspace ALSA configuration, multiplication of large values wraps around
zero, causing kmalloc() to allocate insufficient memory for subsequent DMA
transfers.

Example 5 (Circular lock dependency / deadlock timeline):

A circular locking dependency exists between slots_lock and vcpu mutex
across multiple execution contexts:

    CPU 0                               CPU 1
    -----                               -----
    lock(&kvm->slots_lock);
                                        lock(&vcpu->mutex);
                                        lock(&kvm->slots_lock); // blocks
    sync(&kvm->srcu);
      lock(&vcpu->mutex); // deadlock

CPU 0 holds slots_lock while waiting for vcpu mutex via sync_srcu, while
CPU 1 holds vcpu mutex and attempts to acquire slots_lock, creating an
unresolvable AB-BA deadlock.
"#.to_string()
    }

    fn initial_user_prompt(&self) -> String {
        let loc_str = self
            .locations
            .and_then(|v| serde_json::to_string_pretty(v).ok())
            .unwrap_or_else(|| "[]".to_string());

        let intro_str = self
            .introduced_in_commit
            .map(|s| format!("Introduced in commit: {}\n", s))
            .unwrap_or_default();

        let code_section = if !self.prefetched_context.trim().is_empty() {
            format!(
                "\nVerified Code Context from Repository:\n{}\n",
                self.prefetched_context.trim()
            )
        } else {
            String::new()
        };

        format!(
            "Linux Kernel Defect Details:
Title: {problem}
Severity: {severity}
Description:
{description}
Verification Details:
{explanation}
{intro_str}\
Locations:
{loc_str}
{code_section}
Task:
Write a detailed technical description of the problem for upstream submission.
- Lead with the symptom: The very first sentence must state what goes wrong, in which function/subsystem, and under what condition.
- If the problem is reproducible only under special circumstances, e.g. on a 32-bit machine, highlight it first upfront in sentence 1 (e.g. 'On a 32-bit architecture...').
- Anti-Lecture: Do NOT explain generic kernel concepts (RCU, spinlocks, workqueues). Focus strictly on the broken invariant in this code.
- Keep the description to 1 to 2 cohesive paragraphs. Avoid vertical call-trees (e.g. func_a() -> func_b() -> func_c()); refer to callers inline instead.
- Avoid generic security hyperbole; state concrete technical consequences (e.g. memory leak, panic, use-after-free, deadlock).
- Explain the precise root cause and failure mechanism in depth.
- Adaptive code snippets:
  - If pure prose explains the issue clearly, omit code snippets completely.
  - For paired actions (memory allocations, lock acquisitions, refcounts), the snippet MUST include BOTH the allocation/acquisition site AND the error exit where release was missed.
  - Strict Carets: Do NOT overuse carets (^^^^^). Carets may ONLY point to an existing defective expression. NEVER use carets to highlight a missing call (e.g. do not point carets at 'goto out;' or 'return err;' to denote missing kfree()).
  - Comments on code lines or caret lines must never exceed 75 characters per line.
  - When snippets are included, format with // <filepath>:<start_line>-<end_line>, verbatim tabs, and < ... > (or <...>) omission markers.
- Concurrency formatting:
  - For race conditions or deadlocks across CPUs, format the temporal sequence using whitespace-separated columns and dashed underlines (LKML style).
  - Do NOT draw tables with vertical borders ('|'), crosses ('+'), or markdown table grids.
  - Choose whatever representation best explains the specific problem: prose only, code snippet only, multi-CPU timeline diagram, or both if needed to ground the race in code. Do NOT use multi-column timelines for non-concurrency defects.
- Formatting & Tone:
  - Hard-wrap all prose and comment lines at 75 characters per line (LKML standard).
  - Raw plain text only, no markdown fences, no quote marks ('>'), no backticks (`).
  - For function names, ALWAYS use func() format.
  - Do NOT provide fix recommendations, remediation advice, or patches.
  - Do NOT include headers like 'Defect Report:' or 'Description:'. Start directly with the technical description.",
            problem = self.problem,
            severity = self.severity,
            description = self.canonical_description,
            explanation = self.severity_explanation,
            intro_str = intro_str,
            loc_str = loc_str,
            code_section = code_section,
        )
    }

    fn tools(&self) -> Option<Vec<AiTool>> {
        self.tools.as_ref().map(|t| t.get_declarations_generic())
    }

    async fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        if let Some(ref tools) = self.tools {
            tools.call(name, args).await
        } else {
            bail!("Tool execution requested but no toolbox available");
        }
    }

    fn context_tag(&self) -> Option<String> {
        self.context_tag.clone()
    }

    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
        let text = response.content.as_deref().unwrap_or("").trim();
        if text.is_empty() {
            return Err(ValidationError::FormatViolation("Output was empty".into()));
        }

        Ok(text.to_string())
    }
}

// ---------------------------------------------------------------------------
// 7. Pipeline Driver
// ---------------------------------------------------------------------------

/// Generates a unique bugid for a newly discovered Linux kernel bug (format: linux-<uuid>).
pub fn generate_bugid() -> String {
    format!("linux-{}", uuid::Uuid::new_v4())
}

#[deprecated(note = "use generate_bugid instead")]
pub fn generate_slug() -> String {
    generate_bugid()
}

/// Executes the standalone Linux kernel bug pipeline for a single candidate concern.
pub async fn process_issue(
    _provider: &dyn AiProvider,
    _tools: Option<Arc<ToolBox>>,
    db: &Database,
    input: BugInput,
    _context_tag: Option<&str>,
) -> Result<BugOutcome> {
    info!(
        "Queueing candidate Linux kernel issue: '{}' in subsystems '{:?}'",
        input.problem, input.subsystems
    );
    let bugid = generate_bugid();
    let raw_input_json = serde_json::to_string(&input).ok();
    let new_bug = NewBug {
        bugid: bugid.clone(),
        status: "raw".to_string(),
        problem: input.problem.clone(),
        severity: crate::db::Severity::Unknown,
        severity_explanation: None,
        locations: input.locations.clone(),
        subsystems: input.subsystems.clone(),
        source_files: Some(input.source_files.clone()),
        inline_review: String::new(),
        logs: None,
        vector_json: None,
        discovered_in_patchset_id: input.patchset_id,
        discovered_in_patch_id: input.patch_id,
        discovered_in_commit: input.commit_sha.clone(),
        introduced_in_commit: None,
        verified_on_sha: None,
        is_fixed: false,
        fixed_in_commit: None,
        raw_input: raw_input_json,
        tokens_in: None,
        tokens_out: None,
        tokens_cached: None,
        duplicate_of_id: None,
        created_at: chrono::Utc::now().timestamp(),
    };
    let id = db.create_bug(&new_bug).await?;
    info!("Queued raw bug {} for asynchronous processing", id);
    let bug = db.get_bug(id).await?.unwrap();
    Ok(BugOutcome::NewlyDiscovered { bug })
}

static BUG_DEDUP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn get_master_sha(tools: Option<&Arc<ToolBox>>) -> String {
    let tb = match tools {
        Some(t) => t.clone(),
        None => return "master".to_string(),
    };
    tokio::task::spawn_blocking(move || {
        let worktree = tb.get_worktree_path();
        for ref_name in ["origin/master", "master", "HEAD"] {
            if let Ok(output) = std::process::Command::new("git")
                .current_dir(worktree)
                .args(["rev-parse", ref_name])
                .output()
                && output.status.success()
            {
                let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !sha.is_empty() {
                    return sha;
                }
            }
        }
        "master".to_string()
    })
    .await
    .unwrap_or_else(|_| "master".to_string())
}

async fn format_commit(tools: Option<&Arc<ToolBox>>, sha: Option<String>) -> Option<String> {
    let sha = sha?;
    let tb = match tools {
        Some(t) => t.clone(),
        None => return Some(sha),
    };
    tokio::task::spawn_blocking(move || {
        let worktree = tb.get_worktree_path();

        let subject = std::process::Command::new("git")
            .current_dir(worktree)
            .args(["log", "-1", "--format=%s", &sha])
            .output()
            .ok()
            .and_then(|out| {
                if out.status.success() {
                    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "unknown".to_string());

        let release = std::process::Command::new("git")
            .current_dir(worktree)
            .args(["describe", "--contains", &sha])
            .output()
            .ok()
            .and_then(|out| {
                if out.status.success() {
                    let stdout_str = String::from_utf8_lossy(&out.stdout);
                    Some(
                        stdout_str
                            .trim()
                            .split('~')
                            .next()
                            .unwrap_or("")
                            .split('^')
                            .next()
                            .unwrap_or("")
                            .to_string(),
                    )
                } else {
                    None
                }
            });

        let tag_part = match release {
            Some(rel) if !rel.is_empty() => format!(" [{}]", rel),
            _ => "".to_string(),
        };

        let short_sha = if sha.len() >= 12 { &sha[..12] } else { &sha };
        format!("{} (\"{}\"){}", short_sha, subject, tag_part)
    })
    .await
    .ok()
}

/// Deterministically executes git blame on candidate locations to identify an introducing commit
/// as a fallback when LLM origin tracing cannot identify the commit.
pub async fn deterministic_blame_fallback(
    tools: Option<&Arc<ToolBox>>,
    locations: &Option<Value>,
    target_sha: &str,
) -> Option<String> {
    let tb = tools?;
    let loc_arr = locations.as_ref()?.as_array()?;
    let worktree = tb.get_worktree_path();

    for loc in loc_arr {
        let Some(file) = loc.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(line) = loc.get("line").and_then(|v| v.as_u64()) else {
            continue;
        };
        let blame_output = tokio::process::Command::new("git")
            .current_dir(worktree)
            .args(["-c", "safe.bareRepository=all"])
            .args([
                "blame",
                "-L",
                &format!("{},{}", line, line),
                "--porcelain",
                target_sha,
                "--",
                file,
            ])
            .output()
            .await;

        if let Some(out) = blame_output.ok().filter(|o| o.status.success()) {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if let Some(first_line) = stdout.lines().next() {
                let sha = first_line.split_whitespace().next().unwrap_or("");
                if !sha.is_empty() && !sha.chars().all(|c| c == '0') {
                    info!(
                        "Blame fallback identified introducing commit: {} for {}:{}",
                        sha, file, line
                    );
                    return Some(sha.to_string());
                }
            }
        }
    }

    None
}

/// Parses (file_path, line_number) pairs from Linux kernel stack traces, oops dumps,
/// or panic call traces.
pub fn parse_stack_trace(text: &str) -> Vec<(String, usize)> {
    let Ok(re) = regex::Regex::new(r"\b([a-zA-Z0-9_\-\./]+\.(?:c|h|S|rs))\:([0-9]+)\b") else {
        return Vec::new();
    };
    let mut results = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for cap in re.captures_iter(text) {
        if let (Some(file), Some(line)) = (cap.get(1), cap.get(2)) {
            let file_str = file.as_str().to_string();
            let Ok(line_num) = line.as_str().parse::<usize>() else {
                continue;
            };
            if seen.insert((file_str.clone(), line_num)) {
                results.push((file_str, line_num));
            }
        }
    }
    results
}

pub const MAX_BUG_PREFETCH_CHARS: usize = 20_000;
pub const MAX_BUG_PREFETCH_FILES: usize = 3;
pub const MAX_BUG_PREFETCH_SNIPPETS_PER_FILE: usize = 2;
pub const MAX_BUG_PREFETCH_SNIPPETS_TOTAL: usize = 5;
pub const MAX_BUG_SNIPPET_LINES: usize = 100;

/// Deterministically pre-fetches code snippets around candidate bug locations from mainline.
///
/// Strictly bounded by guardrails to prevent context window bloat from malformed or
/// adversarial candidate reports:
/// 1. At most 3 distinct .c/.h files.
/// 2. At most 2 snippets per file, 5 snippets total.
/// 3. At most 100 lines per snippet (enclosing function via Tree-sitter or clamped window).
/// 4. Total character budget <= 20,000 characters (~5,000 tokens).
pub async fn prefetch_bug_locations(
    tools: Option<&Arc<ToolBox>>,
    master_sha: &str,
    locations: &Option<Value>,
) -> String {
    let Some(tools) = tools else {
        return String::new();
    };
    let Some(loc_arr) = locations.as_ref().and_then(|v| v.as_array()) else {
        return String::new();
    };
    if loc_arr.is_empty() {
        return String::new();
    }

    // Step 1: Collect and sanitize candidate file paths (max 3 distinct valid C files)
    let mut files: Vec<String> = Vec::new();
    for loc in loc_arr {
        let Some(file) = loc.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        let file = file.trim();
        if file.contains("..")
            || file.starts_with('/')
            || file.starts_with('\\')
            || (!file.ends_with(".c") && !file.ends_with(".h"))
        {
            continue;
        }
        if !files.contains(&file.to_string()) {
            files.push(file.to_string());
            if files.len() >= MAX_BUG_PREFETCH_FILES {
                break;
            }
        }
    }

    if files.is_empty() {
        return String::new();
    }

    let worktree = tools.get_worktree_path().to_path_buf();
    let master_sha_owned = master_sha.to_string();
    let loc_arr_owned = loc_arr.clone();

    tokio::task::spawn_blocking(move || {
        let mut output = String::new();
        let mut total_snippets = 0;

        for file in files {
            if total_snippets >= MAX_BUG_PREFETCH_SNIPPETS_TOTAL {
                break;
            }

            let git_output = std::process::Command::new("git")
                .current_dir(&worktree)
                .args(["show", &format!("{}:{}", master_sha_owned, file)])
                .output();

            let (out, actual_file) = match git_output {
                Ok(o) if o.status.success() => (o, file.clone()),
                _ => {
                    // Try tracing rename forwards first
                    let last_commit = std::process::Command::new("git")
                        .current_dir(&worktree)
                        .args(["log", "-n", "1", "--format=%H", "--", &file])
                        .output();
                    let mut resolved = None;
                    if let Ok(lc) = last_commit {
                        let sha = String::from_utf8_lossy(&lc.stdout).trim().to_string();
                        if !sha.is_empty() {
                            let diff_out = std::process::Command::new("git")
                                .current_dir(&worktree)
                                .args(["show", "-M", "--name-status", "--format=", &sha])
                                .output();
                            if let Ok(do_out) = diff_out {
                                let diff_str = String::from_utf8_lossy(&do_out.stdout);
                                for line in diff_str.lines() {
                                    let parts: Vec<&str> = line.split('\t').collect();
                                    if parts.len() >= 3
                                        && parts[0].starts_with('R')
                                        && parts[1] == file
                                    {
                                        let next_path = parts[2].trim();
                                        let try_out = std::process::Command::new("git")
                                            .current_dir(&worktree)
                                            .args([
                                                "show",
                                                &format!("{}:{}", master_sha_owned, next_path),
                                            ])
                                            .output();
                                        if let Some(to) =
                                            try_out.ok().filter(|t| t.status.success())
                                        {
                                            resolved = Some((to, next_path.to_string()));
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if resolved.is_none() {
                        // Fallback: check git log --follow backwards
                        let rename_cmd = std::process::Command::new("git")
                            .current_dir(&worktree)
                            .args(["log", "--follow", "--name-only", "--format=format:", &file])
                            .output();
                        if let Some(ro) = rename_cmd.ok().filter(|r| r.status.success()) {
                            let stdout = String::from_utf8_lossy(&ro.stdout);
                            for line in stdout.lines() {
                                let trimmed = line.trim();
                                if trimmed.is_empty() || trimmed == file {
                                    continue;
                                }
                                let try_out = std::process::Command::new("git")
                                    .current_dir(&worktree)
                                    .args(["show", &format!("{}:{}", master_sha_owned, trimmed)])
                                    .output();
                                if let Some(to) = try_out.ok().filter(|t| t.status.success()) {
                                    resolved = Some((to, trimmed.to_string()));
                                    break;
                                }
                            }
                        }
                    }

                    if let Some(r) = resolved {
                        r
                    } else {
                        continue;
                    }
                }
            };

            let content = String::from_utf8_lossy(&out.stdout).to_string();
            let lines: Vec<&str> = content.lines().collect();
            if lines.is_empty() {
                continue;
            }

            let file_locs: Vec<&Value> = loc_arr_owned
                .iter()
                .filter(|loc| {
                    loc.get("file")
                        .and_then(|v| v.as_str())
                        .map(|f| f.trim() == file || f.trim() == actual_file)
                        .unwrap_or(false)
                })
                .take(MAX_BUG_PREFETCH_SNIPPETS_PER_FILE)
                .collect();

            for loc in file_locs {
                if total_snippets >= MAX_BUG_PREFETCH_SNIPPETS_TOTAL {
                    break;
                }

                let line_opt = loc.get("line").and_then(|v| v.as_u64()).map(|l| l as usize);
                let sym_opt = loc
                    .get("function_or_symbol")
                    .and_then(|v| v.as_str())
                    .map(str::trim);

                let snippet_opt = {
                    let mut extracted = None;
                    if let Some(sym) = sym_opt {
                        let found_idx = lines
                            .iter()
                            .position(|l| {
                                let trimmed = l.trim_start();
                                trimmed.contains(sym)
                                    && (trimmed.contains('(')
                                        || trimmed.starts_with("static")
                                        || trimmed.starts_with("int")
                                        || trimmed.starts_with("void"))
                            })
                            .or_else(|| lines.iter().position(|l| l.contains(sym)));
                        if let Some(idx) = found_idx {
                            let line = idx + 1;
                            if let Some((block_text, name)) =
                                crate::worker::prefetch::extract_enclosing_block(&content, idx, idx)
                            {
                                let b_lines: Vec<&str> = block_text.lines().collect();
                                let clamped_text = if b_lines.len() > MAX_BUG_SNIPPET_LINES {
                                    let half = MAX_BUG_SNIPPET_LINES / 2;
                                    let start = idx
                                        .saturating_sub(half)
                                        .min(lines.len().saturating_sub(MAX_BUG_SNIPPET_LINES));
                                    let end = (start + MAX_BUG_SNIPPET_LINES).min(lines.len());
                                    lines[start..end].join("\n")
                                } else {
                                    block_text
                                };
                                let reported_line = line_opt.unwrap_or(line);
                                extracted = Some((
                                    clamped_text,
                                    name.or_else(|| Some(sym.to_string())),
                                    reported_line,
                                ));
                            } else {
                                let reported_line = line_opt.unwrap_or(line);
                                let start = line.saturating_sub(20).max(1);
                                let end = (start + MAX_BUG_SNIPPET_LINES).min(lines.len());
                                let start_0 = start.saturating_sub(1);
                                let text = lines[start_0..end].join("\n");
                                extracted = Some((text, Some(sym.to_string()), reported_line));
                            }
                        }
                    }

                    if extracted.is_none()
                        && let Some(line) = line_opt
                        && line >= 1
                        && line <= lines.len()
                    {
                        let line_0 = line.saturating_sub(1);
                        if let Some((block_text, name)) =
                            crate::worker::prefetch::extract_enclosing_block(
                                &content, line_0, line_0,
                            )
                        {
                            let b_lines: Vec<&str> = block_text.lines().collect();
                            let clamped_text = if b_lines.len() > MAX_BUG_SNIPPET_LINES {
                                let half = MAX_BUG_SNIPPET_LINES / 2;
                                let start = line_0
                                    .saturating_sub(half)
                                    .min(lines.len().saturating_sub(MAX_BUG_SNIPPET_LINES));
                                let end = (start + MAX_BUG_SNIPPET_LINES).min(lines.len());
                                lines[start..end].join("\n")
                            } else {
                                block_text
                            };
                            extracted = Some((
                                clamped_text,
                                name.or_else(|| sym_opt.map(str::to_string)),
                                line,
                            ));
                        } else {
                            let start = line.saturating_sub(30).max(1);
                            let end = (start + MAX_BUG_SNIPPET_LINES).min(lines.len());
                            let start_0 = start.saturating_sub(1);
                            let text = lines[start_0..end].join("\n");
                            extracted = Some((text, sym_opt.map(str::to_string), line));
                        }
                    }

                    extracted
                };

                if let Some((block, sym_name, line_num)) = snippet_opt {
                    let header = if let Some(ref name) = sym_name {
                        format!("--- {}:{} ({}) ---\n", file, line_num, name)
                    } else {
                        format!("--- {}:{} ---\n", file, line_num)
                    };

                    if output.len() + header.len() + block.len() + 1 > MAX_BUG_PREFETCH_CHARS {
                        output.push_str("\n... (Context prefetch limits reached)\n");
                        return output;
                    }

                    output.push_str(&header);
                    output.push_str(&block);
                    output.push('\n');
                    total_snippets += 1;
                }
            }
        }

        output
    })
    .await
    .unwrap_or_default()
}

pub async fn process_issue_worker(
    provider: &dyn AiProvider,
    tools: Option<Arc<ToolBox>>,
    db: &Database,
    bug_row: &crate::db::Bug,
    input: BugInput,
    context_tag: Option<&str>,
) -> Result<BugOutcome> {
    info!(
        "Processing candidate Linux kernel issue: '{}' in subsystems '{:?}'",
        input.problem, input.subsystems
    );

    let mut full_history = Vec::new();
    let mut total_usage = crate::ai::AiUsage::default();
    let runner = SessionRunner::new(provider).with_max_turns(20);
    let master_sha = get_master_sha(tools.as_ref()).await;

    // Enrich source files and candidate locations via stack trace parsing and git rename tracking
    let mut effective_source_files = input.source_files.clone();
    let mut effective_locations = input.locations.clone();

    let trace_frames = parse_stack_trace(&format!("{}\n{}", input.problem, input.reasoning));
    if !trace_frames.is_empty() {
        let mut new_locs = Vec::new();
        for (file, line) in trace_frames {
            if !effective_source_files.contains(&file) {
                effective_source_files.push(file.clone());
            }
            new_locs.push(serde_json::json!({
                "file": file,
                "line": line
            }));
        }
        let has_no_locations = effective_locations
            .as_ref()
            .and_then(|v| v.as_array())
            .is_none_or(|a| a.is_empty());
        if has_no_locations {
            effective_locations = Some(Value::Array(new_locs));
        }
    }

    if let Some(ref tb) = tools {
        let repo_path = tb.get_worktree_path();
        for file in &mut effective_source_files {
            if crate::git_ops::git_file_exists_at(repo_path, file, Some(&master_sha)).await {
                continue;
            }
            if let Some(renamed) =
                crate::git_ops::git_find_file_rename(repo_path, file, Some(&master_sha)).await
            {
                info!("Traced renamed file from {} to {}", file, renamed);
                *file = renamed;
            }
        }
    }

    // Stage 1: Normalization & Canonical Naming
    info!("--- Stage 1: Normalization ---");
    let maintainers_hint = tools
        .as_ref()
        .and_then(|tb| crate::maintainers::MaintainersIndex::from_repo(tb.get_worktree_path()).ok())
        .map(|mindex| {
            let matched = mindex.match_files(&effective_source_files);
            if matched.is_empty() {
                String::new()
            } else {
                format!(
                    "Detected Subsystems from MAINTAINERS: {}",
                    matched.join(", ")
                )
            }
        });

    let raw_locations_str = effective_locations
        .as_ref()
        .and_then(|v| serde_json::to_string_pretty(v).ok())
        .unwrap_or_else(|| "[]".to_string());

    let mut norm_session = NormalizeSession {
        problem: &input.problem,
        reasoning: &input.reasoning,
        locations: &raw_locations_str,
        master_sha: &master_sha,
        maintainers_hint,
        tools: tools.clone(),
        context_tag: context_tag.map(|s| s.to_string()),
    };

    let norm_result = runner.run(&mut norm_session).await?;
    full_history.extend(norm_result.history);
    total_usage.accumulate(&norm_result.usage);
    let norm = norm_result.output;

    // Verify affected source files against mainline tree, falling back to input locations if empty
    let mut verified_files = Vec::new();
    if let Some(ref tb) = tools {
        let repo_path = tb.get_worktree_path();
        for file in &norm.affected_source_files {
            if crate::git_ops::git_file_exists_at(repo_path, file, Some(&master_sha)).await {
                verified_files.push(file.clone());
            }
        }
    } else {
        verified_files = norm.affected_source_files.clone();
    }

    if verified_files.is_empty() {
        verified_files = effective_source_files.clone();
    }

    // Determine official subsystems programmatically from MAINTAINERS
    let official_subsystems = if let Some(ref tb) = tools {
        if let Ok(mindex) = crate::maintainers::MaintainersIndex::from_repo(tb.get_worktree_path())
        {
            let matched = mindex.match_files(&verified_files);
            if !matched.is_empty() {
                matched
            } else {
                extract_directory_subsystems(&verified_files)
            }
        } else {
            extract_directory_subsystems(&verified_files)
        }
    } else if !input.subsystems.is_empty() {
        input.subsystems.clone()
    } else {
        extract_directory_subsystems(&verified_files)
    };

    let title_prefix = extract_title_prefix(&norm.canonical_title);

    info!(
        "Stage 1 Complete: Normalized to '{}' (prefix '{}') with official subsystems '{:?}'",
        norm.canonical_title, title_prefix, official_subsystems
    );

    // Stage 2: Verification & Ground-Truth Confirmation
    info!("--- Stage 2: Verification ---");
    let prefetched_context =
        prefetch_bug_locations(tools.as_ref(), &master_sha, &effective_locations).await;
    let mut verify_session = VerifySession {
        title: &norm.canonical_title,
        description: &norm.canonical_description,
        subsystem: title_prefix,
        affected_files: &verified_files,
        locations: effective_locations.as_ref(),
        master_sha: master_sha.clone(),
        tools: tools.clone(),
        context_tag: context_tag.map(|s| s.to_string()),
        prefetched_context: prefetched_context.clone(),
    };

    let verify_result = runner.run(&mut verify_session).await?;
    full_history.extend(verify_result.history);
    total_usage.accumulate(&verify_result.usage);
    let verification = verify_result.output;
    info!(
        "Stage 2 Complete: Verification returned is_false_positive={}",
        verification.is_false_positive
    );

    if verification.is_false_positive {
        let reason = verification.refutation_evidence.unwrap_or_else(|| {
            "Discarded as a hallucinated or disproved false positive".to_string()
        });
        info!("Linux kernel candidate discarded: {}", reason);
        let logs = serde_json::to_string(&full_history).unwrap_or_default();
        db.update_bug_outcome(
            bug_row.id,
            crate::db::UpdateBugOutcomeParams {
                status: "dismissed",
                problem: Some(&norm.canonical_title),
                subsystems: Some(&official_subsystems),
                source_files: Some(&verified_files),
                severity_explanation: Some(&reason),
                logs: Some(&logs),
                verified_on_sha: Some(&master_sha),
                tokens_in: Some(total_usage.prompt_tokens),
                tokens_out: Some(total_usage.completion_tokens),
                tokens_cached: total_usage.cached_tokens,
                ..Default::default()
            },
        )
        .await?;
        return Ok(BugOutcome::Discarded {
            reason,
            logs: Some(logs),
        });
    }

    let verified_locations = verification
        .relevant_code_locations
        .or_else(|| input.locations.clone());
    let verified_locations_str = verified_locations
        .as_ref()
        .and_then(|v| serde_json::to_string_pretty(v).ok())
        .unwrap_or_else(|| "[]".to_string());

    // Stage 3: Deduplication Confirmation (under BUG_DEDUP_LOCK)
    info!("--- Stage 3: Deduplication ---");
    let query_vector = extract_bug_vector(
        &norm.canonical_title,
        &official_subsystems,
        &verified_files,
        verified_locations.as_ref(),
    );

    let (is_dup, dup_outcome) = {
        let _dedup_guard = BUG_DEDUP_LOCK.lock().await;

        let mut known_bugs = db.list_all_bugs_for_vector_search().await?;
        known_bugs.retain(|b| b.id != bug_row.id);
        let candidate_matches = find_top_candidates(
            &query_vector,
            &known_bugs,
            DEFAULT_TOP_CANDIDATES,
            DEFAULT_SIMILARITY_THRESHOLD,
        );

        info!(
            "Stage 3: Found {} potential candidates.",
            candidate_matches.len()
        );

        if !candidate_matches.is_empty() {
            let candidate_bugs: Vec<Bug> =
                candidate_matches.iter().map(|m| m.bug.clone()).collect();
            let mut dedup_session = DedupSession {
                candidate_problem: &norm.canonical_title,
                candidate_locations: verified_locations.as_ref(),
                candidate_subsystems: &official_subsystems,
                known_candidates: &candidate_bugs,
                context_tag: context_tag.map(|s| s.to_string()),
            };

            let dedup_result = runner.run(&mut dedup_session).await?;
            full_history.extend(dedup_result.history);
            total_usage.accumulate(&dedup_result.usage);
            let dedup = dedup_result.output;

            let duplicate_match = if dedup.is_duplicate {
                dedup
                    .duplicate_of_id
                    .and_then(|dup_id| candidate_bugs.iter().find(|b| b.id == dup_id))
            } else {
                None
            };

            if let Some(existing) = duplicate_match {
                info!(
                    "Matched duplicate Linux kernel bug #{} ({})",
                    existing.id, existing.bugid
                );
                let logs = serde_json::to_string(&full_history).unwrap_or_default();
                let dup_meta = serde_json::json!({
                    "duplicate_of_bugid": existing.bugid,
                    "duplicate_of_slug": existing.bugid,
                    "duplicate_of_id": existing.id,
                    "reasoning": dedup.reasoning
                });
                let dup_meta_str = serde_json::to_string(&dup_meta).unwrap_or_default();

                db.mark_bug_as_duplicate(crate::db::MarkDuplicateBugParams {
                    ephemeral_id: bug_row.id,
                    canonical_id: existing.id,
                    reasoning: &dup_meta_str,
                    logs: Some(&logs),
                    tokens_in: Some(total_usage.prompt_tokens),
                    tokens_out: Some(total_usage.completion_tokens),
                    tokens_cached: total_usage.cached_tokens,
                })
                .await?;
                (
                    true,
                    Some(BugOutcome::Duplicate {
                        existing_bug: existing.clone(),
                        reasoning: dedup.reasoning,
                        logs: Some(logs),
                    }),
                )
            } else {
                (false, None)
            }
        } else {
            (false, None)
        }
    };

    if is_dup {
        return Ok(dup_outcome.unwrap());
    }
    info!("Stage 3 Complete: Novel bug confirmed.");

    // Stage 4: Origin Tracing (Enrichment)
    info!("--- Stage 4: Origin Tracing ---");
    let tracing_runner = SessionRunner::new(provider).with_max_turns(30);
    let mut tracing_session = TracingSession {
        input: &input,
        master_sha: master_sha.clone(),
        verification_reasoning: verification.verification_reasoning.clone(),
        relevant_locations: verified_locations_str.clone(),
        tools: tools.clone(),
        context_tag: context_tag.map(|s| s.to_string()),
    };
    let tracing_result = tracing_runner.run(&mut tracing_session).await?;
    full_history.extend(tracing_result.history);
    total_usage.accumulate(&tracing_result.usage);
    let introducing_commit_sha = match tracing_result.output.introducing_commit_sha {
        Some(sha) => Some(sha),
        None => {
            deterministic_blame_fallback(tools.as_ref(), &verified_locations, &master_sha).await
        }
    };
    let introduced_in_commit = format_commit(tools.as_ref(), introducing_commit_sha).await;

    // Stage 5: Severity & Impact Estimation (Enrichment)
    info!("--- Stage 5: Severity & Impact Calibration ---");
    let mut severity_session = SeveritySession {
        canonical_title: &norm.canonical_title,
        canonical_description: &norm.canonical_description,
        locations: &verified_locations_str,
        context_tag: context_tag.map(|s| s.to_string()),
    };
    let severity_result = runner.run(&mut severity_session).await?;
    full_history.extend(severity_result.history);
    total_usage.accumulate(&severity_result.usage);
    let severity_output = severity_result.output;
    let severity = Severity::from_str(&severity_output.severity);

    // Stage 6: Standalone Plaintext Review Generation (Enrichment)
    info!("--- Stage 6: Standalone Review Generation ---");
    let verified_prefetched =
        prefetch_bug_locations(tools.as_ref(), &master_sha, &verified_locations).await;
    let effective_prefetched = if !verified_prefetched.is_empty() {
        verified_prefetched
    } else {
        prefetched_context
    };

    let mut report_session = ReportSession {
        problem: &norm.canonical_title,
        severity: severity.as_str(),
        canonical_description: &norm.canonical_description,
        severity_explanation: &severity_output.severity_explanation,
        locations: verified_locations.as_ref(),
        introduced_in_commit: introduced_in_commit.as_deref(),
        tools: tools.clone(),
        context_tag: context_tag.map(|s| s.to_string()),
        prefetched_context: effective_prefetched,
    };
    let report_result = runner.run(&mut report_session).await?;
    full_history.extend(report_result.history);
    total_usage.accumulate(&report_result.usage);
    let inline_review = report_result.output;

    // Stage 7: Final Database Write
    info!("--- Stage 7: Final Database Write ---");
    let logs_json = serde_json::to_string(&full_history).ok();

    db.update_bug_outcome(
        bug_row.id,
        crate::db::UpdateBugOutcomeParams {
            status: "open",
            problem: Some(&norm.canonical_title),
            subsystems: Some(&official_subsystems),
            source_files: Some(&verified_files),
            locations: verified_locations.as_ref(),
            severity,
            severity_explanation: Some(&severity_output.severity_explanation),
            inline_review: &inline_review,
            logs: logs_json.as_deref(),
            vector_json: Some(&query_vector.to_json()),
            introduced_in_commit: introduced_in_commit.as_deref(),
            verified_on_sha: Some(&master_sha),
            is_fixed: false,
            fixed_in_commit: None,
            tokens_in: Some(total_usage.prompt_tokens),
            tokens_out: Some(total_usage.completion_tokens),
            tokens_cached: total_usage.cached_tokens,
        },
    )
    .await?;

    let saved_bug = db.get_bug(bug_row.id).await?.expect("Saved bug must exist");
    info!(
        "Successfully registered newly verified Linux kernel bug #{} ({})",
        bug_row.id, bug_row.bugid
    );
    Ok(BugOutcome::NewlyDiscovered { bug: saved_bug })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiRequest, ProviderCapabilities};
    use serde_json::json;

    struct MockAiProvider {
        response_text: String,
    }

    #[async_trait]
    impl AiProvider for MockAiProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            Ok(AiResponse {
                content: Some(self.response_text.clone()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            100
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 8192,
            }
        }
    }

    #[tokio::test]
    async fn test_verify_session_valid() {
        let input = BugInput {
            problem: "Memory leak in net/core/dev.c".to_string(),
            reasoning: "Allocated buffer not freed on error path".to_string(),
            locations: Some(json!([{"file": "net/core/dev.c", "line": 100}])),
            subsystems: vec!["net".to_string()],
            source_files: vec!["net/core/dev.c".to_string()],
            commit_sha: None,
            patchset_id: None,
            patch_id: None,
            baseline_sha: None,
        };

        let mock_provider = MockAiProvider {
            response_text: json!({
                "verification_reasoning": "1. Buffer allocated. 2. Not freed before return.",
                "is_false_positive": false,
                "refutation_evidence": null,
                "impact_severity": "High",
                "relevant_code_locations": [{"file": "net/core/dev.c", "line": 100}]
            })
            .to_string(),
        };

        let mut session = VerifySession {
            title: "net: dev: memory leak in dev_alloc()",
            description: "Trigger: Netdev allocation failure.\nFailure Mechanism: Missing kfree() on error path.\nImpact: Memory leak.",
            subsystem: "net",
            affected_files: &["net/core/dev.c".to_string()],
            locations: input.locations.as_ref(),
            master_sha: "master".to_string(),
            tools: None,
            context_tag: None,
            prefetched_context: String::new(),
        };

        let runner = SessionRunner::new(&mock_provider);
        let res = runner.run(&mut session).await.unwrap();
        assert!(!res.output.is_false_positive);
        assert_eq!(res.output.impact_severity.as_deref(), Some("High"));
        assert!(
            res.output
                .verification_reasoning
                .contains("Buffer allocated")
        );
    }

    #[tokio::test]
    async fn test_verify_session_false_positive_early_exit() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        let normalize_json = json!({
            "canonical_title": "net: dev: null dereference in dev_read()",
            "canonical_description": "Trigger: Null pointer passed.\nFailure Mechanism: Dereference without check.\nImpact: Panic.",
            "affected_source_files": ["net/core/dev.c"]
        }).to_string();

        let verify_json = json!({
            "verification_reasoning": "Caller checks pointer validity before invocation, so NULL dereference is impossible.",
            "is_false_positive": true,
            "refutation_evidence": "Guarded by if (ptr) in caller at dev.c:85",
            "impact_severity": null,
            "relevant_code_locations": null
        }).to_string();

        let provider = QueuedMockAiProvider::new(vec![normalize_json, verify_json]);

        let input = BugInput {
            problem: "NULL deref in net/core/dev.c".to_string(),
            reasoning: "ptr might be null".to_string(),
            locations: Some(json!([{"file": "net/core/dev.c", "line": 100}])),
            subsystems: vec!["net".to_string()],
            source_files: vec!["net/core/dev.c".to_string()],
            commit_sha: None,
            patchset_id: None,
            patch_id: None,
            baseline_sha: None,
        };

        let outcome = process_issue(&provider, None, &db, input.clone(), None)
            .await
            .unwrap();

        let final_outcome = match outcome {
            BugOutcome::NewlyDiscovered { ref bug } => {
                process_issue_worker(&provider, None, &db, bug, input.clone(), None)
                    .await
                    .unwrap()
            }
            _ => panic!("Expected NewlyDiscovered outcome initially"),
        };

        match final_outcome {
            BugOutcome::Discarded { reason, .. } => {
                assert!(reason.contains("Guarded by if (ptr)"));
            }
            _ => panic!("Expected Discarded outcome, got {:?}", final_outcome),
        }

        let bug = db.get_bug(1).await.unwrap().unwrap();
        assert_eq!(bug.status, "dismissed");
        assert_eq!(bug.problem, "net: dev: null dereference in dev_read()");
        assert_eq!(bug.subsystems, vec!["net".to_string()]);
        assert_eq!(bug.source_files, Some(vec!["net/core/dev.c".to_string()]));
    }

    #[tokio::test]
    async fn test_normalize_session() {
        let mock_provider = MockAiProvider {
            response_text: json!({
                "canonical_title": "net: dev: memory leak in dev_alloc()",
                "canonical_description": "Trigger: Netdev allocation failure.\nFailure Mechanism: Missing kfree() on error path.\nImpact: Memory leak.",
                "affected_source_files": ["net/core/dev.c"],
                "affected_symbols": ["dev_alloc"]
            })
            .to_string(),
        };

        let mut session = NormalizeSession {
            problem: "Memory leak in net/core/dev.c",
            reasoning: "Allocated buffer not freed on error path",
            locations: "[{\"file\": \"net/core/dev.c\", \"line\": 100}]",
            master_sha: "abcdef1234567890abcdef1234567890abcdef12",
            maintainers_hint: Some(
                "Detected Subsystems from MAINTAINERS: NETWORKING [GENERAL]".to_string(),
            ),
            tools: None,
            context_tag: None,
        };

        let runner = SessionRunner::new(&mock_provider);
        let res = runner.run(&mut session).await.unwrap();
        assert_eq!(
            res.output.canonical_title,
            "net: dev: memory leak in dev_alloc()"
        );
        assert_eq!(extract_title_prefix(&res.output.canonical_title), "net");
        assert_eq!(res.output.affected_source_files, vec!["net/core/dev.c"]);
        assert_eq!(
            res.output.affected_symbols,
            Some(vec!["dev_alloc".to_string()])
        );
    }

    #[test]
    fn test_normalize_session_prompt_directives() {
        let session = NormalizeSession {
            problem: "Array compaction flaw in rk_iommu_probe",
            reasoning: "sparse array indexing",
            locations: "[]",
            master_sha: "abcdef1234567890abcdef1234567890abcdef12",
            maintainers_hint: None,
            tools: None,
            context_tag: None,
        };

        let prompt = session.initial_user_prompt();
        assert!(prompt.contains("This is a bug report title describing an existing defect"));
        assert!(prompt.contains("Do NOT use patch/fix action verbs"));
        assert!(
            prompt
                .contains("NEVER 'iommu/rockchip: fix array compaction flaw in rk_iommu_probe()'")
        );
        assert!(
            prompt.contains("Target Mainline Commit: abcdef1234567890abcdef1234567890abcdef12")
        );
        assert!(prompt.contains("git_log"));

        let sys_prompt = session.system_prompt();
        assert!(sys_prompt.contains("abcdef1234567890abcdef1234567890abcdef12"));
        assert!(sys_prompt.contains("git_read_files"));
    }

    #[test]
    fn test_normalize_session_validation_rejects_fix_verbs() {
        let mut session = NormalizeSession {
            problem: "problem",
            reasoning: "reasoning",
            locations: "[]",
            master_sha: "master",
            maintainers_hint: None,
            tools: None,
            context_tag: None,
        };

        let bad_titles = &[
            "iommu/rockchip: fix array compaction flaw in rk_iommu_probe()",
            "net: fixes memory leak in dev_alloc()",
            "btrfs: resolving use-after-free in cleanup()",
            "mm: prevent null dereference in alloc_pages()",
            "sched: avoid deadlock in schedule()",
        ];

        for bad in bad_titles {
            let response = AiResponse {
                content: Some(
                    json!({
                        "canonical_title": bad,
                        "canonical_description": "Description",
                        "affected_source_files": ["file.c"]
                    })
                    .to_string(),
                ),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            };
            let err = session.validate(&response).unwrap_err();
            match err {
                ValidationError::FormatViolation(msg) => {
                    assert!(
                        msg.contains("must describe the defect rather than a patch/fix"),
                        "Expected format violation message for '{}', got: {}",
                        bad,
                        msg
                    );
                }
                _ => panic!("Expected FormatViolation for bad title '{}'", bad),
            }
        }
    }

    #[test]
    fn test_normalize_session_validation_accepts_defect_titles() {
        let mut session = NormalizeSession {
            problem: "problem",
            reasoning: "reasoning",
            locations: "[]",
            master_sha: "master",
            maintainers_hint: None,
            tools: None,
            context_tag: None,
        };

        let good_titles = &[
            "iommu/rockchip: array compaction flaw in rk_iommu_probe()",
            "btrfs: use-after-free in btrfs_cleanup_ordered_extents()",
            "net: dev: memory leak in dev_alloc()",
            "mm: null pointer dereference in alloc_pages()",
        ];

        for good in good_titles {
            let response = AiResponse {
                content: Some(
                    json!({
                        "canonical_title": good,
                        "canonical_description": "Description",
                        "affected_source_files": ["file.c"]
                    })
                    .to_string(),
                ),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            };
            let output = session.validate(&response).unwrap();
            assert_eq!(output.canonical_title, *good);
        }
    }

    #[tokio::test]
    async fn test_dedup_session_duplicate() {
        let mock_provider = MockAiProvider {
            response_text: json!({
                "is_duplicate": true,
                "duplicate_of_id": 42,
                "reasoning": "Identical leak in net/core/dev.c"
            })
            .to_string(),
        };

        let known_bugs = vec![Bug {
            verified_on_sha: None,
            id: 42,
            status: "verified".to_string(),
            bugid: "linux-42".to_string(),
            problem: "Memory leak in dev.c".to_string(),
            severity: Severity::High,
            severity_explanation: None,
            locations: None,
            subsystems: vec!["net".to_string()],
            source_files: None,
            inline_review: "".to_string(),
            logs: None,
            vector_json: None,
            discovered_in_patchset_id: None,
            discovered_in_patch_id: None,
            discovered_in_commit: None,
            introduced_in_commit: None,
            is_fixed: false,
            fixed_in_commit: None,
            raw_input: None,
            tokens_in: None,
            tokens_out: None,
            tokens_cached: None,
            duplicate_of_id: None,
            created_at: 100,
        }];

        let mut session = DedupSession {
            candidate_problem: "Memory leak in net/core/dev.c",
            candidate_locations: None,
            candidate_subsystems: &["net".to_string()],
            known_candidates: &known_bugs,
            context_tag: None,
        };

        let runner = SessionRunner::new(&mock_provider);
        let res = runner.run(&mut session).await.unwrap();
        assert!(res.output.is_duplicate);
        assert_eq!(res.output.duplicate_of_id, Some(42));
    }

    #[tokio::test]
    async fn test_dedup_session_prompt_directives() {
        let known_bugs = vec![Bug {
            verified_on_sha: None,
            id: 42,
            status: "verified".to_string(),
            bugid: "linux-42".to_string(),
            problem: "Memory leak in dev.c".to_string(),
            severity: Severity::High,
            severity_explanation: None,
            locations: None,
            subsystems: vec!["net".to_string()],
            source_files: None,
            inline_review: "".to_string(),
            logs: None,
            vector_json: None,
            discovered_in_patchset_id: None,
            discovered_in_patch_id: None,
            discovered_in_commit: None,
            introduced_in_commit: None,
            is_fixed: false,
            fixed_in_commit: None,
            raw_input: None,
            tokens_in: None,
            tokens_out: None,
            tokens_cached: None,
            duplicate_of_id: None,
            created_at: 100,
        }];

        let session = DedupSession {
            candidate_problem: "Use-after-free crash in dev.c due to race",
            candidate_locations: None,
            candidate_subsystems: &["net".to_string()],
            known_candidates: &known_bugs,
            context_tag: None,
        };

        let sys = session.system_prompt();
        assert!(sys.contains("same root cause but different consequences"));
        assert!(
            sys.contains("if fixing one issue will resolve the other issue, it's the same bug")
        );

        let user = session.initial_user_prompt();
        assert!(user.contains("same root cause but different consequences"));
        assert!(
            user.contains("If fixing one issue will resolve the other issue, it's the same bug")
        );
    }

    struct QueuedMockAiProvider {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
    }

    impl QueuedMockAiProvider {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses.into()),
            }
        }
    }

    #[async_trait]
    impl AiProvider for QueuedMockAiProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            let next_resp = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| "{}".to_string());
            Ok(AiResponse {
                content: Some(next_resp),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            100
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 8192,
            }
        }
    }

    #[tokio::test]
    async fn test_report_session() {
        let mock_provider = MockAiProvider {
            response_text: "In dev_alloc(), the allocated buffer is not freed on error.\n\n    int *ptr = alloc();\n    if (!ptr)\n        return -ENOMEM;\n\nThe buffer is not freed.\n".to_string(),
        };

        let mut session = ReportSession {
            problem: "Memory leak in net/core/dev.c",
            severity: "High",
            canonical_description: "Trigger: Netdev allocation failure.\nFailure Mechanism: Missing kfree() on error path.\nImpact: Memory leak.",
            severity_explanation: "Missing free",
            locations: None,
            introduced_in_commit: Some("11223344 (net: initial dev.c)"),
            tools: None,
            context_tag: None,
            prefetched_context: String::new(),
        };

        let runner = SessionRunner::new(&mock_provider);
        let res = runner.run(&mut session).await.unwrap();
        assert!(res.output.contains("int *ptr = alloc();"));
    }

    #[tokio::test]
    async fn test_report_session_prompt_rules() {
        let session = ReportSession {
            problem: "Integer overflow in sound/core/pcm_native.c",
            severity: "High",
            canonical_description: "Trigger: 32-bit architecture allocation.\nFailure Mechanism: Overflow.\nImpact: Memory corruption.",
            severity_explanation: "Buffer overflow",
            locations: None,
            introduced_in_commit: None,
            tools: None,
            context_tag: None,
            prefetched_context: String::new(),
        };

        let sys_prompt = session.system_prompt();
        assert!(sys_prompt.contains("On a 32-bit architecture"));
        assert!(sys_prompt.contains("<...>"));
        assert!(sys_prompt.contains("^^^^^"));
        assert!(sys_prompt.contains("Paired actions rule"));
        assert!(sys_prompt.contains("NEVER use carets to point at a missing thing"));
        assert!(sys_prompt.contains("75 characters per line"));
        assert!(sys_prompt.contains("CPU 0"));

        let user_prompt = session.initial_user_prompt();
        assert!(user_prompt.contains("32-bit machine"));
        assert!(user_prompt.contains("<...>"));
        assert!(user_prompt.contains("^^^^^"));
        assert!(user_prompt.contains("NEVER use carets to highlight a missing call"));
        assert!(user_prompt.contains("multi-CPU timeline diagram"));
    }

    #[tokio::test]
    async fn test_severity_session_uses_prompt_file_directly() {
        let session = SeveritySession {
            canonical_title: "net: memory leak in dev_alloc()",
            canonical_description: "Buffer allocated but not freed",
            locations: "[]",
            context_tag: None,
        };

        let sys_prompt = session.system_prompt();
        assert!(sys_prompt.contains("# Severity Levels"));
        assert!(sys_prompt.contains("## Calibrating the level"));
        assert!(sys_prompt.contains("## Critical"));
        assert!(sys_prompt.contains("## High"));
        assert!(sys_prompt.contains("## Medium"));
        assert!(sys_prompt.contains("## Low"));
        assert!(sys_prompt.contains("Output raw JSON only"));
    }

    #[tokio::test]
    async fn test_process_issue_flow() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        let thread_id = db.create_thread("t1", "subj", 100).await.unwrap();
        let ps_id = db
            .create_patchset(
                thread_id, None, "m1", "subj", "auth", 100, 1, 0, "", "", None, 1, None, false,
                None, None,
            )
            .await
            .unwrap()
            .unwrap();

        // 1. Normalization
        let normalize_json = json!({
            "canonical_title": "e1000: buffer overflow in e1000_clean_rx_irq()",
            "canonical_description": "Trigger: Jumbo frame without adequate skb buffer.\nMechanism: Unchecked memcpy into skb->data.\nImpact: Kernel memory corruption.",
            "affected_source_files": ["drivers/net/ethernet/intel/e1000/e1000_main.c"]
        }).to_string();

        // 2. Verification
        let verify_json = json!({
            "verification_reasoning": "Buffer overflow occurs when size exceeds MTU.",
            "is_false_positive": false,
            "refutation_evidence": null,
            "impact_severity": "Critical",
            "relevant_code_locations": [{"file": "drivers/net/ethernet/intel/e1000/e1000_main.c", "line": 250}]
        }).to_string();

        // 3. Tracing
        let tracing_json = json!({
            "introducing_commit_sha": "1234567890ab1234567890ab1234567890ab1234"
        })
        .to_string();

        // 4. Severity
        let severity_json = json!({
            "severity": "Critical",
            "severity_explanation": "Buffer overflow leading to potential RCE or kernel panic."
        })
        .to_string();

        // 5. Report
        let report_text = "e1000: buffer overflow in e1000_clean_rx_irq()\n\n    memcpy(skb->data, buf, size);\n\nPotential buffer overflow when size > MTU.\n".to_string();

        let provider = QueuedMockAiProvider::new(vec![
            normalize_json,
            verify_json,
            tracing_json,
            severity_json,
            report_text,
        ]);

        let input = BugInput {
            problem: "Buffer overflow in e1000 rx handler".to_string(),
            reasoning: "Size not checked against MTU".to_string(),
            locations: Some(
                json!([{"file": "drivers/net/ethernet/intel/e1000/e1000_main.c", "line": 250}]),
            ),
            subsystems: vec!["net/intel".to_string()],
            source_files: vec!["drivers/net/ethernet/intel/e1000/e1000_main.c".to_string()],
            commit_sha: Some("abcdef123456".to_string()),
            patchset_id: Some(ps_id),
            patch_id: None,
            baseline_sha: None,
        };

        let outcome = process_issue(&provider, None, &db, input.clone(), None)
            .await
            .unwrap();

        let final_outcome = match outcome {
            BugOutcome::NewlyDiscovered { ref bug } => {
                process_issue_worker(&provider, None, &db, bug, input.clone(), None)
                    .await
                    .unwrap()
            }
            _ => panic!("Expected NewlyDiscovered outcome initially"),
        };

        match final_outcome {
            BugOutcome::NewlyDiscovered { bug } => {
                assert_eq!(
                    bug.problem,
                    "e1000: buffer overflow in e1000_clean_rx_irq()"
                );
                assert_eq!(bug.severity, Severity::Critical);
                assert!(bug.bugid.starts_with("linux-"));
                assert!(uuid::Uuid::parse_str(&bug.bugid[6..]).is_ok());
                assert_eq!(bug.discovered_in_patchset_id, Some(ps_id));
                assert_eq!(bug.subsystems, vec!["net/intel".to_string()]);
                assert_eq!(
                    bug.introduced_in_commit.as_deref(),
                    Some("1234567890ab1234567890ab1234567890ab1234")
                );
                assert!(!bug.is_fixed);
                assert!(bug.fixed_in_commit.is_none());
                assert!(bug.logs.is_some(), "Logs must be populated");
                assert!(bug.raw_input.is_some(), "Raw input must be preserved");
            }
            _ => panic!("Expected NewlyDiscovered outcome, got {:?}", outcome),
        }
    }

    #[tokio::test]
    async fn test_process_issue_duplicate_after_verification_aborts_db_write() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&db_settings).await.unwrap();
        db.migrate().await.unwrap();

        let existing_vector = extract_bug_vector(
            "e1000: buffer overflow in e1000_clean_rx_irq()",
            &["net/intel".to_string()],
            &["drivers/net/ethernet/intel/e1000/e1000_main.c".to_string()],
            None,
        );

        let existing_id = db
            .create_bug(&NewBug {
                verified_on_sha: None,
                bugid: "linux-existing1".to_string(),
                status: "open".to_string(),
                problem: "e1000: buffer overflow in e1000_clean_rx_irq()".to_string(),
                severity: Severity::High,
                severity_explanation: Some("Known buffer overflow".to_string()),
                locations: None,
                subsystems: vec!["net/intel".to_string()],
                source_files: Some(vec![
                    "drivers/net/ethernet/intel/e1000/e1000_main.c".to_string(),
                ]),
                inline_review: "Inline review text".to_string(),
                logs: None,
                vector_json: Some(existing_vector.to_json()),
                discovered_in_patchset_id: None,
                discovered_in_patch_id: None,
                discovered_in_commit: None,
                introduced_in_commit: None,
                is_fixed: false,
                fixed_in_commit: None,
                raw_input: None,
                tokens_in: None,
                tokens_out: None,
                tokens_cached: None,
                duplicate_of_id: None,
                created_at: 1000,
            })
            .await
            .unwrap();

        let normalize_json = json!({
            "canonical_title": "e1000: buffer overflow in e1000_clean_rx_irq()",
            "canonical_description": "Trigger: Jumbo frame.\nMechanism: memcpy.\nImpact: Crash.",
            "affected_source_files": ["drivers/net/ethernet/intel/e1000/e1000_main.c"]
        })
        .to_string();

        let verify_json = json!({
            "verification_reasoning": "Buffer overflow verified.",
            "is_false_positive": false,
            "refutation_evidence": null,
            "impact_severity": "High",
            "relevant_code_locations": [{"file": "drivers/net/ethernet/intel/e1000/e1000_main.c", "line": 250}]
        })
        .to_string();

        let dedup_json = json!({
            "is_duplicate": true,
            "duplicate_of_id": existing_id,
            "reasoning": "Exact match with known bug #1 in e1000 driver"
        })
        .to_string();

        // Normalization, Verification, Dedup (and enrichment stages 4-6 are skipped!)
        let provider = QueuedMockAiProvider::new(vec![normalize_json, verify_json, dedup_json]);

        let input = BugInput {
            problem: "Buffer overflow in e1000 rx handler".to_string(),
            reasoning: "Size not checked against MTU".to_string(),
            locations: Some(
                json!([{"file": "drivers/net/ethernet/intel/e1000/e1000_main.c", "line": 250}]),
            ),
            subsystems: vec!["net/intel".to_string()],
            source_files: vec!["drivers/net/ethernet/intel/e1000/e1000_main.c".to_string()],
            commit_sha: Some("abcdef123456".to_string()),
            patchset_id: None,
            patch_id: None,
            baseline_sha: None,
        };

        let outcome = process_issue(&provider, None, &db, input.clone(), None)
            .await
            .unwrap();

        let final_outcome = match outcome {
            BugOutcome::NewlyDiscovered { ref bug } => {
                process_issue_worker(&provider, None, &db, bug, input.clone(), None)
                    .await
                    .unwrap()
            }
            _ => panic!("Expected NewlyDiscovered initially"),
        };

        match final_outcome {
            BugOutcome::Duplicate {
                existing_bug,
                reasoning,
                logs,
            } => {
                assert_eq!(existing_bug.id, existing_id);
                assert_eq!(existing_bug.bugid, "linux-existing1");
                assert_eq!(reasoning, "Exact match with known bug #1 in e1000 driver");
                assert!(logs.is_some());
            }
            _ => panic!("Expected Duplicate outcome, got {:?}", final_outcome),
        }
    }

    #[tokio::test]
    async fn test_prefetch_bug_locations_handles_none_or_malformed() {
        assert_eq!(prefetch_bug_locations(None, "master", &None).await, "");
        assert_eq!(
            prefetch_bug_locations(None, "master", &Some(json!([]))).await,
            ""
        );
        assert_eq!(
            prefetch_bug_locations(None, "master", &Some(json!("not an array"))).await,
            ""
        );
        assert_eq!(
            prefetch_bug_locations(None, "master", &Some(json!(42))).await,
            ""
        );
    }

    #[tokio::test]
    async fn test_prefetch_bug_locations_filters_dangerous_and_non_c_paths() {
        let temp = tempfile::tempdir().unwrap();
        let tb = Arc::new(ToolBox::new(temp.path().to_path_buf(), None));

        let dangerous_locations = json!([
            {"file": "../../etc/passwd", "line": 10},
            {"file": "/etc/shadow", "line": 20},
            {"file": "script.py", "line": 30},
            {"file": "binary.bin", "line": 40},
        ]);

        let res = prefetch_bug_locations(Some(&tb), "master", &Some(dangerous_locations)).await;
        assert_eq!(res, "");
    }

    #[tokio::test]
    async fn test_prefetch_bug_locations_extracts_enclosing_function_from_repo() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path();

        // Initialize a minimal git repository with a C source file
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(path)
            .output()
            .unwrap();

        let c_code = r#"#include <stdio.h>

static int target_kernel_func(int a, int b)
{
    if (a < 0) {
        return -1;
    }
    return a + b;
}
"#;
        std::fs::write(path.join("test_file.c"), c_code).unwrap();
        std::process::Command::new("git")
            .args(["add", "test_file.c"])
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(path)
            .output()
            .unwrap();

        let tb = Arc::new(ToolBox::new(path.to_path_buf(), None));
        let locations = json!([
            {
                "file": "test_file.c",
                "line": 5,
                "function_or_symbol": "target_kernel_func"
            }
        ]);

        let res = prefetch_bug_locations(Some(&tb), "HEAD", &Some(locations)).await;
        assert!(res.contains("--- test_file.c:5 (target_kernel_func) ---"));
        assert!(res.contains("static int target_kernel_func"));
        assert!(res.contains("return a + b;"));
    }

    #[test]
    fn test_parse_stack_trace_extracts_frames() {
        let trace = r#"
[  12.345678] ? e1000_clean_rx_irq+0x120/0x340 [e1000] drivers/net/ethernet/intel/e1000/e1000_main.c:456
Call Trace:
 <TASK>
 dev_queue_xmit+0x10/0x20 net/core/dev.c:3821
 ? e1000_clean_rx_irq+0x120/0x340 [e1000] drivers/net/ethernet/intel/e1000/e1000_main.c:456
 kernel_clone+0x9d/0x3a0 kernel/fork.c:2685
 </TASK>
"#;
        let frames = parse_stack_trace(trace);
        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames[0],
            (
                "drivers/net/ethernet/intel/e1000/e1000_main.c".to_string(),
                456
            )
        );
        assert_eq!(frames[1], ("net/core/dev.c".to_string(), 3821));
        assert_eq!(frames[2], ("kernel/fork.c".to_string(), 2685));
    }

    #[tokio::test]
    async fn test_prefetch_bug_locations_traces_file_rename() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path();

        std::process::Command::new("git")
            .args(["init"])
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(path)
            .output()
            .unwrap();

        let c_code = "int old_fn() {\n    return 42;\n}\n";
        std::fs::write(path.join("old_net.c"), c_code).unwrap();
        std::process::Command::new("git")
            .args(["add", "old_net.c"])
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add old_net.c"])
            .current_dir(path)
            .output()
            .unwrap();

        // Rename old_net.c -> new_net.c
        std::process::Command::new("git")
            .args(["mv", "old_net.c", "new_net.c"])
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "rename to new_net.c"])
            .current_dir(path)
            .output()
            .unwrap();

        let tb = Arc::new(ToolBox::new(path.to_path_buf(), None));
        let locations = json!([
            {
                "file": "old_net.c",
                "line": 2,
            }
        ]);

        let res = prefetch_bug_locations(Some(&tb), "HEAD", &Some(locations)).await;
        assert!(
            res.contains("return 42;"),
            "Prefetch must trace renamed file and extract snippet: {}",
            res
        );
    }

    #[tokio::test]
    async fn test_deterministic_blame_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path();

        std::process::Command::new("git")
            .args(["init"])
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(path)
            .output()
            .unwrap();

        let c_code = "int a = 1;\nint b = 2;\nint c = 3;\n";
        std::fs::write(path.join("file.c"), c_code).unwrap();
        std::process::Command::new("git")
            .args(["add", "file.c"])
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "initial commit"])
            .current_dir(path)
            .output()
            .unwrap();

        let head_commit = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .current_dir(path)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();

        let tb = Arc::new(ToolBox::new(path.to_path_buf(), None));
        let locations = json!([
            {
                "file": "file.c",
                "line": 2,
            }
        ]);

        let sha = deterministic_blame_fallback(Some(&tb), &Some(locations), "HEAD").await;
        assert_eq!(sha, Some(head_commit));
    }

    #[test]
    fn test_extract_title_prefix() {
        assert_eq!(
            extract_title_prefix("btrfs: use-after-free in cleanup()"),
            "btrfs"
        );
        assert_eq!(
            extract_title_prefix("net/sched: qdisc enqueue overflow"),
            "net/sched"
        );
        assert_eq!(extract_title_prefix("no_prefix_title"), "no_prefix_title");
        assert_eq!(extract_title_prefix("  mm:  null deref  "), "mm");
    }

    #[test]
    fn test_extract_directory_subsystems() {
        let files = vec![
            "fs/btrfs/ordered-data.c".to_string(),
            "drivers/net/ethernet/intel/e1000/e1000_main.c".to_string(),
            "arch/x86/kernel/cpu/common.c".to_string(),
            "kernel/sched/core.c".to_string(),
        ];
        let subs = extract_directory_subsystems(&files);
        assert_eq!(
            subs,
            vec![
                "fs/btrfs".to_string(),
                "drivers/net".to_string(),
                "arch/x86".to_string(),
                "kernel/sched".to_string()
            ]
        );

        let empty: Vec<String> = Vec::new();
        assert_eq!(
            extract_directory_subsystems(&empty),
            vec!["kernel".to_string()]
        );
    }

    #[test]
    fn test_normalize_session_validation_rejects_missing_colon() {
        let mut session = NormalizeSession {
            problem: "problem",
            reasoning: "reasoning",
            locations: "[]",
            master_sha: "master",
            maintainers_hint: None,
            tools: None,
            context_tag: None,
        };

        let response = AiResponse {
            content: Some(
                json!({
                    "canonical_title": "missing colon in title",
                    "canonical_description": "Description",
                    "affected_source_files": ["file.c"]
                })
                .to_string(),
            ),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            usage: None,
            truncated: false,
        };
        let err = session.validate(&response).unwrap_err();
        match err {
            ValidationError::FormatViolation(msg) => {
                assert!(
                    msg.contains(
                        "must follow the format '<subsystem_prefix>: <defect description>'"
                    )
                );
            }
            _ => panic!("Expected FormatViolation for missing colon"),
        }
    }

    #[test]
    fn test_normalize_session_validation_rejects_empty_files() {
        let mut session = NormalizeSession {
            problem: "problem",
            reasoning: "reasoning",
            locations: "[]",
            master_sha: "master",
            maintainers_hint: None,
            tools: None,
            context_tag: None,
        };

        let response = AiResponse {
            content: Some(
                json!({
                    "canonical_title": "btrfs: memory leak in alloc()",
                    "canonical_description": "Description",
                    "affected_source_files": []
                })
                .to_string(),
            ),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            usage: None,
            truncated: false,
        };
        let err = session.validate(&response).unwrap_err();
        match err {
            ValidationError::FormatViolation(msg) => {
                assert!(msg.contains("affected_source_files cannot be empty"));
            }
            _ => panic!("Expected FormatViolation for empty affected_source_files"),
        }
    }

    #[tokio::test]
    async fn test_deterministic_subsystem_resolution_with_maintainers() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path();

        let maintainers_content = r#"Maintainers List
===================

BTRFS FILE SYSTEM
M:	Chris Mason <clm@fb.com>
L:	linux-btrfs@vger.kernel.org
S:	Maintained
F:	fs/btrfs/

INTEL E1000 NETWORK DRIVER
M:	Jesse Brandeburg <jesse.brandeburg@intel.com>
L:	netdev@vger.kernel.org
S:	Supported
F:	drivers/net/ethernet/intel/e1000/
"#;
        std::fs::write(path.join("MAINTAINERS"), maintainers_content).unwrap();

        let mindex = crate::maintainers::MaintainersIndex::from_repo(path).unwrap();
        let matched = mindex.match_files([
            "fs/btrfs/inode.c",
            "drivers/net/ethernet/intel/e1000/e1000_main.c",
        ]);
        assert_eq!(
            matched,
            vec![
                "BTRFS FILE SYSTEM".to_string(),
                "INTEL E1000 NETWORK DRIVER".to_string()
            ]
        );
    }
}
