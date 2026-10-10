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
//! operating over [`LinuxPatchReviewState`].

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::workflow::graph::Workflow;
use crate::workflow::output::OutputFormat;
use crate::workflow::policy::{ParallelPolicy, RecitationPolicy, StagePolicy, ToolScope};
use crate::workflow::prompt::PromptTemplate;
use crate::workflow::stage::{ExecutableStage, Stage};

/// Complete execution state of a Linux kernel patch review.
#[derive(Clone, Debug, Default)]
pub struct LinuxPatchReviewState {
    pub ps_id: String,
    pub p_id: String,
    pub target_commit_sha: String,
    pub baseline_sha: String,
    pub target_commit_diff: String,
    pub target_commit_diff_only: String,
    pub prefetched_context: String,
    /// Source prefetch failed; stages must gather target context through Git tools.
    pub prefetch_failed: bool,
    pub series_range: Option<String>,
    pub follow_up_series_context: Option<String>,

    /// Subsystem guide markdown files selected during the pre-screen and
    /// shared with every stage.
    pub selected_guides: Vec<String>,
    /// Optional manual stages filter (e.g. `--stages goal,locking`).
    pub manual_stages: Option<Vec<String>>,
    /// Caller-supplied instructions appended to the shared system prompt.
    pub custom_prompt: Option<String>,
    /// Stages selected by dynamic planning (or overridden by manual_stages).
    pub planned_stages: Vec<String>,
    /// Skip plain-text report and summary generation stages (e.g. in `--agent` mode).
    pub skip_report: bool,
    /// Retain pre-existing concerns through verification and inline report generation.
    pub report_preexisting: bool,

    /// Target project identifier (e.g. "linux" or "sashiko") used for finding UUID prefixes.
    pub project: String,

    /// Aggregated raw concerns collected from the analysis stages.
    pub all_concerns: Vec<Value>,
    /// Aggregated raw dismissed concerns collected from the analysis stages.
    pub all_dismissed_concerns: Vec<Value>,

    /// Direct findings (Category 1a, VF1..VFp) emitted by the verification stage.
    pub verification_findings: Vec<Value>,
    /// Direct dismissals (Category 1b, VD1..VDq) emitted by the verification stage.
    pub verification_dismissed: Vec<Value>,
    /// Findings confirmed by parallel post-verification stages (PVF1..PVFr).
    pub post_verification_findings: Vec<Value>,
    /// Hard cases refuted by parallel post-verification stages (PVD1..PVDs).
    pub post_verification_dismissed: Vec<Value>,

    /// Deduplicated dismissed concerns from the verification stage.
    pub deduplicated_dismissed_concerns: Vec<Value>,
    /// Speculative or contested candidate items routed to parallel post-verification.
    pub hard_cases: Vec<Value>,
    /// Candidate pre-existing concerns extracted for separate processing.
    pub concerns: Vec<Value>,

    /// Verified findings from the verification and post-verification stages.
    pub findings: Vec<Value>,

    /// Concise plain-text summary of the change generated at the end of review.
    pub summary: String,
    /// Generated LKML plain-text review from the report stage.
    pub review_inline: String,
    /// Fix suggestions.
    pub fixes: String,
}

