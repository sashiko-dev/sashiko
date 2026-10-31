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

//! Declarative patch review workflow for Zephyr.
//!
//! A sibling of `qemu_patch_review`: the same three-layer prompt architecture
//! and consolidation pipeline, with Zephyr's own stage set, instructions, and
//! severity calibration. A fix to the concern JSON contract or the
//! consolidation prompts usually applies to both files. The concern validators
//! and the plain-text report helpers are project-neutral and shared from
//! `qemu_patch_review`, as is the report format.

use serde_json::{Value, json};
use std::path::PathBuf;

use crate::workflow::{
    ExecutableStage, OutputFormat, ParallelPolicy, PromptTemplate, RecitationPolicy, Stage,
    StagePolicy, ToolScope, Workflow,
};
use crate::workflows::guard::{normalize_guide_name, normalize_stage_name};
use crate::workflows::linux_patch_review::{
    AnalysisStage, ConsolidationStage, LinuxPatchReviewState, POST_VERIFICATION_STAGE_NAMES,
    PlanningOutput, PostVerificationOutput, SERIES_CONTEXT_PLACEHOLDER, StageConcernsOutput,
    VerificationOutput, append_stage_dismissed_concerns_with_prompts,
    append_stage_items_with_prompts, apply_planning_stage_output,
    apply_post_verification_stage_output, apply_verification_stage_output,
    batch_inconclusive_concerns_by_severity, build_stage_efforts_schema, collect_stage_prompts,
    extra_prompt_paths_for_items, extra_prompt_paths_for_state, format_planning_feedback,
    format_post_verification_feedback, format_verification_stage_feedback, resolve_stage_effort,
    validate_planning_stage_efforts, validate_post_verification_batch_items,
    validate_verification_stage_output,
};

use crate::workflows::qemu_patch_review::{
    format_concerns_feedback, format_qemu_inline_findings, format_qemu_report_feedback,
    validate_concerns_output, validate_qemu_report_format,
};

/// State container for a Zephyr patch review run.
pub type ZephyrPatchReviewState = LinuxPatchReviewState;

// ---------------------------------------------------------------------------
// System Prompt Template
// ---------------------------------------------------------------------------

