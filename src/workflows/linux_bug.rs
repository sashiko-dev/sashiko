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

use crate::api::{BugInput, BugOutcome};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::{info, warn};

use crate::ai::session::{LlmSession, SessionRunner, ValidationError};
use crate::ai::vector_search::{
    DEFAULT_SIMILARITY_THRESHOLD, DEFAULT_TOP_CANDIDATES, extract_bug_vector, find_top_candidates,
};
use crate::ai::{AiProvider, AiResponse, AiResponseFormat, AiTool, ToolCall};
use crate::db::{AttributedSubsystem, Bug, Database, NewBug, Severity};
use crate::project::ProjectId;
use crate::toolbox::ToolBox;

/// Named stages of the Linux kernel bug pipeline, in execution order.
///
/// Every stage announces itself at the top of its first user prompt, so the
/// stored interaction log shows exactly where each stage begins and ends, the
/// same way the patch review pipeline labels its stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BugStage {
    Normalization,
    Verification,
    Deduplication,
    OriginTracing,
    SeverityAssessment,
    ReportGeneration,
}

impl BugStage {
    /// Every stage in execution order.
    pub const ALL: [BugStage; 6] = [
        BugStage::Normalization,
        BugStage::Verification,
        BugStage::Deduplication,
        BugStage::OriginTracing,
        BugStage::SeverityAssessment,
        BugStage::ReportGeneration,
    ];

    /// Stable machine-readable identifier used to build enrichment kinds.
    pub const fn id(self) -> &'static str {
        match self {
            BugStage::Normalization => "normalization",
            BugStage::Verification => "verification",
            BugStage::Deduplication => "deduplication",
            BugStage::OriginTracing => "origin_tracing",
            BugStage::SeverityAssessment => "severity_assessment",
            BugStage::ReportGeneration => "report_generation",
        }
    }

    /// Human-readable stage name shown in the interface.
    pub const fn title(self) -> &'static str {
        match self {
            BugStage::Normalization => "Normalization",
            BugStage::Verification => "Verification",
            BugStage::Deduplication => "Deduplication",
            BugStage::OriginTracing => "Origin tracing",
            BugStage::SeverityAssessment => "Severity assessment",
            BugStage::ReportGeneration => "Report generation",
        }
    }

    /// Heading that opens the stage in the interaction log.
    ///
    /// Names the stage rather than numbering it, because a position in the
    /// pipeline stops being true the moment a stage is inserted ahead of it,
    /// and this string is read by people, by the model and by the interface.
    pub fn heading(self) -> String {
        format!("# {}", self.title())
    }

    /// Enrichment kind under which the stage interactions are persisted.
    pub fn enrichment_kind(self) -> String {
        format!("{}_run", self.id())
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
    project: ProjectId,
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
        match self.project {
            ProjectId::Linux => format!(
                "Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references strictly on this current date.\n\n\
                You are an expert Linux kernel maintainer. Your task is to rigorously verify a candidate Linux kernel defect or vulnerability against the top-of-trunk of Linus Torvalds' main Linux kernel tree.\n\
                Use available tools (git_read_files, git_grep, git_blame, git_log, git_show, git_diff) to inspect the mainline codebase, verify call chains, and confirm whether this defect exists.\n\n\
                CRITICAL VALIDATION FILTER: You must assess if the bug is genuine. Do not give the code the benefit of the doubt. To mark an issue as a false positive (is_false_positive=true), you must find concrete proof in the local codebase that the described conditions are impossible, unreachable, or already safely handled. If you cannot prove it is false, verify the code locations and provide your step-by-step reasoning in verification_reasoning."
            ),
            ProjectId::Sashiko => format!(
                "Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references strictly on this current date.\n\n\
                You are an expert Sashiko maintainer and Rust systems engineer. Your task is to rigorously verify a candidate Sashiko defect or vulnerability against the top-of-trunk of the Sashiko repository (`origin/main`).\n\
                Use available tools (git_read_files, git_grep, git_blame, git_log, git_show, git_diff) to inspect the mainline codebase, verify call chains, and confirm whether this defect exists.\n\n\
                CRITICAL VALIDATION FILTER: You must assess if the bug is genuine. Do not give the code the benefit of the doubt. To mark an issue as a false positive (is_false_positive=true), you must find concrete proof in the local codebase that the described conditions are impossible, unreachable, or already safely handled. If you cannot prove it is false, verify the code locations and provide your step-by-step reasoning in verification_reasoning."
            ),
        }
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

        match self.project {
            ProjectId::Linux => format!(
                "{stage_heading}

Candidate Defect to Verify:
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
                stage_heading = BugStage::Verification.heading(),
                master_sha = self.master_sha,
                title = self.title,
                subsystem = self.subsystem,
                description = self.description,
                locations = loc_str,
                prefetch_block = prefetch_block,
            ),
            ProjectId::Sashiko => format!(
                "{stage_heading}

Candidate Defect to Verify:
Title: {title}
Subsystem: {subsystem}
{files_str}Description:
{description}
Locations:
{locations}
{prefetch_block}
Task:
1. Verify the problem against the mainline code shown above and top-of-trunk of the Sashiko `main` branch (commit `{master_sha}`). IMPORTANT: Use this exact `{master_sha}` SHA in any tool calls instead of `HEAD` or `main` to check the actual top-of-trunk.
2. Scope your verification to the relevant functions and modules. Do not wander across unrelated files.
3. Determine if the issue is a genuine, reachable defect in the codebase.
4. If the defect is hallucinated, or a false positive that you can prove based on the code is impossible or safely handled, set \"is_false_positive\": true, provide concrete proof in \"refutation_evidence\", and summarize in \"verification_reasoning\".
5. If it is a confirmed bug, set \"is_false_positive\": false, \"refutation_evidence\": null, provide your step-by-step proof in \"verification_reasoning\", carry forward and refine the verified code locations in \"relevant_code_locations\", and optionally suggest an \"impact_severity\" (\"Low\", \"Medium\", \"High\", \"Critical\", or \"Unknown\").

EFFICIENCY LIMIT REQUIREMENT: Limit your investigation to the core defect. Do not trace unrelated history. You have a strict limit on tool calls; be extremely efficient instead of wandering the history.

Return ONLY a valid JSON object matching this schema:
{{
  \"verification_reasoning\": \"1. Call chain... 2. Condition...\",
  \"is_false_positive\": false,
  \"refutation_evidence\": null,
  \"impact_severity\": \"High\",
  \"relevant_code_locations\": [ {{\"file\": \"src/reviewer.rs\", \"function_or_symbol\": \"function_name\", \"line\": 123}} ]
}}",
                stage_heading = BugStage::Verification.heading(),
                master_sha = self.master_sha,
                title = self.title,
                subsystem = self.subsystem,
                description = self.description,
                locations = loc_str,
                prefetch_block = prefetch_block,
            ),
        }
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
    project: ProjectId,
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
        match self.project {
            ProjectId::Linux => format!(
                "You are an expert Linux kernel maintainer and technical editor. Your role is to normalize a candidate Linux kernel defect into canonical form.\n\
                You must standardize the defect's title, describe the technical substance, and identify the verified affected source files and symbols.\n\
                The target codebase is Linus Torvalds' mainline Linux kernel tree at top-of-trunk commit `{master_sha}`.\n\
                Use available tools (git_read_files, git_log, git_grep) to inspect the codebase at `{master_sha}`. Specifically:\n\
                - Use git_read_files with revision: \"{master_sha}\" or git_grep to inspect source code and identify affected files and symbols.\n\
                - Use git_log with range: \"{master_sha}\" on affected files to observe the conventional subsystem commit prefix used by maintainers (e.g. 'btrfs:', 'net:', 'mm:', 'drm/i915:').",
                master_sha = self.master_sha
            ),
            ProjectId::Sashiko => format!(
                "You are an expert Sashiko maintainer and technical editor. Your role is to normalize a candidate Sashiko defect into canonical form.\n\
                You must standardize the defect's title, describe the technical substance, and identify the verified affected source files and symbols.\n\
                The target codebase is the Sashiko repository at top-of-trunk commit `{master_sha}`.\n\
                Use available tools (git_read_files, git_log, git_grep) to inspect the codebase at `{master_sha}`. Specifically:\n\
                - Use git_read_files with revision: \"{master_sha}\" or git_grep to inspect source code and identify affected files and symbols.\n\
                - Use git_log with range: \"{master_sha}\" on affected files to observe the conventional module commit prefix used in Sashiko (e.g. 'workflows:', 'db:', 'reviewer:', 'api:', 'toolbox:', 'worker:').",
                master_sha = self.master_sha
            ),
        }
    }

    fn initial_user_prompt(&self) -> String {
        let hint_section = self
            .maintainers_hint
            .as_ref()
            .map(|h| format!("\n{}\n", h))
            .unwrap_or_default();

        match self.project {
            ProjectId::Linux => format!(
                "{stage_heading}

Candidate Bug Details:
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
                stage_heading = BugStage::Normalization.heading(),
                problem = self.problem,
                reasoning = self.reasoning,
                locations = self.locations,
                hint = hint_section,
                master_sha = self.master_sha
            ),
            ProjectId::Sashiko => format!(
                "{stage_heading}

Candidate Bug Details:
Original Problem: {problem}
Reasoning: {reasoning}
Reported Locations:
{locations}
{hint}
Target Mainline Commit: {master_sha}

Task:
1. Determine the conventional module commit prefix for this defect (e.g. 'workflows', 'db', 'reviewer', 'api', 'toolbox', 'worker', 'cli', 'prompts', 'settings', 'forge'). Use git_log on the affected file(s) at revision '{master_sha}' to observe the standard commit prefix used in Sashiko.
2. Formulate a canonical title matching Sashiko commit conventions: '<subsystem_prefix>: <defect or broken invariant in function_name()>' (strict limit of under 80 characters, NO backticks, NO markdown).
   - CRITICAL: This is a bug report title describing an existing defect, NOT a patch or commit title. Do NOT use patch/fix action verbs like 'fix', 'resolve', 'prevent', 'avoid', or 'handle'. State the defect directly (e.g. 'reviewer: leaked worktree on early return in run_review()', NEVER 'reviewer: fix leaked worktree in run_review()').
3. Provide a detailed, structured canonical description:
   - Trigger / Preconditions: Specific conditions, inputs, or states required to trigger the defect.
   - Call Chain / Execution Path: Detail the complete chain of events/calls leading up to the problem.
   - Failure Mechanism: Detail the exact root cause and how the fault, state inconsistency, or resource leak occurs.
   - Impact: Consequence of the failure (e.g. stuck review, panic, database lock contention, leaked worktree, silent data drop).
4. Verify and list the affected source files and symbols in the mainline tree at commit '{master_sha}'.

Return ONLY a valid JSON object matching this schema:
{{
  \"canonical_title\": \"reviewer: leaked worktree on early return in run_review()\",
  \"canonical_description\": \"Trigger / Preconditions: ...\\nFailure Mechanism: ...\\nImpact: ...\",
  \"affected_source_files\": [\"src/reviewer.rs\"],
  \"affected_symbols\": [\"run_review\"]
}}",
                stage_heading = BugStage::Normalization.heading(),
                problem = self.problem,
                reasoning = self.reasoning,
                locations = self.locations,
                hint = hint_section,
                master_sha = self.master_sha
            ),
        }
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
    extract_directory_subsystems_for_project(files, ProjectId::Linux)
}

/// Extracts coarse directory-based subsystem prefixes for `project` from file paths.
pub fn extract_directory_subsystems_for_project(
    files: &[String],
    project: ProjectId,
) -> Vec<String> {
    let mut subs = Vec::new();
    for file in files {
        let parts: Vec<&str> = file.split('/').filter(|p| !p.is_empty()).collect();
        let sub = match project {
            ProjectId::Linux => {
                if parts.len() >= 2 {
                    format!("{}/{}", parts[0], parts[1])
                } else if !parts.is_empty() {
                    parts[0].to_string()
                } else {
                    continue;
                }
            }
            ProjectId::Sashiko => {
                if parts.first() == Some(&"src") && parts.len() >= 2 {
                    parts[1].strip_suffix(".rs").unwrap_or(parts[1]).to_string()
                } else if parts.len() >= 2 {
                    parts[0].to_string()
                } else if !parts.is_empty() {
                    parts[0].strip_suffix(".rs").unwrap_or(parts[0]).to_string()
                } else {
                    continue;
                }
            }
        };
        if !subs.contains(&sub) {
            subs.push(sub);
        }
    }
    if subs.is_empty() {
        match project {
            ProjectId::Linux => vec!["kernel".to_string()],
            ProjectId::Sashiko => vec!["sashiko".to_string()],
        }
    } else {
        subs
    }
}

/// Resolves the subsystems to record against a bug, retaining where each name
/// came from.
///
/// A MAINTAINERS match is the only origin that names a real maintainer, so the
/// provenance has to travel with the name from the moment it is produced. The
/// lookup order is unchanged: an available index wins, a directory prefix is
/// the fallback, and a name the caller supplied is only used when no index can
/// be consulted at all.
#[cfg(test)]
fn resolve_official_subsystems(
    tools: Option<&ToolBox>,
    input_subsystems: &[AttributedSubsystem],
    verified_files: &[String],
) -> Vec<AttributedSubsystem> {
    resolve_official_subsystems_for_project(
        tools,
        input_subsystems,
        verified_files,
        ProjectId::Linux,
    )
}

fn resolve_official_subsystems_for_project(
    tools: Option<&ToolBox>,
    input_subsystems: &[AttributedSubsystem],
    verified_files: &[String],
    project: ProjectId,
) -> Vec<AttributedSubsystem> {
    let from_index = |index: &crate::maintainers::MaintainersIndex| {
        let matched = index.match_files(verified_files);
        (!matched.is_empty()).then(|| {
            matched
                .into_iter()
                .map(AttributedSubsystem::from_maintainers)
                .collect::<Vec<_>>()
        })
    };
    let from_paths = || {
        extract_directory_subsystems_for_project(verified_files, project)
            .into_iter()
            .map(AttributedSubsystem::from_path_prefix)
            .collect::<Vec<_>>()
    };

    if project.uses_maintainers() {
        if let Some(tb) = tools {
            return match crate::maintainers::get_global_maintainers() {
                Some(index) => from_index(&index).unwrap_or_else(from_paths),
                None => {
                    match crate::maintainers::MaintainersIndex::from_repo(tb.get_worktree_path()) {
                        Ok(index) => from_index(&index).unwrap_or_else(from_paths),
                        Err(_) => from_paths(),
                    }
                }
            };
        }

        if !input_subsystems.is_empty() {
            return input_subsystems.to_vec();
        }

        return match crate::maintainers::get_global_maintainers() {
            Some(index) => from_index(&index).unwrap_or_else(from_paths),
            None => from_paths(),
        };
    }

    if !input_subsystems.is_empty() {
        return input_subsystems.to_vec();
    }
    from_paths()
}

// ---------------------------------------------------------------------------
// 3. Deduplication Confirmation Session
// ---------------------------------------------------------------------------

const MAX_DEDUP_DESCRIPTION_BYTES: usize = 800;