// ---------------------------------------------------------------------------
// Typed Output Structures for Stage Serialization
// ---------------------------------------------------------------------------

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct PrescreenOutput {
    pub selected_prompts: Vec<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct PlanningOutput {
    pub relevant_stages: Vec<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct StageConcernsOutput {
    pub concerns: Vec<Value>,
    #[serde(default)]
    pub dismissed_concerns: Vec<Value>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct VerificationOutput {
    pub findings: Vec<Value>,
    pub hard_cases: Vec<Value>,
    pub dismissed_concerns: Vec<Value>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct PostVerificationOutput {
    pub findings: Vec<Value>,
    #[serde(default)]
    pub dismissed_concerns: Vec<Value>,
}

// ---------------------------------------------------------------------------
// Common System Prompt Template
// ---------------------------------------------------------------------------

pub fn linux_system_prompt(use_log: bool) -> PromptTemplate<LinuxPatchReviewState> {
    let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();
    let diff_var = if use_log {
        "{{target_commit_diff}}"
    } else {
        "{{target_commit_diff_only}}"
    };

    PromptTemplate::<LinuxPatchReviewState>::new(format!(
        r#"Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references (e.g., 'today', 'last week', 'next year') strictly on this current date.

You are an expert Linux kernel maintainer. Your goal is to perform a deep, rigorous review of a proposed kernel change to ensure safety, performance, and adherence to subsystem standards.

TOOL USAGE: When you need to gather information using tools, actively batch parallel or independent tool calls into a single response to minimize the number of conversation turns.

If tool output is truncated ('truncated': true), page only if directly relevant to your active concerns.

<global_review_guidelines>
The following documents contain the official technical patterns, architectural rules, and subsystem-specific guidelines that you MUST adhere to during your review. Use these as the absolute source of truth for identifying anti-patterns and violations.
@includes
</global_review_guidelines>

=== Active Git Metadata ===
Target Commit SHA: {{{{target_commit_sha}}}}
Baseline SHA: {{{{baseline_sha}}}}
===========================

Target Commit:
{diff_var}
{{{{prefetched_block}}}}{{{{custom_prompt_block}}}}"#
    ))
    .with_var("target_commit_sha", |s: &LinuxPatchReviewState| s.target_commit_sha.clone())
    .with_var("baseline_sha", |s: &LinuxPatchReviewState| s.baseline_sha.clone())
    .with_var("target_commit_diff", |s: &LinuxPatchReviewState| s.target_commit_diff.clone())
    .with_var("target_commit_diff_only", |s: &LinuxPatchReviewState| s.target_commit_diff_only.clone())
    .with_var("prefetched_block", |s: &LinuxPatchReviewState| {
        if s.prefetch_failed {
            format!(
                "\n\nAutomatic source prefetch failed for target commit {}. Before analyzing the code, use git_read_files and git_grep at that revision to gather the source context. Do not infer source contents from the physical checkout.\n",
                s.target_commit_sha
            )
        } else if s.prefetched_context.is_empty() {
            String::new()
        } else {
            format!(
                "\n\n<pre_fetched_context>\nThe following source excerpts were fetched from the target commit identified by Source revision below, based on the modified lines in the patch. They include modified definitions and selected dependencies. Parent and series-final revisions must be inspected separately with Git tools.\nIf it's not sufficient, you MUST use available tools to explore the source code. Don't make assumptions without actually looking into the relevant code.\n\n{}\n</pre_fetched_context>",
                s.prefetched_context
            )
        }
    })
    .with_var("custom_prompt_block", |s: &LinuxPatchReviewState| {
        s.custom_prompt.as_deref().map(str::trim).filter(|p| !p.is_empty()).map_or_else(String::new, |p| {
            format!("\n\n<custom_instructions>\n{p}\n</custom_instructions>")
        })
    })
    .include_files_from_state(|s: &LinuxPatchReviewState| {
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

const STAGE_GOAL_INSTRUCTION: &str = r#"# Analyze commit main goal

You are a senior Linux kernel maintainer evaluating the high-level intent of a proposed commit. Analyze the commit message and the conceptual change. Focus on the big picture: Are there architectural flaws, UAPI breakages, backwards compatibility issues, or fundamentally flawed concepts? Consider the long-term maintainability and system-wide implications of this design. If the core idea is dangerous, incorrect, or violates established kernel principles, raise a concern. Be open-minded but thorough; question assumptions made by the author and consider alternative, simpler designs."#;

const STAGE_IMPLEMENTATION_INSTRUCTION: &str = r#"# High-level implementation verification

You are verifying if the provided code changes actually implement what the commit message claims. Look for undocumented side-effects, missing pieces (e.g., a core change without updating corresponding callers, or changing a struct without updating all initializers), and unhandled corner cases related to the feature's logic. Explicitly check for missing API callbacks and interface omissions: when defining or modifying structures containing function pointers, verify that all logically required callbacks are implemented. When a patch constructs, registers, or restores a kernel object (e.g., `struct file`, `struct gpio_chip`, `ndo_get_stats64` / `net_device_ops`, or restored memory pages), compare its initialized flags (such as `f_mode` capabilities), security hooks, allocation tags, and required callbacks/stats against the standard creation or allocation path in that subsystem to ensure nothing required was omitted. Verify that all claims in the commit message are fully realized in the code. Identify any incomplete implementations, implicit behavioral changes, or API contract violations. Furthermore, verify that the logic is mathematically and semantically sound. Check for off-by-one errors in bounds, incorrect bitwise operations (e.g., bitwise arithmetic that incorrectly shifts values leading to overlapping masks or clobbering adjacent fields), and verify that all arguments passed to external subsystems (like kobjects or netdevs) are valid and semantically correct (e.g., non-empty strings, correct sizes, correct format specifiers). Do not stop after finding several bugs in one function or hunk; systematically audit every modified function, struct initializer, and hunk in the diff before concluding. Don't trust the commit message without verifying each claim. Assume that the message might be incorrect or even intentionally malicious. Do not focus on low-level memory or locking errors yet."#;

const STAGE_EXECUTION_FLOW_INSTRUCTION: &str = r#"# Execution flow verification

You are a static analysis engine tracing execution flow in C or Rust code. Carefully trace the control flow of the provided patch. Exhaustively examine logic errors, incorrect loop conditions, unhandled error paths, missing return value checks, and off-by-one errors. Check every branch, switch statement, and conditional. Specifically look for missing teardown/restore of state in error paths (e.g. failing to cleanly restore global state or struct fields that were temporarily modified, such as resetting ID/minor values to -1 before returning on failure). Verify that mathematical operations, sizing, or rounding algorithms don't bypass capacity checks or enable out-of-bounds reads/writes. Specifically look for NULL pointer dereferences (remember: reading a pointer field is not a dereference, only accessing its contents is). Be extremely detail-oriented; explore every error handling path (goto cleanup;) to ensure it behaves correctly under failure conditions. Do not stop after finding several bugs in one function or hunk; systematically trace every modified function and error path across the entire diff before concluding. Additionally, verify preprocessor macro correctness and spelling (e.g., ensuring CONFIG_ prefixes are used where expected instead of HAVE_). Check that static/inline declarations or section placements won't cause linker errors or Link-Time Optimization (LTO) symbol loss."#;

const STAGE_RESOURCES_INSTRUCTION: &str = r#"# Resource management

You are an expert in C and Rust resource management within the Linux kernel. Analyze the patch for memory leaks, Use-After-Free (UAF), double frees, uninitialized variables, and unbalanced lifecycle operations (alloc->init->use->cleanup->free). Pay special attention to error paths where resources might be leaked. Ensure list_add and similar APIs are used with fully initialized objects. Track the lifetime of every allocated struct and file descriptor. Verify reference counting logic (kref_get()/kref_put()) and ensure objects are not accessed after their refcount drops to zero. Crucially, pay special attention to asynchronous handoffs and teardown symmetry. Check if resources freed in one path are accessed in another concurrent or cleanup flow (UAF). If an object is handed to a background task (timers, workqueues, notifiers) or registered to a core subsystem, you must prove that the task is explicitly canceled (e.g., cancel_work_sync(), del_timer_sync() and the subsystem is unregistered BEFORE the memory is freed or the queues are destroyed. Do not stop after finding several bugs in one function or hunk; systematically audit every modified function and lifecycle path across the entire diff before concluding."#;

const STAGE_LOCKING_INSTRUCTION: &str = r#"# Locking and synchronization

You are a world-class concurrency and locking expert auditing a Linux kernel patch.
Carefully review the proposed patch for ANY locking, concurrency, or synchronization bugs.
You MUST consider the following categories of issues and report any violations:
1. Sleeping in atomic context: Are there any calls to `mutex_lock`, `kzalloc` with `GFP_KERNEL`, `msleep`, `cond_resched`, `flush_workqueue`, `synchronize_rcu`, or `cancel_work_sync` while holding a spinlock, rwlock, or within an RCU read-side critical section (`rcu_read_lock`)?
2. Lock ordering and deadlocks: Are locks acquired in a different order than elsewhere? Does it acquire a mutex while holding another mutex that could cause AB-BA deadlocks? Are IRQs disabled (`spin_lock_irqsave`) when acquiring a lock that is used in hardirq context? Does it acquire a lock already held by a higher-level subsystem (e.g., ethtool)?
3. Race conditions and lockless access: Are shared variables, list entries, or pointers accessed without holding the appropriate lock (e.g., clearing pointers concurrently while others dereference them)? Are there missing memory barriers (`smp_mb`, `smp_wmb`, `smp_rmb`) when lockless access is intended (e.g. in lockless readers reading reused elements)? Are there TOCTOU races where a state is checked outside a lock but relied upon inside?
4. UAF / Locking Freed Memory: Are locks (`mutex_unlock`, `spin_unlock`) called on objects that have already been freed? Are works/timers destroyed before subsystems are unregistered, allowing new events to use freed works/timers? Is the protocol initialized flag set before private data is ready?
5. RCU rules: Is `list_splice_init` or similar non-RCU-safe operations used on RCU-protected lists? Is `list_for_each_rcu` used without `rcu_read_lock`?
6. Unprotected state modifications: Does the patch check state before acquiring the lock (e.g., checking power state before taking mutex)? Are hardware state, flags, or stats updated without proper protection?
7. Sequence counters: Are stats accumulations directly inside a `u64_stats_fetch_retry` loop leading to double counting? Is it possible for an interrupt to read a sequence counter while the interrupted context is modifying it (deadlock)?
8. Lock re-initialization: Does it re-initialize a lock that was already initialized, or destroy a lock on a failure path improperly?
9. Missing locking: Is a port or file exposed to userspace before the driver/TTY linking is complete? Are objects added to global/shared lists before they are fully initialized or their resources attached? Does a worker race with cleanup code leading to dropped/leaked frames?"#;

const STAGE_SECURITY_INSTRUCTION: &str = r#"# Security audit

You are a Red Team security researcher auditing a Linux kernel patch. Look for security vulnerabilities such as buffer overflows, out-of-bounds reads/writes, integer overflows, privilege escalation vectors, time-of-check to time-of-use (TOCTOU) races, and information leaks (e.g., copying uninitialized kernel memory to user-space via copy_to_user). Scrutinize all points where untrusted user input reaches sensitive functions without validation. Ensure all length checks and bounds checks are robust against malicious input. Focus heavily on attack surfaces and data boundaries."#;

const STAGE_HARDWARE_INSTRUCTION: &str = r#"# Hardware engineer's review

You are a hardware engineer reviewing device driver changes. If this patch touches driver or hardware-specific code, rigorously review register accesses, IRQ handling, DMA mapping/unmapping, memory barriers, and timing/delays. Look for missing dma_wmb()/dma_rmb() barriers, incorrect endianness conversions (cpu_to_le32), and unsafe DMA buffer allocations. Ensure the hardware state machine is handled correctly, especially during suspend/resume or device reset. Evaluate the physical state machine constraints: verify that clocks and power domains are enabled before registers are accessed, and that hardware rings/queues are actually initialized in the current hardware state before being unconditionally accessed. If the patch is purely generic software logic (e.g., VFS, core networking), return {"concerns": [], "dismissed_concerns": []}."#;

const STAGE_VERIFICATION_INSTRUCTION: &str = r#"# Verification and severity estimation

You are the lead reviewer consolidating `concerns` and `dismissed_concerns` generated by parallel specialized review stages.
Your task is to (1) deduplicate overlapping items across both lists while preserving exact bug boundaries, and (2) classify every unique candidate into one of two high-level categories:
- **Category 1: Well-Justified** — split into **1a. Well-Justified Concerns (`findings`)** and **1b. Well-Justified Dismissals (`dismissed_concerns`)**.
- **Category 2: Speculative or Contested (`hard_cases`)** — routed to parallel per-finding `post-verification` for deep codebase verification with tools.

### Step 1: Deduplication and Boundary Preservation
1. Group `concerns` and `dismissed_concerns` that refer to the same underlying root cause AND the same function/lifecycle phase.
2. Do NOT merge a setup/registration bug with a teardown/unregistration bug in a separate function; if an input concern combines setup and teardown bugs across separate functions, split them into separate items. Similarly, do NOT merge distinct races or bugs in separate callbacks or functions, or races on different resources/fields (e.g., a timer race vs. a workqueue/rfkill race) even within the same teardown function—either keep them as separate items or explicitly name ALL distinct racing resources, callbacks, and failure mechanisms in the item's title/description and explanation fields (`problem` and `severity_explanation` for `findings`, or `description` and `concern_arguments` for `hard_cases`).
3. SPECIFICITY REQUIREMENT: When merging overlapping items, preserve and consolidate the most specific details: exact function names, file paths, line numbers when known, and ALL distinct triggering conditions, syscalls, consequences, and racing callbacks/resources mentioned across the merged items. When a missing cleanup, missing export/linkage attribute, or incomplete error-path unwind causes both an immediate failure (e.g., probe failure, symbol loss under LTO) and a downstream resource impact (e.g., reference count leak, memory leak, or UAF), explicitly state BOTH consequences in the item's explanation (`severity_explanation` for `findings`, or `concern_arguments` for `hard_cases`). Preserve and merge the `locations` arrays. Do not invent line numbers; use `null` when unknown.
4. Set `"preexisting": false` whenever the patch introduces, modifies, triggers, exposes, or relies on the buggy code path, caller/callee interaction, or error cleanup path (even if the underlying helper function, check, or cleanup label already existed before the patch). Set `"preexisting": true` ONLY if the bug is in untouched code whose reachability, inputs, and behavior are completely unaffected by the patch.

### Step 2: Classification Rules (1: Well-Justified vs. 2: Speculative or Contested)
Classify every consolidated item using these strict signal rules. **Repetition is NOT justification: multiple overlapping items are never enough for Category 1 (1a or 1b) unless their reasoning is backed by specific, concrete code proof.**

1. **Category 1a — Well-Justified Concern (`findings` array):**
   - **Mandatory prerequisite & strong signals:** Concrete, self-contained code proof directly visible in the target diff, prefetched context, and cited `locations`, with **no** competing `dismissed_concern`, **no** reliance on unverified assumptions about unseen code, and **no** potential resolution by a follow-up patch in `=== Follow-Up Patches in Series ===`. Multiple deduplicated `concerns` with no attempts to dismiss is a strong signal for 1a **only when grounded in concrete code proof** (if multiple stages repeat an unproven assumption or vague claim without specific code proof, classify into `hard_cases` instead).
   - **Action:** Validate it directly and emit it in `findings`. Assign a calibrated `severity` (`Low`, `Medium`, `High`, or `Critical`) following `severity.md`, state all consequences (both immediate and downstream), triggering paths, and reachability at the start of `severity_explanation` (preserving every distinct function, callback, racing resource, and mechanism), set `"preexisting"`, and include `locations`.

2. **Category 1b — Well-Justified Dismissal (`dismissed_concerns` array):**
   - **Mandatory prerequisite & strong signals:** Concrete disproving `code_snippet` in `locations` (showing the exact local guard, lock, bounds check, cleanup path, or lifecycle invariant that prevents the bug), with **no** competing `concern` and **no** reliance on unverified assumptions about external callers, callees, hardware bounds, or configurations. Multiple overlapping `dismissed_concerns` for the same code is NOT a signal that the dismissal is safe—it indicates multiple analysts independently found the code suspicious; when multiple stages flag and dismiss the same non-trivial mechanism using assumptions about caller behavior, hardware/firmware handling of dummy values, concurrent truncation/teardown, or build/config macros rather than a direct local guard in the same function, classify into `hard_cases` with `"signal_reason": "speculative_dismissal"`.
   - **Disqualifiers for Category 1b (NEVER classify as 1b):**
     - **Single-caller or happy-path-only proofs:** A dismissal that proves an invariant (such as non-NULL pointers, prior ring/vector mapping, packet length bounds, or external serialization) in only one caller or normal init/open/default mode without verifying ALL callers and modes in the tree (including suspend/resume, rebuild, reset, default-mapping flags, sysfs/debugfs, and external-module `W=`/`M=` build paths) is INVALID.
     - **Asymmetry or cleared-state rationalizations:** A dismissal that rationalizes an unpaired init/patch/fini call, an unpatched hardware packet field, or clearing/consuming an error or status field (e.g., via `xchg`) before downstream event/notification consumers read it as a "harmless no-op" or "intentional protocol behavior" without concrete code proving that exact value or omission is safely handled is INVALID.
     - **Build or compile-time assertion failures are bugs, not mitigations:** A dismissal that argues a type, size, alignment, or Kconfig mismatch is safe because a compile-time assertion, compiler error, or build/test failure will prevent runtime execution is INVALID—breaking the build under any valid configuration or host/target architecture is itself a real bug.
     - **Indirect state checks or "harmless side-effect" rationalizations:** A dismissal that relies on an indirect state check (such as checking interrupt disablement inside a non-raw spinlock critical section that does not disable hardware interrupts on `PREEMPT_RT`) or rationalizes an unintended state advancement, range expansion, or operation on unmodified state as "harmless" or "benign" is INVALID.
   - **Action:** Place in `dismissed_concerns` (dropped from further verification).
   - **CRITICAL INVARIANT:** Never place any item that was raised as a `concern` into `dismissed_concerns` in this stage. If a raised `concern` is contested by a `dismissed_concern` or appears questionable, it MUST be placed in `hard_cases` for tool-based `post-verification`.

3. **Category 2 — Speculative or Contested (`hard_cases` array — sent to parallel `post-verification`):**
   - **Strong signals:**
     - **Mixed signals (`"mixed_signals"`):** Similar or overlapping `concerns` and `dismissed_concerns` exist for the same root cause, function, or code path.
     - **Speculative or assumption-based dismissal (`"speculative_dismissal"`):** Even when NO stage raised a `concern` (and especially when multiple stages emitted overlapping `dismissed_concerns`), inspect every standalone dismissal critically! Multiple overlapping dismissals are NOT enough if they are not justified by specific local disproving code. If one or more `dismissed_concerns` identified a plausible bug (such as a missing cleanup on an error path, NULL dereference when a field is unmapped on resume/rebuild, unlocked shared state access, race with teardown or truncation, unpaired init/patch call, cleared error field before event reporting, sleeping/rescheduling under a lock, unintended state or index advancement, or type/width/config/build mismatch) and dismissed it using a single-caller proof, an assumption not proven by the cited `code_snippet`, a rationalization that an unpatched `0` or cleared error is "harmless" or "intentional", a claim that a compile-time assertion/build failure prevents runtime corruption, or an indirect predicate check that fails under `PREEMPT_RT` or other valid configurations, you MUST classify the item into `hard_cases` with `"signal_reason": "speculative_dismissal"`.
     - **Speculative or incomplete concern (`"speculative_concern"`):** One or more overlapping `concerns` whose argument is vague, relies on assumptions not based on specific code in the diff/locations (requiring tool inspection of callers, callees, struct definitions, or lock contexts — even if multiple stages repeated the concern), or mixes a partially inaccurate premise with a potentially real underlying bug in the same code path.
     - **Series interaction (`"series_interaction"`):** Any concern that could plausibly be resolved, wired up, or rewritten by a subsequent patch listed in `=== Follow-Up Patches in Series ===`.
   - **Action:** Emit into `hard_cases` with `"estimated_severity"` (`Critical`, `High`, `Medium`, or `Low`), `"signal_reason"`, `"concern_arguments"`, `"dismissal_arguments"`, a concrete `"verification_question"` specifying what code `post-verification` must inspect with tools, `"preexisting"`, and `"locations"`."#;

const STAGE_POST_VERIFICATION_INSTRUCTION: &str = r#"# Per-finding post-verification and conflict resolution

You are the lead reviewer performing deep, tool-assisted codebase verification of a speculative or contested candidate issue (`hard_cases`) identified during initial verification.
1. **Targeted Tool Verification:** Use the available Git and file tools (`git_read_files`, `git_grep`, `git_diff`, `git_show`, `git_blame`) to answer each candidate's `verification_question` and inspect the actual repository code for both `concern_arguments` and `dismissal_arguments`.
2. **SYMMETRICAL PROOF BAR & ALL-CALLERS VERIFICATION:** Both `concern_arguments` and `dismissal_arguments` are untrusted hypotheses. To discard a candidate issue as a false positive, you MUST find concrete proof in the codebase that explicitly invalidates the failure mechanism across ALL callers, entry points, and modes. Citing a single caller (such as normal `open`/`probe` or one delayed-work cancel site) does NOT disprove a NULL dereference, TOCTOU race, or missing lock in a helper function unless `git_grep` across all callers (including `resume`, `rebuild`, `reset`, `sysfs`/`debugfs`, and external-module `W=`/`M=` paths) proves every caller upholds the invariant. Never discard an issue based on unverified assumptions about external callers, helpers, hardware bounds, or build configurations.
3. **LOCAL BOUNDARY & ASYMMETRY RULE:** Do not discard a defect within the modified code of the patch by assuming that surrounding caller systems, parallel execution, or legacy API layers will safely mask or prevent the issue, or by rationalizing an unpaired init/patch/fini call, unpatched hardware packet field, or cleared error/status field (`xchg`) as a "harmless no-op" or "intentional protocol behavior", unless you can point to specific code in the repository that concretely proves the failure mode is structurally impossible.
4. **PROMOTING SPECULATIVE DISMISSALS:** When `"signal_reason"` is `"speculative_dismissal"`, a previous analyst spotted the candidate bug described in `concern_arguments` and dismissed it using `dismissal_arguments`. Inspect the actual code with tools: if the dismissal's assumption is false or incomplete (for example: another caller such as `resume`/`rebuild` or `sysfs` does NOT uphold the invariant; the caller does NOT clean up the resource on error; the cited lock does NOT serialize against concurrent teardown or truncation; an init call lacks its matching patch/fini call; clearing an error hides it from downstream event listeners; a compile-time assertion or build error is triggered under a valid configuration or host/target architecture; an indirect check such as interrupt disablement does not hold under `PREEMPT_RT` non-raw spinlocks; or an unintended state/range advancement causes operations on unmodified ranges), you MUST report the bug as a verified finding in `findings`.
5. **REFINING PARTIALLY INACCURATE PREMISES:** If a candidate concern contains a partially inaccurate premise while also identifying a real bug in the same code path, refine and report the valid underlying bug rather than discarding the entire candidate.
6. **SERIES VALIDATION RULE:** If follow-up patches in this series are provided in the context, check whether each candidate issue is resolved, fixed, or rewritten in the final state of the series (`Series End Commit`) using tools (`git_read_files` or `git_diff` at `Series End Commit`); do not trust promises in commit messages. If resolved by the end of the series, discard it. When referring to other patches within this series in your explanation, DO NOT use ephemeral git hashes; refer to them by their patch subject (e.g., 'commit "mm: fix allocation"').
7. **SEVERITY CALIBRATION AND COMPLETENESS:** Assign a severity (`Low`, `Medium`, `High`, or `Critical`) to each validated finding following `severity.md`: reason through all consequences (both immediate failures and downstream leaks/UAF), triggering paths, and reachability, and state that reasoning at the start of `severity_explanation`. Preserve all distinct function names, file paths, line numbers when known, triggering syscalls/callbacks, racing resources, and consequences. Set `"preexisting": false` whenever the patch introduces, modifies, triggers, exposes, or relies on the buggy code path, caller/callee interaction, or error cleanup path; mark `"preexisting": true` ONLY if the bug is in untouched code completely unaffected by the patch."#;

pub const STAGE_REPORT_INSTRUCTION: &str = r#"# LKML-friendly report generation

You are an automated review bot generating a report for the Linux Kernel Mailing List (LKML). Convert the provided JSON findings into a polite, standard, inline-commented LKML email reply.

Follow the formatting rules strictly. Do not use markdown headers or ALL CAPS shouting. Ensure the tone is constructive and professional. Do not use backticks to quote any names or expressions.

SPECIFICITY REQUIREMENT: Each inline comment MUST reference the exact function name, file, line number when known, and specific triggering condition. Prefer the finding's `locations` field when present. Do not produce vague summaries like 'potential issue in error handling'. State precisely what goes wrong, where, and under what circumstances. Do not invent line numbers; if the exact line is unavailable, anchor the comment to the nearest verified function or symbol and explain the triggering condition.

PRE-EXISTING ISSUES: If any finding has `"preexisting": true`, include it in the report and state explicitly at the start of its comment that the problem was not introduced by this patch (for example: "This problem wasn't introduced by this patch, but...")."#;

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
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations". They mean the same as above, except that "description" is the candidate concern that was investigated and disproved, "reasoning" is the evidence proving it does not apply, and "locations" MUST cite the concrete disproving code (the exact guard, lock, cleanup path, or caller/callee implementation that proves the issue cannot occur — not merely repeating the suspected line from the diff).

Use the 'dismissed_concerns' array ONLY for candidate concerns that you considered plausible, investigated, and disproved with concrete evidence. This is especially important when you first suspect a concern and then follow the evidence chain proving that it does NOT apply.

NO DISMISSAL WITHOUT VERIFIED PROOF: To place a candidate issue in 'dismissed_concerns' (or to discard a suspected issue), you MUST find concrete proof in the code ('file', 'function_or_symbol', 'line', and verbatim 'code_snippet' in 'locations') that explicitly invalidates the concern's reasoning. If the disproving code lives outside the diff (for example, in a caller, callee, macro, sysctl, or build script), you MUST verify that code first using tools ('git_read_files' or 'git_grep') and quote the verified disproving snippet in 'locations'. If you cannot find definitive code proof that the candidate issue is impossible, you MUST report it in 'concerns' (NOT 'dismissed_concerns') and make the condition explicit: if X is possible, then problem Y can occur.
- Citing a single caller (such as normal open/probe) does NOT disprove a NULL dereference, race, or missing lock in a helper function. A caller-based dismissal is valid ONLY if every caller in the tree ('git_grep' across all callers, including suspend/resume, rebuild, reset, sysfs/debugfs, and external-module 'W='/'M=' paths) is verified to uphold the invariant; otherwise report it in 'concerns'.
- Never dismiss an unpaired API/lifecycle call (e.g., calling init_X without a matching patch_X/fini_X), an unpatched hardware packet field, or clearing/consuming an error or status field (e.g., via xchg) before downstream event/notification consumers read it by rationalizing that a dummy 0 value or missing field is a "harmless no-op" or "intentional protocol behavior".
- Never dismiss a type, size, alignment, or Kconfig mismatch because a compile-time assertion, compiler error, or build failure will catch it: breaking compilation under any valid configuration or host/target architecture is itself a bug that must be reported in 'concerns'.
- Never dismiss a locking/preemption violation based on an indirect state check (such as checking interrupt disablement inside a non-raw spinlock critical section that sleeps on PREEMPT_RT), and never dismiss an unintended state/index advancement or operation on unmodified state by rationalizing the side effect as "harmless".

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

pub(crate) fn has_valid_proof_location(item: &Value) -> bool {
    let Some(locations) = item.get("locations").and_then(Value::as_array) else {
        return false;
    };
    locations.iter().any(|loc| {
        let has_file = loc
            .get("file")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_symbol = loc
            .get("function_or_symbol")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_snippet = loc
            .get("code_snippet")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        has_file && has_symbol && has_snippet
    })
}

fn validate_concerns_output(
    output: &StageConcernsOutput,
    _state: &LinuxPatchReviewState,
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
                "dismissed_concerns[{idx}] must have non-empty 'description' and 'reasoning' strings proving why the candidate concern does not apply."
            ));
        }
        if !has_valid_proof_location(dismissed) {
            return Err(format!(
                "dismissed_concerns[{idx}] must include at least one entry in 'locations' with non-empty 'file', 'function_or_symbol', and verbatim disproving 'code_snippet' proving the candidate concern cannot occur. If you do not have concrete code proof, move the candidate issue to 'concerns' instead."
            ));
        }
    }

    Ok(())
}

fn format_concerns_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. You MUST return ONLY a JSON object containing 'concerns' and 'dismissed_concerns' arrays. If there are no concerns and no dismissed concerns, return `{{\"concerns\": [], \"dismissed_concerns\": []}}`.",
        violation
    )
}

fn validate_finding_object(finding: &Value, idx: usize) -> Result<(), String> {
    let has_problem = finding
        .get("problem")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
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
    let has_preexisting = finding.get("preexisting").is_some_and(Value::is_boolean);
    let has_locations = finding.get("locations").is_some_and(Value::is_array);
    if !has_problem || !has_severity || !has_explanation || !has_preexisting || !has_locations {
        return Err(format!(
            "findings[{idx}] must have non-empty 'problem', valid 'severity' (Low, Medium, High, Critical, or Unknown), non-empty 'severity_explanation', boolean 'preexisting', and a 'locations' array."
        ));
    }
    Ok(())
}

pub fn validate_verification_stage_output(
    output: &VerificationOutput,
    state: &LinuxPatchReviewState,
) -> Result<(), String> {
    for (idx, finding) in output.findings.iter().enumerate() {
        validate_finding_object(finding, idx)?;
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
        let has_preexisting = hard_case.get("preexisting").is_some_and(Value::is_boolean);
        let has_locations = hard_case.get("locations").is_some_and(Value::is_array);
        if !has_type
            || !has_desc
            || !has_severity
            || !has_signal_reason
            || !has_args
            || !has_dismissal_args
            || !has_question
            || !has_preexisting
            || !has_locations
        {
            return Err(format!(
                "hard_cases[{idx}] must have non-empty 'type', 'description', 'signal_reason', 'concern_arguments', and 'verification_question' strings, valid 'estimated_severity' (Low, Medium, High, Critical, or Unknown), string 'dismissal_arguments', boolean 'preexisting', and a 'locations' array."
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
                "dismissed_concerns[{idx}] must include at least one entry in 'locations' with non-empty 'file', 'function_or_symbol', and disproving 'code_snippet'. Unproven or assumption-based dismissals must be classified into 'hard_cases' with signal_reason 'speculative_dismissal'."
            ));
        }
        let matches_raised_concern = state.all_concerns.iter().any(|c| {
            c.get("description")
                .and_then(Value::as_str)
                .is_some_and(|cd| cd.trim().eq_ignore_ascii_case(desc))
        });
        if matches_raised_concern {
            return Err(format!(
                "dismissed_concerns[{idx}] matches a raised concern in all_concerns. Raised or contested concerns must be classified into 'findings' or 'hard_cases', never 'dismissed_concerns'."
            ));
        }
    }

    if !state.all_concerns.is_empty() && output.findings.is_empty() && output.hard_cases.is_empty()
    {
        return Err(
            "At least one concern was raised by the analysis stages, so 'findings' and 'hard_cases' cannot both be empty. Every consolidated concern must be classified either into 'findings' (if well-justified with no competing dismissal) or into 'hard_cases' (if contested, speculative, or requiring tool verification)."
                .to_string(),
        );
    }

    if !state.all_dismissed_concerns.is_empty()
        && output.dismissed_concerns.is_empty()
        && output.hard_cases.is_empty()
    {
        return Err(
            "At least one dismissed_concern was provided in input, so 'dismissed_concerns' and 'hard_cases' cannot both be empty. Every input dismissed_concern must be classified into either 'dismissed_concerns' (if backed by concrete disproving code) or 'hard_cases' (if contested or relying on unproven assumptions)."
                .to_string(),
        );
    }

    validate_verification_source_ids(output, state)?;

    Ok(())
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

fn validate_verification_source_ids(
    output: &VerificationOutput,
    state: &LinuxPatchReviewState,
) -> Result<(), String> {
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

    if concern_ids.is_empty() && dismissed_ids.is_empty() {
        return Ok(());
    }

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

    Ok(())
}

pub fn format_verification_stage_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. Return ONLY a JSON object with 'findings', 'hard_cases', and 'dismissed_concerns' arrays.",
        violation
    )
}

pub fn validate_post_verification_batch_output(
    output: &PostVerificationOutput,
    expected_min_items: usize,
) -> Result<(), String> {
    if output.findings.is_empty() && output.dismissed_concerns.is_empty() {
        return Err(
            "post-verification must not return both empty 'findings' and empty 'dismissed_concerns'. Every candidate hard case must be either validated in 'findings' or disproved in 'dismissed_concerns' with concrete code evidence in 'locations'."
                .to_string(),
        );
    }
    let required = expected_min_items.max(1);
    let total = output.findings.len() + output.dismissed_concerns.len();
    if total < required {
        return Err(format!(
            "post-verification returned {total} item(s) across 'findings' and 'dismissed_concerns', but this batch has {required} candidate hard case(s). Every candidate in the batch must be accounted for in either 'findings' or 'dismissed_concerns'."
        ));
    }
    for (idx, finding) in output.findings.iter().enumerate() {
        validate_finding_object(finding, idx)?;
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
                "dismissed_concerns[{idx}] must include a non-empty 'locations' array with at least one entry containing a non-empty 'code_snippet' proving why the candidate hard case is not a bug."
            ));
        }
    }
    Ok(())
}

