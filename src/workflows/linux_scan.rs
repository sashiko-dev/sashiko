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

//! Declarative two-phase workflow for active Linux kernel codebase scanning (`sashiko scan`).
//!
//! Phase 1 (`pre-screen` + `triage`) selects relevant subsystem/pattern guides,
//! explores the target source chunk, identifies high-risk functions and entry
//! points (`focus_targets`), and selects which specialist stages should run
//! (early-exiting benign/boilerplate chunks).
//!
//! Phase 2 runs parallel specialist stages (`execution-flow`, `resources`,
//! `locking`, `security`, `hardware`) with full Git tool access, followed by
//! `verification` and parallel `post-verification` so confirmed `High` and
//! `Critical` chunk findings can be converted into candidate [`crate::api::BugInput`]
//! payloads and fed into the [`crate::workflows::linux_bug`] pipeline.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::workflow::graph::Workflow;
use crate::workflow::output::OutputFormat;
use crate::workflow::policy::{ParallelPolicy, StagePolicy, ToolScope};
use crate::workflow::prompt::PromptTemplate;
use crate::workflow::stage::{ExecutableStage, Stage};
use crate::workflows::guard::{normalize_guide_name, normalize_stage_name, sanitize_guide_name};
use crate::workflows::linux_patch_review::{
    AnalysisStage, ConsolidationStage, MAX_POST_VERIFICATION_STAGES, POST_VERIFICATION_STAGE_NAMES,
    PostVerificationOutput, PrescreenOutput, StageConcernsOutput, VerificationOutput,
    append_stage_dismissed_concerns_with_prompts, append_stage_items_with_prompts,
    batch_hard_cases_by_severity, build_stage_efforts_schema, collect_stage_prompts,
    enrich_post_verification_output, extra_prompt_paths_for_items, extract_item_stages_and_prompts,
    format_post_verification_feedback, format_verification_stage_feedback,
    has_valid_proof_location, push_unique_prompt, push_unique_string,
};

/// High-risk function or code range identified during Phase 1 triage.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ScanFocusTarget {
    pub function: String,
    #[serde(default)]
    pub lines: String,
    pub risk_category: String,
    pub hypothesis: String,
    #[serde(default)]
    pub callers_or_callees_to_check: Vec<String>,
}

/// Output of the Phase 1 `triage` stage.
#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct ScanTriageOutput {
    pub relevant_stages: Vec<String>,
    #[serde(default)]
    pub stage_efforts: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub focus_targets: Vec<ScanFocusTarget>,
}

/// Execution state for scanning a single source chunk in the Linux kernel.
#[derive(Clone, Debug, Default)]
pub struct LinuxScanState {
    pub chunk_id: String,
    pub target_sha: String,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub numbered_source: String,
    pub chunk_functions_json: String,
    pub file_function_index: String,
    pub prefetched_context: String,
    pub maintainers_hint: Option<String>,

    /// Subsystem/pattern guides selected during `pre-screen`.
    pub selected_guides: Vec<String>,
    /// Optional manual stages filter (`--stages locking,security`).
    pub manual_stages: Option<Vec<String>>,
    /// Caller-supplied instructions appended to the shared system prompt.
    pub custom_prompt: Option<String>,
    /// Specialist stages selected by `triage` (or overridden by `manual_stages`).
    pub planned_stages: Vec<String>,
    /// Optional reasoning effort mode (`Mid`, `High`, or `Dynamic`).
    pub effort: Option<crate::ai::EffortMode>,
    /// Per-stage reasoning efforts selected by `triage`.
    pub planned_efforts: std::collections::BTreeMap<String, crate::ai::ReasoningEffort>,
    /// High-risk functions and hypotheses identified by `triage`.
    pub focus_targets: Vec<ScanFocusTarget>,

    /// Aggregated raw concerns from specialist stages (`C1..Cn`).
    pub all_concerns: Vec<Value>,
    /// Aggregated raw dismissed concerns from specialist stages (`D1..Dm`).
    pub all_dismissed_concerns: Vec<Value>,

    /// Direct findings (`VF1..VFp`) emitted by `verification`.
    pub verification_findings: Vec<Value>,
    /// Direct dismissals (`VD1..VDq`) emitted by `verification`.
    pub verification_dismissed: Vec<Value>,
    /// Findings confirmed by parallel `post-verification` (`PVF1..PVFr`).
    pub post_verification_findings: Vec<Value>,
    /// Hard cases refuted by parallel `post-verification` (`PVD1..PVDs`).
    pub post_verification_dismissed: Vec<Value>,
    /// Deduplicated dismissed concerns across verification stages.
    pub deduplicated_dismissed_concerns: Vec<Value>,
    /// Contested or speculative candidates routed to `post-verification`.
    pub hard_cases: Vec<Value>,

    /// Verified `High` and `Critical` findings discovered in this chunk.
    pub findings: Vec<Value>,
}

// ---------------------------------------------------------------------------
// Stage Definitions & Tables
// ---------------------------------------------------------------------------

const STAGE_SCAN_EXECUTION_FLOW_INSTRUCTION: &str = r#"# Active Codebase Scan: Execution Flow & Error Path Audit

You are a static analysis engine auditing existing Linux kernel C or Rust source code in the target chunk for serious (`High` or `Critical`) execution flow defects.
1. Prioritize the `focus_targets` identified during triage, then systematically audit the remaining functions in the chunk.
2. Trace control flow across every branch, switch statement, loop boundary, and `goto` error-handling label:
   - Look for missing state restoration or partial-init unwind omissions on error paths (e.g., leaving global/shared pointers, IDs, or list links dangling when returning an error).
   - Look for NULL pointer dereferences reachable via concrete caller paths (remember: reading a pointer field is not a dereference; accessing its target memory is).
   - Check for off-by-one array bounds, signed/unsigned comparison bugs, integer truncation, or bitwise mask/shift errors that corrupt kernel state.
   - Use `git_read_files` and `git_grep` at the target revision to verify caller preconditions and callee return-value contracts across the repository.
3. Focus strictly on reachable `High` and `Critical` bugs (crash, panic, memory corruption, UAF, privilege escalation, or persistent resource/state corruption). Ignore style, minor cleanups, or purely theoretical issues guarded by callers."#;

const STAGE_SCAN_RESOURCES_INSTRUCTION: &str = r#"# Active Codebase Scan: Resource Lifecycle & Memory Safety Audit

You are an expert in Linux kernel resource management auditing the target source chunk for serious (`High` or `Critical`) memory and lifecycle bugs.
1. Prioritize the `focus_targets` identified during triage, then systematically audit every allocation, reference count operation, registration, and teardown path in the chunk.
2. Check for:
   - Use-After-Free (UAF) and double-free vulnerabilities across error paths, concurrent callbacks, and teardown routines.
   - Reference counting imbalances (`kref_get`/`kref_put`, `get_device`/`put_device`, `fget`/`fput`, `of_node_get`/`of_node_put`, RCU grace-period violations).
   - Memory, page, DMA, or descriptor leaks on reachable error returns or abnormal exit paths.
   - Asynchronous teardown ordering bugs: freeing an object before canceling timers (`timer_delete_sync`), workqueues (`cancel_work_sync` / `disable_work_sync`), IRQs (`free_irq`), or unregistering subsystem interfaces that can still invoke callbacks on that object.
3. Use `git_read_files` and `git_grep` at the target revision to inspect callers and paired init/cleanup functions before concluding."#;

const STAGE_SCAN_LOCKING_INSTRUCTION: &str = r#"# Active Codebase Scan: Locking, Concurrency & Synchronization Audit

You are a Linux kernel concurrency and locking specialist auditing the target source chunk for serious (`High` or `Critical`) synchronization defects.
1. Prioritize the `focus_targets` identified during triage, then audit shared-state accesses and lock scopes across the chunk.
2. Check for:
   - Sleeping in atomic context: calling `mutex_lock`, `kzalloc(..., GFP_KERNEL)`, `msleep`, `synchronize_rcu`, `cancel_work_sync`, or `copy_from_user`/`copy_to_user` while holding a spinlock, rwlock, RCU read lock (`rcu_read_lock`), or with IRQs/preemption disabled.
   - AB-BA deadlocks, lock order inversions across functions/callers, recursive locking, or missing `spin_lock_irqsave` when a lock is also acquired in interrupt/timer context.
   - Missing unlock on error return paths.
   - Data races / TOCTOU races where shared lists, pointers, or state flags are modified or freed without holding the required lock or RCU synchronization, leading to UAF or kernel memory corruption.
3. Use `git_read_files` and `git_grep` at the target revision to verify lock hierarchies and caller lock contexts across the tree."#;

const STAGE_SCAN_SECURITY_INSTRUCTION: &str = r#"# Active Codebase Scan: Security & Boundary Audit

You are a Linux kernel security researcher auditing the target source chunk for serious (`High` or `Critical`) vulnerabilities.
1. Prioritize the `focus_targets` identified during triage, focusing on user-space entry points (`ioctl`, `netlink`, `sysfs`, `debugfs`, `procfs`, file operations, socket/network packet parsers, and vhost/virtio handlers).
2. Check for:
   - Out-of-bounds reads/writes, heap/stack buffer overflows, and integer overflows/underflows in allocation size or offset calculations controlled by untrusted input.
   - Missing capability/permission checks (`capable()`, `ns_capable()`) on privileged operations.
   - Uninitialized kernel stack or heap memory copied to user space (`copy_to_user`, `put_user`, `nla_put`) causing kernel information leaks.
   - Double-fetch / TOCTOU vulnerabilities across user-space memory reads.
3. Trace untrusted inputs from caller entry points using `git_grep` and `git_read_files` at the target revision to confirm reachability."#;

const STAGE_SCAN_HARDWARE_INSTRUCTION: &str = r#"# Active Codebase Scan: Hardware & Driver State Machine Audit

