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
//! 4. Standalone LKML-style inline review generation for newly discovered bugs.
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub primary_subsystem: String,
    pub affected_source_files: Vec<String>,
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
    input: &'a BugInput,
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
            TOOL USAGE DIRECTIVES:\n\
            - Actively batch parallel or independent tool calls into a single response when possible to minimize turns.\n\
            - If tool output is truncated ('truncated': true), page only if directly relevant.\n\
            - Scope your investigation strictly to the reported functions, immediate error handling paths, and direct caller contracts. Do NOT attempt open-ended whole-kernel call-graph or destructor traversals.\n\n\
            CRITICAL VALIDATION FILTER: You must assess if the bug is genuine. Do not give the code the benefit of the doubt. To mark an issue as a false positive (is_false_positive=true), you must find concrete proof in the local codebase that the described conditions are impossible, unreachable, or already safely handled. If you cannot prove it is false, verify the code locations and provide your step-by-step reasoning in verification_reasoning."
        )
    }

    fn initial_user_prompt(&self) -> String {
        let loc_str = self
            .input
            .locations
            .as_ref()
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

        format!(
            "Candidate Vulnerability:
Problem: {problem}
Reasoning: {reasoning}
Locations:
{locations}
{prefetch_block}
Task:
1. Verify the problem against the mainline code shown above and top-of-trunk of Linus's main tree (commit `{master_sha}`). IMPORTANT: Use this exact `{master_sha}` SHA in any tool calls instead of `HEAD` or `master` to check the actual top-of-trunk.
2. Scope your verification to the reported functions, immediate error handling paths, and direct caller contracts. Do not wander across unrelated drivers or files.
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
            problem = self.input.problem,
            reasoning = self.input.reasoning,
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
    verification_reasoning: &'a str,
    verified_locations: &'a str,
    maintainers_hint: Option<String>,
    tools: Option<Arc<ToolBox>>,
    context_tag: Option<String>,
}

#[async_trait]
impl LlmSession for NormalizeSession<'_> {
    type Output = NormalizationJson;

    fn system_prompt(&self) -> String {
        "You are an expert Linux kernel maintainer and technical editor. Your role is to normalize a newly verified Linux kernel defect into canonical form.\n\
        You must standardize the defect's title, subsystem classification, and structured description without altering the technical substance verified in the code.\n\
        Use available tools (git_log, git_read_files, git_grep) to inspect the codebase and git history of affected files to determine the conventional subsystem prefix used by maintainers."
            .to_string()
    }

    fn initial_user_prompt(&self) -> String {
        let hint_section = self
            .maintainers_hint
            .as_ref()
            .map(|h| format!("\n{}\n", h))
            .unwrap_or_default();

        format!(
            "Verified Bug Details:
Original Problem: {problem}
Reasoning: {reasoning}
Verification Evidence: {ver_reasoning}
Verified Locations:
{locations}
{hint}
Task:
1. Determine the canonical subsystem for this defect (e.g. 'btrfs', 'net/sched', 'bpf', 'drm/i915', 'sched', 'mm'). Use git_log on the affected file(s) to observe the standard commit prefix used by kernel maintainers.
2. Formulate a canonical title matching Linux kernel standards: '<subsystem>: <root cause in function_name()>' (strict limit of under 80 characters, NO backticks, NO markdown).
3. Provide a structured canonical description summarizing:
   - Trigger / Preconditions: Conditions required to trigger the bug.
   - Failure Mechanism: Step-by-step execution path causing the fault.
   - Impact: Consequence of the failure (e.g. UAF, leak, deadlock, panic).
4. List the affected source files.

Return ONLY a valid JSON object matching this schema:
{{
  \"canonical_title\": \"btrfs: use-after-free in btrfs_cleanup_ordered_extents()\",
  \"canonical_description\": \"Trigger / Preconditions: ...\\nFailure Mechanism: ...\\nImpact: ...\",
  \"primary_subsystem\": \"btrfs\",
  \"affected_source_files\": [\"fs/btrfs/ordered-data.c\"]
}}",
            problem = self.problem,
            reasoning = self.reasoning,
            ver_reasoning = self.verification_reasoning,
            locations = self.verified_locations,
            hint = hint_section
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
        if parsed.canonical_title.trim().is_empty() {
            return Err(ValidationError::FormatViolation(
                "canonical_title cannot be empty".into(),
            ));
        }
        if parsed.primary_subsystem.trim().is_empty() {
            return Err(ValidationError::FormatViolation(
                "primary_subsystem cannot be empty".into(),
            ));
        }
        Ok(parsed)
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
                "- Bug ID {}: [Slug: {}] [Severity: {}] [Subsystems: {}]\n  Problem: {}\n  Affected Files: {}\n\n",
                bug.id,
                bug.slug,
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
        "You are an expert Linux kernel security engineer and maintainer. Assess the severity and impact of a verified defect in the Linux kernel.\n\
        Evaluate whether the issue leads to memory corruption, privilege escalation, denial of service, data loss, or information leakage.\n\
        Output raw JSON only."
            .to_string()
    }

    fn initial_user_prompt(&self) -> String {
        format!(
            "Verified Linux Kernel Vulnerability:
Title: {title}
Description:
{description}
Code Locations:
{locations}

Task:
Assess the severity of this defect and provide an explanation.
Options for severity: \"Low\", \"Medium\", \"High\", \"Critical\", or \"Unknown\" (use \"Unknown\" if prerequisites or exploitability cannot be definitively determined).

Return ONLY a valid JSON object matching:
{{
  \"severity\": \"High\",
  \"severity_explanation\": \"Explain attack prerequisites, required privileges, and blast radius...\"
}}",
            title = self.canonical_title,
            description = self.canonical_description,
            locations = self.locations
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
// 6. Standalone Inline Review Generation Session (Enrichment)
// ---------------------------------------------------------------------------

struct ReportSession<'a> {
    problem: &'a str,
    severity: &'a str,
    severity_explanation: &'a str,
    locations: Option<&'a Value>,
    introduced_in_commit: Option<&'a str>,
    context_tag: Option<String>,
}

#[async_trait]
impl LlmSession for ReportSession<'_> {
    type Output = String;

    fn system_prompt(&self) -> String {
        r#"You are an automated review bot generating a dedicated, standalone defect report for a vulnerability discovered in the codebase.
Generate a concise, short but detailed description of the problem, straight to the point.

CRITICAL RULES:
1. Follow standard plain-text email style. Ensure the tone is objective and professional. Do not use conversational filler (e.g. "While reviewing... I noticed" or "This is a Linux kernel issue").
2. The report MUST contain a detailed description of the bug (describing execution paths, context, and root causes).
3. Do NOT use markdown code fences AND do not use quote marks ('>') for code.
4. If you present a code snippet, put filename and line range first on a separate line and use a left margin (4 spaces of indentation) to separate it from the normal text.
5. Do NOT use backticks to quote code, function, or symbol names.
6. Explain the exact execution trigger and state precisely what goes wrong.
7. Any text explanation MUST be wrapped at 78 characters. Do not wrap code snippets.

EXAMPLE CODE FORMATTING:

fs/btrfs/send.c:2333-2338
    nce = name_cache_search(sctx, ino, gen);
    if (nce) {
        if (ino < sctx->send_progress && nce->need_later_update) {
            btrfs_lru_cache_remove(&sctx->name_cache, &nce->entry);
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
        let status_str = "Mainline Status: Active / Unfixed in top-of-trunk\n".to_string();

        format!(
            "Linux Kernel Vulnerability Details:\n\
            Problem: {}\n\
            Severity: {}\n\
            Severity Explanation: {}\n\
            {}{}\
            Locations:\n{}\n\n\
            Task:\n\
            Generate a complete, standalone LKML-style review comment block for this issue.\n\
            Format problematic source lines with 4 spaces of indentation and provide interspersed explanations and remediation suggestions.\n\
            Return raw plain text, not JSON or markdown fences.",
            self.problem, self.severity, self.severity_explanation, intro_str, status_str, loc_str
        )
    }

    fn tools(&self) -> Option<Vec<AiTool>> {
        None
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

/// Generates a random unique UUID for a newly discovered Linux kernel bug.
pub fn generate_slug() -> String {
    uuid::Uuid::new_v4().to_string()
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
    let slug = generate_slug();
    let raw_input_json = serde_json::to_string(&input).ok();
    let new_bug = NewBug {
        slug: slug.clone(),
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
        std::process::Command::new("git")
            .current_dir(tb.get_worktree_path())
            .args(["rev-parse", "master"])
            .output()
            .ok()
            .and_then(|output| {
                if output.status.success() {
                    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "master".to_string())
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

        format!("{} ({}){}", sha, subject, tag_part)
    })
    .await
    .ok()
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

            let Ok(out) = git_output else {
                continue;
            };
            if !out.status.success() {
                continue;
            }

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
                        .map(|f| f.trim() == file)
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

                let snippet_opt = if let Some(line) = line_opt {
                    if line >= 1 && line <= lines.len() {
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
                            Some((
                                clamped_text,
                                name.or_else(|| sym_opt.map(str::to_string)),
                                line,
                            ))
                        } else {
                            let start = line.saturating_sub(30).max(1);
                            let end = (start + MAX_BUG_SNIPPET_LINES).min(lines.len());
                            let start_0 = start.saturating_sub(1);
                            let text = lines[start_0..end].join("\n");
                            Some((text, sym_opt.map(str::to_string), line))
                        }
                    } else {
                        None
                    }
                } else if let Some(sym) = sym_opt {
                    let found_idx = lines.iter().position(|l| l.contains(sym));
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
                            Some((clamped_text, name.or_else(|| Some(sym.to_string())), line))
                        } else {
                            let start = line.saturating_sub(20).max(1);
                            let end = (start + MAX_BUG_SNIPPET_LINES).min(lines.len());
                            let start_0 = start.saturating_sub(1);
                            let text = lines[start_0..end].join("\n");
                            Some((text, Some(sym.to_string()), line))
                        }
                    } else {
                        None
                    }
                } else {
                    None
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
    let runner = SessionRunner::new(provider).with_max_turns(20);
    let master_sha = get_master_sha(tools.as_ref()).await;

    // Stage 1: Verification & Ground-Truth Confirmation
    info!("--- Stage 1: Verification ---");
    let prefetched_context =
        prefetch_bug_locations(tools.as_ref(), &master_sha, &input.locations).await;
    let mut verify_session = VerifySession {
        input: &input,
        master_sha: master_sha.clone(),
        tools: tools.clone(),
        context_tag: context_tag.map(|s| s.to_string()),
        prefetched_context,
    };

    let verify_result = runner.run(&mut verify_session).await?;
    full_history.extend(verify_result.history);
    let verification = verify_result.output;
    info!(
        "Stage 1 Complete: Verification returned is_false_positive={}",
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
                severity_explanation: Some(&reason),
                logs: Some(&logs),
                verified_on_sha: Some(&master_sha),
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

    // Stage 2: Normalization & Canonical Naming
    info!("--- Stage 2: Normalization ---");
    let maintainers_hint = tools
        .as_ref()
        .and_then(|tb| crate::maintainers::MaintainersIndex::from_repo(tb.get_worktree_path()).ok())
        .map(|mindex| {
            let matched = mindex.match_files(&input.source_files);
            if matched.is_empty() {
                String::new()
            } else {
                format!(
                    "Detected Subsystems from MAINTAINERS: {}",
                    matched.join(", ")
                )
            }
        });

    let mut norm_session = NormalizeSession {
        problem: &input.problem,
        reasoning: &input.reasoning,
        verification_reasoning: &verification.verification_reasoning,
        verified_locations: &verified_locations_str,
        maintainers_hint,
        tools: tools.clone(),
        context_tag: context_tag.map(|s| s.to_string()),
    };

    let norm_result = runner.run(&mut norm_session).await?;
    full_history.extend(norm_result.history);
    let norm = norm_result.output;
    info!(
        "Stage 2 Complete: Normalized to '{}' in subsystem '{}'",
        norm.canonical_title, norm.primary_subsystem
    );

    // Stage 3: Deduplication Confirmation (under BUG_DEDUP_LOCK)
    info!("--- Stage 3: Deduplication ---");
    let query_vector = extract_bug_vector(
        &norm.canonical_title,
        std::slice::from_ref(&norm.primary_subsystem),
        &norm.affected_source_files,
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
                candidate_subsystems: std::slice::from_ref(&norm.primary_subsystem),
                known_candidates: &candidate_bugs,
                context_tag: context_tag.map(|s| s.to_string()),
            };

            let dedup_result = runner.run(&mut dedup_session).await?;
            full_history.extend(dedup_result.history);
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
                    existing.id, existing.slug
                );
                let logs = serde_json::to_string(&full_history).unwrap_or_default();
                let dup_meta = serde_json::json!({
                    "duplicate_of_slug": existing.slug,
                    "duplicate_of_id": existing.id,
                    "reasoning": dedup.reasoning
                });
                let dup_meta_str = serde_json::to_string(&dup_meta).unwrap_or_default();

                db.mark_bug_as_duplicate(bug_row.id, existing.id, &dup_meta_str, Some(&logs))
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
    let introduced_in_commit =
        format_commit(tools.as_ref(), tracing_result.output.introducing_commit_sha).await;

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
    let severity_output = severity_result.output;
    let severity = Severity::from_str(&severity_output.severity);

    // Stage 6: Standalone Plaintext LKML Report (Enrichment)
    info!("--- Stage 6: Standalone Review Generation ---");
    let mut report_session = ReportSession {
        problem: &norm.canonical_title,
        severity: severity.as_str(),
        severity_explanation: &severity_output.severity_explanation,
        locations: verified_locations.as_ref(),
        introduced_in_commit: introduced_in_commit.as_deref(),
        context_tag: context_tag.map(|s| s.to_string()),
    };
    let report_result = runner.run(&mut report_session).await?;
    full_history.extend(report_result.history);
    let inline_review = report_result.output;

    // Stage 7: Final Database Write
    info!("--- Stage 7: Final Database Write ---");
    let logs_json = serde_json::to_string(&full_history).ok();
    let norm_subsystems = vec![norm.primary_subsystem];

    db.update_bug_outcome(
        bug_row.id,
        crate::db::UpdateBugOutcomeParams {
            status: "open",
            problem: Some(&norm.canonical_title),
            subsystems: Some(&norm_subsystems),
            source_files: Some(&norm.affected_source_files),
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
        },
    )
    .await?;

    let saved_bug = db.get_bug(bug_row.id).await?.expect("Saved bug must exist");
    info!(
        "Successfully registered newly verified Linux kernel bug #{} ({})",
        bug_row.id, bug_row.slug
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
            input: &input,
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

        let verify_json = json!({
            "verification_reasoning": "Caller checks pointer validity before invocation, so NULL dereference is impossible.",
            "is_false_positive": true,
            "refutation_evidence": "Guarded by if (ptr) in caller at dev.c:85",
            "impact_severity": null,
            "relevant_code_locations": null
        }).to_string();

        let provider = QueuedMockAiProvider::new(vec![verify_json]);

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
    }

    #[tokio::test]
    async fn test_normalize_session() {
        let mock_provider = MockAiProvider {
            response_text: json!({
                "canonical_title": "net: dev: fix memory leak in dev_alloc()",
                "canonical_description": "Trigger: Netdev allocation failure.\nFailure Mechanism: Missing kfree() on error path.\nImpact: Memory leak.",
                "primary_subsystem": "net",
                "affected_source_files": ["net/core/dev.c"]
            })
            .to_string(),
        };

        let mut session = NormalizeSession {
            problem: "Memory leak in net/core/dev.c",
            reasoning: "Allocated buffer not freed on error path",
            verification_reasoning: "Buffer allocated by dev_alloc() is not freed.",
            verified_locations: "[{\"file\": \"net/core/dev.c\", \"line\": 100}]",
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
            "net: dev: fix memory leak in dev_alloc()"
        );
        assert_eq!(res.output.primary_subsystem, "net");
        assert_eq!(res.output.affected_source_files, vec!["net/core/dev.c"]);
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
            slug: "pb-42".to_string(),
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
            response_text: "This is a Linux kernel issue in the codebase.\n\n    int *ptr = alloc();\n    if (!ptr)\n        return -ENOMEM;\n\nThe buffer is not freed.\n".to_string(),
        };

        let mut session = ReportSession {
            problem: "Memory leak in net/core/dev.c",
            severity: "High",
            severity_explanation: "Missing free",
            locations: None,
            introduced_in_commit: Some("11223344 (net: initial dev.c)"),
            context_tag: None,
        };

        let runner = SessionRunner::new(&mock_provider);
        let res = runner.run(&mut session).await.unwrap();
        assert!(res.output.contains("int *ptr = alloc();"));
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

        // 1. Verification
        let verify_json = json!({
            "verification_reasoning": "Buffer overflow occurs when size exceeds MTU.",
            "is_false_positive": false,
            "refutation_evidence": null,
            "impact_severity": "Critical",
            "relevant_code_locations": [{"file": "drivers/net/ethernet/intel/e1000/e1000_main.c", "line": 250}]
        }).to_string();

        // 2. Normalization
        let normalize_json = json!({
            "canonical_title": "e1000: buffer overflow in e1000_clean_rx_irq()",
            "canonical_description": "Trigger: Jumbo frame without adequate skb buffer.\nMechanism: Unchecked memcpy into skb->data.\nImpact: Kernel memory corruption.",
            "primary_subsystem": "net/intel",
            "affected_source_files": ["drivers/net/ethernet/intel/e1000/e1000_main.c"]
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
            verify_json,
            normalize_json,
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
            subsystems: vec!["net:intel".to_string()],
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
                assert!(uuid::Uuid::parse_str(&bug.slug).is_ok());
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
                slug: "pb-existing1".to_string(),
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
                created_at: 1000,
            })
            .await
            .unwrap();

        let verify_json = json!({
            "verification_reasoning": "Buffer overflow verified.",
            "is_false_positive": false,
            "refutation_evidence": null,
            "impact_severity": "High",
            "relevant_code_locations": [{"file": "drivers/net/ethernet/intel/e1000/e1000_main.c", "line": 250}]
        })
        .to_string();

        let normalize_json = json!({
            "canonical_title": "e1000: buffer overflow in e1000_clean_rx_irq()",
            "canonical_description": "Trigger: Jumbo frame.\nMechanism: memcpy.\nImpact: Crash.",
            "primary_subsystem": "net/intel",
            "affected_source_files": ["drivers/net/ethernet/intel/e1000/e1000_main.c"]
        })
        .to_string();

        let dedup_json = json!({
            "is_duplicate": true,
            "duplicate_of_id": existing_id,
            "reasoning": "Exact match with known bug #1 in e1000 driver"
        })
        .to_string();

        // Verification, Normalization, Dedup (and enrichment stages 4-6 are skipped!)
        let provider = QueuedMockAiProvider::new(vec![verify_json, normalize_json, dedup_json]);

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
                assert_eq!(existing_bug.slug, "pb-existing1");
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
}