fn format_known_bug_locations(bug: &Bug) -> String {
    if let Some(locs) = bug.locations()
        && let Some(arr) = locs.as_array()
    {
        let mut items = Vec::new();
        for obj in arr {
            let file = obj
                .get("file")
                .or_else(|| obj.get("path"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let line = obj
                .get("line")
                .or_else(|| obj.get("start_line"))
                .and_then(|v| v.as_i64());
            let func = obj
                .get("function")
                .or_else(|| obj.get("symbol"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty());

            match (file, line, func) {
                (Some(f), Some(l), Some(fn_name)) => {
                    items.push(format!("{f}:{l} ({fn_name})"));
                }
                (Some(f), Some(l), None) => {
                    items.push(format!("{f}:{l}"));
                }
                (Some(f), None, Some(fn_name)) => {
                    items.push(format!("{f} ({fn_name})"));
                }
                (Some(f), None, None) => {
                    items.push(f.to_string());
                }
                (None, _, Some(fn_name)) => {
                    items.push(fn_name.to_string());
                }
                _ => {}
            }
        }
        if !items.is_empty() {
            return items.join(", ");
        }
    }
    bug.source_files()
        .map(|f| f.join(", "))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none".to_string())
}

fn find_known_bug_description_slice(bug: &Bug) -> Option<&str> {
    let report = bug.enrichments.iter().rev().find_map(|e| {
        if e.kind == "report" {
            e.content
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        } else {
            None
        }
    });
    if report.is_some() {
        return report;
    }

    let sev = bug.enrichments.iter().rev().find_map(|e| {
        if e.kind == "severity_calibration" {
            e.content
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    e.data_json
                        .as_ref()
                        .and_then(|d| d.get("explanation"))
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                })
        } else if e.kind == "verification" {
            e.data_json
                .as_ref()
                .and_then(|d| d.get("refutation_evidence"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        } else {
            None
        }
    });
    if sev.is_some() {
        return sev;
    }

    bug.enrichments.iter().rev().find_map(|e| {
        let s = match e.kind.as_str() {
            "verification" | "candidate" | "raw_candidate" => e.content.as_deref(),
            "normalization_run" => e
                .data_json
                .as_ref()
                .and_then(|d| d.get("canonical_description"))
                .and_then(|v| v.as_str()),
            _ => None,
        };
        s.map(str::trim).filter(|s| !s.is_empty())
    })
}

fn format_known_bug_description(bug: &Bug) -> String {
    let Some(raw) = find_known_bug_description_slice(bug) else {
        return "none".to_string();
    };

    let mut compact = String::with_capacity(MAX_DEDUP_DESCRIPTION_BYTES.min(raw.len()));
    let mut was_truncated = false;
    for word in raw.split_whitespace() {
        let sep_len = usize::from(!compact.is_empty());
        if compact.len() + sep_len + word.len() > MAX_DEDUP_DESCRIPTION_BYTES {
            let remaining = MAX_DEDUP_DESCRIPTION_BYTES.saturating_sub(compact.len() + sep_len);
            let prefix = crate::utils::utf8_prefix(word, remaining);
            if !prefix.is_empty() {
                if sep_len > 0 {
                    compact.push(' ');
                }
                compact.push_str(prefix);
            }
            was_truncated = true;
            break;
        }
        if sep_len > 0 {
            compact.push(' ');
        }
        compact.push_str(word);
    }

    if was_truncated {
        format!("{compact}...[truncated]...")
    } else {
        compact
    }
}

struct DedupSession<'a> {
    project: ProjectId,
    candidate_problem: &'a str,
    candidate_description: &'a str,
    candidate_locations: Option<&'a Value>,
    candidate_subsystems: &'a [String],
    known_candidates: &'a [Bug],
    context_tag: Option<String>,
}

#[async_trait]
impl LlmSession for DedupSession<'_> {
    type Output = DedupJson;

    fn system_prompt(&self) -> String {
        match self.project {
            ProjectId::Linux => "You are an expert Linux kernel maintainer responsible for defect tracking and deduplication.\n\
        You will compare a newly verified Linux kernel bug against a list of known Linux kernel bugs in the codebase.\n\
        Determine if the newly verified bug is an identical duplicate (describing the same root cause in the same code path/function) of one of the candidate bugs.\n\
        IMPORTANT: Bugs that have the same root cause but different consequences (e.g. wrong synchronization leads to a data race which might look like a memory leak or use-after-free crash) should be considered a duplicate and be merged. Rule of thumb: if fixing one issue will resolve the other issue, it's the same bug.\n\
        Output raw JSON only."
                .to_string(),
            ProjectId::Sashiko => "You are an expert Sashiko maintainer responsible for defect tracking and deduplication.\n\
        You will compare a newly verified Sashiko bug against a list of known Sashiko bugs in the codebase.\n\
        Determine if the newly verified bug is an identical duplicate (describing the same root cause in the same code path/function) of one of the candidate bugs.\n\
        IMPORTANT: Bugs that have the same root cause but different consequences should be considered a duplicate and be merged. Rule of thumb: if fixing one issue will resolve the other issue, it's the same bug.\n\
        Output raw JSON only."
                .to_string(),
        }
    }

    fn initial_user_prompt(&self) -> String {
        let loc_str = self
            .candidate_locations
            .map(|v| serde_json::to_string_pretty(v).unwrap_or_else(|_| "[]".to_string()))
            .unwrap_or_else(|| "[]".to_string());

        let mut known_list = String::new();
        for bug in self.known_candidates {
            let files_str = bug
                .source_files()
                .map(|f: Vec<String>| f.join(", "))
                .unwrap_or_default();
            let subs_str = if bug.subsystems.is_empty() {
                "unknown".to_string()
            } else {
                bug.subsystems.join(", ")
            };
            let locs_str = format_known_bug_locations(bug);
            let desc_str = format_known_bug_description(bug);
            known_list.push_str(&format!(
                "- Bug ID {}: [BugID: {}] [Status: {}] [Severity: {}] [Subsystems: {}]\n  Problem: {}\n  Affected Files: {}\n  Locations: {}\n  Description: {}\n\n",
                bug.id,
                bug.bugid,
                bug.lifecycle_status.as_str(),
                bug.severity().as_str(),
                subs_str,
                bug.problem(),
                files_str,
                locs_str,
                desc_str,
            ));
        }

        let cand_subs = if self.candidate_subsystems.is_empty() {
            "unknown".to_string()
        } else {
            self.candidate_subsystems.join(", ")
        };
        let cand_desc = if self.candidate_description.trim().is_empty() {
            "none"
        } else {
            self.candidate_description.trim()
        };

        let bug_header = match self.project {
            ProjectId::Linux => "Newly Verified Linux Kernel Bug:",
            ProjectId::Sashiko => "Newly Verified Sashiko Bug:",
        };

        format!(
            "{stage_heading}\n\n\
            {bug_header}\n\
            Problem: {problem}\n\
            Description:\n{description}\n\
            Subsystems: {subsystems}\n\
            Locations:\n{locations}\n\n\
            Candidate Known Bugs in Database:\n\
            {known_bugs}\n\
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
            stage_heading = BugStage::Deduplication.heading(),
            bug_header = bug_header,
            problem = self.candidate_problem,
            description = cand_desc,
            subsystems = cand_subs,
            locations = loc_str,
            known_bugs = known_list,
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
    project: ProjectId,
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
        match self.project {
            ProjectId::Linux => "You are an expert Linux kernel maintainer. Your task is to determine the exact commit that introduced a verified kernel defect.\n\
        Use available tools (git_blame, git_log, git_diff, git_show, git_read_files) to inspect history backwards and confirm which commit actually introduced the buggy logic rather than just refactoring lines.\n\
        EFFICIENCY LIMIT REQUIREMENT: You have a strict limit on tool calls; be extremely efficient instead of wandering the history.".to_string(),
            ProjectId::Sashiko => "You are an expert Sashiko maintainer. Your task is to determine the exact commit that introduced a verified Sashiko defect.\n\
        Use available tools (git_blame, git_log, git_diff, git_show, git_read_files) to inspect history backwards and confirm which commit actually introduced the buggy logic rather than just refactoring lines.\n\
        EFFICIENCY LIMIT REQUIREMENT: You have a strict limit on tool calls; be extremely efficient instead of wandering the history.".to_string(),
        }
    }

    fn initial_user_prompt(&self) -> String {
        format!(
            "{stage_heading}

Verified Vulnerability:
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
            stage_heading = BugStage::OriginTracing.heading(),
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
                let output = crate::git_cmd::in_dir(tb.get_worktree_path())
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
    project: ProjectId,
    canonical_title: &'a str,
    canonical_description: &'a str,
    locations: &'a str,
    context_tag: Option<String>,
}

#[async_trait]
impl LlmSession for SeveritySession<'_> {
    type Output = SeverityJson;

    fn system_prompt(&self) -> String {
        let target_name = match self.project {
            ProjectId::Linux => "the Linux kernel",
            ProjectId::Sashiko => "Sashiko",
        };
        format!(
            "{}\n\nAssess the severity and impact of a verified defect in {} following the severity definitions and calibration guidance above.\n\
            Output raw JSON only matching the schema.",
            crate::prompt_bundle::severity_guide(self.project),
            target_name
        )
    }

    fn initial_user_prompt(&self) -> String {
        let defect_header = match self.project {
            ProjectId::Linux => "Verified Linux Kernel Defect:",
            ProjectId::Sashiko => "Verified Sashiko Defect:",
        };
        format!(
            "{stage_heading}

{defect_header}
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
            stage_heading = BugStage::SeverityAssessment.heading(),
            defect_header = defect_header,
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
    project: ProjectId,
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
        match self.project {
            ProjectId::Linux => r#"You are an expert Linux kernel maintainer drafting a comprehensive, standalone technical defect description suitable for submission to the Linux Kernel Mailing List (LKML).
Maintainers demand technical rigor, exactness, and zero wasted prose.

# SECTION 1: ROLE & AUDIENCE PRINCIPLES

- Maintainer Voice:
  Assume the reader is an experienced Linux kernel maintainer. Write with technical precision as an engineering peer.
- Anti-Lecture Directive:
  Do NOT explain basic kernel mechanics (how RCU works, what spinlocks do, what workqueues or slab caches are, how refcounts function). Focus strictly on the broken contract or invariant in this code.
- Zero Boilerplate:
  Do NOT include greetings, conversational preamble ("While reviewing...", "I noticed that...", "In the Linux kernel..."), or section headers ("Defect Report:", "Report:", "Issue:", "Description:", or the bug title). Start immediately with the technical description.
- Scope Boundary:
  Describe ONLY the defect, root cause, and technical impact. Do NOT suggest how to resolve the issue, do NOT recommend remediation advice, and do NOT write a patch or diff. Maintainers determine how to resolve issues within their subsystems.

# SECTION 2: REPORT STRUCTURE & NARRATIVE FLOW

- Length Discipline (Shortest Sufficient Report):
  Length must be proportional to the complexity of the defect, not to the amount of context you were given. Write the shortest report that fully proves the defect to a maintainer, then stop. A defect confined to one function and one execution path is normally an opening sentence plus a single body paragraph. Add a second body paragraph only when the causal chain genuinely crosses functions, execution contexts, or CPUs. As a guideline, prose should stay within roughly 15 lines; if a multi-stage race or cross-subsystem interaction genuinely requires more, exceed this rather than omit a step of the proof. Accuracy always outranks brevity.
- No Restatement:
  Each fact appears exactly once. Do not repeat the impact in both the opening sentence and a closing paragraph, and never end with a summary or recap of what you already wrote.
- The reference examples in Section 5 show the target length for a typical defect.

A maintainer defect report follows a structured narrative across 1 to 2 cohesive paragraphs (or 3 for complex multi-stage races):

1. Opening Sentence (Broken Invariant & Trigger Condition):
   - The very first sentence must state what goes wrong, in which function and subsystem/file, and under what condition or execution path.
   - If the problem is reproducible only under special circumstances (e.g. on a 32-bit architecture, specific config options, or specific hardware), highlight it first at the very beginning of the opening sentence (e.g. "On a 32-bit architecture, ...", "During device unbind, ...").

2. Failure Mechanics (Causal Chain):
   - Trace the precise cause-and-effect chain: precondition -> triggering event -> faulty state transition / missing check / interleaving -> failure.
   - Detail the exact root cause and failure mechanism (e.g. memory leak on error path, use-after-free, deadlock, null pointer dereference, race condition, integer overflow).
   - Refer to callers or execution flow inline within prose (e.g. "when called from func_a()"). Do NOT output vertical ASCII call-trees (e.g. func_a() -> func_b() -> func_c()).

3. Concrete Technical Consequence:
   - State the immediate technical consequence directly (e.g. memory leak, panic, use-after-free, deadlock). Avoid generic security hyperbole ("may allow an attacker to exploit the system").
   - Argue the case once with precision; do NOT include defensive rationalizations or concluding summaries.

# SECTION 3: CODE PRESENTATION RULES

Choose the representation that most clearly demonstrates the defect:

- Code Presentation Principle:
  Include code snippets for all localized defects (including flawed logic, error paths, and paired resource handling). Omit snippets only when the bug is not localized within existing code (e.g. a missing architectural hook or high-level design omission where pure prose is clearer).
  Quote only the lines that prove the defect. Elide everything else with the < ... > marker, including unrelated branches, switch arms, error paths, and declarations that play no role in the failure. A snippet that needs more than about 20 lines usually means unrelated code is being quoted.

- Paired Actions Rule (Allocations & Releases, Locks, Refcounts):
  For memory leaks or paired resource lifecycle bugs, any snippet MUST include BOTH the allocation or acquisition site (e.g. kzalloc or mutex_lock) AND the error or exit path where release was missed. Never show only the exit path without showing what was allocated or acquired.

- Cross-Function Boundaries (Caller & Callee):
  If a defect arises from incompatible assumptions between two functions (e.g., a caller passing a NULL argument to a callee that blindly dereferences it), snippets MUST include relevant context from BOTH the caller and the callee. Prove the broken contract by showing both sides of the interface, rather than describing the callee's expectations purely in prose.

- Concurrency, Race Conditions, and Deadlocks (LKML Timeline Style):
  For race conditions, deadlocks, lock order inversions, or multi-CPU concurrency issues—and ONLY when it clearly improves clarity—you may illustrate the temporal sequence of events using a clean multi-column timeline across the involved CPUs (e.g. CPU 0 and CPU 1). Start the leftmost column at column 0. Format columns using whitespace separation and dashed underlines. Do NOT draw an ASCII table with vertical borders ('|'), crosses ('+'), or markdown table grids. Do NOT use multi-column timelines for non-concurrency defects (single-threaded leaks, null pointer dereferences on error paths, missing validation).

- Strict Caret (^^^^^) Highlighting Rules:
  Carets are overused and must be used with extreme discipline. In most reports, NO carets should be used. Carets may ONLY point to an existing defective code token or operator that is visibly present in the code (e.g. an unsigned variable compared with < 0, an inverted relational operator, or an off-by-one boundary). NEVER use carets to point at a missing thing (e.g. NEVER point carets at 'goto out;' or 'return err;' or a blank line to say "missing kfree()"). Absence of a function call cannot be highlighted with carets.

- Snippet Formatting:
  When a code snippet is used, format it as:
// <filepath>:<start_line>-<end_line>
return_type func_name(args)
{
	< ... >
	some_code();
	< ... >
}
  Start every snippet line at column 0. Do NOT wrap the block in any extra leading indentation; the only whitespace at the start of a line is the verbatim indentation (tabs/spaces) copied from the source code. Indent the < ... > (or <...>) omission marker to match surrounding block level.
  Comments on code lines or caret lines must NEVER cause total line width (including indentation) to exceed 75 characters. If an explanation is needed, place it on a separate comment line or explain it in the prose below the snippet.
  Do NOT mention raw line numbers in prose; refer to function names or the snippet header instead.

# SECTION 4: OUTPUT FORMAT SPECIFICATION

- Output must be 100% plain text suitable for email.
- Do NOT use markdown code fences (```) or quote marks ('>').
- Do NOT use backticks (`) to quote any names (variables, functions, symbols, or files). For function names, use func() format.
- Format all text paragraphs and comment lines hard-wrapped at 75 characters per line (LKML standard: 72-75 columns). Do not wrap code lines or multi-column diagrams.

# SECTION 5: REFERENCE EXAMPLES (FROM GIT HISTORY)

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

In ffs_epfile_open(), opening an endpoint races with dynamic endpoint
removal, which can leave file->private_data pointing to a freed endpoint
object:

CPU 0 (removal thread)              CPU 1 (open thread)
----------------------              -------------------
                                    ffs_epfile_open()
                                      ep = ffs->epfiles[i];
ffs_data_closed()
  atomic_dec_and_test(&ffs->opened)
  kfree(ep); // dynamic removal
                                      file->private_data = ep; // UAF

When the total open count reaches zero, removal tears down and frees dynamic
endpoints. If a concurrent opener reads ffs->epfiles[i] before the count is
incremented, removal proceeds concurrently and frees the endpoint before the
file descriptor is populated, resulting in a use-after-free on subsequent
read() or write() syscalls.

Example 3 (Broken invariant described in pure prose without a code snippet):

When creating new files in an overlayfs mount, the security layer expects
the original caller credentials to be passed. However, ovl_create_or_link()
supplies the mounter credentials by referencing current->cred, which has
already been overridden with the overlay creator credentials earlier in the
call path.

Because current->cred no longer reflects the security context of the task
requesting file creation, downstream LSM hooks and inode initialization
evaluate permissions against the super-block mounter rather than the
originating task, leading to incorrect permission checks and audit log
attribution.

Example 4 (Arithmetic / boundary / overflow with targeted carets and short comment):

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
zero. This causes kmalloc() to allocate insufficient memory for subsequent DMA
transfers, leading to kernel heap corruption.

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
"#
            .to_string(),
            ProjectId::Sashiko => r#"You are an expert Sashiko maintainer drafting a concise, standalone technical defect description for a verified bug in the Sashiko Rust codebase.

# PRINCIPLES & STRUCTURE
- Maintainer Voice: Write for an experienced Rust and systems engineer. Do NOT explain generic Rust or Tokio concepts. Focus strictly on the broken contract or invariant in this code.
- Zero Boilerplate: Do NOT include greetings, conversational preamble, or section headers ("Defect Report:", "Description:", or the bug title). Start immediately with the technical description.
- Scope Boundary: Describe ONLY the defect, root cause, and technical impact. Do NOT suggest how to resolve the issue or write a patch.
- Narrative Flow (1 to 2 cohesive paragraphs):
  1. Opening Sentence: State what goes wrong, in which function and module/file, and under what condition.
  2. Failure Mechanics: Trace the precise cause-and-effect chain (precondition -> triggering event -> faulty state transition / missing cleanup / race -> failure).
  3. Concrete Consequence: State the direct impact (e.g. leaked worktree, wedged review queue, SQLITE_BUSY stall, panic, dropped finding).
- Code Presentation:
  Include a concise code snippet (`// <filepath>:<start_line>-<end_line>`) starting at column 0 with `< ... >` omission markers when localized code proves the defect.
- Output Format:
  100% plain text: no markdown code fences (```), no quote marks ('>'), and no backticks (`). Use func() format for function names. Hard-wrap prose lines at 75 characters per line."#
                .to_string(),
        }
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

        let details_header = match self.project {
            ProjectId::Linux => "Linux Kernel Defect Details:",
            ProjectId::Sashiko => "Sashiko Defect Details:",
        };

        format!(
            "{stage_heading}

{details_header}
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
Draft the standalone technical defect description for upstream submission following this structured narrative:

1. Opening Sentence:
   - State what goes wrong, in which function and subsystem, and under what condition.
   - If reproducible only under special circumstances (e.g. on a 32-bit machine or specific configuration), highlight it first upfront in sentence 1 (e.g. 'On a 32-bit architecture...').
   - Do NOT include headers like 'Defect Report:' or 'Description:'. Start directly with the technical description.

2. Technical Analysis (1 to 2 cohesive paragraphs):
   - Explain the precise root cause, execution flow, and failure mechanism using the fewest words that fully prove the defect. One body paragraph is the default; add a second only if the causal chain crosses functions, execution contexts, or CPUs. Do not restate the impact twice and do not end with a recap.
   - Anti-Lecture: Write for expert kernel maintainers. Do NOT explain generic kernel concepts (RCU, spinlocks, workqueues, refcounts). Focus strictly on the broken invariant in this code.
   - Refer to callers inline within prose; avoid vertical call-trees (e.g. func_a() -> func_b() -> func_c()).
   - State concrete technical consequences (e.g. memory leak, panic, use-after-free, deadlock); avoid generic security hyperbole.
   - Do NOT provide fix recommendations, patches, or remediation advice. Describe ONLY the bug itself.

3. Code Presentation:
   - Include code snippets for all localized defects (including flawed logic, error paths, and paired resource handling). Omit snippets only when the bug is not localized within existing code (e.g. a missing architectural hook or high-level design omission where pure prose is clearer).
   - For paired actions (memory allocations, lock acquisitions, refcounts), the snippet MUST include BOTH the allocation/acquisition site AND the error exit where release was missed.
   - For cross-function mismatches (e.g., a caller passing a NULL argument to a callee that expects non-NULL), snippets MUST show the relevant code from BOTH the caller and the callee to prove the disconnect.
   - For race conditions or deadlocks across CPUs, format the temporal sequence using whitespace-separated columns and dashed underlines (LKML style multi-CPU timeline diagram). Do NOT draw tables with vertical borders ('|'), crosses ('+'), or markdown table grids.
   - Strict Carets: Do NOT overuse carets (^^^^^). Carets may ONLY point to an existing defective expression. NEVER use carets to highlight a missing call (e.g. do not point carets at 'goto out;' or 'return err;' to denote missing kfree()).
   - Comments on code lines or caret lines must never exceed 75 characters per line.
   - Format snippets with // <filepath>:<start_line>-<end_line>, verbatim tabs, and < ... > (or <...>) omission markers.
   - Start every snippet and timeline line at column 0. Do NOT wrap the block in any extra leading indentation; the only leading whitespace is the verbatim indentation copied from the source code.

4. Formatting Constraints:
   - Raw plain text only: no markdown fences (```), no quote marks ('>'), no backticks (`).
   - For function names, ALWAYS use func() format.
   - Hard-wrap all prose and comment lines at 75 characters per line (LKML standard: 72-75 columns).",
            stage_heading = BugStage::ReportGeneration.heading(),
            details_header = details_header,
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
    generate_bugid_for_project(ProjectId::Linux)
}

/// Generates a unique bugid for a newly discovered bug in `project` (format: <project>-<uuid>).
pub fn generate_bugid_for_project(project: ProjectId) -> String {
    format!("{}-{}", project.as_str(), uuid::Uuid::new_v4())
}

pub fn infer_project_from_bug_or_tool(
    bugid: Option<&str>,
    tool: Option<&str>,
    default_project: ProjectId,
) -> ProjectId {
    if default_project == ProjectId::Sashiko
        || bugid.is_some_and(|id| id.starts_with("sashiko-"))
        || tool.is_some_and(|t| t.starts_with("sashiko:sashiko"))
    {
        ProjectId::Sashiko
    } else if bugid.is_some_and(|id| id.starts_with("linux-"))
        || tool.is_some_and(|t| t.starts_with("sashiko:linux"))
    {
        ProjectId::Linux
    } else {
        default_project
    }
}

#[deprecated(note = "use generate_bugid instead")]
pub fn generate_slug() -> String {
    generate_bugid()
}

/// Executes the standalone bug pipeline for a single candidate concern.
pub async fn process_issue(
    provider: &dyn AiProvider,
    tools: Option<Arc<ToolBox>>,
    db: &Database,
    input: BugInput,
    context_tag: Option<&str>,
) -> Result<BugOutcome> {
    let project = infer_project_from_bug_or_tool(
        None,
        if db.has_bug_actor() {
            Some(db.bug_tool())
        } else {
            None
        },
        ProjectId::Linux,
    );
    process_issue_for_project(provider, tools, db, input, context_tag, project).await
}

/// Executes the standalone bug pipeline for a single candidate concern in `project`.
pub async fn process_issue_for_project(
    provider: &dyn AiProvider,
    _tools: Option<Arc<ToolBox>>,
    db: &Database,
    input: BugInput,
    _context_tag: Option<&str>,
    project: ProjectId,
) -> Result<BugOutcome> {
    info!(
        "Queueing candidate {} issue: '{}' in subsystems '{:?}'",
        project.display_name(),
        input.problem,
        input.subsystems
    );
    let default_tool = match project {
        ProjectId::Linux => "sashiko:linux_patch_review",
        ProjectId::Sashiko => "sashiko:sashiko_patch_review",
    };
    let reviewer_db;
    let db = if db.has_bug_actor() && db.bug_model().is_some() {
        db
    } else {
        let (actor, tool) = if db.has_bug_actor() {
            (db.bug_actor().to_string(), db.bug_tool().to_string())
        } else {
            ("sashiko".to_string(), default_tool.to_string())
        };
        reviewer_db =
            db.with_bug_actor(&actor, &tool, Some(provider.get_capabilities().model_name));
        &reviewer_db
    };
    let bugid = generate_bugid_for_project(project);
    let now = chrono::Utc::now().timestamp();
    let initial_subsystems = if input.subsystems.is_empty()
        && !input.source_files.is_empty()
        && !project.uses_maintainers()
    {
        extract_directory_subsystems_for_project(&input.source_files, project)
            .into_iter()
            .map(AttributedSubsystem::from_path_prefix)
            .collect()
    } else {
        input.subsystems.clone()
    };
    let new_bug = NewBug {
        bugid: bugid.clone(),
        title: input.problem.clone(),
        lifecycle_status: crate::db::BugLifecycleStatus::New,
        pipeline_state: crate::db::BugPipelineState::Pending,
        assignee: None,
        reporter: db.bug_actor().to_string(),
        reported_at: now,
        discovered_in_patchset_id: input.patchset_id,
        discovered_in_patch_id: input.patch_id,
        discovered_in_commit: input.commit_sha.clone(),
        source_ref: input.commit_sha.clone(),
        vector_json: None,
        duplicate_of_id: None,
        subsystems: initial_subsystems,
    };
    let id = db
        .create_bug_with_enrichment(
            &new_bug,
            Some(&crate::db::NewBugEnrichment {
                kind: "candidate".to_string(),
                tool: db.bug_tool().to_string(),
                model: db.bug_model().map(|s| s.to_string()),
                author: Some(db.bug_actor().to_string()),
                created_at: now,
                content: Some(input.reasoning.clone()),
                data_json: serde_json::to_value(&input).ok(),
                ..Default::default()
            }),
        )
        .await?;
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
        for ref_name in ["origin/master", "origin/main", "master", "main", "HEAD"] {
            if let Ok(output) = crate::git_cmd::in_dir(worktree)
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

        let subject = crate::git_cmd::in_dir(worktree)
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

        let release = crate::git_cmd::in_dir(worktree)
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

        let short_sha: String = sha.chars().take(12).collect();
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
        let blame_output = crate::git_cmd::in_dir_async(worktree)
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
/// 1. At most 3 distinct .c/.h/.rs files.
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

    // Step 1: Collect and sanitize candidate file paths (max 3 distinct valid C/Rust files)
    let mut files: Vec<String> = Vec::new();
    for loc in loc_arr {
        let Some(file) = loc.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        let file = file.trim();
        if file.contains("..")
            || file.starts_with('/')
            || file.starts_with('\\')
            || (!file.ends_with(".c") && !file.ends_with(".h") && !file.ends_with(".rs"))
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

            let git_output = crate::git_cmd::in_dir(&worktree)
                .args(["show", &format!("{}:{}", master_sha_owned, file)])
                .output();

            let (out, actual_file) = match git_output {
                Ok(o) if o.status.success() => (o, file.clone()),
                _ => {
                    // Try tracing rename forwards first
                    let last_commit = crate::git_cmd::in_dir(&worktree)
                        .args(["log", "-n", "1", "--format=%H", "--", &file])
                        .output();
                    let mut resolved = None;
                    if let Ok(lc) = last_commit {
                        let sha = String::from_utf8_lossy(&lc.stdout).trim().to_string();
                        if !sha.is_empty() {
                            let diff_out = crate::git_cmd::in_dir(&worktree)
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
                                        let try_out = crate::git_cmd::in_dir(&worktree)
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
                        let rename_cmd = crate::git_cmd::in_dir(&worktree)
                            .args(["log", "--follow", "--name-only", "--format=format:", &file])
                            .output();
                        if let Some(ro) = rename_cmd.ok().filter(|r| r.status.success()) {
                            let stdout = String::from_utf8_lossy(&ro.stdout);
                            for line in stdout.lines() {
                                let trimmed = line.trim();
                                if trimmed.is_empty() || trimmed == file {
                                    continue;
                                }
                                let try_out = crate::git_cmd::in_dir(&worktree)
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
            let is_c_file = actual_file.ends_with(".c") || actual_file.ends_with(".h");

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
                                        || trimmed.starts_with("void")
                                        || trimmed.starts_with("fn ")
                                        || trimmed.starts_with("pub fn ")
                                        || trimmed.starts_with("pub(crate) fn ")
                                        || trimmed.starts_with("async fn ")
                                        || trimmed.starts_with("pub async fn "))
                            })
                            .or_else(|| lines.iter().position(|l| l.contains(sym)));
                        if let Some(idx) = found_idx {
                            let line = idx + 1;
                            if is_c_file
                                && let Some((block_text, name)) =
                                    crate::worker::prefetch::extract_enclosing_block(
                                        &content, idx, idx,
                                    )
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
                        if is_c_file
                            && let Some((block_text, name)) =
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

// Persist each completed stage immediately. If a later stage fails, its
// predecessors' original interactions and usage remain available for inspection.
async fn record_bug_stage<T>(
    db: &Database,
    bug_id: i64,
    stage: BugStage,
    result: &crate::ai::session::SessionResult<T>,
) -> Result<()> {
    db.add_bug_enrichment(
        bug_id,
        &crate::db::NewBugEnrichment {
            kind: stage.enrichment_kind(),
            content: Some(format!("{} completed", stage.title())),
            data_json: Some(serde_json::json!({
                "stage_id": stage.id(),
                "stage_title": stage.title(),
            })),
            logs: Some(serde_json::to_string(&result.history)?),
            tokens_in: Some(result.usage.prompt_tokens),
            tokens_out: Some(result.usage.completion_tokens),
            tokens_cached: result.usage.cached_tokens,
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

pub async fn process_issue_worker(
    provider: &dyn AiProvider,
    tools: Option<Arc<ToolBox>>,
    db: &Database,
    bug_row: &crate::db::Bug,
    input: BugInput,
    context_tag: Option<&str>,
) -> Result<BugOutcome> {
    let default_project = infer_project_from_bug_or_tool(
        Some(&bug_row.bugid),
        if db.has_bug_actor() {
            Some(db.bug_tool())
        } else {
            None
        },
        ProjectId::Linux,
    );
    process_issue_worker_for_project(
        provider,
        tools,
        db,
        bug_row,
        input,
        context_tag,
        default_project,
    )
    .await
}

pub async fn process_issue_worker_for_project(
    provider: &dyn AiProvider,
    tools: Option<Arc<ToolBox>>,
    db: &Database,
    bug_row: &crate::db::Bug,
    input: BugInput,
    context_tag: Option<&str>,
    project: ProjectId,
) -> Result<BugOutcome> {
    let project_label = match project {
        ProjectId::Linux => "Linux kernel",
        ProjectId::Sashiko => "Sashiko",
    };
    info!(
        "Processing candidate {} issue: '{}' in subsystems '{:?}'",
        project_label, input.problem, input.subsystems
    );

    let actor = if db.has_bug_actor() {
        db.bug_actor().to_string()
    } else if !bug_row.reporter.is_empty() {
        bug_row.reporter.clone()
    } else {
        "sashiko".to_string()
    };
    let tool = if db.has_bug_actor() {
        db.bug_tool().to_string()
    } else {
        match project {
            ProjectId::Linux => "sashiko:linux_bug".to_string(),
            ProjectId::Sashiko => "sashiko:sashiko_bug".to_string(),
        }
    };
    let attributed_db =
        db.with_bug_actor(&actor, &tool, Some(provider.get_capabilities().model_name));
    let db = &attributed_db;
    let mut full_history = Vec::new();
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

    // Normalization and canonical naming.
    info!("--- {} ---", BugStage::Normalization.title());
    let maintainers_hint = if project.uses_maintainers() {
        crate::maintainers::get_global_maintainers()
            .or_else(|| {
                tools.as_ref().and_then(|tb| {
                    crate::maintainers::MaintainersIndex::from_repo(tb.get_worktree_path())
                        .ok()
                        .map(std::sync::Arc::new)
                })
            })
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
            })
    } else {
        None
    };

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
        project,
    };

    let norm_result = runner.run(&mut norm_session).await?;
    record_bug_stage(db, bug_row.id, BugStage::Normalization, &norm_result).await?;
    full_history.extend(norm_result.history);

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

    let official_subsystems = resolve_official_subsystems_for_project(
        tools.as_deref(),
        &input.subsystems,
        &verified_files,
        project,
    );
    let official_subsystem_names: Vec<String> =
        official_subsystems.iter().map(|s| s.name.clone()).collect();

    let title_prefix = extract_title_prefix(&norm.canonical_title);

    info!(
        "Normalization complete: title '{}' (prefix '{}') with official subsystems '{:?}'",
        norm.canonical_title, title_prefix, official_subsystems
    );

    // Verification and ground-truth confirmation.
    info!("--- {} ---", BugStage::Verification.title());
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
        project,
    };

    let verify_result = runner.run(&mut verify_session).await?;
    record_bug_stage(db, bug_row.id, BugStage::Verification, &verify_result).await?;
    full_history.extend(verify_result.history);

    let verification = verify_result.output;
    info!(
        "Verification complete: is_false_positive={}",
        verification.is_false_positive
    );

    if verification.is_false_positive {
        let reason = verification.refutation_evidence.unwrap_or_else(|| {
            "Discarded as a hallucinated or disproved false positive".to_string()
        });
        info!("{} candidate discarded: {}", project_label, reason);
        let logs = serde_json::to_string(&full_history).unwrap_or_default();
        db.update_bug_outcome(
            bug_row.id,
            crate::db::UpdateBugOutcomeParams {
                lifecycle_status: crate::db::BugLifecycleStatus::Dismissed,
                problem: Some(&norm.canonical_title),
                subsystems: Some(&official_subsystems),
                source_files: Some(&verified_files),
                severity_explanation: Some(&reason),
                logs: None,
                verified_on_sha: Some(&master_sha),
                tokens_in: None,
                tokens_out: None,
                tokens_cached: None,
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

    // Deduplication confirmation, under BUG_DEDUP_LOCK.
    info!("--- {} ---", BugStage::Deduplication.title());
    let query_vector = extract_bug_vector(
        &norm.canonical_title,
        &official_subsystem_names,
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
            "Deduplication: found {} potential candidates.",
            candidate_matches.len()
        );

        if !candidate_matches.is_empty() {
            let candidate_bugs: Vec<Bug> =
                candidate_matches.iter().map(|m| m.bug.clone()).collect();
            let mut dedup_session = DedupSession {
                candidate_problem: &norm.canonical_title,
                candidate_description: &norm.canonical_description,
                candidate_locations: verified_locations.as_ref(),
                candidate_subsystems: &official_subsystem_names,
                known_candidates: &candidate_bugs,
                context_tag: context_tag.map(|s| s.to_string()),
                project,
            };

            let dedup_result = runner.run(&mut dedup_session).await?;
            record_bug_stage(db, bug_row.id, BugStage::Deduplication, &dedup_result).await?;
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
                    "Matched duplicate {} bug #{} ({})",
                    project_label, existing.id, existing.bugid
                );
                let logs = serde_json::to_string(&full_history).unwrap_or_default();
                let folded = db
                    .mark_bug_as_duplicate(crate::db::MarkDuplicateBugParams {
                        preserve_triage: true,
                        ephemeral_id: bug_row.id,
                        canonical_id: existing.id,
                        reasoning: &dedup.reasoning,
                        logs: None,
                        tokens_in: None,
                        tokens_out: None,
                        tokens_cached: None,
                    })
                    .await?;
                if folded {
                    if let Some(r_id) = input.review_id {
                        db.link_review_to_bug(r_id, existing.id, false).await?;
                    }
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
        } else {
            (false, None)
        }
    };

    if is_dup {
        return Ok(dup_outcome.unwrap());
    }
    info!("Deduplication complete: novel bug confirmed.");

    // Origin tracing, recorded as an enrichment.
    info!("--- {} ---", BugStage::OriginTracing.title());
    let tracing_runner = SessionRunner::new(provider).with_max_turns(30);
    let mut tracing_session = TracingSession {
        input: &input,
        master_sha: master_sha.clone(),
        verification_reasoning: verification.verification_reasoning.clone(),
        relevant_locations: verified_locations_str.clone(),
        tools: tools.clone(),
        context_tag: context_tag.map(|s| s.to_string()),
        project,
    };
    let tracing_result = tracing_runner.run(&mut tracing_session).await?;
    record_bug_stage(db, bug_row.id, BugStage::OriginTracing, &tracing_result).await?;
    full_history.extend(tracing_result.history);

    let introducing_commit_sha = match tracing_result.output.introducing_commit_sha {
        Some(sha) => Some(sha),
        None => {
            deterministic_blame_fallback(tools.as_ref(), &verified_locations, &master_sha).await
        }
    };
    let introduced_in_commit = format_commit(tools.as_ref(), introducing_commit_sha).await;

    // Severity and impact estimation, recorded as an enrichment.
    info!("--- {} ---", BugStage::SeverityAssessment.title());
    let mut severity_session = SeveritySession {
        canonical_title: &norm.canonical_title,
        canonical_description: &norm.canonical_description,
        locations: &verified_locations_str,
        context_tag: context_tag.map(|s| s.to_string()),
        project,
    };
    let severity_result = runner.run(&mut severity_session).await?;
    record_bug_stage(
        db,
        bug_row.id,
        BugStage::SeverityAssessment,
        &severity_result,
    )
    .await?;
    full_history.extend(severity_result.history);

    let severity_output = severity_result.output;
    let severity = Severity::from_str(&severity_output.severity);

    // Standalone plaintext review generation, recorded as an enrichment.
    info!("--- {} ---", BugStage::ReportGeneration.title());
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
        project,
    };
    let report_result = runner.run(&mut report_session).await?;
    record_bug_stage(db, bug_row.id, BugStage::ReportGeneration, &report_result).await?;
    full_history.extend(report_result.history);

    let inline_review = report_result.output;

    // Final database write.
    info!("--- Final database write ---");

    db.update_bug_outcome(
        bug_row.id,
        crate::db::UpdateBugOutcomeParams {
            lifecycle_status: crate::db::BugLifecycleStatus::Open,
            problem: Some(&norm.canonical_title),
            subsystems: Some(&official_subsystems),
            source_files: Some(&verified_files),
            locations: verified_locations.as_ref(),
            severity,
            severity_explanation: Some(&severity_output.severity_explanation),
            inline_review: &inline_review,
            logs: None,
            vector_json: Some(&query_vector.to_json()),
            introduced_in_commit: introduced_in_commit.as_deref(),
            verified_on_sha: Some(&master_sha),
            is_fixed: false,
            fixed_in_commit: None,
            tokens_in: None,
            tokens_out: None,
            tokens_cached: None,
        },
    )
    .await?;

    let saved_bug = db.get_bug(bug_row.id).await?.expect("Saved bug must exist");
    if let Some(r_id) = input.review_id {
        db.link_review_to_bug(r_id, saved_bug.id, true).await?;
    }
    info!(
        "Successfully registered newly verified {} bug #{} ({})",
        project_label, bug_row.id, bug_row.bugid
    );
    Ok(BugOutcome::NewlyDiscovered { bug: saved_bug })
}

// ---------------------------------------------------------------------------
// 7. Periodic Upstream Fix Check (Mainline Tree)
// ---------------------------------------------------------------------------

const UPSTREAM_GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
pub const MAX_FIX_CANDIDATE_COMMITS: usize = 10;
pub const MAX_FIX_CHECK_PREFETCH_BYTES: usize = 24_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpstreamFixVerdict {
    /// One of `"fixed"`, `"still_present"`, or `"uncertain"`.
    pub status: String,
    #[serde(default)]
    pub fixing_commit_sha: Option<String>,
    pub explanation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamFixCheckOutcome {
    SkippedAlreadyAtSha,
    SkippedNotAncestor,
    AdvancedWithoutLlm {
        verified_on_sha: String,
    },
    StillPresentAfterLlm {
        verified_on_sha: String,
        explanation: String,
    },
    FixedUpstream {
        fixing_commit_sha: String,
        verified_on_sha: String,
        explanation: String,
    },
    Uncertain {
        explanation: String,
    },
}

async fn run_git_query(
    repo_path: &std::path::Path,
    args: &[&str],
) -> Result<crate::toolbox::command::CappedOutput> {
    run_git_query_capped(
        repo_path,
        args,
        crate::toolbox::command::MAX_CAPTURED_STDOUT,
    )
    .await
}

async fn run_git_query_capped(
    repo_path: &std::path::Path,
    args: &[&str],
    max_stdout: usize,
) -> Result<crate::toolbox::command::CappedOutput> {
    let mut cmd = crate::git_cmd::in_dir_async(repo_path);
    cmd.env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["-c", "safe.bareRepository=all"])
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    tokio::time::timeout(
        UPSTREAM_GIT_TIMEOUT,
        crate::toolbox::command::capped_output_with_limit(&mut cmd, max_stdout),
    )
    .await
    .with_context(|| {
        format!(
            "git command {:?} timed out after {:?} in {}",
            args,
            UPSTREAM_GIT_TIMEOUT,
            repo_path.display()
        )
    })?
    .with_context(|| {
        format!(
            "failed to execute git command {:?} in {}",
            args,
            repo_path.display()
        )
    })
}

/// Resolves the current HEAD commit SHA of the upstream mainline tree in `repo_path`.
///
/// Prefers a remote whose URL matches `torvalds/linux` (`{name}/master`) or
/// `sashiko-dev/sashiko` (`{name}/main`), falling back to `origin/master`,
/// `origin/main`, `master`, `main`, and `HEAD`.
pub async fn resolve_linus_sha(repo_path: &std::path::Path) -> Option<String> {
    let mut candidate_refs = Vec::new();
    match run_git_query(repo_path, &["remote", "-v"]).await {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                let mut parts = line.split_whitespace();
                if let (Some(name), Some(url)) = (parts.next(), parts.next()) {
                    if url.contains("torvalds/linux") {
                        let r = format!("{}/master", name);
                        if !candidate_refs.contains(&r) {
                            candidate_refs.push(r);
                        }
                    } else if url.contains("sashiko-dev/sashiko") {
                        let r = format!("{}/main", name);
                        if !candidate_refs.contains(&r) {
                            candidate_refs.push(r);
                        }
                    }
                }
            }
        }
        Ok(_) => {}
        Err(e) => {
            warn!("Failed to query git remotes for mainline tree: {}", e);
        }
    }
    for fallback in ["origin/master", "origin/main", "master", "main", "HEAD"] {
        let s = fallback.to_string();
        if !candidate_refs.contains(&s) {
            candidate_refs.push(s);
        }
    }

    for ref_name in candidate_refs {
        let rev = format!("{}^{{commit}}", ref_name);
        match run_git_query(
            repo_path,
            &["rev-parse", "--verify", "--quiet", "--end-of-options", &rev],
        )
        .await
        {
            Ok(out) if out.status.success() => {
                let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(sha);
                }
            }
            Ok(_) => {}
            Err(e) => {
                warn!("Failed to resolve ref '{}' in git repo: {}", ref_name, e);
            }
        }
    }
    None
}

fn sanitize_bug_file_path(raw: &str) -> Option<String> {
    let path = raw.trim();
    if path.is_empty()
        || path.starts_with('-')
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains("..")
    {
        return None;
    }
    Some(path.to_string())
}

/// Extracts deduplicated, sanitized file paths associated with `bug` from both
/// `source_files` and `locations`.
pub fn extract_bug_files(bug: &Bug) -> Vec<String> {
    let mut files = Vec::new();
    if let Some(src_files) = bug.source_files() {
        for f in src_files {
            if let Some(clean) = sanitize_bug_file_path(&f)
                && !files.contains(&clean)
            {
                files.push(clean);
            }
        }
    }
    if let Some(locs) = bug.locations()
        && let Some(arr) = locs.as_array()
    {
        for loc in arr {
            if let Some(f) = loc.get("file").and_then(|v| v.as_str())
                && let Some(clean) = sanitize_bug_file_path(f)
                && !files.contains(&clean)
            {
                files.push(clean);
            }
        }
    }
    files
}

async fn resolve_verified_commit_sha(
    repo_path: &std::path::Path,
    raw_sha: &str,
) -> Result<Option<String>> {
    let trimmed = raw_sha.trim();
    if trimmed.len() < 7 || trimmed.len() > 40 || !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let rev = format!("{}^{{commit}}", trimmed);
    let out = run_git_query(
        repo_path,
        &["rev-parse", "--verify", "--quiet", "--end-of-options", &rev],
    )
    .await?;
    if !out.status.success() {
        return Ok(None);
    }
    let full_sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if full_sha.len() < 7 || !full_sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(None);
    }
    Ok(Some(full_sha))
}

/// Verifies that `commit_sha` resolves to a valid commit in `repo_path` and is
/// an ancestor of `target_sha`, returning the full 40-character SHA when valid.
pub async fn verify_commit_is_ancestor(
    repo_path: &std::path::Path,
    commit_sha: &str,
    target_sha: &str,
) -> Option<String> {
    let full_sha = match resolve_verified_commit_sha(repo_path, commit_sha).await {
        Ok(Some(sha)) => sha,
        Ok(None) => return None,
        Err(e) => {
            warn!(
                "Git error resolving commit_sha '{}' during ancestry check: {}",
                commit_sha, e
            );
            return None;
        }
    };
    let full_target_sha = match resolve_verified_commit_sha(repo_path, target_sha).await {
        Ok(Some(sha)) => sha,
        Ok(None) => return None,
        Err(e) => {
            warn!(
                "Git error resolving target_sha '{}' during ancestry check: {}",
                target_sha, e
            );
            return None;
        }
    };
    match run_git_query(
        repo_path,
        &["merge-base", "--is-ancestor", &full_sha, &full_target_sha],
    )
    .await
    {
        Ok(anc) if anc.status.success() => Some(full_sha),
        Ok(_) => None,
        Err(e) => {
            warn!(
                "Git error checking merge-base --is-ancestor {} {}: {}",
                full_sha, full_target_sha, e
            );
            None
        }
    }
}

async fn commit_subject(repo_path: &std::path::Path, sha: &str) -> Option<String> {
    let out = run_git_query(repo_path, &["log", "-1", "--format=%s", sha])
        .await
        .ok()
        .filter(|o| o.status.success())?;
    let subject = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!subject.is_empty()).then_some(subject)
}

/// Maximum number of non-merge commits in `verified_on_sha..linus_sha` to index
/// in memory with a single `git log` invocation. Ranges larger than this fall
/// back to per-bug `find_candidate_fix_commits`.
pub const MAX_INDEXED_RANGE_COMMITS: usize = 2_000;

/// Maximum byte size of a full candidate commit diff (`git show --stat --patch`)
/// before falling back to filtering the patch hunks by the bug's tracked files.
pub const MAX_FULL_COMMIT_PREFETCH_BYTES: usize = 12_000;

/// In-memory index of modified file paths and commit messages across a commit
/// range `from_sha..to_sha`, enabling $\mathcal{O}(1)$ per-bug candidate lookup
/// across $\mathcal{O}(10\text{k})$ open bugs without spawning per-bug `git`
/// subprocesses.
#[derive(Debug, Clone, Default)]
pub struct CommitRangeIndex {
    files_to_commits: std::collections::HashMap<String, Vec<(usize, String)>>,
    commit_messages: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub enum CommitRangeLookup {
    /// `from_sha` or `to_sha` could not be resolved or `from_sha` is not an
    /// ancestor of `to_sha`.
    NotAncestor,
    /// `from_sha` is an ancestor of `to_sha`, but the range exceeds
    /// `MAX_INDEXED_RANGE_COMMITS` so callers should fall back to per-bug
    /// `find_candidate_fix_commits`.
    TooLarge,
    /// Complete in-memory index for `from_sha..to_sha`.
    Indexed(CommitRangeIndex),
}

impl CommitRangeIndex {
    /// Matches candidate fix commits for a bug touching `files` and optionally
    /// introduced by `introducing_sha`, using the exact same semantics as
    /// `find_candidate_fix_commits` with zero `git` subprocess calls.
    pub fn find_candidates(
        &self,
        files: &[String],
        introducing_sha: Option<&str>,
    ) -> Option<Vec<String>> {
        if files.is_empty() {
            return None;
        }
        let mut matched_by_file: Vec<(usize, &str)> = Vec::new();
        for f in files {
            if let Some(entries) = self.files_to_commits.get(f.as_str()) {
                for (idx, sha) in entries {
                    matched_by_file.push((*idx, sha.as_str()));
                }
            }
        }
        matched_by_file.sort_unstable_by_key(|(idx, _)| *idx);
        matched_by_file.dedup_by_key(|(idx, _)| *idx);

        let mut candidates: Vec<String> = matched_by_file
            .into_iter()
            .take(100)
            .map(|(_, sha)| sha.to_string())
            .collect();

        if let Some(intro) = introducing_sha {
            let short_intro: String = intro
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_hexdigit())
                .take(12)
                .collect();
            if short_intro.len() >= 7 {
                let mut grep_matches = 0usize;
                for (sha, body) in &self.commit_messages {
                    if body.contains(&short_intro) {
                        grep_matches += 1;
                        if !candidates.iter().any(|c| c == sha) {
                            candidates.push(sha.clone());
                        }
                        if grep_matches >= 100 {
                            break;
                        }
                    }
                }
            }
        }

        Some(candidates)
    }
}

fn parse_commit_range_index_output(text: &str) -> CommitRangeLookup {
    let mut index = CommitRangeIndex::default();
    for (order_idx, record) in text
        .split('\x1e')
        .filter(|s| !s.trim().is_empty())
        .enumerate()
    {
        if order_idx >= MAX_INDEXED_RANGE_COMMITS {
            return CommitRangeLookup::TooLarge;
        }
        let Some((sha_raw, rest)) = record.split_once('\x1f') else {
            return CommitRangeLookup::TooLarge;
        };
        let Some((body, files_raw)) = rest.split_once('\x1d') else {
            return CommitRangeLookup::TooLarge;
        };
        let sha = sha_raw.trim();
        if sha.len() < 7 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return CommitRangeLookup::TooLarge;
        }
        let sha_owned = sha.to_string();
        for f in files_raw.split(['\0', '\n']) {
            let clean = f.trim();
            if clean.is_empty() {
                continue;
            }
            let list = index.files_to_commits.entry(clean.to_string()).or_default();
            if list.last().map(|(idx, _)| *idx) != Some(order_idx) {
                list.push((order_idx, sha_owned.clone()));
            }
        }
        index.commit_messages.push((sha_owned, body.to_string()));
    }
    CommitRangeLookup::Indexed(index)
}

/// Builds a single-pass `CommitRangeIndex` for `from_sha..to_sha` when the
/// range contains at most `MAX_INDEXED_RANGE_COMMITS` non-merge commits.
pub async fn build_commit_range_index(
    repo_path: &std::path::Path,
    from_sha: &str,
    to_sha: &str,
) -> Result<CommitRangeLookup> {
    let Some(full_from_sha) = resolve_verified_commit_sha(repo_path, from_sha).await? else {
        return Ok(CommitRangeLookup::NotAncestor);
    };
    let Some(full_to_sha) = resolve_verified_commit_sha(repo_path, to_sha).await? else {
        return Ok(CommitRangeLookup::NotAncestor);
    };
    if full_from_sha == full_to_sha {
        return Ok(CommitRangeLookup::Indexed(CommitRangeIndex::default()));
    }
    let anc = run_git_query(
        repo_path,
        &["merge-base", "--is-ancestor", &full_from_sha, &full_to_sha],
    )
    .await?;
    if !anc.status.success() {
        return Ok(CommitRangeLookup::NotAncestor);
    }

    let limit_str = (MAX_INDEXED_RANGE_COMMITS + 1).to_string();
    let range = format!("{}..{}", full_from_sha, full_to_sha);
    let out = run_git_query(
        repo_path,
        &[
            "log",
            "--no-merges",
            "-n",
            &limit_str,
            "-z",
            "--name-only",
            "--format=%x1e%H%x1f%B%x1d",
            &range,
        ],
    )
    .await?;
    if !out.status.success() || out.stdout_capped {
        return Ok(CommitRangeLookup::TooLarge);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(parse_commit_range_index_output(&stdout))
}

/// Deterministic Tier-2 git pre-filter for upstream fix checking.
///
/// Returns:
/// - `Ok(None)` if `from_sha` is not an ancestor of `to_sha` or `files` is empty.
/// - `Ok(Some(vec![]))` if `from_sha` is an ancestor of `to_sha` and zero commits
///   in `from_sha..to_sha` modified `files` or referenced `introducing_sha`.
/// - `Ok(Some(shas))` with deduplicated candidate commit SHAs when commits
///   touched `files` or mentioned `introducing_sha`.
pub async fn find_candidate_fix_commits(
    repo_path: &std::path::Path,
    from_sha: &str,
    to_sha: &str,
    files: &[String],
    introducing_sha: Option<&str>,
) -> Result<Option<Vec<String>>> {
    if files.is_empty() {
        return Ok(None);
    }
    let Some(full_from_sha) = resolve_verified_commit_sha(repo_path, from_sha).await? else {
        return Ok(None);
    };
    let Some(full_to_sha) = resolve_verified_commit_sha(repo_path, to_sha).await? else {
        return Ok(None);
    };
    let anc = run_git_query(
        repo_path,
        &["merge-base", "--is-ancestor", &full_from_sha, &full_to_sha],
    )
    .await?;
    if !anc.status.success() {
        return Ok(None);
    }

    let range = format!("{}..{}", full_from_sha, full_to_sha);
    let mut log_args: Vec<&str> = vec![
        "log",
        "--no-merges",
        "-n",
        "100",
        "--format=%H",
        &range,
        "--",
    ];
    for f in files {
        log_args.push(f.as_str());
    }
    let out = run_git_query(repo_path, &log_args).await?;
    if !out.status.success() {
        return Ok(None);
    }

    let mut candidates = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let sha = line.trim();
        if sha.len() >= 7
            && sha.chars().all(|c| c.is_ascii_hexdigit())
            && !candidates.iter().any(|c| c == sha)
        {
            candidates.push(sha.to_string());
        }
    }

    if let Some(intro) = introducing_sha {
        let short_intro: String = intro
            .trim()
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .take(12)
            .collect();
        if short_intro.len() >= 7 {
            let grep_arg = format!("--grep={}", short_intro);
            let grep_out = run_git_query(
                repo_path,
                &[
                    "log",
                    "--no-merges",
                    "-n",
                    "100",
                    "--fixed-strings",
                    &grep_arg,
                    "--format=%H",
                    &range,
                ],
            )
            .await?;
            if !grep_out.status.success() {
                return Ok(None);
            }
            for line in String::from_utf8_lossy(&grep_out.stdout).lines() {
                let sha = line.trim();
                if sha.len() >= 7
                    && sha.chars().all(|c| c.is_ascii_hexdigit())
                    && !candidates.iter().any(|c| c == sha)
                {
                    candidates.push(sha.to_string());
                }
            }
        }
    }

    Ok(Some(candidates))
}

async fn prefetch_candidate_fix_commits(
    repo_path: &std::path::Path,
    candidate_shas: &[String],
    files: &[String],
) -> String {
    let mut output = String::new();
    for sha in candidate_shas.iter().take(MAX_FIX_CANDIDATE_COMMITS) {
        let (mut commit_text, mut was_capped) = run_git_query_capped(
            repo_path,
            &["show", "--stat", "--patch", "--unified=5", sha.as_str()],
            MAX_FULL_COMMIT_PREFETCH_BYTES,
        )
        .await
        .ok()
        .filter(|o| o.is_usable())
        .map(|o| {
            (
                String::from_utf8_lossy(&o.stdout).into_owned(),
                o.stdout_capped,
            )
        })
        .unwrap_or_default();

        if was_capped && !files.is_empty() {
            let mut args: Vec<&str> = vec![
                "show",
                "--stat",
                "--patch",
                "--unified=5",
                sha.as_str(),
                "--",
            ];
            for f in files {
                args.push(f.as_str());
            }
            if let Some((filtered, filtered_capped)) =
                run_git_query_capped(repo_path, &args, MAX_FIX_CHECK_PREFETCH_BYTES)
                    .await
                    .ok()
                    .filter(|o| o.is_usable())
                    .map(|o| {
                        (
                            String::from_utf8_lossy(&o.stdout).into_owned(),
                            o.stdout_capped,
                        )
                    })
                    .filter(|(s, _)| !s.trim().is_empty())
            {
                commit_text = filtered;
                was_capped = filtered_capped;
            }
        }
        if commit_text.trim().is_empty() {
            continue;
        }
        if was_capped {
            commit_text.push_str("\n... (Candidate commit diff truncated)\n");
        }

        let entry = format!("=== Candidate Commit {} ===\n{}\n", sha, commit_text);
        let current_bytes = output.len();
        let entry_bytes = entry.len();
        if current_bytes + entry_bytes > MAX_FIX_CHECK_PREFETCH_BYTES {
            let remaining = MAX_FIX_CHECK_PREFETCH_BYTES.saturating_sub(current_bytes);
            if remaining > 120 {
                let mut end = remaining.min(entry_bytes);
                while end > 0 && !entry.is_char_boundary(end) {
                    end -= 1;
                }
                output.push_str(&entry[..end]);
            }
            output.push_str("\n... (Candidate commit prefetch limit reached)\n");
            break;
        }
        output.push_str(&entry);
    }
    output
}

struct VerifyUpstreamFixSession<'a> {
    bug: &'a Bug,
    affected_files: &'a [String],
    previous_sha: &'a str,
    linus_sha: &'a str,
    candidate_shas: &'a [String],
    prefetched_commits: String,
    prefetched_code: String,
    tools: Option<Arc<ToolBox>>,
    context_tag: Option<String>,
    last_turn_tool_calls: Vec<(String, Value)>,
    project: ProjectId,
}

#[async_trait]
impl LlmSession for VerifyUpstreamFixSession<'_> {
    type Output = UpstreamFixVerdict;

    fn system_prompt(&self) -> String {
        match self.project {
            ProjectId::Linux => {
                "You are an expert Linux kernel maintainer auditing whether a previously verified kernel bug has been fixed in Linus's upstream mainline tree.\n\
                Do NOT give commits the benefit of the doubt: only mark a bug as \"fixed\" if you can point to a specific upstream commit that genuinely resolves the root cause of the defect or removes the vulnerable code path.\n\
                If commits in the range only refactor, move lines, rename symbols, or modify unrelated functions while the defect remains triggerable, you MUST report \"still_present\".\n\
                Output raw JSON only."
                    .to_string()
            }
            ProjectId::Sashiko => {
                "You are an expert Rust and distributed systems maintainer auditing whether a previously verified Sashiko bug has been fixed on the upstream main branch.\n\
                Do NOT give commits the benefit of the doubt: only mark a bug as \"fixed\" if you can point to a specific upstream commit that genuinely resolves the root cause of the defect or removes the vulnerable code path.\n\
                If commits in the range only refactor, move lines, rename symbols, or modify unrelated functions while the defect remains triggerable, you MUST report \"still_present\".\n\
                Output raw JSON only."
                    .to_string()
            }
        }
    }

    fn initial_user_prompt(&self) -> String {
        let locations_str = self
            .bug
            .locations()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or_else(|| "[]".to_string());
        let files_str = self.affected_files.join(", ");
        let intro_str = self
            .bug
            .introduced_in_commit()
            .unwrap_or_else(|| "unknown".to_string());
        let displayed_candidates = self
            .candidate_shas
            .iter()
            .take(MAX_FIX_CANDIDATE_COMMITS)
            .map(|s| format!("- {}", s))
            .collect::<Vec<_>>()
            .join("\n");
        let candidate_list = if self.candidate_shas.len() > MAX_FIX_CANDIDATE_COMMITS {
            format!(
                "{}\n- ... ({} additional candidate commits omitted; use git_log to inspect)",
                displayed_candidates,
                self.candidate_shas.len() - MAX_FIX_CANDIDATE_COMMITS
            )
        } else {
            displayed_candidates
        };
        let report_body = {
            let rev = self.bug.inline_review();
            if !rev.trim().is_empty() {
                rev
            } else {
                self.bug
                    .severity_explanation()
                    .unwrap_or_else(|| "No additional description recorded.".to_string())
            }
        };

        let (sha_label, bug_heading, tree_label) = match self.project {
            ProjectId::Linux => (
                "Current Linus Mainline SHA",
                "Open Linux Kernel Bug",
                "Linus's tree",
            ),
            ProjectId::Sashiko => (
                "Current Upstream Main SHA",
                "Open Sashiko Bug",
                "the upstream main branch",
            ),
        };

        format!(
            "# Upstream Mainline Fix Verification\n\n\
            Previously Verified On SHA: {prev_sha}\n\
            {sha_label}: {linus_sha}\n\n\
            ## {bug_heading} (#{bug_id} / {bugid})\n\
            Title: {title}\n\
            Introduced In Commit: {intro}\n\
            Affected Files: {files}\n\
            Locations:\n{locations}\n\n\
            Bug Report / Mechanism:\n{report}\n\n\
            ## Candidate Commits in {prev_sha}..{linus_sha} ({total_candidates} total)\n\
            {candidate_list}\n\n\
            <prefetched_candidate_commits>\n\
            {prefetched_commits}\n\
            </prefetched_candidate_commits>\n\n\
            <current_code_at_linus_sha>\n\
            {prefetched_code}\n\
            </current_code_at_linus_sha>\n\n\
            ## Task\n\
            Determine whether the bug described above has been fixed in {tree_label} at `{linus_sha}`.\n\
            1. Inspect the candidate commits and the current code at `{linus_sha}`. If the prefetched context is insufficient (for example, more than {max_commits} candidate commits exist or a fix moved across files), use `git_show`, `git_diff`, `git_log`, or `git_read_files` at `{linus_sha}`.\n\
            2. If a commit in `{prev_sha}..{linus_sha}` fixes the defect (or deletes the buggy code path so the defect no longer exists), set `\"status\": \"fixed\"` and set `\"fixing_commit_sha\"` to that commit's SHA.\n\
            3. If the defect is still present at `{linus_sha}`, set `\"status\": \"still_present\"` and `\"fixing_commit_sha\": null`.\n\
            4. If you cannot conclusively determine whether the bug is fixed or still present, set `\"status\": \"uncertain\"` and `\"fixing_commit_sha\": null`.\n\n\
            Return ONLY a valid JSON object matching:\n\
            {{\n\
              \"status\": \"fixed\" | \"still_present\" | \"uncertain\",\n\
              \"fixing_commit_sha\": \"<40-char or >=7-char commit SHA, or null>\",\n\
              \"explanation\": \"Cite the exact commit and code changes that resolved the bug, or explain why the defect remains present at {linus_sha}.\"\n\
            }}",
            prev_sha = self.previous_sha,
            sha_label = sha_label,
            linus_sha = self.linus_sha,
            bug_heading = bug_heading,
            bug_id = self.bug.id,
            bugid = self.bug.bugid,
            title = self.bug.problem(),
            intro = intro_str,
            files = files_str,
            locations = locations_str,
            report = report_body,
            total_candidates = self.candidate_shas.len(),
            candidate_list = candidate_list,
            prefetched_commits = self.prefetched_commits,
            prefetched_code = self.prefetched_code,
            tree_label = tree_label,
            max_commits = MAX_FIX_CANDIDATE_COMMITS,
        )
    }

    fn tools(&self) -> Option<Vec<AiTool>> {
        self.tools.as_ref().map(|t| t.get_declarations_generic())
    }

    async fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        let Some(ref tools) = self.tools else {
            bail!("Tool execution requested but no toolbox available");
        };
        let repeated = self
            .last_turn_tool_calls
            .iter()
            .any(|prev| prev.0 == name && prev.1 == args);
        if repeated {
            warn!("Blocked duplicate tool call: {} with args {:?}", name, args);
            return Ok(json!({
                "error": "Duplicate tool call blocked. Please change parameters or use a different tool."
            }));
        }
        self.last_turn_tool_calls = vec![(name.to_string(), args.clone())];
        match tools.call(name, args).await {
            Ok(v) => Ok(v),
            Err(e) => Ok(json!({ "error": e.to_string() })),
        }
    }

    async fn call_tools(&mut self, calls: Vec<ToolCall>) -> Result<Vec<(String, Value)>> {
        let Some(ref tools) = self.tools else {
            bail!("Tool execution requested but no toolbox available");
        };
        let mut results: Vec<Option<(String, Value)>> = vec![None; calls.len()];
        let mut to_run = Vec::new();
        let mut current_turn_calls: Vec<(String, Value)> = Vec::new();

        for (idx, call) in calls.into_iter().enumerate() {
            let repeated = self
                .last_turn_tool_calls
                .iter()
                .any(|prev| prev.0 == call.function_name && prev.1 == call.arguments)
                || current_turn_calls
                    .iter()
                    .any(|prev| prev.0 == call.function_name && prev.1 == call.arguments);
            if repeated {
                warn!(
                    "Blocked duplicate tool call: {} with args {:?}",
                    call.function_name, call.arguments
                );
                results[idx] = Some((
                    call.id,
                    json!({
                        "error": "Duplicate tool call blocked. Please change parameters or use a different tool."
                    }),
                ));
            } else {
                current_turn_calls.push((call.function_name.clone(), call.arguments.clone()));
                to_run.push((idx, call));
            }
        }
        if !current_turn_calls.is_empty() {
            self.last_turn_tool_calls = current_turn_calls;
        }

        let futures = to_run.into_iter().map(|(idx, call)| {
            let tools = tools.clone();
            async move {
                let res = match tools.call(&call.function_name, call.arguments).await {
                    Ok(v) => v,
                    Err(e) => json!({ "error": e.to_string() }),
                };
                (idx, (call.id, res))
            }
        });
        for (idx, res) in futures::future::join_all(futures).await {
            results[idx] = Some(res);
        }

        Ok(results.into_iter().flatten().collect())
    }

    fn response_format(&self) -> Option<AiResponseFormat> {
        Some(AiResponseFormat::Json { schema: None })
    }

    fn context_tag(&self) -> Option<String> {
        self.context_tag.clone()
    }

    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
        let text = response.content.as_deref().unwrap_or("");
        let mut parsed: UpstreamFixVerdict = crate::workflow::output::parse_json_from_text(text)
            .map_err(ValidationError::FormatViolation)?;

        parsed.status = parsed.status.trim().to_lowercase();
        if !matches!(
            parsed.status.as_str(),
            "fixed" | "still_present" | "uncertain"
        ) {
            return Err(ValidationError::FormatViolation(format!(
                "status must be one of 'fixed', 'still_present', or 'uncertain', got '{}'",
                parsed.status
            )));
        }

        if parsed.explanation.trim().is_empty() {
            return Err(ValidationError::FormatViolation(
                "explanation must not be empty".to_string(),
            ));
        }

        if parsed.status == "fixed" {
            let sha = parsed
                .fixing_commit_sha
                .as_deref()
                .map(str::trim)
                .unwrap_or("");
            if sha.len() < 7 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(ValidationError::FormatViolation(
                    "fixing_commit_sha must be a valid hexadecimal commit SHA (>= 7 chars) when status is 'fixed'"
                        .to_string(),
                ));
            }
            parsed.fixing_commit_sha = Some(sha.to_string());
        } else {
            parsed.fixing_commit_sha = None;
        }

        Ok(parsed)
    }
}

/// Checks whether a single open bug has been fixed in the upstream mainline tree at `linus_sha`.
///
/// Uses a three-tier pipeline:
/// 1. Fast skip if `bug.verified_on_sha() == Some(linus_sha)`.
/// 2. Deterministic zero-token git check (`find_candidate_fix_commits`): if no
///    commits in `verified_on_sha..linus_sha` touched the bug's files or
///    referenced its introducing commit, advances `verified_on_sha` to
///    `linus_sha` without invoking the LLM.
/// 3. Single-stage LLM verification (`VerifyUpstreamFixSession`) when candidate
///    commits exist, guarded by deterministic git commit + ancestry verification.
pub async fn check_bug_fixed_upstream(
    provider: &dyn AiProvider,
    repo_path: &std::path::Path,
    db: &Database,
    bug: &Bug,
    linus_sha: &str,
) -> Result<UpstreamFixCheckOutcome> {
    let project = infer_project_from_bug_or_tool(Some(&bug.bugid), None, ProjectId::Linux);
    check_bug_fixed_upstream_for_project(provider, repo_path, db, bug, linus_sha, project).await
}

pub async fn check_bug_fixed_upstream_for_project(
    provider: &dyn AiProvider,
    repo_path: &std::path::Path,
    db: &Database,
    bug: &Bug,
    linus_sha: &str,
    project: ProjectId,
) -> Result<UpstreamFixCheckOutcome> {
    check_bug_fixed_upstream_with_candidates(provider, repo_path, db, bug, linus_sha, project, None)
        .await
}

pub async fn check_bug_fixed_upstream_with_candidates(
    provider: &dyn AiProvider,
    repo_path: &std::path::Path,
    db: &Database,
    bug: &Bug,
    linus_sha: &str,
    project: ProjectId,
    precomputed_candidates: Option<Vec<String>>,
) -> Result<UpstreamFixCheckOutcome> {
    let linus_sha = linus_sha.trim();
    let Some(prev_sha) = bug.verified_on_sha() else {
        db.touch_bug_fix_check_timestamp(bug.id).await?;
        return Ok(UpstreamFixCheckOutcome::SkippedNotAncestor);
    };
    if prev_sha.trim() == linus_sha {
        db.release_bug_lease(bug.id).await?;
        return Ok(UpstreamFixCheckOutcome::SkippedAlreadyAtSha);
    }

    let files = extract_bug_files(bug);
    let intro_sha = bug.introducing_commit_sha();
    let locations = bug.locations();
    let source_files = bug.source_files();

    let candidates = match precomputed_candidates {
        Some(c) => c,
        None => {
            let Some(c) = find_candidate_fix_commits(
                repo_path,
                &prev_sha,
                linus_sha,
                &files,
                intro_sha.as_deref(),
            )
            .await?
            else {
                db.touch_bug_fix_check_timestamp(bug.id).await?;
                return Ok(UpstreamFixCheckOutcome::SkippedNotAncestor);
            };
            c
        }
    };

    let fix_tool = match project {
        ProjectId::Linux => "sashiko:linux_bug:fix_check",
        ProjectId::Sashiko => "sashiko:sashiko_bug:fix_check",
    };

    if candidates.is_empty() {
        db.with_bug_actor("sashiko", fix_tool, None)
            .record_upstream_fix_check(
                bug.id,
                crate::db::UpstreamFixCheckParams {
                    verified_on_sha: linus_sha,
                    fixing_commit_sha: None,
                    explanation: None,
                    locations: locations.as_ref(),
                    source_files: source_files.as_deref(),
                    llm_checked: false,
                    ..Default::default()
                },
            )
            .await?;
        return Ok(UpstreamFixCheckOutcome::AdvancedWithoutLlm {
            verified_on_sha: linus_sha.to_string(),
        });
    }

    let attributed_db = db.with_bug_actor(
        "sashiko",
        fix_tool,
        Some(provider.get_capabilities().model_name),
    );

    let mut tb = ToolBox::new(repo_path.to_path_buf(), None);
    tb.set_virtual_head(linus_sha.to_string());
    let tools = Arc::new(tb);

    let prefetched_commits = prefetch_candidate_fix_commits(repo_path, &candidates, &files).await;
    let prefetched_code = prefetch_bug_locations(Some(&tools), linus_sha, &locations).await;

    let mut session = VerifyUpstreamFixSession {
        bug,
        affected_files: &files,
        previous_sha: &prev_sha,
        linus_sha,
        candidate_shas: &candidates,
        prefetched_commits,
        prefetched_code,
        tools: Some(tools),
        context_tag: Some("upstream_fix_check".to_string()),
        last_turn_tool_calls: Vec::new(),
        project,
    };

    let runner = SessionRunner::new(provider).with_max_turns(8);
    let session_result = runner.run(&mut session).await?;
    let verdict = session_result.output;
    let logs = serde_json::to_string(&session_result.history).ok();

    match verdict.status.as_str() {
        "fixed" => {
            let raw_fix_sha = verdict.fixing_commit_sha.as_deref().unwrap_or("");
            let Some(verified_fix_sha) =
                verify_commit_is_ancestor(repo_path, raw_fix_sha, linus_sha).await
            else {
                warn!(
                    "Upstream fix check for bug #{} ({}) returned non-ancestor or invalid commit '{}'; keeping bug open",
                    bug.id, bug.bugid, raw_fix_sha
                );
                attributed_db.touch_bug_fix_check_timestamp(bug.id).await?;
                return Ok(UpstreamFixCheckOutcome::Uncertain {
                    explanation: verdict.explanation,
                });
            };

            let fixing_commit_title = commit_subject(repo_path, &verified_fix_sha).await;

            attributed_db
                .record_upstream_fix_check(
                    bug.id,
                    crate::db::UpstreamFixCheckParams {
                        verified_on_sha: linus_sha,
                        fixing_commit_sha: Some(&verified_fix_sha),
                        fixing_commit_title: fixing_commit_title.as_deref(),
                        explanation: Some(&verdict.explanation),
                        locations: locations.as_ref(),
                        source_files: source_files.as_deref(),
                        llm_checked: true,
                        logs: logs.as_deref(),
                        tokens_in: Some(session_result.usage.prompt_tokens),
                        tokens_out: Some(session_result.usage.completion_tokens),
                        tokens_cached: session_result.usage.cached_tokens,
                    },
                )
                .await?;

            Ok(UpstreamFixCheckOutcome::FixedUpstream {
                fixing_commit_sha: verified_fix_sha,
                verified_on_sha: linus_sha.to_string(),
                explanation: verdict.explanation,
            })
        }
        "still_present" => {
            attributed_db
                .record_upstream_fix_check(
                    bug.id,
                    crate::db::UpstreamFixCheckParams {
                        verified_on_sha: linus_sha,
                        fixing_commit_sha: None,
                        fixing_commit_title: None,
                        explanation: Some(&verdict.explanation),
                        locations: locations.as_ref(),
                        source_files: source_files.as_deref(),
                        llm_checked: true,
                        logs: logs.as_deref(),
                        tokens_in: Some(session_result.usage.prompt_tokens),
                        tokens_out: Some(session_result.usage.completion_tokens),
                        tokens_cached: session_result.usage.cached_tokens,
                    },
                )
                .await?;

            Ok(UpstreamFixCheckOutcome::StillPresentAfterLlm {
                verified_on_sha: linus_sha.to_string(),
                explanation: verdict.explanation,
            })
        }
        _ => {
            attributed_db.touch_bug_fix_check_timestamp(bug.id).await?;
            Ok(UpstreamFixCheckOutcome::Uncertain {
                explanation: verdict.explanation,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiRequest, ProviderCapabilities};
    use crate::db::{BugEnrichment, SubsystemSource};
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
            subsystems: vec![AttributedSubsystem::from_maintainers("net")],
            source_files: vec!["net/core/dev.c".to_string()],
            commit_sha: None,
            patchset_id: None,
            patch_id: None,
            baseline_sha: None,
            review_id: None,
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
            project: ProjectId::Linux,
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

        let thread_id = db.create_thread("t1", "subj", 100).await.unwrap();
        let ps_id = db
            .create_patchset(
                thread_id, None, "m1", "subj", "auth", 100, 1, 0, "", "", None, 1, None, false,
                None, None,
            )
            .await
            .unwrap()
            .unwrap();
        let rev_id = db
            .create_review(ps_id, None, "gemini", "mock", None, None)
            .await
            .unwrap();

        let input = BugInput {
            problem: "NULL deref in net/core/dev.c".to_string(),
            reasoning: "ptr might be null".to_string(),
            locations: Some(json!([{"file": "net/core/dev.c", "line": 100}])),
            subsystems: vec![AttributedSubsystem::from_maintainers("net")],
            source_files: vec!["net/core/dev.c".to_string()],
            commit_sha: None,
            patchset_id: None,
            patch_id: None,
            baseline_sha: None,
            review_id: Some(rev_id),
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

        let linked = db.list_bugs_for_review(rev_id).await.unwrap();
        assert!(
            linked.is_empty(),
            "Discarded bug must NOT be linked to review"
        );

        let bug = db.get_bug(1).await.unwrap().unwrap();
        assert_eq!(
            bug.lifecycle_status,
            crate::db::BugLifecycleStatus::Dismissed
        );
        assert_eq!(bug.problem(), "net: dev: null dereference in dev_read()");
        assert_eq!(bug.subsystems, vec!["net".to_string()]);
        assert_eq!(bug.source_files(), Some(vec!["net/core/dev.c".to_string()]));
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
            project: ProjectId::Linux,
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
            project: ProjectId::Linux,
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
            project: ProjectId::Linux,
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
            project: ProjectId::Linux,
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
            id: 42,
            bugid: "linux-42".to_string(),
            title: "Memory leak in dev.c".to_string(),
            lifecycle_status: crate::db::BugLifecycleStatus::Open,
            pipeline_state: crate::db::BugPipelineState::Succeeded,
            assignee: None,
            assigned_at: None,
            reporter: "sashiko".to_string(),
            reported_at: 100,
            discovered_in_patchset_id: None,
            discovered_in_patch_id: None,
            discovered_in_commit: None,
            source_ref: None,
            vector_json: None,
            duplicate_of_id: None,
            created_at: 100,
            updated_at: 100,
            subsystems: vec!["net".to_string()],
            enrichments: vec![BugEnrichment {
                id: 1,
                bug_id: 42,
                kind: "severity_calibration".to_string(),
                tool: "sashiko".to_string(),
                model: None,
                author: None,
                created_at: 100,
                content: None,
                data_json: Some(json!({"severity": "High", "severity_int": 3})),
                tokens_in: None,
                tokens_out: None,
                tokens_cached: None,
                logs: None,
            }],
        }];

        let mut session = DedupSession {
            candidate_problem: "Memory leak in net/core/dev.c",
            candidate_description: "Buffer allocated in dev_alloc is leaked on error path.",
            candidate_locations: None,
            candidate_subsystems: &["net".to_string()],
            known_candidates: &known_bugs,
            context_tag: None,
            project: ProjectId::Linux,
        };

        let runner = SessionRunner::new(&mock_provider);
        let res = runner.run(&mut session).await.unwrap();
        assert!(res.output.is_duplicate);
        assert_eq!(res.output.duplicate_of_id, Some(42));
    }

    #[tokio::test]
    async fn test_dedup_session_prompt_directives() {
        let known_bugs = vec![Bug {
            id: 42,
            bugid: "linux-42".to_string(),
            title: "Memory leak in dev.c".to_string(),
            lifecycle_status: crate::db::BugLifecycleStatus::Open,
            pipeline_state: crate::db::BugPipelineState::Succeeded,
            assignee: None,
            assigned_at: None,
            reporter: "sashiko".to_string(),
            reported_at: 100,
            discovered_in_patchset_id: None,
            discovered_in_patch_id: None,
            discovered_in_commit: None,
            source_ref: None,
            vector_json: None,
            duplicate_of_id: None,
            created_at: 100,
            updated_at: 100,
            subsystems: vec!["net".to_string()],
            enrichments: vec![BugEnrichment {
                id: 1,
                bug_id: 42,
                kind: "severity_calibration".to_string(),
                tool: "sashiko".to_string(),
                model: None,
                author: None,
                created_at: 100,
                content: None,
                data_json: Some(json!({"severity": "High", "severity_int": 3})),
                tokens_in: None,
                tokens_out: None,
                tokens_cached: None,
                logs: None,
            }],
        }];

        let session = DedupSession {
            candidate_problem: "Use-after-free crash in dev.c due to race",
            candidate_description: "Concurrent teardown frees netdev while rx handler is running.",
            candidate_locations: None,
            candidate_subsystems: &["net".to_string()],
            known_candidates: &known_bugs,
            context_tag: None,
            project: ProjectId::Linux,
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

    #[test]
    fn test_dedup_session_prompt_includes_locations_and_descriptions() {
        let known_bugs = vec![Bug {
            id: 42,
            bugid: "linux-42".to_string(),
            title: "amdgpu: missing ring_muxer cleanup on pci remove".to_string(),
            lifecycle_status: crate::db::BugLifecycleStatus::Fixed,
            pipeline_state: crate::db::BugPipelineState::Succeeded,
            assignee: None,
            assigned_at: None,
            reporter: "sashiko".to_string(),
            reported_at: 100,
            discovered_in_patchset_id: None,
            discovered_in_patch_id: None,
            discovered_in_commit: None,
            source_ref: None,
            vector_json: None,
            duplicate_of_id: None,
            created_at: 100,
            updated_at: 100,
            subsystems: vec!["DRM DRIVERS FOR AMD GPU".to_string()],
            enrichments: vec![
                BugEnrichment {
                    id: 1,
                    bug_id: 42,
                    kind: "verification".to_string(),
                    tool: "sashiko".to_string(),
                    model: None,
                    author: None,
                    created_at: 100,
                    content: Some("Verified missing ring_muxer_fini call.".to_string()),
                    data_json: Some(json!({
                        "verified_on_sha": "deadbeef",
                        "is_valid": true,
                        "source_files": ["drivers/gpu/drm/amd/amdgpu/amdgpu_ring_mux.c"],
                        "locations": [{
                            "file": "drivers/gpu/drm/amd/amdgpu/amdgpu_ring_mux.c",
                            "line": 218,
                            "function": "amdgpu_ring_mux_fini"
                        }]
                    })),
                    tokens_in: None,
                    tokens_out: None,
                    tokens_cached: None,
                    logs: None,
                },
                BugEnrichment {
                    id: 2,
                    bug_id: 42,
                    kind: "report".to_string(),
                    tool: "sashiko".to_string(),
                    model: None,
                    author: None,
                    created_at: 101,
                    content: Some(
                        "In amdgpu_ring_mux_fini(), the timer is not cancelled before freeing."
                            .to_string(),
                    ),
                    data_json: None,
                    tokens_in: None,
                    tokens_out: None,
                    tokens_cached: None,
                    logs: None,
                },
            ],
        }];

        let cand_locs = json!([{
            "file": "drivers/gpu/drm/amd/amdgpu/amdgpu_ring_mux.c",
            "line": 224,
            "function": "amdgpu_ring_mux_fini"
        }]);

        let session = DedupSession {
            candidate_problem: "amdgpu: use-after-free of mux timer after ring_mux_fini",
            candidate_description: "Trigger: GPU reset or unload.\nFailure Mechanism: Timer fires after ring_mux_fini frees the struct.\nImpact: Use-after-free.",
            candidate_locations: Some(&cand_locs),
            candidate_subsystems: &["DRM DRIVERS FOR AMD GPU".to_string()],
            known_candidates: &known_bugs,
            context_tag: None,
            project: ProjectId::Linux,
        };

        let prompt = session.initial_user_prompt();
        // Candidate description and line number must be preserved.
        assert!(prompt.contains("Timer fires after ring_mux_fini frees the struct."));
        assert!(prompt.contains("\"line\": 224"));
        // Known bug status, function/line locations, and description must be present.
        assert!(prompt.contains("[Status: fixed]"));
        assert!(prompt.contains(
            "Locations: drivers/gpu/drm/amd/amdgpu/amdgpu_ring_mux.c:218 (amdgpu_ring_mux_fini)"
        ));
        assert!(prompt.contains(
            "Description: In amdgpu_ring_mux_fini(), the timer is not cancelled before freeing."
        ));

        let mut long_bug = known_bugs[0].clone();
        long_bug.enrichments[1].content = Some("a".repeat(MAX_DEDUP_DESCRIPTION_BYTES + 50));
        let truncated_desc = format_known_bug_description(&long_bug);
        assert!(truncated_desc.ends_with("...[truncated]..."));
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

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 8192,
            }
        }
    }

    #[tokio::test]
    async fn test_bug_completed_stage_survives_later_failure() {
        let db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".into(),
            token: String::new(),
        })
        .await
        .unwrap();
        db.migrate().await.unwrap();
        let provider = QueuedMockAiProvider::new(vec![
            json!({
                "canonical_title": "net: missing length check",
                "canonical_description": "An unchecked length overruns the buffer.",
                "affected_source_files": ["net/core/dev.c"]
            })
            .to_string(),
        ]);
        // The mock has no valid response for verification, after normalization succeeds.
        let input = BugInput {
            problem: "Unchecked length".into(),
            reasoning: "Length is unbounded".into(),
            locations: None,
            subsystems: vec![AttributedSubsystem::from_maintainers("net")],
            source_files: vec!["net/core/dev.c".into()],
            commit_sha: None,
            patchset_id: None,
            patch_id: None,
            baseline_sha: None,
            review_id: None,
        };
        let BugOutcome::NewlyDiscovered { bug } =
            process_issue(&provider, None, &db, input.clone(), None)
                .await
                .unwrap()
        else {
            panic!("Expected candidate");
        };
        assert!(
            process_issue_worker(&provider, None, &db, &bug, input, None)
                .await
                .is_err()
        );
        let raw = db.bug_family(bug.id, true).await.unwrap();
        let stage = raw[0]
            .enrichments
            .iter()
            .find(|e| e.kind == "normalization_run")
            .expect("Completed normalization must be retained");
        assert_eq!(stage.model.as_deref(), Some("mock"));
        assert_eq!(stage.author.as_deref(), Some("sashiko"));
        assert!(
            stage
                .logs
                .as_deref()
                .unwrap()
                .contains("missing length check")
        );
        assert_eq!(
            db.bug_evidence(&db.bug_family(bug.id, false).await.unwrap())
                .await
                .unwrap()["count"],
            1
        );
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
            project: ProjectId::Linux,
        };

        let runner = SessionRunner::new(&mock_provider);
        let res = runner.run(&mut session).await.unwrap();
        assert!(res.output.contains("int *ptr = alloc();"));
    }

    #[test]
    fn test_bug_stages_are_named_not_numbered() {
        for stage in BugStage::ALL {
            assert_eq!(stage.heading(), format!("# {}", stage.title()));
            assert_eq!(stage.enrichment_kind(), format!("{}_run", stage.id()));
            // A heading that carried a position would go stale the moment a
            // stage was inserted ahead of it.
            assert!(
                !stage.heading().contains(char::is_numeric),
                "{stage:?} heading names a position: {}",
                stage.heading()
            );
        }
        let ids: std::collections::BTreeSet<&str> = BugStage::ALL.iter().map(|s| s.id()).collect();
        assert_eq!(ids.len(), BugStage::ALL.len());
        assert_eq!(BugStage::ReportGeneration.heading(), "# Report generation");
    }

    #[test]
    fn test_stage_prompts_open_with_their_stage_heading() {
        let input = BugInput {
            problem: "UAF in foo()".to_string(),
            reasoning: "Freed then used".to_string(),
            locations: None,
            subsystems: vec![AttributedSubsystem::from_maintainers("net")],
            source_files: vec!["net/foo.c".to_string()],
            commit_sha: None,
            patchset_id: None,
            patch_id: None,
            baseline_sha: None,
            review_id: None,
        };
        let no_files: Vec<String> = Vec::new();
        let subsystem_names: Vec<String> =
            input.subsystems.iter().map(|s| s.name.clone()).collect();

        let prompts: Vec<(BugStage, String)> = vec![
            (
                BugStage::Normalization,
                NormalizeSession {
                    problem: &input.problem,
                    reasoning: &input.reasoning,
                    locations: "[]",
                    master_sha: "abc123",
                    maintainers_hint: None,
                    tools: None,
                    context_tag: None,
                    project: ProjectId::Linux,
                }
                .initial_user_prompt(),
            ),
            (
                BugStage::Verification,
                VerifySession {
                    title: &input.problem,
                    description: &input.reasoning,
                    subsystem: "net",
                    affected_files: &no_files,
                    locations: None,
                    master_sha: "abc123".to_string(),
                    tools: None,
                    context_tag: None,
                    prefetched_context: String::new(),
                    project: ProjectId::Linux,
                }
                .initial_user_prompt(),
            ),
            (
                BugStage::Deduplication,
                DedupSession {
                    candidate_problem: &input.problem,
                    candidate_description: &input.reasoning,
                    candidate_locations: None,
                    candidate_subsystems: &subsystem_names,
                    known_candidates: &[],
                    context_tag: None,
                    project: ProjectId::Linux,
                }
                .initial_user_prompt(),
            ),
            (
                BugStage::OriginTracing,
                TracingSession {
                    input: &input,
                    master_sha: "abc123".to_string(),
                    verification_reasoning: "verified".to_string(),
                    relevant_locations: "[]".to_string(),
                    tools: None,
                    context_tag: None,
                    project: ProjectId::Linux,
                }
                .initial_user_prompt(),
            ),
            (
                BugStage::SeverityAssessment,
                SeveritySession {
                    canonical_title: &input.problem,
                    canonical_description: &input.reasoning,
                    locations: "[]",
                    context_tag: None,
                    project: ProjectId::Linux,
                }
                .initial_user_prompt(),
            ),
            (
                BugStage::ReportGeneration,
                ReportSession {
                    problem: &input.problem,
                    severity: "High",
                    canonical_description: &input.reasoning,
                    severity_explanation: "reachable",
                    locations: None,
                    introduced_in_commit: None,
                    tools: None,
                    context_tag: None,
                    prefetched_context: String::new(),
                    project: ProjectId::Linux,
                }
                .initial_user_prompt(),
            ),
        ];

        assert_eq!(prompts.len(), BugStage::ALL.len());
        for (stage, prompt) in prompts {
            assert!(
                prompt.starts_with(&stage.heading()),
                "{:?} prompt must open with its heading, got: {}",
                stage,
                prompt.chars().take(80).collect::<String>()
            );
        }
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
            project: ProjectId::Linux,
        };

        let sys_prompt = session.system_prompt();
        assert!(sys_prompt.contains("On a 32-bit architecture"));
        assert!(sys_prompt.contains("<...>"));
        assert!(sys_prompt.contains("^^^^^"));
        assert!(
            sys_prompt.contains("Paired Actions Rule")
                || sys_prompt.contains("Paired actions rule")
        );
        assert!(sys_prompt.contains("NEVER use carets to point at a missing thing"));
        assert!(sys_prompt.contains("75 characters per line"));
        assert!(sys_prompt.contains("CPU 0"));
        assert!(sys_prompt.contains("parse_durable_handle_context"));
        assert!(sys_prompt.contains("ffs_epfile_open"));
        assert!(sys_prompt.contains("ovl_create_or_link"));
        assert!(sys_prompt.contains("snd_pcm_hw_params"));
        assert!(sys_prompt.contains("slots_lock"));
        assert!(sys_prompt.contains("Include code snippets for all localized defects"));
        assert!(sys_prompt.contains("missing architectural hook"));

        let user_prompt = session.initial_user_prompt();
        assert!(user_prompt.contains("32-bit machine"));
        assert!(user_prompt.contains("<...>"));
        assert!(user_prompt.contains("^^^^^"));
        assert!(user_prompt.contains("NEVER use carets to highlight a missing call"));
        assert!(user_prompt.contains("multi-CPU timeline diagram"));
        assert!(user_prompt.contains("Include code snippets for all localized defects"));
        assert!(user_prompt.contains("missing architectural hook"));

        // Snippets are flush left so code keeps its full 75 column budget.
        assert!(sys_prompt.contains("Start every snippet line at column 0"));
        assert!(user_prompt.contains("Start every snippet and timeline line at column 0"));
        assert!(sys_prompt.contains("\n// fs/smb/server/smb2pdu.c:2450-2475\n"));
        assert!(sys_prompt.contains("\nstatic int parse_durable_handle_context(...)\n"));
        assert!(sys_prompt.contains("\nCPU 0 (removal thread)"));
        // Column alignment inside multi-column timelines is still allowed, so
        // only the four space block wrapper is rejected.
        for line in sys_prompt.lines() {
            let indent = line.len() - line.trim_start_matches(' ').len();
            assert_ne!(
                indent, 4,
                "example blocks must not be wrapped in four space indentation: {line:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_severity_session_uses_prompt_file_directly() {
        let session = SeveritySession {
            canonical_title: "net: memory leak in dev_alloc()",
            canonical_description: "Buffer allocated but not freed",
            locations: "[]",
            context_tag: None,
            project: ProjectId::Linux,
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

        let rev_id = db
            .create_review(ps_id, None, "gemini", "mock", None, None)
            .await
            .unwrap();

        let input = BugInput {
            problem: "Buffer overflow in e1000 rx handler".to_string(),
            reasoning: "Size not checked against MTU".to_string(),
            locations: Some(
                json!([{"file": "drivers/net/ethernet/intel/e1000/e1000_main.c", "line": 250}]),
            ),
            subsystems: vec![AttributedSubsystem::from_maintainers("net/intel")],
            source_files: vec!["drivers/net/ethernet/intel/e1000/e1000_main.c".to_string()],
            commit_sha: Some("abcdef123456".to_string()),
            patchset_id: Some(ps_id),
            patch_id: None,
            baseline_sha: None,
            review_id: Some(rev_id),
        };

        let outcome = process_issue(&provider, None, &db, input.clone(), None)
            .await
            .unwrap();

        // While pending, the bug must NOT be linked to the review yet (Invariant 2).
        let pre_linked = db.list_bugs_for_review(rev_id).await.unwrap();
        assert!(
            pre_linked.is_empty(),
            "Pending bug must NOT be linked to review yet"
        );

        let final_outcome = match outcome {
            BugOutcome::NewlyDiscovered { ref bug } => {
                process_issue_worker(&provider, None, &db, bug, input.clone(), None)
                    .await
                    .unwrap()
            }
            _ => panic!("Expected NewlyDiscovered outcome initially"),
        };

        match final_outcome {
            BugOutcome::NewlyDiscovered { ref bug } => {
                assert_eq!(
                    bug.problem(),
                    "e1000: buffer overflow in e1000_clean_rx_irq()"
                );
                assert_eq!(bug.severity(), Severity::Critical);
                assert!(bug.bugid.starts_with("linux-"));
                assert!(uuid::Uuid::parse_str(&bug.bugid[6..]).is_ok());
                assert_eq!(bug.discovered_in_patchset_id, Some(ps_id));
                assert_eq!(bug.subsystems, vec!["net/intel".to_string()]);
                assert_eq!(
                    bug.introduced_in_commit().as_deref(),
                    Some("1234567890ab1234567890ab1234567890ab1234")
                );
                assert!(!bug.is_fixed());
                assert!(bug.fixed_in_commit().is_none());
                assert!(!bug.enrichments.is_empty(), "Enrichments must be populated");
                assert!(bug.raw_input().is_some(), "Raw input must be preserved");

                let linked = db.list_bugs_for_review(rev_id).await.unwrap();
                assert_eq!(linked.len(), 1);
                assert_eq!(linked[0].0.id, bug.id);
                assert!(linked[0].1, "Should be marked is_newly_discovered");
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
                bugid: "linux-existing1".to_string(),
                title: "e1000: buffer overflow in e1000_clean_rx_irq()".to_string(),
                lifecycle_status: crate::db::BugLifecycleStatus::Open,
                pipeline_state: crate::db::BugPipelineState::Succeeded,
                assignee: None,
                reporter: "sashiko".to_string(),
                reported_at: 1000,
                discovered_in_patchset_id: None,
                discovered_in_patch_id: None,
                discovered_in_commit: None,
                source_ref: None,
                vector_json: Some(existing_vector.to_json()),
                duplicate_of_id: None,
                subsystems: vec![AttributedSubsystem::from_maintainers("net/intel")],
            })
            .await
            .unwrap();
        db.add_bug_enrichment(
            existing_id,
            &crate::db::NewBugEnrichment {
                kind: "severity_calibration".to_string(),
                tool: "sashiko".to_string(),
                model: None,
                author: None,
                created_at: 1000,
                content: Some("Known buffer overflow".to_string()),
                data_json: Some(serde_json::json!({
                    "severity": "High",
                    "severity_int": 3,
                })),
                ..Default::default()
            },
        )
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

        let thread_id = db.create_thread("t1", "subj", 100).await.unwrap();
        let ps_id = db
            .create_patchset(
                thread_id, None, "m1", "subj", "auth", 100, 1, 0, "", "", None, 1, None, false,
                None, None,
            )
            .await
            .unwrap()
            .unwrap();
        let rev_id = db
            .create_review(ps_id, None, "gemini", "mock", None, None)
            .await
            .unwrap();

        let input = BugInput {
            problem: "Buffer overflow in e1000 rx handler".to_string(),
            reasoning: "Size not checked against MTU".to_string(),
            locations: Some(
                json!([{"file": "drivers/net/ethernet/intel/e1000/e1000_main.c", "line": 250}]),
            ),
            subsystems: vec![AttributedSubsystem::from_maintainers("net/intel")],
            source_files: vec!["drivers/net/ethernet/intel/e1000/e1000_main.c".to_string()],
            commit_sha: Some("abcdef123456".to_string()),
            patchset_id: None,
            patch_id: None,
            baseline_sha: None,
            review_id: Some(rev_id),
        };

        let outcome = process_issue(&provider, None, &db, input.clone(), None)
            .await
            .unwrap();

        // While pending, the bug must NOT be linked to the review yet (Invariant 2).
        let pre_linked = db.list_bugs_for_review(rev_id).await.unwrap();
        assert!(
            pre_linked.is_empty(),
            "Pending bug must NOT be linked to review yet"
        );

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

                let linked = db.list_bugs_for_review(rev_id).await.unwrap();
                assert_eq!(linked.len(), 1);
                assert_eq!(linked[0].0.id, existing_id);
                assert!(
                    !linked[0].1,
                    "Duplicate bug must NOT be marked is_newly_discovered"
                );
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
        crate::git_cmd::in_dir(path)
            .args(["init"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(path)
            .args(["config", "user.name", "Test"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(path)
            .args(["config", "user.email", "test@example.com"])
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
        crate::git_cmd::in_dir(path)
            .args(["add", "test_file.c"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(path)
            .args(["commit", "-m", "initial commit"])
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

        crate::git_cmd::in_dir(path)
            .args(["init"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(path)
            .args(["config", "user.name", "Test"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .unwrap();

        let c_code = "int old_fn() {\n    return 42;\n}\n";
        std::fs::write(path.join("old_net.c"), c_code).unwrap();
        crate::git_cmd::in_dir(path)
            .args(["add", "old_net.c"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(path)
            .args(["commit", "-m", "add old_net.c"])
            .output()
            .unwrap();

        // Rename old_net.c -> new_net.c
        crate::git_cmd::in_dir(path)
            .args(["mv", "old_net.c", "new_net.c"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(path)
            .args(["commit", "-m", "rename to new_net.c"])
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

        crate::git_cmd::in_dir(path)
            .args(["init"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(path)
            .args(["config", "user.name", "Test"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .unwrap();

        let c_code = "int a = 1;\nint b = 2;\nint c = 3;\n";
        std::fs::write(path.join("file.c"), c_code).unwrap();
        crate::git_cmd::in_dir(path)
            .args(["add", "file.c"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(path)
            .args(["commit", "-m", "initial commit"])
            .output()
            .unwrap();

        let head_commit = String::from_utf8_lossy(
            &crate::git_cmd::in_dir(path)
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
    fn test_resolve_official_subsystems_keeps_caller_supplied_names_unpromoted() {
        let caller = vec![
            AttributedSubsystem::new("NETWORKING [IPv4/IPv6]", SubsystemSource::CallerSupplied),
            AttributedSubsystem::new("anything at all", SubsystemSource::CallerSupplied),
        ];
        let files = vec!["net/ipv4/tcp.c".to_string()];
        // With no toolbox the caller's own list is used verbatim. Naming a real
        // MAINTAINERS section must not turn it into a MAINTAINERS match, or
        // filing a bug would be enough to choose who can read it.
        let resolved = resolve_official_subsystems(None, &caller, &files);
        assert_eq!(resolved, caller);
        assert!(resolved.iter().all(|s| !s.source.confers_authority()));
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
            project: ProjectId::Linux,
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
            project: ProjectId::Linux,
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

    struct NeverCalledAiProvider;

    #[async_trait]
    impl AiProvider for NeverCalledAiProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            panic!("LLM should not be called when zero commits touched the bug's files");
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "never-called".to_string(),
                context_window_size: 8192,
            }
        }
    }

    fn init_test_git_repo(dir: &std::path::Path) -> String {
        let run = |args: &[&str]| {
            let out = crate::git_cmd::in_dir(dir).args(args).output().unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-b", "master"]);
        run(&["config", "user.name", "Test User"]);
        run(&["config", "user.email", "test@example.com"]);
        std::fs::create_dir_all(dir.join("net/core")).unwrap();
        std::fs::create_dir_all(dir.join("fs/ext4")).unwrap();
        std::fs::write(
            dir.join("net/core/dev.c"),
            "int dev_open(void) {\n    char *buf = alloc();\n    return -EINVAL;\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("fs/ext4/inode.c"),
            "int ext4_iget(void) { return 0; }\n",
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "Initial commit"]);
        run(&["rev-parse", "HEAD"])
    }

    async fn create_open_bug_at_sha(db: &Database, sha: &str) -> i64 {
        let id = db
            .create_bug(&crate::db::NewBug {
                bugid: "bug-upstream-check-1".to_string(),
                title: "net: dev: memory leak in dev_open()".to_string(),
                lifecycle_status: crate::db::BugLifecycleStatus::New,
                pipeline_state: crate::db::BugPipelineState::Running,
                reporter: "sashiko".to_string(),
                reported_at: 1000,
                assignee: None,
                discovered_in_patchset_id: None,
                discovered_in_patch_id: None,
                discovered_in_commit: Some(sha.to_string()),
                source_ref: Some(sha.to_string()),
                vector_json: None,
                duplicate_of_id: None,
                subsystems: vec![AttributedSubsystem::from_maintainers("NETWORKING")],
            })
            .await
            .unwrap();

        let locs = json!([{"file": "net/core/dev.c", "line": 3, "function_or_symbol": "dev_open"}]);
        let files = vec!["net/core/dev.c".to_string()];
        db.update_bug_outcome(
            id,
            crate::db::UpdateBugOutcomeParams {
                lifecycle_status: crate::db::BugLifecycleStatus::Open,
                problem: Some("net: dev: memory leak in dev_open()"),
                source_files: Some(&files),
                locations: Some(&locs),
                severity: Severity::High,
                severity_explanation: Some("Leaks buf on error return"),
                inline_review: "dev_open() leaks buf when returning -EINVAL.",
                verified_on_sha: Some(sha),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        id
    }

    #[tokio::test]
    async fn test_upstream_fix_check_zero_token_advance_when_files_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let sha1 = init_test_git_repo(repo);

        // Second commit only touches fs/ext4/inode.c, leaving net/core/dev.c untouched.
        std::fs::write(
            repo.join("fs/ext4/inode.c"),
            "int ext4_iget(void) { return 1; }\n",
        )
        .unwrap();
        crate::git_cmd::in_dir(repo)
            .args(["commit", "-am", "ext4: update return value"])
            .output()
            .unwrap();
        let sha2 = String::from_utf8_lossy(
            &crate::git_cmd::in_dir(repo)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();

        let db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await
        .unwrap();
        db.migrate().await.unwrap();

        let bug_id = create_open_bug_at_sha(&db, &sha1).await;
        let claimed = db
            .claim_open_bug_for_fix_check(&sha2, "worker-1", 300)
            .await
            .unwrap()
            .expect("open bug should be claimed for fix check");
        // While worker-1 holds the lease, a second worker cannot claim the same bug.
        assert!(
            db.claim_open_bug_for_fix_check(&sha2, "worker-2", 300)
                .await
                .unwrap()
                .is_none()
        );

        let outcome = check_bug_fixed_upstream(&NeverCalledAiProvider, repo, &db, &claimed, &sha2)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            UpstreamFixCheckOutcome::AdvancedWithoutLlm {
                verified_on_sha: sha2.clone()
            }
        );

        let updated = db.get_bug(bug_id).await.unwrap().unwrap();
        assert_eq!(
            updated.lifecycle_status,
            crate::db::BugLifecycleStatus::Open
        );
        assert_eq!(updated.verified_on_sha().as_deref(), Some(sha2.as_str()));
        assert_eq!(
            updated.source_files(),
            Some(vec!["net/core/dev.c".to_string()])
        );
        // Updating the verification SHA in place avoids adding activity feed rows on 0-token sweeps.
        let verification_count = updated
            .enrichments
            .iter()
            .filter(|e| e.kind == "verification")
            .count();
        assert_eq!(verification_count, 1);
        assert!(
            db.claim_open_bug_for_fix_check(&sha2, "worker-1", 300)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_upstream_fix_check_marks_bug_fixed_when_llm_confirms_ancestor_commit() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let sha1 = init_test_git_repo(repo);

        // Second commit touches net/core/dev.c and fixes the leak.
        std::fs::write(
            repo.join("net/core/dev.c"),
            "int dev_open(void) {\n    char *buf = alloc();\n    kfree(buf);\n    return -EINVAL;\n}\n",
        )
        .unwrap();
        crate::git_cmd::in_dir(repo)
            .args(["commit", "-am", "net: dev: free buf on error path"])
            .output()
            .unwrap();
        let sha2 = String::from_utf8_lossy(
            &crate::git_cmd::in_dir(repo)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();

        let db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await
        .unwrap();
        db.migrate().await.unwrap();

        let bug_id = create_open_bug_at_sha(&db, &sha1).await;
        let bug = db.get_bug(bug_id).await.unwrap().unwrap();

        let provider = MockAiProvider {
            response_text: json!({
                "status": "fixed",
                "fixing_commit_sha": &sha2[..12],
                "explanation": "Commit adds kfree(buf) before returning -EINVAL."
            })
            .to_string(),
        };

        let outcome = check_bug_fixed_upstream(&provider, repo, &db, &bug, &sha2)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            UpstreamFixCheckOutcome::FixedUpstream {
                fixing_commit_sha: sha2.clone(),
                verified_on_sha: sha2.clone(),
                explanation: "Commit adds kfree(buf) before returning -EINVAL.".to_string(),
            }
        );

        let updated = db.get_bug(bug_id).await.unwrap().unwrap();
        assert_eq!(
            updated.lifecycle_status,
            crate::db::BugLifecycleStatus::Fixed
        );
        assert!(updated.is_fixed());
        assert_eq!(
            updated.fixed_in_commit().as_deref(),
            Some(format!("{} (net: dev: free buf on error path)", &sha2[..12]).as_str())
        );
        assert_eq!(updated.verified_on_sha().as_deref(), Some(sha2.as_str()));
    }

    #[tokio::test]
    async fn test_upstream_fix_check_rejects_non_ancestor_hallucinated_commit() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let sha1 = init_test_git_repo(repo);

        // Create a commit on an unmerged side branch, then return to master and make an unrelated change to dev.c.
        crate::git_cmd::in_dir(repo)
            .args(["checkout", "-b", "side-branch"])
            .output()
            .unwrap();
        std::fs::write(
            repo.join("net/core/dev.c"),
            "int dev_open(void) { return 0; }\n",
        )
        .unwrap();
        crate::git_cmd::in_dir(repo)
            .args(["commit", "-am", "side branch fix"])
            .output()
            .unwrap();
        let side_sha = String::from_utf8_lossy(
            &crate::git_cmd::in_dir(repo)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();

        crate::git_cmd::in_dir(repo)
            .args(["checkout", "master"])
            .output()
            .unwrap();
        std::fs::write(
            repo.join("net/core/dev.c"),
            "int dev_open(void) {\n    /* comment */\n    char *buf = alloc();\n    return -EINVAL;\n}\n",
        )
        .unwrap();
        crate::git_cmd::in_dir(repo)
            .args(["commit", "-am", "net: dev: add comment"])
            .output()
            .unwrap();
        let master_sha2 = String::from_utf8_lossy(
            &crate::git_cmd::in_dir(repo)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();

        let db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await
        .unwrap();
        db.migrate().await.unwrap();

        let bug_id = create_open_bug_at_sha(&db, &sha1).await;
        let bug = db.get_bug(bug_id).await.unwrap().unwrap();

        // LLM claims side_sha fixed the bug, which is NOT an ancestor of master_sha2.
        let provider = MockAiProvider {
            response_text: json!({
                "status": "fixed",
                "fixing_commit_sha": side_sha,
                "explanation": "Fixed on side branch."
            })
            .to_string(),
        };

        let outcome = check_bug_fixed_upstream(&provider, repo, &db, &bug, &master_sha2)
            .await
            .unwrap();
        assert!(matches!(outcome, UpstreamFixCheckOutcome::Uncertain { .. }));

        let updated = db.get_bug(bug_id).await.unwrap().unwrap();
        assert_eq!(
            updated.lifecycle_status,
            crate::db::BugLifecycleStatus::Open
        );
        assert!(!updated.is_fixed());
    }

    #[tokio::test]
    async fn test_upstream_fix_check_still_present_when_commit_does_not_fix_bug() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let sha1 = init_test_git_repo(repo);

        // Second commit modifies net/core/dev.c with an unrelated comment, leaving the leak intact.
        std::fs::write(
            repo.join("net/core/dev.c"),
            "int dev_open(void) {\n    /* unrelated comment */\n    char *buf = alloc();\n    return -EINVAL;\n}\n",
        )
        .unwrap();
        crate::git_cmd::in_dir(repo)
            .args(["commit", "-am", "net: dev: document dev_open"])
            .output()
            .unwrap();
        let sha2 = String::from_utf8_lossy(
            &crate::git_cmd::in_dir(repo)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();

        let db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await
        .unwrap();
        db.migrate().await.unwrap();

        let bug_id = create_open_bug_at_sha(&db, &sha1).await;
        let bug = db.get_bug(bug_id).await.unwrap().unwrap();

        let provider = MockAiProvider {
            response_text: json!({
                "status": "still_present",
                "fixing_commit_sha": null,
                "explanation": "Commit only adds a comment; buf is still leaked on -EINVAL return."
            })
            .to_string(),
        };

        let outcome = check_bug_fixed_upstream(&provider, repo, &db, &bug, &sha2)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            UpstreamFixCheckOutcome::StillPresentAfterLlm {
                verified_on_sha: sha2.clone(),
                explanation: "Commit only adds a comment; buf is still leaked on -EINVAL return."
                    .to_string(),
            }
        );

        let updated = db.get_bug(bug_id).await.unwrap().unwrap();
        assert_eq!(
            updated.lifecycle_status,
            crate::db::BugLifecycleStatus::Open
        );
        assert!(!updated.is_fixed());
        assert_eq!(updated.verified_on_sha().as_deref(), Some(sha2.as_str()));
    }

    #[tokio::test]
    async fn test_sashiko_bug_triage_and_fix_check_flow() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let run = |args: &[&str]| {
            let out = crate::git_cmd::in_dir(repo).args(args).output().unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.name", "Test User"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&[
            "remote",
            "add",
            "origin",
            "https://github.com/sashiko-dev/sashiko.git",
        ]);
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(
            repo.join("src/api.rs"),
            "pub async fn handle_query(limit: usize) -> usize {\n    let items = vec![1, 2, 3];\n    items[limit]\n}\n",
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "api: initial query handler"]);
        let sha1 = run(&["rev-parse", "HEAD"]);
        run(&["update-ref", "refs/remotes/origin/main", &sha1]);

        let resolved_main = resolve_linus_sha(repo).await;
        assert_eq!(resolved_main.as_deref(), Some(sha1.as_str()));

        let tb = Arc::new(ToolBox::new(repo.to_path_buf(), None));
        let locs = json!([{"file": "src/api.rs", "line": 3, "function_or_symbol": "handle_query"}]);
        let prefetched = prefetch_bug_locations(Some(&tb), &sha1, &Some(locs.clone())).await;
        assert!(
            prefetched.contains("pub async fn handle_query"),
            "Rust function should be prefetched: {}",
            prefetched
        );

        let db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await
        .unwrap();
        db.migrate().await.unwrap();

        let normalize_json = json!({
            "canonical_title": "api: out-of-bounds panic in handle_query()",
            "canonical_description": "Trigger: limit >= 3.\nFailure Mechanism: Unchecked slice index.\nImpact: Worker task panic.",
            "affected_source_files": ["src/api.rs"],
            "affected_symbols": ["handle_query"]
        })
        .to_string();
        let verify_json = json!({
            "verification_reasoning": "items[limit] panics when limit >= 3.",
            "is_false_positive": false,
            "refutation_evidence": null,
            "impact_severity": "High",
            "relevant_code_locations": [{"file": "src/api.rs", "line": 3, "function_or_symbol": "handle_query"}]
        })
        .to_string();
        let tracing_json = json!({
            "introducing_commit_sha": &sha1
        })
        .to_string();
        let severity_json = json!({
            "severity": "High",
            "severity_explanation": "Unbounded limit parameter triggers panic in API handler."
        })
        .to_string();
        let report_text = "api: out-of-bounds panic in handle_query()\n\n// src/api.rs:1-4\npub async fn handle_query(limit: usize) -> usize {\n    let items = vec![1, 2, 3];\n    items[limit]\n    ^^^^^^^^^^^^\n}\n\nIndexing items[limit] without bounds checking panics when limit >= 3.\n".to_string();

        let provider = QueuedMockAiProvider::new(vec![
            normalize_json,
            verify_json,
            tracing_json,
            severity_json,
            report_text,
        ]);

        let input = BugInput {
            problem: "Out-of-bounds index in handle_query".to_string(),
            reasoning: "items[limit] can panic".to_string(),
            locations: Some(locs),
            subsystems: Vec::new(),
            source_files: vec!["src/api.rs".to_string()],
            commit_sha: Some(sha1.clone()),
            patchset_id: None,
            patch_id: None,
            baseline_sha: Some(sha1.clone()),
            review_id: None,
        };

        let BugOutcome::NewlyDiscovered { bug: pending_bug } = process_issue_for_project(
            &provider,
            Some(tb.clone()),
            &db,
            input.clone(),
            None,
            ProjectId::Sashiko,
        )
        .await
        .unwrap() else {
            panic!("Expected NewlyDiscovered pending bug");
        };
        assert!(pending_bug.bugid.starts_with("sashiko-"));
        assert_eq!(pending_bug.subsystems, vec!["api".to_string()]);

        let BugOutcome::NewlyDiscovered { bug: verified_bug } = process_issue_worker_for_project(
            &provider,
            Some(tb),
            &db,
            &pending_bug,
            input,
            None,
            ProjectId::Sashiko,
        )
        .await
        .unwrap() else {
            panic!("Expected NewlyDiscovered verified bug");
        };
        assert_eq!(
            verified_bug.problem(),
            "api: out-of-bounds panic in handle_query()"
        );
        assert_eq!(
            verified_bug.verified_on_sha().as_deref(),
            Some(sha1.as_str())
        );

        // Advance main branch with a fix commit and verify check_bug_fixed_upstream_for_project marks it Fixed.
        std::fs::write(
            repo.join("src/api.rs"),
            "pub async fn handle_query(limit: usize) -> usize {\n    let items = vec![1, 2, 3];\n    items.get(limit).copied().unwrap_or(0)\n}\n",
        )
        .unwrap();
        run(&["commit", "-am", "api: bounds-check handle_query index"]);
        let sha2 = run(&["rev-parse", "HEAD"]);
        run(&["update-ref", "refs/remotes/origin/main", &sha2]);

        let fix_provider = MockAiProvider {
            response_text: json!({
                "status": "fixed",
                "fixing_commit_sha": &sha2,
                "explanation": "Commit replaces items[limit] with items.get(limit).copied().unwrap_or(0)."
            })
            .to_string(),
        };

        let fix_outcome = check_bug_fixed_upstream_for_project(
            &fix_provider,
            repo,
            &db,
            &verified_bug,
            &sha2,
            ProjectId::Sashiko,
        )
        .await
        .unwrap();
        assert!(matches!(
            fix_outcome,
            UpstreamFixCheckOutcome::FixedUpstream { .. }
        ));

        let final_bug = db.get_bug(verified_bug.id).await.unwrap().unwrap();
        assert_eq!(
            final_bug.lifecycle_status,
            crate::db::BugLifecycleStatus::Fixed
        );
        assert_eq!(
            final_bug.fixed_in_commit().as_deref(),
            Some(format!("{} (api: bounds-check handle_query index)", &sha2[..12]).as_str())
        );
        assert_eq!(
            infer_project_from_bug_or_tool(Some("linux-legacy-id"), None, ProjectId::Sashiko),
            ProjectId::Sashiko
        );
        assert_eq!(
            infer_project_from_bug_or_tool(Some("sashiko-123"), None, ProjectId::Linux),
            ProjectId::Sashiko
        );
        let summaries = db
            .bug_discovery_summaries(&[verified_bug.id])
            .await
            .unwrap();
        assert_eq!(
            summaries[&verified_bug.id]["tools"],
            json!(["sashiko:sashiko_patch_review"])
        );
    }

    #[tokio::test]
    async fn test_commit_range_index_and_cross_file_prefetch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();

        let run = |args: &[&str]| {
            let out = crate::git_cmd::in_dir(repo).args(args).output().unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);

        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/reviewer.rs"), "pub fn run() {}\n").unwrap();
        std::fs::write(repo.join("src/git_ops.rs"), "pub struct Worktree;\n").unwrap();
        std::fs::write(repo.join("src/other.rs"), "pub fn other() {}\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "initial"]);
        let sha1 = run(&["rev-parse", "HEAD"]);

        // Commit 2 modifies both src/reviewer.rs and src/git_ops.rs (cross-file fix).
        std::fs::write(
            repo.join("src/reviewer.rs"),
            "pub fn run() { /* rely on Drop */ }\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("src/git_ops.rs"),
            "pub struct Worktree;\nimpl Drop for Worktree { fn drop(&mut self) {} }\n",
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "git_ops: add Drop guard for Worktree"]);
        let sha2 = run(&["rev-parse", "HEAD"]);

        // Commit 3 modifies only src/other.rs but cites Fixes: <sha1[..12]>.
        std::fs::write(repo.join("src/other.rs"), "pub fn other_fixed() {}\n").unwrap();
        run(&["add", "."]);
        run(&[
            "commit",
            "-m",
            &format!("other: fix regression\n\nFixes: {}", &sha1[..12]),
        ]);
        let sha3 = run(&["rev-parse", "HEAD"]);

        let lookup = build_commit_range_index(repo, &sha1, &sha3).await.unwrap();
        let CommitRangeLookup::Indexed(index) = lookup else {
            panic!("Expected CommitRangeLookup::Indexed, got {:?}", lookup);
        };

        // Untouched file with no Fixes match -> 0 candidates.
        assert_eq!(
            index.find_candidates(&["src/untouched.rs".to_string()], None),
            Some(Vec::new())
        );

        // File touched in sha2 -> [sha2].
        assert_eq!(
            index.find_candidates(&["src/reviewer.rs".to_string()], None),
            Some(vec![sha2.clone()])
        );

        // File touched in sha2 + introducing sha1 cited in sha3 -> [sha2, sha3].
        assert_eq!(
            index.find_candidates(&["src/reviewer.rs".to_string()], Some(&sha1)),
            Some(vec![sha2.clone(), sha3.clone()])
        );

        // Cross-file prefetching: even when bug only tracks src/reviewer.rs,
        // prefetch_candidate_fix_commits includes the Drop implementation in
        // src/git_ops.rs because the full commit diff fits within MAX_FULL_COMMIT_PREFETCH_BYTES.
        let prefetched = prefetch_candidate_fix_commits(
            repo,
            std::slice::from_ref(&sha2),
            &["src/reviewer.rs".to_string()],
        )
        .await;
        assert!(
            prefetched.contains("impl Drop for Worktree"),
            "Expected cross-file hunk from src/git_ops.rs in prefetched diff, got:\n{prefetched}"
        );
    }
}