You are a hardware and device-driver engineer auditing the target source chunk for serious (`High` or `Critical`) driver and hardware interaction bugs.
1. If the target chunk is purely generic software logic with no hardware/driver interaction, return `{"concerns": [], "dismissed_concerns": []}`.
2. Otherwise, prioritize `focus_targets` and check for:
   - Accessing MMIO registers or hardware queues before clocks/power domains/runtime PM are enabled or after they are disabled (`pm_runtime_get_sync` / `clk_prepare_enable` ordering).
   - DMA mapping/unmapping bugs: DMA-mapping stack memory, wrong `dma_data_direction`, missing `dma_mapping_error()` checks, or freeing/unmapping DMA buffers while hardware DMA or IRQ handlers are still active.
   - Suspend/resume, device reset, or probe/remove asymmetry that leaves IRQs enabled, hardware rings uninitialized, or resources leaked/corrupted."#;

const SCAN_STAGE_JSON_SCHEMA_INSTRUCTIONS: &str = r#"
Once you have gathered sufficient information with tools, return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
If you find no High or Critical bugs and no dismissed concerns, return `{"concerns": [], "dismissed_concerns": []}`.
- Set `"preexisting": true` on all concerns discovered during codebase scanning.
- Each object in `'concerns'` MUST use: `"type"`, `"description"`, `"reasoning"`, `"preexisting"` (`true`), and `"locations"` (non-empty array of `{"file", "function_or_symbol", "line", "code_snippet", "why_this_location_matters"}`).
- Each object in `'dismissed_concerns'` MUST use: `"type"`, `"description"`, `"reasoning"`, and `"locations"` (quoting the verbatim disproving guard, lock, cleanup path, or caller check in the repository).
- NO DISMISSAL WITHOUT VERIFIED PROOF: Never dismiss a candidate bug by assuming callers or surrounding code handle it without verifying ALL callers via `git_grep` and `git_read_files` and quoting the disproving code in `locations`. If you cannot find concrete disproving code, report it in `'concerns'`."#;

pub static SCAN_ANALYSIS_STAGES: &[AnalysisStage] = &[
    AnalysisStage {
        name: "execution-flow",
        short: "Execution Flow",
        instruction: STAGE_SCAN_EXECUTION_FLOW_INSTRUCTION,
        guides: &["callstack.md", "technical-patterns.md"],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "resources",
        short: "Resource Mgmt",
        instruction: STAGE_SCAN_RESOURCES_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "locking",
        short: "Locking & Sync",
        instruction: STAGE_SCAN_LOCKING_INSTRUCTION,
        guides: &["subsystem/locking.md"],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "security",
        short: "Security Audit",
        instruction: STAGE_SCAN_SECURITY_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "hardware",
        short: "Hardware Review",
        instruction: STAGE_SCAN_HARDWARE_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
];

pub static SCAN_VERIFICATION: ConsolidationStage = ConsolidationStage {
    name: "verification",
    short: "Verification",
    wants_series_context: false,
};

pub static SCAN_POST_VERIFICATION: ConsolidationStage = ConsolidationStage {
    name: "post-verification",
    short: "Post-Verification",
    wants_series_context: false,
};

pub static SCAN_CONSOLIDATION_STAGES: &[&ConsolidationStage] =
    &[&SCAN_VERIFICATION, &SCAN_POST_VERIFICATION];

pub fn scan_analysis_stage_by_name(name: &str) -> Option<&'static AnalysisStage> {
    let normalized = normalize_stage_name(name);
    SCAN_ANALYSIS_STAGES.iter().find(|s| s.name == normalized)
}

pub fn scan_consolidation_stage_by_name(name: &str) -> Option<&'static ConsolidationStage> {
    let normalized = normalize_stage_name(name);
    if POST_VERIFICATION_STAGE_NAMES.contains(&normalized.as_str()) {
        return Some(&SCAN_POST_VERIFICATION);
    }
    SCAN_CONSOLIDATION_STAGES
        .iter()
        .copied()
        .find(|s| s.name == normalized)
}

pub fn scan_stage_short_label(name: &str) -> Option<&'static str> {
    if let Some(def) = scan_analysis_stage_by_name(name) {
        return Some(def.short);
    }
    if let Some(cons) = scan_consolidation_stage_by_name(name) {
        return Some(cons.short);
    }
    match normalize_stage_name(name).as_str() {
        "pre-screen" => Some("Pre-Screen"),
        "triage" => Some("Chunk Triage"),
        _ => None,
    }
}

pub fn is_known_scan_stage(name: &str) -> bool {
    scan_stage_short_label(name).is_some()
}

pub fn is_scan_stage_exclusive_guide(name: &str) -> bool {
    SCAN_ANALYSIS_STAGES
        .iter()
        .flat_map(|def| def.guides)
        .any(|guide| guide.rsplit('/').next() == Some(name))
}

// ---------------------------------------------------------------------------
// System Prompt Template
// ---------------------------------------------------------------------------

pub fn linux_scan_system_prompt() -> PromptTemplate<LinuxScanState> {
    let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();

    PromptTemplate::<LinuxScanState>::new(format!(
        r#"Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references strictly on this current date.

You are an expert Linux kernel maintainer and static security auditor performing an active deep scan of the Linux kernel source tree to uncover serious (`High` and `Critical`) defects.

TOOL USAGE: When you need to gather information using tools (`git_read_files`, `git_grep`, `git_blame`, `git_log`, `git_show`, `read_prompt`), actively batch parallel or independent tool calls into a single response to minimize conversation turns. Always pass the exact `Target Revision SHA` (`{{{{target_sha}}}}`) when calling Git tools.

<global_review_guidelines>
The following documents contain the official technical patterns, architectural rules, and subsystem-specific guidelines that you MUST adhere to during your audit.
@includes
</global_review_guidelines>

=== Active Scan Target Metadata ===
Target Revision SHA: {{{{target_sha}}}}
Chunk ID: {{{{chunk_id}}}}
Target File: {{{{file_path}}}} (lines {{{{start_line}}}}..={{{{end_line}}}}){{{{maintainers_block}}}}
===================================

<file_function_index>
Outline of all functions in {{{{file_path}}}}:
{{{{file_function_index}}}}
</file_function_index>

<chunk_functions>
Functions defined in this chunk (lines {{{{start_line}}}}..={{{{end_line}}}}):
{{{{chunk_functions_json}}}}
</chunk_functions>

<target_chunk_source>
{{{{numbered_source}}}}
</target_chunk_source>{{{{prefetched_block}}}}{{{{focus_targets_block}}}}{{{{custom_prompt_block}}}}"#
    ))
    .with_var("target_sha", |s: &LinuxScanState| s.target_sha.clone())
    .with_var("chunk_id", |s: &LinuxScanState| s.chunk_id.clone())
    .with_var("file_path", |s: &LinuxScanState| s.file_path.clone())
    .with_var("start_line", |s: &LinuxScanState| s.start_line.to_string())
    .with_var("end_line", |s: &LinuxScanState| s.end_line.to_string())
    .with_var("file_function_index", |s: &LinuxScanState| {
        s.file_function_index.clone()
    })
    .with_var("chunk_functions_json", |s: &LinuxScanState| {
        s.chunk_functions_json.clone()
    })
    .with_var("numbered_source", |s: &LinuxScanState| {
        s.numbered_source.clone()
    })
    .with_var("maintainers_block", |s: &LinuxScanState| {
        s.maintainers_hint
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map_or_else(String::new, |h| {
                if h.starts_with("MAINTAINERS") || h.starts_with("<official_maintainers") {
                    format!("\n{h}")
                } else {
                    format!("\nMAINTAINERS Subsystem(s): {h}")
                }
            })
    })
    .with_var("prefetched_block", |s: &LinuxScanState| {
        if s.prefetched_context.trim().is_empty() {
            String::new()
        } else {
            format!(
                "\n\n<pre_fetched_context>\nThe following external definitions (structs, enums, macros, and callees) referenced by this chunk were pre-fetched at revision `{}`:\n\n{}\n</pre_fetched_context>",
                s.target_sha, s.prefetched_context
            )
        }
    })
    .with_var("focus_targets_block", |s: &LinuxScanState| {
        if s.focus_targets.is_empty() {
            String::new()
        } else {
            let json_str = serde_json::to_string_pretty(&s.focus_targets).unwrap_or_default();
            format!(
                "\n\n<triage_focus_targets>\nPhase 1 triage identified the following high-risk functions and hypotheses in this chunk:\n{json_str}\n</triage_focus_targets>"
            )
        }
    })
    .with_var("custom_prompt_block", |s: &LinuxScanState| {
        s.custom_prompt
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map_or_else(String::new, |p| {
                format!("\n\n<custom_instructions>\n{p}\n</custom_instructions>")
            })
    })
    .include_files_from_state(|s: &LinuxScanState| {
        let mut paths = Vec::new();
        for guide in &s.selected_guides {
            let name = normalize_guide_name(guide);
            paths.push(PathBuf::from("subsystem").join(name));
            paths.push(PathBuf::from("patterns").join(name));
        }
        paths
    })
}

// ---------------------------------------------------------------------------
// Validation & Reduction Helpers
// ---------------------------------------------------------------------------

const FORBIDDEN_TITLE_FIX_PREFIXES: &[&str] = &[
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

fn split_canonical_title_segments(trimmed: &str) -> Result<Vec<&str>, String> {
    let bytes = trimmed.as_bytes();
    let mut segments = Vec::new();
    let mut seg_start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b':' {
            let run_start = i;
            while i < bytes.len() && bytes[i] == b':' {
                i += 1;
            }
            let run_len = i - run_start;
            if run_len == 1 {
                segments.push(trimmed[seg_start..run_start].trim());
                seg_start = i;
            } else if run_len == 2 {
                let prev_ok = trimmed[..run_start]
                    .chars()
                    .next_back()
                    .is_some_and(|c| !c.is_whitespace());
                let next_ok = trimmed[i..]
                    .chars()
                    .next()
                    .is_some_and(|c| !c.is_whitespace());
                if !prev_ok || !next_ok {
                    return Err(format!(
                        "problem (canonical title) contains a malformed '::' separator; use '<subsystem_prefix>: <defect in fn()>' and only use '::' inside Rust/C++ identifiers like 'Type::method()'. Got: '{trimmed}'."
                    ));
                }
            } else {
                return Err(format!(
                    "problem (canonical title) contains an invalid colon sequence ('{}'). Got: '{trimmed}'.",
                    &trimmed[run_start..i]
                ));
            }
        } else {
            i += 1;
        }
    }
    segments.push(trimmed[seg_start..].trim());
    if segments.len() < 2 {
        return Err(format!(
            "problem (canonical title) must follow '<subsystem_prefix>: <defect in fn()>' format with a colon. Got: '{trimmed}'."
        ));
    }
    if segments.iter().any(|seg| seg.is_empty()) {
        return Err(format!(
            "problem (canonical title) must have both a non-empty subsystem prefix and defect description separated by ':'. Got: '{trimmed}'."
        ));
    }
    Ok(segments)
}

