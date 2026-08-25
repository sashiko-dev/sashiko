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

//! Single declarative workflow definition for Sashiko's Linux Kernel Code Review.
//!
//! This module specifies the multi-stage review pipeline as a declarative [`Workflow`]
//! operating over [`KernelReviewState`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::workflow::graph::Workflow;
use crate::workflow::output::OutputFormat;
use crate::workflow::policy::{ParallelPolicy, RecitationPolicy, StagePolicy, ToolScope};
use crate::workflow::prompt::PromptTemplate;
use crate::workflow::stage::{ExecutableStage, Stage};

/// Subsystem guides that are loaded per-stage and should be excluded
/// from Phase 0 shared context to avoid redundant token usage.
pub const STAGE_EXCLUSIVE_GUIDES: &[&str] = &["locking.md"];

/// Complete execution state of a Linux kernel patch review.
#[derive(Clone, Debug, Default)]
pub struct KernelReviewState {
    pub ps_id: String,
    pub p_id: String,
    pub target_commit_sha: String,
    pub baseline_sha: String,
    pub target_commit_diff: String,
    pub target_commit_diff_only: String,
    pub prefetched_context: String,
    pub series_range: Option<String>,
    pub follow_up_series_context: Option<String>,

    /// Subsystem guide markdown files selected during Phase 0 pre-screen.
    pub selected_guides: Vec<String>,
    /// Optional manual stages filter (e.g. `--stages 1,2,5`).
    pub manual_stages: Option<Vec<u8>>,
    /// Stages selected by dynamic planning (or overridden by manual_stages).
    pub planned_stages: Vec<u8>,

    /// Aggregated raw concerns collected from Stages 1-7.
    pub all_concerns: Vec<Value>,
    /// Aggregated raw dismissed concerns collected from Stages 1-7.
    pub all_dismissed_concerns: Vec<Value>,

    /// Deduplicated concerns from Stage 8.
    pub deduplicated_concerns: Vec<Value>,
    /// Deduplicated dismissed concerns from Stage 8.
    pub deduplicated_dismissed_concerns: Vec<Value>,

    /// Filtered concerns after Stage 9 conflict resolution.
    pub conflict_resolved_concerns: Vec<Value>,

    /// Verified findings from Stage 10.
    pub findings: Vec<Value>,

    /// Generated LKML plain-text review from Stage 11.
    pub review_inline: String,
    /// Fix suggestions.
    pub fixes: String,
}

