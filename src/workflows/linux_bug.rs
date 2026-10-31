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

use crate::api::BugInput;
#[cfg(feature = "server")]
use crate::api::BugOutcome;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(feature = "server")]
use serde_json::json;
use std::sync::Arc;
#[cfg(feature = "server")]
use tracing::info;
use tracing::{debug, warn};

#[cfg(feature = "server")]
use crate::ai::ToolCall;
use crate::ai::session::{LlmSession, SessionRunner, ValidationError};
#[cfg(feature = "server")]
use crate::ai::vector_search::{
    DEFAULT_SIMILARITY_THRESHOLD, DEFAULT_TOP_CANDIDATES, extract_bug_vector, find_top_candidates,
};
use crate::ai::{AiProvider, AiResponse, AiResponseFormat, AiTool};
use crate::db::{AttributedSubsystem, Bug, Severity};
#[cfg(feature = "server")]
use crate::db::{Database, NewBug};
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
                You are an expert Rust and distributed systems maintainer. Your task is to rigorously verify a candidate Sashiko defect or vulnerability against the top-of-trunk of the Sashiko repository.\n\
                Use available tools (git_read_files, git_grep, git_blame, git_log, git_show, git_diff) to inspect the mainline codebase, verify call chains, and confirm whether this defect exists.\n\n\
                CRITICAL VALIDATION FILTER: You must assess if the bug is genuine. Do not give the code the benefit of the doubt. To mark an issue as a false positive (is_false_positive=true), you must find concrete proof in the local codebase that the described conditions are impossible, unreachable, or already safely handled. If you cannot prove it is false, verify the code locations and provide your step-by-step reasoning in verification_reasoning."
            ),
            ProjectId::Gcc => format!(
                "Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references strictly on this current date.\n\n\
                You are an expert GCC compiler maintainer. Your task is to rigorously verify a candidate GCC defect against the top-of-trunk of the GCC repository.\n\
                Use available tools (git_read_files, git_grep, git_blame, git_log, git_show, git_diff) to inspect the mainline codebase, verify call chains, and confirm whether this defect exists.\n\n\
                CRITICAL VALIDATION FILTER: You must assess if the bug is genuine. Do not give the code the benefit of the doubt. To mark an issue as a false positive (is_false_positive=true), you must find concrete proof in the local codebase that the described conditions are impossible, unreachable, or already safely handled. If you cannot prove it is false, verify the code locations and provide your step-by-step reasoning in verification_reasoning."
            ),
            ProjectId::Qemu => format!(
                "Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references strictly on this current date.\n\n\
                You are an expert QEMU maintainer and system emulator engineer. Your task is to rigorously verify a candidate QEMU defect or vulnerability against the top-of-trunk of the QEMU master tree.\n\
                Use available tools (git_read_files, git_grep, git_blame, git_log, git_show, git_diff) to inspect the mainline codebase, verify call chains, and confirm whether this defect exists.\n\n\
                CRITICAL VALIDATION FILTER: You must assess if the bug is genuine. Do not give the code the benefit of the doubt. To mark an issue as a false positive (is_false_positive=true), you must find concrete proof in the local codebase that the described conditions are impossible, unreachable, or already safely handled. If you cannot prove it is false, verify the code locations and provide your step-by-step reasoning in verification_reasoning."
            ),
            ProjectId::Zephyr => format!(
                "Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references strictly on this current date.\n\n\
                You are an expert Zephyr RTOS maintainer and embedded systems engineer. Your task is to rigorously verify a candidate Zephyr defect or vulnerability against the top-of-trunk of the Zephyr main branch.\n\
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
            ProjectId::Sashiko | ProjectId::Gcc => format!(
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
1. Verify the problem against the mainline code shown above and top-of-trunk of the repository (commit `{master_sha}`). IMPORTANT: Use this exact `{master_sha}` SHA in any tool calls instead of `HEAD` or `main` to check the actual top-of-trunk.
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
            ProjectId::Qemu => format!(
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
1. Verify the problem against the mainline code shown above and top-of-trunk of the QEMU master tree (commit `{master_sha}`). IMPORTANT: Use this exact `{master_sha}` SHA in any tool calls instead of `HEAD` or `master` to check the actual top-of-trunk.
2. Scope your verification to the relevant device models and subsystem files. Do not wander across unrelated devices or files.
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
  \"relevant_code_locations\": [ {{\"file\": \"hw/net/virtio-net.c\", \"function_or_symbol\": \"function_name\", \"line\": 123}} ]
}}",
                stage_heading = BugStage::Verification.heading(),
                master_sha = self.master_sha,
                title = self.title,
                subsystem = self.subsystem,
                description = self.description,
                locations = loc_str,
                prefetch_block = prefetch_block,
            ),
            ProjectId::Zephyr => format!(
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
1. Verify the problem against the mainline code shown above and top-of-trunk of the Zephyr main branch (commit `{master_sha}`). IMPORTANT: Use this exact `{master_sha}` SHA in any tool calls instead of `HEAD` or `main` to check the actual top-of-trunk.
2. Scope your verification to the relevant drivers, subsystem files, and their Kconfig and Devicetree configuration. Do not wander across unrelated drivers or files.
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
  \"relevant_code_locations\": [ {{\"file\": \"kernel/sem.c\", \"function_or_symbol\": \"function_name\", \"line\": 123}} ]
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
            ProjectId::Gcc => format!(
                "You are an expert GCC global reviewer and technical editor. Your role is to normalize a candidate GCC defect into canonical form.\n\
                You must standardize the defect's title, describe the technical substance, and identify the verified affected source files and symbols.\n\
                The target codebase is the GCC repository at top-of-trunk commit `{master_sha}`.\n\
                Use available tools (git_read_files, git_log, git_grep) to inspect the codebase at `{master_sha}`. Specifically:\n\
                - Use git_read_files with revision: \"{master_sha}\" or git_grep to inspect source code and identify affected files and symbols.\n\
                - Use git_log with range: \"{master_sha}\" on affected files to observe the conventional component commit prefix used in GCC (e.g. 'tree-optimization:', 'c++:', 'target:', 'middle-end:', 'rtl-optimization:', 'fortran:', 'ipa:', 'libstdc++:', 'c:').",
                master_sha = self.master_sha
            ),
            ProjectId::Qemu => format!(
                "You are an expert QEMU maintainer and technical editor. Your role is to normalize a candidate QEMU defect into canonical form.\n\
                You must standardize the defect's title, describe the technical substance, and identify the verified affected source files and symbols.\n\
                The target codebase is the QEMU repository at top-of-trunk commit `{master_sha}`.\n\
                Use available tools (git_read_files, git_log, git_grep) to inspect the codebase at `{master_sha}`. Specifically:\n\
                - Use git_read_files with revision: \"{master_sha}\" or git_grep to inspect source code and identify affected files and symbols.\n\
                - Use git_log with range: \"{master_sha}\" on affected files to observe the conventional subsystem commit prefix used by QEMU maintainers (e.g. 'virtio-net:', 'hw/arm:', 'target/riscv:', 'migration:', 'qapi:', 'block:').",
                master_sha = self.master_sha
            ),
            ProjectId::Zephyr => format!(
                "You are an expert Zephyr maintainer and technical editor. Your role is to normalize a candidate Zephyr defect into canonical form.\n\
                You must standardize the defect's title, describe the technical substance, and identify the verified affected source files and symbols.\n\
                The target codebase is the Zephyr repository at top-of-trunk commit `{master_sha}`.\n\
                Use available tools (git_read_files, git_log, git_grep) to inspect the codebase at `{master_sha}`. Specifically:\n\
                - Use git_read_files with revision: \"{master_sha}\" or git_grep to inspect source code and identify affected files and symbols.\n\
                - Use git_log with range: \"{master_sha}\" on affected files to observe the conventional subsystem commit prefix used by Zephyr maintainers (e.g. 'kernel:', 'drivers: spi:', 'net: tcp:', 'Bluetooth: Host:', 'arch: arm:', 'dts:').",
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
1. Determine the conventional Sashiko module commit prefix for this defect (e.g. 'workflows', 'db', 'reviewer', 'api', 'toolbox', 'worker', 'ingestor', 'cli'). Use git_log on the affected file(s) at revision '{master_sha}' to observe the standard commit prefix used in the repository.
2. Formulate a canonical title matching Sashiko conventions: '<module_prefix>: <defect or broken invariant in function_name()>' (strict limit of under 80 characters, NO backticks, NO markdown).
   - CRITICAL: This is a bug report title describing an existing defect, NOT a patch or commit title. Do NOT use patch/fix action verbs like 'fix', 'resolve', 'prevent', 'avoid', or 'handle'. State the defect directly (e.g. 'reviewer: leaked worktree on early return in run_review()', NEVER 'reviewer: fix leaked worktree in run_review()').
3. Provide a detailed, structured canonical description:
   - Trigger / Preconditions: Specific conditions, inputs, or states required to trigger the defect.
   - Call Chain / Execution Path: Detail the complete chain of events/calls leading up to the problem.
   - Failure Mechanism: Detail the exact root cause and how the fault, state inconsistency, or resource leak occurs.
   - Impact: Consequence of the failure (e.g. stuck review, panic, dropped finding, database lock contention, leaked resource).
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
            ProjectId::Gcc => format!(
                "{stage_heading}

Candidate Bug Details:
Original Problem: {problem}
Reasoning: {reasoning}
Reported Locations:
{locations}
{hint}
Target Mainline Commit: {master_sha}

Task:
1. Determine the conventional GCC component commit prefix for this defect (e.g. 'tree-optimization', 'c++', 'target', 'middle-end', 'rtl-optimization', 'fortran', 'ipa', 'libstdc++', 'c'). Use git_log on the affected file(s) at revision '{master_sha}' to observe the standard commit prefix used in the repository.
2. Formulate a canonical title matching GCC conventions: '<component_prefix>: <defect or broken invariant in function_name()>' (strict limit of under 80 characters, NO backticks, NO markdown).
   - CRITICAL: This is a bug report title describing an existing defect, NOT a patch or commit title. Do NOT use patch/fix action verbs like 'fix', 'resolve', 'prevent', 'avoid', or 'handle'. State the defect directly.
3. Provide a detailed, structured canonical description:
   - Trigger / Preconditions: Specific conditions, inputs, or compiler flags required to trigger the defect.
   - Call Chain / Execution Path: Detail the complete chain of events/calls leading up to the problem.
   - Failure Mechanism: Detail the exact root cause and how the fault or wrong transformation occurs.
   - Impact: Consequence of the failure (e.g. wrong-code miscompilation, ICE, rejects-valid, accepts-invalid, GGC memory corruption).
4. Verify and list the affected source files and symbols in the mainline tree at commit '{master_sha}'.

Return ONLY a valid JSON object matching this schema:
{{
  \"canonical_title\": \"tree-optimization: missing reset_flow_sensitive_info in fold_stmt()\",
  \"canonical_description\": \"Trigger / Preconditions: ...\\nFailure Mechanism: ...\\nImpact: ...\",
  \"affected_source_files\": [\"gcc/tree-ssa-phiopt.cc\"],
  \"affected_symbols\": [\"fold_stmt\"]
}}",
                stage_heading = BugStage::Normalization.heading(),
                problem = self.problem,
                reasoning = self.reasoning,
                locations = self.locations,
                hint = hint_section,
                master_sha = self.master_sha
            ),
            ProjectId::Qemu => format!(
                "{stage_heading}

Candidate Bug Details:
Original Problem: {problem}
Reasoning: {reasoning}
Reported Locations:
{locations}
{hint}
Target Mainline Commit: {master_sha}

Task:
1. Determine the conventional subsystem commit prefix for this defect (e.g. 'virtio', 'virtio-net', 'hw/arm', 'target/riscv', 'migration', 'qapi', 'block'). Use git_log on the affected file(s) at revision '{master_sha}' to observe the standard commit prefix used by QEMU maintainers.
2. Formulate a canonical title matching QEMU patch conventions: '<subsystem_prefix>: <defect or broken invariant in function_name()>' (strict limit of under 80 characters, NO backticks, NO markdown).
   - CRITICAL: This is a bug report title describing an existing defect, NOT a patch or commit title. Do NOT use patch/fix action verbs like 'fix', 'resolve', 'prevent', 'avoid', or 'handle'. State the defect directly (e.g. 'virtio-net: out-of-bounds read in virtio_net_handle_rsc()', NEVER 'virtio-net: fix out-of-bounds read in virtio_net_handle_rsc()').
3. Provide a detailed, structured canonical description:
   - Trigger / Preconditions: Specific conditions, inputs, or guest-triggerable states required to trigger the defect (e.g. a specific register write, descriptor or config-space access).
   - Call Chain / Execution Path: Detail the complete chain of events/calls leading up to the problem.
   - Failure Mechanism: Detail the exact root cause and how the fault or state corruption occurs.
   - Impact: Consequence of the failure (e.g. out-of-bounds host read/write, memory leak on the unrealize path, abort, incorrect guest-visible behavior).
4. Verify and list the affected source files and symbols in the mainline tree at commit '{master_sha}'.

Return ONLY a valid JSON object matching this schema:
{{
  \"canonical_title\": \"virtio-net: out-of-bounds read in virtio_net_handle_rsc()\",
  \"canonical_description\": \"Trigger / Preconditions: ...\\nFailure Mechanism: ...\\nImpact: ...\",
  \"affected_source_files\": [\"hw/net/virtio-net.c\"],
  \"affected_symbols\": [\"virtio_net_handle_rsc\"]
}}",
                stage_heading = BugStage::Normalization.heading(),
                problem = self.problem,
                reasoning = self.reasoning,
                locations = self.locations,
                hint = hint_section,
                master_sha = self.master_sha
            ),
            ProjectId::Zephyr => format!(
                "{stage_heading}

Candidate Bug Details:
Original Problem: {problem}
Reasoning: {reasoning}
Reported Locations:
{locations}
{hint}
Target Mainline Commit: {master_sha}

Task:
1. Determine the conventional subsystem commit prefix for this defect (e.g. 'kernel', 'drivers: spi', 'net: tcp', 'Bluetooth: Host', 'arch: arm', 'dts'). Use git_log on the affected file(s) at revision '{master_sha}' to observe the standard commit prefix used by Zephyr maintainers.
2. Formulate a canonical title matching Zephyr commit conventions: '<subsystem_prefix>: <defect or broken invariant in function_name()>' (strict limit of under 80 characters, NO backticks, NO markdown).
   - CRITICAL: This is a bug report title describing an existing defect, NOT a patch or commit title. Do NOT use patch/fix action verbs like 'fix', 'resolve', 'prevent', 'avoid', or 'handle'. State the defect directly (e.g. 'net: tcp: out-of-bounds read in tcp_parse_options()', NEVER 'net: tcp: fix out-of-bounds read in tcp_parse_options()').
3. Provide a detailed, structured canonical description:
   - Trigger / Preconditions: Specific conditions, inputs, and Kconfig or Devicetree configuration required to trigger the defect (e.g. a received packet, a system call argument, or an interrupt arriving during a given operation).
   - Call Chain / Execution Path: Detail the complete chain of events/calls leading up to the problem.
   - Failure Mechanism: Detail the exact root cause and how the fault or state corruption occurs.
   - Impact: Consequence of the failure (e.g. memory corruption, a kernel panic, a deadlock, a leak on an error path, or incorrect device behavior).
4. Verify and list the affected source files and symbols in the mainline tree at commit '{master_sha}'.

Return ONLY a valid JSON object matching this schema:
{{
  \"canonical_title\": \"net: tcp: out-of-bounds read in tcp_parse_options()\",
  \"canonical_description\": \"Trigger / Preconditions: ...\\nFailure Mechanism: ...\\nImpact: ...\",
  \"affected_source_files\": [\"subsys/net/ip/tcp.c\"],
  \"affected_symbols\": [\"tcp_parse_options\"]
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
            ProjectId::Linux | ProjectId::Gcc => {
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
            // Public headers sit under include/zephyr/<subsystem>/, where two
            // components would give every header the same label.
            ProjectId::Zephyr if parts.len() >= 4 && parts[..2] == ["include", "zephyr"] => {
                format!("{}/{}/{}", parts[0], parts[1], parts[2])
            }
            ProjectId::Qemu | ProjectId::Zephyr => {
                if parts.len() >= 2 {
                    format!("{}/{}", parts[0], parts[1])
                } else if !parts.is_empty() {
                    parts[0].to_string()
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
            ProjectId::Gcc => vec!["gcc".to_string()],
            ProjectId::Qemu => vec!["qemu".to_string()],
            ProjectId::Zephyr => vec!["zephyr".to_string()],
        }
    } else {
        subs
    }
}

/// The subsystem names that describe a bug for similarity search and the
/// deduplication prompt.
///
/// Upstream grant sections are left out: they only widen who may see the bug
/// and are never part of its hydrated subsystems, so including them would
/// skew the query vector against the stored ones and show the dedup model
/// sections the bug does not belong to. A grant section that also matched
/// through MAINTAINERS is still kept, once, through that match.
#[cfg(feature = "server")]
fn similarity_subsystem_names(subsystems: &[AttributedSubsystem]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for sub in subsystems {
        if sub.source != crate::db::SubsystemSource::UpstreamGrant && !names.contains(&sub.name) {
            names.push(sub.name.clone());
        }
    }
    names
}

/// Resolves the subsystems to record against a bug, retaining where each name
/// came from.
///
/// A MAINTAINERS match is the only origin that names a real maintainer, so the
/// provenance has to travel with the name from the moment it is produced. The
/// lookup order is unchanged: an available index wins, a directory prefix is
/// the fallback, and a name the caller supplied is only used when no index can
/// be consulted at all.
#[cfg(all(test, feature = "server"))]
fn resolve_official_subsystems(
    tools: Option<&ToolBox>,
    input_subsystems: &[AttributedSubsystem],
    verified_files: &[String],
) -> Vec<AttributedSubsystem> {
    resolve_official_subsystems_for_project(
        tools.is_some(),
        None,
        input_subsystems,
        verified_files,
        ProjectId::Linux,
    )
}

fn resolve_official_subsystems_for_project(
    has_tools: bool,
    loaded_maintainers: Option<&crate::maintainers::MaintainersIndex>,
    input_subsystems: &[AttributedSubsystem],
    verified_files: &[String],
    project: ProjectId,
) -> Vec<AttributedSubsystem> {
    let from_paths = || {
        extract_directory_subsystems_for_project(verified_files, project)
            .into_iter()
            .map(AttributedSubsystem::from_path_prefix)
            .collect::<Vec<_>>()
    };
    // MAINTAINERS matches plus upstream grant sections. Without a single
    // MAINTAINERS match the directory prefixes are added for display, as
    // before; grant sections are kept either way, since they are what lets
    // an upstream maintainer see the bug at all.
    let from_index = |index: &crate::maintainers::MaintainersIndex| {
        let mut attributed = index.attribute_files(verified_files);
        if !attributed
            .iter()
            .any(|s| s.source == crate::db::SubsystemSource::MaintainersSection)
        {
            attributed.splice(0..0, from_paths());
        }
        attributed
    };

    if project.uses_maintainers() {
        if has_tools {
            if let Some(index) = loaded_maintainers {
                return from_index(index);
            }
            return match crate::maintainers::get_global_maintainers() {
                Some(index) => from_index(&index),
                None => from_paths(),
            };
        }

        // Without a repository toolbox (`has_tools == false`), `verified_files`
        // have not been checked against the tree, so caller-supplied subsystem
        // names must stay unpromoted (`CallerSupplied`) even when a MAINTAINERS
        // index is loaded in process memory.
        if !input_subsystems.is_empty() {
            return input_subsystems.to_vec();
        }

        if let Some(index) = loaded_maintainers {
            return from_index(index);
        }

        return match crate::maintainers::get_global_maintainers() {
            Some(index) => from_index(&index),
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

#[cfg(feature = "server")]
const MAX_DEDUP_DESCRIPTION_BYTES: usize = 800;

#[cfg(feature = "server")]
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

#[cfg(feature = "server")]
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

#[cfg(feature = "server")]
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

#[cfg(feature = "server")]
struct DedupSession<'a> {
    project: ProjectId,
    candidate_problem: &'a str,
    candidate_description: &'a str,
    candidate_locations: Option<&'a Value>,
    candidate_subsystems: &'a [String],
    known_candidates: &'a [Bug],
    context_tag: Option<String>,
}

#[cfg(feature = "server")]
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
            ProjectId::Sashiko => "You are an expert Rust and distributed systems maintainer responsible for defect tracking and deduplication.\n\
        You will compare a newly verified Sashiko bug against a list of known Sashiko bugs in the codebase.\n\
        Determine if the newly verified bug is an identical duplicate (describing the same root cause in the same code path/function) of one of the candidate bugs.\n\
        IMPORTANT: Bugs that have the same root cause but different consequences should be considered a duplicate and be merged. Rule of thumb: if fixing one issue will resolve the other issue, it's the same bug.\n\
        Output raw JSON only."
                .to_string(),
            ProjectId::Gcc => "You are an expert GCC compiler maintainer responsible for defect tracking and deduplication.\n\
        You will compare a newly verified GCC bug against a list of known GCC bugs in the codebase.\n\
        Determine if the newly verified bug is an identical duplicate (describing the same root cause in the same code path/function) of one of the candidate bugs.\n\
        IMPORTANT: Bugs that have the same root cause but different consequences should be considered a duplicate and be merged. Rule of thumb: if fixing one issue will resolve the other issue, it's the same bug.\n\
        Output raw JSON only."
                .to_string(),
            ProjectId::Qemu => "You are an expert QEMU maintainer responsible for defect tracking and deduplication.\n\
        You will compare a newly verified QEMU bug against a list of known QEMU bugs in the codebase.\n\
        Determine if the newly verified bug is an identical duplicate (describing the same root cause in the same code path/function) of one of the candidate bugs.\n\
        IMPORTANT: Bugs that have the same root cause but different consequences should be considered a duplicate and be merged. Rule of thumb: if fixing one issue will resolve the other issue, it's the same bug.\n\
        Output raw JSON only."
                .to_string(),
            ProjectId::Zephyr => "You are an expert Zephyr maintainer responsible for defect tracking and deduplication.\n\
        You will compare a newly verified Zephyr bug against a list of known Zephyr bugs in the codebase.\n\
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
            ProjectId::Gcc => "Newly Verified GCC Bug:",
            ProjectId::Qemu => "Newly Verified QEMU Bug:",
            ProjectId::Zephyr => "Newly Verified Zephyr Bug:",
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
            ProjectId::Gcc => "You are an expert GCC compiler maintainer. Your task is to determine the exact commit that introduced a verified GCC defect.\n\
        Use available tools (git_blame, git_log, git_diff, git_show, git_read_files) to inspect history backwards and confirm which commit actually introduced the buggy logic rather than just refactoring lines.\n\
        EFFICIENCY LIMIT REQUIREMENT: You have a strict limit on tool calls; be extremely efficient instead of wandering the history.".to_string(),
            ProjectId::Qemu => "You are an expert QEMU maintainer. Your task is to determine the exact commit that introduced a verified QEMU defect.\n\
        Use available tools (git_blame, git_log, git_diff, git_show, git_read_files) to inspect history backwards and confirm which commit actually introduced the buggy logic rather than just refactoring lines.\n\
        EFFICIENCY LIMIT REQUIREMENT: You have a strict limit on tool calls; be extremely efficient instead of wandering the history.".to_string(),
            ProjectId::Zephyr => "You are an expert Zephyr maintainer. Your task is to determine the exact commit that introduced a verified Zephyr defect.\n\
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
        let mut parsed: TracingJson = crate::workflow::output::parse_json_from_text(text)
            .map_err(|e| ValidationError::FormatViolation(e.to_string()))?;

        if let Some(ref mut sha) = parsed.introducing_commit_sha {
            *sha = sha.trim().to_string();
        }
        if parsed
            .introducing_commit_sha
            .as_deref()
            .is_some_and(str::is_empty)
        {
            parsed.introducing_commit_sha = None;
        }

        if let Some(ref sha) = parsed.introducing_commit_sha {
            if !(7..=40).contains(&sha.len()) || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(ValidationError::FormatViolation(format!(
                    "The value '{}' provided for introducing_commit_sha must be a 7 to 40 character hexadecimal commit SHA (or null).",
                    sha
                )));
            }
            if let Some(tb) = &self.tools {
                let output = crate::git_cmd::in_dir(tb.get_worktree_path())
                    .args([
                        "-c",
                        "safe.bareRepository=all",
                        "cat-file",
                        "-e",
                        "--end-of-options",
                        &format!("{}^{{commit}}", sha),
                    ])
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
    verification_reasoning: &'a str,
    locations: &'a str,
    prefetched_context: &'a str,
    master_sha: &'a str,
    kernel_threat_model: Option<&'a crate::threat_model::KernelThreatModel>,
    tools: Option<Arc<ToolBox>>,
    context_tag: Option<String>,
}

#[async_trait]
impl LlmSession for SeveritySession<'_> {
    type Output = SeverityJson;

    fn system_prompt(&self) -> String {
        let target_name = match self.project {
            ProjectId::Linux => "the Linux kernel",
            ProjectId::Sashiko => "Sashiko",
            ProjectId::Gcc => "GCC",
            ProjectId::Qemu => "QEMU",
            ProjectId::Zephyr => "Zephyr",
        };
        let threat_model_section = if self.project == ProjectId::Linux {
            let rendered = crate::threat_model::KernelThreatModel::render_for_severity(
                self.kernel_threat_model,
            );
            if rendered.is_empty() {
                String::new()
            } else {
                format!("\n\n{rendered}")
            }
        } else {
            String::new()
        };
        format!(
            "{}{threat_model_section}\n\nAssess the severity and impact of a verified defect in {} following the severity definitions and calibration guidance above.\n\
            Output raw JSON only matching the schema.",
            crate::prompt_bundle::severity_guide(self.project),
            target_name
        )
    }

    fn initial_user_prompt(&self) -> String {
        let defect_header = match self.project {
            ProjectId::Linux => "Verified Linux Kernel Defect:",
            ProjectId::Sashiko => "Verified Sashiko Defect:",
            ProjectId::Gcc => "Verified GCC Defect:",
            ProjectId::Qemu => "Verified QEMU Defect:",
            ProjectId::Zephyr => "Verified Zephyr Defect:",
        };
        let verification_block = if self.verification_reasoning.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\nVerification Proof:\n{}",
                self.verification_reasoning.trim()
            )
        };
        let code_block = if self.prefetched_context.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\nRelevant Code Context (at commit {}):\n{}",
                self.master_sha,
                self.prefetched_context.trim()
            )
        };
        let tool_hint = match (self.project, self.tools.is_some()) {
            (ProjectId::Linux, true) => format!(
                "\nIf caller privileges, file permissions (e.g. debugfs/sysfs modes), capability checks (e.g. capable(CAP_SYS_ADMIN)), or Kconfig dependencies are not already clear from the context above, inspect them at commit `{}` with git_read_files or git_grep before assigning High or Critical.\n",
                self.master_sha
            ),
            (_, true) => format!(
                "\nIf caller reachability or preconditions are not clear from the context above, inspect them at commit `{}` with git_read_files or git_grep before assigning High or Critical.\n",
                self.master_sha
            ),
            (_, false) => String::new(),
        };
        format!(
            "{stage_heading}

{defect_header}
Title: {title}
Description:
{description}{verification_block}
Code Locations:
{locations}{code_block}

Task:
Assess the severity of this defect and provide an explanation following the severity levels and calibration guidance above.{tool_hint}
State your reasoning (consequence and blast radius, actor/privilege and trust boundary, and trigger preconditions/likelihood) at the start of severity_explanation so the label is auditable.

Return ONLY a valid JSON object matching:
{{
  \"severity\": \"Low\" | \"Medium\" | \"High\" | \"Critical\" | \"Unknown\",
  \"severity_explanation\": \"Explain consequence, actor/required privileges, trust boundary, triggering path, and likelihood...\"
}}",
            stage_heading = BugStage::SeverityAssessment.heading(),
            defect_header = defect_header,
            title = self.canonical_title,
            description = self.canonical_description,
            verification_block = verification_block,
            locations = self.locations,
            code_block = code_block,
            tool_hint = tool_hint,
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
        let parsed: SeverityJson = crate::workflow::output::parse_json_from_text(text)
            .map_err(|e| ValidationError::FormatViolation(e.to_string()))?;
        let _ = Severity::from_str(&parsed.severity);
        Ok(parsed)
    }
}

// ---------------------------------------------------------------------------
// 6. Standalone Description Generation Session (Enrichment)
// ---------------------------------------------------------------------------

/// Writing rules shared by every project's report prompt. They adapt the
/// governing rule of the Linux inline review template to a standalone bug
/// report: every line either explains the bug or shows why it is worth
/// fixing, and length follows how hard the bug is to explain.
#[cfg(feature = "server")]
const REPORT_WRITING_RULES: &str = r#"# The governing rule

Every line and every word you write must do one of two jobs:

- help the reader understand the problem: what is wrong, where, what
  triggers it, and what happens, or
- show why it is worth fixing: the consequence that the verified analysis
  establishes.

Delete anything that does neither: restating the inputs, proof steps the
reader does not need, generic remarks about a class of bug, alternative
paths that add no new insight, and closing summaries.

Earlier stages have already verified and rated this bug.  Do not re-judge
it here: do not downplay it, upgrade it, or question whether it matters.
Convey what the analysis established, clearly.

The reader is a maintainer who knows this code better than you do.
Following the rule above, an obvious bug gets two or three lines, and a
subtle race gets a diagram and a couple of paragraphs.  Length follows
from how hard the bug is to explain, never from its severity or from how
much context you were given.

# Writing the report

- The first sentence says what is wrong, in which function, and what
  triggers it.  When the bug only shows up under a narrowing condition
  (an architecture, a configuration option, specific hardware), lead with
  it: "On 32-bit architectures, ...".
- Add a sentence only if the reader would otherwise have to work
  something out: a trigger that is not obvious, or a consequence that
  does not follow from the first sentence.
- Give one trigger, the most likely or most convincing one, not a list of
  every input that hits the bug.
- Give the consequence that matters most, not every object or code path
  affected.
- Write statements, not questions.  This is a report on existing code,
  not a reply to a patch author.
- Describe the defect only.  Do not suggest a fix, recommend remediation
  or write a patch; maintainers decide how to resolve it.
- Start directly with the technical description: no greeting, no
  preamble ("While reviewing ...", "I noticed ..."), no header ("Defect
  Report:", "Description:") and no restated title.

# Distill, do not transcribe

The description, verification details and severity explanation you are
given are working notes, not text for the report.  Take from them only
what the reader needs.  They often list many triggers, affected objects
and code paths; pick one of each and drop the rest.  Never copy their
"Trigger:", "Failure Mechanism:", "Impact:" or step-by-step proof
structure into the report.

The title, severity and introducing commit are displayed next to the
report.  Do not restate them.

# Evidence

## Code snippets

Quote code only when it shows something prose cannot say as briefly.
Never quote code to show a fact that fits in one sentence, such as that a
call is absent or that an object lives on the stack.  Never quote a whole
function when two lines carry the point.  A snippet supports the prose:
state the consequence in words, and never end the report on a snippet
left to speak for itself.

When a snippet is needed, label it with the file and function, copy only
the lines that matter verbatim (including their indentation), and elide
the rest with "...":

path/to/file.c:func_name() {
    ...
	the_line_that_matters();
    ...
}

Carets (^^^^) are rarely needed.  Use them only under an existing
defective token or operator, never to point at a missing call.

## Scenario diagrams

Show every race, deadlock or reentrancy bug where the order of events
across contexts matters as a scenario diagram.  Prose alone ("Meanwhile,
... while concurrently ...") is hard to follow.

- Time flows downward, one step per line.
- Each CPU, thread or task gets its own column, labelled with the actual
  context (CPU0, CPU1, kworker, <hardirq>, or a task name).
- When everything happens in one context (an interrupt or a reentrant
  call nesting into the same code), use a single column with no label and
  mark where the nesting happens.
- Keep only the steps a reader needs: the entry point of each context,
  the key steps (lock, publish, clear, free) and the bad access.
  Collapse intermediate callers with "...".
- One diagram shows one concrete interleaving.  Describe other outcomes
  in the prose.
- Mark the failing step with a short arrow note such as
  "<-- use-after-free".  Explanations go in the prose below the diagram.
- Separate columns with whitespace only: no table borders, pipes or plus
  signs.
- Every line fits in 75 columns.  Elide arguments with "...".

Skip the diagram when one sentence says the same: a hang where one side
waits for a stuck peer, a symmetric race where every context runs the
same code ("two concurrent callers both reach X"), or a single-context
problem where a call chain (func_a() -> func_b()) is enough.

# Paragraphs and wording

- Short sentences, one idea each.  Paragraphs of at most four lines, with
  a blank line between them.
- Use the established terms of the project named above.  Avoid "can lead
  to a potential situation in which".
- One "can" or "could" is enough; state the explanation as facts and do
  not stack hedges.
- Refer to each code element by one name and keep using it.
- No generic security hyperbole ("may allow an attacker to ...").

# Format

- Plain text, fit for a mailing list: no markdown, no code fences, no
  "> " quote marks and no backticks.  Name functions as func().
- Wrap your own text at 75 columns.  Never wrap quoted code.
- Never use ALL CAPS, except in quoted code and diagram labels.
- Never mention line numbers.  They go stale as soon as the tree moves.
  Use function names, call chains and short snippets instead.
- Cite a commit as its 12 character short sha followed by the subject in
  parentheses and double quotes: 1a2b3c4d5e6f ("foo: fix bar handling").
  Never write a bare sha, and never guess a subject.
- End the report after the last sentence that does one of the two jobs."#;

/// The parts of the report prompt that differ between projects.
#[cfg(feature = "server")]
struct ReportPromptParts {
    /// Who writes the report, and for which codebase.
    role: &'static str,
    /// What the reader already knows, and the project's established terms.
    audience: &'static str,
    /// Made-up sample reports. They are invented so that they can never
    /// match a benchmark expectation.
    examples: &'static str,
}

#[cfg(feature = "server")]
fn report_prompt_parts(project: ProjectId) -> ReportPromptParts {
    match project {
        ProjectId::Linux => ReportPromptParts {
            role: "You are an experienced Linux kernel maintainer writing a standalone report of a verified defect in the current kernel tree, fit for the Linux kernel mailing list (LKML).",
            audience: "Do not explain basic kernel mechanisms such as RCU, spinlocks, workqueues, slab caches or refcounts. Established terms: use-after-free, double free, leak, ABBA deadlock, NULL pointer dereference, data race, sleeping in atomic context.",
            examples: LINUX_REPORT_EXAMPLES,
        },
        ProjectId::Sashiko => ReportPromptParts {
            role: "You are an experienced Sashiko maintainer writing a standalone report of a verified defect in the Sashiko Rust codebase.",
            audience: "Do not explain generic Rust, Tokio or SQLite concepts. Established terms: leak, deadlock, blocking call in async context, SQLITE_BUSY stall, panic, lost update, data race.",
            examples: SASHIKO_REPORT_EXAMPLES,
        },
        ProjectId::Gcc => ReportPromptParts {
            role: "You are an experienced GCC maintainer writing a standalone report of a verified defect in the GCC codebase.",
            audience: "Do not explain generic compiler concepts such as GIMPLE, RTL, SSA form or the pass manager. Established terms: wrong-code, ICE, rejects-valid, accepts-invalid, GGC memory corruption, missed optimization.",
            examples: GCC_REPORT_EXAMPLES,
        },
        ProjectId::Qemu => ReportPromptParts {
            role: "You are an experienced QEMU maintainer writing a standalone report of a verified defect in the QEMU codebase.",
            audience: "Do not explain generic QEMU mechanics such as the QOM property system, MemoryRegions, bottom halves or iothreads. When the guest can trigger the bug, say how. Established terms: out-of-bounds read or write, use-after-free, leak on the unrealize path, assertion failure, abort, incorrect guest-visible behavior.",
            examples: QEMU_REPORT_EXAMPLES,
        },
        ProjectId::Zephyr => ReportPromptParts {
            role: "You are an experienced Zephyr maintainer writing a standalone report of a verified defect in the Zephyr RTOS codebase.",
            audience: "Do not explain generic RTOS, Kconfig or Devicetree concepts. Name the execution context and configuration that trigger the defect. Established terms: ISR, thread, workqueue, spinlock, kernel object, runtime PM, DMA buffer.",
            examples: "foo_submit() leaks its buffer if k_msgq_put() times out: the failure path\nreturns without calling k_free(). Repeated full-queue submissions exhaust\nthe heap.",
        },
    }
}

#[cfg(feature = "server")]
const LINUX_REPORT_EXAMPLES: &str = r#"## An obvious bug, prose only

foo_probe() leaks the DMA buffer when foo_hw_init() fails: the error path
returns without calling dma_free_coherent().  Every deferred probe retry
leaks another buffer.

## An ordering bug, where a snippet is shorter than prose

baz_get_stats() dereferences priv->ring before the NULL check that guards
it:

drivers/net/ethernet/baz/baz_main.c:baz_get_stats() {
	struct baz_priv *priv = netdev_priv(dev);
	u64 drops = priv->ring->drops;
    ...
	if (!priv->ring)
		return;
    ...
}

priv->ring stays NULL until baz_open() runs, so reading the statistics of
an interface that was never brought up causes a NULL pointer dereference.

## A race, with a scenario diagram

qux_remove() can free the device while its interrupt handler still queues
work that uses it:

CPU0                              CPU1
qux_remove()
  cancel_work_sync(&qux->work)
                                  qux_irq()
                                    schedule_work(&qux->work)
  kfree(qux)
                                  qux_work_fn()
                                    qux->state = 0;  <-- use-after-free

qux_remove() cancels the work before free_irq(), so the handler can queue
it again in between.  Unbinding the device under interrupt load is
enough."#;

#[cfg(feature = "server")]
const SASHIKO_REPORT_EXAMPLES: &str = r#"## An obvious bug, prose only

run_review() leaks the review worktree when apply_series() fails: the
early return skips remove_worktree().  Every series that does not apply
leaves a full checkout behind until the disk fills.

## A symmetric race, where a sentence replaces the diagram

claim_next_patch() selects the next pending patch and marks it running in
two separate statements outside a transaction.  Two workers can select
the same row before either update lands, so the patch is reviewed twice
and its findings are posted twice."#;

#[cfg(feature = "server")]
const GCC_REPORT_EXAMPLES: &str = r#"## A wrong-code bug, prose only

fold_shift_pair() folds (x << c) >> c to x for unsigned x, although the
left shift drops the top c bits.  With x = 0x80000001 and c = 1 the folded
code yields 0x80000001 instead of 1 at -O1 and above.

## An ICE, prose only

lower_foo_builtin() assumes the second argument of __builtin_foo() is an
INTEGER_CST and calls tree_to_uhwi() on it.  A non-constant argument hits
the assertion in tree_to_uhwi() and causes an ICE instead of a
diagnostic."#;

#[cfg(feature = "server")]
const QEMU_REPORT_EXAMPLES: &str = r#"## A guest-triggerable bug, prose only

s->regs[] in virt_foo has 16 entries, but virt_foo_mmio_read() indexes it
with addr >> 2 over the whole 0x100 byte MMIO region.  A guest read above
offset 0x3c returns host heap memory.

## A leak, prose only

foo_realize() allocates s->fifo, but foo_unrealize() never frees it, so
every device_del of the device leaks the FIFO."#;

/// Builds the report stage's system prompt for `project` from the shared
/// writing rules and the project's own parts.
#[cfg(feature = "server")]
fn report_system_prompt(project: ProjectId) -> String {
    let parts = report_prompt_parts(project);
    format!(
        "{role}\n\n{audience}\n\n{rules}\n\n# Examples\n\nThe sample reports below are made up.  Match their length and \
         style, not their content.\n\n{examples}\n",
        role = parts.role,
        audience = parts.audience,
        rules = REPORT_WRITING_RULES,
        examples = parts.examples,
    )
}

#[cfg(feature = "server")]
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

#[cfg(feature = "server")]
#[async_trait]
impl LlmSession for ReportSession<'_> {
    type Output = String;

    fn system_prompt(&self) -> String {
        report_system_prompt(self.project)
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
            ProjectId::Gcc => "GCC Defect Details:",
            ProjectId::Qemu => "QEMU Defect Details:",
            ProjectId::Zephyr => "Zephyr Defect Details:",
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
Write the report for this bug following the system prompt. Distill the description and verification details instead of transcribing them, never mention line numbers from the locations or code context, and keep the report as short as the bug allows.",
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

/// The audit tool that records an upstream fix check of `project`'s bugs.
pub fn fix_check_tool(project: ProjectId) -> &'static str {
    match project {
        ProjectId::Linux => "sashiko:linux_bug:fix_check",
        ProjectId::Sashiko => "sashiko:sashiko_bug:fix_check",
        ProjectId::Gcc => "sashiko:gcc_bug:fix_check",
        ProjectId::Qemu => "sashiko:qemu_bug:fix_check",
        ProjectId::Zephyr => "sashiko:zephyr_bug:fix_check",
    }
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
    } else if default_project == ProjectId::Gcc
        || bugid.is_some_and(|id| id.starts_with("gcc-"))
        || tool.is_some_and(|t| t.starts_with("sashiko:gcc"))
    {
        ProjectId::Gcc
    } else if default_project == ProjectId::Qemu
        || bugid.is_some_and(|id| id.starts_with("qemu-"))
        || tool.is_some_and(|t| t.starts_with("sashiko:qemu"))
    {
        ProjectId::Qemu
    } else if default_project == ProjectId::Zephyr
        || bugid.is_some_and(|id| id.starts_with("zephyr-"))
        || tool.is_some_and(|t| t.starts_with("sashiko:zephyr"))
    {
        ProjectId::Zephyr
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
#[cfg(feature = "server")]
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

#[cfg(feature = "server")]
fn build_candidate_bug_and_enrichment(
    provider: &dyn AiProvider,
    db: &Database,
    input: &BugInput,
    project: ProjectId,
) -> (Database, NewBug, crate::db::NewBugEnrichment) {
    let default_tool = match project {
        ProjectId::Linux => "sashiko:linux_patch_review",
        ProjectId::Sashiko => "sashiko:sashiko_patch_review",
        ProjectId::Gcc => "sashiko:gcc_patch_review",
        ProjectId::Qemu => "sashiko:qemu_patch_review",
        ProjectId::Zephyr => "sashiko:zephyr_patch_review",
    };
    let effective_db = if db.has_bug_actor() && db.bug_model().is_some() {
        db.with_bug_actor(
            db.bug_actor(),
            db.bug_tool(),
            db.bug_model().map(str::to_string),
        )
    } else {
        let (actor, tool) = if db.has_bug_actor() {
            (db.bug_actor().to_string(), db.bug_tool().to_string())
        } else {
            ("sashiko".to_string(), default_tool.to_string())
        };
        db.with_bug_actor(&actor, &tool, Some(provider.get_capabilities().model_name))
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
        bugid,
        title: input.problem.clone(),
        lifecycle_status: crate::db::BugLifecycleStatus::New,
        pipeline_state: crate::db::BugPipelineState::Pending,
        assignee: None,
        reporter: effective_db.bug_actor().to_string(),
        reported_at: now,
        discovered_in_patchset_id: input.patchset_id,
        discovered_in_patch_id: input.patch_id,
        discovered_in_commit: input.commit_sha.clone(),
        source_ref: input.commit_sha.clone(),
        vector_json: None,
        duplicate_of_id: None,
        subsystems: initial_subsystems,
    };
    let enrichment = crate::db::NewBugEnrichment {
        kind: "candidate".to_string(),
        tool: effective_db.bug_tool().to_string(),
        model: effective_db.bug_model().map(str::to_string),
        author: Some(effective_db.bug_actor().to_string()),
        created_at: now,
        content: Some(input.reasoning.clone()),
        data_json: serde_json::to_value(input).ok(),
        ..Default::default()
    };
    (effective_db, new_bug, enrichment)
}

/// Executes the standalone bug pipeline for a single candidate concern in `project`.
#[cfg(feature = "server")]
pub async fn process_issue_for_project(
    provider: &dyn AiProvider,
    _tools: Option<Arc<ToolBox>>,
    db: &Database,
    input: BugInput,
    _context_tag: Option<&str>,
    project: ProjectId,
) -> Result<BugOutcome> {
    debug!(
        "Queueing candidate {} issue: '{}' in subsystems '{:?}'",
        project.display_name(),
        input.problem,
        input.subsystems
    );
    let (effective_db, new_bug, enrichment) =
        build_candidate_bug_and_enrichment(provider, db, &input, project);
    let bug = effective_db
        .create_bug_with_enrichment_returning(&new_bug, Some(&enrichment))
        .await?;
    debug!("Queued raw bug {} for asynchronous processing", bug.bugid);
    Ok(BugOutcome::NewlyDiscovered { bug })
}

/// Atomically inserts and claims a candidate bug in `project` so that a
/// synchronous caller (`sashiko scan --submit-bugs`) owns the row before any
/// concurrent `BugWorker` can claim it.
#[cfg(feature = "server")]
pub async fn create_claimed_issue_for_project(
    provider: &dyn AiProvider,
    db: &Database,
    input: BugInput,
    project: ProjectId,
    worker_id: &str,
    lease_ttl_seconds: i64,
) -> Result<Bug> {
    debug!(
        "Creating and claiming candidate {} issue: '{}' in subsystems '{:?}'",
        project.display_name(),
        input.problem,
        input.subsystems
    );
    let (effective_db, new_bug, enrichment) =
        build_candidate_bug_and_enrichment(provider, db, &input, project);
    effective_db
        .create_claimed_bug_with_enrichment(
            &new_bug,
            Some(&enrichment),
            worker_id,
            lease_ttl_seconds,
        )
        .await
}

/// Serializes deduplication checks and canonical bug commits across concurrent
/// `BugWorker` Tokio tasks running inside the single `sashiko` server daemon.
#[cfg(feature = "server")]
static BUG_DEDUP_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(feature = "server")]
const BUG_DEDUP_LEASE_TTL_SECONDS: i64 = 60;

#[cfg(feature = "server")]
const BUG_DEDUP_LEASE_RENEW_INTERVAL_SECONDS: u64 = 20;

#[cfg(feature = "server")]
async fn maintain_bug_dedup_lease(db: &Database, owner: &str) {
    let mut deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs((BUG_DEDUP_LEASE_TTL_SECONDS - 1) as u64);
    loop {
        let renewal = tokio::time::timeout_at(deadline, async {
            tokio::time::sleep(std::time::Duration::from_secs(
                BUG_DEDUP_LEASE_RENEW_INTERVAL_SECONDS,
            ))
            .await;
            let started = tokio::time::Instant::now();
            (
                started,
                db.renew_bug_dedup_lease(owner, BUG_DEDUP_LEASE_TTL_SECONDS)
                    .await,
            )
        })
        .await;
        match renewal {
            Ok((started, Ok(true))) => {
                deadline = started
                    + std::time::Duration::from_secs((BUG_DEDUP_LEASE_TTL_SECONDS - 1) as u64);
            }
            Ok((_, Ok(false))) | Err(_) => return,
            Ok((_, Err(e))) => {
                warn!("Failed to renew bug_dedup_lease for {}: {}", owner, e);
            }
        }
    }
}

#[cfg(feature = "server")]
pub(crate) fn bug_dedup_lease_owner(bug_id: i64, claim_id: Option<&str>) -> String {
    match claim_id {
        Some(claim_id) => format!(
            "bug:{}:pid:{}:claim:{}",
            bug_id,
            std::process::id(),
            claim_id
        ),
        None => format!(
            "bug:{}:pid:{}:attempt:{}",
            bug_id,
            std::process::id(),
            uuid::Uuid::new_v4()
        ),
    }
}

#[cfg(feature = "server")]
async fn run_with_bug_dedup_lease<T>(
    db: &Database,
    owner: &str,
    critical_section: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let _dedup_guard = BUG_DEDUP_LOCK.lock().await;
    db.acquire_bug_dedup_lease(owner, BUG_DEDUP_LEASE_TTL_SECONDS)
        .await?;
    let outcome = tokio::select! {
        res = critical_section => Some(res),
        () = maintain_bug_dedup_lease(db, owner) => None,
    };
    let _ = db.release_bug_dedup_lease(owner).await;
    match outcome {
        Some(res) => res,
        None => bail!("Lost cross-process bug_dedup_lease for {owner} while deduplicating"),
    }
}

async fn get_mainline_sha(tools: Option<&Arc<ToolBox>>, project: ProjectId) -> String {
    if let Some(tools) = tools {
        resolve_mainline_sha(tools.get_worktree_path(), project)
            .await
            .unwrap_or_else(|| project.mainline_branch().to_string())
    } else {
        project.mainline_branch().to_string()
    }
}

async fn format_commit(tools: Option<&Arc<ToolBox>>, sha: Option<String>) -> Option<String> {
    let sha = sha?.trim().to_string();
    if sha.is_empty() {
        return None;
    }
    if sha.contains(" (\"") {
        return Some(sha);
    }
    if !(7..=40).contains(&sha.len()) || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let tb = match tools {
        Some(t) => t.clone(),
        None => return Some(sha),
    };
    tokio::task::spawn_blocking(move || {
        let worktree = tb.get_worktree_path();

        let subject = crate::git_cmd::in_dir(worktree)
            .args([
                "-c",
                "safe.bareRepository=all",
                "log",
                "-1",
                "--format=%s",
                "--end-of-options",
                &sha,
            ])
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
            .args([
                "-c",
                "safe.bareRepository=all",
                "describe",
                "--contains",
                "--end-of-options",
                &sha,
            ])
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
    let target_sha = target_sha.trim();
    if target_sha.is_empty() || target_sha.starts_with('-') {
        return None;
    }
    let tb = tools?;
    let loc_arr = locations.as_ref()?.as_array()?;
    let worktree = tb.get_worktree_path();

    for loc in loc_arr {
        let Some(file) = loc
            .get("file")
            .and_then(|v| v.as_str())
            .and_then(crate::workflows::linux_scan::sanitize_repo_relative_path)
        else {
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
                    debug!(
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

fn is_bug_prefetch_source_file(file: &str) -> bool {
    const ALLOWED_EXTS: &[&str] = &[
        ".c", ".h", ".rs", ".cc", ".cpp", ".cxx", ".C", ".hh", ".hpp", ".tcc", ".inc", ".def",
        ".pd", ".f90", ".F90",
    ];
    ALLOWED_EXTS.iter().any(|ext| file.ends_with(ext))
        || (file.starts_with("gcc/") && file.ends_with(".md"))
}

fn is_c_family_source_file(file: &str) -> bool {
    const C_FAMILY_EXTS: &[&str] = &[".c", ".h", ".cc", ".cpp", ".cxx", ".C", ".hh", ".hpp"];
    C_FAMILY_EXTS.iter().any(|ext| file.ends_with(ext))
}

/// Deterministically pre-fetches code snippets around candidate bug locations from mainline.
///
/// Strictly bounded by guardrails to prevent context window bloat from malformed or
/// adversarial candidate reports:
/// 1. At most 3 distinct source files.
/// 2. At most 2 snippets per file, 5 snippets total.
/// 3. At most 100 lines per snippet (enclosing function via Tree-sitter or clamped window).
/// 4. Total character budget <= 20,000 characters (~5,000 tokens).
pub async fn prefetch_bug_locations(
    tools: Option<&Arc<ToolBox>>,
    master_sha: &str,
    locations: &Option<Value>,
) -> String {
    let master_sha = master_sha.trim();
    if master_sha.is_empty() || master_sha.starts_with('-') {
        return String::new();
    }
    let Some(tools) = tools else {
        return String::new();
    };
    let Some(loc_arr) = locations.as_ref().and_then(|v| v.as_array()) else {
        return String::new();
    };
    if loc_arr.is_empty() {
        return String::new();
    }

    // Step 1: Collect and sanitize candidate file paths (max 3 distinct valid source files)
    let mut files: Vec<String> = Vec::new();
    for loc in loc_arr {
        let Some(file) = loc
            .get("file")
            .and_then(|v| v.as_str())
            .and_then(crate::workflows::linux_scan::sanitize_repo_relative_path)
        else {
            continue;
        };
        if !is_bug_prefetch_source_file(file) {
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
                .args([
                    "-c",
                    "safe.bareRepository=all",
                    "show",
                    "--end-of-options",
                    &format!("{}:{}", master_sha_owned, file),
                ])
                .output();

            let (out, actual_file) = match git_output {
                Ok(o) if o.status.success() => (o, file.clone()),
                _ => {
                    // Try tracing rename forwards first
                    let last_commit = crate::git_cmd::in_dir(&worktree)
                        .args([
                            "-c",
                            "safe.bareRepository=all",
                            "log",
                            "-n",
                            "1",
                            "--format=%H",
                            "--",
                            &file,
                        ])
                        .output();
                    let mut resolved = None;
                    if let Ok(lc) = last_commit {
                        let sha = String::from_utf8_lossy(&lc.stdout).trim().to_string();
                        if !sha.is_empty() && !sha.starts_with('-') {
                            let diff_out = crate::git_cmd::in_dir(&worktree)
                                .args([
                                    "-c",
                                    "safe.bareRepository=all",
                                    "show",
                                    "-M",
                                    "--name-status",
                                    "--format=",
                                    "--end-of-options",
                                    &sha,
                                ])
                                .output();
                            if let Ok(do_out) = diff_out {
                                let diff_str = String::from_utf8_lossy(&do_out.stdout);
                                for line in diff_str.lines() {
                                    let parts: Vec<&str> = line.split('\t').collect();
                                    if parts.len() >= 3
                                        && parts[0].starts_with('R')
                                        && parts[1] == file
                                        && let Some(next_path) =
                                            crate::workflows::linux_scan::sanitize_repo_relative_path(
                                                parts[2],
                                            )
                                    {
                                        let try_out = crate::git_cmd::in_dir(&worktree)
                                            .args([
                                                "-c",
                                                "safe.bareRepository=all",
                                                "show",
                                                "--end-of-options",
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
                            .args([
                                "-c",
                                "safe.bareRepository=all",
                                "log",
                                "--follow",
                                "--name-only",
                                "--format=format:",
                                "--",
                                &file,
                            ])
                            .output();
                        if let Some(ro) = rename_cmd.ok().filter(|r| r.status.success()) {
                            let stdout = String::from_utf8_lossy(&ro.stdout);
                            for line in stdout.lines() {
                                let Some(trimmed) =
                                    crate::workflows::linux_scan::sanitize_repo_relative_path(line)
                                else {
                                    continue;
                                };
                                if trimmed == file {
                                    continue;
                                }
                                let try_out = crate::git_cmd::in_dir(&worktree)
                                    .args([
                                        "-c",
                                        "safe.bareRepository=all",
                                        "show",
                                        "--end-of-options",
                                        &format!("{}:{}", master_sha_owned, trimmed),
                                    ])
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
            let is_c_file = is_c_family_source_file(&actual_file);

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

/// Verified bug details produced by [`run_standalone_bug_pipeline_for_project`]
/// when running without a database (e.g., `sashiko scan` without `--submit-bugs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StandaloneVerifiedBug {
    pub canonical_title: String,
    pub canonical_description: String,
    pub severity: Severity,
    pub severity_explanation: String,
    pub subsystems: Vec<AttributedSubsystem>,
    pub source_files: Vec<String>,
    pub locations: Option<Value>,
    pub introduced_in_commit: Option<String>,
    pub verified_on_sha: String,
    #[serde(default)]
    pub tokens_in: usize,
    #[serde(default)]
    pub tokens_out: usize,
    #[serde(default)]
    pub tokens_cached: usize,
}

/// Outcome of running [`run_standalone_bug_pipeline_for_project`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StandaloneBugOutcome {
    Discarded {
        reason: String,
        #[serde(default)]
        tokens_in: usize,
        #[serde(default)]
        tokens_out: usize,
        #[serde(default)]
        tokens_cached: usize,
    },
    Verified(Box<StandaloneVerifiedBug>),
}

/// Error returned when [`run_standalone_bug_pipeline_for_project`] fails,
/// preserving token usage accumulated across completed stages and the failing
/// [`crate::ai::session::SessionFailure`].
#[derive(Debug, Clone)]
pub struct StandaloneBugPipelineFailure {
    pub message: String,
    pub tokens_in: usize,
    pub tokens_out: usize,
    pub tokens_cached: usize,
}

impl std::fmt::Display for StandaloneBugPipelineFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for StandaloneBugPipelineFailure {}

fn wrap_standalone_bug_pipeline_error(
    err: anyhow::Error,
    mut tokens_in: usize,
    mut tokens_out: usize,
    mut tokens_cached: usize,
) -> anyhow::Error {
    if let Some(sf) = err.downcast_ref::<crate::ai::session::SessionFailure>() {
        tokens_in += sf.usage.prompt_tokens;
        tokens_out += sf.usage.completion_tokens;
        tokens_cached += sf.usage.cached_tokens.unwrap_or(0);
    }
    anyhow::Error::new(StandaloneBugPipelineFailure {
        message: err.to_string(),
        tokens_in,
        tokens_out,
        tokens_cached,
    })
}

async fn load_maintainers_for_bug_pipeline(
    tools: Option<&Arc<ToolBox>>,
) -> Option<Arc<crate::maintainers::MaintainersIndex>> {
    if let Some(idx) = crate::maintainers::get_global_maintainers() {
        return Some(idx);
    }
    let repo_path = tools?.get_worktree_path().to_path_buf();
    tokio::task::spawn_blocking(move || {
        crate::maintainers::MaintainersIndex::from_top_of_trunk(&repo_path)
            .ok()
            .map(Arc::new)
    })
    .await
    .ok()
    .flatten()
}

/// Runs the `linux_bug` verification pipeline (`NormalizeSession` ->
/// `VerifySession` -> `TracingSession` -> `SeveritySession`) without a database
/// or LKML `ReportSession`, returning structured verified bug metadata for
/// standalone CLI tools such as `sashiko scan`.
pub async fn run_standalone_bug_pipeline_for_project(
    provider: &dyn AiProvider,
    tools: Option<Arc<ToolBox>>,
    input: BugInput,
    context_tag: Option<&str>,
    project: ProjectId,
) -> Result<StandaloneBugOutcome> {
    let project_label = match project {
        ProjectId::Linux => "Linux kernel",
        ProjectId::Sashiko => "Sashiko",
        ProjectId::Gcc => "GCC",
        ProjectId::Qemu => "QEMU",
        ProjectId::Zephyr => "Zephyr",
    };
    debug!(
        "Processing standalone candidate {} issue: '{}' in subsystems '{:?}'",
        project_label, input.problem, input.subsystems
    );

    let runner = SessionRunner::new(provider).with_max_turns(20);
    let mut tokens_in = 0usize;
    let mut tokens_out = 0usize;
    let mut tokens_cached = 0usize;
    let add_usage =
        |usage: &crate::ai::AiUsage, tin: &mut usize, tout: &mut usize, tcached: &mut usize| {
            *tin += usage.prompt_tokens;
            *tout += usage.completion_tokens;
            *tcached += usage.cached_tokens.unwrap_or(0);
        };

    let master_sha = match input
        .baseline_sha
        .as_deref()
        .or(input.commit_sha.as_deref())
        .map(str::trim)
        .filter(|s| (7..=40).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit()))
    {
        Some(sha) => sha.to_string(),
        None => get_mainline_sha(tools.as_ref(), project).await,
    };

    let mut effective_source_files: Vec<String> = input
        .source_files
        .iter()
        .filter_map(|f| crate::workflows::linux_scan::sanitize_repo_relative_path(f))
        .map(str::to_string)
        .collect();
    let mut effective_locations = input.locations.clone();

    let trace_frames = parse_stack_trace(&format!("{}\n{}", input.problem, input.reasoning));
    if !trace_frames.is_empty() {
        let mut new_locs = Vec::new();
        for (file, line) in trace_frames {
            let Some(clean_file) = crate::workflows::linux_scan::sanitize_repo_relative_path(&file)
                .map(str::to_string)
            else {
                continue;
            };
            if !effective_source_files.contains(&clean_file) {
                effective_source_files.push(clean_file.clone());
            }
            new_locs.push(serde_json::json!({
                "file": clean_file,
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
                debug!("Traced renamed file from {} to {}", file, renamed);
                *file = renamed;
            }
        }
    }

    debug!("--- {} ---", BugStage::Normalization.title());
    let loaded_maintainers = if project.uses_maintainers() {
        load_maintainers_for_bug_pipeline(tools.as_ref()).await
    } else {
        None
    };
    let maintainers_hint = loaded_maintainers.as_ref().map(|mindex| {
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
        context_tag: context_tag.map(str::to_string),
        project,
    };

    let norm_result = runner
        .run(&mut norm_session)
        .await
        .map_err(|e| wrap_standalone_bug_pipeline_error(e, tokens_in, tokens_out, tokens_cached))?;
    add_usage(
        &norm_result.usage,
        &mut tokens_in,
        &mut tokens_out,
        &mut tokens_cached,
    );
    let norm = norm_result.output;

    let mut verified_files = Vec::new();
    let sanitized_norm_files: Vec<String> = norm
        .affected_source_files
        .iter()
        .filter_map(|f| crate::workflows::linux_scan::sanitize_repo_relative_path(f))
        .map(str::to_string)
        .collect();
    if let Some(ref tb) = tools {
        let repo_path = tb.get_worktree_path();
        for file in &sanitized_norm_files {
            if crate::git_ops::git_file_exists_at(repo_path, file, Some(&master_sha)).await {
                verified_files.push(file.clone());
            }
        }
    } else {
        verified_files = sanitized_norm_files;
    }
    if verified_files.is_empty() {
        verified_files = effective_source_files;
    }

    let official_subsystems = resolve_official_subsystems_for_project(
        tools.is_some(),
        loaded_maintainers.as_deref(),
        &input.subsystems,
        &verified_files,
        project,
    );
    let title_prefix = extract_title_prefix(&norm.canonical_title);

    debug!("--- {} ---", BugStage::Verification.title());
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
        context_tag: context_tag.map(str::to_string),
        prefetched_context: prefetched_context.clone(),
        project,
    };

    let verify_result = runner
        .run(&mut verify_session)
        .await
        .map_err(|e| wrap_standalone_bug_pipeline_error(e, tokens_in, tokens_out, tokens_cached))?;
    add_usage(
        &verify_result.usage,
        &mut tokens_in,
        &mut tokens_out,
        &mut tokens_cached,
    );
    let verification = verify_result.output;
    if verification.is_false_positive {
        let reason = verification.refutation_evidence.unwrap_or_else(|| {
            "Discarded as a hallucinated or disproved false positive".to_string()
        });
        return Ok(StandaloneBugOutcome::Discarded {
            reason,
            tokens_in,
            tokens_out,
            tokens_cached,
        });
    }

    let verified_locations = verification
        .relevant_code_locations
        .or_else(|| input.locations.clone());
    let verified_locations_str = verified_locations
        .as_ref()
        .and_then(|v| serde_json::to_string_pretty(v).ok())
        .unwrap_or_else(|| "[]".to_string());

    debug!("--- {} ---", BugStage::OriginTracing.title());
    let tracing_runner = SessionRunner::new(provider).with_max_turns(30);
    let mut tracing_session = TracingSession {
        input: &input,
        master_sha: master_sha.clone(),
        verification_reasoning: verification.verification_reasoning.clone(),
        relevant_locations: verified_locations_str.clone(),
        tools: tools.clone(),
        context_tag: context_tag.map(str::to_string),
        project,
    };
    let tracing_result = tracing_runner
        .run(&mut tracing_session)
        .await
        .map_err(|e| wrap_standalone_bug_pipeline_error(e, tokens_in, tokens_out, tokens_cached))?;
    add_usage(
        &tracing_result.usage,
        &mut tokens_in,
        &mut tokens_out,
        &mut tokens_cached,
    );
    let tracing_output = tracing_result.output;
    let introducing_commit_sha = match tracing_output.introducing_commit_sha {
        Some(sha) => Some(sha),
        None => {
            deterministic_blame_fallback(tools.as_ref(), &verified_locations, &master_sha).await
        }
    };
    let introduced_in_commit = format_commit(tools.as_ref(), introducing_commit_sha).await;

    let verified_prefetched =
        prefetch_bug_locations(tools.as_ref(), &master_sha, &verified_locations).await;
    let effective_prefetched = if !verified_prefetched.is_empty() {
        verified_prefetched
    } else {
        prefetched_context
    };
    let kernel_threat_model = if project == ProjectId::Linux {
        match tools.as_ref() {
            Some(tb) => crate::threat_model::KernelThreatModel::load(tb.get_worktree_path()).await,
            None => None,
        }
    } else {
        None
    };

    debug!("--- {} ---", BugStage::SeverityAssessment.title());
    let mut severity_session = SeveritySession {
        canonical_title: &norm.canonical_title,
        canonical_description: &norm.canonical_description,
        verification_reasoning: &verification.verification_reasoning,
        locations: &verified_locations_str,
        prefetched_context: &effective_prefetched,
        master_sha: &master_sha,
        kernel_threat_model: kernel_threat_model.as_ref(),
        tools: tools.clone(),
        context_tag: context_tag.map(str::to_string),
        project,
    };
    let severity_result = runner
        .run(&mut severity_session)
        .await
        .map_err(|e| wrap_standalone_bug_pipeline_error(e, tokens_in, tokens_out, tokens_cached))?;
    add_usage(
        &severity_result.usage,
        &mut tokens_in,
        &mut tokens_out,
        &mut tokens_cached,
    );
    let severity_output = severity_result.output;
    let severity = Severity::from_str(&severity_output.severity);
    if !matches!(severity, Severity::High | Severity::Critical) {
        return Ok(StandaloneBugOutcome::Discarded {
            reason: format!(
                "Calibrated severity is {} (requires High or Critical): {}",
                severity.as_str(),
                severity_output.severity_explanation
            ),
            tokens_in,
            tokens_out,
            tokens_cached,
        });
    }

    Ok(StandaloneBugOutcome::Verified(Box::new(
        StandaloneVerifiedBug {
            canonical_title: norm.canonical_title,
            canonical_description: norm.canonical_description,
            severity,
            severity_explanation: severity_output.severity_explanation,
            subsystems: official_subsystems,
            source_files: verified_files,
            locations: verified_locations,
            introduced_in_commit,
            verified_on_sha: master_sha,
            tokens_in,
            tokens_out,
            tokens_cached,
        },
    )))
}

// Persist each completed stage immediately. If a later stage fails, its
// predecessors' original interactions and usage remain available for inspection.
#[cfg(feature = "server")]
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

#[cfg(feature = "server")]
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

#[cfg(feature = "server")]
struct DedupAttemptContext<'a> {
    runner: &'a SessionRunner<'a>,
    db: &'a Database,
    bug_row_id: i64,
    review_id: Option<i64>,
    /// Fingerprint the review recorded if it mailed this bug as related.
    disclosure_fingerprint: &'a str,
    project: ProjectId,
    project_label: &'a str,
    context_tag: Option<&'a str>,
    canonical_title: &'a str,
    canonical_description: &'a str,
    verified_locations: Option<&'a Value>,
    official_subsystem_names: &'a [String],
    query_vector: &'a crate::ai::vector_search::BugVector,
}

#[cfg(feature = "server")]
impl DedupAttemptContext<'_> {
    async fn check_and_fold(
        &self,
        known_bugs: &[Bug],
        full_history: &mut Vec<crate::ai::AiMessage>,
    ) -> Result<Option<BugOutcome>> {
        let candidate_matches = find_top_candidates(
            self.query_vector,
            known_bugs,
            DEFAULT_TOP_CANDIDATES,
            DEFAULT_SIMILARITY_THRESHOLD,
        );
        debug!(
            "Deduplication: found {} potential candidates.",
            candidate_matches.len()
        );

        let duplicate_match = if !candidate_matches.is_empty() {
            let candidate_bugs: Vec<Bug> = candidate_matches.into_iter().map(|m| m.bug).collect();

            let mut dedup_session = DedupSession {
                candidate_problem: self.canonical_title,
                candidate_description: self.canonical_description,
                candidate_locations: self.verified_locations,
                candidate_subsystems: self.official_subsystem_names,
                known_candidates: &candidate_bugs,
                context_tag: self.context_tag.map(str::to_string),
                project: self.project,
            };

            let dedup_result = self.runner.run(&mut dedup_session).await?;
            record_bug_stage(
                self.db,
                self.bug_row_id,
                BugStage::Deduplication,
                &dedup_result,
            )
            .await?;
            full_history.extend(dedup_result.history);

            let dedup = dedup_result.output;
            if dedup.is_duplicate {
                dedup.duplicate_of_id.and_then(|dup_id| {
                    candidate_bugs
                        .into_iter()
                        .find(|b| b.id == dup_id)
                        .map(|b| (b, dedup.reasoning))
                })
            } else {
                None
            }
        } else {
            None
        };

        if let Some((existing, reasoning)) = duplicate_match {
            info!(
                "Matched duplicate {} bug {} ({})",
                self.project_label, existing.id, existing.bugid
            );
            let logs = serde_json::to_string(full_history).unwrap_or_default();
            let folded = self
                .db
                .mark_bug_as_duplicate(crate::db::MarkDuplicateBugParams {
                    preserve_triage: true,
                    ephemeral_id: self.bug_row_id,
                    canonical_id: existing.id,
                    reasoning: &reasoning,
                    logs: None,
                    tokens_in: None,
                    tokens_out: None,
                    tokens_cached: None,
                })
                .await?;
            if !folded {
                return Ok(None);
            }
            if let Some(r_id) = self.review_id {
                self.db
                    .link_review_and_disclosure_to_bug(
                        r_id,
                        existing.id,
                        false,
                        self.disclosure_fingerprint,
                    )
                    .await?;
            }
            return Ok(Some(BugOutcome::Duplicate {
                existing_bug: existing,
                reasoning,
                logs: Some(logs),
            }));
        }

        Ok(None)
    }
}

#[cfg(feature = "server")]
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
        ProjectId::Gcc => "GCC",
        ProjectId::Qemu => "QEMU",
        ProjectId::Zephyr => "Zephyr",
    };
    debug!(
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
            ProjectId::Gcc => "sashiko:gcc_bug".to_string(),
            ProjectId::Qemu => "sashiko:qemu_bug".to_string(),
            ProjectId::Zephyr => "sashiko:zephyr_bug".to_string(),
        }
    };
    let is_scan_candidate = tool == "sashiko:linux_scan"
        || bug_row
            .enrichments
            .iter()
            .any(|e| e.kind == "candidate" && e.tool == "sashiko:linux_scan");
    let attributed_db =
        db.with_bug_actor(&actor, &tool, Some(provider.get_capabilities().model_name));
    let db = &attributed_db;
    let mut full_history = Vec::new();
    let runner = SessionRunner::new(provider).with_max_turns(20);
    let master_sha = if (is_scan_candidate
        || (input.patchset_id.is_none() && input.patch_id.is_none() && input.review_id.is_none()))
        && let Some(sha) = input
            .baseline_sha
            .as_deref()
            .or(input.commit_sha.as_deref())
            .map(str::trim)
            .filter(|s| (7..=40).contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit()))
    {
        sha.to_string()
    } else {
        get_mainline_sha(tools.as_ref(), project).await
    };
    let dedup_owner = bug_dedup_lease_owner(bug_row.id, db.bug_claim_owner());

    // Enrich source files and candidate locations via stack trace parsing and git rename tracking
    let mut effective_source_files: Vec<String> = input
        .source_files
        .iter()
        .filter_map(|f| crate::workflows::linux_scan::sanitize_repo_relative_path(f))
        .map(str::to_string)
        .collect();
    let mut effective_locations = input.locations.clone();

    let trace_frames = parse_stack_trace(&format!("{}\n{}", input.problem, input.reasoning));
    if !trace_frames.is_empty() {
        let mut new_locs = Vec::new();
        for (file, line) in trace_frames {
            let Some(clean_file) = crate::workflows::linux_scan::sanitize_repo_relative_path(&file)
                .map(str::to_string)
            else {
                continue;
            };
            if !effective_source_files.contains(&clean_file) {
                effective_source_files.push(clean_file.clone());
            }
            new_locs.push(serde_json::json!({
                "file": clean_file,
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
                debug!("Traced renamed file from {} to {}", file, renamed);
                *file = renamed;
            }
        }
    }

    // Normalization and canonical naming.
    debug!("--- {} ---", BugStage::Normalization.title());
    let loaded_maintainers = if project.uses_maintainers() {
        load_maintainers_for_bug_pipeline(tools.as_ref()).await
    } else {
        None
    };
    let maintainers_hint = loaded_maintainers.as_ref().map(|mindex| {
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
        project,
    };

    let norm_result = runner.run(&mut norm_session).await?;
    record_bug_stage(db, bug_row.id, BugStage::Normalization, &norm_result).await?;
    full_history.extend(norm_result.history);

    let norm = norm_result.output;

    // Verify affected source files against mainline tree, falling back to input locations if empty
    let mut verified_files = Vec::new();
    let sanitized_norm_files: Vec<String> = norm
        .affected_source_files
        .iter()
        .filter_map(|f| crate::workflows::linux_scan::sanitize_repo_relative_path(f))
        .map(str::to_string)
        .collect();
    if let Some(ref tb) = tools {
        let repo_path = tb.get_worktree_path();
        for file in &sanitized_norm_files {
            if crate::git_ops::git_file_exists_at(repo_path, file, Some(&master_sha)).await {
                verified_files.push(file.clone());
            }
        }
    } else {
        verified_files = sanitized_norm_files;
    }

    if verified_files.is_empty() {
        verified_files = effective_source_files.clone();
    }

    let official_subsystems = resolve_official_subsystems_for_project(
        tools.is_some(),
        loaded_maintainers.as_deref(),
        &input.subsystems,
        &verified_files,
        project,
    );
    let official_subsystem_names = similarity_subsystem_names(&official_subsystems);

    let title_prefix = extract_title_prefix(&norm.canonical_title);

    debug!(
        "Normalization complete: title '{}' (prefix '{}') with official subsystems '{:?}'",
        norm.canonical_title, title_prefix, official_subsystems
    );

    // Verification and ground-truth confirmation.
    debug!("--- {} ---", BugStage::Verification.title());
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
    debug!(
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

    // Deduplication confirmation, under BUG_DEDUP_LOCK + cross-process lease.
    debug!("--- {} ---", BugStage::Deduplication.title());
    let query_vector = extract_bug_vector(
        &norm.canonical_title,
        &official_subsystem_names,
        &verified_files,
        verified_locations.as_ref(),
    );

    // The review keyed any related-bug disclosure of this candidate by the
    // concern it handed over, which is exactly this input.
    let disclosure_fingerprint =
        crate::related_preexisting::BugKey::new(&input.problem, input.locations.as_ref())
            .fingerprint;
    let dedup_ctx = DedupAttemptContext {
        runner: &runner,
        db,
        bug_row_id: bug_row.id,
        review_id: input.review_id,
        disclosure_fingerprint: &disclosure_fingerprint,
        project,
        project_label,
        context_tag,
        canonical_title: &norm.canonical_title,
        canonical_description: &norm.canonical_description,
        verified_locations: verified_locations.as_ref(),
        official_subsystem_names: &official_subsystem_names,
        query_vector: &query_vector,
    };

    let (seen_bug_ids, early_dup_outcome) = run_with_bug_dedup_lease(db, &dedup_owner, async {
        let mut known_bugs = db.list_all_bugs_for_vector_search().await?;
        known_bugs.retain(|b| b.id != bug_row.id);
        let seen_ids: std::collections::HashSet<i64> = known_bugs.iter().map(|b| b.id).collect();
        let outcome = dedup_ctx
            .check_and_fold(&known_bugs, &mut full_history)
            .await?;
        Ok((seen_ids, outcome))
    })
    .await?;

    if let Some(dup_outcome) = early_dup_outcome {
        return Ok(dup_outcome);
    }
    debug!("Deduplication complete: novel bug confirmed.");

    // Origin tracing, recorded as an enrichment.
    debug!("--- {} ---", BugStage::OriginTracing.title());
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

    let verified_prefetched =
        prefetch_bug_locations(tools.as_ref(), &master_sha, &verified_locations).await;
    let effective_prefetched = if !verified_prefetched.is_empty() {
        verified_prefetched
    } else {
        prefetched_context
    };
    let kernel_threat_model = if project == ProjectId::Linux {
        match tools.as_ref() {
            Some(tb) => crate::threat_model::KernelThreatModel::load(tb.get_worktree_path()).await,
            None => None,
        }
    } else {
        None
    };

    // Severity and impact estimation, recorded as an enrichment.
    debug!("--- {} ---", BugStage::SeverityAssessment.title());
    let mut severity_session = SeveritySession {
        canonical_title: &norm.canonical_title,
        canonical_description: &norm.canonical_description,
        verification_reasoning: &verification.verification_reasoning,
        locations: &verified_locations_str,
        prefetched_context: &effective_prefetched,
        master_sha: &master_sha,
        kernel_threat_model: kernel_threat_model.as_ref(),
        tools: tools.clone(),
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

    if is_scan_candidate && !matches!(severity, Severity::High | Severity::Critical) {
        let reason = format!(
            "Calibrated severity is {} (requires High or Critical): {}",
            severity.as_str(),
            severity_output.severity_explanation
        );
        info!("{} scan candidate discarded: {}", project_label, reason);
        let logs = serde_json::to_string(&full_history).unwrap_or_default();
        db.update_bug_outcome(
            bug_row.id,
            crate::db::UpdateBugOutcomeParams {
                lifecycle_status: crate::db::BugLifecycleStatus::Dismissed,
                problem: Some(&norm.canonical_title),
                subsystems: Some(&official_subsystems),
                source_files: Some(&verified_files),
                locations: verified_locations.as_ref(),
                severity,
                severity_explanation: Some(&reason),
                introduced_in_commit: introduced_in_commit.as_deref(),
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

    // Standalone plaintext review generation, recorded as an enrichment.
    debug!("--- {} ---", BugStage::ReportGeneration.title());
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

    // Final database write, serialized under BUG_DEDUP_LOCK + cross-process
    // lease so any canonical bug committed while Stages 4-6 were in flight is
    // checked before this bug transitions to 'open' and publishes its vector.
    debug!("--- Final database write ---");

    let late_dup_outcome = run_with_bug_dedup_lease(db, &dedup_owner, async {
        let mut newly_arrived_bugs = db.list_all_bugs_for_vector_search().await?;
        newly_arrived_bugs.retain(|b| b.id != bug_row.id && !seen_bug_ids.contains(&b.id));
        let late_dup = if !newly_arrived_bugs.is_empty() {
            dedup_ctx
                .check_and_fold(&newly_arrived_bugs, &mut full_history)
                .await?
        } else {
            None
        };

        if let Some(dup_outcome) = late_dup {
            Ok(Some(dup_outcome))
        } else {
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
            Ok(None)
        }
    })
    .await?;

    if let Some(dup_outcome) = late_dup_outcome {
        return Ok(dup_outcome);
    }

    let saved_bug = db
        .get_bug_on_write_conn(bug_row.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Bug {} vanished after saving its outcome", bug_row.id))?;
    if let Some(r_id) = input.review_id {
        let disclosure_fingerprint =
            crate::related_preexisting::BugKey::new(&input.problem, input.locations.as_ref())
                .fingerprint;
        db.link_review_and_disclosure_to_bug(r_id, saved_bug.id, true, &disclosure_fingerprint)
            .await?;
    }
    info!(
        "Successfully registered newly verified {} bug {} ({})",
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

/// Resolves the upstream development tip for the configured project.
///
/// Prefers a matching upstream remote, then the project's branch under origin,
/// the local branch, and HEAD. Other projects' reference remotes are ignored.
pub async fn resolve_mainline_sha(
    repo_path: &std::path::Path,
    project: ProjectId,
) -> Option<String> {
    let branch = project.mainline_branch();
    let mut candidate_refs = Vec::new();
    match run_git_query(repo_path, &["remote", "-v"]).await {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                let mut parts = line.split_whitespace();
                if let (Some(name), Some(url), Some("(fetch)")) =
                    (parts.next(), parts.next(), parts.next())
                    && project.matches_upstream_url(url)
                {
                    let candidate = format!("{name}/{branch}");
                    if !candidate_refs.contains(&candidate) {
                        candidate_refs.push(candidate);
                    }
                }
            }
        }
        Ok(_) => {}
        Err(e) => {
            warn!("Failed to query git remotes for mainline tree: {}", e);
        }
    }
    // Match BaselineRegistry's deterministic ordering of upstream remotes.
    candidate_refs.sort_by(|a, b| {
        a.rsplit_once('/')
            .map(|(remote, _)| remote)
            .cmp(&b.rsplit_once('/').map(|(remote, _)| remote))
    });
    let fallback_refs: &[&str] = match project {
        ProjectId::Linux => &[
            "linus/master",
            "origin/master",
            "origin/main",
            "master",
            "main",
            "HEAD",
        ],
        ProjectId::Gcc | ProjectId::Qemu => {
            &["origin/master", "origin/main", "master", "main", "HEAD"]
        }
        ProjectId::Sashiko | ProjectId::Zephyr => &["origin/main", "main", "HEAD"],
    };
    for fallback in fallback_refs {
        if !candidate_refs.iter().any(|candidate| candidate == fallback) {
            candidate_refs.push((*fallback).to_string());
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

#[cfg(feature = "server")]
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

#[cfg(feature = "server")]
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

#[cfg(feature = "server")]
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

#[cfg(feature = "server")]
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
            ProjectId::Gcc => {
                "You are an expert GCC compiler maintainer auditing whether a previously verified GCC bug has been fixed on upstream GCC trunk.\n\
                Do NOT give commits the benefit of the doubt: only mark a bug as \"fixed\" if you can point to a specific upstream commit that genuinely resolves the root cause of the defect or removes the vulnerable code path.\n\
                If commits in the range only refactor, move lines, rename symbols, or modify unrelated functions while the defect remains triggerable, you MUST report \"still_present\".\n\
                Output raw JSON only."
                    .to_string()
            }
            ProjectId::Qemu => {
                "You are an expert QEMU maintainer and system emulation engineer auditing whether a previously verified QEMU bug has been fixed on the upstream QEMU master branch.\n\
                Do NOT give commits the benefit of the doubt: only mark a bug as \"fixed\" if you can point to a specific upstream commit that genuinely resolves the root cause of the defect or removes the vulnerable code path.\n\
                If commits in the range only refactor, move lines, rename symbols, or modify unrelated functions while the defect remains triggerable, you MUST report \"still_present\".\n\
                Output raw JSON only."
                    .to_string()
            }
            ProjectId::Zephyr => {
                "You are an expert Zephyr maintainer and embedded systems engineer auditing whether a previously verified Zephyr bug has been fixed on the upstream Zephyr main branch.\n\
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
            ProjectId::Gcc => (
                "Current Upstream Trunk SHA",
                "Open GCC Bug",
                "upstream GCC trunk",
            ),
            ProjectId::Qemu => (
                "Current Upstream Master SHA",
                "Open QEMU Bug",
                "the upstream QEMU master branch",
            ),
            ProjectId::Zephyr => (
                "Current Upstream Main SHA",
                "Open Zephyr Bug",
                "the upstream Zephyr main branch",
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
        for (idx, res) in futures_util::future::join_all(futures).await {
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
#[cfg(feature = "server")]
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

#[cfg(feature = "server")]
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

#[cfg(feature = "server")]
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

    let fix_tool = fix_check_tool(project);

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

    let mut tb = ToolBox::new(repo_path.to_path_buf());
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

#[cfg(all(test, feature = "server"))]
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

    #[test]
    fn test_infer_project_from_bug_or_tool() {
        // Explicit bugid/tool prefixes always win over a mismatched default.
        for bugid in [None, Some("linux-1"), Some("other")] {
            assert_eq!(
                infer_project_from_bug_or_tool(
                    bugid,
                    Some("sashiko:sashiko_bug"),
                    ProjectId::Linux
                ),
                ProjectId::Sashiko
            );
            assert_eq!(
                infer_project_from_bug_or_tool(bugid, Some("sashiko:qemu_bug"), ProjectId::Linux),
                ProjectId::Qemu
            );
            assert_eq!(
                infer_project_from_bug_or_tool(bugid, Some("sashiko:zephyr_bug"), ProjectId::Linux),
                ProjectId::Zephyr
            );
        }
        assert_eq!(
            infer_project_from_bug_or_tool(Some("qemu-1"), None, ProjectId::Linux),
            ProjectId::Qemu
        );
        assert_eq!(
            infer_project_from_bug_or_tool(Some("zephyr-1"), None, ProjectId::Linux),
            ProjectId::Zephyr
        );
        assert_eq!(
            infer_project_from_bug_or_tool(
                Some("linux-1"),
                Some("sashiko:linux_bug"),
                ProjectId::Sashiko
            ),
            ProjectId::Sashiko
        );
        // The instance's own project wins when the hints say nothing.
        assert_eq!(
            infer_project_from_bug_or_tool(None, None, ProjectId::Sashiko),
            ProjectId::Sashiko
        );
        assert_eq!(
            infer_project_from_bug_or_tool(None, None, ProjectId::Qemu),
            ProjectId::Qemu
        );
        assert_eq!(
            infer_project_from_bug_or_tool(Some("other"), Some("web"), ProjectId::Qemu),
            ProjectId::Qemu
        );
        assert_eq!(
            infer_project_from_bug_or_tool(None, None, ProjectId::Zephyr),
            ProjectId::Zephyr
        );
        assert_eq!(
            infer_project_from_bug_or_tool(None, None, ProjectId::Linux),
            ProjectId::Linux
        );
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

    #[test]
    fn test_dedup_session_validation_checks_candidate_ids() {
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
            enrichments: vec![],
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

        let missing_id = AiResponse {
            content: Some(
                json!({
                    "is_duplicate": true,
                    "duplicate_of_id": null,
                    "reasoning": "Same leak"
                })
                .to_string(),
            ),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            usage: None,
            truncated: false,
        };
        let err_missing = session.validate(&missing_id).unwrap_err();
        assert!(matches!(
            err_missing,
            ValidationError::FormatViolation(ref msg)
                if msg.contains("is_duplicate is true but duplicate_of_id was null")
        ));

        let unknown_id = AiResponse {
            content: Some(
                json!({
                    "is_duplicate": true,
                    "duplicate_of_id": 999,
                    "reasoning": "Same leak"
                })
                .to_string(),
            ),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            usage: None,
            truncated: false,
        };
        let err_unknown = session.validate(&unknown_id).unwrap_err();
        assert!(matches!(
            err_unknown,
            ValidationError::FormatViolation(ref msg)
                if msg.contains("duplicate_of_id 999 is not among candidate IDs [42]")
        ));

        let distinct = AiResponse {
            content: Some(
                json!({
                    "is_duplicate": false,
                    "duplicate_of_id": null,
                    "reasoning": "Distinct leak"
                })
                .to_string(),
            ),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            usage: None,
            truncated: false,
        };
        let parsed = session.validate(&distinct).unwrap();
        assert!(!parsed.is_duplicate);
        assert_eq!(parsed.duplicate_of_id, None);
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
                    verification_reasoning: "verified",
                    locations: "[]",
                    prefetched_context: "",
                    master_sha: "abc123",
                    kernel_threat_model: None,
                    tools: None,
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

    #[test]
    fn test_report_examples_fit_the_width_they_teach() {
        for project in [
            ProjectId::Linux,
            ProjectId::Sashiko,
            ProjectId::Gcc,
            ProjectId::Qemu,
        ] {
            let parts = report_prompt_parts(project);
            for line in parts.examples.lines() {
                let width: usize = line.chars().map(|c| if c == '\t' { 8 } else { 1 }).sum();
                assert!(
                    width <= 75,
                    "{project:?} report example line is {width} columns wide: {line}"
                );
            }
            assert!(report_system_prompt(project).contains(parts.examples));
        }
    }

    #[test]
    fn test_report_session_validation_rejects_empty_and_trims() {
        let mut session = ReportSession {
            problem: "Integer overflow in sound/core/pcm_native.c",
            severity: "High",
            canonical_description: "Overflow",
            severity_explanation: "Buffer overflow",
            locations: None,
            introduced_in_commit: None,
            tools: None,
            context_tag: None,
            prefetched_context: String::new(),
            project: ProjectId::Linux,
        };

        for empty in [None, Some(String::new()), Some("   \n\t  ".to_string())] {
            let resp = AiResponse {
                content: empty,
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            };
            assert!(matches!(
                session.validate(&resp),
                Err(ValidationError::FormatViolation(ref msg)) if msg == "Output was empty"
            ));
        }

        let valid = AiResponse {
            content: Some("  \nIn pcm_native(), size overflows.\n  ".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            usage: None,
            truncated: false,
        };
        assert_eq!(
            session.validate(&valid).unwrap(),
            "In pcm_native(), size overflows."
        );
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
        let tb = Arc::new(ToolBox::new(temp.path().to_path_buf()));

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

        let tb = Arc::new(ToolBox::new(path.to_path_buf()));
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

        let tb = Arc::new(ToolBox::new(path.to_path_buf()));
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

        let tb = Arc::new(ToolBox::new(path.to_path_buf()));
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
    fn test_extract_directory_subsystems_for_zephyr_headers() {
        let files = vec![
            "include/zephyr/drivers/spi.h".to_string(),
            "include/zephyr/kernel.h".to_string(),
            "drivers/spi/spi_foo.c".to_string(),
        ];
        assert_eq!(
            extract_directory_subsystems_for_project(&files, ProjectId::Zephyr),
            vec!["include/zephyr/drivers", "include/zephyr", "drivers/spi"]
        );
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

        // Even when a parsed MAINTAINERS index is already loaded in memory,
        // `has_tools = false` means `verified_files` were not checked against
        // a repository tree, so caller-supplied names must remain unpromoted.
        let maintainers_raw = b"Maintainers List\n===================\n\nNETWORKING [IPv4/IPv6]\nM:\tTest <test@example.com>\nS:\tMaintained\nF:\tnet/ipv4/\n";
        let index =
            crate::maintainers::MaintainersIndex::from_reader(&maintainers_raw[..]).unwrap();
        let resolved_with_loaded = resolve_official_subsystems_for_project(
            false,
            Some(&index),
            &caller,
            &files,
            ProjectId::Linux,
        );
        assert_eq!(resolved_with_loaded, caller);
        assert!(
            resolved_with_loaded
                .iter()
                .all(|s| !s.source.confers_authority())
        );

        // Conversely, when `has_tools = true`, `verified_files` were checked
        // against the tree and ARE promoted via the loaded MAINTAINERS index.
        let resolved_with_tools = resolve_official_subsystems_for_project(
            true,
            Some(&index),
            &caller,
            &files,
            ProjectId::Linux,
        );
        assert_eq!(
            resolved_with_tools,
            vec![AttributedSubsystem::from_maintainers(
                "NETWORKING [IPv4/IPv6]"
            )]
        );
        assert!(resolved_with_tools[0].source.confers_authority());
    }

    #[test]
    fn test_similarity_subsystem_names_skip_upstream_grants() {
        let attributed = vec![
            AttributedSubsystem::from_maintainers("ARM/QUALCOMM SUPPORT"),
            AttributedSubsystem::from_maintainers("ARM/QUALCOMM SUPPORT"),
            AttributedSubsystem::from_path_prefix("arch/arm64"),
            AttributedSubsystem::from_upstream_grant("ARM/SOC SUPPORT"),
            AttributedSubsystem::from_upstream_grant("ARM/QUALCOMM SUPPORT"),
        ];
        assert_eq!(
            similarity_subsystem_names(&attributed),
            vec!["ARM/QUALCOMM SUPPORT".to_string(), "arch/arm64".to_string()]
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

    #[test]
    fn test_tracing_session_validation_requires_hex_commit_sha() {
        let dummy_input = crate::api::BugInput {
            problem: "btrfs: leak".to_string(),
            reasoning: "reason".to_string(),
            locations: None,
            subsystems: Vec::new(),
            source_files: vec!["fs/btrfs/inode.c".to_string()],
            commit_sha: None,
            patchset_id: None,
            patch_id: None,
            baseline_sha: None,
            review_id: None,
        };
        let mut session = TracingSession {
            project: ProjectId::Linux,
            input: &dummy_input,
            verification_reasoning: "verified".to_string(),
            relevant_locations: "[]".to_string(),
            master_sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
            tools: None,
            context_tag: None,
        };

        for invalid in ["HEAD", "--output=/tmp/x", "12345", "not-a-hex-sha"] {
            let resp = AiResponse {
                content: Some(json!({ "introducing_commit_sha": invalid }).to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            };
            assert!(
                matches!(
                    session.validate(&resp),
                    Err(ValidationError::FormatViolation(_))
                ),
                "expected FormatViolation for invalid introducing_commit_sha '{invalid}'"
            );
        }

        let valid_resp = AiResponse {
            content: Some(json!({ "introducing_commit_sha": "  112233445566  " }).to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            usage: None,
            truncated: false,
        };
        let out = session.validate(&valid_resp).unwrap();
        assert_eq!(out.introducing_commit_sha.as_deref(), Some("112233445566"));
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
    async fn mainline_resolution_ignores_other_projects_and_stale_master() {
        for (project, upstream) in [
            (
                ProjectId::Sashiko,
                "https://github.com/sashiko-dev/sashiko.git",
            ),
            (
                ProjectId::Zephyr,
                "https://github.com/zephyrproject-rtos/zephyr.git",
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let repo = temp.path();
            let old = init_test_git_repo(repo);
            let run = |args: &[&str]| {
                let out = crate::git_cmd::in_dir(repo).args(args).output().unwrap();
                assert!(out.status.success(), "git {:?} failed: {:?}", args, out);
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            };
            run(&[
                "remote",
                "add",
                "a-linux",
                "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git",
            ]);
            run(&[
                "remote",
                "add",
                "origin",
                "https://github.com/contributor/renamed-tree.git",
            ]);
            run(&["update-ref", "refs/remotes/origin/master", &old]);
            run(&["commit", "--allow-empty", "-m", "new mainline"]);
            let tip = run(&["rev-parse", "HEAD"]);
            run(&["update-ref", "refs/remotes/origin/main", &tip]);
            run(&["commit", "--allow-empty", "-m", "Linux reference tip"]);
            let linux_tip = run(&["rev-parse", "HEAD"]);
            run(&["update-ref", "refs/remotes/a-linux/master", &linux_tip]);

            assert_eq!(resolve_mainline_sha(repo, project).await, Some(tip.clone()));
            // Linux takes Linus's tree over the fork's master.
            assert_eq!(
                resolve_mainline_sha(repo, ProjectId::Linux).await,
                Some(linux_tip.clone())
            );
            let tools = Arc::new(ToolBox::new(repo.to_path_buf()));
            assert_eq!(get_mainline_sha(Some(&tools), project).await, tip);
            assert_eq!(
                get_mainline_sha(Some(&tools), ProjectId::Linux).await,
                linux_tip
            );

            // A known upstream is preferred over a fork's older main branch.
            run(&["remote", "add", "upstream", upstream]);
            run(&["update-ref", "refs/remotes/upstream/main", &tip]);
            run(&["update-ref", "refs/remotes/origin/main", &old]);
            // A push destination does not supply origin's tracking refs.
            run(&["remote", "set-url", "--push", "origin", upstream]);
            assert_eq!(resolve_mainline_sha(repo, project).await, Some(tip));

            // Linux retains its existing origin/main fallback when no matching
            // upstream remote or origin/master ref exists.
            run(&["remote", "remove", "a-linux"]);
            run(&["update-ref", "-d", "refs/remotes/origin/master"]);
            assert_eq!(
                resolve_mainline_sha(repo, ProjectId::Linux).await,
                Some(old)
            );
        }
    }

    #[tokio::test]
    async fn mainline_registry_and_bug_checks_choose_the_same_mirror() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path();
        let old = init_test_git_repo(repo);
        let run = |args: &[&str]| {
            let out = crate::git_cmd::in_dir(repo).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["remote", "add", "origin", "https://gcc.gnu.org/git/gcc.git"]);
        run(&[
            "remote",
            "add",
            "origin-mirror",
            "https://github.com/gcc-mirror/gcc.git",
        ]);
        run(&["update-ref", "refs/remotes/origin-mirror/master", &old]);
        run(&["commit", "--allow-empty", "-m", "upstream tip"]);
        let tip = run(&["rev-parse", "HEAD"]);
        run(&["update-ref", "refs/remotes/origin/master", &tip]);
        // Exercise both distinct mirrors and duplicate URLs in the remote map.
        for url in [
            "https://github.com/gcc-mirror/gcc.git",
            "https://gcc.gnu.org/git/gcc.git",
        ] {
            run(&["remote", "set-url", "origin-mirror", url]);
            let registry =
                crate::baseline::BaselineRegistry::new(repo, None, Some(ProjectId::Gcc)).unwrap();
            assert_eq!(registry.mainline_ref(), "origin/master");
            let candidates = registry.resolve_candidates(&[], "fix cleanup", None).await;
            let names: Vec<_> = candidates
                .iter()
                .filter_map(|candidate| match candidate {
                    crate::baseline::BaselineResolution::RemoteTarget { name, .. } => {
                        Some(name.as_str())
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(names, vec!["origin", "origin-mirror"]);
            assert_eq!(
                resolve_mainline_sha(repo, ProjectId::Gcc).await,
                Some(tip.clone())
            );
        }

        // A backoff is per remote name, even when both names share a URL.
        let timestamps = repo.join(".sashiko/fetch_timestamps");
        std::fs::create_dir_all(&timestamps).unwrap();
        std::fs::write(timestamps.join("origin.fail"), "").unwrap();
        std::fs::write(timestamps.join("origin-mirror"), "").unwrap();
        run(&[
            "symbolic-ref",
            "refs/remotes/origin-mirror/HEAD",
            "refs/remotes/origin-mirror/master",
        ]);
        let registry =
            crate::baseline::BaselineRegistry::new(repo, None, Some(ProjectId::Gcc)).unwrap();
        let candidates = registry.resolve_candidates(&[], "fix cleanup", None).await;
        let mut resolved = None;
        for candidate in candidates {
            if let crate::baseline::BaselineResolution::RemoteTarget { url, name, .. } = &candidate
                && crate::git_ops::ensure_remote(repo, name, url, false)
                    .await
                    .is_ok()
            {
                resolved = Some(run(&["rev-parse", &candidate.as_str()]));
                break;
            }
        }
        assert_eq!(resolved, Some(old));
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

        let resolved_main = resolve_mainline_sha(repo, ProjectId::Sashiko).await;
        assert_eq!(resolved_main.as_deref(), Some(sha1.as_str()));

        let tb = Arc::new(ToolBox::new(repo.to_path_buf()));
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

    #[tokio::test]
    async fn test_concurrent_in_flight_bugs_deduplicate_at_final_commit() {
        let db = Arc::new(
            Database::new(&crate::settings::DatabaseSettings {
                url: ":memory:".to_string(),
                token: String::new(),
            })
            .await
            .unwrap(),
        );
        db.migrate().await.unwrap();

        let locs = json!([{
            "file": "net/core/dev.c",
            "line": 120,
            "function": "dev_alloc"
        }]);
        let input = BugInput {
            problem: "net: memory leak in dev_alloc".to_string(),
            reasoning: "Allocated buffer is leaked on error return.".to_string(),
            locations: Some(locs.clone()),
            subsystems: vec![AttributedSubsystem::from_maintainers(
                "NETWORKING [GENERAL]",
            )],
            source_files: vec!["net/core/dev.c".to_string()],
            commit_sha: None,
            patchset_id: None,
            patch_id: None,
            baseline_sha: None,
            review_id: None,
        };

        // Both Bug A and Bug B are queued as 'new' / 'pending' before either
        // worker finishes Stages 4-6.
        let dummy = MockAiProvider {
            response_text: "{}".to_string(),
        };
        let BugOutcome::NewlyDiscovered { bug: bug_a } =
            process_issue(&dummy, None, &db, input.clone(), None)
                .await
                .unwrap()
        else {
            panic!("Expected candidate A");
        };
        let BugOutcome::NewlyDiscovered { bug: bug_b } =
            process_issue(&dummy, None, &db, input.clone(), None)
                .await
                .unwrap()
        else {
            panic!("Expected candidate B");
        };

        // Provider for Worker B: when Stage 3 runs, Bug A is still 'new', so
        // Stage 3 sees 0 canonical candidates and makes no LLM call. While
        // Worker B is in Stage 6 (ReportGeneration), Worker A commits its
        // canonical 'open' outcome. Worker B's final-write check under
        // BUG_DEDUP_LOCK then detects Bug A and folds Bug B into Bug A.
        struct ConcurrentCommitProvider {
            db: Arc<Database>,
            bug_a_id: i64,
            locs: Value,
            responses: std::sync::Mutex<std::collections::VecDeque<String>>,
            call_count: std::sync::atomic::AtomicUsize,
        }

        #[async_trait]
        impl AiProvider for ConcurrentCommitProvider {
            async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
                let step = self
                    .call_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Step 0: Normalization, Step 1: Verification, Step 2: OriginTracing,
                // Step 3: SeverityAssessment, Step 4: ReportGeneration (commit Bug A here),
                // Step 5: Late Deduplication at final write.
                if step == 4 {
                    let sub_names = vec!["NETWORKING [GENERAL]".to_string()];
                    let files = vec!["net/core/dev.c".to_string()];
                    let vec_a = extract_bug_vector(
                        "net: memory leak in dev_alloc()",
                        &sub_names,
                        &files,
                        Some(&self.locs),
                    );
                    self.db
                        .update_bug_outcome(
                            self.bug_a_id,
                            crate::db::UpdateBugOutcomeParams {
                                lifecycle_status: crate::db::BugLifecycleStatus::Open,
                                problem: Some("net: memory leak in dev_alloc()"),
                                subsystems: Some(&[AttributedSubsystem::from_maintainers(
                                    "NETWORKING [GENERAL]",
                                )]),
                                source_files: Some(&files),
                                locations: Some(&self.locs),
                                severity: Severity::High,
                                severity_explanation: Some("Leaks buffer on error path"),
                                inline_review: "In dev_alloc(), kfree is missing on error.",
                                vector_json: Some(&vec_a.to_json()),
                                verified_on_sha: Some("HEAD"),
                                ..Default::default()
                            },
                        )
                        .await
                        .unwrap();
                }
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

        let provider = ConcurrentCommitProvider {
            db: db.clone(),
            bug_a_id: bug_a.id,
            locs: locs.clone(),
            responses: std::sync::Mutex::new(
                vec![
                    json!({
                        "canonical_title": "net: memory leak in dev_alloc()",
                        "canonical_description": "Trigger: Error path in dev_alloc.\nFailure Mechanism: Missing kfree.\nImpact: Memory leak.",
                        "affected_source_files": ["net/core/dev.c"]
                    })
                    .to_string(),
                    json!({
                        "verification_reasoning": "Confirmed leak in dev_alloc.",
                        "is_false_positive": false,
                        "refutation_evidence": null,
                        "impact_severity": "High",
                        "relevant_code_locations": locs
                    })
                    .to_string(),
                    json!({
                        "introducing_commit_sha": "112233445566"
                    })
                    .to_string(),
                    json!({
                        "severity": "High",
                        "severity_explanation": "Memory leak in dev_alloc."
                    })
                    .to_string(),
                    "net: memory leak in dev_alloc()\n\nMissing kfree on error return.\n"
                        .to_string(),
                    json!({
                        "is_duplicate": true,
                        "duplicate_of_id": bug_a.id,
                        "reasoning": "Both describe the same missing kfree in dev_alloc()."
                    })
                    .to_string(),
                ]
                .into(),
            ),
            call_count: std::sync::atomic::AtomicUsize::new(0),
        };

        let outcome = process_issue_worker(&provider, None, &db, &bug_b, input, None)
            .await
            .unwrap();
        let BugOutcome::Duplicate { existing_bug, .. } = outcome else {
            panic!("Expected Bug B to fold into Bug A at final write, got {outcome:?}");
        };
        assert_eq!(existing_bug.id, bug_a.id);

        let final_b = db.get_bug(bug_b.id).await.unwrap().unwrap();
        assert_eq!(
            final_b.lifecycle_status,
            crate::db::BugLifecycleStatus::Duplicate
        );
        assert_eq!(final_b.duplicate_of_id, Some(bug_a.id));
    }

    #[tokio::test]
    async fn test_duplicate_of_human_dismissed_bug_preserves_dismissed_status() {
        let db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await
        .unwrap();
        db.migrate().await.unwrap();

        let locs = json!([{
            "file": "net/core/dev.c",
            "line": 120,
            "function": "dev_alloc"
        }]);
        let sub_names = vec!["NETWORKING [GENERAL]".to_string()];
        let files = vec!["net/core/dev.c".to_string()];
        let vec_canon = extract_bug_vector(
            "net: false positive leak in dev_alloc()",
            &sub_names,
            &files,
            Some(&locs),
        );

        let dummy = MockAiProvider {
            response_text: "{}".to_string(),
        };
        let input = BugInput {
            problem: "net: false positive leak in dev_alloc".to_string(),
            reasoning: "Appear to leak buffer.".to_string(),
            locations: Some(locs.clone()),
            subsystems: vec![AttributedSubsystem::from_maintainers(
                "NETWORKING [GENERAL]",
            )],
            source_files: files.clone(),
            commit_sha: None,
            patchset_id: None,
            patch_id: None,
            baseline_sha: None,
            review_id: None,
        };
        let BugOutcome::NewlyDiscovered { bug: canon_bug } =
            process_issue(&dummy, None, &db, input.clone(), None)
                .await
                .unwrap()
        else {
            panic!("Expected canonical candidate");
        };
        db.update_bug_outcome(
            canon_bug.id,
            crate::db::UpdateBugOutcomeParams {
                lifecycle_status: crate::db::BugLifecycleStatus::Open,
                problem: Some("net: false positive leak in dev_alloc()"),
                subsystems: Some(&[AttributedSubsystem::from_maintainers(
                    "NETWORKING [GENERAL]",
                )]),
                source_files: Some(&files),
                locations: Some(&locs),
                severity: Severity::Medium,
                severity_explanation: Some("Potential leak"),
                inline_review: "Report body",
                vector_json: Some(&vec_canon.to_json()),
                verified_on_sha: Some("HEAD"),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // A maintainer dismisses the canonical bug as a false positive.
        db.with_bug_actor("maintainer@kernel.org", "cli", None)
            .set_bug_lifecycle_status(canon_bug.id, crate::db::BugLifecycleStatus::Dismissed)
            .await
            .unwrap();

        let thread_id = db
            .create_thread("<ps-dismissed-dup@example.com>", "net: test series", 1000)
            .await
            .unwrap();
        let ps_id = db
            .create_patchset(
                thread_id,
                None,
                "<ps-dismissed-dup@example.com>",
                "net: test series",
                "dev@kernel.org",
                1000,
                1,
                1,
                "",
                "",
                Some(1),
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let review_id = db
            .create_review(ps_id, None, "mock", "mock", None, None)
            .await
            .unwrap();

        let mut dup_input = input;
        dup_input.review_id = Some(review_id);
        let BugOutcome::NewlyDiscovered { bug: dup_cand } =
            process_issue(&dummy, None, &db, dup_input.clone(), None)
                .await
                .unwrap()
        else {
            panic!("Expected duplicate candidate");
        };

        let provider = QueuedMockAiProvider::new(vec![
            json!({
                "canonical_title": "net: false positive leak in dev_alloc()",
                "canonical_description": "Same false positive in dev_alloc.",
                "affected_source_files": ["net/core/dev.c"]
            })
            .to_string(),
            json!({
                "verification_reasoning": "Verified on tree.",
                "is_false_positive": false,
                "refutation_evidence": null,
                "impact_severity": "Medium",
                "relevant_code_locations": locs
            })
            .to_string(),
            json!({
                "is_duplicate": true,
                "duplicate_of_id": canon_bug.id,
                "reasoning": "Identical to human-dismissed dev_alloc issue."
            })
            .to_string(),
        ]);

        let outcome = process_issue_worker(&provider, None, &db, &dup_cand, dup_input, None)
            .await
            .unwrap();
        assert!(matches!(outcome, BugOutcome::Duplicate { .. }));

        // Canonical bug stays Dismissed (not re-opened) and rediscovery is recorded.
        let refreshed_canon = db.get_bug(canon_bug.id).await.unwrap().unwrap();
        assert_eq!(
            refreshed_canon.lifecycle_status,
            crate::db::BugLifecycleStatus::Dismissed
        );
        let linked = db.list_bugs_for_patchset(ps_id).await.unwrap();
        assert_eq!(linked.len(), 1);
        assert_eq!(linked[0].0.id, canon_bug.id);
        assert!(!linked[0].1);
    }

    #[test]
    fn test_is_bug_prefetch_source_file_supports_gcc_extensions() {
        assert!(is_bug_prefetch_source_file("mm/page_alloc.c"));
        assert!(is_bug_prefetch_source_file("src/main.rs"));
        assert!(is_bug_prefetch_source_file("gcc/tree-ssa-phiopt.cc"));
        assert!(is_bug_prefetch_source_file(
            "libstdc++-v3/include/bits/vector.tcc"
        ));
        assert!(is_bug_prefetch_source_file("gcc/match.pd"));
        assert!(is_bug_prefetch_source_file("gcc/config/i386/i386.md"));
        assert!(is_bug_prefetch_source_file("gcc/fortran/resolve.f90"));
        assert!(!is_bug_prefetch_source_file("README.md"));
        assert!(!is_bug_prefetch_source_file("docs/configuration.md"));
    }
    #[test]
    fn test_severity_session_includes_kernel_threat_model_and_context_for_linux_only() {
        let model = crate::threat_model::KernelThreatModel {
            text: "The Linux Kernel threat model\n=============================\n".to_string(),
            commit: "05441d697f7f0000111122223333444455556666".to_string(),
            git_ref: "linus/master".to_string(),
        };
        let tmp = tempfile::TempDir::new().unwrap();
        let tb = Arc::new(ToolBox::new(tmp.path().to_path_buf()));

        let linux_with = SeveritySession {
            project: ProjectId::Linux,
            canonical_title: "fs/ocfs2: unlocked debugfs list walk",
            canonical_description: "Walks reco.node_data without dlm_reco_state_lock.",
            verification_reasoning: "Verified debug_state_print() holds only dlm->spinlock.",
            locations: "[{\"file\":\"fs/ocfs2/dlm/dlmdebug.c\",\"line\":744}]",
            prefetched_context: "static int debug_state_print(...) { ... }",
            master_sha: "05441d697f7f",
            kernel_threat_model: Some(&model),
            tools: Some(tb),
            context_tag: None,
        };
        let sys = linux_with.system_prompt();
        assert!(sys.contains("<kernel_threat_model source="));
        assert!(sys.contains("linus/master (05441d697f7f)"));
        assert!(sys.contains("The Linux Kernel threat model"));
        let user = linux_with.initial_user_prompt();
        assert!(user.contains("Verification Proof:\nVerified debug_state_print()"));
        assert!(user.contains("Relevant Code Context (at commit 05441d697f7f):"));
        assert!(user.contains("capable(CAP_SYS_ADMIN)"));
        assert!(linux_with.tools().is_some());

        let linux_without = SeveritySession {
            kernel_threat_model: None,
            tools: None,
            ..linux_with
        };
        assert!(
            !linux_without
                .system_prompt()
                .contains("<kernel_threat_model source=")
        );
        assert!(
            !linux_without
                .initial_user_prompt()
                .contains("git_read_files")
        );

        for other in [
            ProjectId::Sashiko,
            ProjectId::Gcc,
            ProjectId::Qemu,
            ProjectId::Zephyr,
        ] {
            let non_linux = SeveritySession {
                project: other,
                kernel_threat_model: Some(&model),
                tools: None,
                context_tag: None,
                ..linux_without
            };
            assert!(
                !non_linux
                    .system_prompt()
                    .contains("<kernel_threat_model source="),
                "{other:?} severity prompt must not include Linux threat model"
            );
        }
    }

    #[tokio::test]
    async fn test_scan_candidate_below_high_severity_is_dismissed_without_report() {
        let db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await
        .unwrap();
        db.migrate().await.unwrap();

        let locs = json!([{
            "file": "net/core/dev.c",
            "line": 120,
            "function_or_symbol": "dev_alloc"
        }]);
        let input = BugInput {
            problem: "net: minor issue in dev_alloc()".to_string(),
            reasoning: "Minor error code mismatch.".to_string(),
            locations: Some(locs.clone()),
            subsystems: vec![AttributedSubsystem::from_maintainers(
                "NETWORKING [GENERAL]",
            )],
            source_files: vec!["net/core/dev.c".to_string()],
            commit_sha: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
            patchset_id: None,
            patch_id: None,
            baseline_sha: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
            review_id: None,
        };

        let dummy = MockAiProvider {
            response_text: "{}".to_string(),
        };
        let scan_db = db.with_bug_actor("sashiko", "sashiko:linux_scan", Some("mock".to_string()));
        let claimed = create_claimed_issue_for_project(
            &dummy,
            &scan_db,
            input.clone(),
            ProjectId::Linux,
            "worker:1",
            300,
        )
        .await
        .unwrap();

        // Only 4 responses provided (Normalize, Verify, Tracing, Severity=Medium).
        // If ReportSession were invoked, QueuedMockAiProvider would return "{}" and
        // the bug would be committed as Open instead of Dismissed.
        let provider = QueuedMockAiProvider::new(vec![
            json!({
                "canonical_title": "net: minor error code in dev_alloc()",
                "canonical_description": "Trigger: Error path.\nFailure Mechanism: Returns -EINVAL.\nImpact: Minor.",
                "affected_source_files": ["net/core/dev.c"]
            })
            .to_string(),
            json!({
                "verification_reasoning": "Verified on tree.",
                "is_false_positive": false,
                "refutation_evidence": null,
                "impact_severity": "Medium",
                "relevant_code_locations": locs
            })
            .to_string(),
            json!({
                "introducing_commit_sha": "112233445566"
            })
            .to_string(),
            json!({
                "severity": "Medium",
                "severity_explanation": "Cosmetic error code difference only."
            })
            .to_string(),
        ]);

        let worker_db = db.with_bug_claim(claimed.id, "worker:1").with_bug_actor(
            "sashiko",
            "sashiko:linux_bug",
            Some("mock".to_string()),
        );
        let outcome = process_issue_worker_for_project(
            &provider,
            None,
            &worker_db,
            &claimed,
            input,
            Some("bug_worker"),
            ProjectId::Linux,
        )
        .await
        .unwrap();

        match outcome {
            BugOutcome::Discarded { reason, .. } => {
                assert!(reason.contains("Calibrated severity is Medium"));
            }
            other => panic!("Expected Discarded outcome for Medium scan bug, got {other:?}"),
        }

        let stored = db.get_bug(claimed.id).await.unwrap().unwrap();
        assert_eq!(
            stored.lifecycle_status,
            crate::db::BugLifecycleStatus::Dismissed
        );
        assert!(stored.vector_json.is_none());
        assert!(
            !stored.enrichments.iter().any(|e| e.kind == "report"),
            "ReportSession must not run for dismissed scan candidates"
        );
    }
}