pub fn validate_canonical_bug_title(title: &str) -> Result<(), String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return Err("problem (canonical title) cannot be empty.".to_string());
    }
    if trimmed.chars().count() >= 80 {
        return Err(format!(
            "problem (canonical title) must be strictly under 80 characters (got {} chars): '{}'.",
            trimmed.chars().count(),
            trimmed
        ));
    }
    if trimmed.contains('`') {
        return Err(format!(
            "problem (canonical title) must not contain backticks ('`'): '{}'.",
            trimmed
        ));
    }
    let segments = split_canonical_title_segments(trimmed)?;
    for segment in &segments[1..] {
        let desc_lower = segment.to_ascii_lowercase();
        if FORBIDDEN_TITLE_FIX_PREFIXES
            .iter()
            .any(|forbidden| desc_lower.starts_with(forbidden))
        {
            return Err(format!(
                "problem (canonical title) must describe the existing defect rather than a patch/fix; do not start the description after ':' with action verbs like 'fix', 'prevent', 'avoid', or 'resolve'. Got: '{}'.",
                trimmed
            ));
        }
    }
    Ok(())
}

pub fn validate_canonical_bug_description(desc: &str) -> Result<(), String> {
    let trimmed = desc.trim();
    if trimmed.is_empty() {
        return Err("canonical_description cannot be empty.".to_string());
    }
    let lower = trimmed.to_ascii_lowercase();
    if !lower.contains("trigger") && !lower.contains("precondition") {
        return Err(
            "canonical_description must include a 'Trigger / Preconditions:' section.".to_string(),
        );
    }
    if !lower.contains("failure mechanism") {
        return Err(
            "canonical_description must include a 'Failure Mechanism:' section.".to_string(),
        );
    }
    if !lower.contains("impact") {
        return Err("canonical_description must include an 'Impact:' section.".to_string());
    }
    Ok(())
}

pub fn validate_scan_finding_object(finding: &Value, idx: usize) -> Result<(), String> {
    let problem = finding.get("problem").and_then(Value::as_str).unwrap_or("");
    validate_canonical_bug_title(problem).map_err(|e| format!("findings[{idx}]: {e}"))?;

    let canonical_desc = finding
        .get("canonical_description")
        .and_then(Value::as_str)
        .unwrap_or("");
    validate_canonical_bug_description(canonical_desc)
        .map_err(|e| format!("findings[{idx}]: {e}"))?;

    let has_severity = finding
        .get("severity")
        .and_then(Value::as_str)
        .is_some_and(|s| {
            matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "low" | "medium" | "high" | "critical" | "unknown"
            )
        });
    let has_explanation = finding
        .get("severity_explanation")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    if !has_severity || !has_explanation {
        return Err(format!(
            "findings[{idx}] must have valid 'severity' (Low, Medium, High, Critical, or Unknown) and non-empty 'severity_explanation'."
        ));
    }
    if !has_valid_proof_location(finding) {
        return Err(format!(
            "findings[{idx}] must include a non-empty 'locations' array with at least one entry containing non-empty 'file', 'function_or_symbol', and verbatim 'code_snippet'."
        ));
    }
    Ok(())
}

fn validate_scan_concerns_output(
    output: &StageConcernsOutput,
    _state: &LinuxScanState,
) -> Result<(), String> {
    for (idx, concern) in output.concerns.iter().enumerate() {
        let has_desc = concern
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_reasoning = concern
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_desc || !has_reasoning {
            return Err(format!(
                "concerns[{idx}] must have non-empty 'description' and 'reasoning' strings."
            ));
        }
        if !has_valid_proof_location(concern) {
            return Err(format!(
                "concerns[{idx}] must include at least one entry in 'locations' with non-empty 'file', 'function_or_symbol', and 'code_snippet'."
            ));
        }
    }

    for (idx, dismissed) in output.dismissed_concerns.iter().enumerate() {
        let has_desc = dismissed
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_reasoning = dismissed
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_desc || !has_reasoning {
            return Err(format!(
                "dismissed_concerns[{idx}] must have non-empty 'description' and 'reasoning' strings."
            ));
        }
        if !has_valid_proof_location(dismissed) {
            return Err(format!(
                "dismissed_concerns[{idx}] must include at least one entry in 'locations' with non-empty 'file', 'function_or_symbol', and verbatim disproving 'code_snippet'."
            ));
        }
    }
    Ok(())
}

fn format_scan_concerns_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. Return ONLY a JSON object containing 'concerns' and 'dismissed_concerns' arrays.",
        violation
    )
}

fn extract_non_empty_source_ids<'a>(
    collection_name: &str,
    idx: usize,
    item: &'a Value,
    valid_ids_label: &str,
) -> Result<Vec<&'a str>, String> {
    let Some(arr) = item.get("source_ids").and_then(Value::as_array) else {
        return Err(format!(
            "{collection_name}[{idx}] must include a non-empty 'source_ids' array referencing input item ID(s) ({valid_ids_label})."
        ));
    };
    let ids: Vec<&str> = arr
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if ids.is_empty() || ids.len() != arr.len() {
        return Err(format!(
            "{collection_name}[{idx}] must have a non-empty 'source_ids' array of non-empty string IDs ({valid_ids_label})."
        ));
    }
    Ok(ids)
}

pub fn validate_scan_verification_output(
    output: &VerificationOutput,
    state: &LinuxScanState,
) -> Result<(), String> {
    for (idx, finding) in output.findings.iter().enumerate() {
        validate_scan_finding_object(finding, idx)?;
    }

    for (idx, hard_case) in output.hard_cases.iter().enumerate() {
        let has_type = hard_case
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_desc = hard_case
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_severity = hard_case
            .get("estimated_severity")
            .and_then(Value::as_str)
            .is_some_and(|s| matches!(s, "Low" | "Medium" | "High" | "Critical" | "Unknown"));
        let has_signal_reason = hard_case
            .get("signal_reason")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_args = hard_case
            .get("concern_arguments")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_dismissal_args = hard_case
            .get("dismissal_arguments")
            .is_some_and(|v| v.is_string() || v.is_null());
        let has_question = hard_case
            .get("verification_question")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_locations = hard_case.get("locations").is_some_and(Value::is_array);
        if !has_type
            || !has_desc
            || !has_severity
            || !has_signal_reason
            || !has_args
            || !has_dismissal_args
            || !has_question
            || !has_locations
        {
            return Err(format!(
                "hard_cases[{idx}] must have non-empty 'type', 'description', 'signal_reason', 'concern_arguments', and 'verification_question' strings, valid 'estimated_severity' (Low, Medium, High, Critical, or Unknown), string 'dismissal_arguments', and a 'locations' array."
            ));
        }
    }

    if state.all_dismissed_concerns.is_empty() && !output.dismissed_concerns.is_empty() {
        return Err(format!(
            "dismissed_concerns contains {} entries, but input dismissed_concerns is empty. Raised concerns from all_concerns must never be placed into dismissed_concerns in verification; route any disputed, speculative, or questionable concern to hard_cases.",
            output.dismissed_concerns.len(),
        ));
    }

    for (idx, dismissed) in output.dismissed_concerns.iter().enumerate() {
        let desc = dismissed
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let has_reasoning = dismissed
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if desc.is_empty() || !has_reasoning {
            return Err(format!(
                "dismissed_concerns[{idx}] must have non-empty 'description' and 'reasoning' strings."
            ));
        }
        if !has_valid_proof_location(dismissed) {
            return Err(format!(
                "dismissed_concerns[{idx}] must include at least one entry in 'locations' with non-empty 'file', 'function_or_symbol', and disproving 'code_snippet'."
            ));
        }
    }

    let concern_ids: Vec<&str> = state
        .all_concerns
        .iter()
        .filter_map(|c| c.get("id").and_then(Value::as_str).map(str::trim))
        .filter(|id| !id.is_empty())
        .collect();
    let dismissed_ids: Vec<&str> = state
        .all_dismissed_concerns
        .iter()
        .filter_map(|d| d.get("id").and_then(Value::as_str).map(str::trim))
        .filter(|id| !id.is_empty())
        .collect();

    if !concern_ids.is_empty() || !dismissed_ids.is_empty() {
        let all_valid: Vec<&str> = concern_ids
            .iter()
            .chain(dismissed_ids.iter())
            .copied()
            .collect();
        let valid_label = all_valid.join(", ");
        let mut accounted = std::collections::BTreeSet::new();

        for (collection_name, items, allow_concerns, allow_dismissed) in [
            ("findings", &output.findings, true, false),
            ("hard_cases", &output.hard_cases, true, true),
            (
                "dismissed_concerns",
                &output.dismissed_concerns,
                false,
                true,
            ),
        ] {
            for (idx, item) in items.iter().enumerate() {
                let sids = extract_non_empty_source_ids(collection_name, idx, item, &valid_label)?;
                for sid in sids {
                    if !all_valid.contains(&sid) {
                        return Err(format!(
                            "{collection_name}[{idx}].source_ids contains unknown ID '{sid}'. Valid input IDs are: {valid_label}."
                        ));
                    }
                    if !allow_concerns && concern_ids.contains(&sid) {
                        return Err(format!(
                            "dismissed_concerns[{idx}].source_ids references raised concern ID '{sid}'. Raised concerns (C*) must never be placed in 'dismissed_concerns' in verification; classify any contested or questionable concern into 'hard_cases'."
                        ));
                    }
                    if !allow_dismissed && dismissed_ids.contains(&sid) {
                        return Err(format!(
                            "findings[{idx}].source_ids references dismissed_concern ID '{sid}'. Category 1a 'findings' are only for uncontested raised concerns (C*); any contested issue (C* + D*) or promoted dismissal (D*) must be placed in 'hard_cases'."
                        ));
                    }
                    accounted.insert(sid);
                }
            }
        }

        let missing: Vec<&str> = all_valid
            .into_iter()
            .filter(|id| !accounted.contains(id))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "Verification output failed to account for input ID(s): {}. Every input concern (C*) and dismissed_concern (D*) ID must appear in 'source_ids' of at least one item in 'findings', 'hard_cases', or 'dismissed_concerns'.",
                missing.join(", ")
            ));
        }
    }

    Ok(())
}