// ---------------------------------------------------------------------------
// Typed Output Structures for Stage Serialization
// ---------------------------------------------------------------------------

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Phase0Output {
    pub selected_prompts: Vec<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct PlanningOutput {
    pub relevant_stages: Vec<u8>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct StageConcernsOutput {
    #[serde(default)]
    pub concerns: Vec<Value>,
    #[serde(default)]
    pub dismissed_concerns: Vec<Value>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct Stage9Output {
    #[serde(default)]
    pub concerns: Vec<Value>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct Stage10Output {
    #[serde(default)]
    pub findings: Vec<Value>,
}

// ---------------------------------------------------------------------------
// Common System Prompt Template
// ---------------------------------------------------------------------------

pub fn kernel_system_prompt(use_log: bool) -> PromptTemplate<KernelReviewState> {
    let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();
    let diff_var = if use_log {
        "{{target_commit_diff}}"
    } else {
        "{{target_commit_diff_only}}"
    };

    PromptTemplate::<KernelReviewState>::new(format!(
        r#"Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references (e.g., 'today', 'last week', 'next year') strictly on this current date.

You are an expert Linux kernel maintainer. Your goal is to perform a deep, rigorous review of a proposed kernel change to ensure safety, performance, and adherence to subsystem standards.

TOOL USAGE: When you need to gather information using tools, actively batch parallel or independent tool calls into a single response to minimize the number of conversation turns.

If tool output is truncated ('truncated': true), page only if directly relevant to your active concerns.

<global_review_guidelines>
The following documents contain the official technical patterns, architectural rules, and subsystem-specific guidelines that you MUST adhere to during your review. Use these as the absolute source of truth for identifying anti-patterns and violations.
</global_review_guidelines>

=== Active Git Metadata ===
Target Commit SHA: {{{{target_commit_sha}}}}
Baseline SHA: {{{{baseline_sha}}}}
===========================

Target Commit:
{diff_var}
{{{{prefetched_block}}}}"#
    ))
    .with_var("target_commit_sha", |s: &KernelReviewState| s.target_commit_sha.clone())
    .with_var("baseline_sha", |s: &KernelReviewState| s.baseline_sha.clone())
    .with_var("target_commit_diff", |s: &KernelReviewState| s.target_commit_diff.clone())
    .with_var("target_commit_diff_only", |s: &KernelReviewState| s.target_commit_diff_only.clone())
    .with_var("prefetched_block", |s: &KernelReviewState| {
        if s.prefetched_context.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n<pre_fetched_context>\nThe following context was automatically pre-fetched based on the modified lines in the patch. It contains the full source code of the functions and structs modified by the diff AFTER applying the target patch.\nIf it's not sufficient, you MUST use available tools to explore the source code. Don't make assumptions without actually looking into the relevant code.\n\n{}\n</pre_fetched_context>",
                s.prefetched_context
            )
        }
    })
    .include_files_from_state(|s: &KernelReviewState| {
        let mut paths = Vec::new();
        if !s.selected_guides.is_empty() {
            for guide in &s.selected_guides {
                paths.push(PathBuf::from("subsystem").join(guide));
                paths.push(PathBuf::from("patterns").join(guide));
            }
        }
        paths
    })
}

// ---------------------------------------------------------------------------
// Stage Builders
// ---------------------------------------------------------------------------

const STAGE_1_INSTRUCTION: &str = r#"# Stage 1. Analyze commit main goal

You are a senior Linux kernel maintainer evaluating the high-level intent of a proposed commit. Analyze the commit message and the conceptual change. Focus on the big picture: Are there architectural flaws, UAPI breakages, backwards compatibility issues, or fundamentally flawed concepts? Consider the long-term maintainability and system-wide implications of this design. If the core idea is dangerous, incorrect, or violates established kernel principles, raise a concern. Be open-minded but thorough; question assumptions made by the author and consider alternative, simpler designs."#;

const STAGE_2_INSTRUCTION: &str = r#"# Stage 2. High-level implementation verification

You are verifying if the provided code changes actually implement what the commit message claims. Look for undocumented side-effects, missing pieces (e.g., a core change without updating corresponding callers, or changing a struct without updating all initializers), and unhandled corner cases related to the feature's logic. Explicitly check for missing API callbacks and interface omissions: when defining or modifying structures containing function pointers, verify that all logically required callbacks are implemented. Verify that all claims in the commit message are fully realized in the code. Identify any incomplete implementations, implicit behavioral changes, or API contract violations. Furthermore, verify that the logic is mathematically and semantically sound. Check for off-by-one errors in bounds, incorrect bitwise operations, and verify that all arguments passed to external subsystems (like kobjects or netdevs) are valid and semantically correct (e.g., non-empty strings, correct sizes, correct format specifiers). Don't trust the commit message without verifying each claim. Assume that the message might be incorrect or even intentionally malicious. Do not focus on low-level memory or locking errors yet."#;

const STAGE_3_INSTRUCTION: &str = r#"# Stage 3. Execution flow verification

You are a static analysis engine tracing execution flow in C or Rust code. Carefully trace the control flow of the provided patch. Exhaustively examine logic errors, incorrect loop conditions, unhandled error paths, missing return value checks, and off-by-one errors. Check every branch, switch statement, and conditional. Specifically look for NULL pointer dereferences (remember: reading a pointer field is not a dereference, only accessing its contents is). Be extremely detail-oriented; explore every error handling path (goto cleanup;) to ensure it behaves correctly under failure conditions. Additionally, verify preprocessor macro correctness and spelling (e.g., ensuring CONFIG_ prefixes are used where expected instead of HAVE_). Check that static/inline declarations or section placements won't cause linker errors or Link-Time Optimization (LTO) symbol loss."#;

const STAGE_4_INSTRUCTION: &str = r#"# Stage 4. Resource management

You are an expert in C and Rust resource management within the Linux kernel. Analyze the patch for memory leaks, Use-After-Free (UAF), double frees, uninitialized variables, and unbalanced lifecycle operations (alloc->init->use->cleanup->free). Pay special attention to error paths where resources might be leaked. Ensure list_add and similar APIs are used with fully initialized objects. Track the lifetime of every allocated struct and file descriptor. Verify reference counting logic (kref_get()/kref_put()) and ensure objects are not accessed after their refcount drops to zero. Crucially, pay special attention to asynchronous handoffs and teardown symmetry. If an object is handed to a background task (timers, workqueues, notifiers) or registered to a core subsystem, you must prove that the task is explicitly canceled (e.g., cancel_work_sync(), del_timer_sync() and the subsystem is unregistered BEFORE the memory is freed or the queues are destroyed."#;

const STAGE_5_INSTRUCTION: &str = r#"# Stage 5. Locking and synchronization

You are a world-class concurrency and locking expert auditing a Linux kernel patch.
Carefully review the proposed patch for ANY locking, concurrency, or synchronization bugs.
You MUST consider the following categories of issues and report any violations:
1. Sleeping in atomic context: Are there any calls to `mutex_lock`, `kzalloc` with `GFP_KERNEL`, `msleep`, `cond_resched`, `flush_workqueue`, `synchronize_rcu`, or `cancel_work_sync` while holding a spinlock, rwlock, or within an RCU read-side critical section (`rcu_read_lock`)?
2. Lock ordering and deadlocks: Are locks acquired in a different order than elsewhere? Does it acquire a mutex while holding another mutex that could cause AB-BA deadlocks? Are IRQs disabled (`spin_lock_irqsave`) when acquiring a lock that is used in hardirq context? Does it acquire a lock already held by a higher-level subsystem (e.g., ethtool)?
3. Race conditions and lockless access: Are shared variables, list entries, or pointers accessed without holding the appropriate lock? Are there missing memory barriers (`smp_mb`, `smp_wmb`, `smp_rmb`) when lockless access is intended? Are there TOCTOU races where a state is checked outside a lock but relied upon inside?
4. UAF / Locking Freed Memory: Are locks (`mutex_unlock`, `spin_unlock`) called on objects that have already been freed? Are works/timers destroyed before subsystems are unregistered, allowing new events to use freed works/timers? Is the protocol initialized flag set before private data is ready?
5. RCU rules: Is `list_splice_init` or similar non-RCU-safe operations used on RCU-protected lists? Is `list_for_each_rcu` used without `rcu_read_lock`?
6. Unprotected state modifications: Does the patch check state before acquiring the lock (e.g., checking power state before taking mutex)? Are hardware state, flags, or stats updated without proper protection?
7. Sequence counters: Are stats accumulations directly inside a `u64_stats_fetch_retry` loop leading to double counting? Is it possible for an interrupt to read a sequence counter while the interrupted context is modifying it (deadlock)?
8. Lock re-initialization: Does it re-initialize a lock that was already initialized, or destroy a lock on a failure path improperly?
9. Missing locking: Is a port or file exposed to userspace before the driver/TTY linking is complete? Does a worker race with cleanup code leading to dropped/leaked frames?"#;

const STAGE_6_INSTRUCTION: &str = r#"# Stage 6. Security audit

You are a Red Team security researcher auditing a Linux kernel patch. Look for security vulnerabilities such as buffer overflows, out-of-bounds reads/writes, integer overflows, privilege escalation vectors, time-of-check to time-of-use (TOCTOU) races, and information leaks (e.g., copying uninitialized kernel memory to user-space via copy_to_user). Scrutinize all points where untrusted user input reaches sensitive functions without validation. Ensure all length checks and bounds checks are robust against malicious input. Focus heavily on attack surfaces and data boundaries."#;

const STAGE_7_INSTRUCTION: &str = r#"# Stage 7. Hardware engineer's review

You are a hardware engineer reviewing device driver changes. If this patch touches driver or hardware-specific code, rigorously review register accesses, IRQ handling, DMA mapping/unmapping, memory barriers, and timing/delays. Look for missing dma_wmb()/dma_rmb() barriers, incorrect endianness conversions (cpu_to_le32), and unsafe DMA buffer allocations. Ensure the hardware state machine is handled correctly, especially during suspend/resume or device reset. Evaluate the physical state machine constraints: verify that clocks and power domains are enabled before registers are accessed, and that hardware rings/queues are actually initialized in the current hardware state before being unconditionally accessed. If the patch is purely generic software logic (e.g., VFS, core networking), return {"concerns": [], "dismissed_concerns": []}."#;

const STAGE_8_INSTRUCTION: &str = r#"# Stage 8. Deduplication and Consolidation

You are the lead reviewer consolidating feedback from multiple specialized analysts. You will be given lists of concerns and dismissed_concerns generated by different review stages.
Your task is to deduplicate identical or overlapping items in both lists.
1. Group concerns that refer to the same root cause or the same line of code.
2. Merge overlapping concerns into a single, comprehensive concern. Combine their reasonings if they complement each other.
3. Group dismissed_concerns that investigated and disproved the same candidate concern.
4. Merge overlapping dismissed_concerns into a single, comprehensive dismissed_concern. Combine their evidence if it complements each other.
5. Ensure the output contains only unique concerns and unique dismissed_concerns.
6. Preserve the `preexisting` flag for concerns. If you merge a pre-existing concern with a newly introduced one, flag it based on the root cause (if the root cause is new, it's not pre-existing).
7. SPECIFICITY REQUIREMENT: When merging concerns or dismissed_concerns, preserve and consolidate the most specific details: exact function names, file paths, line numbers when known, and triggering conditions. Never generalize a specific finding into a vague category.
8. Preserve and merge the `locations` arrays from the input concerns and dismissed_concerns. If multiple items describe the same root cause, keep the most precise file/function_or_symbol/line/code_snippet/why_this_location_matters locations. Do not invent line numbers; keep `line` as null when the exact line is not known.
9. dismissed_concerns do not need a `preexisting` flag."#;

const STAGE_9_INSTRUCTION: &str = r#"# Stage 9. Concern/dismissed-concern conflict resolution

You are the lead reviewer reconciling consolidated concerns with consolidated dismissed_concerns.
Both `concerns` and `dismissed_concerns` are untrusted claims. Do not assume either side is correct. Treat both as hypotheses and verify them against the actual code before deciding whether to keep or discard a concern.
Your task is to identify whether any remaining concern conflicts with a dismissed_concern that investigated the same root cause, code path, or failure mode.
1. Compare each concern against the dismissed_concerns list and find conflicts or overlaps where one says the issue is real and the other says the same candidate issue is disproved.
2. For every conflict, inspect the actual code and reasoning to decide which side is correct.
3. If the concern is correct, keep it in the output. If the dismissed_concern is correct, discard that concern.
4. If there is no direct conflict for a concern, keep it unchanged.
5. Do not discard a concern merely because a dismissed_concern is vaguely related; only discard when the dismissed_concern's evidence concretely disproves that concern.
6. Preserve each retained concern's `type`, `description`, `reasoning`, `preexisting`, and `locations` fields.
7. LOCAL BOUNDARY RULE: Do not discard a defect within the modified code of the patch by assuming that surrounding caller systems, parallel execution, or legacy API layers will safely mask or prevent the issue, unless you can point to specific code that concretely proves the failure mode is structurally impossible. If you cannot prove the safety of the violation based on the specific code, you must keep the concern."#;

const STAGE_10_INSTRUCTION: &str = r#"# Stage 10. Verification and severity estimation

You are the lead reviewer validating consolidated concerns. You will be given a list of deduplicated concerns after conflict resolution.
1. Validate each concern and prove the provided reasoning. Report all valid concerns as findings. If necessary, use tools to gather additional material. Discard all false positives.
2. CRITICAL RULE: To discard a concern as a false positive, you MUST find concrete proof that explicitly invalidates the concern's reasoning. If you cannot find definitive proof that the concern is a false positive, it must be reported as a finding. If you're not sure about something and it's critical in the reasoning validation, make it obvious: if X is possible, then problem Y can occur. Always try to validate if X is possible yourself.
3. SERIES VALIDATION RULE: If follow-up patches in this series are provided in the context, check if each identified concern is resolved or fixed in the final state of the series. If the problem has been resolved, fixed, or the code was rewritten in a subsequent patch in this series, you MUST discard the concern and NOT report it as a finding. You MUST verify this by checking the actual code at the end of the series using tools; do not trust promises or claims in commit messages.
4. When referring to other patches within this series in your explanation, DO NOT use git hashes (they are ephemeral/unstable). Instead, refer to them by their patch subject (e.g., 'commit "mm: fix allocation"'). Existing historical commits in the tree should still be referenced by their standard hash.
5. Assign a severity (low, medium, high, critical) to each remaining valid finding, following the calibration guidance in the severity definitions: reason through consequence, triggering path, and reachability, and state that reasoning at the start of the finding's `severity_explanation` so the label is auditable. Raise the level for a bug reachable by untrusted or remote input, and do not lower it because you believe the code is unreachable. A finding you can only state speculatively is capped at medium but still reported, never dropped. Be rigorous in filtering out verifiable noise, but accurately report real logic flaws and edge cases.
6. If the problem did exist in the code before the patch was applied, say it explicitly: 'This problem wasn't introduced by this patch, but...'. Discard low- and medium-severity pre-existing problems, report only high- and critical severity issues.
7. SPECIFICITY REQUIREMENT: Every finding MUST cite the exact function name(s), file path(s), line number(s) when known, and triggering conditions where the bug manifests. Vague descriptions like 'potential overflow in ring buffer calculations' are insufficient. State precisely which variable overflows, in which function, and under what input conditions. Do not invent line numbers; use `line: null` when the exact line is not known.
8. Carry forward the `locations` from the validated concern into each finding. If you gather better evidence, replace vague locations with the most precise verified locations. Do not invent line numbers; use null when exact values are unknown."#;

const STAGE_11_INSTRUCTION: &str = r#"# Stage 11. LKML-friendly report generation

You are an automated review bot generating a report for the Linux Kernel Mailing List (LKML). Convert the provided JSON findings into a polite, standard, inline-commented LKML email reply.

CRITICAL RULE: If a finding is flagged as pre-existing (`"preexisting": true`), you MUST explicitly state in your inline comment that this issue is pre-existing and was not introduced by the patch under review. Use phrasing like "This isn't a bug introduced by this patch, but..." or "This is a pre-existing issue, but..." to start the comment.

Follow the formatting rules strictly. Do not use markdown headers or ALL CAPS shouting. Ensure the tone is constructive and professional. Do not use backticks to quote any names or expressions.

SPECIFICITY REQUIREMENT: Each inline comment MUST reference the exact function name, file, line number when known, and specific triggering condition. Prefer the finding's `locations` field when present. Do not produce vague summaries like 'potential issue in error handling'. State precisely what goes wrong, where, and under what circumstances. Do not invent line numbers; if the exact line is unavailable, anchor the comment to the nearest verified function or symbol and explain the triggering condition."#;

const STAGE_JSON_SCHEMA_EXAMPLE: &str = r#"
TodoWrite compatibility: vendored prompts may ask you to add tasks or suspected bugs to TodoWrite. Do not call or mention TodoWrite. Treat those instructions as an internal checklist only. If that checklist identifies a concrete suspected bug, carry it forward as a JSON concern with file, function_or_symbol, line when known, triggering condition, and evidence. Do not output generic checklist progress as a concern.

Once you have gathered sufficient information, return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
If you find no concerns and no dismissed concerns, return {"concerns": [], "dismissed_concerns": []}.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
- "type": A short category string.
- "description": A clear description of the problem.
- "reasoning": A step-by-step explanation.
- "preexisting": true if this bug already existed in the codebase before these patches were applied, false if the issue was newly introduced by the reviewed patchset.
- "locations": An array of objects, each containing "file", "function_or_symbol", "line", "code_snippet" and "why_this_location_matters".
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations". They mean the same as above, except that "description" is the candidate concern that was investigated and disproved, and "reasoning" is the evidence proving it does not apply.

Use the 'dismissed_concerns' array ONLY for candidate concerns that you considered plausible, investigated, and disproved with concrete evidence. This is especially important when you first suspect a concern and then follow the evidence chain proving that it does NOT apply.

SPECIFICITY REQUIREMENT: When reporting a concern or dismissed_concern, cite exact function name(s), file path(s), and line number(s) when known. Do not invent line numbers; use null when exact values are unknown.

CRITICAL REVIEW DIRECTIVE: Do NOT dismiss concerns just because you assume the surrounding system or caller handles it perfectly. Do not be overly charitable to the existing code. If there is a missing initialization, an unhandled edge case, or a brittle logic flow, report it as a concern immediately. Assume the worst-case scenario where external inputs and caller states are malformed.

Example Output:
```json
{
  "concerns": [
    {
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }
      ]
    }
  ],
  "dismissed_concerns": [
    {
      "type": "Resource Management",
      "description": "Possible missing cleanup when foo_init() fails after bar_alloc().",
      "reasoning": "The concrete code path or ordering that proves this candidate concern does not apply.",
      "locations": [
        {
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 125,
          "code_snippet": "safe_code_path();",
          "why_this_location_matters": "This is where the cleanup path proves the candidate leak does not apply."
        }
      ]
    }
  ]
}
```"#;

// ---------------------------------------------------------------------------
// Validation Logic
// ---------------------------------------------------------------------------

fn validate_stages_1_to_8(
    _output: &StageConcernsOutput,
    _state: &KernelReviewState,
) -> Result<(), String> {
    Ok(())
}

fn format_stages_1_to_8_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. You MUST return ONLY a JSON object containing 'concerns' and 'dismissed_concerns' arrays. If there are no concerns and no dismissed concerns, return `{{\"concerns\": [], \"dismissed_concerns\": []}}`.",
        violation
    )
}

fn validate_inline_format(content: &str, _state: &KernelReviewState) -> Result<(), String> {
    if content.lines().any(|l| l.trim_start().starts_with("```")) {
        return Err("The output contains Markdown code blocks ('```'). It must be plain text as per `inline-template.md`.".to_string());
    }
    if !content.lines().any(|l| l.trim_start().starts_with('>')) {
        return Err("The output does not appear to quote any code or context using '>'. Please follow the quoting style in `inline-template.md`.".to_string());
    }
    let has_commit_header = content
        .lines()
        .take(20)
        .any(|l| l.trim_start().to_lowercase().starts_with("commit "));
    if !has_commit_header {
        return Err("The output is missing the 'commit <hash>' header. Please start with the commit details (Commit, Author, Subject) as per `inline-template.md`.".to_string());
    }
    let has_author_header = content
        .lines()
        .take(20)
        .any(|l| l.trim_start().to_lowercase().starts_with("author:"));
    if !has_author_header {
        return Err("The output is missing the 'Author: <name>' header. Please start with the commit details (Commit, Author, Subject) as per `inline-template.md`.".to_string());
    }
    let has_comments = content.lines().any(|l| {
        let trimmed = l.trim();
        if trimmed.is_empty() || trimmed.starts_with('>') {
            return false;
        }
        let lower = trimmed.to_lowercase();
        !lower.starts_with("commit ")
            && !lower.starts_with("author:")
            && !lower.starts_with("date:")
            && !lower.starts_with("link:")
    });
    if !has_comments {
        return Err("The output appears to lack any comments or summary. You must include a summary and interspersed comments explaining the findings.".to_string());
    }
    Ok(())
}

fn format_inline_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. Please fix the formatting to match the standard plain text LKML review format with proper headers and '> ' quoted context.",
        violation
    )
}