pub fn zephyr_system_prompt(use_log: bool) -> PromptTemplate<ZephyrPatchReviewState> {
    let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();
    let diff_var = if use_log {
        "{{target_commit_diff}}"
    } else {
        "{{target_commit_diff_only}}"
    };

    PromptTemplate::<ZephyrPatchReviewState>::new(format!(
        r#"Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references (e.g., 'today', 'last week', 'next year') strictly on this current date.

You are a senior Zephyr RTOS maintainer and embedded systems engineer. Your goal is to perform a deep, rigorous review of a proposed Zephyr change to ensure memory safety, correct use of execution contexts, concurrency and timing discipline, consistency with Kconfig and Devicetree configuration, and adherence to subsystem standards.

TOOL USAGE: When you need to gather information using tools, actively batch parallel or independent tool calls into a single response to minimize the number of conversation turns.

If tool output is truncated, page only if directly relevant to your active concerns.

<global_review_guidelines>
The following documents and any accompanying <maintainer_notes> blocks contain the official Zephyr architecture rules, component invariants, and cross-cutting driver and C discipline guidelines that you MUST adhere to during your review. Any accompanying <code_notes> blocks provide reference background notes generated from a fixed codebase snapshot to help you understand surrounding component contracts, callers, and known invariants. However, <code_notes> are NOT a source of truth for the patch under review—that snapshot may be stale or may already incorporate the target commit's own buggy code. The repository source code at Target Commit SHA (and Baseline SHA) is always authoritative: actively check modified functions, locks, device lifecycle transitions, and API calls against the invariants described in <maintainer_notes> and <code_notes>, but NEVER use a <code_notes> entry to justify, excuse, or dismiss a suspected defect in the patch under review.
@includes
</global_review_guidelines>{{{{maintainer_notes}}}}{{{{code_notes}}}}

=== Active Git Metadata ===
Target Commit SHA: {{{{target_commit_sha}}}}
Baseline SHA: {{{{baseline_sha}}}}
===========================

Target Commit:
{diff_var}
{{{{prefetched_block}}}}{{{{custom_prompt_block}}}}"#
    ))
    .with_var("target_commit_sha", |s: &ZephyrPatchReviewState| {
        s.target_commit_sha.clone()
    })
    .with_var("baseline_sha", |s: &ZephyrPatchReviewState| {
        s.baseline_sha.clone()
    })
    .with_var("code_notes", |s: &ZephyrPatchReviewState| s.code_notes.clone())
    .with_var("maintainer_notes", |s: &ZephyrPatchReviewState| {
        s.maintainer_notes.clone()
    })
    .with_var("target_commit_diff", |s: &ZephyrPatchReviewState| {
        s.target_commit_diff.clone()
    })
    .with_var("target_commit_diff_only", |s: &ZephyrPatchReviewState| {
        s.target_commit_diff_only.clone()
    })
    .with_var("prefetched_block", |s: &ZephyrPatchReviewState| {
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
    .with_var("custom_prompt_block", |s: &ZephyrPatchReviewState| {
        s.custom_prompt
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map_or_else(String::new, |p| {
                format!("\n\n<custom_instructions>\n{p}\n</custom_instructions>")
            })
    })
    .include_files_from_state(|s: &ZephyrPatchReviewState| {
        let mut paths = vec![PathBuf::from("review-core.md")];
        if !s.selected_guides.is_empty() {
            for guide in &s.selected_guides {
                let name = normalize_guide_name(guide);
                paths.push(PathBuf::from("subsystem").join(name));
                paths.push(PathBuf::from("patterns").join(name));
            }
        }
        paths
    })
}

// ---------------------------------------------------------------------------
// Stage Instructions
// ---------------------------------------------------------------------------

const STAGE_GOAL_INSTRUCTION: &str = r#"# Analyze commit main goal, architecture, and commit message quality

You are a senior Zephyr maintainer evaluating the high-level intent, architectural soundness, and commit message quality of a proposed Zephyr change. Enforce the priority hierarchy in the review guidelines: memory safety and trust boundaries > execution context correctness > concurrency and timing > configuration consistency > hardware contract > everything else.
- Problem Clarity: Is it clear what concrete problem the patch solves (a defect, a new board or driver, a subsystem feature)? Flag commits where the motivation is vague, circular, or unintelligible.
- Intent vs. Implementation: Does the code change match what the commit message claims? For each concern, cite the specific commit message claim and the code that contradicts or insufficiently implements it.
- Architectural Soundness: Is the chosen approach sound for this Zephyr subsystem? Does it fit Zephyr's patterns (the device model and `DEVICE_API()`, Devicetree for hardware description, Kconfig for software selection, workqueues for deferred work) and respect API contracts?
- Public API Impact: If a public header changes, check the API's status in its `@version` tag. Flag a breaking change to a stable API that does not follow the API lifecycle.
- Alternatives: Are there obviously simpler or more idiomatic approaches that achieve the same goal using existing Zephyr facilities?
- Single Responsibility: Flag commits that bundle unrelated changes or mix concerns that should be split into separate commits.
- Commit Message Audit (Mandatory): Verify the title follows `area: summary`, the body explains both WHAT changed and WHY it is needed rather than restating the diff, and a Signed-off-by trailer is present."#;

const STAGE_IMPLEMENTATION_INSTRUCTION: &str = r#"# Verify implementation against intent

Verify that the code changes faithfully and completely implement what the commit message and design claim.
- Check for incomplete rewiring at runtime boundaries: if a new Kconfig option, Devicetree property, driver API callback, or system call is added, verify every touch point (binding, init path, API struct, verifier, and all consumers) is updated in this patch or in a subsequent patch of the series (see the Series End Commit in context) before flagging it as unused or incomplete.
- If a function's contract changes, verify all callers and implementations were updated. A driver API change affects every driver implementing that API across the tree.
- Check edge cases: empty inputs, zero and boundary values, devices that are not ready, and fallback behavior.
- Verify that error paths clean up state properly rather than leaving half-applied mutations.
- Do not stop after finding several bugs in one function or hunk; systematically audit every modified function and hunk across the entire diff before concluding.
- Series Context Rule: If follow-up patches in this series are listed in the prompt context, check whether newly introduced types, helpers, or fields are wired up in subsequent patches of the series before flagging them as unused."#;

const STAGE_EXECUTION_FLOW_INSTRUCTION: &str = r#"# Trace execution flow and logic correctness

Trace the execution paths through every modified function and its callers and callees.
- Off-by-one errors and boundary conditions.
- Integer overflow/underflow in size, length, and address arithmetic; signedness bugs, truncation, and implicit type conversions, including tick and timeout conversions.
- Null pointer dereferences and uninitialized variables, including uninitialized stack structures passed to drivers.
- Return value checking: are error returns from bus transfers, kernel calls, and driver APIs checked on every path? Do error paths clean up?
- Code that relies on `__ASSERT()` for a condition that can occur at runtime, since assertions are compiled out without `CONFIG_ASSERT`.
- Edge cases: empty input, maximum values, and timeouts that expire.
- Do not stop after finding several bugs in one function or hunk; systematically trace every modified function and error path across the entire diff before concluding.
For every finding, provide the exact execution path or input scenario that triggers the bug. Do not report speculative logic issues without a concrete scenario."#;

const STAGE_RESOURCES_INSTRUCTION: &str = r#"# Audit resource management and device lifecycle

Audit the memory and object lifecycle of everything the change touches:
- Memory management: leaks, use-after-free, double-free, especially on error paths. Cover `k_malloc()`, `k_heap_alloc()`, `k_mem_slab_alloc()`, and `net_buf` reference counts.
- Object lifetime against deferred execution: an object must outlive every pending work item, timer, and callback that refers to it. Freeing or re-initializing a structure containing a pending `k_work` or a running `k_timer` is a defect.
- Fixed pools: blocking forever on an exhausted pool, and buffers that are never returned to it.
- Device lifecycle: init and deinit symmetry, clocks and interrupts released on failure, and `pm_device_runtime_get()` and `pm_device_runtime_put()` balance on every path.
- DMA buffers: ownership while a transfer is running, and cache maintenance around it.
- Stacks: large local buffers on threads, ISRs, or workqueue stacks with a small configured size.
- Do not stop after finding several bugs in one function or hunk; systematically audit every modified resource acquisition and release across the entire diff before concluding.
If it is not provable from the code that every acquired resource is released, raise a concern with the concrete leak path."#;

const STAGE_CONCURRENCY_INSTRUCTION: &str = r#"# Audit execution context, locking, and timing

First establish the context of every modified function: thread, ISR, `k_timer` expiry function, driver callback, workqueue handler, or pre-kernel init. Walk up to the `IRQ_CONNECT()`, callback registration, or `SYS_INIT()` that reaches it. Then audit the change:
- Blocking in the wrong context: sleeping APIs in an ISR or under a spinlock or `irq_lock()`. Check the API's `@isr_ok` tag and any note requiring `K_NO_WAIT`. Remember that `__ASSERT()` is the only guard on many of these paths.
- Protection that matches the contexts: a mutex cannot protect state shared with an ISR. Use a spinlock, `irq_lock()`, or one atomic operation on a single word. Check `k_spin_lock()` and `k_spin_unlock()` pairing on every return, and never leave a `K_SPINLOCK()` block with `return`, `break`, or `goto`.
- Lock ordering and recursion: spinlocks are not recursive, and distinct locks must nest in a consistent order. A work handler waiting on something only the same workqueue can provide deadlocks.
- SMP: a cooperative thread, `k_sched_lock()`, and local interrupt masking do not exclude another CPU under `CONFIG_SMP`. `irq_lock()` does, through a global lock. Check atomics and publish/consume ordering.
- Workqueues: a blocking handler stalls every later item on that queue, and the system workqueue is shared. Check that a pending work item is not altered or freed.
- Timing: unbounded work inside an ISR or critical section, and timeouts that can be `K_FOREVER` on a path that must make progress.
- Do not stop after finding several bugs in one function or hunk; systematically audit every modified shared-state access and lock scope across the entire diff before concluding.
Name the concrete interleaving that triggers a race; do not report concurrency issues as abstract possibilities."#;

const STAGE_CONFIGURATION_INSTRUCTION: &str = r#"# Audit Kconfig, Devicetree, and build configuration

One source file builds under many configurations. Audit whether the configuration the change depends on matches the code it gates.
- Code and Kconfig: a `CONFIG_*` symbol used by the code must be defined and reachable. Trace the disabled path: conditionally compiled fields, functions, and calls must agree, and the build must still link and behave correctly with the option off.
- Dependencies: check `depends on` and `select`. Selecting a symbol with its own dependencies can produce a configuration those dependencies forbid. A new driver option normally has `default y` and `depends on DT_HAS_<COMPAT>_ENABLED`. A plain `config FOO` added in a `Kconfig.defconfig` file weakens the base definition's `depends on`, unlike `configdefault FOO`.
- Build rules: a source file must be compiled under the same condition its users assume. Compare the `zephyr_library_sources_ifdef()` or similar line in `CMakeLists.txt` with the Kconfig option and with any header that declares the file's functions.
- Code and Devicetree: every property the driver reads must exist in the binding and be `required` or handled when absent. `DT_INST_FOREACH_STATUS_OKAY()` instantiation, `DT_DRV_COMPAT`, and `compatible` strings must agree.
- Optional nodes: `DEVICE_DT_GET()` on a node that may be disabled fails to link. Check that optional nodes are guarded.
- Shared files: a change to an SoC `.dtsi`, a shared Kconfig default, or a common binding affects every board using it. Check that boards relying on the old behavior still work.
- Hardware in the wrong place: fixed hardware facts belong in Devicetree, not Kconfig, and a `.conf` assignment to a hidden symbol is silently ignored.
Do not report missing symbols or schema errors that `scripts/ci/check_compliance.py` catches. Report logic errors that build cleanly: name the configuration that misbehaves and what happens in it."#;

const STAGE_SECURITY_INSTRUCTION: &str = r#"# Audit security boundaries and untrusted input

Zephyr devices process untrusted data, often without memory protection. Flag all potential security issues immediately.
- External input: every value received from a network, Bluetooth, USB, or other peripheral peer is attacker-controlled until validated. Check lengths, offsets, and counts before they are used as an index, a size, or a loop bound.
- Buffer overflows and over-reads from parser code, including fixed stack buffers and `memcpy` with a length taken from a packet.
- Integer overflows in size calculations, especially `uint16_t` arithmetic on packet fields.
- System calls, when `CONFIG_USERSPACE` is enabled: every `z_vrfy_` function must validate each user pointer, buffer size, and kernel object with the `K_SYSCALL_*` macros. Compare it with its `z_impl_` function; general parameter checks stay in the implementation. Data read from user memory must be copied before it is checked (TOCTOU), and a kernel-mode callback must never be registered from user mode.
- Kernel objects and stacks: objects must stay outside user-accessible memory, and threads that can run in user mode use `K_THREAD_STACK_*`.
- Secrets and cryptography: keys left in memory, non-constant-time comparison of authentication values, and fixed nonces. Use `sys_csrand_get()` where unpredictability matters.
- Flash, firmware update, and settings paths that can leave a device unable to boot.
- Do not stop after finding several bugs in one function or hunk; systematically audit every path that handles untrusted input across the entire diff before concluding."#;

const STAGE_VERIFICATION_INSTRUCTION: &str = r#"# Verification and severity estimation

You are the lead reviewer consolidating `concerns` and `dismissed_concerns` generated by parallel Zephyr review stages.
Your task is to (1) deduplicate overlapping items across both lists while preserving every distinct failure mechanism and location, and (2) classify every consolidated item into one of two categories:
- **Category 1: Well-Justified** — either a **1a: Well-Justified Concern** (`findings` array) or a **1b: Well-Justified Dismissal** (`dismissed_concerns` array).
- **Category 2: Inconclusive** (`inconclusive_concerns` array) — routed to parallel per-finding `post-verification` stages for deep tool-assisted code inspection.

### Step 1: Deduplication and Boundary Preservation
1. Group `concerns` and `dismissed_concerns` that refer to the same underlying root cause AND the same function/lifecycle phase.
2. Do NOT merge a setup or init bug with a teardown bug in a separate function; if an input concern combines setup and teardown bugs across separate functions, split them into separate items. Similarly, do NOT merge distinct races or bugs in separate functions/handlers, or bugs on different resources/fields (e.g., a leaked net_buf reference vs. a lost work item) even within the same function—either keep them as separate items or explicitly name ALL distinct resources, functions, and failure mechanisms in the item's title/description and explanation fields (`problem` and `severity_explanation` for `findings`, or `description` and `concern_arguments` for `inconclusive_concerns`).
3. SPECIFICITY REQUIREMENT: When merging overlapping items, preserve and consolidate the most specific details: exact function names, file paths, line numbers when known, and ALL distinct triggering conditions, callers, consequences, and racing tasks/handlers mentioned across the merged items. When a missing cleanup, unchecked error, or incomplete error-path unwind causes both an immediate failure and a downstream resource/state impact (e.g., a leaked buffer, a device left half-initialized, or corrupted driver state), explicitly state BOTH consequences in the item's explanation (`severity_explanation` for `findings`, or `concern_arguments` for `inconclusive_concerns`). Preserve and merge the `locations` arrays. Do not invent line numbers; use `null` when unknown.
4. Set `"preexisting": false` whenever the patch introduces the bug or newly exposes, triggers, or makes reachable a latent defect that could not be triggered before the patch (for example, adding a new caller that violates a helper's preconditions or adding an operation before an existing error path that does not clean it up). Set `"preexisting": true` when the problem already existed at `Baseline SHA` (before the commit/series) and was already triggerable before the patch—even if it lives in the same function, caller/callee, or error path modified by the patch without fixing it. Never dismiss a real bug into `dismissed_concerns` or drop it merely because it is pre-existing; validate it in `findings` or route it to `inconclusive_concerns` with `"preexisting": true`. (If a bug was introduced by an earlier commit in the same series after `Baseline SHA`, it is not pre-existing at `Baseline SHA`.)

### Step 2: Classification Rules (1: Well-Justified vs. 2: Inconclusive)
Classify every consolidated item using these strict signal rules. **Repetition is NOT justification: multiple overlapping items are never enough for Category 1 (1a or 1b) unless their reasoning is backed by specific, concrete code proof.**

1. **Category 1a — Well-Justified Concern (`findings` array):**
   - **Mandatory prerequisite & strong signals:** Concrete, self-contained code proof directly visible in the target diff, prefetched context, and cited `locations`, with **no** competing `dismissed_concern`, **no** reliance on unverified assumptions about unseen code (such as callers, callees, or the rest of an unmodified/partially-shown function truncated at the edge of a diff hunk), and **no** potential resolution in subsequent patches of this series (`=== Patch Series Context ===`). Do NOT place suspected build, compilation, or style/checkpatch noise into `findings`; route any such concern to `inconclusive_concerns` so `post-verification` can evaluate and discard it. Multiple deduplicated `concerns` with no attempts to dismiss is a strong signal for 1a **only when grounded in concrete code proof** of the complete function or handler chain.
   - **Action:** Validate it directly and emit it in `findings`. Assign a calibrated `severity` (`Critical`, `High`, `Medium`, or `Low`) strictly following `severity.md`, state all consequences (both immediate and downstream), triggering paths, and reachability at the start of `severity_explanation` (preserving every distinct function, handler, resource, and mechanism), formulate a concise bug title (`problem`) under 80 characters starting with a Zephyr area prefix (e.g. `kernel:`, `drivers: spi:`, `net: tcp:`, `Bluetooth: Host:`, `arch: arm:`, `dts:`), set `"preexisting"`, and include `locations`.

2. **Category 1b — Well-Justified Dismissal (`dismissed_concerns` array):**
   - **Mandatory prerequisite & strong signals:** Concrete disproving `code_snippet` in `locations` (showing the exact local guard, lock, bounds check, cleanup path, or lifecycle invariant that prevents the bug), with **no** competing `concern` and **no** reliance on unverified assumptions about external callers, callees, or peer behavior. Multiple overlapping `dismissed_concerns` for the same code is NOT a signal that the dismissal is safe—it indicates multiple analysts independently found the code suspicious; when multiple stages flag and dismiss the same non-trivial mechanism using assumptions about caller behavior, peer behavior, or external state rather than a direct local guard in the same function, classify into `inconclusive_concerns` with `"signal_reason": "speculative_dismissal"`.
   - **Disqualifiers for Category 1b (NEVER classify as 1b):**
     - **Single-caller or happy-path-only proofs:** A dismissal that proves an invariant (such as a validated index, a held lock, or a released reference) in only one caller or normal happy-path mode without verifying ALL callers and entry points in the tree (`git_grep` across ISRs, work handlers, driver API entry points, shell commands, and system call verifiers) is INVALID.
     - **Asymmetry or cleared-state rationalizations:** A dismissal that rationalizes an unpaired init/deinit, get/put, or ref/unref call, an unchecked error return, or clearing/overwriting state before downstream consumers read it as a "harmless no-op" or "intentional behavior" without concrete code proving that exact state or omission is safely handled is INVALID.
     - **Indirect state checks or "harmless side-effect" rationalizations:** A dismissal that rationalizes an unintended state mutation, duplicate event emission, or unverified fallback as "harmless" or "benign" without concrete code proof in the downstream consumer is INVALID.
   - **Action:** Place in `dismissed_concerns` (dropped from further verification).
   - **CRITICAL INVARIANT:** Never place any item that was raised as a `concern` into `dismissed_concerns` in this stage. If a raised `concern` is contested by a `dismissed_concern` or appears questionable, it MUST be placed in `inconclusive_concerns` for tool-based `post-verification`.

3. **Category 2 — Inconclusive: Speculative or Contested (`inconclusive_concerns` array — routed to parallel `post-verification`):**
   - **Strong signals:**
     - **Mixed signals (`"mixed_signals"`):** Similar or overlapping `concerns` and `dismissed_concerns` exist for the same root cause, function, or code path.
     - **Speculative or assumption-based dismissal (`"speculative_dismissal"`):** Even when NO stage raised a `concern` (and even when multiple stages emitted overlapping `dismissed_concerns`), inspect every standalone dismissal critically! Multiple overlapping dismissals are NOT enough if they are not justified by specific code. If one or more `dismissed_concerns` identified a plausible bug (such as a missing bounds check on an externally controlled value, a missing cleanup on an error path, unlocked shared state access, a blocking call in an ISR, or an integer overflow in size arithmetic) and dismissed it using an assumption not proven by the cited `code_snippet` or a vague argument (for example: assuming an unseen caller validates or cleans up on error, or citing only one caller), you MUST classify the item into `inconclusive_concerns` with `"signal_reason": "speculative_dismissal"`.
     - **Speculative or incomplete concern (`"speculative_concern"` or `"insufficient_evidence"`):** One or more overlapping `concerns` whose argument is vague, relies on assumptions not based on specific code in the diff/locations, claims that a guard/cleanup/call is missing from a function whose full body is not shown in the diff (`"signal_reason": "insufficient_evidence"`), mixes a partially inaccurate premise with a potentially real underlying bug in the same code path, or raises a suspected build/linter (`scripts/checkpatch.pl`, `scripts/ci/check_compliance.py`), documentation snippet (`doc/`, comments), or commit-message meta-concern.
     - **Series interaction (`"series_interaction"`):** Any concern that could plausibly be resolved, wired up, or refactored by a subsequent patch listed in `=== Patch Series Context ===`.
   - **Action:** Emit into `inconclusive_concerns` with `"estimated_severity"` (`Critical`, `High`, `Medium`, or `Low`), `"signal_reason"`, `"concern_arguments"`, `"dismissal_arguments"`, a concrete `"verification_question"` specifying what code `post-verification` must inspect with tools, `"preexisting"`, and `"locations"`."#;

const STAGE_POST_VERIFICATION_INSTRUCTION: &str = r#"# Per-finding post-verification and conflict resolution

You are the lead reviewer performing deep, tool-assisted codebase verification of a speculative or contested candidate issue (`inconclusive_concerns`) identified during initial verification.
For each candidate in `inconclusive_concerns`, use the available Git and file tools (`git_read_files`, `git_grep`, `git_diff`, `git_show`, `git_blame`) to answer each candidate's `verification_question` and inspect the actual code in the worktree.
1. Dismiss in `dismissed_concerns` any candidate that alleges a build, compilation, syntax, or linter error of the kind `scripts/ci/check_compliance.py` or a Twister build reports. A Kconfig or Devicetree logic error that builds cleanly but behaves wrongly is not such an error and must be verified like any other candidate (carry forward the candidate's target `file`, `function_or_symbol`, and `code_snippet` in `locations`).
2. **SYMMETRICAL PROOF BAR & ALL-CALLERS VERIFICATION:** For all other candidates, both `concern_arguments` and `dismissal_arguments` are untrusted hypotheses. Only dismiss a code-behavior candidate in `dismissed_concerns` if concrete, verifiable code in the repository proves the exact failure mechanism, branch, caller, and external interaction cannot occur across ALL callers, entry points, and modes. Citing a single caller does NOT disprove a buffer overflow, race, missing validation, or lifecycle leak in a helper function unless `git_grep` across all callers and entry points (ISRs, work handlers, driver API entry points, shell commands, and system call verifiers) proves every caller upholds the invariant. Do NOT give code the benefit of the doubt.
3. **LOCAL BOUNDARY & ASYMMETRY RULE:** Do not discard a defect within the modified code of the patch by assuming that surrounding caller systems or parallel execution will safely mask or prevent the issue, or by rationalizing an unpaired init/deinit, get/put, or ref/unref call, an unchecked error return, or overwritten state as a "harmless no-op" or "intentional behavior", unless you can point to specific code in the repository that concretely proves the failure mode is structurally impossible.
4. **PROMOTING SPECULATIVE DISMISSALS:** When `"signal_reason"` is `"speculative_dismissal"`, a previous analyst spotted the candidate bug described in `concern_arguments` and dismissed it using `dismissal_arguments`. Inspect the actual code with tools: if the dismissal's assumption is false or incomplete (for example: another caller does NOT uphold the invariant; the caller does NOT clean up the resource on an error path; the cited lock does NOT serialize the access against the competing thread, ISR, or CPU; or an unintended state mutation affects downstream consumers), you MUST report the bug as a verified finding in `findings`.
5. **REFINING PARTIALLY INACCURATE PREMISES:** If a candidate concern contains a partially inaccurate premise while also identifying a real bug in the same code path, refine and report the valid underlying bug rather than discarding the entire candidate.
6. **SERIES VALIDATION & PRE-EXISTING BUG RULE:** If other patches in this series are provided in the context, check whether each candidate is resolved, wired up, or refactored in the final state of the series (`Series End Commit`) using tools (`git_read_files` or `git_diff` with `revision` / `target_revision` set to the `Series End Commit`); do not trust promises in commit messages alone. If resolved by the end of the series, dismiss it in `dismissed_concerns` citing the resolving commit. Never dismiss a real bug into `dismissed_concerns` merely because it already existed before this change (`"preexisting": true` findings are routed to the pre-existing bug tracker and are not posted as patch regressions): if a candidate is a real bug that already existed at `Baseline SHA` and is not fixed at `Series End Commit`, report it in `findings` with `"preexisting": true`. Conversely, if a defect was introduced by an earlier commit in the same series (after `Baseline SHA`) and `Target Commit` does not touch, trigger, or worsen it, dismiss it for this commit rather than marking `"preexisting": true`. When referring to other patches within this series in your explanation, DO NOT use ephemeral git hashes; refer to them by their patch subject (e.g. 'commit "drivers: spi: fix transfer cleanup"').
7. **DESIGN & DOCUMENTATION RULE:** If a candidate targets illustrative pseudo-code or abbreviated snippets in documentation (`doc/`, `README*` files, code comments), inspect the actual implementation at the series head (`Series End Commit` / `HEAD`). If the actual code properly enforces the invariant, dismiss the documentation concern in `dismissed_concerns` citing the enforcing code.
8. **SEVERITY CALIBRATION AND COMPLETENESS:** Set `"preexisting": false` whenever the patch introduces the bug or newly exposes, triggers, or makes reachable a latent defect that could not be triggered before the patch; set `"preexisting": true` when the problem already existed at `Baseline SHA` (before the commit/series) and was already triggerable before the patch, even if it lives in the same function, caller/callee, or error path modified by the patch. For each verified issue, assign an accurate severity (`Critical`, `High`, `Medium`, or `Low`) strictly following `severity.md`: reason through all consequences (both immediate failures and downstream state/resource leaks), triggering paths, and reachability, state that reasoning at the start of `severity_explanation`, preserve all distinct function names, file paths, line numbers when known, triggering entry points, and consequences, and formulate a concise bug title (`problem`) under 80 characters starting with a Zephyr area prefix (e.g. `kernel:`, `drivers: spi:`, `net: tcp:`, `Bluetooth: Host:`, `arch: arm:`, `dts:`)."#;

const STAGE_REPORT_INSTRUCTION: &str = r#"# Generate plain-text inline review report

Generate the plain-text inline review report following the exact formatting rules and structure in `report-template.md`.
- Output ONLY a plain bulleted list of ALL findings ordered from highest severity to lowest (`- [CRITICAL] ...`, `- [HIGH] ...`, `- [MEDIUM] ...`, `- [LOW] ...`), or `No issues found.` if there are no findings.
- If any finding has `"preexisting": true`, state explicitly in its explanation that the issue was not introduced by this change.
- Do NOT include `Summary:` or `Findings:` headers.
- Do NOT use backticks, markdown code blocks, or markdown headings. Wrap all lines at 78 characters or fewer."#;

const CONCERN_JSON_SCHEMA_EXAMPLE: &str = r#"Once you have gathered sufficient information, return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
If you find no concerns and no dismissed concerns, return {"concerns": [], "dismissed_concerns": []}.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
- "type": A short category string (e.g. "Memory Safety", "ISR Context / Blocking Call", "Locking / Race", "Kconfig / Devicetree Mismatch", "Resource Leak", "Syscall Verification", "Error Handling").
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations".
Each location object in "locations" (for both 'concerns' and 'dismissed_concerns') MUST include "file", "function_or_symbol", "line", "code_snippet", "side", and "why_this_location_matters".
- "side": "PARENT" if commenting on deleted or pre-patch code (e.g., '-' lines in the diff or baseline code), or "REVISION" if commenting on newly added or modified code (e.g., '+' lines in the diff or post-patch code). Default to "REVISION" when commenting on unchanged context lines.
In each 'dismissed_concerns' object, "description" is the candidate concern that was investigated and disproved, "reasoning" is the step-by-step explanation of why it is not a bug (citing the exact guard, caller, or invariant), and "locations" MUST cite the concrete disproving code (file, function_or_symbol, line, verbatim code_snippet, side, and why_this_location_matters).
Use the 'dismissed_concerns' array ONLY for candidate concerns that you considered plausible, investigated, and disproved with concrete evidence. This is especially important when you first suspect a concern and then follow the evidence chain proving that it does NOT apply.

PRE-EXISTING BUGS: Do not actively hunt for unrelated bugs outside the reviewed commit and its call paths. However, if while analyzing the commit you spot a concrete bug that already existed in the codebase before this series (`Baseline SHA`) and is not fixed by the commit, you MUST still report it in 'concerns' with `"preexisting": true` (`"preexisting": true` items are routed to the pre-existing bug tracker and are not posted as patch regressions). Never silently drop a real pre-existing bug or place it in 'dismissed_concerns' merely because it was not introduced by this change.

NO DISMISSAL WITHOUT VERIFIED PROOF: To place a candidate issue in 'dismissed_concerns' (or to discard a suspected issue), you MUST find concrete proof in the code ('file', 'function_or_symbol', 'line', and verbatim 'code_snippet' in 'locations') that explicitly invalidates the concern's reasoning. If the disproving code lives outside the diff (for example, in a caller, callee, helper, or configuration), you MUST verify that code first using tools ('git_read_files' or 'git_grep') and quote the verified disproving snippet in 'locations'. If you cannot find definitive code proof that the candidate issue is impossible, you MUST report it in 'concerns' (NOT 'dismissed_concerns') and make the condition explicit: if X is possible, then problem Y can occur.
- Citing a single caller does NOT disprove a buffer overflow, race, missing validation, or lifecycle leak in a helper function. A caller-based dismissal is valid ONLY if every caller in the tree ('git_grep' across all callers and entry points, including ISRs, work handlers, driver API entry points, shell commands, and system call verifiers) is verified to uphold the invariant; otherwise report it in 'concerns'.
- Never dismiss an unpaired lifecycle/state transition (e.g., an init without matching cleanup on failure, a missing pm_device_runtime_put() or net_buf_unref() on an error path, or clearing/overwriting state before downstream consumers read it) by rationalizing that the side effect is a "harmless no-op" or "rare edge case".

SPECIFICITY REQUIREMENT: When reporting a concern or dismissed_concern, cite exact function name(s), file path(s), and line number(s) when known. Do not invent line numbers; use null when exact values are unknown.

CRITICAL REVIEW DIRECTIVE: Do NOT dismiss concerns just because you assume the surrounding kernel, driver framework, or callers handle it perfectly. Do not be overly charitable to the code. Assume that interrupts can arrive between any two statements outside a critical section, that external peers control every received byte, and that user-mode threads control every system call argument.

Example Output:
```json
{
  "concerns": [
    {
      "type": "Memory Safety",
      "description": "Option length from a received packet used without a bounds check",
      "reasoning": "1. opt_len is read from the received packet in foo_parse_option().\n2. opt_len is passed to memcpy on line 415 without a bounds check against the size of the stack buffer.",
      "preexisting": false,
      "locations": [
        {
          "file": "subsys/net/lib/foo/foo.c",
          "function_or_symbol": "foo_parse_option",
          "line": 415,
          "code_snippet": "memcpy(buf, opt, opt_len);",
          "side": "REVISION",
          "why_this_location_matters": "A remote peer controls opt_len, so the copy can overflow the stack buffer."
        }
      ]
    }
  ],
  "dismissed_concerns": [
    {
      "type": "Error Handling",
      "description": "Potential leak when the second bus transfer fails",
      "reasoning": "Verified that the goto fail path on line 220 releases the buffer from line 190 before returning.",
      "locations": [
        {
          "file": "drivers/sensor/foo/foo.c",
          "function_or_symbol": "foo_read",
          "line": 220,
          "code_snippet": "goto fail;",
          "side": "REVISION",
          "why_this_location_matters": "Proves the buffer is released on the error path."
        }
      ]
    }
  ]
}
```"#;

// ---------------------------------------------------------------------------
// Stage Table Definitions
// ---------------------------------------------------------------------------

pub static ANALYSIS_STAGES: &[AnalysisStage] = &[
    AnalysisStage {
        name: "goal",
        short: "Goal Analysis",
        instruction: STAGE_GOAL_INSTRUCTION,
        guides: &[],
        uses_commit_log: true,
        optional: false,
        wants_series_context: true,
    },
    AnalysisStage {
        name: "implementation",
        short: "Implementation",
        instruction: STAGE_IMPLEMENTATION_INSTRUCTION,
        guides: &[],
        uses_commit_log: true,
        optional: false,
        wants_series_context: true,
    },
    AnalysisStage {
        name: "execution-flow",
        short: "Execution Flow",
        instruction: STAGE_EXECUTION_FLOW_INSTRUCTION,
        guides: &[],
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
        name: "concurrency",
        short: "Concurrency",
        instruction: STAGE_CONCURRENCY_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "configuration",
        short: "Kconfig & DT",
        instruction: STAGE_CONFIGURATION_INSTRUCTION,
        guides: &[],
        uses_commit_log: false,
        optional: true,
        wants_series_context: true,
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
];

pub static VERIFICATION: ConsolidationStage = ConsolidationStage {
    name: "verification",
    short: "Severity Estimation",
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

pub static CONSOLIDATION_STAGES: &[&ConsolidationStage] =
    &[&VERIFICATION, &POST_VERIFICATION, &REPORT];

fn series_context_placeholder(wants: bool) -> &'static str {
    if wants {
        SERIES_CONTEXT_PLACEHOLDER
    } else {
        ""
    }
}

fn with_series_context(
    template: PromptTemplate<ZephyrPatchReviewState>,
    wants: bool,
) -> PromptTemplate<ZephyrPatchReviewState> {
    if !wants {
        return template;
    }
    template.with_var("follow_up_series_section", |s: &ZephyrPatchReviewState| {
        s.follow_up_series_context
            .as_ref()
            .map(|ctx| format!("\n\n{}", ctx))
            .unwrap_or_default()
    })
}

pub fn analysis_stage_by_name(name: &str) -> Option<&'static AnalysisStage> {
    let normalized = normalize_stage_name(name);
    ANALYSIS_STAGES.iter().find(|s| s.name == normalized)
}

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

pub fn stage_short_label(name: &str) -> Option<&'static str> {
    if let Some(def) = analysis_stage_by_name(name) {
        return Some(def.short);
    }
    consolidation_stage_by_name(name).map(|s| s.short)
}

// ---------------------------------------------------------------------------
// Stage Builders
// ---------------------------------------------------------------------------

pub fn validate_zephyr_planning_output_with_effort(
    out: &PlanningOutput,
    _state: &ZephyrPatchReviewState,
) -> Result<(), String> {
    validate_planning_stage_efforts(analysis_stage_by_name, out)
}

pub fn planning_stage_with_effort(
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Stage<ZephyrPatchReviewState, PlanningOutput> {
    let dynamic_effort = effort == Some(crate::ai::EffortMode::Dynamic);
    let optional_stages: Vec<&'static str> = ANALYSIS_STAGES
        .iter()
        .filter(|s| s.optional)
        .map(|s| s.name)
        .collect();
    let optional_list = optional_stages.join(", ");

    let (sys_prompt, output_format) = if dynamic_effort {
        let stage_efforts_schema = build_stage_efforts_schema(ANALYSIS_STAGES);
        (
            format!(
                "You are an AI assistant planning a Zephyr patch review.\nThe core stages (goal, implementation, execution-flow) always run.\n1. Select which optional specialized stages should also run based on the patch contents.\nAvailable optional stages: [{optional_list}]\nInclude an optional stage when the patch touches or interacts with that domain; omit stages that are clearly irrelevant to the modified code.\n\n2. Select the reasoning effort (\"medium\" or \"high\") in stage_efforts for each stage that will run (goal, implementation, execution-flow, plus each stage selected in relevant_stages):\n- Use \"high\" for stages whose domain is central to the patch or where complex control flow, subtle error/teardown paths, concurrency, or untrusted-input validation must be verified.\n- Use \"medium\" for stages performing a routine check on a straightforward aspect of the patch.\n\nRespond with ONLY a JSON object. Example:\n```json\n{{\"relevant_stages\": [\"resources\", \"security\"], \"stage_efforts\": {{\"goal\": \"medium\", \"implementation\": \"high\", \"execution-flow\": \"high\", \"resources\": \"high\", \"security\": \"medium\"}}}}\n```"
            ),
            OutputFormat::json_with_schema(json!({
                "type": "object",
                "properties": {
                    "relevant_stages": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "stage_efforts": stage_efforts_schema
                },
                "required": ["relevant_stages", "stage_efforts"]
            }))
            .with_validator(validate_zephyr_planning_output_with_effort)
            .with_feedback_formatter(format_planning_feedback),
        )
    } else {
        (
            format!(
                "You are an AI assistant planning a Zephyr patch review.\nThe core stages (goal, implementation, execution-flow) always run.\nSelect which optional specialized stages should also run based on the patch contents.\nAvailable optional stages: [{optional_list}]\n\nCRITICAL BIAS RULE: Err on the side of inclusion. Include any stage whose domain could plausibly be affected by the patch.\n\nRespond with ONLY a JSON object listing the relevant optional stages. Example:\n```json\n{{\"relevant_stages\": [\"resources\", \"security\"]}}\n```"
            ),
            OutputFormat::json_with_schema(json!({
                "type": "object",
                "properties": {
                    "relevant_stages": {
                        "type": "array",
                        "items": { "type": "string" }
                    }
                },
                "required": ["relevant_stages"]
            })),
        )
    };

    Stage::builder("planning")
        .system_prompt(PromptTemplate::<ZephyrPatchReviewState>::new(sys_prompt))
        .user_prompt(
            PromptTemplate::<ZephyrPatchReviewState>::new(
                "<patch>\n{{target_commit_diff}}\n</patch>",
            )
            .with_var("target_commit_diff", |s: &ZephyrPatchReviewState| {
                s.target_commit_diff.clone()
            }),
        )
        .output_format(output_format)
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns: 1,
            temperature,
            reasoning_effort: effort.map(crate::ai::EffortMode::lightweight_stage_effort),
            ..Default::default()
        })
        .skip_if(|s| s.manual_stages.is_some())
        .reduce(move |state, out: PlanningOutput| {
            apply_planning_stage_output(
                ANALYSIS_STAGES,
                analysis_stage_by_name,
                dynamic_effort,
                out,
                &mut state.planned_stages,
                &mut state.planned_efforts,
            );
        })
        .build()
}

fn analysis_stage(
    def: &'static AnalysisStage,
    max_turns: usize,
    temperature: f32,
    reasoning_effort: Option<crate::ai::ReasoningEffort>,
) -> Box<dyn ExecutableStage<ZephyrPatchReviewState>> {
    let series_context = series_context_placeholder(def.wants_series_context);
    let mut user_template = PromptTemplate::<ZephyrPatchReviewState>::new(format!(
        "{}{}\n\n{}",
        def.instruction, series_context, CONCERN_JSON_SCHEMA_EXAMPLE
    ));

    for guide in def.guides {
        user_template = user_template.include_file(*guide);
    }

    let user_template = with_series_context(user_template, def.wants_series_context);

    Box::new(
        Stage::builder(def.name)
            .required()
            .system_prompt(zephyr_system_prompt(def.uses_commit_log))
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
                reasoning_effort,
                ..Default::default()
            })
            .reduce(
                move |state: &mut ZephyrPatchReviewState, out: StageConcernsOutput| {
                    let prompts = collect_stage_prompts(&state.selected_guides, def.guides);
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
    state: &ZephyrPatchReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<ZephyrPatchReviewState>>> {
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
            Some(def) => {
                let effort = resolve_stage_effort(state, def.name);
                stages.push(analysis_stage(def, max_turns, temperature, effort));
            }
            None => tracing::warn!("Ignoring unknown Zephyr review stage {:?}", name),
        }
    }
    stages
}

pub fn verification_stage(
    max_turns: usize,
    temperature: f32,
) -> Stage<ZephyrPatchReviewState, VerificationOutput> {
    verification_stage_with_effort(max_turns, temperature, None)
}

pub fn verification_stage_with_effort(
    max_turns: usize,
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Stage<ZephyrPatchReviewState, VerificationOutput> {
    let series_context = series_context_placeholder(VERIFICATION.wants_series_context);
    let user_template = with_series_context(
        PromptTemplate::<ZephyrPatchReviewState>::new(format!(
            r#"{STAGE_VERIFICATION_INSTRUCTION}

<false_positive_guide>
@include("false-positive-guide.md")
</false_positive_guide>

<severity_guidelines>
@include("severity.md")
</severity_guidelines>@includes

CRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must have concrete evidence in the code that proves the concern is invalid. Never drop a raised concern into 'dismissed_concerns' in this stage: every consolidated concern must be placed either in 'findings' (if well-justified with concrete code proof and no competing dismissal) or in 'inconclusive_concerns' (if contested, speculative, or requiring tool verification). Also inspect every standalone dismissed_concern: if it dismissed a plausible bug using an unverified assumption, promote it into 'inconclusive_concerns' with '"signal_reason": "speculative_dismissal"'.{series_context}

Aggregated Concerns:
{{{{aggregated_concerns}}}}

Aggregated Dismissed Concerns:
{{{{aggregated_dismissed_concerns}}}}

Return ONLY a JSON object with 'findings', 'inconclusive_concerns', and 'dismissed_concerns' arrays.
- LINEAGE REQUIREMENT ('source_ids'): Every item in Aggregated Concerns has an "id" ("C1", "C2", ...) and every item in Aggregated Dismissed Concerns has an "id" ("D1", "D2", ...). Every object in 'findings', 'inconclusive_concerns', and 'dismissed_concerns' MUST include a non-empty "source_ids" array of strings listing the exact input "C*" and/or "D*" IDs consolidated into that output item. Every input "C*" and "D*" ID MUST appear in at least one output item's "source_ids". Category 1a 'findings' may ONLY reference "C*" IDs (any contested "C*" + "D*" or promoted "D*" must be routed to 'inconclusive_concerns'), and Category 1b 'dismissed_concerns' may ONLY reference "D*" IDs in "source_ids" (never "C*" IDs).
- Each object in 'findings' (Category 1a: Well-Justified Concerns) MUST use the keys: "source_ids" (non-empty array of "C*" IDs only), "problem" (a short naming string under 80 characters starting with a Zephyr area prefix like 'kernel:', 'drivers: spi:', 'net: tcp:', 'Bluetooth: Host:', 'arch: arm:', 'dts:', NEVER using backquotes), "severity" ("Low", "Medium", "High", "Critical", or "Unknown"), "severity_explanation" (detailed reasoning and proof), "preexisting" (boolean), and "locations" (array of objects with file, function_or_symbol, line, code_snippet, side, and why_this_location_matters; side MUST be "PARENT" for deleted/pre-patch code or "REVISION" for added/modified/context code), and may include "stages" (array of stage names) and "prompts" (array of prompt files).
- Each object in 'inconclusive_concerns' (Category 2: Inconclusive) MUST use the keys: "source_ids" (non-empty array of input IDs), "type", "description", "estimated_severity" ("Low", "Medium", "High", "Critical", or "Unknown"), "signal_reason" ("mixed_signals", "speculative_concern", "speculative_dismissal", "series_interaction", or "other"), "concern_arguments" (consolidated arguments for why the bug can occur), "dismissal_arguments" (consolidated arguments/snippets from any competing or standalone dismissal, or "" if none), "verification_question" (the specific code question post-verification must answer with tools), "preexisting" (boolean), and "locations" (array of objects with file, function_or_symbol, line, code_snippet, side, and why_this_location_matters; side MUST be "PARENT" or "REVISION"), and may include "stages" and "prompts".
- Each object in 'dismissed_concerns' (Category 1b: Well-Justified Dismissals) MUST use the keys: "source_ids" (non-empty array of "D*" IDs only), "type", "description", "reasoning", and "locations" (array of objects with file, function_or_symbol, line, code_snippet, side, and why_this_location_matters; side MUST be "PARENT" or "REVISION"), and may include "stages" and "prompts"."#
        ))
        .include_file("false-positive-guide.md")
        .include_file("severity.md")
        .include_files_from_state(|s: &ZephyrPatchReviewState| {
            extra_prompt_paths_for_state(s, analysis_stage_by_name)
        }),
        VERIFICATION.wants_series_context,
    )
    .with_var("aggregated_concerns", |s: &ZephyrPatchReviewState| {
        serde_json::to_string_pretty(&s.all_concerns).unwrap_or_default()
    })
    .with_var(
        "aggregated_dismissed_concerns",
        |s: &ZephyrPatchReviewState| {
            serde_json::to_string_pretty(&s.all_dismissed_concerns).unwrap_or_default()
        },
    );

    Stage::builder(VERIFICATION.name)
        .system_prompt(zephyr_system_prompt(true))
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
            reasoning_effort: effort.map(crate::ai::EffortMode::verification_stage_effort),
            ..Default::default()
        })
        .skip_if(|s| s.all_concerns.is_empty() && s.all_dismissed_concerns.is_empty())
        .reduce(|state, out: VerificationOutput| {
            apply_verification_stage_output(state, out, analysis_stage_by_name);
        })
        .build()
}

pub fn post_verification_stage(
    stage_name: &'static str,
    batch: Vec<Value>,
    max_turns: usize,
    temperature: f32,
) -> Stage<ZephyrPatchReviewState, PostVerificationOutput> {
    post_verification_stage_with_effort(stage_name, batch, max_turns, temperature, None)
}

pub fn post_verification_stage_with_effort(
    stage_name: &'static str,
    batch: Vec<Value>,
    max_turns: usize,
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Stage<ZephyrPatchReviewState, PostVerificationOutput> {
    let candidate_json = serde_json::to_string_pretty(&batch).unwrap_or_default();
    let batch_for_prompts = batch.clone();
    let batch_for_validate = batch.clone();
    let batch_for_reduce = batch;
    let series_context = series_context_placeholder(POST_VERIFICATION.wants_series_context);
    let user_template = with_series_context(
        PromptTemplate::<ZephyrPatchReviewState>::new(format!(
            r#"{STAGE_POST_VERIFICATION_INSTRUCTION}

<false_positive_guide>
@include("false-positive-guide.md")
</false_positive_guide>

<severity_guidelines>
@include("severity.md")
</severity_guidelines>@includes

CRITICAL REVIEW DIRECTIVE: To dismiss a code-behavior candidate issue as a false positive, you must find concrete evidence in the code that proves the issue is invalid and quote that disproving code in `dismissed_concerns[].locations` (for policy-excluded build/linter or out-of-scope meta-concerns under rule 1, carry forward the candidate's target location or commit message snippet in `locations`). If you cannot find concrete proof of safety, you must validate and report the finding in `findings`.{series_context}

Inconclusive Concern(s) to Verify:
{{{{candidate_inconclusive_concerns}}}}

Return ONLY a JSON object with 'findings' and 'dismissed_concerns' arrays. Every candidate in this batch MUST be accounted for in either 'findings' (if validated) or 'dismissed_concerns' (if disproved by concrete code or excluded by rule 1; never return both empty arrays).
- LINEAGE REQUIREMENT ('source_ids'): Every inconclusive concern in this batch has an "id" ("I1", "I2", ...). Every object in 'findings' and 'dismissed_concerns' MUST include a non-empty "source_ids" array of strings listing the exact candidate "I*" ID(s) from this batch that it resolves, and every candidate "I*" ID in this batch MUST be accounted for in at least one output item's "source_ids".
- Each object in 'findings' MUST use: "source_ids" (non-empty array of batch "I*" IDs), "problem" (a short naming string under 80 characters starting with a Zephyr area prefix like 'kernel:', 'drivers: spi:', 'net: tcp:', 'Bluetooth: Host:', 'arch: arm:', 'dts:', NEVER using backquotes), "severity" (Low, Medium, High, Critical, or Unknown), "severity_explanation" (detailed reasoning and proof), "preexisting" (boolean), "locations" (array of objects with file, function_or_symbol, line, code_snippet, side, and why_this_location_matters; side MUST be "PARENT" for deleted/pre-patch code or "REVISION" for added/modified/context code).
- Each object in 'dismissed_concerns' MUST use: "source_ids" (non-empty array of batch "I*" IDs), "description" (the candidate issue that was disproved), "reasoning" (step-by-step explanation of how the inspected code or policy rule disproves the candidate), and "locations" (a non-empty array of objects with file, function_or_symbol, line, code_snippet, side, and why_this_location_matters; side MUST be "PARENT" or "REVISION", quoting the verbatim disproving code or target snippet)."#
        ))
        .include_file("false-positive-guide.md")
        .include_file("severity.md")
        .include_files_from_state(move |s: &ZephyrPatchReviewState| {
            extra_prompt_paths_for_items(
                &s.selected_guides,
                &batch_for_prompts,
                analysis_stage_by_name,
            )
        }),
        POST_VERIFICATION.wants_series_context,
    )
    .with_var("candidate_inconclusive_concerns", move |_: &ZephyrPatchReviewState| {
        candidate_json.clone()
    });

    Stage::builder(stage_name)
        .required()
        .system_prompt(zephyr_system_prompt(true))
        .user_prompt(user_template)
        .output_format(
            OutputFormat::json()
                .with_validator(
                    move |out: &PostVerificationOutput, _: &ZephyrPatchReviewState| {
                        validate_post_verification_batch_items(out, &batch_for_validate)
                    },
                )
                .with_feedback_formatter(format_post_verification_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            reasoning_effort: effort.map(crate::ai::EffortMode::verification_stage_effort),
            ..Default::default()
        })
        .reduce(move |state, out: PostVerificationOutput| {
            apply_post_verification_stage_output(
                state,
                stage_name,
                &batch_for_reduce,
                out,
                analysis_stage_by_name,
            );
        })
        .build()
}

pub fn resolve_post_verification_stages_with_options(
    state: &ZephyrPatchReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<ZephyrPatchReviewState>>> {
    let effort = state.effort;
    let batches = batch_inconclusive_concerns_by_severity(&state.inconclusive_concerns);
    batches
        .into_iter()
        .enumerate()
        .map(|(idx, batch)| {
            let stage_name = POST_VERIFICATION_STAGE_NAMES[idx];
            Box::new(post_verification_stage_with_effort(
                stage_name,
                batch,
                max_turns,
                temperature,
                effort,
            )) as Box<dyn ExecutableStage<ZephyrPatchReviewState>>
        })
        .collect()
}

pub fn report_stage(max_turns: usize, temperature: f32) -> Stage<ZephyrPatchReviewState, String> {
    report_stage_with_effort(max_turns, temperature, None)
}

pub fn report_stage_with_effort(
    max_turns: usize,
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Stage<ZephyrPatchReviewState, String> {
    Stage::builder(REPORT.name)
        .system_prompt(zephyr_system_prompt(true))
        .user_prompt(
            PromptTemplate::<ZephyrPatchReviewState>::new(format!(
                r#"{STAGE_REPORT_INSTRUCTION}

<report_template>
@include("report-template.md")
</report_template>

Findings:
{{{{findings}}}}

Return strictly plain text output (no markdown, no backticks, wrapped at 78 characters), not JSON."#
            ))
            .include_file("report-template.md")
            .with_var("findings", |s: &ZephyrPatchReviewState| {
                serde_json::to_string_pretty(&s.findings).unwrap_or_default()
            }),
        )
        .output_format(OutputFormat::text_with_validator(
            validate_qemu_report_format,
            format_qemu_report_feedback,
        ))
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            reasoning_effort: effort.map(crate::ai::EffortMode::report_stage_effort),
            recitation_policy: RecitationPolicy::FallbackToFreeForm {
                reminder: "Do not quote large blocks of code verbatim. Summarize concisely."
                    .to_string(),
            },
            ..Default::default()
        })
        .skip_if(|s| s.skip_report || s.findings.is_empty())
        .reduce(|state, out: String| {
            state.review_inline = format_qemu_inline_findings(&out);
        })
        .build()
}

// ---------------------------------------------------------------------------
// Complete Zephyr Review Workflow Graph
// ---------------------------------------------------------------------------

pub fn build_zephyr_patch_review_workflow() -> Workflow<ZephyrPatchReviewState> {
    build_zephyr_patch_review_workflow_with_options(20, 0.0)
}

/// Builds the Zephyr patch review workflow.
///
/// Like [`crate::workflows::gcc_patch_review::build_gcc_patch_review_workflow_with_effort`],
/// the workflow early-exits when the analysis fan-out produced no concerns or
/// dismissed concerns, and again when verification validated no findings, so
/// a clean patch costs only the planning and core analysis stages.
pub fn build_zephyr_patch_review_workflow_with_options(
    max_turns: usize,
    temperature: f32,
) -> Workflow<ZephyrPatchReviewState> {
    build_zephyr_patch_review_workflow_with_effort(max_turns, temperature, None)
}

pub fn build_zephyr_patch_review_workflow_with_effort(
    max_turns: usize,
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Workflow<ZephyrPatchReviewState> {
    let stage_max_turns = max_turns.min(20);
    Workflow::builder("zephyr_patch_review")
        .dynamic_parallel(
            planning_stage_with_effort(temperature, effort),
            move |state| resolve_analysis_stages_with_options(state, stage_max_turns, temperature),
            ParallelPolicy::BestEffort,
        )
        .early_exit_if(
            |s| s.all_concerns.is_empty() && s.all_dismissed_concerns.is_empty(),
            "No concerns or dismissed concerns raised in initial analysis stages",
        )
        .dynamic_parallel(
            verification_stage_with_effort(stage_max_turns, temperature, effort),
            move |state| {
                resolve_post_verification_stages_with_options(state, stage_max_turns, temperature)
            },
            ParallelPolicy::BestEffort,
        )
        .early_exit_if(
            |s| s.findings.is_empty(),
            "No findings validated in verification stages",
        )
        .stage(report_stage_with_effort(
            stage_max_turns,
            temperature,
            effort,
        ))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selected_notes(files: &[&str], diff: &str) -> Vec<String> {
        let prompts_dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("projects/zephyr/prompts");
        let files: Vec<String> = files.iter().map(|f| f.to_string()).collect();
        let notes = crate::notes::maintainer_notes::load_maintainer_notes(&prompts_dir);
        let idents = crate::notes::maintainer_notes::diff_identifiers_all(diff);
        notes
            .iter()
            .filter(|n| n.applies(&files, &idents))
            .map(|n| n.name.clone())
            .collect()
    }

    #[test]
    fn test_zephyr_analysis_stages_table() {
        let names: Vec<&str> = ANALYSIS_STAGES.iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![
                "goal",
                "implementation",
                "execution-flow",
                "resources",
                "concurrency",
                "configuration",
                "security",
            ]
        );

        for def in ANALYSIS_STAGES {
            assert!(
                def.guides.is_empty(),
                "{}: maintainer notes are selected from the diff, not per stage",
                def.name
            );
        }

        for core in ["goal", "implementation", "execution-flow"] {
            assert!(!analysis_stage_by_name(core).unwrap().optional, "{core}");
        }
        for optional in ["resources", "concurrency", "configuration", "security"] {
            assert!(
                analysis_stage_by_name(optional).unwrap().optional,
                "{optional}"
            );
        }
    }

    #[test]
    fn test_zephyr_stage_lookup_and_labels() {
        assert_eq!(stage_short_label("Execution_Flow"), Some("Execution Flow"));
        assert_eq!(stage_short_label("configuration"), Some("Kconfig & DT"));
        assert_eq!(
            stage_short_label("post-verification-3"),
            Some("Post-Verification")
        );
        assert_eq!(stage_short_label("report"), Some("Report Generation"));
        assert_eq!(stage_short_label("locking"), None);
        assert_eq!(stage_short_label("summary"), None);
    }

    #[test]
    fn test_build_zephyr_patch_review_workflow() {
        let wf = build_zephyr_patch_review_workflow();
        assert_eq!(wf.name, "zephyr_patch_review");
        // analysis fan-out, no-findings guard, verification fan-out,
        // no-findings guard, report
        assert_eq!(wf.steps.len(), 5);
        assert_eq!(
            planning_stage_with_effort(1.0, None).policy.temperature,
            1.0
        );
    }

    #[test]
    fn test_zephyr_notes_for_a_driver_patch() {
        let selected = selected_notes(
            &["drivers/spi/spi_foo.c"],
            "@@ -10,2 +10,3 @@\n+\tk_mutex_lock(&data->lock, K_FOREVER);\n",
        );
        for note in [
            "general",
            "device-drivers",
            "execution-context",
            "synchronization",
        ] {
            assert!(
                selected.iter().any(|n| n == note),
                "{note} must load: {selected:?}"
            );
        }
        for note in ["userspace", "devicetree", "tests-and-samples"] {
            assert!(
                !selected.iter().any(|n| n == note),
                "{note} must not load: {selected:?}"
            );
        }
    }

    #[test]
    fn test_zephyr_notes_for_a_configuration_patch() {
        let selected = selected_notes(&["drivers/spi/Kconfig.foo"], "");
        assert!(selected.iter().any(|n| n == "kconfig"), "{selected:?}");

        let selected = selected_notes(&["Kconfig.zephyr"], "");
        assert!(selected.iter().any(|n| n == "kconfig"), "{selected:?}");

        let selected = selected_notes(&["samples/hello_world/prj.conf"], "");
        assert!(selected.iter().any(|n| n == "kconfig"), "{selected:?}");

        for file in ["prj.conf", "board_defconfig"] {
            let selected = selected_notes(&[file], "");
            assert!(
                selected.iter().any(|n| n == "kconfig"),
                "{file}: {selected:?}"
            );
        }
        for file in ["board.dts", "board.dtsi", "app.overlay"] {
            let selected = selected_notes(&[file], "");
            assert!(
                selected.iter().any(|n| n == "devicetree"),
                "{file}: {selected:?}"
            );
        }

        let selected = selected_notes(&["dts/arm/st/f4/stm32f4.dtsi"], "");
        assert!(selected.iter().any(|n| n == "devicetree"), "{selected:?}");

        let selected = selected_notes(&["tests/foo/boards/bar.dts"], "");
        assert!(selected.contains(&"devicetree".to_string()));

        let selected = selected_notes(&["samples/foo/app.dtsi"], "");
        assert!(selected.contains(&"devicetree".to_string()));

        let selected = selected_notes(&["samples/net/foo/boards/bar.overlay"], "");
        assert!(selected.iter().any(|n| n == "devicetree"), "{selected:?}");
    }

    #[test]
    fn test_zephyr_notes_for_a_system_call_patch() {
        let diff = "@@ -1,2 +1,3 @@\n+\tK_OOPS(K_SYSCALL_MEMORY_WRITE(buf, len));\n";
        let selected = selected_notes(&["subsys/foo/foo_handlers.c"], diff);
        assert!(selected.iter().any(|n| n == "userspace"), "{selected:?}");
    }

    #[test]
    fn test_zephyr_notes_for_a_timer_expiry_function() {
        // A k_timer expiry function runs in interrupt context wherever the
        // file lives, so the symbol must load the note outside drivers/.
        let diff = "@@ -1,2 +1,3 @@\n+\tk_timer_start(&timer, K_MSEC(1), K_NO_WAIT);\n";
        let selected = selected_notes(&["subsys/net/foo.c"], diff);
        assert!(
            selected.iter().any(|n| n == "execution-context"),
            "{selected:?}"
        );
    }

    #[test]
    fn test_zephyr_severity_guide_is_bundled_for_the_project() {
        let guide = crate::prompt_bundle::severity_guide(crate::project::ProjectId::Zephyr);
        assert!(guide.contains("# Severity Levels"));
        assert!(guide.contains("user-mode thread"));
    }

    #[test]
    fn test_zephyr_maintainer_notes_render_before_code_notes() {
        let rendered = zephyr_system_prompt(true).render_for_log(&ZephyrPatchReviewState {
            maintainer_notes: "\n\n<maintainer_notes>M</maintainer_notes>".to_string(),
            code_notes: "\n\n<code_notes>C</code_notes>".to_string(),
            ..Default::default()
        });
        let m = rendered.find("<maintainer_notes>M").unwrap();
        let c = rendered.find("<code_notes>C").unwrap();
        let guidelines = rendered.find("</global_review_guidelines>").unwrap();
        assert!(guidelines < m && m < c);
    }

    #[test]
    fn test_zephyr_stages_carry_the_review_directives() {
        for required in [
            "the candidate concern that was investigated and disproved",
            "NO DISMISSAL WITHOUT VERIFIED PROOF",
            "MUST cite the concrete disproving code",
        ] {
            assert!(
                CONCERN_JSON_SCHEMA_EXAMPLE.contains(required),
                "Zephyr analysis stage guidance lost: {required}"
            );
        }
        for (name, instruction) in [
            ("implementation", STAGE_IMPLEMENTATION_INSTRUCTION),
            ("execution-flow", STAGE_EXECUTION_FLOW_INSTRUCTION),
            ("resources", STAGE_RESOURCES_INSTRUCTION),
            ("concurrency", STAGE_CONCURRENCY_INSTRUCTION),
        ] {
            assert!(
                instruction.contains("Do not stop after finding several bugs"),
                "Zephyr {name} stage must include full-hunk sweep directive"
            );
        }
        assert!(
            STAGE_VERIFICATION_INSTRUCTION.contains("speculative_dismissal")
                && STAGE_VERIFICATION_INSTRUCTION
                    .contains("Single-caller or happy-path-only proofs"),
            "Zephyr verification stage must classify speculative dismissals"
        );
        assert!(
            STAGE_POST_VERIFICATION_INSTRUCTION.contains("SYMMETRICAL PROOF BAR")
                && STAGE_POST_VERIFICATION_INSTRUCTION.contains("ALL-CALLERS VERIFICATION"),
            "Zephyr post-verification stage must enforce the proof bar"
        );
    }

    #[test]
    fn test_devicetree_logic_errors_are_not_dismissed_as_build_noise() {
        // QEMU treats device-tree complaints as noise. For Zephyr Devicetree
        // is a review subject, so the consolidation stages must not discard it.
        assert!(!STAGE_VERIFICATION_INSTRUCTION.contains("device-tree"));
        assert!(!STAGE_POST_VERIFICATION_INSTRUCTION.contains("device-tree"));
        assert!(STAGE_POST_VERIFICATION_INSTRUCTION.contains("Kconfig or Devicetree logic error"));
    }

    #[test]
    fn test_series_context_enabled_for_zephyr_wiring_and_verification_stages() {
        for stage_name in ["goal", "implementation", "configuration"] {
            let def = analysis_stage_by_name(stage_name).expect(stage_name);
            assert!(
                def.wants_series_context,
                "{stage_name} should have wants_series_context enabled"
            );
        }
        assert!(VERIFICATION.wants_series_context);
        assert!(POST_VERIFICATION.wants_series_context);
    }

    #[test]
    fn test_zephyr_verification_rejects_malformed_output() {
        let state = ZephyrPatchReviewState::default();
        let stage = verification_stage(20, 0.0);
        assert!(stage.output_format.validate("{}", &state).is_err());
        assert!(
            stage
                .output_format
                .validate(
                    r#"{"findings": [{"problem": "kernel: missing bounds check", "severity": "High", "severity_explanation": "Unchecked length", "preexisting": false, "locations": []}], "inconclusive_concerns": [], "dismissed_concerns": []}"#,
                    &state
                )
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_workflow_runs_planning_and_core_stages_on_a_clean_patch() {
        struct CleanProvider {
            calls: std::sync::Mutex<usize>,
        }

        #[async_trait::async_trait]
        impl crate::ai::AiProvider for CleanProvider {
            async fn generate_content(
                &self,
                request: crate::ai::AiRequest,
            ) -> anyhow::Result<crate::ai::AiResponse> {
                *self.calls.lock().unwrap() += 1;
                let body = if request.response_format.is_some() {
                    r#"{"relevant_stages": []}"#
                } else {
                    r#"{"concerns": [], "dismissed_concerns": []}"#
                };
                Ok(crate::ai::AiResponse {
                    content: Some(body.to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    usage: None,
                    truncated: false,
                })
            }

            fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
                crate::ai::ProviderCapabilities {
                    model_name: "mock".to_string(),
                    context_window_size: 1000,
                }
            }
        }

        let temp_dir = tempfile::tempdir().unwrap();
        let provider = std::sync::Arc::new(CleanProvider {
            calls: std::sync::Mutex::new(0),
        });
        let tools =
            std::sync::Arc::new(crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf()));
        let env = crate::workflow::stage::WorkflowEnv {
            provider: provider.clone(),
            tools,
            base_dir: std::path::Path::new("projects/zephyr/prompts"),
            context_tag: None,
        };

        let wf = build_zephyr_patch_review_workflow_with_options(1, 0.0);
        let mut state = ZephyrPatchReviewState::default();
        crate::workflow::WorkflowEngine::execute(&wf, &env, &mut state, None)
            .await
            .unwrap();

        // planning + goal, implementation, execution-flow; the empty analysis
        // results exit before verification and report.
        assert_eq!(*provider.calls.lock().unwrap(), 4);
    }

    #[tokio::test]
    async fn partial_analysis_and_post_verification_failures_abort_review() {
        struct FaultProvider {
            fail_post: bool,
            successful_analysis: std::sync::atomic::AtomicUsize,
        }

        #[async_trait::async_trait]
        impl crate::ai::AiProvider for FaultProvider {
            async fn generate_content(
                &self,
                request: crate::ai::AiRequest,
            ) -> anyhow::Result<crate::ai::AiResponse> {
                let prompt: String = request
                    .messages
                    .iter()
                    .filter_map(|message| message.content.as_deref())
                    .collect();
                let body = if request.response_format.is_some() {
                    json!({"relevant_stages": []})
                } else if prompt.contains("# Per-finding post-verification") {
                    if prompt.contains("FAIL_POST_CANDIDATE") {
                        anyhow::bail!("injected post-verification failure");
                    }
                    json!({"findings": [], "dismissed_concerns": [{
                        "source_ids": ["I2"], "type": "Validation",
                        "description": "PASS_POST_CANDIDATE",
                        "reasoning": "The call site rejects the invalid input.",
                        "locations": [{"file": "drivers/foo.c", "function_or_symbol": "validate",
                            "code_snippet": "if (!valid) return -EINVAL;", "side": "REVISION",
                            "why_this_location_matters": "The guard rejects the invalid input."}]
                    }]})
                } else if prompt.contains("# Verification and severity estimation") {
                    let candidates: Vec<Value> = ["FAIL_POST_CANDIDATE", "PASS_POST_CANDIDATE"]
                        .iter().enumerate().map(|(index, description)| json!({
                            "source_ids": [format!("C{}", index + 1)],
                            "type": "Validation", "description": description,
                            "estimated_severity": "Medium", "signal_reason": "other",
                            "concern_arguments": "The input may not be checked.",
                            "dismissal_arguments": "", "verification_question": "Is it checked?",
                            "preexisting": false, "locations": []
                        })).collect();
                    json!({"findings": [], "inconclusive_concerns": candidates, "dismissed_concerns": []})
                } else if prompt.contains("# Analyze commit main goal") {
                    if !self.fail_post {
                        anyhow::bail!("injected analysis failure");
                    }
                    let concerns: Vec<Value> = ["FAIL_POST_CANDIDATE", "PASS_POST_CANDIDATE"]
                        .iter()
                        .map(|description| {
                            json!({
                                "type": "Validation", "description": description,
                                "reasoning": "The input may not be checked.",
                                "preexisting": false, "locations": []
                            })
                        })
                        .collect();
                    json!({"concerns": concerns, "dismissed_concerns": []})
                } else {
                    self.successful_analysis
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    json!({"concerns": [], "dismissed_concerns": []})
                };
                Ok(crate::ai::AiResponse {
                    content: Some(body.to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    usage: None,
                    truncated: false,
                })
            }

            fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
                crate::ai::ProviderCapabilities {
                    model_name: "injected-failure".to_string(),
                    context_window_size: 1000,
                }
            }
        }

        for fail_post in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let provider = std::sync::Arc::new(FaultProvider {
                fail_post,
                successful_analysis: std::sync::atomic::AtomicUsize::new(0),
            });
            let env = crate::workflow::stage::WorkflowEnv {
                provider: provider.clone(),
                tools: std::sync::Arc::new(crate::toolbox::ToolBox::new(dir.path().to_path_buf())),
                base_dir: std::path::Path::new("projects/zephyr/prompts"),
                context_tag: None,
            };
            let workflow = build_zephyr_patch_review_workflow_with_options(1, 0.0);
            let mut state = ZephyrPatchReviewState::default();
            let error = crate::workflow::WorkflowEngine::execute(&workflow, &env, &mut state, None)
                .await
                .expect_err("a partial stage failure must not produce a clean review");
            let failed_stage = if fail_post {
                "post-verification"
            } else {
                "goal"
            };
            assert!(format!("{error:#}").contains(failed_stage), "{error:#}");
            assert!(
                provider
                    .successful_analysis
                    .load(std::sync::atomic::Ordering::SeqCst)
                    > 0
            );
            if fail_post {
                assert_eq!(
                    state.post_verification_dismissed.len(),
                    1,
                    "the other post-verification batch must have succeeded"
                );
            }
        }
    }

    /// Plays a review with two concerns: verification confirms one and leaves
    /// the other inconclusive, and post-verification confirms that one too.
    struct TwoConcernProvider {
        stages: std::sync::Mutex<Vec<&'static str>>,
    }

    #[async_trait::async_trait]
    impl crate::ai::AiProvider for TwoConcernProvider {
        async fn generate_content(
            &self,
            request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            let prompt: String = request
                .messages
                .iter()
                .filter_map(|m| m.content.as_deref())
                .collect();
            let (stage, body) = if request.response_format.is_some() {
                ("planning", r#"{"relevant_stages": []}"#)
            } else if prompt.contains("# Generate plain-text inline review report") {
                (
                    "report",
                    "- [HIGH] In drivers/foo.c, the length is not checked.",
                )
            } else if prompt.contains("# Per-finding post-verification") {
                (
                    "post-verification",
                    r#"{"findings": [{"source_ids": ["I1"], "problem": "drivers: foo: lock held across sleep", "severity": "Medium", "severity_explanation": "Stalls an ISR.", "preexisting": false, "locations": []}], "dismissed_concerns": []}"#,
                )
            } else if prompt.contains("# Verification and severity estimation") {
                (
                    "verification",
                    r#"{"findings": [{"source_ids": ["C1"], "problem": "drivers: foo: missing bounds check", "severity": "High", "severity_explanation": "Peer sets the length.", "preexisting": false, "locations": []}], "inconclusive_concerns": [{"source_ids": ["C2"], "type": "Locking / Race", "description": "lock held across sleep", "estimated_severity": "Medium", "signal_reason": "The callee may sleep.", "concern_arguments": "A callee may sleep.", "dismissal_arguments": null, "verification_question": "Does the callee sleep?", "preexisting": false, "locations": []}], "dismissed_concerns": []}"#,
                )
            } else if prompt.contains("# Trace execution flow") {
                (
                    "execution-flow",
                    r#"{"concerns": [{"type": "Memory Safety", "description": "missing bounds check", "reasoning": "The length comes from the peer.", "preexisting": false, "locations": []}, {"type": "Locking / Race", "description": "lock held across sleep", "reasoning": "A callee may sleep.", "preexisting": false, "locations": []}], "dismissed_concerns": []}"#,
                )
            } else {
                ("analysis", r#"{"concerns": [], "dismissed_concerns": []}"#)
            };
            self.stages.lock().unwrap().push(stage);
            Ok(crate::ai::AiResponse {
                content: Some(body.to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    async fn run_two_concern_review(
        skip_report: bool,
    ) -> (ZephyrPatchReviewState, Vec<&'static str>) {
        let temp_dir = tempfile::tempdir().unwrap();
        let provider = std::sync::Arc::new(TwoConcernProvider {
            stages: std::sync::Mutex::new(Vec::new()),
        });
        let tools =
            std::sync::Arc::new(crate::toolbox::ToolBox::new(temp_dir.path().to_path_buf()));
        let env = crate::workflow::stage::WorkflowEnv {
            provider: provider.clone(),
            tools,
            base_dir: std::path::Path::new("projects/zephyr/prompts"),
            context_tag: None,
        };
        let wf = build_zephyr_patch_review_workflow_with_options(1, 0.0);
        let mut state = ZephyrPatchReviewState {
            skip_report,
            ..Default::default()
        };
        crate::workflow::WorkflowEngine::execute(&wf, &env, &mut state, None)
            .await
            .unwrap();
        let stages = provider.stages.lock().unwrap().clone();
        (state, stages)
    }

    #[tokio::test]
    async fn test_workflow_reports_concerns_confirmed_by_either_verification_stage() {
        let (state, stages) = run_two_concern_review(false).await;

        assert_eq!(
            stages
                .iter()
                .filter(|s| ["verification", "post-verification", "report"].contains(s))
                .copied()
                .collect::<Vec<_>>(),
            vec!["verification", "post-verification", "report"]
        );
        let problems: Vec<&str> = state
            .findings
            .iter()
            .filter_map(|f| f["problem"].as_str())
            .collect();
        assert_eq!(
            problems,
            vec![
                "drivers: foo: missing bounds check",
                "drivers: foo: lock held across sleep",
            ]
        );
        assert!(state.review_inline.contains("[HIGH]"));
    }

    #[tokio::test]
    async fn test_skip_report_keeps_findings_and_omits_the_report_stage() {
        let (state, stages) = run_two_concern_review(true).await;

        assert_eq!(state.findings.len(), 2);
        assert!(!stages.contains(&"report"));
        assert!(state.review_inline.is_empty());
    }
}
