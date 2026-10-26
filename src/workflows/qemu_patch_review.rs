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

//! Declarative patch review workflow for QEMU.
//!
//! Modeled on `sashiko_patch_review`: the same three-layer prompt
//! architecture (universal review procedures, deterministic maintainer
//! notes, and generated code notes) and the same consolidation pipeline
//! (verification deduplicates and classifies, contested candidates fan out
//! to parallel post-verification stages), with QEMU's own stage
//! instructions, severity calibration, and report formatting.

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
    batch_hard_cases_by_severity, build_stage_efforts_schema, collect_stage_prompts,
    extra_prompt_paths_for_items, extra_prompt_paths_for_state, format_planning_feedback,
    format_post_verification_feedback, format_verification_stage_feedback,
    has_valid_proof_location, resolve_stage_effort, validate_locations_side,
    validate_planning_stage_efforts, validate_post_verification_batch_items,
    validate_verification_stage_output,
};

/// State container for a QEMU patch review run.
pub type QemuPatchReviewState = LinuxPatchReviewState;

// ---------------------------------------------------------------------------
// System Prompt Template
// ---------------------------------------------------------------------------

pub fn qemu_system_prompt(use_log: bool) -> PromptTemplate<QemuPatchReviewState> {
    let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();
    let diff_var = if use_log {
        "{{target_commit_diff}}"
    } else {
        "{{target_commit_diff_only}}"
    };

    PromptTemplate::<QemuPatchReviewState>::new(format!(
        r#"Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references (e.g., 'today', 'last week', 'next year') strictly on this current date.

You are a senior QEMU maintainer and system emulator engineer. Your goal is to perform a deep, rigorous review of a proposed QEMU change to ensure guest-visible correctness, security of the guest/host boundary, resource and lifecycle correctness, concurrency discipline, and adherence to subsystem standards.

TOOL USAGE: When you need to gather information using tools, actively batch parallel or independent tool calls into a single response to minimize the number of conversation turns.

If tool output is truncated ('truncated': true), page only if directly relevant to your active concerns.

<global_review_guidelines>
The following documents and any accompanying <maintainer_notes> blocks contain the official QEMU architecture rules, component invariants, and cross-cutting device-model and C discipline guidelines that you MUST adhere to during your review. Any accompanying <code_notes> blocks provide reference background notes generated from a fixed codebase snapshot to help you understand surrounding component contracts, callers, and known invariants. However, <code_notes> are NOT a source of truth for the patch under review—that snapshot may be stale or may already incorporate the target commit's own buggy code. The repository source code at Target Commit SHA (and Baseline SHA) is always authoritative: actively check modified functions, locks, device lifecycle transitions, and API calls against the invariants described in <maintainer_notes> and <code_notes>, but NEVER use a <code_notes> entry to justify, excuse, or dismiss a suspected defect in the patch under review.
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
    .with_var("target_commit_sha", |s: &QemuPatchReviewState| {
        s.target_commit_sha.clone()
    })
    .with_var("baseline_sha", |s: &QemuPatchReviewState| {
        s.baseline_sha.clone()
    })
    .with_var("code_notes", |s: &QemuPatchReviewState| s.code_notes.clone())
    .with_var("maintainer_notes", |s: &QemuPatchReviewState| {
        s.maintainer_notes.clone()
    })
    .with_var("target_commit_diff", |s: &QemuPatchReviewState| {
        s.target_commit_diff.clone()
    })
    .with_var("target_commit_diff_only", |s: &QemuPatchReviewState| {
        s.target_commit_diff_only.clone()
    })
    .with_var("prefetched_block", |s: &QemuPatchReviewState| {
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
    .with_var("custom_prompt_block", |s: &QemuPatchReviewState| {
        s.custom_prompt
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map_or_else(String::new, |p| {
                format!("\n\n<custom_instructions>\n{p}\n</custom_instructions>")
            })
    })
    .include_files_from_state(|s: &QemuPatchReviewState| {
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

You are a senior QEMU maintainer evaluating the high-level intent, architectural soundness, and commit message quality of a proposed QEMU change. Enforce the priority hierarchy in the review guidelines: guest/host security boundary > guest-visible correctness > state and lifecycle integrity > concurrency discipline > everything else.
- Problem Clarity: Is it clear what concrete problem the patch solves (emulated hardware behavior, device contract, specification compliance)? Flag commits where the motivation is vague, circular, or unintelligible.
- Intent vs. Implementation: Does the code change match what the commit message claims? For each concern, cite the specific commit message claim and the code that contradicts or insufficiently implements it.
- Architectural Soundness: Is the chosen approach sound for this QEMU subsystem? Does it align with QEMU's design patterns (qdev device model, QOM type registration, qapi/qmp discipline, memory API, accelerator layering) and respect API contracts and interfaces?
- Alternatives: Are there obviously simpler or more idiomatic approaches that achieve the same goal using existing QEMU core facilities or helpers?
- Single Responsibility: Flag commits that bundle unrelated drive-by changes or mix concerns that should be split into separate commits.
- Commit Message Audit (Mandatory): Verify the message explains both WHAT changed and WHY it is needed (rationale/motivation), rather than merely restating the diff, and verify a Signed-off-by trailer is present."#;

const STAGE_IMPLEMENTATION_INSTRUCTION: &str = r#"# Verify implementation against intent

Verify that the code changes faithfully and completely implement what the commit message and design claim.
- Check for incomplete rewiring at runtime boundaries: if a new device property, MachineClass/CPU model field, QOM type, qapi command, or trace point is added, verify every touch point (realize path, reset path, type registration, and all consumers) is updated in this patch or in a subsequent patch of the series (see the Series End Commit in context) before flagging it as unused or incomplete.
- If a function's contract changes, verify all callers and callees were updated.
- Check edge cases: empty inputs, zero/boundary values, not-yet-realized device state, and fallback behavior.
- Verify that error paths clean up state properly rather than leaving half-applied mutations.
- Do not stop after finding several bugs in one function or hunk; systematically audit every modified function and hunk across the entire diff before concluding.
- Series Context Rule: If follow-up patches in this series are listed in the prompt context, check whether newly introduced types, helpers, or fields are wired up in subsequent patches of the series before flagging them as unused."#;

const STAGE_EXECUTION_FLOW_INSTRUCTION: &str = r#"# Trace execution flow and logic correctness

Trace the execution paths through every modified function and its callers and callees.
- Off-by-one errors and boundary conditions.
- Integer overflow/underflow in size, length, and address arithmetic; signedness bugs, truncation, and implicit type conversions.
- Null pointer dereferences and uninitialized variables.
- Return value checking: are error returns from called functions checked on every path? Do error paths clean up?
- Edge cases: empty input, maximum values, and concurrent-access windows.
- Do not stop after finding several bugs in one function or hunk; systematically trace every modified function and error path across the entire diff before concluding.
For every finding, provide the exact execution path or input scenario that triggers the bug. Do not report speculative logic issues without a concrete scenario."#;

const STAGE_RESOURCES_INSTRUCTION: &str = r#"# Audit resource management and QOM device lifecycle

Audit the memory and object lifecycle of everything the change touches:
- Memory management: leaks, use-after-free, double-free, especially on error and cleanup paths (goto fail cleanup discipline).
- Reference counting: object_ref/object_unref balance; every allocation and queue is freed on all paths it can be abandoned.
- QEMU Object Model lifecycle: realize/unrealize and instance_init/instance_finalize symmetry. MemoryRegions are delisted and destroyed, timers and bottom halves are deleted or cancelled, file descriptors and chardev/backends references are released on unrealize.
- Hot-unplug safety: the device cannot be torn out from under guest activity that keeps referencing it (e.g. queued DMA).
- Do not stop after finding several bugs in one function or hunk; systematically audit every modified resource acquisition and release across the entire diff before concluding.
If it is not provable from the code that every acquired resource is released, raise a concern with the concrete leak path."#;

const STAGE_LOCKING_INSTRUCTION: &str = r#"# Audit locking and thread-safety

QEMU device emulation runs across vCPU threads, the main loop, and iothreads. Audit all concurrency in the change:
- BQL (Big QEMU Lock) discipline: verify state shared across devices and threads is protected, and that handlers or bottom halves which run without the BQL do not touch BQL-protected state.
- Lock ordering and potential deadlocks between the BQL and per-device or per-AioContext locks.
- Thread-safety of shared state mutated from vCPU threads; atomics versus locked access; publish/consume ordering.
- Races in hotplug/unplug paths against guest-triggered DMA or timer activity.
- Do not stop after finding several bugs in one function or hunk; systematically audit every modified shared-state access and lock scope across the entire diff before concluding.
Name the concrete interleaving that triggers the race; do not report concurrency issues as abstract possibilities."#;

const STAGE_SECURITY_INSTRUCTION: &str = r#"# Audit security boundaries and device emulation security

QEMU is a security boundary between guest and host. Flag all potential security issues immediately.
- Guest input validation: every value that originates from the guest (MMIO reads/writes, PCI config space writes, DMA descriptors, guest-visible device state) MUST be validated before it is used as an index, a size, or an address.
- Buffer overflows from guest-controlled sizes or indices; out-of-bounds reads or writes.
- Integer overflows in guest-controlled address calculations.
- DMA: dma_memory_read/dma_memory_write and descriptor walks with guest-controlled addresses or lengths.
- MMIO handlers: proper range checking and alignment handling; PCI BAR and config space access validation.
- Side-channel considerations in crypto or security-relevant device code.
- Migration compatibility as a state-integrity invariant: vmstate field versioning and subsection guards for migrated device state."#;

const STAGE_VERIFICATION_INSTRUCTION: &str = r#"# Verification and severity estimation

You are the lead reviewer consolidating `concerns` and `dismissed_concerns` generated by parallel QEMU review stages.
Your task is to (1) deduplicate overlapping items across both lists while preserving every distinct failure mechanism and location, and (2) classify every consolidated item into one of two categories:
- **Category 1: Well-Justified** — either a **1a: Well-Justified Concern** (`findings` array) or a **1b: Well-Justified Dismissal** (`dismissed_concerns` array).
- **Category 2: Speculative or Contested** (`hard_cases` array) — routed to parallel per-finding `post-verification` stages for deep tool-assisted code inspection.

### Step 1: Deduplication and Boundary Preservation
1. Group `concerns` and `dismissed_concerns` that refer to the same underlying root cause AND the same function/lifecycle phase.
2. Do NOT merge a setup/realize bug with a teardown/unrealize bug in a separate function; if an input concern combines setup and teardown bugs across separate functions, split them into separate items. Similarly, do NOT merge distinct races or bugs in separate functions/handlers, or bugs on different resources/fields (e.g., a leaked MemoryRegion reference vs. a lost vmstate field) even within the same function—either keep them as separate items or explicitly name ALL distinct resources, functions, and failure mechanisms in the item's title/description and explanation fields (`problem` and `severity_explanation` for `findings`, or `description` and `concern_arguments` for `hard_cases`).
3. SPECIFICITY REQUIREMENT: When merging overlapping items, preserve and consolidate the most specific details: exact function names, file paths, line numbers when known, and ALL distinct triggering conditions, callers, consequences, and racing tasks/handlers mentioned across the merged items. When a missing cleanup, unchecked error, or incomplete error-path unwind causes both an immediate failure and a downstream resource/state impact (e.g., a leaked device reference, a device left half-realized, or a guest-visible state corruption), explicitly state BOTH consequences in the item's explanation (`severity_explanation` for `findings`, or `concern_arguments` for `hard_cases`). Preserve and merge the `locations` arrays. Do not invent line numbers; use `null` when unknown.
4. Set `"preexisting": false` whenever the patch introduces the bug or newly exposes, triggers, or makes reachable a latent defect that could not be triggered before the patch (for example, adding a new caller that violates a helper's preconditions or adding an operation before an existing error path that does not clean it up). Set `"preexisting": true` when the problem already existed at `Baseline SHA` (before the commit/series) and was already triggerable before the patch—even if it lives in the same function, caller/callee, or error path modified by the patch without fixing it. Never dismiss a real bug into `dismissed_concerns` or drop it merely because it is pre-existing; validate it in `findings` or route it to `hard_cases` with `"preexisting": true`. (If a bug was introduced by an earlier commit in the same series after `Baseline SHA`, it is not pre-existing at `Baseline SHA`.)

### Step 2: Classification Rules (1: Well-Justified vs. 2: Speculative or Contested)
Classify every consolidated item using these strict signal rules. **Repetition is NOT justification: multiple overlapping items are never enough for Category 1 (1a or 1b) unless their reasoning is backed by specific, concrete code proof.**

1. **Category 1a — Well-Justified Concern (`findings` array):**
   - **Mandatory prerequisite & strong signals:** Concrete, self-contained code proof directly visible in the target diff, prefetched context, and cited `locations`, with **no** competing `dismissed_concern`, **no** reliance on unverified assumptions about unseen code (such as callers, callees, or the rest of an unmodified/partially-shown function truncated at the edge of a diff hunk), and **no** potential resolution in subsequent patches of this series (`=== Patch Series Context ===`). Do NOT place suspected build, compilation, device-tree, or style/checkpatch noise into `findings`; route any such concern to `hard_cases` so `post-verification` can evaluate and discard it. Multiple deduplicated `concerns` with no attempts to dismiss is a strong signal for 1a **only when grounded in concrete code proof** of the complete function or handler chain.
   - **Action:** Validate it directly and emit it in `findings`. Assign a calibrated `severity` (`Critical`, `High`, `Medium`, or `Low`) strictly following `severity.md`, state all consequences (both immediate and downstream), triggering paths, and reachability at the start of `severity_explanation` (preserving every distinct function, handler, resource, and mechanism), formulate a concise bug title (`problem`) under 80 characters starting with a QEMU component prefix (e.g. `virtio:`, `hw/arm:`, `target/i386:`, `migration:`, `qapi:`, `block:`), set `"preexisting"`, and include `locations`.

2. **Category 1b — Well-Justified Dismissal (`dismissed_concerns` array):**
   - **Mandatory prerequisite & strong signals:** Concrete disproving `code_snippet` in `locations` (showing the exact local guard, lock, bounds check, cleanup path, or lifecycle invariant that prevents the bug), with **no** competing `concern` and **no** reliance on unverified assumptions about external callers, callees, or guest behavior. Multiple overlapping `dismissed_concerns` for the same code is NOT a signal that the dismissal is safe—it indicates multiple analysts independently found the code suspicious; when multiple stages flag and dismiss the same non-trivial mechanism using assumptions about caller behavior, guest behavior, or external state rather than a direct local guard in the same function, classify into `hard_cases` with `"signal_reason": "speculative_dismissal"`.
   - **Disqualifiers for Category 1b (NEVER classify as 1b):**
     - **Single-caller or happy-path-only proofs:** A dismissal that proves an invariant (such as a validated guest index, a held lock, or a released reference) in only one caller or normal happy-path mode without verifying ALL callers and entry points in the tree (`git_grep` across QOM realize/unrealize, MMIO handlers, QMP/HMP commands, and migration callbacks) is INVALID.
     - **Asymmetry or cleared-state rationalizations:** A dismissal that rationalizes an unpaired realize/unrealize or ref/unref call, an unchecked error return, or clearing/overwriting state before downstream consumers read it as a "harmless no-op" or "intentional behavior" without concrete code proving that exact state or omission is safely handled is INVALID.
     - **Indirect state checks or "harmless side-effect" rationalizations:** A dismissal that rationalizes an unintended state mutation, duplicate event emission, or unverified fallback as "harmless" or "benign" without concrete code proof in the downstream consumer is INVALID.
   - **Action:** Place in `dismissed_concerns` (dropped from further verification).
   - **CRITICAL INVARIANT:** Never place any item that was raised as a `concern` into `dismissed_concerns` in this stage. If a raised `concern` is contested by a `dismissed_concern` or appears questionable, it MUST be placed in `hard_cases` for tool-based `post-verification`.

3. **Category 2 — Speculative or Contested (`hard_cases` array — routed to parallel `post-verification`):**
   - **Strong signals:**
     - **Mixed signals (`"mixed_signals"`):** Similar or overlapping `concerns` and `dismissed_concerns` exist for the same root cause, function, or code path.
     - **Speculative or assumption-based dismissal (`"speculative_dismissal"`):** Even when NO stage raised a `concern` (and even when multiple stages emitted overlapping `dismissed_concerns`), inspect every standalone dismissal critically! Multiple overlapping dismissals are NOT enough if they are not justified by specific code. If one or more `dismissed_concerns` identified a plausible bug (such as a missing bounds check on a guest-controlled value, a missing cleanup on error or hot-unplug, unlocked shared state access, or an integer overflow in address arithmetic) and dismissed it using an assumption not proven by the cited `code_snippet` or a vague argument (for example: assuming an unseen caller validates or cleans up on error, or citing only one caller), you MUST classify the item into `hard_cases` with `"signal_reason": "speculative_dismissal"`.
     - **Speculative or incomplete concern (`"speculative_concern"` or `"insufficient_evidence"`):** One or more overlapping `concerns` whose argument is vague, relies on assumptions not based on specific code in the diff/locations, claims that a guard/cleanup/call is missing from a function whose full body is not shown in the diff (`"signal_reason": "insufficient_evidence"`), mixes a partially inaccurate premise with a potentially real underlying bug in the same code path, or raises a suspected build/linter (`scripts/checkpatch.pl`), documentation snippet (`docs/`, comments), or commit-message meta-concern.
     - **Series interaction (`"series_interaction"`):** Any concern that could plausibly be resolved, wired up, or refactored by a subsequent patch listed in `=== Patch Series Context ===`.
   - **Action:** Emit into `hard_cases` with `"estimated_severity"` (`Critical`, `High`, `Medium`, or `Low`), `"signal_reason"`, `"concern_arguments"`, `"dismissal_arguments"`, a concrete `"verification_question"` specifying what code `post-verification` must inspect with tools, `"preexisting"`, and `"locations"`."#;

const STAGE_POST_VERIFICATION_INSTRUCTION: &str = r#"# Per-finding post-verification and conflict resolution

You are the lead reviewer performing deep, tool-assisted codebase verification of a speculative or contested candidate issue (`hard_cases`) identified during initial verification.
For each candidate in `hard_cases`, use the available Git and file tools (`git_read_files`, `git_grep`, `git_diff`, `git_show`, `git_blame`) to answer each candidate's `verification_question` and inspect the actual code in the worktree.
1. Dismiss in `dismissed_concerns` any candidate that alleges a build, compilation, syntax, device-tree, or linter (`scripts/checkpatch.pl`) error (carry forward the candidate's target `file`, `function_or_symbol`, and `code_snippet` in `locations`).
2. **SYMMETRICAL PROOF BAR & ALL-CALLERS VERIFICATION:** For all other candidates, both `concern_arguments` and `dismissal_arguments` are untrusted hypotheses. Only dismiss a code-behavior candidate in `dismissed_concerns` if concrete, verifiable code in the repository proves the exact failure mechanism, branch, caller, and guest interaction cannot occur across ALL callers, entry points, and modes. Citing a single caller does NOT disprove a buffer overflow, race, missing validation, or lifecycle leak in a helper function unless `git_grep` across all callers and entry points (QOM realize/unrealize paths, MMIO handlers, QMP/HMP commands, and migration callbacks) proves every caller upholds the invariant. Do NOT give code the benefit of the doubt.
3. **LOCAL BOUNDARY & ASYMMETRY RULE:** Do not discard a defect within the modified code of the patch by assuming that surrounding caller systems or parallel execution will safely mask or prevent the issue, or by rationalizing an unpaired realize/unrealize or ref/unref call, an unchecked error return, or overwritten state as a "harmless no-op" or "intentional behavior", unless you can point to specific code in the repository that concretely proves the failure mode is structurally impossible.
4. **PROMOTING SPECULATIVE DISMISSALS:** When `"signal_reason"` is `"speculative_dismissal"`, a previous analyst spotted the candidate bug described in `concern_arguments` and dismissed it using `dismissal_arguments`. Inspect the actual code with tools: if the dismissal's assumption is false or incomplete (for example: another caller does NOT uphold the invariant; the caller does NOT clean up the resource on error or hot-unplug; the cited lock does NOT serialize the access against the vCPU thread or iothread; or an unintended state mutation affects downstream consumers), you MUST report the bug as a verified finding in `findings`.
5. **REFINING PARTIALLY INACCURATE PREMISES:** If a candidate concern contains a partially inaccurate premise while also identifying a real bug in the same code path, refine and report the valid underlying bug rather than discarding the entire candidate.
6. **SERIES VALIDATION & PRE-EXISTING BUG RULE:** If other patches in this series are provided in the context, check whether each candidate is resolved, wired up, or refactored in the final state of the series (`Series End Commit`) using tools (`git_read_files` or `git_diff` with `revision` / `target_revision` set to the `Series End Commit`); do not trust promises in commit messages alone. If resolved by the end of the series, dismiss it in `dismissed_concerns` citing the resolving commit. Never dismiss a real bug into `dismissed_concerns` merely because it already existed before this change (`"preexisting": true` findings are routed to the pre-existing bug tracker and are not posted as patch regressions): if a candidate is a real bug that already existed at `Baseline SHA` and is not fixed at `Series End Commit`, report it in `findings` with `"preexisting": true`. Conversely, if a defect was introduced by an earlier commit in the same series (after `Baseline SHA`) and `Target Commit` does not touch, trigger, or worsen it, dismiss it for this commit rather than marking `"preexisting": true`. When referring to other patches within this series in your explanation, DO NOT use ephemeral git hashes; refer to them by their patch subject (e.g. 'commit "virtio-net: fix RSC handling"').
7. **DESIGN & DOCUMENTATION RULE:** If a candidate targets illustrative pseudo-code or abbreviated snippets in documentation (`docs/`, `README*` files, code comments), inspect the actual implementation at the series head (`Series End Commit` / `HEAD`). If the actual code properly enforces the invariant, dismiss the documentation concern in `dismissed_concerns` citing the enforcing code.
8. **SEVERITY CALIBRATION AND COMPLETENESS:** Set `"preexisting": false` whenever the patch introduces the bug or newly exposes, triggers, or makes reachable a latent defect that could not be triggered before the patch; set `"preexisting": true` when the problem already existed at `Baseline SHA` (before the commit/series) and was already triggerable before the patch, even if it lives in the same function, caller/callee, or error path modified by the patch. For each verified issue, assign an accurate severity (`Critical`, `High`, `Medium`, or `Low`) strictly following `severity.md`: reason through all consequences (both immediate failures and downstream state/resource leaks), triggering paths, and reachability, state that reasoning at the start of `severity_explanation`, preserve all distinct function names, file paths, line numbers when known, triggering entry points, and consequences, and formulate a concise bug title (`problem`) under 80 characters starting with a QEMU component prefix (e.g. `virtio:`, `hw/arm:`, `target/i386:`, `migration:`, `qapi:`, `block:`)."#;

const STAGE_REPORT_INSTRUCTION: &str = r#"# Generate plain-text inline review report

Generate the plain-text inline review report following the exact formatting rules and structure in `report-template.md`.
- Output ONLY a plain bulleted list of ALL findings ordered from highest severity to lowest (`- [CRITICAL] ...`, `- [HIGH] ...`, `- [MEDIUM] ...`, `- [LOW] ...`), or `No issues found.` if there are no findings.
- If any finding has `"preexisting": true`, state explicitly in its explanation that the issue was not introduced by this change.
- Do NOT include `Summary:` or `Findings:` headers.
- Do NOT use backticks, markdown code blocks, or markdown headings. Wrap all lines at 78 characters or fewer."#;

const CONCERN_JSON_SCHEMA_EXAMPLE: &str = r#"Once you have gathered sufficient information, return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
If you find no concerns and no dismissed concerns, return {"concerns": [], "dismissed_concerns": []}.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
- "type": A short category string (e.g. "Guest Input / Out-of-Bounds", "QOM Lifecycle / Leak", "Threading / BQL Violation", "Migration / vmstate Compat", "Error Handling").
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations".
Each location object in "locations" (for both 'concerns' and 'dismissed_concerns') MUST include "file", "function_or_symbol", "line", "code_snippet", "side", and "why_this_location_matters".
- "side": "PARENT" if commenting on deleted or pre-patch code (e.g., '-' lines in the diff or baseline code), or "REVISION" if commenting on newly added or modified code (e.g., '+' lines in the diff or post-patch code). Default to "REVISION" when commenting on unchanged context lines.
In each 'dismissed_concerns' object, "description" is the candidate concern that was investigated and disproved, "reasoning" is the step-by-step explanation of why it is not a bug (citing the exact guard, caller, or invariant), and "locations" MUST cite the concrete disproving code (file, function_or_symbol, line, verbatim code_snippet, side, and why_this_location_matters).
Use the 'dismissed_concerns' array ONLY for candidate concerns that you considered plausible, investigated, and disproved with concrete evidence. This is especially important when you first suspect a concern and then follow the evidence chain proving that it does NOT apply.

PRE-EXISTING BUGS: Do not actively hunt for unrelated bugs outside the reviewed commit and its call paths. However, if while analyzing the commit you spot a concrete bug that already existed in the codebase before this series (`Baseline SHA`) and is not fixed by the commit, you MUST still report it in 'concerns' with `"preexisting": true` (`"preexisting": true` items are routed to the pre-existing bug tracker and are not posted as patch regressions). Never silently drop a real pre-existing bug or place it in 'dismissed_concerns' merely because it was not introduced by this change.

NO DISMISSAL WITHOUT VERIFIED PROOF: To place a candidate issue in 'dismissed_concerns' (or to discard a suspected issue), you MUST find concrete proof in the code ('file', 'function_or_symbol', 'line', and verbatim 'code_snippet' in 'locations') that explicitly invalidates the concern's reasoning. If the disproving code lives outside the diff (for example, in a caller, callee, helper, or configuration), you MUST verify that code first using tools ('git_read_files' or 'git_grep') and quote the verified disproving snippet in 'locations'. If you cannot find definitive code proof that the candidate issue is impossible, you MUST report it in 'concerns' (NOT 'dismissed_concerns') and make the condition explicit: if X is possible, then problem Y can occur.
- Citing a single caller does NOT disprove a buffer overflow, race, missing validation, or lifecycle leak in a helper function. A caller-based dismissal is valid ONLY if every caller in the tree ('git_grep' across all callers and entry points, including QOM realize/unrealize paths, MMIO handlers, QMP/HMP commands, and migration callbacks) is verified to uphold the invariant; otherwise report it in 'concerns'.
- Never dismiss an unpaired lifecycle/state transition (e.g., a realize without a matching unrealize cleanup, a missing object_unref on an error path, or clearing/overwriting state before downstream consumers read it) by rationalizing that the side effect is a "harmless no-op" or "rare edge case".

SPECIFICITY REQUIREMENT: When reporting a concern or dismissed_concern, cite exact function name(s), file path(s), and line number(s) when known. Do not invent line numbers; use null when exact values are unknown.

CRITICAL REVIEW DIRECTIVE: Do NOT dismiss concerns just because you assume the surrounding device framework or callers handle it perfectly. Do not be overly charitable to the code. Assume the guest can drive every emulated register write, DMA descriptor, and device state transition in arbitrary order.

Example Output:
```json
{
  "concerns": [
    {
      "type": "Memory Safety",
      "description": "Guest-controlled descriptor index used without a bounds check",
      "reasoning": "1. The guest writes idx into the descriptor on the MMIO write path.\n2. idx is used to index the ring on line 415 without a bounds check against the queue size.",
      "preexisting": false,
      "locations": [
        {
          "file": "hw/net/virtio-net.c",
          "function_or_symbol": "virtio_net_handle_rsc",
          "line": 415,
          "code_snippet": "entry = &queue->vring.desc[idx];",
          "side": "REVISION",
          "why_this_location_matters": "A guest-controlled index reaching the ring base allows an out-of-bounds read of host memory."
        }
      ]
    }
  ],
  "dismissed_concerns": [
    {
      "type": "Error Handling",
      "description": "Potential leak when vmstate_load_state fails mid-array",
      "reasoning": "Verified that the goto fail path on line 220 frees the allocation from line 190 before returning.",
      "locations": [
        {
          "file": "hw/misc/foo.c",
          "function_or_symbol": "foo_load",
          "line": 220,
          "code_snippet": "goto fail;",
          "side": "REVISION",
          "why_this_location_matters": "Proves the allocation is released on the error path."
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
        name: "locking",
        short: "Locking & Sync",
        instruction: STAGE_LOCKING_INSTRUCTION,
        guides: &[],
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
    template: PromptTemplate<QemuPatchReviewState>,
    wants: bool,
) -> PromptTemplate<QemuPatchReviewState> {
    if !wants {
        return template;
    }
    template.with_var("follow_up_series_section", |s: &QemuPatchReviewState| {
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

pub fn is_stage_exclusive_guide(name: &str) -> bool {
    ANALYSIS_STAGES
        .iter()
        .flat_map(|def| def.guides)
        .any(|guide| guide.rsplit('/').next() == Some(name))
}

pub fn is_known_stage(name: &str) -> bool {
    let normalized = normalize_stage_name(name);
    analysis_stage_by_name(&normalized).is_some()
        || consolidation_stage_by_name(&normalized).is_some()
        || matches!(normalized.as_str(), "pre-screen" | "planning")
}

// ---------------------------------------------------------------------------
// Validators and Helpers
// ---------------------------------------------------------------------------

fn validate_concerns_output(
    output: &StageConcernsOutput,
    _state: &QemuPatchReviewState,
) -> Result<(), String> {
    for (idx, concern) in output.concerns.iter().enumerate() {
        let Some(obj) = concern.as_object() else {
            return Err(format!("concerns[{idx}] must be a JSON object."));
        };
        let has_type = obj
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_desc = obj
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_reasoning = obj
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_type || !has_desc || !has_reasoning {
            return Err(format!(
                "concerns[{idx}] must have non-empty 'type', 'description', and 'reasoning' strings."
            ));
        }
        if !obj.get("preexisting").is_some_and(Value::is_boolean) {
            return Err(format!(
                "concerns[{idx}] must have a boolean 'preexisting' field (true or false)."
            ));
        }
        let locations = obj.get("locations").and_then(Value::as_array);
        if locations.is_none() {
            return Err(format!("concerns[{idx}] must have a 'locations' array."));
        }
        validate_locations_side(locations.unwrap(), "concerns", idx)?;
    }

    for (idx, dismissed) in output.dismissed_concerns.iter().enumerate() {
        let Some(obj) = dismissed.as_object() else {
            return Err(format!("dismissed_concerns[{idx}] must be a JSON object."));
        };
        let has_type = obj
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_desc = obj
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        let has_reasoning = obj
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_type || !has_desc || !has_reasoning {
            return Err(format!(
                "dismissed_concerns[{idx}] must have non-empty 'type', 'description', and 'reasoning' strings."
            ));
        }
        if let Some(locations) = obj.get("locations").and_then(Value::as_array) {
            validate_locations_side(locations, "dismissed_concerns", idx)?;
        }
        if !has_valid_proof_location(dismissed) {
            return Err(format!(
                "dismissed_concerns[{idx}] must include at least one entry in 'locations' with non-empty 'file', 'function_or_symbol', valid 'side' ('PARENT' or 'REVISION'), and verbatim disproving 'code_snippet' proving the candidate concern cannot occur. If you do not have concrete code proof, move the candidate issue to 'concerns' instead."
            ));
        }
    }

    Ok(())
}

fn format_concerns_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. You MUST return ONLY a JSON object containing 'concerns' (each with 'type', non-empty 'description' and 'reasoning' strings, a boolean 'preexisting', and a 'locations' array) and 'dismissed_concerns' (each with 'type', non-empty 'description' and 'reasoning' strings, and a 'locations' array) arrays. If there are no concerns and no dismissed concerns, return `{{\"concerns\": [], \"dismissed_concerns\": []}}`.",
        violation
    )
}

fn validate_qemu_report_format(content: &str, state: &QemuPatchReviewState) -> Result<(), String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err("The inline review report cannot be empty.".to_string());
    }
    if trimmed.contains('`') {
        return Err(
            "The report contains backticks ('`'). Use strictly plain text without backticks or markdown code blocks."
                .to_string(),
        );
    }
    for line in trimmed.lines() {
        let l = line.trim_start();
        if l.starts_with("# ") || l.starts_with("## ") || l.starts_with("### ") {
            return Err(
                "Do not use markdown headings ('#') in the plain-text review report. Follow report-template.md."
                    .to_string(),
            );
        }
        if l.starts_with("Summary:") || l.starts_with("Findings:") {
            return Err(
                "Do not include 'Summary:' or 'Findings:' headers in the inline review report. Output ONLY the plain bulleted list of findings (or 'No issues found.')."
                    .to_string(),
            );
        }
        if !line.starts_with("    ") && !line.starts_with('\t') && line.chars().count() > 84 {
            return Err(format!(
                "Line exceeds 78-character terminal width ({} chars): \"{}...\". Wrap all prose lines at 78 characters.",
                line.chars().count(),
                line.chars().take(40).collect::<String>()
            ));
        }
    }
    if !state.findings.is_empty() {
        let has_severity_bullet = trimmed.lines().any(|line| {
            let l = line.trim_start();
            l.starts_with("- [CRITICAL]")
                || l.starts_with("- [HIGH]")
                || l.starts_with("- [MEDIUM]")
                || l.starts_with("- [LOW]")
        });
        if !has_severity_bullet {
            return Err(
                "Findings were provided in state, but the report does not list them as bullets starting with '- [CRITICAL]', '- [HIGH]', '- [MEDIUM]', or '- [LOW]'. Include every finding."
                    .to_string(),
            );
        }
    }
    Ok(())
}

pub fn format_qemu_inline_findings(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "No issues found." {
        return trimmed.to_string();
    }

    let mut out_lines: Vec<&str> = Vec::new();
    for line in trimmed.lines() {
        let l = line.trim_start();
        let is_bullet_severity = l.starts_with("- [CRITICAL]")
            || l.starts_with("- [HIGH]")
            || l.starts_with("- [MEDIUM]")
            || l.starts_with("- [LOW]")
            || l.starts_with("- [critical]")
            || l.starts_with("- [high]")
            || l.starts_with("- [medium]")
            || l.starts_with("- [low]");

        if is_bullet_severity
            && !out_lines.is_empty()
            && !out_lines.last().unwrap().trim().is_empty()
        {
            out_lines.push("");
        }
        if line.trim().is_empty() {
            if out_lines.last().is_some_and(|prev| !prev.trim().is_empty()) {
                out_lines.push("");
            }
        } else {
            out_lines.push(line.trim_end());
        }
    }
    out_lines.join("\n")
}

fn format_qemu_report_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. Follow `report-template.md`: return strictly plain text without backticks, markdown headings, or 'Summary:'/'Findings:' headers; wrap prose lines at 78 characters; separate individual findings with an empty line; and list every finding as a bullet starting with '- [<SEVERITY>]'.",
        violation
    )
}

// ---------------------------------------------------------------------------
// Stage Builders
// ---------------------------------------------------------------------------

pub fn validate_qemu_planning_output_with_effort(
    out: &PlanningOutput,
    _state: &QemuPatchReviewState,
) -> Result<(), String> {
    validate_planning_stage_efforts(analysis_stage_by_name, out)
}

pub fn planning_stage() -> Stage<QemuPatchReviewState, PlanningOutput> {
    planning_stage_with_effort(0.0, None)
}

pub fn planning_stage_with_effort(
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Stage<QemuPatchReviewState, PlanningOutput> {
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
                "You are an AI assistant planning a QEMU patch review.\nThe core stages (goal, implementation, execution-flow) always run.\n1. Select which optional specialized stages should also run based on the patch contents.\nAvailable optional stages: [{optional_list}]\nInclude an optional stage when the patch touches or interacts with that domain; omit stages that are clearly irrelevant to the modified code.\n\n2. Select the reasoning effort (\"medium\" or \"high\") in stage_efforts for each stage that will run (goal, implementation, execution-flow, plus each stage selected in relevant_stages):\n- Use \"high\" for stages whose domain is central to the patch or where complex control flow, subtle error/teardown paths, concurrency, or guest-input validation must be verified.\n- Use \"medium\" for stages performing a routine check on a straightforward aspect of the patch.\n\nRespond with ONLY a JSON object. Example:\n```json\n{{\"relevant_stages\": [\"resources\", \"security\"], \"stage_efforts\": {{\"goal\": \"medium\", \"implementation\": \"high\", \"execution-flow\": \"high\", \"resources\": \"high\", \"security\": \"medium\"}}}}\n```"
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
            .with_validator(validate_qemu_planning_output_with_effort)
            .with_feedback_formatter(format_planning_feedback),
        )
    } else {
        (
            format!(
                "You are an AI assistant planning a QEMU patch review.\nThe core stages (goal, implementation, execution-flow) always run.\nSelect which optional specialized stages should also run based on the patch contents.\nAvailable optional stages: [{optional_list}]\n\nCRITICAL BIAS RULE: Err on the side of inclusion. Include any stage whose domain could plausibly be affected by the patch.\n\nRespond with ONLY a JSON object listing the relevant optional stages. Example:\n```json\n{{\"relevant_stages\": [\"resources\", \"security\"]}}\n```"
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
        .system_prompt(PromptTemplate::<QemuPatchReviewState>::new(sys_prompt))
        .user_prompt(
            PromptTemplate::<QemuPatchReviewState>::new(
                "<patch>\n{{target_commit_diff}}\n</patch>",
            )
            .with_var("target_commit_diff", |s: &QemuPatchReviewState| {
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
) -> Box<dyn ExecutableStage<QemuPatchReviewState>> {
    let series_context = series_context_placeholder(def.wants_series_context);
    let mut user_template = PromptTemplate::<QemuPatchReviewState>::new(format!(
        "{}{}\n\n{}",
        def.instruction, series_context, CONCERN_JSON_SCHEMA_EXAMPLE
    ));

    for guide in def.guides {
        user_template = user_template.include_file(*guide);
    }

    let user_template = with_series_context(user_template, def.wants_series_context);

    Box::new(
        Stage::builder(def.name)
            .system_prompt(qemu_system_prompt(def.uses_commit_log))
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
            .reduce_with_outcome(
                move |state: &mut QemuPatchReviewState,
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
    state: &QemuPatchReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<QemuPatchReviewState>>> {
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
            None => tracing::warn!("Ignoring unknown QEMU review stage {:?}", name),
        }
    }
    stages
}

pub fn verification_stage(
    max_turns: usize,
    temperature: f32,
) -> Stage<QemuPatchReviewState, VerificationOutput> {
    verification_stage_with_effort(max_turns, temperature, None)
}

pub fn verification_stage_with_effort(
    max_turns: usize,
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Stage<QemuPatchReviewState, VerificationOutput> {
    let series_context = series_context_placeholder(VERIFICATION.wants_series_context);
    let user_template = with_series_context(
        PromptTemplate::<QemuPatchReviewState>::new(format!(
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
- LINEAGE REQUIREMENT ('source_ids'): Every item in Aggregated Concerns has an "id" ("C1", "C2", ...) and every item in Aggregated Dismissed Concerns has an "id" ("D1", "D2", ...). Every object in 'findings', 'hard_cases', and 'dismissed_concerns' MUST include a non-empty "source_ids" array of strings listing the exact input "C*" and/or "D*" IDs consolidated into that output item. Every input "C*" and "D*" ID MUST appear in at least one output item's "source_ids". Category 1a 'findings' may ONLY reference "C*" IDs (any contested "C*" + "D*" or promoted "D*" must be routed to 'hard_cases'), and Category 1b 'dismissed_concerns' may ONLY reference "D*" IDs in "source_ids" (never "C*" IDs).
- Each object in 'findings' (Category 1a: Well-Justified Concerns) MUST use the keys: "source_ids" (non-empty array of "C*" IDs only), "problem" (a short naming string under 80 characters starting with a QEMU component prefix like 'virtio:', 'hw/arm:', 'target/i386:', 'migration:', 'qapi:', 'block:', NEVER using backquotes), "severity" ("Low", "Medium", "High", "Critical", or "Unknown"), "severity_explanation" (detailed reasoning and proof), "preexisting" (boolean), and "locations" (array of objects with file, function_or_symbol, line, code_snippet, side, and why_this_location_matters; side MUST be "PARENT" for deleted/pre-patch code or "REVISION" for added/modified/context code), and may include "stages" (array of stage names) and "prompts" (array of prompt files).
- Each object in 'hard_cases' (Category 2: Speculative or Contested) MUST use the keys: "source_ids" (non-empty array of input IDs), "type", "description", "estimated_severity" ("Low", "Medium", "High", "Critical", or "Unknown"), "signal_reason" ("mixed_signals", "speculative_concern", "speculative_dismissal", "series_interaction", or "other"), "concern_arguments" (consolidated arguments for why the bug can occur), "dismissal_arguments" (consolidated arguments/snippets from any competing or standalone dismissal, or "" if none), "verification_question" (the specific code question post-verification must answer with tools), "preexisting" (boolean), and "locations" (array of objects with file, function_or_symbol, line, code_snippet, side, and why_this_location_matters; side MUST be "PARENT" or "REVISION"), and may include "stages" and "prompts".
- Each object in 'dismissed_concerns' (Category 1b: Well-Justified Dismissals) MUST use the keys: "source_ids" (non-empty array of "D*" IDs only), "type", "description", "reasoning", and "locations" (array of objects with file, function_or_symbol, line, code_snippet, side, and why_this_location_matters; side MUST be "PARENT" or "REVISION"), and may include "stages" and "prompts"."#
        ))
        .include_file("false-positive-guide.md")
        .include_file("severity.md")
        .include_files_from_state(|s: &QemuPatchReviewState| {
            extra_prompt_paths_for_state(s, analysis_stage_by_name)
        }),
        VERIFICATION.wants_series_context,
    )
    .with_var("aggregated_concerns", |s: &QemuPatchReviewState| {
        serde_json::to_string_pretty(&s.all_concerns).unwrap_or_default()
    })
    .with_var(
        "aggregated_dismissed_concerns",
        |s: &QemuPatchReviewState| {
            serde_json::to_string_pretty(&s.all_dismissed_concerns).unwrap_or_default()
        },
    );

    Stage::builder(VERIFICATION.name)
        .system_prompt(qemu_system_prompt(true))
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
) -> Stage<QemuPatchReviewState, PostVerificationOutput> {
    post_verification_stage_with_effort(stage_name, batch, max_turns, temperature, None)
}

pub fn post_verification_stage_with_effort(
    stage_name: &'static str,
    batch: Vec<Value>,
    max_turns: usize,
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Stage<QemuPatchReviewState, PostVerificationOutput> {
    let candidate_json = serde_json::to_string_pretty(&batch).unwrap_or_default();
    let batch_for_prompts = batch.clone();
    let batch_for_validate = batch.clone();
    let batch_for_reduce = batch;
    let series_context = series_context_placeholder(POST_VERIFICATION.wants_series_context);
    let user_template = with_series_context(
        PromptTemplate::<QemuPatchReviewState>::new(format!(
            r#"{STAGE_POST_VERIFICATION_INSTRUCTION}

<false_positive_guide>
@include("false-positive-guide.md")
</false_positive_guide>

<severity_guidelines>
@include("severity.md")
</severity_guidelines>@includes

CRITICAL REVIEW DIRECTIVE: To dismiss a code-behavior candidate issue as a false positive, you must find concrete evidence in the code that proves the issue is invalid and quote that disproving code in `dismissed_concerns[].locations` (for policy-excluded build/linter or out-of-scope meta-concerns under rule 1, carry forward the candidate's target location or commit message snippet in `locations`). If you cannot find concrete proof of safety, you must validate and report the finding in `findings`.{series_context}

Candidate Hard Case(s) to Verify:
{{{{candidate_hard_cases}}}}

Return ONLY a JSON object with 'findings' and 'dismissed_concerns' arrays. Every candidate in this batch MUST be accounted for in either 'findings' (if validated) or 'dismissed_concerns' (if disproved by concrete code or excluded by rule 1; never return both empty arrays).
- LINEAGE REQUIREMENT ('source_ids'): Every candidate hard case in this batch has an "id" ("H1", "H2", ...). Every object in 'findings' and 'dismissed_concerns' MUST include a non-empty "source_ids" array of strings listing the exact candidate "H*" ID(s) from this batch that it resolves, and every candidate "H*" ID in this batch MUST be accounted for in at least one output item's "source_ids".
- Each object in 'findings' MUST use: "source_ids" (non-empty array of batch "H*" IDs), "problem" (a short naming string under 80 characters starting with a QEMU component prefix like 'virtio:', 'hw/arm:', 'target/i386:', 'migration:', 'qapi:', 'block:', NEVER using backquotes), "severity" (Low, Medium, High, Critical, or Unknown), "severity_explanation" (detailed reasoning and proof), "preexisting" (boolean), "locations" (array of objects with file, function_or_symbol, line, code_snippet, side, and why_this_location_matters; side MUST be "PARENT" for deleted/pre-patch code or "REVISION" for added/modified/context code).
- Each object in 'dismissed_concerns' MUST use: "source_ids" (non-empty array of batch "H*" IDs), "description" (the candidate issue that was disproved), "reasoning" (step-by-step explanation of how the inspected code or policy rule disproves the candidate), and "locations" (a non-empty array of objects with file, function_or_symbol, line, code_snippet, side, and why_this_location_matters; side MUST be "PARENT" or "REVISION", quoting the verbatim disproving code or target snippet)."#
        ))
        .include_file("false-positive-guide.md")
        .include_file("severity.md")
        .include_files_from_state(move |s: &QemuPatchReviewState| {
            extra_prompt_paths_for_items(
                &s.selected_guides,
                &batch_for_prompts,
                analysis_stage_by_name,
            )
        }),
        POST_VERIFICATION.wants_series_context,
    )
    .with_var("candidate_hard_cases", move |_: &QemuPatchReviewState| {
        candidate_json.clone()
    });

    Stage::builder(stage_name)
        .system_prompt(qemu_system_prompt(true))
        .user_prompt(user_template)
        .output_format(
            OutputFormat::json()
                .with_validator(
                    move |out: &PostVerificationOutput, _: &QemuPatchReviewState| {
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
) -> Box<dyn ExecutableStage<QemuPatchReviewState>> {
    Box::new(post_verification_stage_with_effort(
        stage_name,
        batch,
        max_turns,
        temperature,
        None,
    ))
}

pub fn resolve_post_verification_stages_with_options(
    state: &QemuPatchReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<QemuPatchReviewState>>> {
    let effort = state.effort;
    let batches = batch_hard_cases_by_severity(&state.hard_cases);
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
            )) as Box<dyn ExecutableStage<QemuPatchReviewState>>
        })
        .collect()
}

pub fn report_stage(max_turns: usize, temperature: f32) -> Stage<QemuPatchReviewState, String> {
    report_stage_with_effort(max_turns, temperature, None)
}

pub fn report_stage_with_effort(
    max_turns: usize,
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Stage<QemuPatchReviewState, String> {
    Stage::builder(REPORT.name)
        .system_prompt(qemu_system_prompt(true))
        .user_prompt(
            PromptTemplate::<QemuPatchReviewState>::new(format!(
                r#"{STAGE_REPORT_INSTRUCTION}

<report_template>
@include("report-template.md")
</report_template>

Findings:
{{{{findings}}}}

Return strictly plain text output (no markdown, no backticks, wrapped at 78 characters), not JSON."#
            ))
            .include_file("report-template.md")
            .with_var("findings", |s: &QemuPatchReviewState| {
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
// Complete QEMU Review Workflow Graph
// ---------------------------------------------------------------------------

pub fn build_qemu_patch_review_workflow() -> Workflow<QemuPatchReviewState> {
    build_qemu_patch_review_workflow_with_options(20, 0.0)
}

/// Builds the QEMU patch review workflow.
///
/// Like [`crate::workflows::gcc_patch_review::build_gcc_patch_review_workflow_with_effort`],
/// the workflow early-exits when the analysis fan-out produced no concerns or
/// dismissed concerns, and again when verification validated no findings, so
/// a clean patch costs only the planning and core analysis stages.
pub fn build_qemu_patch_review_workflow_with_options(
    max_turns: usize,
    temperature: f32,
) -> Workflow<QemuPatchReviewState> {
    build_qemu_patch_review_workflow_with_effort(max_turns, temperature, None)
}

pub fn build_qemu_patch_review_workflow_with_effort(
    max_turns: usize,
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Workflow<QemuPatchReviewState> {
    let stage_max_turns = max_turns.min(20);
    Workflow::builder("qemu_patch_review")
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

    #[test]
    fn test_qemu_analysis_stages_table() {
        assert_eq!(ANALYSIS_STAGES.len(), 6);
        let names: Vec<&str> = ANALYSIS_STAGES.iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![
                "goal",
                "implementation",
                "execution-flow",
                "resources",
                "locking",
                "security",
            ]
        );

        // The three-layer prompt architecture loads maintainer notes and
        // code notes deterministically instead of per-stage static guides.
        for def in ANALYSIS_STAGES {
            assert!(
                def.guides.is_empty(),
                "{}: static guides belong to the retired pre-screen paradigm",
                def.name
            );
        }

        // Core always-on stages
        assert!(!analysis_stage_by_name("goal").unwrap().optional);
        assert!(!analysis_stage_by_name("implementation").unwrap().optional);
        assert!(!analysis_stage_by_name("execution-flow").unwrap().optional);

        // Specialized planner-gated stages
        assert!(analysis_stage_by_name("resources").unwrap().optional);
        assert!(analysis_stage_by_name("locking").unwrap().optional);
        assert!(analysis_stage_by_name("security").unwrap().optional);
    }

    #[test]
    fn test_qemu_stage_lookup_and_labels() {
        assert_eq!(stage_short_label("Execution_Flow"), Some("Execution Flow"));
        assert_eq!(stage_short_label("resources"), Some("Resource Mgmt"));
        assert_eq!(
            stage_short_label("post-verification"),
            Some("Post-Verification")
        );
        assert_eq!(
            stage_short_label("post-verification-3"),
            Some("Post-Verification")
        );
        assert_eq!(stage_short_label("report"), Some("Report Generation"));
        assert!(is_known_stage("planning"));
        assert!(is_known_stage("goal"));
        assert!(is_known_stage("verification"));
        assert!(is_known_stage("post-verification"));
        assert!(is_known_stage("post-verification-1"));
        assert!(is_known_stage("report"));
        assert!(!is_known_stage("summary"));
        assert!(!is_known_stage("hardware"));
        assert!(!is_known_stage("persistence"));
        assert!(!is_stage_exclusive_guide("guest-input.md"));
    }

    #[test]
    fn test_qemu_report_validator() {
        let mut state = QemuPatchReviewState::default();
        assert!(validate_qemu_report_format("No issues found.", &state).is_ok());
        assert!(validate_qemu_report_format("   ", &state).is_err());
        assert!(validate_qemu_report_format("Adds `backticks` here.", &state).is_err());
        assert!(
            validate_qemu_report_format("# Top-level heading\nNo issues found.", &state).is_err()
        );
        assert!(
            validate_qemu_report_format(
                "Summary: Adds device support.\n\nFindings:\n- [HIGH] Test issue.",
                &state
            )
            .is_err()
        );
        let long_line = "a".repeat(90);
        assert!(validate_qemu_report_format(&long_line, &state).is_err());

        state
            .findings
            .push(json!({"severity": "High", "problem": "Test issue"}));
        assert!(validate_qemu_report_format("1 finding: highest severity High.", &state).is_err());
        assert!(
            validate_qemu_report_format(
                "- [HIGH] In hw/net/virtio-net.c (virtio_net_handle_rsc), the \n  guest index lacks a bounds check, allowing OOB read.",
                &state
            )
            .is_ok()
        );
    }

    #[test]
    fn test_build_qemu_patch_review_workflow() {
        let wf = build_qemu_patch_review_workflow();
        assert_eq!(wf.name, "qemu_patch_review");
        // analysis fan-out, no-findings guard, verification fan-out,
        // no-findings guard, report
        assert_eq!(wf.steps.len(), 5);
        assert_eq!(
            planning_stage_with_effort(1.0, None).policy.temperature,
            1.0
        );
    }

    #[test]
    fn test_qemu_resolve_selects_maintainer_notes() {
        let prompts_dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("projects/qemu/prompts");
        let files = vec!["hw/net/virtio-net.c".to_string()];
        let diff = "\
diff --git a/hw/net/virtio-net.c b/hw/net/virtio-net.c
--- a/hw/net/virtio-net.c
+++ b/hw/net/virtio-net.c
@@ -100,3 +100,4 @@
+  dma_memory_write (address_space, addr, &val, sizeof (val));
";

        let notes = crate::notes::maintainer_notes::load_maintainer_notes(&prompts_dir);
        let idents = crate::notes::maintainer_notes::diff_identifiers_all(diff);
        let selected: Vec<_> = notes
            .iter()
            .filter(|n| n.applies(&files, &idents))
            .map(|n| n.name.as_str())
            .collect();
        assert!(
            selected.contains(&"general"),
            "repository-wide note must load for every patch: {selected:?}"
        );
        assert!(
            selected.contains(&"guest-input"),
            "hw/ patch with dma_memory_write must load the guest-input note: {selected:?}"
        );
        assert!(
            !selected.contains(&"riscv"),
            "a virtio-net patch must not load the RISC-V note: {selected:?}"
        );
    }

    #[test]
    fn test_qemu_maintainer_notes_render_before_code_notes() {
        let rendered = qemu_system_prompt(true).render_for_log(&QemuPatchReviewState {
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
    fn test_qemu_analysis_stage_dismissal_guidance() {
        for required in [
            "the candidate concern that was investigated and disproved",
            "NO DISMISSAL WITHOUT VERIFIED PROOF",
            "MUST cite the concrete disproving code",
        ] {
            assert!(
                CONCERN_JSON_SCHEMA_EXAMPLE.contains(required),
                "QEMU analysis stage guidance lost: {required}"
            );
        }
        for (name, instruction) in [
            ("implementation", STAGE_IMPLEMENTATION_INSTRUCTION),
            ("execution-flow", STAGE_EXECUTION_FLOW_INSTRUCTION),
            ("resources", STAGE_RESOURCES_INSTRUCTION),
            ("locking", STAGE_LOCKING_INSTRUCTION),
        ] {
            assert!(
                instruction.contains("Do not stop after finding several bugs"),
                "QEMU {name} stage must include full-hunk sweep directive"
            );
        }
        assert!(
            STAGE_VERIFICATION_INSTRUCTION.contains("speculative_dismissal")
                && STAGE_VERIFICATION_INSTRUCTION
                    .contains("Single-caller or happy-path-only proofs")
                && STAGE_VERIFICATION_INSTRUCTION
                    .contains("Asymmetry or cleared-state rationalizations"),
            "QEMU verification stage must classify speculative, single-caller, and asymmetry dismissals into hard_cases"
        );
        assert!(
            STAGE_POST_VERIFICATION_INSTRUCTION.contains("SYMMETRICAL PROOF BAR")
                && STAGE_POST_VERIFICATION_INSTRUCTION.contains("ALL-CALLERS VERIFICATION"),
            "QEMU post-verification stage must enforce symmetrical proof bar and all-callers verification"
        );

        let state = QemuPatchReviewState::default();
        let v_stage = verification_stage(5, 0.2);
        assert!(
            v_stage
                .user_prompt
                .render_for_log(&state)
                .contains(r#"side MUST be "PARENT""#),
            "QEMU verification stage user prompt must define valid side enum values"
        );
        let pv_stage = post_verification_stage("post-verification-1", vec![], 5, 0.2);
        assert!(
            pv_stage
                .user_prompt
                .render_for_log(&state)
                .contains(r#"side MUST be "PARENT""#),
            "QEMU post-verification stage user prompt must define valid side enum values"
        );
    }

    #[test]
    fn test_format_qemu_inline_findings_inserts_empty_lines() {
        let raw = "- [HIGH] First finding line one\n  first finding continuation.\n- [MEDIUM] Second finding line one\n  second finding continuation.\n- [LOW] Third finding.";
        let formatted = format_qemu_inline_findings(raw);
        assert_eq!(
            formatted,
            "- [HIGH] First finding line one\n  first finding continuation.\n\n- [MEDIUM] Second finding line one\n  second finding continuation.\n\n- [LOW] Third finding."
        );
        // Idempotent when already separated by empty lines
        assert_eq!(format_qemu_inline_findings(&formatted), formatted);
    }

    #[test]
    fn test_series_context_enabled_for_qemu_wiring_and_verification_stages() {
        for stage_name in ["goal", "implementation"] {
            let def = analysis_stage_by_name(stage_name).expect(stage_name);
            assert!(
                def.wants_series_context,
                "{stage_name} should have wants_series_context enabled"
            );
        }
        assert!(VERIFICATION.wants_series_context);
        assert!(POST_VERIFICATION.wants_series_context);
    }

    #[tokio::test]
    async fn test_workflow_forwards_temperature_to_planning() {
        struct RecordingProvider {
            temperatures: std::sync::Mutex<Vec<Option<f32>>>,
        }

        #[async_trait::async_trait]
        impl crate::ai::AiProvider for RecordingProvider {
            async fn generate_content(
                &self,
                request: crate::ai::AiRequest,
            ) -> anyhow::Result<crate::ai::AiResponse> {
                self.temperatures.lock().unwrap().push(request.temperature);
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
        let prompts_dir = std::path::Path::new("projects/qemu/prompts");

        let provider = std::sync::Arc::new(RecordingProvider {
            temperatures: std::sync::Mutex::new(Vec::new()),
        });
        let tools = std::sync::Arc::new(crate::toolbox::ToolBox::new(
            temp_dir.path().to_path_buf(),
            None,
        ));
        let env = crate::workflow::stage::WorkflowEnv {
            provider: provider.clone(),
            tools,
            base_dir: prompts_dir,
            context_tag: None,
        };

        let wf = build_qemu_patch_review_workflow_with_options(1, 1.0);
        let mut state = QemuPatchReviewState::default();
        crate::workflow::WorkflowEngine::execute(&wf, &env, &mut state, None)
            .await
            .unwrap();

        let recorded = provider.temperatures.lock().unwrap().clone();
        // planning + 3 core analysis stages (goal, implementation,
        // execution-flow); the empty analysis results trigger the early exit
        // before verification and report.
        assert_eq!(recorded.len(), 4);
        assert!(
            recorded.iter().all(|&t| t == Some(1.0)),
            "expected all stages including planning to receive temperature 1.0, got {recorded:?}"
        );
    }

    #[tokio::test]
    async fn test_qemu_verification_routes_preexisting_only_to_concerns_not_findings() {
        struct MockVerificationProvider;

        #[async_trait::async_trait]
        impl crate::ai::AiProvider for MockVerificationProvider {
            async fn generate_content(
                &self,
                _request: crate::ai::AiRequest,
            ) -> anyhow::Result<crate::ai::AiResponse> {
                Ok(crate::ai::AiResponse {
                    content: Some(
                        r#"{
                            "findings": [
                                {
                                    "problem": "migration: pre-existing missing vmstate subsection guard",
                                    "severity": "Medium",
                                    "severity_explanation": "Existed before this commit.",
                                    "preexisting": true,
                                    "locations": []
                                },
                                {
                                    "problem": "virtio: newly introduced guest index without bounds check",
                                    "severity": "High",
                                    "severity_explanation": "Introduced by this patch.",
                                    "preexisting": false,
                                    "locations": []
                                }
                            ],
                            "hard_cases": [],
                            "dismissed_concerns": []
                        }"#
                        .to_string(),
                    ),
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
        let prompts_dir = temp_dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(prompts_dir.join("false-positive-guide.md"), "").unwrap();
        std::fs::write(prompts_dir.join("severity.md"), "").unwrap();

        let provider = std::sync::Arc::new(MockVerificationProvider);
        let tools = std::sync::Arc::new(crate::toolbox::ToolBox::new(
            temp_dir.path().to_path_buf(),
            None,
        ));
        let env = crate::workflow::stage::WorkflowEnv {
            provider,
            tools,
            base_dir: &prompts_dir,
            context_tag: None,
        };

        let mut state = QemuPatchReviewState {
            all_concerns: vec![json!({
                "stage": "security",
                "stages": ["security"],
                "prompts": ["review-core.md"],
                "description": "candidate"
            })],
            ..Default::default()
        };

        let stage = verification_stage(1, 0.0);
        stage.execute(&env, &mut state, None).await.unwrap();

        assert_eq!(state.findings.len(), 1);
        assert_eq!(
            state.findings[0]["problem"],
            "virtio: newly introduced guest index without bounds check"
        );
        assert_eq!(state.findings[0]["stage"], "security");
        assert_eq!(state.findings[0]["stages"], json!(["security"]));
        assert_eq!(state.findings[0]["prompts"], json!(["review-core.md"]));
        assert_eq!(state.concerns.len(), 1);
        assert_eq!(
            state.concerns[0]["description"],
            "migration: pre-existing missing vmstate subsection guard"
        );
        assert_eq!(state.concerns[0]["preexisting"], true);
        assert_eq!(state.concerns[0]["stage"], "security");
        assert_eq!(state.concerns[0]["stages"], json!(["security"]));
        assert_eq!(state.concerns[0]["prompts"], json!(["review-core.md"]));
        // Verification keeps the raw, ID-tagged inputs intact for the review
        // map and records its own verified outputs separately.
        assert_eq!(
            state.all_concerns,
            vec![json!({
                "stage": "security",
                "stages": ["security"],
                "prompts": ["review-core.md"],
                "description": "candidate"
            })]
        );
        assert_eq!(state.verification_findings.len(), 2);

        // When report_preexisting is true, the verified pre-existing finding
        // is also retained in findings, and state.concerns appends without
        // clearing earlier items.
        let mut state_with_preexisting = QemuPatchReviewState {
            report_preexisting: true,
            all_concerns: vec![json!({"description": "candidate"})],
            concerns: vec![json!({"description": "earlier concern", "preexisting": true})],
            ..Default::default()
        };
        stage
            .execute(&env, &mut state_with_preexisting, None)
            .await
            .unwrap();
        assert_eq!(state_with_preexisting.findings.len(), 2);
        assert_eq!(state_with_preexisting.findings[0]["preexisting"], true);
        assert_eq!(state_with_preexisting.findings[1]["preexisting"], false);
        assert_eq!(state_with_preexisting.concerns.len(), 2);
        assert_eq!(
            state_with_preexisting.concerns[0]["description"],
            "earlier concern"
        );
        assert_eq!(
            state_with_preexisting.concerns[1]["description"],
            "migration: pre-existing missing vmstate subsection guard"
        );
    }

    #[test]
    fn test_qemu_stage_output_validators() {
        let state = QemuPatchReviewState::default();
        let ver_stage = verification_stage(20, 0.0);

        // Missing top-level arrays or stray inner objects must fail.
        assert!(ver_stage.output_format.validate("{}", &state).is_err());
        assert!(
            ver_stage
                .output_format
                .validate(r#"{"file": "a.c"}"#, &state)
                .is_err()
        );

        // Malformed findings or dismissed_concerns in verification must fail.
        let err = ver_stage
            .output_format
            .validate(
                r#"{"findings": [{"file": "a.c"}], "hard_cases": [], "dismissed_concerns": []}"#,
                &state,
            )
            .expect_err("finding missing problem must fail");
        assert!(err.contains("findings[0]"));

        let err = ver_stage
            .output_format
            .validate(
                r#"{"findings": [{"problem": "virtio: bug", "severity": "High", "severity_explanation": "explain", "preexisting": false}], "hard_cases": [], "dismissed_concerns": []}"#,
                &state,
            )
            .expect_err("finding missing locations must fail");
        assert!(err.contains("locations"));

        // Valid outputs succeed.
        assert!(
            ver_stage
                .output_format
                .validate(
                    r#"{"findings": [{"problem": "virtio: missing bounds check", "severity": "High", "severity_explanation": "Unchecked guest index", "preexisting": false, "locations": []}], "hard_cases": [], "dismissed_concerns": []}"#,
                    &state
                )
                .is_ok()
        );

        let pv_ok = PostVerificationOutput {
            findings: vec![json!({
                "problem": "virtio: missing bounds check",
                "severity": "High",
                "severity_explanation": "Unchecked guest index",
                "preexisting": false,
                "locations": []
            })],
            dismissed_concerns: vec![],
        };
        assert!(
            crate::workflows::linux_patch_review::validate_post_verification_batch_output(
                &pv_ok, 1
            )
            .is_ok()
        );
        assert!(
            crate::workflows::linux_patch_review::validate_post_verification_batch_output(
                &pv_ok, 2
            )
            .is_err()
        );

        let pv_bad = PostVerificationOutput {
            findings: vec![json!({"file": "a.c"})],
            dismissed_concerns: vec![],
        };
        assert!(
            crate::workflows::linux_patch_review::validate_post_verification_batch_output(
                &pv_bad, 1
            )
            .is_err()
        );
    }

    #[test]
    fn test_qemu_post_verification_stage_preserves_existing_state() {
        let batch = vec![json!({
            "type": "Concurrency Hazard",
            "description": "Candidate deadlock in foo()",
            "estimated_severity": "High",
        })];
        let stage = post_verification_stage("post-verification-1", batch, 10, 0.0);
        let mut state = QemuPatchReviewState {
            findings: vec![json!({
                "problem": "virtio: earlier finding",
                "severity": "High",
                "preexisting": false,
            })],
            concerns: vec![json!({
                "type": "Pre-existing Issue",
                "description": "Old issue",
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
                    "problem": "virtio: new post-verified finding",
                    "severity": "High",
                    "preexisting": false,
                }),
                json!({
                    "problem": "migration: post-verified pre-existing",
                    "severity": "Medium",
                    "severity_explanation": "Old missing subsection guard",
                    "preexisting": true,
                }),
            ],
            dismissed_concerns: vec![json!({
                "description": "Disproved hard case",
                "reasoning": "Caller holds the BQL",
            })],
        };

        (stage.reducer)(&mut state, output);

        assert_eq!(state.findings.len(), 2);
        assert_eq!(state.findings[0]["problem"], "virtio: earlier finding");
        assert_eq!(
            state.findings[1]["problem"],
            "virtio: new post-verified finding"
        );
        assert_eq!(state.concerns.len(), 2);
        assert_eq!(state.concerns[0]["description"], "Old issue");
        assert_eq!(
            state.concerns[1]["description"],
            "migration: post-verified pre-existing"
        );
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
}