fn append_stage_items(
    dest: &mut Vec<Value>,
    src: &[Value],
    stage_num: u8,
    default_type: &str,
    _key: &str,
) {
    for item in src {
        let mut obj = item.clone();
        if let Some(map) = obj.as_object_mut() {
            if !map.contains_key("type")
                || map
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .is_empty()
            {
                map.insert("type".to_string(), json!(default_type));
            }
            map.insert("stage".to_string(), json!(stage_num));
        }
        dest.push(obj);
    }
}

fn append_stage_dismissed_concerns(dest: &mut Vec<Value>, src: &[Value], stage_num: u8) {
    for item in src {
        let mut obj = item.clone();
        if let Some(map) = obj.as_object_mut() {
            map.insert("stage".to_string(), json!(stage_num));
        }
        dest.push(obj);
    }
}

// ---------------------------------------------------------------------------
// Stage Definitions
// ---------------------------------------------------------------------------

pub fn prescreen_stage() -> Stage<KernelReviewState, Phase0Output> {
    Stage::builder("stage_0_prescreen")
        .system_prompt(PromptTemplate::<KernelReviewState>::new(
            "You are an AI assistant preparing a Linux kernel patch review.\nReview the provided Patch and select all potentially relevant subsystem guides from the index below.\nCRITICAL BIAS RULE: You MUST err on the side of inclusion. Only exclude a guide if it is 100% irrelevant to the modified code. If there is any doubt, include the file.\n\nYou MUST respond with ONLY a JSON object, no other text. Example:\n```json\n{\"selected_prompts\": [\"networking.md\", \"locking.md\"]}\n```",
        ))
        .user_prompt(
            PromptTemplate::<KernelReviewState>::new(
                "<subsystem_guide_index>\n@include(\"subsystem/subsystem.md\")\n</subsystem_guide_index>\n\n<patch>\n{{target_commit_diff}}\n</patch>",
            )
            .with_var("target_commit_diff", |s: &KernelReviewState| s.target_commit_diff.clone())
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
            ..Default::default()
        })
        .skip_if(|s| s.manual_stages.is_some())
        .reduce(|state, out: Phase0Output| {
            let prompts: Vec<String> = out
                .selected_prompts
                .into_iter()
                .filter(|name| !STAGE_EXCLUSIVE_GUIDES.contains(&name.as_str()))
                .collect();
            state.selected_guides = prompts;
        })
        .build()
}