pub fn validate_scan_post_verification_batch_items(
    output: &PostVerificationOutput,
    batch: &[Value],
) -> Result<(), String> {
    if output.findings.is_empty() && output.dismissed_concerns.is_empty() {
        return Err(
            "post-verification must not return both empty 'findings' and empty 'dismissed_concerns'. Every candidate hard case must be either validated in 'findings' or disproved in 'dismissed_concerns' with concrete code evidence in 'locations'."
                .to_string(),
        );
    }
    let expected_ids: Vec<&str> = batch
        .iter()
        .filter_map(|h| h.get("id").and_then(Value::as_str).map(str::trim))
        .filter(|id| !id.is_empty())
        .collect();
    let required = if expected_ids.is_empty() {
        batch.len().max(1)
    } else {
        1
    };
    let total = output.findings.len() + output.dismissed_concerns.len();
    if total < required {
        return Err(format!(
            "post-verification returned {total} item(s) across 'findings' and 'dismissed_concerns', but this batch has {required} candidate hard case(s). Every candidate in the batch must be accounted for in either 'findings' or 'dismissed_concerns'."
        ));
    }
    for (idx, finding) in output.findings.iter().enumerate() {
        validate_scan_finding_object(finding, idx)?;
    }
    for (idx, dismissed) in output.dismissed_concerns.iter().enumerate() {
        let has_desc = dismissed
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_reasoning = dismissed
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_desc || !has_reasoning {
            return Err(format!(
                "dismissed_concerns[{idx}] must be an object with non-empty 'description' and 'reasoning' strings."
            ));
        }
        if !has_valid_proof_location(dismissed) {
            return Err(format!(
                "dismissed_concerns[{idx}] must include a non-empty 'locations' array with at least one entry containing non-empty 'file', 'function_or_symbol', and verbatim disproving 'code_snippet'."
            ));
        }
    }
    if expected_ids.is_empty() {
        return Ok(());
    }

    let valid_label = expected_ids.join(", ");
    let mut accounted = std::collections::BTreeSet::new();

    for (collection_name, items) in [
        ("findings", &output.findings),
        ("dismissed_concerns", &output.dismissed_concerns),
    ] {
        for (idx, item) in items.iter().enumerate() {
            let sids = extract_non_empty_source_ids(collection_name, idx, item, &valid_label)?;
            for sid in sids {
                if !expected_ids.contains(&sid) {
                    return Err(format!(
                        "{collection_name}[{idx}].source_ids contains unknown hard case ID '{sid}'. Valid candidate ID(s) in this batch: {valid_label}."
                    ));
                }
                accounted.insert(sid);
            }
        }
    }

    let missing: Vec<&str> = expected_ids
        .into_iter()
        .filter(|id| !accounted.contains(id))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "post-verification output failed to account for candidate hard case ID(s): {}. Every candidate ID in the batch must appear in 'source_ids' of at least one item in 'findings' or 'dismissed_concerns'.",
            missing.join(", ")
        ));
    }
    Ok(())
}

pub(crate) fn sanitize_repo_relative_path(raw: &str) -> Option<&str> {
    let mut cleaned = raw.trim();
    while let Some(rest) = cleaned.strip_prefix("./") {
        cleaned = rest;
    }
    if cleaned.is_empty()
        || cleaned.starts_with('-')
        || cleaned.starts_with('/')
        || cleaned.contains('\0')
        || cleaned.chars().any(char::is_control)
    {
        return None;
    }
    let path = std::path::Path::new(cleaned);
    if !path
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(cleaned)
}

fn ensure_affected_source_files(finding: &mut Value, default_file: &str) {
    let mut files = Vec::new();
    if let Some(arr) = finding
        .get("affected_source_files")
        .and_then(Value::as_array)
    {
        for v in arr {
            if let Some(f) = v.as_str().and_then(sanitize_repo_relative_path) {
                push_unique_string(&mut files, f);
            }
        }
    }
    if let Some(locs) = finding.get("locations").and_then(Value::as_array) {
        for loc in locs {
            if let Some(f) = loc
                .get("file")
                .and_then(Value::as_str)
                .and_then(sanitize_repo_relative_path)
            {
                push_unique_string(&mut files, f);
            }
        }
    }
    if files.is_empty()
        && let Some(default_clean) = sanitize_repo_relative_path(default_file)
    {
        push_unique_string(&mut files, default_clean);
    }
    if let Some(map) = finding.as_object_mut() {
        map.insert("affected_source_files".to_string(), json!(files));
        map.insert("preexisting".to_string(), json!(true));
    }
}

pub fn is_high_or_critical_finding(finding: &Value) -> bool {
    finding
        .get("severity")
        .and_then(Value::as_str)
        .is_some_and(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "high" | "critical"))
}

pub(crate) fn record_verified_scan_findings(state: &mut LinuxScanState, findings: Vec<Value>) {
    for mut finding in findings {
        ensure_affected_source_files(&mut finding, &state.file_path);
        if is_high_or_critical_finding(&finding) {
            state.findings.push(finding);
        }
    }
}

pub fn apply_scan_verification_stage_output(
    state: &mut LinuxScanState,
    mut out: VerificationOutput,
    outcome: &crate::workflow::stage::StageOutcome,
) {
    let read_prompts = outcome.read_prompts();
    let mut temp_pv = PostVerificationOutput {
        findings: std::mem::take(&mut out.findings),
        dismissed_concerns: std::mem::take(&mut out.dismissed_concerns),
    };
    let mut combined_sources =
        Vec::with_capacity(state.all_concerns.len() + state.all_dismissed_concerns.len());
    combined_sources.extend_from_slice(&state.all_concerns);
    combined_sources.extend_from_slice(&state.all_dismissed_concerns);
    enrich_post_verification_output(
        &state.selected_guides,
        &combined_sources,
        &mut temp_pv,
        outcome,
        scan_analysis_stage_by_name,
    );
    out.findings = temp_pv.findings;
    out.dismissed_concerns = temp_pv.dismissed_concerns;

    for (idx, finding) in out.findings.iter_mut().enumerate() {
        ensure_affected_source_files(finding, &state.file_path);
        if let Some(map) = finding.as_object_mut() {
            let vf_id = format!("VF{}", idx.saturating_add(1));
            map.insert("id".to_string(), json!(vf_id));
            map.insert("stage_item_id".to_string(), json!(vf_id));
            map.insert("origin".to_string(), json!("verification"));
        }
    }
    for (idx, hard_case) in out.hard_cases.iter_mut().enumerate() {
        let (mut stages, mut prompts) =
            extract_item_stages_and_prompts(hard_case, scan_analysis_stage_by_name);
        for g in &state.selected_guides {
            push_unique_prompt(&mut prompts, g);
        }
        for p in &read_prompts {
            push_unique_prompt(&mut prompts, p);
        }
        if let Some(sids) = hard_case.get("source_ids").and_then(Value::as_array) {
            for sid in sids.iter().filter_map(Value::as_str) {
                for src in &combined_sources {
                    if src.get("id").and_then(Value::as_str) == Some(sid) {
                        let (src_stages, src_prompts) =
                            extract_item_stages_and_prompts(src, scan_analysis_stage_by_name);
                        for s in src_stages {
                            push_unique_string(&mut stages, &s);
                        }
                        for p in src_prompts {
                            push_unique_prompt(&mut prompts, &p);
                        }
                    }
                }
            }
        }
        if let Some(map) = hard_case.as_object_mut() {
            map.insert(
                "id".to_string(),
                json!(format!("H{}", idx.saturating_add(1))),
            );
            if !stages.is_empty() {
                map.insert("stage".to_string(), json!(stages[0]));
                map.insert("stages".to_string(), json!(stages));
            }
            map.insert("prompts".to_string(), json!(prompts));
        }
    }
    let planned_batches = batch_hard_cases_by_severity(&out.hard_cases);
    for (batch_idx, batch) in planned_batches.iter().enumerate() {
        let assigned_stage = POST_VERIFICATION_STAGE_NAMES[batch_idx];
        for batch_item in batch {
            if let Some(hid) = batch_item.get("id").and_then(Value::as_str) {
                for hc in &mut out.hard_cases {
                    if hc.get("id").and_then(Value::as_str) == Some(hid)
                        && let Some(map) = hc.as_object_mut()
                    {
                        map.insert("assigned_stage".to_string(), json!(assigned_stage));
                    }
                }
            }
        }
    }
    for (idx, dismissed) in out.dismissed_concerns.iter_mut().enumerate() {
        if let Some(map) = dismissed.as_object_mut() {
            map.insert(
                "id".to_string(),
                json!(format!("VD{}", idx.saturating_add(1))),
            );
            map.insert("origin".to_string(), json!("verification"));
        }
    }

    state.verification_findings.extend(out.findings.clone());
    state
        .verification_dismissed
        .extend(out.dismissed_concerns.clone());
    record_verified_scan_findings(state, out.findings);
    state.hard_cases = out.hard_cases;
    state
        .deduplicated_dismissed_concerns
        .extend(out.dismissed_concerns);
}