pub fn validate_post_verification_batch_items(
    output: &PostVerificationOutput,
    batch: &[Value],
) -> Result<(), String> {
    let expected_ids: Vec<&str> = batch
        .iter()
        .filter_map(|h| h.get("id").and_then(Value::as_str).map(str::trim))
        .filter(|id| !id.is_empty())
        .collect();
    let min_items = if expected_ids.is_empty() {
        batch.len().max(1)
    } else {
        1
    };
    validate_post_verification_batch_output(output, min_items)?;
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
            "post-verification output failed to account for candidate hard case ID(s): {}. Every candidate hard case ID in this batch must appear in 'source_ids' of at least one item in 'findings' or 'dismissed_concerns'.",
            missing.join(", ")
        ));
    }

    Ok(())
}

pub fn validate_post_verification_output(
    output: &PostVerificationOutput,
    _state: &LinuxPatchReviewState,
) -> Result<(), String> {
    validate_post_verification_batch_output(output, 1)
}

pub fn format_post_verification_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. Return ONLY a JSON object with 'findings' and 'dismissed_concerns' arrays.",
        violation
    )
}

fn validate_inline_format(content: &str, _state: &LinuxPatchReviewState) -> Result<(), String> {
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

pub fn push_unique_string(list: &mut Vec<String>, candidate: &str) {
    let trimmed = candidate.trim();
    if !trimmed.is_empty() && !list.iter().any(|existing| existing == trimmed) {
        list.push(trimmed.to_string());
    }
}

pub fn push_unique_prompt(prompts: &mut Vec<String>, candidate: &str) {
    let trimmed = candidate.trim();
    if crate::workflows::guard::sanitize_prompt_relpath(trimmed)
        && !prompts.iter().any(|existing| existing == trimmed)
    {
        prompts.push(trimmed.to_string());
    }
}

pub fn collect_stage_prompts(
    selected_guides: &[String],
    stage_guides: &[&str],
    outcome: &crate::workflow::stage::StageOutcome,
) -> Vec<String> {
    let mut prompts = Vec::new();
    for guide in selected_guides {
        push_unique_prompt(&mut prompts, guide);
    }
    for guide in stage_guides {
        push_unique_prompt(&mut prompts, guide);
    }
    for prompt in outcome.read_prompts() {
        push_unique_prompt(&mut prompts, &prompt);
    }
    prompts
}

pub fn append_stage_items_with_prompts(
    dest: &mut Vec<Value>,
    src: &[Value],
    stage: &str,
    default_type: &str,
    prompts: &[String],
) {
    for item in src {
        let next_id = format!("C{}", dest.len().saturating_add(1));
        let mut obj = item.clone();
        if let Some(map) = obj.as_object_mut() {
            if map
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(|s| s.trim().is_empty())
            {
                map.insert("id".to_string(), json!(next_id));
            }
            if !map.contains_key("type")
                || map
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .is_empty()
            {
                map.insert("type".to_string(), json!(default_type));
            }
            map.insert("stage".to_string(), json!(stage));
            map.insert("stages".to_string(), json!([stage]));
            map.insert("prompts".to_string(), json!(prompts));
        }
        dest.push(obj);
    }
}

pub fn append_stage_dismissed_concerns_with_prompts(
    dest: &mut Vec<Value>,
    src: &[Value],
    stage: &str,
    prompts: &[String],
) {
    for item in src {
        let next_id = format!("D{}", dest.len().saturating_add(1));
        let mut obj = item.clone();
        if let Some(map) = obj.as_object_mut() {
            if map
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(|s| s.trim().is_empty())
            {
                map.insert("id".to_string(), json!(next_id));
            }
            map.insert("stage".to_string(), json!(stage));
            map.insert("stages".to_string(), json!([stage]));
            map.insert("prompts".to_string(), json!(prompts));
        }
        dest.push(obj);
    }
}

pub fn extract_item_stages_and_prompts(
    item: &Value,
    stage_lookup: fn(&str) -> Option<&'static AnalysisStage>,
) -> (Vec<String>, Vec<String>) {
    let mut stages = Vec::new();
    let mut prompts = Vec::new();

    if let Some(stage_str) = item.get("stage").and_then(Value::as_str)
        && let Some(def) = stage_lookup(stage_str)
    {
        push_unique_string(&mut stages, def.name);
    }
    if let Some(arr) = item.get("stages").and_then(Value::as_array) {
        for val in arr {
            if let Some(stage_str) = val.as_str()
                && let Some(def) = stage_lookup(stage_str)
            {
                push_unique_string(&mut stages, def.name);
            }
        }
    }
    if let Some(arr) = item.get("prompts").and_then(Value::as_array) {
        for val in arr {
            if let Some(p) = val.as_str() {
                push_unique_prompt(&mut prompts, p);
            }
        }
    }
    for stage_name in &stages {
        if let Some(def) = stage_lookup(stage_name) {
            for guide in def.guides {
                push_unique_prompt(&mut prompts, guide);
            }
        }
    }

    (stages, prompts)
}

pub fn extra_prompt_paths_for_items(
    selected_guides: &[String],
    items: &[Value],
    stage_lookup: fn(&str) -> Option<&'static AnalysisStage>,
) -> Vec<PathBuf> {
    let mut prompt_names = Vec::new();
    for item in items {
        let (_stages, item_prompts) = extract_item_stages_and_prompts(item, stage_lookup);
        for p in item_prompts {
            push_unique_prompt(&mut prompt_names, &p);
        }
    }

    let mut paths = Vec::new();
    let mut push_path = |pb: PathBuf| {
        if !paths.iter().any(|existing| existing == &pb) {
            paths.push(pb);
        }
    };

    for p in prompt_names {
        if matches!(
            p.as_str(),
            "false-positive-guide.md" | "severity.md" | "review-core.md"
        ) {
            continue;
        }
        let basename = p.rsplit('/').next().unwrap_or(p.as_str());
        if selected_guides
            .iter()
            .any(|g| g == &p || g.as_str() == basename)
        {
            continue;
        }
        if p.contains('/')
            || matches!(
                p.as_str(),
                "callstack.md" | "technical-patterns.md" | "prompt-injection.md"
            )
        {
            push_path(PathBuf::from(&p));
        } else {
            push_path(PathBuf::from(&p));
            push_path(PathBuf::from("subsystem").join(&p));
            push_path(PathBuf::from("patterns").join(&p));
        }
    }

    paths
}

pub fn extra_prompt_paths_for_state(
    state: &LinuxPatchReviewState,
    stage_lookup: fn(&str) -> Option<&'static AnalysisStage>,
) -> Vec<PathBuf> {
    let mut combined =
        Vec::with_capacity(state.all_concerns.len() + state.all_dismissed_concerns.len());
    combined.extend_from_slice(&state.all_concerns);
    combined.extend_from_slice(&state.all_dismissed_concerns);
    extra_prompt_paths_for_items(&state.selected_guides, &combined, stage_lookup)
}

fn normalize_symbol_name(sym: &str) -> &str {
    sym.trim().trim_end_matches("()").trim()
}

fn files_match(a: &str, b: &str) -> bool {
    let a = a.trim().trim_start_matches("./");
    let b = b.trim().trim_start_matches("./");
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b || a.ends_with(&format!("/{b}")) || b.ends_with(&format!("/{a}"))
}

fn symbols_match(a: &str, b: &str) -> bool {
    let a = normalize_symbol_name(a);
    let b = normalize_symbol_name(b);
    !a.is_empty() && !b.is_empty() && a.eq_ignore_ascii_case(b)
}

fn lines_match(a: Option<u64>, b: Option<u64>) -> bool {
    match (a, b) {
        (Some(la), Some(lb)) => la.abs_diff(lb) <= 5,
        _ => false,
    }
}

fn items_share_precise_location(item: &Value, src: &Value) -> bool {
    let (Some(item_locs), Some(src_locs)) = (
        item.get("locations").and_then(Value::as_array),
        src.get("locations").and_then(Value::as_array),
    ) else {
        return false;
    };
    for iloc in item_locs {
        let ifile = iloc.get("file").and_then(Value::as_str).unwrap_or("");
        let isym = iloc
            .get("function_or_symbol")
            .and_then(Value::as_str)
            .unwrap_or("");
        let iline = iloc.get("line").and_then(Value::as_u64);
        for sloc in src_locs {
            let sfile = sloc.get("file").and_then(Value::as_str).unwrap_or("");
            let ssym = sloc
                .get("function_or_symbol")
                .and_then(Value::as_str)
                .unwrap_or("");
            let sline = sloc.get("line").and_then(Value::as_u64);
            if files_match(ifile, sfile) && (symbols_match(isym, ssym) || lines_match(iline, sline))
            {
                return true;
            }
        }
    }
    false
}

fn items_share_file_location(item: &Value, src: &Value) -> bool {
    let (Some(item_locs), Some(src_locs)) = (
        item.get("locations").and_then(Value::as_array),
        src.get("locations").and_then(Value::as_array),
    ) else {
        return false;
    };
    for iloc in item_locs {
        let ifile = iloc.get("file").and_then(Value::as_str).unwrap_or("");
        for sloc in src_locs {
            let sfile = sloc.get("file").and_then(Value::as_str).unwrap_or("");
            if files_match(ifile, sfile) {
                return true;
            }
        }
    }
    false
}

fn items_share_text(item: &Value, src: &Value) -> bool {
    let Some(src_desc) = src
        .get("description")
        .or_else(|| src.get("problem"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| s.len() >= 6)
    else {
        return false;
    };
    let src_lower = src_desc.to_ascii_lowercase();
    for key in [
        "description",
        "problem",
        "concern_arguments",
        "dismissal_arguments",
        "severity_explanation",
        "reasoning",
    ] {
        if let Some(field) = item
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| s.len() >= 6)
        {
            let field_lower = field.to_ascii_lowercase();
            if field_lower.contains(&src_lower) || src_lower.contains(&field_lower) {
                return true;
            }
        }
    }
    false
}

fn extract_id_list(item: &Value, field: &str) -> Vec<String> {
    item.get(field)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn enrich_item_provenance(
    item: &mut Value,
    primary_sources: &[Value],
    secondary_sources: &[Value],
    selected_guides: &[String],
    stage_read_prompts: &[String],
    stage_lookup: fn(&str) -> Option<&'static AnalysisStage>,
) {
    if !item.is_object() {
        return;
    }

    let (mut stages, mut prompts) = extract_item_stages_and_prompts(item, stage_lookup);
    let mut allowed_prompts = Vec::new();
    for g in selected_guides {
        push_unique_prompt(&mut allowed_prompts, g);
    }
    for src in primary_sources.iter().chain(secondary_sources.iter()) {
        let (_s, src_prompts) = extract_item_stages_and_prompts(src, stage_lookup);
        for p in src_prompts {
            push_unique_prompt(&mut allowed_prompts, &p);
        }
    }
    for p in stage_read_prompts {
        push_unique_prompt(&mut allowed_prompts, p);
    }
    prompts.retain(|p| allowed_prompts.iter().any(|ap| ap == p));

    let all_sources: Vec<&Value> = primary_sources
        .iter()
        .chain(secondary_sources.iter())
        .collect();

    let explicit_source_ids = extract_id_list(item, "source_ids");
    let mut matched: Vec<&Value> = if !explicit_source_ids.is_empty() {
        all_sources
            .iter()
            .copied()
            .filter(|src| {
                src.get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| explicit_source_ids.iter().any(|sid| sid == id))
            })
            .collect()
    } else {
        Vec::new()
    };

    if matched.is_empty() {
        matched = all_sources
            .iter()
            .copied()
            .filter(|src| items_share_precise_location(item, src) || items_share_text(item, src))
            .collect();
    }

    if matched.is_empty() {
        matched = all_sources
            .iter()
            .copied()
            .filter(|src| items_share_file_location(item, src))
            .collect();
    }

    if matched.is_empty() && !stages.is_empty() {
        matched = all_sources
            .iter()
            .copied()
            .filter(|src| {
                let (src_stages, _) = extract_item_stages_and_prompts(src, stage_lookup);
                src_stages.iter().any(|ss| stages.iter().any(|s| s == ss))
            })
            .collect();
    }
    if matched.is_empty() && stages.is_empty() {
        let fallback = if !primary_sources.is_empty() {
            primary_sources
        } else {
            secondary_sources
        };
        matched = fallback.iter().collect();
    }

    let mut resolved_source_ids = explicit_source_ids;
    let mut raw_source_ids = extract_id_list(item, "raw_source_ids");

    for g in selected_guides {
        push_unique_prompt(&mut prompts, g);
    }
    let mut inherited_type: Option<String> = None;
    for src in matched {
        if let Some(id) = src.get("id").and_then(Value::as_str) {
            push_unique_string(&mut resolved_source_ids, id);
        }
        for raw_id in extract_id_list(src, "source_ids") {
            push_unique_string(&mut raw_source_ids, &raw_id);
        }
        if inherited_type.is_none()
            && let Some(t) = src
                .get("type")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            inherited_type = Some(t.to_string());
        }
        let (src_stages, src_prompts) = extract_item_stages_and_prompts(src, stage_lookup);
        for s in src_stages {
            push_unique_string(&mut stages, &s);
        }
        for p in src_prompts {
            push_unique_prompt(&mut prompts, &p);
        }
    }
    for s in &stages {
        if let Some(def) = stage_lookup(s) {
            for g in def.guides {
                push_unique_prompt(&mut prompts, g);
            }
        }
        for src in &all_sources {
            let (src_stages, src_prompts) = extract_item_stages_and_prompts(src, stage_lookup);
            if src_stages.iter().any(|ss| ss == s) {
                for p in src_prompts {
                    push_unique_prompt(&mut prompts, &p);
                }
            }
        }
    }
    for p in stage_read_prompts {
        push_unique_prompt(&mut prompts, p);
    }

    if let Some(map) = item.as_object_mut() {
        let has_type = map
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_type && let Some(t) = inherited_type {
            map.insert("type".to_string(), json!(t));
        }
        if !resolved_source_ids.is_empty() {
            map.insert("source_ids".to_string(), json!(resolved_source_ids));
        }
        if !raw_source_ids.is_empty() {
            map.insert("raw_source_ids".to_string(), json!(raw_source_ids));
        }
        if !stages.is_empty() {
            map.insert("stage".to_string(), json!(stages[0]));
            map.insert("stages".to_string(), json!(stages));
        }
        map.insert("prompts".to_string(), json!(prompts));
    }
}