pub fn planning_stage() -> Stage<KernelReviewState, PlanningOutput> {
    Stage::builder("stage_planning")
        .system_prompt(kernel_system_prompt(true))
        .user_prompt(PromptTemplate::<KernelReviewState>::new(
            r#"Analyze the provided patch and determine which of the following review stages are relevant and should be executed:
- Stage 4: Resource management
- Stage 5: Locking and synchronization
- Stage 6: Security audit
- Stage 7: Hardware engineer's review

CRITICAL: Always err on the side of running more stages. If you are not absolutely sure, include the stage. If the patch is a trivial typo fix, you may omit some stages. Stages 1, 2, and 3 are always run and should not be included in your answer.

You MUST respond with ONLY a JSON object, no other text. Example:
```json
{"relevant_stages": [4, 5, 6, 7]}
```"#,
        ))
        .output_format(OutputFormat::json_with_schema(json!({
            "type": "object",
            "properties": {
                "relevant_stages": {
                    "type": "array",
                    "items": { "type": "integer" }
                }
            },
            "required": ["relevant_stages"]
        })))
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns: 1,
            ..Default::default()
        })
        .skip_if(|s| s.manual_stages.is_some())
        .reduce(|state, out: PlanningOutput| {
            let mut stages = vec![1, 2, 3];
            for n in out.relevant_stages {
                if (4..=7).contains(&n) && !stages.contains(&n) {
                    stages.push(n);
                }
            }
            state.planned_stages = stages;
        })
        .build()
}