pub fn apply_scan_post_verification_stage_output(
    state: &mut LinuxScanState,
    stage_name: &str,
    batch: &[Value],
    mut out: PostVerificationOutput,
    outcome: &crate::workflow::stage::StageOutcome,
) {
    enrich_post_verification_output(
        &state.selected_guides,
        batch,
        &mut out,
        outcome,
        scan_analysis_stage_by_name,
    );
    for finding in &mut out.findings {
        ensure_affected_source_files(finding, &state.file_path);
        let pv_idx = state.post_verification_findings.len().saturating_add(1);
        let pvf_id = format!("PVF{pv_idx}");
        if let Some(map) = finding.as_object_mut() {
            map.insert("origin".to_string(), json!(stage_name));
            map.insert("post_verification_stage".to_string(), json!(stage_name));
            map.insert("id".to_string(), json!(pvf_id));
            map.insert("stage_item_id".to_string(), json!(pvf_id));
        }
        state.post_verification_findings.push(finding.clone());
    }
    for dismissed in &mut out.dismissed_concerns {
        let pvd_idx = state.post_verification_dismissed.len().saturating_add(1);
        let pvd_id = format!("PVD{pvd_idx}");
        if let Some(map) = dismissed.as_object_mut() {
            map.insert("id".to_string(), json!(pvd_id));
            map.insert("origin".to_string(), json!(stage_name));
            map.insert("post_verification_stage".to_string(), json!(stage_name));
        }
        state.post_verification_dismissed.push(dismissed.clone());
    }
    record_verified_scan_findings(state, out.findings);
    state
        .deduplicated_dismissed_concerns
        .extend(out.dismissed_concerns);
}

// ---------------------------------------------------------------------------
// Stage Builders
// ---------------------------------------------------------------------------

pub fn scan_prescreen_stage_with_effort(
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Stage<LinuxScanState, PrescreenOutput> {
    Stage::builder("pre-screen")
        .system_prompt(PromptTemplate::<LinuxScanState>::new(
            "You are an AI assistant preparing an active Linux kernel codebase security and bug scan.\nReview the target file path, function inventory, and source chunk below, and select all potentially relevant subsystem and pattern guides from the index.\nCRITICAL BIAS RULE: You MUST err on the side of inclusion. Only exclude a guide if it is 100% irrelevant to the target code.\n\nYou MUST respond with ONLY a JSON object, no other text. Example:\n```json\n{\"selected_prompts\": [\"networking-core.md\", \"locking.md\"]}\n```",
        ))
        .user_prompt(
            PromptTemplate::<LinuxScanState>::new(
                "<subsystem_guide_index>\n@include(\"subsystem/subsystem.md\")\n</subsystem_guide_index>\n\n<target_file>\nFile: {{file_path}} (lines {{start_line}}..={{end_line}})\nFunctions in chunk:\n{{chunk_functions_json}}\n\nSource:\n{{numbered_source}}\n</target_file>",
            )
            .with_var("file_path", |s: &LinuxScanState| s.file_path.clone())
            .with_var("start_line", |s: &LinuxScanState| s.start_line.to_string())
            .with_var("end_line", |s: &LinuxScanState| s.end_line.to_string())
            .with_var("chunk_functions_json", |s: &LinuxScanState| {
                s.chunk_functions_json.clone()
            })
            .with_var("numbered_source", |s: &LinuxScanState| {
                s.numbered_source.clone()
            })
            .include_file("subsystem/subsystem.md"),
        )
        .output_format(OutputFormat::json_with_schema(json!({
            "type": "object",
            "properties": {
                "selected_prompts": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            },
            "required": ["selected_prompts"]
        })))
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns: 1,
            temperature,
            reasoning_effort: effort.map(crate::ai::EffortMode::lightweight_stage_effort),
            ..Default::default()
        })
        .reduce(|state, out: PrescreenOutput| {
            let prompts: Vec<String> = out
                .selected_prompts
                .into_iter()
                .map(|name| normalize_guide_name(&name).to_string())
                .filter(|name| !is_scan_stage_exclusive_guide(name))
                .filter(|name| sanitize_guide_name(name))
                .collect();
            state.selected_guides = prompts;
        })
        .build()
}

pub fn validate_scan_triage_output(
    out: &ScanTriageOutput,
    _state: &LinuxScanState,
) -> Result<(), String> {
    for raw_stage in &out.relevant_stages {
        if scan_analysis_stage_by_name(raw_stage).is_none() {
            return Err(format!(
                "relevant_stages contains unknown stage {raw_stage:?}; expected one of execution-flow, resources, locking, security, hardware."
            ));
        }
    }
    for (raw_stage, raw_effort) in &out.stage_efforts {
        if scan_analysis_stage_by_name(raw_stage).is_none() {
            return Err(format!(
                "stage_efforts contains unknown stage {raw_stage:?}."
            ));
        }
        if crate::ai::ReasoningEffort::from_str_loose(raw_effort).is_none() {
            return Err(format!(
                "stage_efforts[{raw_stage:?}] has invalid effort {raw_effort:?}; expected \"medium\" or \"high\"."
            ));
        }
    }
    Ok(())
}

pub fn validate_scan_triage_output_with_effort(
    out: &ScanTriageOutput,
    state: &LinuxScanState,
) -> Result<(), String> {
    validate_scan_triage_output(out, state)?;
    for raw_stage in &out.relevant_stages {
        if let Some(def) = scan_analysis_stage_by_name(raw_stage) {
            let has_effort = out
                .stage_efforts
                .keys()
                .any(|k| scan_analysis_stage_by_name(k).is_some_and(|d| d.name == def.name));
            if !has_effort {
                return Err(format!(
                    "stage_efforts must provide an effort (\"medium\" or \"high\") for every stage in relevant_stages (missing {:?}).",
                    def.name
                ));
            }
        }
    }
    Ok(())
}

pub fn format_scan_triage_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. Respond with ONLY a valid JSON object containing \"relevant_stages\" and \"focus_targets\" (plus \"stage_efforts\" when requested).",
        violation
    )
}

pub fn scan_triage_stage_with_effort(
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Stage<LinuxScanState, ScanTriageOutput> {
    let dynamic_effort = effort == Some(crate::ai::EffortMode::Dynamic);
    let stage_names: Vec<&'static str> = SCAN_ANALYSIS_STAGES.iter().map(|d| d.name).collect();

    let focus_targets_schema = json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "function": { "type": "string" },
                "lines": { "type": "string" },
                "risk_category": { "type": "string" },
                "hypothesis": { "type": "string" },
                "callers_or_callees_to_check": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            },
            "required": ["function", "risk_category", "hypothesis"]
        }
    });

    let (user_prompt, output_format) = if dynamic_effort {
        let stage_efforts_schema = build_stage_efforts_schema(SCAN_ANALYSIS_STAGES);
        (
            r#"Perform a Phase 1 exploration and triage pass over the target kernel source chunk:
1. Select which specialist audit stages should run on this chunk (`relevant_stages`):
   - `execution-flow`: Complex branching, multi-label `goto` error unwinds, state mutations, pointer/bounds arithmetic.
   - `resources`: Allocations (`kmalloc`/`kzalloc`/pages/DMA), refcounts (`kref`/`get_device`/`fget`), subsystem registrations, teardown callbacks, timers/workqueues.
   - `locking`: Spinlocks, mutexes, RCU, atomics, lockless shared state, interrupt/timer synchronization.
   - `security`: User-space inputs (`copy_from_user`, `ioctl`, `netlink`, `sysfs`), packet/header parsing, length/offset calculations, capability checks, `copy_to_user`.
   - `hardware`: Device driver MMIO registers, DMA mappings, IRQ handlers, runtime PM / clocks, suspend/resume/reset state machines.
   CRITICAL EARLY-EXIT RULE: If this chunk consists purely of benign boilerplate (e.g., static tables, constant definitions, tracepoints, simple getters/setters, or trivial wrappers with no plausible High/Critical bug surface), return `"relevant_stages": []` and `"focus_targets": []`.
2. Identify the highest-risk functions and entry points in `focus_targets`, stating the specific `hypothesis` to test and which external `callers_or_callees_to_check` specialist stages should inspect with Git tools.
3. For each selected stage in `relevant_stages`, assign `"medium"` or `"high"` reasoning effort in `stage_efforts`.

Respond with ONLY a JSON object."#,
            OutputFormat::json_with_schema(json!({
                "type": "object",
                "properties": {
                    "relevant_stages": {
                        "type": "array",
                        "items": { "type": "string", "enum": stage_names }
                    },
                    "focus_targets": focus_targets_schema,
                    "stage_efforts": stage_efforts_schema,
                },
                "required": ["relevant_stages", "focus_targets", "stage_efforts"]
            }))
            .with_validator(validate_scan_triage_output_with_effort)
            .with_feedback_formatter(format_scan_triage_feedback),
        )
    } else {
        (
            r#"Perform a Phase 1 exploration and triage pass over the target kernel source chunk:
1. Select which specialist audit stages should run on this chunk (`relevant_stages`):
   - `execution-flow`: Complex branching, multi-label `goto` error unwinds, state mutations, pointer/bounds arithmetic.
   - `resources`: Allocations (`kmalloc`/`kzalloc`/pages/DMA), refcounts (`kref`/`get_device`/`fget`), subsystem registrations, teardown callbacks, timers/workqueues.
   - `locking`: Spinlocks, mutexes, RCU, atomics, lockless shared state, interrupt/timer synchronization.
   - `security`: User-space inputs (`copy_from_user`, `ioctl`, `netlink`, `sysfs`), packet/header parsing, length/offset calculations, capability checks, `copy_to_user`.
   - `hardware`: Device driver MMIO registers, DMA mappings, IRQ handlers, runtime PM / clocks, suspend/resume/reset state machines.
   CRITICAL EARLY-EXIT RULE: If this chunk consists purely of benign boilerplate (e.g., static tables, constant definitions, tracepoints, simple getters/setters, or trivial wrappers with no plausible High/Critical bug surface), return `"relevant_stages": []` and `"focus_targets": []`.
2. Identify the highest-risk functions and entry points in `focus_targets`, stating the specific `hypothesis` to test and which external `callers_or_callees_to_check` specialist stages should inspect with Git tools.

Respond with ONLY a JSON object. Example:
```json
{
  "relevant_stages": ["resources", "locking", "security"],
  "focus_targets": [
    {
      "function": "smb2_ioctl",
      "lines": "140-215",
      "risk_category": "resources",
      "hypothesis": "Early return when out_buf_len < sizeof(...) may leak allocated payload buffer",
      "callers_or_callees_to_check": ["smb2_allocate_rsp_buf", "ksmbd_iov_pin_rsp"]
    }
  ]
}
```"#,
            OutputFormat::json_with_schema(json!({
                "type": "object",
                "properties": {
                    "relevant_stages": {
                        "type": "array",
                        "items": { "type": "string", "enum": stage_names }
                    },
                    "focus_targets": focus_targets_schema,
                },
                "required": ["relevant_stages", "focus_targets"]
            }))
            .with_validator(validate_scan_triage_output)
            .with_feedback_formatter(format_scan_triage_feedback),
        )
    };

    Stage::builder("triage")
        .system_prompt(linux_scan_system_prompt())
        .user_prompt(PromptTemplate::<LinuxScanState>::new(user_prompt))
        .output_format(output_format)
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns: 1,
            temperature,
            reasoning_effort: effort.map(crate::ai::EffortMode::lightweight_stage_effort),
            ..Default::default()
        })
        .skip_if(|s| s.manual_stages.is_some())
        .reduce(move |state, out: ScanTriageOutput| {
            let mut stages = Vec::new();
            for raw_name in out.relevant_stages {
                if let Some(def) = scan_analysis_stage_by_name(&raw_name)
                    && !stages.iter().any(|s| s == def.name)
                {
                    stages.push(def.name.to_string());
                }
            }
            let mut efforts = std::collections::BTreeMap::new();
            if dynamic_effort {
                for (raw_stage, raw_effort) in out.stage_efforts {
                    if let Some(def) = scan_analysis_stage_by_name(&raw_stage)
                        && let Some(parsed) =
                            crate::ai::ReasoningEffort::from_str_loose(&raw_effort)
                    {
                        let clamped = match parsed {
                            crate::ai::ReasoningEffort::Low => crate::ai::ReasoningEffort::Medium,
                            other => other,
                        };
                        efforts.insert(def.name.to_string(), clamped);
                    }
                }
            }
            state.planned_stages = stages;
            state.planned_efforts = efforts;
            state.focus_targets = out.focus_targets;
        })
        .build()
}