pub fn enrich_verification_output(
    state: &LinuxPatchReviewState,
    out: &mut VerificationOutput,
    outcome: &crate::workflow::stage::StageOutcome,
    stage_lookup: fn(&str) -> Option<&'static AnalysisStage>,
) {
    let read_prompts = outcome.read_prompts();
    for (idx, finding) in out.findings.iter_mut().enumerate() {
        // Category 1a findings originate strictly from C* concerns.
        enrich_item_provenance(
            finding,
            &state.all_concerns,
            &[],
            &state.selected_guides,
            &read_prompts,
            stage_lookup,
        );
        if let Some(map) = finding.as_object_mut() {
            let vf_id = format!("VF{}", idx.saturating_add(1));
            map.insert("id".to_string(), json!(vf_id));
            map.insert("stage_item_id".to_string(), json!(vf_id));
            map.insert("origin".to_string(), json!("verification"));
        }
    }
    for (idx, hard_case) in out.hard_cases.iter_mut().enumerate() {
        // Category 2 hard cases can originate from both C* concerns and D* dismissals.
        let primary_concerns = &state.all_concerns;
        let secondary_dismissals = &state.all_dismissed_concerns;
        enrich_item_provenance(
            hard_case,
            primary_concerns,
            secondary_dismissals,
            &state.selected_guides,
            &read_prompts,
            stage_lookup,
        );
        if let Some(map) = hard_case.as_object_mut() {
            map.insert(
                "id".to_string(),
                json!(format!("H{}", idx.saturating_add(1))),
            );
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
        enrich_item_provenance(
            dismissed,
            &state.all_dismissed_concerns,
            &[],
            &state.selected_guides,
            &read_prompts,
            stage_lookup,
        );
        if let Some(map) = dismissed.as_object_mut() {
            map.insert(
                "id".to_string(),
                json!(format!("VD{}", idx.saturating_add(1))),
            );
            map.insert("origin".to_string(), json!("verification"));
        }
    }
}

pub fn enrich_post_verification_output(
    selected_guides: &[String],
    batch: &[Value],
    out: &mut PostVerificationOutput,
    outcome: &crate::workflow::stage::StageOutcome,
    stage_lookup: fn(&str) -> Option<&'static AnalysisStage>,
) {
    let read_prompts = outcome.read_prompts();
    for finding in &mut out.findings {
        enrich_item_provenance(
            finding,
            batch,
            &[],
            selected_guides,
            &read_prompts,
            stage_lookup,
        );
    }
    for dismissed in &mut out.dismissed_concerns {
        enrich_item_provenance(
            dismissed,
            batch,
            &[],
            selected_guides,
            &read_prompts,
            stage_lookup,
        );
    }
}

// ---------------------------------------------------------------------------
// Stage Definitions
// ---------------------------------------------------------------------------

pub fn prescreen_stage() -> Stage<LinuxPatchReviewState, PrescreenOutput> {
    Stage::builder("pre-screen")
        .system_prompt(PromptTemplate::<LinuxPatchReviewState>::new(
            "You are an AI assistant preparing a Linux kernel patch review.\nReview the provided Patch and select all potentially relevant subsystem guides from the index below.\nCRITICAL BIAS RULE: You MUST err on the side of inclusion. Only exclude a guide if it is 100% irrelevant to the modified code. If there is any doubt, include the file.\n\nYou MUST respond with ONLY a JSON object, no other text. Example:\n```json\n{\"selected_prompts\": [\"networking.md\", \"locking.md\"]}\n```",
        ))
        .user_prompt(
            PromptTemplate::<LinuxPatchReviewState>::new(
                "<subsystem_guide_index>\n@include(\"subsystem/subsystem.md\")\n</subsystem_guide_index>\n\n<patch>\n{{target_commit_diff}}\n</patch>",
            )
            .with_var("target_commit_diff", |s: &LinuxPatchReviewState| s.target_commit_diff.clone())
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
        .reduce(|state, out: PrescreenOutput| {
            let prompts: Vec<String> = out
                .selected_prompts
                .into_iter()
                .filter(|name| !is_stage_exclusive_guide(name))
                .filter(|name| crate::workflows::guard::sanitize_guide_name(name))
                .collect();
            state.selected_guides = prompts;
        })

        .build()
}

pub fn planning_stage() -> Stage<LinuxPatchReviewState, PlanningOutput> {
    let optional_stages: Vec<&'static str> = ANALYSIS_STAGES
        .iter()
        .filter(|d| d.optional)
        .map(|d| d.name)
        .collect();

    Stage::builder("planning")
        .system_prompt(linux_system_prompt(true))
        .user_prompt(PromptTemplate::<LinuxPatchReviewState>::new(
            r#"Analyze the provided patch and determine which of the following review stages are relevant and should be executed:
- resources: Resource management
- locking: Locking and synchronization
- security: Security audit
- hardware: Hardware engineer's review

CRITICAL: Always err on the side of running more stages. If you are not absolutely sure, include the stage. If the patch is a trivial typo fix, you may omit some stages. Stages not listed above always run and should not be included in your answer.

You MUST respond with ONLY a JSON object, no other text. Use the names exactly as given above. Example:
```json
{"relevant_stages": ["resources", "locking", "security", "hardware"]}
```"#,
        ))
        .output_format(OutputFormat::json_with_schema(json!({
            "type": "object",
            "properties": {
                "relevant_stages": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": optional_stages,
                    }
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
            // The stages the planner is not asked about run regardless. Its
            // answer is then admitted only where it names an optional stage,
            // which is what stops a hallucinated name reaching the resolver.
            let mut stages: Vec<String> = ANALYSIS_STAGES
                .iter()
                .filter(|d| !d.optional)
                .map(|d| d.name.to_string())
                .collect();
            for raw_name in out.relevant_stages {
                if let Some(def) = analysis_stage_by_name(&raw_name) {
                    if def.optional && !stages.iter().any(|s| s == def.name) {
                        stages.push(def.name.to_string());
                    }
                } else {
                    tracing::warn!("Ignoring unknown planned review stage {:?}", raw_name);
                }
            }
            state.planned_stages = stages;
        })
        .build()
}

/// One analysis stage: everything that distinguishes it from its siblings.
///
/// The workflow already identifies stages by name, so `name` is the whole
/// identity: it is what `--stages` selects, what the planning stage returns,
/// what is attached to a concern to say which analyst raised it, and what the
/// progress display shows. Properties that used to be inferred from a stage's
/// number live here instead, where they cannot fall out of step with it.
pub struct AnalysisStage {
    /// Stable identifier. Lowercase, hyphenated, and never renamed casually:
    /// it appears in `--stages` and in stored review output.
    pub name: &'static str,
    /// Short label for the progress display.
    pub short: &'static str,
    pub instruction: &'static str,
    pub guides: &'static [&'static str],
    /// Whether the system prompt carries the commit message as well as the
    /// diff: the git show output with the changelog injected, rather than the
    /// hunks alone. Stages that judge the change against its stated intent
    /// need it; those reading the hunks on their own terms do not.
    pub uses_commit_log: bool,
    /// Whether the planning stage may leave this one out. The first three
    /// always run, so the planner is only ever asked about the rest.
    pub optional: bool,
    /// Whether the prompt carries the list of patches that follow this one in
    /// the series. See [`SERIES_CONTEXT_PLACEHOLDER`].
    pub wants_series_context: bool,
}

pub static ANALYSIS_STAGES: &[AnalysisStage] = &[
    AnalysisStage {
        name: "goal",
        short: "Goal Analysis",
        instruction: STAGE_GOAL_INSTRUCTION,
        guides: &[],
        uses_commit_log: true,
        optional: false,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "implementation",
        short: "Implementation",
        instruction: STAGE_IMPLEMENTATION_INSTRUCTION,
        guides: &[],
        uses_commit_log: true,
        optional: false,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "execution-flow",
        short: "Execution Flow",
        instruction: STAGE_EXECUTION_FLOW_INSTRUCTION,
        guides: &["callstack.md", "technical-patterns.md"],
        uses_commit_log: false,
        optional: false,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "resources",
        short: "Resource Mgmt",
        instruction: STAGE_RESOURCES_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "locking",
        short: "Locking & Sync",
        instruction: STAGE_LOCKING_INSTRUCTION,
        guides: &["subsystem/locking.md"],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "security",
        short: "Security Audit",
        instruction: STAGE_SECURITY_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "hardware",
        short: "Hardware Review",
        instruction: STAGE_HARDWARE_INSTRUCTION,
        guides: &[],
        uses_commit_log: true,
        optional: true,
        wants_series_context: false,
    },
];

/// The consolidation stages, in the order the workflow runs them. They take no
/// per-stage configuration, so a name and a display label is all there is to
/// hold, but holding it once keeps the list from being restated wherever a
/// stage name has to be recognised or shown.
pub struct ConsolidationStage {
    pub name: &'static str,
    pub short: &'static str,
    /// Whether the prompt carries the list of patches that follow this one in
    /// the series. See [`SERIES_CONTEXT_PLACEHOLDER`].
    pub wants_series_context: bool,
}

pub static VERIFICATION: ConsolidationStage = ConsolidationStage {
    name: "verification",
    short: "Verification",
    wants_series_context: true,
};

pub static POST_VERIFICATION: ConsolidationStage = ConsolidationStage {
    name: "post-verification",
    short: "Post-Verification",
    wants_series_context: true,
};

pub static REPORT: ConsolidationStage = ConsolidationStage {
    name: "report",
    short: "Report Generation",
    wants_series_context: false,
};

/// In the order the workflow runs them. Each builder refers to its own
/// definition above, so the name a stage registers under is the same string
/// this list recognises and labels.
pub static CONSOLIDATION_STAGES: &[&ConsolidationStage] =
    &[&VERIFICATION, &POST_VERIFICATION, &REPORT];

/// Maximum number of parallel post-verification stages executed concurrently.
pub const MAX_POST_VERIFICATION_STAGES: usize = 10;

/// Static stage names for parallel post-verification fan-out.
pub static POST_VERIFICATION_STAGE_NAMES: [&str; MAX_POST_VERIFICATION_STAGES] = [
    "post-verification-1",
    "post-verification-2",
    "post-verification-3",
    "post-verification-4",
    "post-verification-5",
    "post-verification-6",
    "post-verification-7",
    "post-verification-8",
    "post-verification-9",
    "post-verification-10",
];

/// Marks where a stage's prompt carries the list of patches that follow this
/// one in the series.
///
/// Two kinds of question need it. Where a test belongs in a series is only
/// answerable from what comes after it, and whether a concern still stands can
/// depend on a later patch reworking the code it is about. Both are declared in
/// the stage tables rather than wired up per builder, so the placeholder and
/// the variable that fills it cannot get separated.
pub const SERIES_CONTEXT_PLACEHOLDER: &str = "{{follow_up_series_section}}";

fn series_context_placeholder(wants: bool) -> &'static str {
    if wants {
        SERIES_CONTEXT_PLACEHOLDER
    } else {
        ""
    }
}

fn with_series_context(
    template: PromptTemplate<LinuxPatchReviewState>,
    wants: bool,
) -> PromptTemplate<LinuxPatchReviewState> {
    if !wants {
        return template;
    }
    template.with_var("follow_up_series_section", |s: &LinuxPatchReviewState| {
        s.follow_up_series_context
            .as_ref()
            .map(|ctx| format!("\n\n{}", ctx))
            .unwrap_or_default()
    })
}

use crate::workflows::guard::normalize_stage_name;

pub fn consolidation_stage_by_name(name: &str) -> Option<&'static ConsolidationStage> {
    let normalized = normalize_stage_name(name);
    if POST_VERIFICATION_STAGE_NAMES.contains(&normalized.as_str()) {
        return Some(&POST_VERIFICATION);
    }
    CONSOLIDATION_STAGES
        .iter()
        .copied()
        .find(|s| s.name == normalized)
}

/// Display label for any stage the pipeline runs.
pub fn stage_short_label(name: &str) -> Option<&'static str> {
    if let Some(def) = analysis_stage_by_name(name) {
        return Some(def.short);
    }
    consolidation_stage_by_name(name).map(|s| s.short)
}

/// Whether a guide belongs to one stage rather than to the shared context.
///
/// The pre-screen offers a guide to the whole review, but a guide some stage
/// loads for itself would then arrive twice: once in that stage's user prompt
/// and again in every stage's system prompt. Deriving the answer from the
/// stage table means a guide claimed in the table is excluded by that fact
/// alone, with no second list to keep in step.
pub fn is_stage_exclusive_guide(name: &str) -> bool {
    ANALYSIS_STAGES
        .iter()
        .flat_map(|def| def.guides)
        .any(|guide| guide.rsplit('/').next() == Some(name))
}

pub fn analysis_stage_by_name(name: &str) -> Option<&'static AnalysisStage> {
    let normalized = normalize_stage_name(name);
    ANALYSIS_STAGES.iter().find(|s| s.name == normalized)
}

/// Every stage name a review can produce, analysis and consolidation alike,
/// for validating what a caller or the planner asked for.
pub fn is_known_stage(name: &str) -> bool {
    let normalized = normalize_stage_name(name);
    analysis_stage_by_name(&normalized).is_some()
        || consolidation_stage_by_name(&normalized).is_some()
        || matches!(normalized.as_str(), "pre-screen" | "planning")
}