/// Stages 3 to 6 review the diff hunks alone. Every other stage also needs the
/// commit message, so it gets the git show output with the changelog injected.
fn stage_uses_commit_log(stage_num: u8) -> bool {
    !(3..=6).contains(&stage_num)
}

fn analysis_stage(
    stage_num: u8,
    name: &'static str,
    instruction: &'static str,
    guides: &[&'static str],
    max_turns: usize,
    temperature: f32,
) -> Box<dyn ExecutableStage<KernelReviewState>> {
    let mut user_template = PromptTemplate::<KernelReviewState>::new(format!(
        "{}\n\n{}",
        instruction, STAGE_JSON_SCHEMA_EXAMPLE
    ));
    for guide in guides {
        user_template = user_template.include_file(*guide);
    }

    Box::new(
        Stage::builder(name)
            .system_prompt(kernel_system_prompt(stage_uses_commit_log(stage_num)))
            .user_prompt(user_template)
            .output_format(
                OutputFormat::json()
                    .with_validator(validate_stages_1_to_8)
                    .with_feedback_formatter(format_stages_1_to_8_feedback),
            )
            .policy(StagePolicy {
                tools: ToolScope::All,
                max_turns,
                temperature,
                ..Default::default()
            })
            .reduce(
                move |state: &mut KernelReviewState, out: StageConcernsOutput| {
                    append_stage_items(
                        &mut state.all_concerns,
                        &out.concerns,
                        stage_num,
                        "General",
                        "description",
                    );
                    append_stage_dismissed_concerns(
                        &mut state.all_dismissed_concerns,
                        &out.dismissed_concerns,
                        stage_num,
                    );
                },
            )
            .build(),
    )
}

pub fn resolve_analysis_stages_with_options(
    state: &KernelReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<KernelReviewState>>> {
    let selected_stages = if let Some(ref manual) = state.manual_stages {
        manual.clone()
    } else if !state.planned_stages.is_empty() {
        state.planned_stages.clone()
    } else {
        vec![1, 2, 3, 4, 5, 6, 7]
    };

    let mut stages = Vec::new();
    for num in selected_stages {
        match num {
            1 => stages.push(analysis_stage(
                1,
                "stage_1",
                STAGE_1_INSTRUCTION,
                &[],
                max_turns,
                temperature,
            )),
            2 => stages.push(analysis_stage(
                2,
                "stage_2",
                STAGE_2_INSTRUCTION,
                &[],
                max_turns,
                temperature,
            )),
            3 => stages.push(analysis_stage(
                3,
                "stage_3",
                STAGE_3_INSTRUCTION,
                &["callstack.md", "technical-patterns.md"],
                max_turns,
                temperature,
            )),
            4 => stages.push(analysis_stage(
                4,
                "stage_4",
                STAGE_4_INSTRUCTION,
                &[],
                max_turns,
                temperature,
            )),
            5 => stages.push(analysis_stage(
                5,
                "stage_5",
                STAGE_5_INSTRUCTION,
                &["subsystem/locking.md"],
                max_turns,
                temperature,
            )),
            6 => stages.push(analysis_stage(
                6,
                "stage_6",
                STAGE_6_INSTRUCTION,
                &[],
                max_turns,
                temperature,
            )),
            7 => stages.push(analysis_stage(
                7,
                "stage_7",
                STAGE_7_INSTRUCTION,
                &[],
                max_turns,
                temperature,
            )),
            _ => {}
        }
    }
    stages
}

pub fn stage_8_deduplication(
    max_turns: usize,
    temperature: f32,
) -> Stage<KernelReviewState, StageConcernsOutput> {
    Stage::builder("stage_8_deduplication")
        .system_prompt(kernel_system_prompt(true))
        .user_prompt(
            PromptTemplate::<KernelReviewState>::new(format!(
                r#"{STAGE_8_INSTRUCTION}

Aggregated Concerns:
{{{{aggregated_concerns}}}}

Aggregated Dismissed Concerns:
{{{{aggregated_dismissed_concerns}}}}

Return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations".
Preserve the most precise location details from the input. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "concerns": [
    {{
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ],
  "dismissed_concerns": [
    {{
      "type": "Resource Management",
      "description": "Possible missing cleanup when foo_init() fails after bar_alloc().",
      "reasoning": "The concrete code path or ordering that proves this candidate concern does not apply.",
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 125,
          "code_snippet": "safe_code_path();",
          "why_this_location_matters": "This is where the cleanup path proves the candidate leak does not apply."
        }}
      ]
    }}
  ]
}}
```"#
            ))
            .with_var("aggregated_concerns", |s: &KernelReviewState| {
                serde_json::to_string_pretty(&s.all_concerns).unwrap_or_default()
            })
            .with_var("aggregated_dismissed_concerns", |s: &KernelReviewState| {
                serde_json::to_string_pretty(&s.all_dismissed_concerns).unwrap_or_default()
            }),
        )
        .output_format(
            OutputFormat::json()
                .with_validator(validate_stages_1_to_8)
                .with_feedback_formatter(format_stages_1_to_8_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce(|state, out: StageConcernsOutput| {
            state.deduplicated_concerns = out.concerns;
            state.deduplicated_dismissed_concerns = out.dismissed_concerns;
        })
        .build()
}

pub fn stage_9_conflict_resolution(
    max_turns: usize,
    temperature: f32,
) -> Stage<KernelReviewState, Stage9Output> {
    Stage::builder("stage_9_conflict_resolution")
        .system_prompt(kernel_system_prompt(true))
        .user_prompt(
            PromptTemplate::<KernelReviewState>::new(format!(
                r#"{STAGE_9_INSTRUCTION}

Consolidated Concerns:
{{{{deduplicated_concerns}}}}

Consolidated Dismissed Concerns:
{{{{deduplicated_dismissed_concerns}}}}

Return ONLY a JSON object with a 'concerns' array containing the remaining concerns after resolving conflicts. Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
Preserve the most precise locations from the retained concerns. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "concerns": [
    {{
      "type": "Memory Leak",
      "description": "Memory leak in function X",
      "reasoning": "1. X is called.\n2. Y is allocated but not freed on error path.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ]
}}
```"#
            ))
            .with_var("deduplicated_concerns", |s: &KernelReviewState| {
                serde_json::to_string_pretty(&s.deduplicated_concerns).unwrap_or_default()
            })
            .with_var("deduplicated_dismissed_concerns", |s: &KernelReviewState| {
                serde_json::to_string_pretty(&s.deduplicated_dismissed_concerns).unwrap_or_default()
            }),
        )
        .output_format(OutputFormat::json())
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce(|state, out: Stage9Output| {
            state.conflict_resolved_concerns = out.concerns;
        })
        .build()
}