fn scan_analysis_stage(
    def: &'static AnalysisStage,
    max_turns: usize,
    temperature: f32,
    reasoning_effort: Option<crate::ai::ReasoningEffort>,
) -> Box<dyn ExecutableStage<LinuxScanState>> {
    let mut user_template = PromptTemplate::<LinuxScanState>::new(format!(
        "{}\n\n{}",
        def.instruction, SCAN_STAGE_JSON_SCHEMA_INSTRUCTIONS
    ));
    for guide in def.guides {
        user_template = user_template.include_file(*guide);
    }

    Box::new(
        Stage::builder(def.name)
            .system_prompt(linux_scan_system_prompt())
            .user_prompt(user_template)
            .output_format(
                OutputFormat::json()
                    .with_validator(validate_scan_concerns_output)
                    .with_feedback_formatter(format_scan_concerns_feedback),
            )
            .policy(StagePolicy {
                tools: ToolScope::All,
                max_turns,
                temperature,
                reasoning_effort,
                ..Default::default()
            })
            .reduce_with_outcome(
                move |state: &mut LinuxScanState,
                      out: StageConcernsOutput,
                      outcome: &crate::workflow::stage::StageOutcome| {
                    let prompts =
                        collect_stage_prompts(&state.selected_guides, def.guides, outcome);
                    append_stage_items_with_prompts(
                        &mut state.all_concerns,
                        &out.concerns,
                        def.name,
                        "General",
                        &prompts,
                    );
                    append_stage_dismissed_concerns_with_prompts(
                        &mut state.all_dismissed_concerns,
                        &out.dismissed_concerns,
                        def.name,
                        &prompts,
                    );
                },
            )
            .build(),
    )
}

pub fn resolve_scan_stage_effort(
    state: &LinuxScanState,
    stage_name: &str,
) -> Option<crate::ai::ReasoningEffort> {
    state
        .effort
        .map(|mode| mode.analysis_stage_effort(state.planned_efforts.get(stage_name).copied()))
}