fn analysis_stage(
    def: &'static AnalysisStage,
    max_turns: usize,
    temperature: f32,
) -> Box<dyn ExecutableStage<LinuxPatchReviewState>> {
    let mut user_template = PromptTemplate::<LinuxPatchReviewState>::new(format!(
        "{}\n\n{}{}",
        def.instruction,
        STAGE_JSON_SCHEMA_EXAMPLE,
        series_context_placeholder(def.wants_series_context)
    ));
    for guide in def.guides {
        user_template = user_template.include_file(*guide);
    }
    let user_template = with_series_context(user_template, def.wants_series_context);

    Box::new(
        Stage::builder(def.name)
            .system_prompt(linux_system_prompt(def.uses_commit_log))
            .user_prompt(user_template)
            .output_format(
                OutputFormat::json()
                    .with_validator(validate_concerns_output)
                    .with_feedback_formatter(format_concerns_feedback),
            )
            .policy(StagePolicy {
                tools: ToolScope::All,
                max_turns,
                temperature,
                ..Default::default()
            })
            .reduce_with_outcome(
                move |state: &mut LinuxPatchReviewState,
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

pub fn resolve_analysis_stages_with_options(
    state: &LinuxPatchReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<LinuxPatchReviewState>>> {
    let selected_stages: Vec<String> = if let Some(ref manual) = state.manual_stages {
        manual.clone()
    } else if !state.planned_stages.is_empty() {
        state.planned_stages.clone()
    } else {
        ANALYSIS_STAGES.iter().map(|d| d.name.to_string()).collect()
    };

    let mut stages = Vec::new();
    for name in selected_stages {
        match analysis_stage_by_name(&name) {
            Some(def) => stages.push(analysis_stage(def, max_turns, temperature)),
            // Previously an unrecognised entry was dropped in silence, so a
            // mistyped --stages looked like it had worked.
            None => tracing::warn!("Ignoring unknown review stage {:?}", name),
        }
    }
    stages
}

fn estimated_severity_rank(item: &Value) -> u8 {
    match item
        .get("estimated_severity")
        .or_else(|| item.get("severity"))
        .and_then(Value::as_str)
        .unwrap_or("Medium")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "critical" => 4,
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 2,
    }
}

/// Groups `hard_cases` into at most [`MAX_POST_VERIFICATION_STAGES`] batches.
///
/// When `hard_cases.len() <= MAX_POST_VERIFICATION_STAGES`, each item gets its
/// own 1-item batch so every candidate is verified in an independent parallel
/// stage. When `hard_cases.len() > MAX_POST_VERIFICATION_STAGES`, candidates
/// are sorted by `estimated_severity` descending (`Critical` > `High` >
/// `Medium` > `Low`): the highest-severity items receive dedicated 1-item
/// stages and the lowest-severity items are batched together into the tail
/// stages so total parallel stages never exceed [`MAX_POST_VERIFICATION_STAGES`].
pub fn batch_hard_cases_by_severity(hard_cases: &[Value]) -> Vec<Vec<Value>> {
    if hard_cases.is_empty() {
        return Vec::new();
    }
    if hard_cases.len() <= MAX_POST_VERIFICATION_STAGES {
        return hard_cases.iter().map(|item| vec![item.clone()]).collect();
    }

    let mut indexed: Vec<(usize, Value)> = hard_cases.iter().cloned().enumerate().collect();
    indexed.sort_by(|(idx_a, a), (idx_b, b)| {
        estimated_severity_rank(b)
            .cmp(&estimated_severity_rank(a))
            .then_with(|| idx_a.cmp(idx_b))
    });
    let sorted: Vec<Value> = indexed.into_iter().map(|(_, v)| v).collect();

    let total = sorted.len();
    let extra = total - MAX_POST_VERIFICATION_STAGES;
    let tail_stages = extra.min(MAX_POST_VERIFICATION_STAGES);
    let solo_stages = MAX_POST_VERIFICATION_STAGES - tail_stages;

    let mut batches = Vec::with_capacity(MAX_POST_VERIFICATION_STAGES);
    for item in sorted.iter().take(solo_stages) {
        batches.push(vec![item.clone()]);
    }

    let remaining = &sorted[solo_stages..];
    let rem_len = remaining.len();
    let base_size = rem_len / tail_stages;
    let remainder = rem_len % tail_stages;

    let mut offset = 0;
    for stage_idx in 0..tail_stages {
        let extra_one = usize::from(stage_idx >= tail_stages - remainder);
        let size = base_size + extra_one;
        batches.push(remaining[offset..offset + size].to_vec());
        offset += size;
    }

    batches
}

pub(crate) fn record_verified_findings(state: &mut LinuxPatchReviewState, findings: Vec<Value>) {
    for mut finding in findings {
        if let Some(map) = finding.as_object_mut()
            && let Some(existing_id) = map.get("id").and_then(Value::as_str).map(str::to_string)
            && (existing_id.starts_with("VF") || existing_id.starts_with("PVF"))
        {
            map.insert("stage_item_id".to_string(), json!(existing_id));
        }

        let is_preexisting = finding
            .get("preexisting")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_preexisting {
            let mut concern = json!({
                "type": finding.get("problem").and_then(|v| v.as_str()).unwrap_or("Pre-existing Issue"),
                "description": finding.get("problem").and_then(|v| v.as_str()).unwrap_or(""),
                "reasoning": finding.get("severity_explanation").and_then(|v| v.as_str()).unwrap_or(""),
                "severity": finding.get("severity").and_then(|v| v.as_str()).unwrap_or("Unknown"),
                "preexisting": true,
                "locations": finding.get("locations").cloned().unwrap_or(json!([])),
            });
            if let Some(map) = concern.as_object_mut() {
                for key in [
                    "id",
                    "stage_item_id",
                    "source_ids",
                    "raw_source_ids",
                    "origin",
                    "post_verification_stage",
                    "stage",
                    "stages",
                    "prompts",
                ] {
                    if let Some(val) = finding.get(key).cloned() {
                        map.insert(key.to_string(), val);
                    }
                }
            }
            state.concerns.push(concern);
            if state.report_preexisting {
                state.findings.push(finding);
            }
        } else {
            state.findings.push(finding);
        }
    }
}

pub fn apply_verification_stage_output(
    state: &mut LinuxPatchReviewState,
    mut out: VerificationOutput,
    outcome: &crate::workflow::stage::StageOutcome,
    stage_lookup: fn(&str) -> Option<&'static AnalysisStage>,
) {
    enrich_verification_output(state, &mut out, outcome, stage_lookup);
    state.verification_findings.extend(out.findings.clone());
    state
        .verification_dismissed
        .extend(out.dismissed_concerns.clone());
    record_verified_findings(state, out.findings);
    state.hard_cases = out.hard_cases;
    state
        .deduplicated_dismissed_concerns
        .extend(out.dismissed_concerns);
}

pub fn apply_post_verification_stage_output(
    state: &mut LinuxPatchReviewState,
    stage_name: &str,
    batch: &[Value],
    mut out: PostVerificationOutput,
    outcome: &crate::workflow::stage::StageOutcome,
    stage_lookup: fn(&str) -> Option<&'static AnalysisStage>,
) {
    enrich_post_verification_output(
        &state.selected_guides,
        batch,
        &mut out,
        outcome,
        stage_lookup,
    );
    for finding in &mut out.findings {
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
    record_verified_findings(state, out.findings);
    state
        .deduplicated_dismissed_concerns
        .extend(out.dismissed_concerns);
}

pub fn verification_stage(
    max_turns: usize,
    temperature: f32,
) -> Stage<LinuxPatchReviewState, VerificationOutput> {
    let series_context = series_context_placeholder(VERIFICATION.wants_series_context);
    let user_template = with_series_context(
        PromptTemplate::<LinuxPatchReviewState>::new(format!(
            r#"{STAGE_VERIFICATION_INSTRUCTION}

<false_positive_guide>
@include("false-positive-guide.md")
</false_positive_guide>

<severity_guidelines>
@include("severity.md")
</severity_guidelines>@includes

CRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must have concrete evidence in the code that proves the concern is invalid. Never drop a raised concern into 'dismissed_concerns' in this stage: every consolidated concern must be placed either in 'findings' (if well-justified with concrete code proof and no competing dismissal) or in 'hard_cases' (if contested, speculative, or requiring tool verification). Also inspect every standalone dismissed_concern: if it dismissed a plausible bug using an unverified assumption, promote it into 'hard_cases' with '"signal_reason": "speculative_dismissal"'.{series_context}

Aggregated Concerns:
{{{{aggregated_concerns}}}}

Aggregated Dismissed Concerns:
{{{{aggregated_dismissed_concerns}}}}

Return ONLY a JSON object with 'findings', 'hard_cases', and 'dismissed_concerns' arrays.
- LINEAGE REQUIREMENT ('source_ids'): Every input item in Aggregated Concerns has an "id" ("C1", "C2", ...) and every input item in Aggregated Dismissed Concerns has an "id" ("D1", "D2", ...). Every output object across 'findings', 'hard_cases', and 'dismissed_concerns' MUST include a non-empty "source_ids" array listing the exact input "id" string(s) merged into that item (e.g. ["C1", "D2"]). Every input "id" (all C* and D* IDs) MUST appear in at least one output item's "source_ids". Category 1a 'findings' may ONLY reference C* IDs (any contested C* + D* or promoted D* must go to 'hard_cases'), and Category 1b 'dismissed_concerns' may ONLY reference D* IDs (never C* IDs).
- Each object in 'findings' (Category 1a: Well-Justified Concerns) MUST use the keys: "source_ids" (non-empty array of C* input IDs), "problem" (a short naming string under 80 characters, preferably starting with a subsystem prefix like 'mm:' or 'bpf:', NEVER using backquotes, using fn_name() format for functions, describing the root cause), "severity" ("Low", "Medium", "High", "Critical", or "Unknown"), "severity_explanation" (detailed reasoning and proof), "preexisting" (boolean), and "locations" (array of location objects), and may include "stages" (array of stage names that raised the merged concern) and "prompts" (array of prompt files from the merged concern).
- Each object in 'hard_cases' (Category 2: Speculative or Contested) MUST use the keys: "source_ids" (non-empty array of input IDs), "type", "description", "estimated_severity" ("Low", "Medium", "High", "Critical", or "Unknown"), "signal_reason" ("mixed_signals", "speculative_concern", "speculative_dismissal", "series_interaction", or "other"), "concern_arguments" (consolidated arguments for why the bug can occur), "dismissal_arguments" (consolidated arguments/snippets from any competing or standalone dismissal, or "" if none), "verification_question" (the specific code question post-verification must answer with tools), "preexisting" (boolean), and "locations" (array of location objects), and may include "stages" and "prompts".
- Each object in 'dismissed_concerns' (Category 1b: Well-Justified Dismissals) MUST use the keys: "source_ids" (non-empty array of D* input IDs), "type", "description", "reasoning", and "locations", and may include "stages" and "prompts".

Example Output:
```json
{{
  "findings": [
    {{
      "source_ids": ["C1"],
      "problem": "mm: memory leak in func_x() due to unmet condition Y",
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
  ],
  "hard_cases": [
    {{
      "source_ids": ["D1"],
      "type": "Resource Management",
      "description": "Potential leak of child node in parse_tree() on error return",
      "estimated_severity": "Medium",
      "signal_reason": "speculative_dismissal",
      "concern_arguments": "parse_tree() allocates node via kzalloc() and returns -EINVAL on line 88 without freeing it.",
      "dismissal_arguments": "Dismissed by resources stage assuming caller frees partial state on error.",
      "verification_question": "Inspect all callers of parse_tree() using git_grep and git_read_files to verify whether node is reachable or freed when parse_tree() returns an error.",
      "preexisting": false,
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "parse_tree",
          "line": 88,
          "code_snippet": "return -EINVAL;",
          "why_this_location_matters": "Early error return after allocation without local kfree."
        }}
      ]
    }}
  ],
  "dismissed_concerns": []
}}
```"#
        ))
        .include_file("false-positive-guide.md")
        .include_file("severity.md")
        .include_files_from_state(|s: &LinuxPatchReviewState| {
            extra_prompt_paths_for_state(s, analysis_stage_by_name)
        }),
        VERIFICATION.wants_series_context,
    )
    .with_var("aggregated_concerns", |s: &LinuxPatchReviewState| {
        serde_json::to_string_pretty(&s.all_concerns).unwrap_or_default()
    })
    .with_var(
        "aggregated_dismissed_concerns",
        |s: &LinuxPatchReviewState| {
            serde_json::to_string_pretty(&s.all_dismissed_concerns).unwrap_or_default()
        },
    );

    Stage::builder(VERIFICATION.name)
        .system_prompt(linux_system_prompt(true))
        .user_prompt(user_template)
        .output_format(
            OutputFormat::json()
                .with_validator(validate_verification_stage_output)
                .with_feedback_formatter(format_verification_stage_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce_with_outcome(|state, out: VerificationOutput, outcome| {
            apply_verification_stage_output(state, out, outcome, analysis_stage_by_name);
        })
        .build()
}

pub fn post_verification_stage(
    stage_name: &'static str,
    batch: Vec<Value>,
    max_turns: usize,
    temperature: f32,
) -> Stage<LinuxPatchReviewState, PostVerificationOutput> {
    let candidate_json = serde_json::to_string_pretty(&batch).unwrap_or_default();
    let batch_for_prompts = batch.clone();
    let batch_for_validate = batch.clone();
    let batch_for_reduce = batch;
    let series_context = series_context_placeholder(POST_VERIFICATION.wants_series_context);
    let user_template = with_series_context(
        PromptTemplate::<LinuxPatchReviewState>::new(format!(
            r#"{STAGE_POST_VERIFICATION_INSTRUCTION}

<false_positive_guide>
@include("false-positive-guide.md")
</false_positive_guide>

<severity_guidelines>
@include("severity.md")
</severity_guidelines>@includes

CRITICAL REVIEW DIRECTIVE: To dismiss a candidate issue as a false positive, you must find concrete evidence in the code that proves the issue is invalid (e.g., verifying with tools that the caller or callee prevents the exact failure mode) and quote that disproving code in `dismissed_concerns[].locations`. If you cannot find concrete proof of safety, you must validate and report the finding in `findings`.{series_context}

Candidate Hard Case(s) to Verify:
{{{{candidate_hard_cases}}}}

Return ONLY a JSON object with 'findings' and 'dismissed_concerns' arrays. Every candidate in this batch MUST be accounted for in either 'findings' (if validated) or 'dismissed_concerns' (ONLY if concrete code disproves the candidate; never return both empty arrays).
- LINEAGE REQUIREMENT ('source_ids'): Each candidate hard case in this batch has an "id" (e.g. "H1"). Every object in 'findings' and 'dismissed_concerns' MUST include a non-empty "source_ids" array listing the candidate "id"(s) from this batch that it resolves (e.g. ["H1"]), and every candidate "id" in this batch must be accounted for.
- Each object in 'findings' MUST use: "source_ids" (non-empty array of candidate H* IDs from this batch), "problem" (a short naming string containing the vulnerability description. BUG NAME RULES: 1) less than 80 characters, 2) preferably start with a short subsystem prefix like 'mm:' or 'bpf:', 3) NEVER use backquotes, 4) if referring to a function, use fn_name() format, 5) try to describe the root cause instead of the consequence of the problem), "severity" (a string: Low, Medium, High, Critical, or Unknown), "severity_explanation" (a string detailing the reasoning and proof), "preexisting" (a boolean: false whenever the patch introduces, modifies, triggers, exposes, or relies on the buggy code path, caller/callee interaction, or error cleanup path, even if the underlying helper, check, or cleanup label already existed; true ONLY if the bug is in untouched code whose reachability, inputs, and behavior are completely unaffected by the reviewed patchset), "locations" (an array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters). Carry forward the locations from the validated candidate; if you gather better evidence, replace vague locations with the most precise verified locations. Do not invent line numbers; use null when exact values are unknown.
- Each object in 'dismissed_concerns' MUST use: "source_ids" (non-empty array of candidate H* IDs from this batch), "description" (the candidate issue that was disproved), "reasoning" (step-by-step explanation of how the inspected code disproves the candidate), and "locations" (a non-empty array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters, quoting the verbatim disproving guard, lock, cleanup path, or caller/callee implementation).

Example Output:
```json
{{
  "findings": [
    {{
      "source_ids": ["H1"],
      "problem": "mm: memory leak in func_x() due to unmet condition Y",
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
  ],
  "dismissed_concerns": [
    {{
      "source_ids": ["H2"],
      "description": "Possible missing cleanup when foo_init() fails after bar_alloc().",
      "reasoning": "Inspecting caller_fn() confirms bar_free() is unconditionally invoked in the err_out cleanup path.",
      "locations": [
        {{
          "file": "path/to/file.c",
          "function_or_symbol": "caller_fn",
          "line": 125,
          "code_snippet": "err_out:\n\tbar_free(bar);",
          "why_this_location_matters": "This caller error path frees the resource when foo_init() returns an error."
        }}
      ]
    }}
  ]
}}
```"#
        ))
        .include_file("false-positive-guide.md")
        .include_file("severity.md")
        .include_files_from_state(move |s: &LinuxPatchReviewState| {
            extra_prompt_paths_for_items(&s.selected_guides, &batch_for_prompts, analysis_stage_by_name)
        }),
        POST_VERIFICATION.wants_series_context,
    )
    .with_var("candidate_hard_cases", move |_: &LinuxPatchReviewState| {
        candidate_json.clone()
    });

    Stage::builder(stage_name)
        .system_prompt(linux_system_prompt(true))
        .user_prompt(user_template)
        .output_format(
            OutputFormat::json()
                .with_validator(move |out, _state| {
                    validate_post_verification_batch_items(out, &batch_for_validate)
                })
                .with_feedback_formatter(format_post_verification_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce_with_outcome(move |state, out: PostVerificationOutput, outcome| {
            apply_post_verification_stage_output(
                state,
                stage_name,
                &batch_for_reduce,
                out,
                outcome,
                analysis_stage_by_name,
            );
        })
        .build()
}

pub fn post_verification_stage_for_batch(
    stage_name: &'static str,
    batch: Vec<Value>,
    max_turns: usize,
    temperature: f32,
) -> Box<dyn ExecutableStage<LinuxPatchReviewState>> {
    Box::new(post_verification_stage(
        stage_name,
        batch,
        max_turns,
        temperature,
    ))
}

pub fn resolve_post_verification_stages_with_options(
    state: &LinuxPatchReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<LinuxPatchReviewState>>> {
    let batches = batch_hard_cases_by_severity(&state.hard_cases);
    batches
        .into_iter()
        .enumerate()
        .map(|(idx, batch)| {
            let stage_name = POST_VERIFICATION_STAGE_NAMES[idx];
            post_verification_stage_for_batch(stage_name, batch, max_turns, temperature)
        })
        .collect()
}

pub fn report_stage(max_turns: usize, temperature: f32) -> Stage<LinuxPatchReviewState, String> {
    Stage::builder(REPORT.name)
        .system_prompt(linux_system_prompt(true))
        .user_prompt(
            PromptTemplate::<LinuxPatchReviewState>::new(format!(
                r#"{STAGE_REPORT_INSTRUCTION}

Findings:
{{{{findings}}}}

Return raw text output, not JSON."#
            ))
            .include_file("inline-template.md")
            .with_var("findings", |s: &LinuxPatchReviewState| {
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
                reminder: "CRITICAL OVERRIDE: Your previous response was blocked by the API recitation filter for quoting the patch diff verbatim. Ignore the diff-quoting instructions in inline-template.md. Do NOT quote any code or diff lines with '>'. Instead, start with the Commit/Author/Subject headers, write the summary, and describe each finding directly by referencing file names, function names, and line numbers in plain prose.".to_string(),
            },
            ..Default::default()
        })
        .skip_if(|s| s.skip_report)
        .reduce(|state, out: String| {
            state.review_inline = out;
        })
        .build()
}

// ---------------------------------------------------------------------------
// Complete Kernel Review Workflow Graph
// ---------------------------------------------------------------------------

/// Constructs the complete declarative workflow for Linux kernel patch review.
pub fn build_linux_patch_review_workflow() -> Workflow<LinuxPatchReviewState> {
    build_linux_patch_review_workflow_with_options(20, 0.0)
}

/// Constructs the declarative workflow with custom per-stage interaction limits and temperature.
pub fn build_linux_patch_review_workflow_with_options(
    max_turns: usize,
    temperature: f32,
) -> Workflow<LinuxPatchReviewState> {
    Workflow::builder("linux_patch_review")
        .stage(prescreen_stage())
        .dynamic_parallel(
            planning_stage(),
            move |state| resolve_analysis_stages_with_options(state, max_turns, temperature),
            ParallelPolicy::BestEffort,
        )
        .early_exit_if(
            |s| s.all_concerns.is_empty() && s.all_dismissed_concerns.is_empty(),
            "No concerns or dismissed concerns raised in initial analysis stages",
        )
        .dynamic_parallel(
            verification_stage(max_turns, temperature),
            move |state| {
                resolve_post_verification_stages_with_options(state, max_turns, temperature)
            },
            ParallelPolicy::BestEffort,
        )
        .early_exit_if(
            |s| s.findings.is_empty(),
            "No findings validated in verification stages",
        )
        .stage(report_stage(max_turns, temperature))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_each_stage_declares_whether_it_needs_the_commit_message() {
        // This was a numeric range in a free function, which kept compiling
        // while meaning something else whenever the stages moved.
        for name in ["goal", "implementation", "hardware"] {
            assert!(
                analysis_stage_by_name(name).unwrap().uses_commit_log,
                "{name} judges the change against its stated intent"
            );
        }
        for name in ["execution-flow", "resources", "locking", "security"] {
            assert!(
                !analysis_stage_by_name(name).unwrap().uses_commit_log,
                "{name} reads the diff hunks on their own terms"
            );
        }
    }

    #[test]
    fn test_stage_names_are_unique_and_resolvable() {
        let mut seen = std::collections::BTreeSet::new();
        for def in ANALYSIS_STAGES {
            assert!(seen.insert(def.name), "duplicate stage name {}", def.name);
            assert!(is_known_stage(def.name));
            assert!(std::ptr::eq(analysis_stage_by_name(def.name).unwrap(), def));
        }
        assert!(analysis_stage_by_name("nonexistent").is_none());
        assert!(!is_known_stage("nonexistent"));
    }

    #[test]
    fn test_series_context_is_declared_in_the_tables() {
        // Both verification and post-verification ask whether a later patch
        // reworks the code a concern is about.
        assert!(
            consolidation_stage_by_name("verification")
                .unwrap()
                .wants_series_context
        );
        assert!(
            consolidation_stage_by_name("post-verification")
                .unwrap()
                .wants_series_context
        );
        assert!(
            !consolidation_stage_by_name("report")
                .unwrap()
                .wants_series_context
        );
        // The placeholder and the variable that fills it travel together, so a
        // stage that declares the flag cannot end up rendering it literally.
        assert_eq!(series_context_placeholder(true), SERIES_CONTEXT_PLACEHOLDER);
        assert_eq!(series_context_placeholder(false), "");
    }

    #[test]
    fn test_no_analysis_stage_asks_for_series_context_yet() {
        // Both builders honour the flag; nothing in the analysis table sets it
        // until a stage needs it. A stage that did would get the placeholder
        // and the variable together, never one without the other.
        for def in ANALYSIS_STAGES {
            assert!(!def.wants_series_context, "{} does not use it", def.name);
        }
    }

    #[test]
    fn test_stage_exclusive_guides_follow_the_stage_table() {
        // Claimed by a stage, so the pre-screen must not also broadcast them.
        assert!(is_stage_exclusive_guide("locking.md"));
        assert!(is_stage_exclusive_guide("callstack.md"));
        assert!(is_stage_exclusive_guide("technical-patterns.md"));

        // Not claimed by any stage: the pre-screen's to offer.
        assert!(!is_stage_exclusive_guide("mm-vma.md"));
        assert!(!is_stage_exclusive_guide("subsystem.md"));

        // Matched on the file name, since that is what the pre-screen returns,
        // while the table holds the path a stage includes it by.
        assert!(
            ANALYSIS_STAGES
                .iter()
                .any(|d| d.guides.contains(&"subsystem/locking.md"))
        );
        assert!(!is_stage_exclusive_guide("subsystem/locking.md"));
    }

    #[test]
    fn test_every_stage_the_workflow_builds_is_a_known_name() {
        // The builders name their stages with literals; this is what stops one
        // drifting from the table that has to recognise and display it.
        for name in [
            verification_stage(1, 1.0).name(),
            report_stage(1, 1.0).name(),
        ] {
            assert!(is_known_stage(name), "{name} is not in any stage table");
            assert!(stage_short_label(name).is_some(), "{name} has no label");
        }
        for &post_name in &POST_VERIFICATION_STAGE_NAMES {
            let stage = post_verification_stage_for_batch(post_name, vec![], 1, 1.0);
            assert!(is_known_stage(stage.name()), "{} not known", stage.name());
            assert_eq!(stage_short_label(stage.name()), Some("Post-Verification"));
        }
        assert!(is_known_stage(prescreen_stage().name()));
        assert!(is_known_stage(planning_stage().name()));
    }

    #[test]
    fn test_the_planner_is_only_asked_about_optional_stages() {
        // The prompt lists exactly the optional stages, so a name it returns
        // that is not one of them is a hallucination rather than a choice.
        let optional: Vec<&str> = ANALYSIS_STAGES
            .iter()
            .filter(|d| d.optional)
            .map(|d| d.name)
            .collect();
        assert_eq!(optional, ["resources", "locking", "security", "hardware"]);

        let required: Vec<&str> = ANALYSIS_STAGES
            .iter()
            .filter(|d| !d.optional)
            .map(|d| d.name)
            .collect();
        assert_eq!(required, ["goal", "implementation", "execution-flow"]);
    }

    #[test]
    fn test_planning_stage_schema_restricts_to_optional_stages() {
        if let OutputFormat::Json {
            schema: Some(ref s),
            ..
        } = planning_stage().output_format
        {
            let items_enum = s["properties"]["relevant_stages"]["items"]["enum"]
                .as_array()
                .expect("enum array in schema");
            let names: Vec<&str> = items_enum
                .iter()
                .map(|v| v.as_str().expect("string enum value"))
                .collect();
            let optional: Vec<&str> = ANALYSIS_STAGES
                .iter()
                .filter(|d| d.optional)
                .map(|d| d.name)
                .collect();
            assert_eq!(names, optional);
        } else {
            panic!("expected planning stage to use json_with_schema");
        }
    }

    #[test]
    fn test_stage_lookup_normalizes_casing_and_separators() {
        assert_eq!(
            analysis_stage_by_name("Locking").map(|d| d.name),
            Some("locking")
        );
        assert_eq!(
            analysis_stage_by_name(" locking ").map(|d| d.name),
            Some("locking")
        );
        assert_eq!(
            analysis_stage_by_name("stage_locking").map(|d| d.name),
            Some("locking")
        );
        assert_eq!(
            analysis_stage_by_name("stage-locking").map(|d| d.name),
            Some("locking")
        );
        assert_eq!(
            analysis_stage_by_name("execution_flow").map(|d| d.name),
            Some("execution-flow")
        );
        assert_eq!(
            analysis_stage_by_name("EXECUTION_FLOW").map(|d| d.name),
            Some("execution-flow")
        );
        assert_eq!(
            consolidation_stage_by_name("Post_Verification").map(|d| d.name),
            Some("post-verification")
        );
        assert_eq!(
            consolidation_stage_by_name("post_verification_3").map(|d| d.name),
            Some("post-verification")
        );
        assert_eq!(
            consolidation_stage_by_name("stage-verification").map(|d| d.name),
            Some("verification")
        );
        assert!(is_known_stage("Locking"));
        assert!(is_known_stage("execution_flow"));
        assert!(is_known_stage("stage_report"));
        assert!(is_known_stage("post-verification-1"));
    }

    #[test]
    fn test_planning_stage_reduce_normalizes_and_canonicalizes() {
        let stage = planning_stage();
        let mut state = LinuxPatchReviewState::default();
        let output = PlanningOutput {
            relevant_stages: vec![
                "Locking".to_string(),
                "stage_resources".to_string(),
                "  security  ".to_string(),
                "nonexistent_stage".to_string(),
            ],
        };
        (stage.reducer)(&mut state, output);
        assert_eq!(
            state.planned_stages,
            vec![
                "goal",
                "implementation",
                "execution-flow",
                "locking",
                "resources",
                "security"
            ]
        );
    }

    #[test]
    fn test_analysis_stages_keep_the_guidance_the_schema_alone_does_not_carry() {
        // The vendored guides still tell the model to use TodoWrite, which no
        // longer exists, and the verification stages keep anti-charity and
        // symmetrical proof directives of their own.
        for required in [
            "Do not call or mention TodoWrite",
            "Do not be overly charitable to the existing code",
            "If you find no concerns and no dismissed concerns",
            "investigated, and disproved with concrete evidence",
            "\"preexisting\": true if this bug already existed",
            "\"reasoning\": A step-by-step explanation.",
            "the candidate concern that was investigated and disproved",
            "NO DISMISSAL WITHOUT VERIFIED PROOF",
            "MUST cite the concrete disproving code",
            "Citing a single caller",
            "unpaired API/lifecycle call",
        ] {
            assert!(
                STAGE_JSON_SCHEMA_EXAMPLE.contains(required),
                "analysis stage guidance lost: {required}"
            );
        }
        assert!(
            STAGE_IMPLEMENTATION_INSTRUCTION.contains("Do not stop after finding several bugs"),
            "implementation stage must include full-hunk sweep directive"
        );
        assert!(
            STAGE_VERIFICATION_INSTRUCTION.contains("speculative_dismissal")
                && STAGE_VERIFICATION_INSTRUCTION
                    .contains("Single-caller or happy-path-only proofs")
                && STAGE_VERIFICATION_INSTRUCTION
                    .contains("Asymmetry or cleared-state rationalizations"),
            "verification stage must classify speculative, single-caller, and asymmetry dismissals into hard_cases"
        );
        assert!(
            STAGE_POST_VERIFICATION_INSTRUCTION.contains("SYMMETRICAL PROOF BAR")
                && STAGE_POST_VERIFICATION_INSTRUCTION.contains("ALL-CALLERS VERIFICATION"),
            "post-verification stage must enforce symmetrical proof bar and all-callers verification"
        );
    }

    #[test]
    fn test_validate_concerns_output_enforces_symmetrical_proof() {
        let state = LinuxPatchReviewState::default();

        // Empty output is valid.
        assert!(validate_concerns_output(&StageConcernsOutput::default(), &state).is_ok());

        // Valid concern and valid dismissed_concern with concrete disproving snippet.
        let valid_output = StageConcernsOutput {
            concerns: vec![json!({
                "type": "Memory Leak",
                "description": "Leaked buffer on error path",
                "reasoning": "Buffer is not freed before return",
                "preexisting": false,
                "locations": [{
                    "file": "drivers/foo/bar.c",
                    "function_or_symbol": "bar_probe",
                    "line": 42,
                    "code_snippet": "return -ENOMEM;",
                    "why_this_location_matters": "Early return leaks buf"
                }]
            })],
            dismissed_concerns: vec![json!({
                "type": "Locking",
                "description": "Suspected unlocked access to state",
                "reasoning": "Caller bar_ioctl holds bar_mutex across the call",
                "locations": [{
                    "file": "drivers/foo/bar.c",
                    "function_or_symbol": "bar_ioctl",
                    "line": 108,
                    "code_snippet": "guard(mutex)(&bar->mutex);\nret = bar_update(bar);",
                    "why_this_location_matters": "Proves mutex is held by caller"
                }]
            })],
        };
        assert!(validate_concerns_output(&valid_output, &state).is_ok());

        // Dismissed concern without disproving code_snippet is rejected.
        let missing_snippet = StageConcernsOutput {
            concerns: vec![],
            dismissed_concerns: vec![json!({
                "type": "Locking",
                "description": "Suspected unlocked access",
                "reasoning": "Caller probably locks it",
                "locations": [{
                    "file": "drivers/foo/bar.c",
                    "function_or_symbol": "bar_ioctl",
                    "line": null,
                    "code_snippet": "   ",
                    "why_this_location_matters": "Unverified assumption"
                }]
            })],
        };
        let err = validate_concerns_output(&missing_snippet, &state)
            .expect_err("dismissed_concern without disproving snippet must fail");
        assert!(err.contains("dismissed_concerns[0]"));
        assert!(err.contains("move the candidate issue to 'concerns'"));
    }

    #[test]
    fn test_validate_verification_stage_output_prevents_silent_concern_drop() {
        let state_with_concerns = LinuxPatchReviewState {
            all_concerns: vec![
                json!({
                    "description": "Memory leak in foo()",
                    "locations": [{"file": "mm/foo.c", "function_or_symbol": "foo"}]
                }),
                json!({
                    "description": "Race condition in bar()",
                    "locations": [{"file": "mm/bar.c", "function_or_symbol": "bar"}]
                }),
                json!({
                    "description": "Refcount leak in foo_helper()",
                    "locations": [{"file": "mm/foo.c", "function_or_symbol": "foo_helper"}]
                }),
            ],
            all_dismissed_concerns: vec![json!({
                "description": "Suspected NULL deref in baz()",
                "locations": [{"file": "mm/baz.c", "function_or_symbol": "baz"}]
            })],
            ..Default::default()
        };

        let state_no_dismissed = LinuxPatchReviewState {
            all_concerns: state_with_concerns.all_concerns.clone(),
            all_dismissed_concerns: vec![],
            ..Default::default()
        };
        let unexpected_dismissed = VerificationOutput {
            findings: vec![],
            hard_cases: vec![],
            dismissed_concerns: vec![json!({"description": "Dropped concern 1"})],
        };
        let err = validate_verification_stage_output(&unexpected_dismissed, &state_no_dismissed)
            .expect_err("cannot emit dismissed_concerns when input dismissed_concerns is empty");
        assert!(err.contains("input dismissed_concerns is empty"));

        // Placing a raised concern into dismissed_concerns is rejected.
        let raised_in_dismissed = VerificationOutput {
            findings: vec![json!({
                "problem": "mm: memory leak in foo()",
                "severity": "High",
                "severity_explanation": "Leaks buf on error path",
                "preexisting": false,
                "locations": [{"file": "mm/foo.c", "function_or_symbol": "foo"}]
            })],
            hard_cases: vec![],
            dismissed_concerns: vec![json!({
                "type": "Locking",
                "description": "Race condition in bar()",
                "reasoning": "Claimed safe without post-verification",
                "locations": [{
                    "file": "mm/bar.c",
                    "function_or_symbol": "bar",
                    "code_snippet": "spin_lock(&lock);"
                }]
            })],
        };
        let err_raised =
            validate_verification_stage_output(&raised_in_dismissed, &state_with_concerns)
                .expect_err("cannot move raised concern into dismissed_concerns");
        assert!(err_raised.contains("matches a raised concern in all_concerns"));

        // Empty findings and hard_cases when all_concerns is non-empty is rejected.
        let both_empty_concerns = VerificationOutput {
            findings: vec![],
            hard_cases: vec![],
            dismissed_concerns: vec![json!({
                "type": "Null Deref",
                "description": "Suspected NULL deref in baz()",
                "reasoning": "Checked before call",
                "locations": [{
                    "file": "mm/baz.c",
                    "function_or_symbol": "baz",
                    "code_snippet": "if (!ptr) return -EINVAL;"
                }]
            })],
        };
        let err_empty_concerns =
            validate_verification_stage_output(&both_empty_concerns, &state_with_concerns)
                .expect_err("findings and hard_cases cannot both be empty when concerns exist");
        assert!(err_empty_concerns.contains("'findings' and 'hard_cases' cannot both be empty"));

        // Empty dismissed_concerns and hard_cases when all_dismissed_concerns is non-empty is rejected.
        let both_empty_dismissed = VerificationOutput {
            findings: vec![json!({
                "problem": "mm: memory leak in foo()",
                "severity": "High",
                "severity_explanation": "Leaks buf on error path",
                "preexisting": false,
                "locations": [{"file": "mm/foo.c", "function_or_symbol": "foo"}]
            })],
            hard_cases: vec![],
            dismissed_concerns: vec![],
        };
        let err_empty_dismissed = validate_verification_stage_output(
            &both_empty_dismissed,
            &state_with_concerns,
        )
        .expect_err(
            "dismissed_concerns and hard_cases cannot both be empty when dismissed_concerns exist",
        );
        assert!(
            err_empty_dismissed
                .contains("'dismissed_concerns' and 'hard_cases' cannot both be empty")
        );

        // Valid output accounting for concerns and dismissed_concerns.
        let valid_output = VerificationOutput {
            findings: vec![json!({
                "problem": "mm: memory leak in foo()",
                "severity": "High",
                "severity_explanation": "Leaks buf on error path",
                "preexisting": false,
                "locations": [{"file": "mm/foo.c", "function_or_symbol": "foo"}]
            })],
            hard_cases: vec![json!({
                "type": "Locking",
                "description": "Race condition in bar()",
                "estimated_severity": "High",
                "signal_reason": "mixed_signals",
                "concern_arguments": "Unprotected write in bar()",
                "dismissal_arguments": "Caller might hold lock",
                "verification_question": "Does caller hold lock across bar()?",
                "preexisting": false,
                "locations": [{"file": "mm/bar.c", "function_or_symbol": "bar"}]
            })],
            dismissed_concerns: vec![
                json!({
                    "type": "Null Deref",
                    "description": "Suspected NULL deref in baz()",
                    "reasoning": "Checked before call",
                    "locations": [{
                        "file": "mm/baz.c",
                        "function_or_symbol": "baz",
                        "code_snippet": "if (!ptr) return -EINVAL;"
                    }]
                }),
                json!({
                    "type": "Null Deref",
                    "description": "Suspected NULL deref in foo()",
                    "reasoning": "Checked at entry to foo()",
                    "locations": [{
                        "file": "mm/foo.c",
                        "function_or_symbol": "foo",
                        "code_snippet": "if (!arg) return -EINVAL;"
                    }]
                }),
            ],
        };
        assert!(validate_verification_stage_output(&valid_output, &state_with_concerns).is_ok());
    }

    #[test]
    fn test_validate_post_verification_output() {
        let state = LinuxPatchReviewState::default();

        // Both empty findings and empty dismissed_concerns is rejected.
        let both_empty = PostVerificationOutput {
            findings: vec![],
            dismissed_concerns: vec![],
        };
        let err_empty = validate_post_verification_output(&both_empty, &state)
            .expect_err("both empty arrays must be rejected");
        assert!(err_empty.contains("must not return both empty"));

        // Disproved candidate in dismissed_concerns with valid proof location is accepted.
        let disproved = PostVerificationOutput {
            findings: vec![],
            dismissed_concerns: vec![json!({
                "description": "Candidate leak in foo()",
                "reasoning": "Caller bar() frees the buffer on error.",
                "locations": [{
                    "file": "mm/foo.c",
                    "function_or_symbol": "bar",
                    "line": 88,
                    "code_snippet": "if (err) kfree(buf);"
                }]
            })],
        };
        assert!(validate_post_verification_output(&disproved, &state).is_ok());

        // Dismissed concern without code_snippet is rejected.
        let disproved_no_snippet = PostVerificationOutput {
            findings: vec![],
            dismissed_concerns: vec![json!({
                "description": "Candidate leak in foo()",
                "reasoning": "Caller bar() frees the buffer on error.",
                "locations": [{
                    "file": "mm/foo.c",
                    "function_or_symbol": "bar",
                    "line": 88
                }]
            })],
        };
        let err_no_snippet = validate_post_verification_output(&disproved_no_snippet, &state)
            .expect_err("dismissed concern without code_snippet must be rejected");
        assert!(
            err_no_snippet
                .contains("dismissed_concerns[0] must include a non-empty 'locations' array")
        );

        // Valid finding is accepted.
        let valid = PostVerificationOutput {
            findings: vec![json!({
                "problem": "mm: memory leak in foo()",
                "severity": "High",
                "severity_explanation": "Buffer allocated in foo() is not freed on error return.",
                "preexisting": false,
                "locations": [{"file": "mm/foo.c", "function_or_symbol": "foo", "line": 42}]
            })],
            dismissed_concerns: vec![],
        };
        assert!(validate_post_verification_output(&valid, &state).is_ok());

        // Finding with empty problem string is rejected.
        let empty_problem = PostVerificationOutput {
            findings: vec![json!({
                "problem": "   ",
                "severity": "High",
                "severity_explanation": "Explanation",
                "preexisting": false,
                "locations": []
            })],
            dismissed_concerns: vec![],
        };
        let err = validate_post_verification_output(&empty_problem, &state)
            .expect_err("empty problem must be rejected");
        assert!(err.contains("findings[0] must have non-empty 'problem'"));

        // Non-object finding is rejected.
        let non_obj = PostVerificationOutput {
            findings: vec![json!("not an object")],
            dismissed_concerns: vec![],
        };
        let err_non_obj = validate_post_verification_output(&non_obj, &state)
            .expect_err("non-object finding must be rejected");
        assert!(err_non_obj.contains("findings[0] must have non-empty 'problem'"));

        // Multi-item batch requires every candidate in the batch to be accounted for.
        let err_partial = validate_post_verification_batch_output(&valid, 2)
            .expect_err("partial batch output must be rejected");
        assert!(err_partial.contains("2 candidate hard case(s)"));
    }

    #[test]
    fn test_batch_hard_cases_by_severity_caps_at_10_and_batches_lowest_first() {
        assert!(batch_hard_cases_by_severity(&[]).is_empty());

        // <= 10 items: 1 item per stage.
        let five: Vec<Value> = (0..5)
            .map(|i| json!({"id": i, "estimated_severity": "Medium"}))
            .collect();
        let batches_five = batch_hard_cases_by_severity(&five);
        assert_eq!(batches_five.len(), 5);
        assert!(batches_five.iter().all(|b| b.len() == 1));

        // 13 items (10 < N <= 20) with mixed severities: capped at 10 stages.
        // Top 7 highest-severity items get dedicated 1-item stages;
        // bottom 6 lowest-severity items are batched into 3 stages of 2 items each.
        let items = vec![
            json!({"id": "low1", "estimated_severity": "Low"}),
            json!({"id": "crit1", "estimated_severity": "Critical"}),
            json!({"id": "med1", "estimated_severity": "Medium"}),
            json!({"id": "high1", "estimated_severity": "High"}),
            json!({"id": "low2", "estimated_severity": "Low"}),
            json!({"id": "crit2", "estimated_severity": "Critical"}),
            json!({"id": "high2", "estimated_severity": "High"}),
            json!({"id": "med2", "estimated_severity": "Medium"}),
            json!({"id": "low3", "estimated_severity": "Low"}),
            json!({"id": "high3", "estimated_severity": "High"}),
            json!({"id": "med3", "estimated_severity": "Medium"}),
            json!({"id": "low4", "estimated_severity": "Low"}),
            json!({"id": "high4", "estimated_severity": "High"}),
        ];
        let batches = batch_hard_cases_by_severity(&items);
        assert_eq!(batches.len(), MAX_POST_VERIFICATION_STAGES);
        // First 7 stages are 1-item stages (2 Critical + 4 High + 1 Medium).
        for b in &batches[..7] {
            assert_eq!(b.len(), 1);
        }
        assert_eq!(batches[0][0]["id"], "crit1");
        assert_eq!(batches[1][0]["id"], "crit2");
        assert_eq!(batches[2][0]["id"], "high1");
        assert_eq!(batches[5][0]["id"], "high4");
        assert_eq!(batches[6][0]["id"], "med1");
        // Last 3 stages batch the remaining 2 Medium and 4 Low items (2 per stage).
        for b in &batches[7..] {
            assert_eq!(b.len(), 2);
        }
        let total_items: usize = batches.iter().map(Vec::len).sum();
        assert_eq!(total_items, 13);

        // N > 20 (e.g. 23 items): solo_stages = 0, all 10 stages are multi-item batches
        // (first 7 stages have 2 items each, last 3 lowest-severity stages have 3 items each).
        let twenty_three: Vec<Value> = (0..23)
            .map(|i| {
                let sev = match i % 4 {
                    0 => "Critical",
                    1 => "High",
                    2 => "Medium",
                    _ => "Low",
                };
                json!({"id": i, "estimated_severity": sev})
            })
            .collect();
        let batches_23 = batch_hard_cases_by_severity(&twenty_three);
        assert_eq!(batches_23.len(), MAX_POST_VERIFICATION_STAGES);
        for b in &batches_23[..7] {
            assert_eq!(b.len(), 2);
        }
        for b in &batches_23[7..] {
            assert_eq!(b.len(), 3);
        }
        let total_23: usize = batches_23.iter().map(Vec::len).sum();
        assert_eq!(total_23, 23);
    }

    #[test]
    fn test_stage_outputs_do_not_match_inner_location_on_malformed_outer_json() {
        let malformed_with_inner_location = r#"{
  "concerns": [
    {
      "type": "Configuration Bug",
      "description": "Missing core dependency",
      "reasoning": "It omits "core" and "std" from deps.",
      "preexisting": false,
      "locations": [
        {
          "file": "scripts/generate_rust_analyzer.py",
          "function_or_symbol": "generate_crates",
          "line": 149,
          "code_snippet": "append_crate(\"quote\", ...)",
          "why_this_location_matters": "Missing core dependency"
        }
      ]
    }
  ],
  "dismissed_concerns": []
}"#;

        let err = crate::workflow::output::parse_json_from_text::<StageConcernsOutput>(
            malformed_with_inner_location,
        )
        .expect_err("malformed outer JSON must not match inner location as StageConcernsOutput");
        assert!(err.contains("line 6"));

        assert!(
            crate::workflow::output::parse_json_from_text::<VerificationOutput>(
                malformed_with_inner_location
            )
            .is_err(),
            "malformed outer JSON must not match inner location as VerificationOutput"
        );
        assert!(
            crate::workflow::output::parse_json_from_text::<PostVerificationOutput>(
                malformed_with_inner_location
            )
            .is_err(),
            "malformed outer JSON must not match inner location as PostVerificationOutput"
        );
        let omitted_dismissed = crate::workflow::output::parse_json_from_text::<
            PostVerificationOutput,
        >(r#"{"findings": [{"problem": "bug"}]}"#)
        .expect("PostVerificationOutput should default omitted dismissed_concerns");
        assert_eq!(omitted_dismissed.findings.len(), 1);
        assert!(omitted_dismissed.dismissed_concerns.is_empty());
    }

    #[test]
    fn test_build_workflow_graph_structure() {
        let workflow = build_linux_patch_review_workflow();
        assert_eq!(workflow.name, "linux_patch_review");
        assert_eq!(workflow.steps.len(), 6);
    }

    #[test]
    fn test_custom_prompt_renders_last_and_only_when_it_has_content() {
        // A non-empty prefetched context, so that closing the prompt is
        // distinguishable from merely rendering somewhere in it.
        let render = |custom_prompt| {
            linux_system_prompt(true).render_for_log(&LinuxPatchReviewState {
                custom_prompt,
                prefetched_context: "struct foo { int bar; };".to_string(),
                ..Default::default()
            })
        };
        let without = render(None);

        for empty in [Some(String::new()), Some("  \n\t ".to_string())] {
            assert_eq!(
                render(empty),
                without,
                "an empty custom prompt renders nothing"
            );
        }

        assert_eq!(
            render(Some("  Check the locking.  ".to_string())),
            format!(
                "{without}\n\n<custom_instructions>\nCheck the locking.\n</custom_instructions>"
            ),
            "the custom prompt closes the system prompt"
        );
    }

    #[test]
    fn test_verification_stage_preserves_preexisting_concerns() {
        let stage = verification_stage(20, 0.0);
        let mut state = LinuxPatchReviewState {
            findings: vec![json!({
                "problem": "existing finding",
                "preexisting": false,
            })],
            concerns: vec![json!({
                "type": "Pre-existing Race",
                "description": "Old race condition",
                "preexisting": true,
            })],
            deduplicated_dismissed_concerns: vec![json!({
                "description": "existing dismissed",
            })],
            ..Default::default()
        };

        let output = VerificationOutput {
            findings: vec![
                json!({
                    "problem": "new regression",
                    "severity": "High",
                    "preexisting": false,
                }),
                json!({
                    "problem": "another pre-existing",
                    "severity": "Medium",
                    "severity_explanation": "Old leak",
                    "preexisting": true,
                }),
            ],
            hard_cases: vec![json!({
                "description": "contested issue",
                "estimated_severity": "High",
            })],
            dismissed_concerns: vec![json!({
                "description": "new dismissed",
            })],
        };

        (stage.reducer)(&mut state, output);

        assert_eq!(state.concerns.len(), 2);
        assert_eq!(state.concerns[0]["description"], "Old race condition");
        assert_eq!(state.concerns[1]["description"], "another pre-existing");
        assert_eq!(state.concerns[1]["reasoning"], "Old leak");
        assert_eq!(state.findings.len(), 2);
        assert_eq!(state.findings[0]["problem"], "existing finding");
        assert_eq!(state.findings[1]["problem"], "new regression");
        assert_eq!(state.hard_cases.len(), 1);
        assert_eq!(state.hard_cases[0]["description"], "contested issue");
        assert_eq!(state.deduplicated_dismissed_concerns.len(), 2);
        assert_eq!(
            state.deduplicated_dismissed_concerns[0]["description"],
            "existing dismissed"
        );
        assert_eq!(
            state.deduplicated_dismissed_concerns[1]["description"],
            "new dismissed"
        );
    }

    #[test]
    fn test_post_verification_stage_preserves_existing_state() {
        let batch = vec![json!({
            "type": "Race Condition",
            "description": "Candidate race in foo()",
            "estimated_severity": "High",
        })];
        let stage = post_verification_stage("post-verification-1", batch, 10, 0.0);
        let mut state = LinuxPatchReviewState {
            findings: vec![json!({
                "problem": "earlier finding",
                "severity": "High",
                "preexisting": false,
            })],
            concerns: vec![json!({
                "type": "Pre-existing Race",
                "description": "Old race condition",
                "preexisting": true,
            })],
            deduplicated_dismissed_concerns: vec![json!({
                "description": "Earlier dismissed concern",
                "reasoning": "Already proved safe",
            })],
            ..Default::default()
        };

        let output = PostVerificationOutput {
            findings: vec![
                json!({
                    "problem": "new post-verified finding",
                    "severity": "High",
                    "preexisting": false,
                }),
                json!({
                    "problem": "post-verified pre-existing",
                    "severity": "Medium",
                    "severity_explanation": "Old leak",
                    "preexisting": true,
                }),
            ],
            dismissed_concerns: vec![json!({
                "description": "Disproved hard case",
                "reasoning": "Caller holds lock",
            })],
        };

        (stage.reducer)(&mut state, output);

        assert_eq!(state.findings.len(), 2);
        assert_eq!(state.findings[0]["problem"], "earlier finding");
        assert_eq!(state.findings[1]["problem"], "new post-verified finding");
        assert_eq!(state.concerns.len(), 2);
        assert_eq!(state.concerns[0]["description"], "Old race condition");
        assert_eq!(
            state.concerns[1]["description"],
            "post-verified pre-existing"
        );
        assert_eq!(state.concerns[1]["reasoning"], "Old leak");
        assert_eq!(state.deduplicated_dismissed_concerns.len(), 2);
        assert_eq!(
            state.deduplicated_dismissed_concerns[0]["description"],
            "Earlier dismissed concern"
        );
        assert_eq!(
            state.deduplicated_dismissed_concerns[1]["description"],
            "Disproved hard case"
        );
    }

    #[test]
    fn test_report_preexisting_keeps_preexisting_through_verification() {
        let ver_stage = verification_stage(20, 0.0);

        let mut state = LinuxPatchReviewState {
            report_preexisting: true,
            ..Default::default()
        };

        let ver_output = VerificationOutput {
            findings: vec![json!({
                "problem": "net: unlocked access in foo_tx()",
                "severity": "High",
                "severity_explanation": "foo_tx() mutates x without holding foo_lock",
                "preexisting": true,
                "locations": [{"file": "net/foo.c", "function_or_symbol": "foo_tx", "line": 42, "code_snippet": "x++;", "why_this_location_matters": "unlocked"}]
            })],
            hard_cases: vec![],
            dismissed_concerns: vec![],
        };

        (ver_stage.reducer)(&mut state, ver_output);
        // Only the verified pre-existing finding is appended to both findings (for report_stage) and concerns.
        assert_eq!(state.findings.len(), 1);
        assert_eq!(
            state.findings[0]["problem"],
            "net: unlocked access in foo_tx()"
        );
        assert_eq!(state.findings[0]["preexisting"], true);
        assert_eq!(state.concerns.len(), 1);
        assert_eq!(
            state.concerns[0]["description"],
            "net: unlocked access in foo_tx()"
        );
        assert_eq!(state.concerns[0]["severity"], "High");
    }

    #[test]
    fn test_collect_stage_prompts_and_provenance_enrichment() {
        // Use Gemini-style function-name tool call IDs ("read_prompt") across parallel
        // calls where one call succeeds and another fails, verifying that a failed call
        // does not drop a sibling successful call sharing the same tool_call_id.
        let outcome = crate::workflow::stage::StageOutcome {
            skipped: false,
            tokens_in: 10,
            tokens_out: 10,
            tokens_cached: 0,
            history: vec![
                crate::ai::AiMessage {
                    role: crate::ai::AiRole::Assistant,
                    content: None,
                    thought: None,
                    thought_signature: None,
                    tool_calls: Some(vec![
                        crate::ai::ToolCall {
                            id: "read_prompt".to_string(),
                            function_name: "read_prompt".to_string(),
                            arguments: json!({"name": "patterns/BPF-001.md"}),
                            thought_signature: None,
                        },
                        crate::ai::ToolCall {
                            id: "read_prompt".to_string(),
                            function_name: "read_prompt".to_string(),
                            arguments: json!({"name": "patterns/missing.md"}),
                            thought_signature: None,
                        },
                        crate::ai::ToolCall {
                            id: "read_prompt".to_string(),
                            function_name: "read_prompt".to_string(),
                            arguments: json!({"name": "../secret.md"}),
                            thought_signature: None,
                        },
                    ]),
                    tool_call_id: None,
                },
                crate::ai::AiMessage {
                    role: crate::ai::AiRole::Tool,
                    content: Some(r#"{"content": "BPF pattern guide"}"#.to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: Some("read_prompt".to_string()),
                },
                crate::ai::AiMessage {
                    role: crate::ai::AiRole::Tool,
                    content: Some(r#"{"error": "File not found"}"#.to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: Some("read_prompt".to_string()),
                },
                crate::ai::AiMessage {
                    role: crate::ai::AiRole::Tool,
                    content: Some(
                        r#"{"content": "should be rejected by sanitize_guide_path"}"#.to_string(),
                    ),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: Some("read_prompt".to_string()),
                },
            ],
        };

        let selected = vec!["networking.md".to_string()];
        let locking_def = analysis_stage_by_name("locking").unwrap();
        let prompts = collect_stage_prompts(&selected, locking_def.guides, &outcome);
        assert_eq!(
            prompts,
            vec![
                "networking.md".to_string(),
                "subsystem/locking.md".to_string(),
                "patterns/BPF-001.md".to_string(),
            ]
        );

        let mut state = LinuxPatchReviewState {
            selected_guides: selected.clone(),
            ..Default::default()
        };
        append_stage_items_with_prompts(
            &mut state.all_concerns,
            &[json!({
                "type": "Locking",
                "description": "Deadlock in foo_lock()",
                "reasoning": "Takes spin_lock inside rcu",
                "preexisting": false,
                "locations": [{"file": "net/core/dev.c", "function_or_symbol": "foo_lock", "line": 42}]
            })],
            "locking",
            "General",
            &prompts,
        );
        let exec_def = analysis_stage_by_name("execution-flow").unwrap();
        let exec_prompts = collect_stage_prompts(
            &selected,
            exec_def.guides,
            &crate::workflow::stage::StageOutcome::default(),
        );
        append_stage_dismissed_concerns_with_prompts(
            &mut state.all_dismissed_concerns,
            &[json!({
                "type": "Execution Flow",
                "description": "Deadlock in foo_lock()",
                "reasoning": "Claimed safe",
                "locations": [{"file": "net/core/dev.c", "function_or_symbol": "foo_lock", "line": 44, "code_snippet": "spin_lock(&l);"}]
            })],
            "execution-flow",
            &exec_prompts,
        );

        assert_eq!(state.all_concerns[0]["stage"], "locking");
        assert_eq!(state.all_concerns[0]["stages"], json!(["locking"]));
        assert_eq!(state.all_concerns[0]["prompts"], json!(prompts));

        // Extra prompt paths for verification exclude networking.md (already in selected_guides)
        // and include subsystem/locking.md, patterns/BPF-001.md, callstack.md, technical-patterns.md.
        let extra_paths = extra_prompt_paths_for_state(&state, analysis_stage_by_name);
        assert_eq!(
            extra_paths,
            vec![
                PathBuf::from("subsystem/locking.md"),
                PathBuf::from("patterns/BPF-001.md"),
                PathBuf::from("callstack.md"),
                PathBuf::from("technical-patterns.md"),
            ]
        );

        // Verification stage user prompt log includes @subsystem/locking.md, @patterns/BPF-001.md, etc.
        let ver_stage = verification_stage(20, 0.0);
        let rendered_ver_log = ver_stage.user_prompt.render_for_log(&state);
        assert!(rendered_ver_log.contains("@subsystem/locking.md"));
        assert!(rendered_ver_log.contains("@patterns/BPF-001.md"));
        assert!(rendered_ver_log.contains("@callstack.md"));
        assert!(rendered_ver_log.contains("@technical-patterns.md"));

        let mut ver_out = VerificationOutput {
            findings: vec![],
            hard_cases: vec![json!({
                "type": "Locking",
                "description": "Deadlock in foo_lock()",
                "estimated_severity": "High",
                "signal_reason": "mixed_signals",
                "concern_arguments": "Takes spin_lock inside rcu",
                "dismissal_arguments": "Claimed safe",
                "verification_question": "Is foo_lock() called under RCU?",
                "preexisting": false,
                "locations": [{"file": "net/core/dev.c", "function_or_symbol": "foo_lock", "line": 42}]
            })],
            dismissed_concerns: vec![],
        };
        let outcome_ver = ver_stage.outcome_reducer.as_ref().unwrap();
        outcome_ver(
            &mut state,
            ver_out.clone(),
            &crate::workflow::stage::StageOutcome::default(),
        );
        assert_eq!(
            state.hard_cases[0]["stages"],
            json!(["locking", "execution-flow"])
        );
        assert_eq!(state.hard_cases[0]["stage"], "locking");
        assert_eq!(
            state.hard_cases[0]["prompts"],
            json!([
                "networking.md",
                "subsystem/locking.md",
                "patterns/BPF-001.md",
                "callstack.md",
                "technical-patterns.md"
            ])
        );

        // Post-verification stage for this hard_case batch gets the exact same prompts
        // and preserves stage, stages, and prompts on the resulting finding.
        let batch = state.hard_cases.clone();
        let pv_extra =
            extra_prompt_paths_for_items(&state.selected_guides, &batch, analysis_stage_by_name);
        assert_eq!(pv_extra, extra_paths);

        let mut pv_out = PostVerificationOutput {
            findings: vec![json!({
                "problem": "net: deadlock in foo_lock() under RCU",
                "severity": "High",
                "severity_explanation": "Verified deadlock",
                "preexisting": false,
                "locations": [{"file": "net/core/dev.c", "function_or_symbol": "foo_lock", "line": 42}]
            })],
            dismissed_concerns: vec![],
        };
        enrich_post_verification_output(
            &state.selected_guides,
            &batch,
            &mut pv_out,
            &crate::workflow::stage::StageOutcome::default(),
            analysis_stage_by_name,
        );
        record_verified_findings(&mut state, pv_out.findings);
        assert_eq!(state.findings.len(), 1);
        assert_eq!(state.findings[0]["stage"], "locking");
        assert_eq!(
            state.findings[0]["stages"],
            json!(["locking", "execution-flow"])
        );
        assert_eq!(
            state.findings[0]["prompts"],
            json!([
                "networking.md",
                "subsystem/locking.md",
                "patterns/BPF-001.md",
                "callstack.md",
                "technical-patterns.md"
            ])
        );
        let _ = &mut ver_out;
    }

    #[test]
    fn test_lineage_ids_validators_and_uuid_minting() {
        let mut state = LinuxPatchReviewState {
            project: "linux".to_string(),
            ..Default::default()
        };
        append_stage_items_with_prompts(
            &mut state.all_concerns,
            &[
                json!({"description": "concern one", "reasoning": "r1", "locations": []}),
                json!({"description": "concern two", "reasoning": "r2", "locations": []}),
            ],
            "goal",
            "General",
            &["review-core.md".to_string()],
        );
        append_stage_dismissed_concerns_with_prompts(
            &mut state.all_dismissed_concerns,
            &[json!({"description": "dismissed one", "reasoning": "d1", "locations": []})],
            "locking",
            &["subsystem/locking.md".to_string()],
        );

        assert_eq!(state.all_concerns[0]["id"], "C1");
        assert_eq!(state.all_concerns[1]["id"], "C2");
        assert_eq!(state.all_dismissed_concerns[0]["id"], "D1");

        let proof_loc = json!([{
            "file": "net/core/dev.c",
            "function_or_symbol": "foo",
            "line": 10,
            "code_snippet": "if (!ptr) return;"
        }]);

        // Missing C2 in source_ids must be rejected.
        let incomplete_ver = VerificationOutput {
            findings: vec![json!({
                "source_ids": ["C1"],
                "problem": "net: bug one",
                "severity": "High",
                "severity_explanation": "explain",
                "preexisting": false,
                "locations": []
            })],
            hard_cases: vec![],
            dismissed_concerns: vec![json!({
                "source_ids": ["D1"],
                "type": "Locking",
                "description": "dismissed one",
                "reasoning": "d1",
                "locations": proof_loc
            })],
        };
        let err = validate_verification_stage_output(&incomplete_ver, &state).unwrap_err();
        assert!(err.contains("C2"));

        // Putting a C* ID in Category 1b dismissed_concerns must be rejected.
        let illegal_1b = VerificationOutput {
            findings: vec![json!({
                "source_ids": ["C1"],
                "problem": "net: bug one",
                "severity": "High",
                "severity_explanation": "explain",
                "preexisting": false,
                "locations": []
            })],
            hard_cases: vec![],
            dismissed_concerns: vec![json!({
                "source_ids": ["C2", "D1"],
                "type": "Locking",
                "description": "dismissed one",
                "reasoning": "d1",
                "locations": proof_loc
            })],
        };
        let err = validate_verification_stage_output(&illegal_1b, &state).unwrap_err();
        assert!(err.contains("C2"));

        // Putting D1 into Category 1a findings is rejected.
        let illegal_1a = VerificationOutput {
            findings: vec![json!({
                "source_ids": ["C1", "D1"],
                "problem": "net: bug one",
                "severity": "High",
                "severity_explanation": "explain",
                "preexisting": false,
                "locations": []
            })],
            hard_cases: vec![json!({
                "source_ids": ["C2"],
                "type": "Bug",
                "description": "concern two",
                "estimated_severity": "Medium",
                "signal_reason": "speculative_concern",
                "concern_arguments": "r2",
                "dismissal_arguments": "",
                "verification_question": "verify c2",
                "preexisting": false,
                "locations": []
            })],
            dismissed_concerns: vec![],
        };
        let err = validate_verification_stage_output(&illegal_1a, &state).unwrap_err();
        assert!(err.contains("D1") || err.contains("dismissed_concerns"));

        // Complete valid verification output succeeds and stamps VF1, H1, VD1, and UUID.
        let valid_ver = VerificationOutput {
            findings: vec![json!({
                "source_ids": ["C1"],
                "problem": "net: bug one",
                "severity": "High",
                "severity_explanation": "explain",
                "preexisting": false,
                "locations": []
            })],
            hard_cases: vec![json!({
                "source_ids": ["C2"],
                "type": "Bug",
                "description": "concern two",
                "estimated_severity": "Medium",
                "signal_reason": "speculative_concern",
                "concern_arguments": "r2",
                "dismissal_arguments": "",
                "verification_question": "verify c2",
                "preexisting": false,
                "locations": []
            })],
            dismissed_concerns: vec![json!({
                "source_ids": ["D1"],
                "type": "Locking",
                "description": "dismissed one",
                "reasoning": "d1",
                "locations": proof_loc
            })],
        };
        assert!(validate_verification_stage_output(&valid_ver, &state).is_ok());

        apply_verification_stage_output(
            &mut state,
            valid_ver,
            &crate::workflow::stage::StageOutcome::default(),
            analysis_stage_by_name,
        );
        assert_eq!(state.verification_findings[0]["id"], "VF1");
        assert_eq!(state.hard_cases[0]["id"], "H1");
        assert_eq!(state.hard_cases[0]["assigned_stage"], "post-verification-1");
        assert_eq!(state.verification_dismissed[0]["id"], "VD1");
        assert_eq!(state.findings[0]["id"], "VF1");

        // Post-verification validator checks H1 coverage.
        let batch = state.hard_cases.clone();
        let bad_pv = PostVerificationOutput {
            findings: vec![json!({
                "source_ids": ["H99"],
                "problem": "net: bug two",
                "severity": "Medium",
                "severity_explanation": "explain",
                "preexisting": false,
                "locations": []
            })],
            dismissed_concerns: vec![],
        };
        assert!(validate_post_verification_batch_items(&bad_pv, &batch).is_err());

        // Merging multiple hard cases (H1, H2) in a single batch into one finding is valid.
        let two_hard_cases = vec![
            json!({"id": "H1", "source_ids": ["C1"]}),
            json!({"id": "H2", "source_ids": ["C2"]}),
        ];
        let merged_pv = PostVerificationOutput {
            findings: vec![json!({
                "source_ids": ["H1", "H2"],
                "problem": "net: merged bug",
                "severity": "Medium",
                "severity_explanation": "explain",
                "preexisting": false,
                "locations": []
            })],
            dismissed_concerns: vec![],
        };
        assert!(validate_post_verification_batch_items(&merged_pv, &two_hard_cases).is_ok());

        let good_pv = PostVerificationOutput {
            findings: vec![json!({
                "source_ids": ["H1"],
                "problem": "net: bug two",
                "severity": "Medium",
                "severity_explanation": "explain",
                "preexisting": false,
                "locations": []
            })],
            dismissed_concerns: vec![],
        };
        assert!(validate_post_verification_batch_items(&good_pv, &batch).is_ok());

        apply_post_verification_stage_output(
            &mut state,
            "post-verification-1",
            &batch,
            good_pv,
            &crate::workflow::stage::StageOutcome::default(),
            analysis_stage_by_name,
        );
        assert_eq!(state.post_verification_findings[0]["id"], "PVF1");
        assert_eq!(
            state.post_verification_findings[0]["raw_source_ids"],
            json!(["C2"])
        );
        assert_eq!(state.findings.len(), 2);
        assert_eq!(state.findings[1]["id"], "PVF1");
        assert_eq!(state.findings[0]["stage_item_id"], "VF1");
        assert_eq!(state.findings[1]["stage_item_id"], "PVF1");
    }
}