pub fn stage_10_verification(
    max_turns: usize,
    temperature: f32,
) -> Stage<KernelReviewState, Stage10Output> {
    Stage::builder("stage_10_verification")
        .system_prompt(kernel_system_prompt(true))
        .user_prompt(
            PromptTemplate::<KernelReviewState>::new(format!(
                r#"{STAGE_10_INSTRUCTION}

CRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must find concrete evidence in the code that proves the concern is invalid (e.g., verifying the caller handles the edge case). If you cannot find concrete proof of safety, you must retain the concern.{{{{follow_up_series_section}}}}

Consolidated Concerns:
{{{{conflict_resolved_concerns}}}}

Return ONLY a JSON object with a 'findings' array. Each object in the 'findings' array MUST use exactly the following keys: "problem" (a string containing the vulnerability description), "severity" (a string: Low, Medium, High, or Critical), "severity_explanation" (a string detailing the reasoning and proof), "preexisting" (a boolean: true if the problem already existed in the codebase before these patches were applied, or false if it was newly introduced by the reviewed patchset), "locations" (an array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters). Carry forward the locations from the validated concern; if you gather better evidence, replace vague locations with the most precise verified locations. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{{
  "findings": [
    {{
      "problem": "Memory leak in function X when condition Y is met.",
      "severity": "High",
      "severity_explanation": "1. Condition Y is met.\n2. The buffer is allocated but not freed before return.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "function_name",
          "line": 123,
          "code_snippet": "problematic_code();",
          "why_this_location_matters": "This is where the newly allocated resource is dropped on the error path."
        }}
      ]
    }}
  ]
}}
```"#
            ))
            .include_file("false-positive-guide.md")
            .include_file("severity.md")
            .with_var("follow_up_series_section", |s: &KernelReviewState| {
                s.follow_up_series_context
                    .as_ref()
                    .map(|ctx| format!("\n\n{}", ctx))
                    .unwrap_or_default()
            })
            .with_var("conflict_resolved_concerns", |s: &KernelReviewState| {
                serde_json::to_string_pretty(&s.conflict_resolved_concerns).unwrap_or_default()
            }),
        )
        .output_format(OutputFormat::json())
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce(|state, out: Stage10Output| {
            state.findings = out.findings;
        })
        .build()
}

pub fn stage_11_inline_report(
    max_turns: usize,
    temperature: f32,
) -> Stage<KernelReviewState, String> {
    Stage::builder("stage_11_report")
        .system_prompt(kernel_system_prompt(true))
        .user_prompt(
            PromptTemplate::<KernelReviewState>::new(format!(
                r#"{STAGE_11_INSTRUCTION}

Findings:
{{{{findings}}}}

Return raw text output, not JSON."#
            ))
            .include_file("inline-template.md")
            .with_var("findings", |s: &KernelReviewState| {
                serde_json::to_string_pretty(&s.findings).unwrap_or_default()
            }),
        )
        .output_format(OutputFormat::text_with_validator(
            validate_inline_format,
            format_inline_feedback,
        ))
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            recitation_policy: RecitationPolicy::FallbackToFreeForm {
                reminder: "Do not quote code verbatim. Summarize your review directly.".to_string(),
            },
            ..Default::default()
        })
        .reduce(|state, out: String| {
            state.review_inline = out;
        })
        .build()
}

// ---------------------------------------------------------------------------
// Complete Kernel Review Workflow Graph
// ---------------------------------------------------------------------------

/// Constructs the complete declarative workflow for Linux kernel patch review.
pub fn build_kernel_review_workflow() -> Workflow<KernelReviewState> {
    build_kernel_review_workflow_with_options(20, 0.0)
}

/// Constructs the declarative workflow with custom per-stage interaction limits and temperature.
pub fn build_kernel_review_workflow_with_options(
    max_turns: usize,
    temperature: f32,
) -> Workflow<KernelReviewState> {
    Workflow::builder("linux_kernel_code_review")
        .stage(prescreen_stage())
        .dynamic_parallel(
            planning_stage(),
            move |state| resolve_analysis_stages_with_options(state, max_turns, temperature),
            ParallelPolicy::FailFast,
        )
        .early_exit_if(
            |s| s.all_concerns.is_empty(),
            "No concerns raised in initial analysis stages",
        )
        .stage(stage_8_deduplication(max_turns, temperature))
        .early_exit_if(
            |s| s.deduplicated_concerns.is_empty(),
            "No concerns remaining after deduplication",
        )
        .stage(stage_9_conflict_resolution(max_turns, temperature))
        .early_exit_if(
            |s| s.conflict_resolved_concerns.is_empty(),
            "No concerns remaining after conflict resolution",
        )
        .stage(stage_10_verification(max_turns, temperature))
        .early_exit_if(
            |s| s.findings.is_empty(),
            "No findings validated in verification stage",
        )
        .stage(stage_11_inline_report(max_turns, temperature))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_only_stages_3_to_6_review_the_diff_alone() {
        // Matches ReviewStage::use_log_in_context, which the workflow replaced.
        for stage in [1, 2, 7] {
            assert!(
                stage_uses_commit_log(stage),
                "stage {stage} needs the commit message"
            );
        }
        for stage in [3, 4, 5, 6] {
            assert!(
                !stage_uses_commit_log(stage),
                "stage {stage} reviews the diff hunks alone"
            );
        }
    }

    #[test]
    fn test_analysis_stages_keep_the_guidance_the_schema_alone_does_not_carry() {
        // The vendored guides still tell the model to use TodoWrite, which no
        // longer exists, and stage 10 keeps an anti-charity directive of its
        // own. Both belong to stages 1 to 7 as well.
        for required in [
            "Do not call or mention TodoWrite",
            "Do not be overly charitable to the existing code",
            "If you find no concerns and no dismissed concerns",
            "investigated, and disproved with concrete evidence",
            "\"preexisting\": true if this bug already existed",
            "\"reasoning\": A step-by-step explanation.",
            "the candidate concern that was investigated and disproved",
        ] {
            assert!(
                STAGE_JSON_SCHEMA_EXAMPLE.contains(required),
                "stage 1-7 guidance lost: {required}"
            );
        }
    }

    #[test]
    fn test_build_workflow_graph_structure() {
        let workflow = build_kernel_review_workflow();
        assert_eq!(workflow.name, "linux_kernel_code_review");
        assert_eq!(workflow.steps.len(), 10);
    }
}