pub fn resolve_scan_analysis_stages_with_options(
    state: &LinuxScanState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<LinuxScanState>>> {
    let selected_stages: Vec<String> = if let Some(ref manual) = state.manual_stages {
        manual.clone()
    } else {
        // When `triage` ran and selected an empty `planned_stages`, honor the
        // early-exit decision by returning no specialist stages.
        state.planned_stages.clone()
    };

    let mut stages = Vec::new();
    for name in selected_stages {
        match scan_analysis_stage_by_name(&name) {
            Some(def) => {
                let effort = resolve_scan_stage_effort(state, def.name);
                stages.push(scan_analysis_stage(def, max_turns, temperature, effort));
            }
            None => tracing::warn!("Ignoring unknown scan stage {:?}", name),
        }
    }
    stages
}

const STAGE_SCAN_VERIFICATION_INSTRUCTION: &str = r#"# Scan Verification, Severity Calibration & Canonical Bug Formatting

You are the lead Linux kernel maintainer consolidating `concerns` and `dismissed_concerns` produced by parallel specialist scan stages on an existing kernel source chunk.
Your task is to (1) deduplicate overlapping items that share the same root cause and function, and (2) classify every unique candidate into:
- **Category 1a (`findings`):** Well-justified, uncontested bugs backed by concrete code proof.
- **Category 1b (`dismissed_concerns`):** Well-justified dismissals backed by a concrete disproving `code_snippet` in `locations` (with no competing `concern`).
- **Category 2 (`hard_cases`):** Contested (`C*` + `D*`), speculative, or assumption-based dismissals (`"signal_reason": "speculative_dismissal"`) requiring tool-based `post-verification` across callers/callees.

### CRITICAL SEVERITY & CANONICAL BUGREPO FORMAT REQUIREMENTS
Active codebase scanning targets ONLY serious (`High` or `Critical`) kernel defects.
- **LINEAGE REQUIREMENT (`source_ids`):** Every input item in Aggregated Concerns has an `"id"` (`"C1"`, `"C2"`, ...) and every input item in Aggregated Dismissed Concerns has an `"id"` (`"D1"`, `"D2"`, ...). Every output object across `findings`, `hard_cases`, and `dismissed_concerns` MUST include a non-empty `"source_ids"` array listing the exact input `"id"` string(s) merged into that item. Every input `"C*"` and `"D*"` ID MUST appear in at least one output item's `"source_ids"`.
- **Category 1a (`findings`) required keys:**
  1. `"source_ids"`: non-empty array of input `C*` IDs only (never `D*` IDs).
  2. `"problem"` (Canonical Title):
     - Format: `"<subsystem_prefix>: <defect or broken invariant in function_name()>"` (e.g. `"smb: server: memory leak in parse_durable_handle_context()"` or `"btrfs: use-after-free in btrfs_cleanup_ordered_extents()"`).
     - Strictly under 80 characters, NO backticks (`), NO markdown.
     - Do NOT start the description after `:` with patch/fix verbs (`fix`, `fixes`, `prevent`, `avoid`, `resolve`). State the defect directly.
  3. `"canonical_description"`:
     - A structured multi-line technical description containing all 4 canonical sections:
       `Trigger / Preconditions: ...`
       `Call Chain: ...`
       `Failure Mechanism: ...`
       `Impact: ...`
  4. `"severity"`: `"Low"`, `"Medium"`, `"High"`, `"Critical"`, or `"Unknown"` calibrated per `severity.md`. (Only `"High"` and `"Critical"` findings will be promoted to the bug repository.)
  5. `"severity_explanation"`: Detailed step-by-step proof of reachability, triggering conditions, and blast radius.
  6. `"affected_source_files"`: Non-empty array of repository-relative file paths (e.g. `["fs/smb/server/smb2pdu.c"]`).
  7. `"locations"`: Non-empty array of verified location objects (`file`, `function_or_symbol`, `line`, `code_snippet`, `why_this_location_matters`).
- **Category 2 (`hard_cases`) required keys:**
  1. `"source_ids"`: non-empty array of input `C*` and/or `D*` IDs.
  2. `"type"`: short defect category string.
  3. `"description"`: clear description of the candidate issue.
  4. `"estimated_severity"`: `"Low"`, `"Medium"`, `"High"`, `"Critical"`, or `"Unknown"`.
  5. `"signal_reason"`: `"mixed_signals"`, `"speculative_concern"`, `"speculative_dismissal"`, `"series_interaction"`, or `"other"`.
  6. `"concern_arguments"`: consolidated arguments for why the bug can occur.
  7. `"dismissal_arguments"`: consolidated arguments/snippets from any competing or standalone dismissal (or `""` if none).
  8. `"verification_question"`: specific code question that `post-verification` must answer using Git tools.
  9. `"locations"`: non-empty array of location objects (`file`, `function_or_symbol`, `line`, `code_snippet`, `why_this_location_matters`).
- **Category 1b (`dismissed_concerns`) required keys:**
  1. `"source_ids"`: non-empty array of input `D*` IDs only (never place a raised `C*` concern into `dismissed_concerns` in this stage).
  2. `"type"`: short category string.
  3. `"description"`: the concern that was investigated and dismissed.
  4. `"reasoning"`: concrete code explanation proving safety.
  5. `"locations"`: non-empty array of location objects (`file`, `function_or_symbol`, `line`, `code_snippet`, `why_this_location_matters`) with a non-empty verbatim disproving `code_snippet`."#;

pub fn scan_verification_stage_with_effort(
    max_turns: usize,
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Stage<LinuxScanState, VerificationOutput> {
    let user_template = PromptTemplate::<LinuxScanState>::new(format!(
        r#"{STAGE_SCAN_VERIFICATION_INSTRUCTION}

<false_positive_guide>
@include("false-positive-guide.md")
</false_positive_guide>

<severity_guidelines>
@include("severity.md")
</severity_guidelines>@includes

Aggregated Concerns:
{{{{aggregated_concerns}}}}

Aggregated Dismissed Concerns:
{{{{aggregated_dismissed_concerns}}}}

Return ONLY a JSON object with 'findings', 'hard_cases', and 'dismissed_concerns' arrays.
- Every input "C*" and "D*" ID MUST appear in at least one output item's "source_ids".
- Never place a raised concern ("C*") into 'dismissed_concerns' in this stage; route any questionable or contested concern into 'hard_cases'."#
    ))
    .include_file("false-positive-guide.md")
    .include_file("severity.md")
    .include_files_from_state(|s: &LinuxScanState| {
        let mut combined =
            Vec::with_capacity(s.all_concerns.len() + s.all_dismissed_concerns.len());
        combined.extend_from_slice(&s.all_concerns);
        combined.extend_from_slice(&s.all_dismissed_concerns);
        extra_prompt_paths_for_items(&s.selected_guides, &combined, scan_analysis_stage_by_name)
    })
    .with_var("aggregated_concerns", |s: &LinuxScanState| {
        serde_json::to_string_pretty(&s.all_concerns).unwrap_or_default()
    })
    .with_var("aggregated_dismissed_concerns", |s: &LinuxScanState| {
        serde_json::to_string_pretty(&s.all_dismissed_concerns).unwrap_or_default()
    });

    Stage::builder(SCAN_VERIFICATION.name)
        .system_prompt(linux_scan_system_prompt())
        .user_prompt(user_template)
        .output_format(
            OutputFormat::json()
                .with_validator(validate_scan_verification_output)
                .with_feedback_formatter(format_verification_stage_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            reasoning_effort: effort.map(crate::ai::EffortMode::verification_stage_effort),
            ..Default::default()
        })
        .reduce_with_outcome(|state, out: VerificationOutput, outcome| {
            apply_scan_verification_stage_output(state, out, outcome);
        })
        .build()
}

const STAGE_SCAN_POST_VERIFICATION_INSTRUCTION: &str = r#"# Per-Finding Deep Codebase Post-Verification & Canonical Bug Formatting

You are the lead Linux kernel maintainer performing deep, tool-assisted verification of a speculative or contested candidate issue (`hard_cases`) against the repository at `Target Revision SHA`.
1. Use `git_read_files`, `git_grep`, `git_blame`, and `git_show` at the target revision to answer each candidate's `verification_question` and verify ALL callers, callees, lock contexts, and lifecycle paths.
2. Every candidate `"H*"` ID in this batch MUST be accounted for in either `findings` or `dismissed_concerns` via a non-empty `"source_ids"` array.
3. To dismiss a candidate in `dismissed_concerns`, you MUST use the required keys:
   - `"source_ids"`: non-empty array of batch `"H*"` IDs.
   - `"description"`: the candidate issue that was disproved.
   - `"reasoning"`: step-by-step explanation of how the inspected code disproves the candidate.
   - `"locations"`: non-empty array of location objects (`file`, `function_or_symbol`, `line`, `code_snippet`, `why_this_location_matters`) quoting the verbatim disproving code proving the failure mode is impossible across all callers.
4. If the defect is real and reachable, emit it in `findings` using the exact canonical bug repository format:
   - `"source_ids"`: non-empty array of batch `"H*"` IDs.
   - `"problem"`: `"<subsystem_prefix>: <defect in function_name()>"` (strictly < 80 chars, NO backticks, NO fix-verbs like `fix`, `prevent`, `avoid`, `resolve` after `:`).
   - `"canonical_description"`: multi-line string containing `Trigger / Preconditions:`, `Call Chain:`, `Failure Mechanism:`, and `Impact:`.
   - `"severity"`: `"Low"`, `"Medium"`, `"High"`, `"Critical"`, or `"Unknown"` per `severity.md`.
   - `"severity_explanation"`: concrete step-by-step proof of reachability and impact.
   - `"affected_source_files"`: non-empty array of repository-relative file paths.
   - `"locations"`: non-empty array of verified location objects (`file`, `function_or_symbol`, `line`, `code_snippet`, `why_this_location_matters`)."#;

pub fn scan_post_verification_stage_with_effort(
    stage_name: &'static str,
    batch: Vec<Value>,
    max_turns: usize,
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Stage<LinuxScanState, PostVerificationOutput> {
    let candidate_json = serde_json::to_string_pretty(&batch).unwrap_or_default();
    let batch_for_prompts = batch.clone();
    let batch_for_validate = batch.clone();
    let batch_for_reduce = batch;

    let user_template = PromptTemplate::<LinuxScanState>::new(format!(
        r#"{STAGE_SCAN_POST_VERIFICATION_INSTRUCTION}

<false_positive_guide>
@include("false-positive-guide.md")
</false_positive_guide>

<severity_guidelines>
@include("severity.md")
</severity_guidelines>@includes

Candidate Hard Case(s) to Verify:
{{{{candidate_hard_cases}}}}

Return ONLY a JSON object with 'findings' and 'dismissed_concerns' arrays accounting for every candidate H* ID in this batch."#
    ))
    .include_file("false-positive-guide.md")
    .include_file("severity.md")
    .include_files_from_state(move |s: &LinuxScanState| {
        extra_prompt_paths_for_items(
            &s.selected_guides,
            &batch_for_prompts,
            scan_analysis_stage_by_name,
        )
    })
    .with_var("candidate_hard_cases", move |_: &LinuxScanState| {
        candidate_json.clone()
    });

    Stage::builder(stage_name)
        .system_prompt(linux_scan_system_prompt())
        .user_prompt(user_template)
        .output_format(
            OutputFormat::json()
                .with_validator(move |out, _state| {
                    validate_scan_post_verification_batch_items(out, &batch_for_validate)
                })
                .with_feedback_formatter(format_post_verification_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            reasoning_effort: effort.map(crate::ai::EffortMode::verification_stage_effort),
            ..Default::default()
        })
        .reduce_with_outcome(move |state, out: PostVerificationOutput, outcome| {
            apply_scan_post_verification_stage_output(
                state,
                stage_name,
                &batch_for_reduce,
                out,
                outcome,
            );
        })
        .build()
}

pub fn resolve_scan_post_verification_stages_with_options(
    state: &LinuxScanState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<LinuxScanState>>> {
    let effort = state.effort;
    let batches = batch_hard_cases_by_severity(&state.hard_cases);
    batches
        .into_iter()
        .take(MAX_POST_VERIFICATION_STAGES)
        .enumerate()
        .map(|(idx, batch)| {
            let stage_name = POST_VERIFICATION_STAGE_NAMES[idx];
            Box::new(scan_post_verification_stage_with_effort(
                stage_name,
                batch,
                max_turns,
                temperature,
                effort,
            )) as Box<dyn ExecutableStage<LinuxScanState>>
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Complete Linux Scan Workflow Graph
// ---------------------------------------------------------------------------

impl LinuxScanState {
    /// Converts confirmed `High` and `Critical` chunk findings into standard
    /// [`crate::api::BugInput`] payloads ready for the [`crate::workflows::linux_bug`]
    /// pipeline.
    pub fn to_candidate_bug_inputs(
        &self,
        maintainers: Option<&crate::maintainers::MaintainersIndex>,
    ) -> Vec<crate::api::BugInput> {
        let mut inputs = Vec::with_capacity(self.findings.len());
        for finding in &self.findings {
            let problem = finding
                .get("problem")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if problem.is_empty() {
                continue;
            }
            let canonical_desc = finding
                .get("canonical_description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let severity_expl = finding
                .get("severity_explanation")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let reasoning = match (!canonical_desc.is_empty(), !severity_expl.is_empty()) {
                (true, true) => {
                    format!("{canonical_desc}\n\nSeverity Explanation:\n{severity_expl}")
                }
                (true, false) => canonical_desc.to_string(),
                (false, true) => severity_expl.to_string(),
                (false, false) => String::new(),
            };

            let mut source_files: Vec<String> = finding
                .get("affected_source_files")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .filter_map(sanitize_repo_relative_path)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if source_files.is_empty()
                && let Some(default_clean) = sanitize_repo_relative_path(&self.file_path)
            {
                source_files.push(default_clean.to_string());
            }

            let subsystems: Vec<crate::db::AttributedSubsystem> = maintainers
                .map(|mindex| {
                    mindex
                        .match_files(&source_files)
                        .into_iter()
                        .map(crate::db::AttributedSubsystem::from_maintainers)
                        .collect()
                })
                .unwrap_or_default();

            inputs.push(crate::api::BugInput {
                problem,
                reasoning,
                locations: finding.get("locations").cloned(),
                subsystems,
                source_files,
                commit_sha: Some(self.target_sha.clone()),
                patchset_id: None,
                patch_id: None,
                baseline_sha: Some(self.target_sha.clone()),
                review_id: None,
            });
        }
        inputs
    }
}

pub fn build_linux_scan_workflow() -> Workflow<LinuxScanState> {
    build_linux_scan_workflow_with_options(20, 0.0)
}

pub fn build_linux_scan_workflow_with_options(
    max_turns: usize,
    temperature: f32,
) -> Workflow<LinuxScanState> {
    build_linux_scan_workflow_with_effort(max_turns, temperature, None)
}

pub fn build_linux_scan_workflow_with_effort(
    max_turns: usize,
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Workflow<LinuxScanState> {
    Workflow::builder("linux_scan")
        .stage(scan_prescreen_stage_with_effort(temperature, effort))
        .dynamic_parallel(
            scan_triage_stage_with_effort(temperature, effort),
            move |state| resolve_scan_analysis_stages_with_options(state, max_turns, temperature),
            ParallelPolicy::BestEffort,
        )
        .early_exit_if(
            |s| s.all_concerns.is_empty() && s.all_dismissed_concerns.is_empty(),
            "No concerns or dismissed concerns raised in scan specialist stages",
        )
        .dynamic_parallel(
            scan_verification_stage_with_effort(max_turns, temperature, effort),
            move |state| {
                resolve_scan_post_verification_stages_with_options(state, max_turns, temperature)
            },
            ParallelPolicy::BestEffort,
        )
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_stage_names_are_unique_and_resolvable() {
        let mut seen = std::collections::BTreeSet::new();
        for def in SCAN_ANALYSIS_STAGES {
            assert!(seen.insert(def.name), "duplicate stage name {}", def.name);
            assert!(is_known_scan_stage(def.name));
            assert!(std::ptr::eq(
                scan_analysis_stage_by_name(def.name).unwrap(),
                def
            ));
        }
        assert!(is_known_scan_stage("pre-screen"));
        assert!(is_known_scan_stage("triage"));
        assert!(is_known_scan_stage("verification"));
        assert!(is_known_scan_stage("post-verification-1"));
        assert!(!is_known_scan_stage("origin-tracing"));
        assert!(!is_known_scan_stage("report-1"));
        assert!(!is_known_scan_stage("nonexistent"));
    }

    #[test]
    fn test_validate_canonical_bug_title_enforces_bugrepo_rules() {
        assert!(
            validate_canonical_bug_title(
                "smb: server: memory leak in parse_durable_handle_context()"
            )
            .is_ok()
        );
        assert!(
            validate_canonical_bug_title("rust: sync: use-after-free in Arc::from_raw()").is_ok()
        );
        assert!(
            validate_canonical_bug_title("rust: sync: fix use-after-free in Arc::from_raw()")
                .is_err()
        );
        assert!(validate_canonical_bug_title("Arc::from_raw() use-after-free").is_err());
        assert!(validate_canonical_bug_title("smb:: memory leak in foo()").is_err());
        assert!(validate_canonical_bug_title("missing colon in title").is_err());
        assert!(validate_canonical_bug_title("smb: server: ").is_err());
        assert!(validate_canonical_bug_title("smb: : memory leak in foo()").is_err());
        assert!(validate_canonical_bug_title("smb: `backticks` in title()").is_err());
        assert!(
            validate_canonical_bug_title("smb: fix memory leak in parse_durable_handle_context()")
                .is_err()
        );
        assert!(
            validate_canonical_bug_title(
                "smb: server: fix memory leak in parse_durable_handle_context()"
            )
            .is_err()
        );
        assert!(
            validate_canonical_bug_title("smb: prevent use-after-free in foo_release()").is_err()
        );
        let too_long = format!("smb: {}", "a".repeat(80));
        assert!(validate_canonical_bug_title(&too_long).is_err());
    }

    #[test]
    fn test_sanitize_repo_relative_path_rejects_flags_and_traversal() {
        assert_eq!(
            sanitize_repo_relative_path("./fs/smb/server/smb2pdu.c"),
            Some("fs/smb/server/smb2pdu.c")
        );
        assert_eq!(sanitize_repo_relative_path("--output=/tmp/pwn"), None);
        assert_eq!(sanitize_repo_relative_path("-f"), None);
        assert_eq!(sanitize_repo_relative_path("/etc/passwd"), None);
        assert_eq!(sanitize_repo_relative_path("../secret.c"), None);
        assert_eq!(sanitize_repo_relative_path("fs/../secret.c"), None);
        assert_eq!(sanitize_repo_relative_path(""), None);
    }

    #[test]
    fn test_validate_scan_triage_output_with_effort_requires_stage_efforts() {
        let state = LinuxScanState::default();
        let empty_early_exit = ScanTriageOutput {
            relevant_stages: vec![],
            focus_targets: vec![],
            stage_efforts: std::collections::BTreeMap::new(),
        };
        assert!(validate_scan_triage_output_with_effort(&empty_early_exit, &state).is_ok());

        let missing_efforts = ScanTriageOutput {
            relevant_stages: vec!["resources".to_string(), "locking".to_string()],
            focus_targets: vec![],
            stage_efforts: std::collections::BTreeMap::from([(
                "resources".to_string(),
                "high".to_string(),
            )]),
        };
        assert!(validate_scan_triage_output(&missing_efforts, &state).is_ok());
        assert!(validate_scan_triage_output_with_effort(&missing_efforts, &state).is_err());

        let complete_efforts = ScanTriageOutput {
            relevant_stages: vec!["resources".to_string(), "locking".to_string()],
            focus_targets: vec![],
            stage_efforts: std::collections::BTreeMap::from([
                ("resources".to_string(), "high".to_string()),
                ("locking".to_string(), "medium".to_string()),
            ]),
        };
        assert!(validate_scan_triage_output_with_effort(&complete_efforts, &state).is_ok());
    }

    #[test]
    fn test_validate_canonical_bug_description_requires_sections() {
        let valid = "Trigger / Preconditions: Invalid sharename.\nCall Chain: smb2_open() -> parse_durable_handle_context().\nFailure Mechanism: Returns rc without freeing fp.\nImpact: Kernel memory leak.";
        assert!(validate_canonical_bug_description(valid).is_ok());
        assert!(validate_canonical_bug_description("Missing sections").is_err());
    }

    #[test]
    fn test_record_verified_scan_findings_filters_to_high_and_critical() {
        let mut state = LinuxScanState {
            file_path: "fs/smb/server/smb2pdu.c".to_string(),
            target_sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
            ..Default::default()
        };
        let findings = vec![
            json!({
                "id": "VF1",
                "problem": "smb: minor issue in foo()",
                "canonical_description": "Trigger: x\nFailure Mechanism: y\nImpact: z",
                "severity": "Medium",
                "severity_explanation": "Medium issue",
                "locations": [{"file": "fs/smb/server/smb2pdu.c", "function_or_symbol": "foo", "code_snippet": "x();"}]
            }),
            json!({
                "id": "VF2",
                "problem": "smb: use-after-free in bar()",
                "canonical_description": "Trigger: x\nFailure Mechanism: y\nImpact: z",
                "severity": "High",
                "severity_explanation": "High UAF",
                "locations": [{"file": "fs/smb/server/smb2pdu.c", "function_or_symbol": "bar", "code_snippet": "kfree(p);"}]
            }),
        ];
        record_verified_scan_findings(&mut state, findings);
        assert_eq!(state.findings.len(), 1);
        assert_eq!(state.findings[0]["problem"], "smb: use-after-free in bar()");
        assert_eq!(
            state.findings[0]["affected_source_files"],
            json!(["fs/smb/server/smb2pdu.c"])
        );

        let inputs = state.to_candidate_bug_inputs(None);
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].problem, "smb: use-after-free in bar()");
        assert!(inputs[0].reasoning.contains("Failure Mechanism: y"));
        assert!(inputs[0].reasoning.contains("High UAF"));
        assert_eq!(
            inputs[0].source_files,
            vec!["fs/smb/server/smb2pdu.c".to_string()]
        );
        assert_eq!(
            inputs[0].commit_sha.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
    }

    #[test]
    fn test_triage_empty_relevant_stages_resolves_zero_specialist_stages() {
        let state = LinuxScanState::default();
        let stages = resolve_scan_analysis_stages_with_options(&state, 10, 0.0);
        assert!(stages.is_empty());
    }

    #[test]
    fn test_linux_scan_system_prompt_interpolates_maintainers_hint() {
        let state = LinuxScanState {
            chunk_id: "fs/smb/server/smb2pdu.c:1-200".to_string(),
            file_path: "fs/smb/server/smb2pdu.c".to_string(),
            target_sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
            start_line: 1,
            end_line: 200,
            maintainers_hint: Some(
                "COMMON INTERNET FILE SYSTEM CLIENT (CIFS and SMB3)".to_string(),
            ),
            ..Default::default()
        };
        let rendered = linux_scan_system_prompt().render_for_log(&state);
        assert!(
            rendered.contains("MAINTAINERS Subsystem(s): COMMON INTERNET FILE SYSTEM CLIENT"),
            "rendered system prompt should contain interpolated maintainers_hint: {rendered}"
        );
        assert!(!rendered.contains("{{maintainers_block}}"));
    }

    #[test]
    fn test_validate_scan_post_verification_batch_items_accepts_finding_without_preexisting() {
        let batch = vec![json!({
            "id": "H1",
            "type": "Resource Leak",
            "description": "Potential leak on error path",
            "estimated_severity": "High",
            "signal_reason": "cross_file",
            "concern_arguments": "Allocated buffer not freed",
            "dismissal_arguments": null,
            "verification_question": "Does caller free buf on error?",
            "locations": [{
                "file": "fs/smb/server/smb2pdu.c",
                "function_or_symbol": "smb2_ioctl()",
                "line": 120,
                "code_snippet": "return -EINVAL;",
                "why_this_location_matters": "Early return"
            }]
        })];
        let output = PostVerificationOutput {
            findings: vec![json!({
                "source_ids": ["H1"],
                "problem": "smb: memory leak in smb2_ioctl() on error path",
                "canonical_description": "Trigger / Preconditions: Invalid ioctl request.\nCall Chain: smb2_ioctl().\nFailure Mechanism: Early return skips kfree(buf).\nImpact: Unbounded kernel memory leak.",
                "severity": "High",
                "severity_explanation": "Unprivileged client can trigger repeated allocations.",
                "affected_source_files": ["fs/smb/server/smb2pdu.c"],
                "locations": [{
                    "file": "fs/smb/server/smb2pdu.c",
                    "function_or_symbol": "smb2_ioctl()",
                    "line": 120,
                    "code_snippet": "return -EINVAL;",
                    "why_this_location_matters": "Returns without freeing buf"
                }]
            })],
            dismissed_concerns: vec![],
        };
        assert!(validate_scan_post_verification_batch_items(&output, &batch).is_ok());
    }
}
