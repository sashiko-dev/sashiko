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

//! Declarative patch review workflow for changes to Sashiko itself.
//!
//! While inspired by `linux_patch_review`, this workflow is tailored to
//! Sashiko's Rust codebase, async/Tokio concurrency model, SQLite persistence
//! layer, declarative LLM stage engine, and GitHub pull request summary format.

use serde_json::{Value, json};
use std::path::PathBuf;

use crate::workflow::{
    ExecutableStage, OutputFormat, ParallelPolicy, PromptTemplate, RecitationPolicy, Stage,
    StagePolicy, ToolScope, Workflow,
};
use crate::workflows::guard::{normalize_stage_name, sanitize_guide_name};
use crate::workflows::linux_patch_review::{
    AnalysisStage, ConsolidationStage, LinuxPatchReviewState, POST_VERIFICATION_STAGE_NAMES,
    PlanningOutput, PostVerificationOutput, PrescreenOutput, SERIES_CONTEXT_PLACEHOLDER,
    StageConcernsOutput, VerificationOutput, append_stage_dismissed_concerns_with_prompts,
    append_stage_items_with_prompts, batch_hard_cases_by_severity, collect_stage_prompts,
    enrich_post_verification_output, enrich_verification_output, extra_prompt_paths_for_items,
    extra_prompt_paths_for_state, format_post_verification_feedback,
    format_verification_stage_feedback, has_valid_proof_location, record_verified_findings,
    validate_post_verification_batch_output, validate_verification_stage_output,
};

/// State container for a Sashiko patch review run.
pub type SashikoPatchReviewState = LinuxPatchReviewState;

// ---------------------------------------------------------------------------
// System Prompt Template
// ---------------------------------------------------------------------------

pub fn sashiko_system_prompt(use_log: bool) -> PromptTemplate<SashikoPatchReviewState> {
    let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();
    let diff_var = if use_log {
        "{{target_commit_diff}}"
    } else {
        "{{target_commit_diff_only}}"
    };

    PromptTemplate::<SashikoPatchReviewState>::new(format!(
        r#"Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references (e.g., 'today', 'last week', 'next year') strictly on this current date.

You are a principal Rust and distributed systems engineer maintaining Sashiko, an automated AI patch review system. Your goal is to perform a deep, rigorous review of a proposed Sashiko change to ensure memory/concurrency safety, database integrity, prompt/workflow invariants, security against untrusted inputs, and long-term maintainability.

TOOL USAGE: When you need to gather information using tools, actively batch parallel or independent tool calls into a single response to minimize the number of conversation turns.

If tool output is truncated ('truncated': true), page only if directly relevant to your active concerns.

<global_review_guidelines>
The following documents contain the official Sashiko architecture rules, component invariants, and cross-cutting Rust/async guidelines that you MUST adhere to during your review. Use these as the absolute source of truth for identifying anti-patterns and violations.
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
    .with_var("target_commit_sha", |s: &SashikoPatchReviewState| {
        s.target_commit_sha.clone()
    })
    .with_var("baseline_sha", |s: &SashikoPatchReviewState| {
        s.baseline_sha.clone()
    })
    .with_var("target_commit_diff", |s: &SashikoPatchReviewState| {
        s.target_commit_diff.clone()
    })
    .with_var("target_commit_diff_only", |s: &SashikoPatchReviewState| {
        s.target_commit_diff_only.clone()
    })
    .with_var("prefetched_block", |s: &SashikoPatchReviewState| {
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
    .include_file("review-core.md")
    .with_var("custom_prompt_block", |s: &SashikoPatchReviewState| {
        s.custom_prompt
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map_or_else(String::new, |p| {
                format!("\n\n<custom_instructions>\n{p}\n</custom_instructions>")
            })
    })
    .include_files_from_state(|s: &SashikoPatchReviewState| {
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
// Stage Instructions
// ---------------------------------------------------------------------------

const STAGE_GOAL_INSTRUCTION: &str = r#"# Analyze commit main goal, architecture, high-level engineering, and commit message quality

You are a principal engineer evaluating the high-level intent, architectural soundness, engineering necessity, and commit message quality of a proposed Sashiko commit. Enforce Sashiko's strict priority hierarchy: User Experience (UX) > Data Integrity > Security > Everything else.
- High-Level Engineering & Problem/Solution Audit (Mandatory):
  1. Problem Clarity: Is it clear what concrete problem the commit solves? Flag commits where the motivation is vague, circular, or unintelligible.
  2. No Unrelated Changes (Single Responsibility): Does the commit contain unrelated changes, drive-by edits, or mixed concerns? It must NOT — each commit must implement one consistent, self-sufficient change. Flag commits that bundle unrelated changes that should be split into separate commits.
  3. Problem Validity & Worth: Is the problem real and worth solving? Flag over-engineered solutions to hypothetical or non-existent problems, or changes whose complexity outweighs their benefit.
  4. Solution Optimality & Alternatives: Is the chosen solution the best engineering approach, or are there obviously simpler, safer, or more idiomatic alternatives? If a clearly superior alternative exists, raise a concern explaining why.
- Benchmark Backing for Linux Review-Quality Changes (HIGH Severity): Do NOT demand benchmark data, measurements, or manual test procedures in commit messages for ordinary code, CLI, UI, or bug-fix commits where correctness is clear, nor for changes to Sashiko's own self-review prompts (`prompts/sashiko/`, `sashiko_patch_review.rs`), nor for post-triage maintenance tasks (such as periodic upstream fix checks on already-triaged open bugs) or filtering pre-existing issues out of patch review reports that are not exercised by the patch-review or initial bug-discovery benchmark suites (`benchmarks/`). Furthermore, NEVER report that a prompt or workflow commit failed to modify or add files in the `benchmarks/` directory (`benchmarks/*.json` are static ground-truth corpora of known positive bugs and do not store negative/false-positive test cases). However, if a change can meaningfully affect overall Linux AI patch-review or initial bug-discovery quality across the board (such as `third_party/prompts/`, `linux_patch_review.rs`, `linux_bug.rs` discovery/verification/deduplication stages, generic workflow graph structure, model parameters, or shared verification/deduplication rules), its commit message MUST include validation evidence (either benchmark evaluation results or, for targeted false-positive prompt refinements, local re-review results on the affected Linux patches/series). Only if such a Linux review-quality change provides neither benchmark results nor targeted patch re-review results in the commit message, flag it as a High severity issue.
- Unix-Only Target Environment: Sashiko exclusively targets Linux/Unix environments. NEVER report non-Unix or Windows compilation/portability issues (such as `tokio::signal::unix`, `rustix`, `/dev/ptmx`, `libc`, or POSIX signals/paths) as concerns.
- Never Vibe-Guess Build or Compilation Bugs: Build verification (`cargo check`, `cargo test`, `cargo clippy`) is deterministic. NEVER report alleged build failures, syntax errors, missing imports (`use`), unresolved symbols/types/methods/macros, type mismatches, missing trait bounds, borrow-checker/lifetime errors, or Cargo build issues.
- Global UX & Regressions: If the change can affect the user experience globally (CLI ergonomics, review output clarity/false-positive rate, progress display, or web UI/API behavior), apply maximum scrutiny and reject regressions.
- Architectural Boundaries: Check whether the change violates instance isolation, leaks project-specific assumptions into generic engines, or introduces subtle regressions in daemon/worker coordination.
- Commit Message Audit (Mandatory): Inspect the commit message header, body, and trailers in the patch:
  1. Signed-off-by with Real Name: Verify a `Signed-off-by: Real Name <email>` trailer is present and uses a real human name (first and last name), NOT a single-word handle, cryptic nickname, username, or AI/bot placeholder.
  2. Substantive Description (When Non-Trivial): For non-trivial code, architectural, or behavior changes, verify the commit body clearly explains both *what* changed and *why* it is needed (rationale/motivation). Flag missing bodies on non-trivial commits or descriptions that merely parrot a complex diff without explaining why. However, for trivial or self-explanatory commits — such as adding a mailing list to track, adding a subsystem email policy or git remote, simple configuration/deployment updates, or typo/comment fixes — a brief one-sentence description is completely self-sufficient; do NOT demand additional rationale explaining why a mailing list, subsystem, or config entry is being added.
  3. Commit Message Formatting: Flag backticks (`) used to quote code/functions/variables/filenames in the commit message or internal metadata tags (such as `TAG=` or `CONV=`). For line length, do NOT nitpick minor overruns (e.g. 73-80 characters) and allow reasonable exceptions (such as quoting code, compiler/log output, URLs, or file paths); only flag genuinely unwrapped prose lines that exceed ~85 characters."#;

const STAGE_IMPLEMENTATION_INSTRUCTION: &str = r#"# Verify implementation against intent

Verify that the code changes faithfully and completely implement what the commit message and design claim.
- Check for incomplete refactors at runtime boundaries: if a new enum variant, CLI flag, or configuration field is added, verify that wildcard/catch-all match arms (`_ => ...`), subprocess boundaries (`reviewer.rs`, `sashiko-cli`), and serialization paths handle it properly. Do NOT vibe-guess compile-time errors (such as non-exhaustive match arms on closed enums, missing imports, unresolved symbols/types, type mismatches, or borrow-checker errors) — build verification is deterministic.
- Series Context Rule: If follow-up patches in this series are listed in the prompt context, check whether newly introduced types, helpers, schema changes, or configuration fields are wired up in subsequent patches of the series (`Series End Commit`) before flagging them as unused or incomplete.
- Design Document Cross-Check: If this commit adds or updates a design document (`designs/*.md`) or documentation, verify any abbreviated code snippets against the actual Rust implementation in `src/` at the series head (`Series End Commit` / `HEAD`) before reporting an issue.
- Check edge cases: empty inputs, missing optional fields, zero/boundary values, and fallback behavior.
- Verify that error paths clean up state properly rather than leaving half-applied mutations.
- Do not stop after finding several bugs in one function or hunk; systematically audit every modified function, struct/enum, and hunk across the entire diff before concluding.
- Never report Windows or non-Unix portability concerns; Sashiko is strictly a Linux/Unix system."#;

const STAGE_EXECUTION_FLOW_INSTRUCTION: &str = r#"# Trace execution flow and panic safety

Trace the execution paths through every modified function and caller.
- Audit strictly for panic vectors on untrusted or runtime inputs: `.unwrap()`, `.expect()`, direct slice/array indexing (`[i]`), or string slicing (`&s[..n]`) that could land inside a multi-byte UTF-8 character.
- Audit for silently swallowed errors (`let _ = ...`, `.ok()`, `.unwrap_or_default()`) on critical operations such as database status updates, worktree cleanup, or structured LLM output parsing.
- Check numeric casts (`as`) and arithmetic for potential truncation or underflow/overflow.
- Do not stop after finding several bugs in one function or hunk; systematically trace every modified function and error path across the entire diff before concluding.
- Never vibe-guess or report compile-time/build errors (borrow-checker, lifetime/move, type mismatch, unresolved import/symbol, or missing trait bound errors); focus strictly on runtime behavior and panics."#;

const STAGE_CONCURRENCY_INSTRUCTION: &str = r#"# Audit async Tokio discipline and concurrency

Audit all async execution, locking, child process management, and shared state access:
- Check for synchronous blocking I/O (`std::fs`, synchronous `git` CLI commands, heavy CPU loops, `std::thread::sleep`) inside `async fn` on Tokio worker threads without `spawn_blocking`.
- Check for `std::sync::MutexGuard` or `RwLockGuard` held across `.await` points.
- Check child process management (`tokio::process::Command`): verify `.kill_on_drop(true)` when subject to timeouts, and verify stdout/stderr are drained concurrently (`wait_with_output`) to prevent 64 KB pipe buffer deadlocks.
- Check cancellation safety in `tokio::select!` and `timeout` blocks: ensure dropped futures do not leak git worktrees or leave SQLite rows stuck in `in_progress`.
- Check cross-process contention on `sashiko.db` and shared `review_trees/` git repositories.
- Do not stop after finding several bugs in one function or hunk; systematically audit every modified async path, lock scope, and lifecycle transition across the entire diff before concluding."#;

const STAGE_PERSISTENCE_INSTRUCTION: &str = r#"# Audit database schema, queries, and transactions

Data integrity matters second only to UX. Audit all SQLite/libsql operations in `src/db.rs` and schema migrations in `src/migrations/` against two mandatory questions:
1. Will it work with an existing/old database? Verify that schema changes are strictly additive, migrations run atomically inside transactions (`PRAGMA user_version`), and existing rows/queries remain valid on upgrade without data loss.
2. Will it scale? Verify that queries on high-cardinality tables (`patches`, `messages`, `reviews`, `findings`, `ai_interactions`) are backed by indexes, list queries include explicit `LIMIT` clauses, write transactions are kept short (never held across network I/O or LLM calls), and task claims use atomic `UPDATE ... WHERE status = ...` rather than TOCTOU `SELECT` then `UPDATE`.
- Do not stop after finding several bugs in one query or migration; systematically audit every modified query, transaction, and schema change across the entire diff before concluding."#;

const STAGE_LLM_PIPELINE_INSTRUCTION: &str = r#"# Audit LLM workflow engine, stages, and AI providers

Audit changes to `src/workflow/`, `src/workflows/`, `src/worker/prompts.rs`, and `src/ai/`:
- Verify that `Stage` definitions respect `WorkflowEngine` invariants: state mutations must happen strictly inside the `reduce` closure (`StateMutation<S>`), never via side-channel interior mutability during parallel execution.
- Check prompt templates and variable injections (`with_var`, `@include`): ensure included files cannot trigger recursive template expansion or prompt injection.
- Verify JSON output schemas and custom validators (`OutputFormat`): validators must return clear, specific feedback strings so the LLM retry loop can self-correct.
- Check token budget accounting, context truncation safety (UTF-8 boundaries + explicit truncation markers), and transient vs. permanent error classification (`ClassifyAiError`)."#;

const STAGE_SECURITY_INSTRUCTION: &str = r#"# Audit security boundaries and untrusted input handling

Security of Sashiko matters a lot — flag all potential security issues immediately. Sashiko processes untrusted patches, commit messages, email headers, git trees, and webhook payloads:
- Prompt injection: verify untrusted patch/commit content or LLM-selected guide names cannot escape XML/markdown framing or traverse directories (`sanitize_guide_name`).
- Toolbox path traversal and command injection: verify `validate_path` confines all file reads to the worktree root, and verify `git` CLI invocations pass `--` before file paths or refs so user strings cannot be interpreted as git flags (e.g. `--upload-pack` or `--output`).
- Forge and webhook security: verify HMAC signature checks use constant-time comparison before payload processing, and verify `is_safe_repo_url` prevents SSRF or local file cloning.
- API authorization: verify axum routes enforce authentication and capability checks (`[server.acl]`, `read_only` mode)."#;

const STAGE_INTERFACES_COMPAT_INSTRUCTION: &str = r#"# Audit CLI, configuration, email safety, and API compatibility

Audit external interfaces, configuration schemas, email delivery, and cross-process contracts:
- Email Safety (CRITICAL): Be EXTRA careful with any change touching email routing (`src/email_router.rs`), policy (`src/email_policy.rs`), or delivery (`src/worker/email.rs`). Emails sent to public mailing lists are preserved forever and can destroy Sashiko's reputation in a few hours. Flag any risk of widening recipients, bypassing `dry_run` or embargo rules, causing bot reply loops, or sending malformed/duplicate messages.
- Settings (`src/settings.rs`): since `Settings` structs use `#[serde(deny_unknown_fields)]`, verify any new or renamed field has a sensible `#[serde(default)]` and is documented in `docs/examples/Settings.example.toml`.
- Subprocess CLI flags: when the daemon spawns worker subprocesses (`sashiko review` or `sashiko worker`), verify all relevant global flags (`--project`, `--settings`, etc.) are forwarded across the process boundary.
- Series Context Rule: If this commit is part of a multi-patch series, check whether CLI subcommands, HTTP endpoints, or configuration consumers are wired in subsequent patches of the series (`Series End Commit`) before flagging missing interface wiring.
- REST API & UX: verify API response shapes remain backwards-compatible and global user-facing behavior does not regress.
- Target OS: Sashiko runs exclusively on Linux/Unix. Never flag Unix-specific APIs or lack of Windows support."#;

const STAGE_TESTS_INSTRUCTION: &str = r#"# Audit test coverage and determinism

Evaluate the tests accompanying this change (or check whether new tests are required):
- Verify that non-trivial logic, bug fixes, parser edge cases, or database queries include unit or integration tests when appropriate. Do NOT demand tests or manual test descriptions for trivial changes or where existing coverage is sufficient.
- Check test determinism and isolation: tests must not depend on wall-clock timing races, shared hardcoded TCP ports, or mutable global filesystem paths outside `tempfile::TempDir`.
- Standard Unix pseudo-devices (such as `/dev/null` or `/dev/ptmx` for PTY tests) are standard on Linux/Unix and must NOT be flagged as non-Unix portability or filesystem isolation violations.
- Verify that environment variable mutations in tests (`std::env::set_var`) restore previous values or are properly isolated."#;

const STAGE_VERIFICATION_INSTRUCTION: &str = r#"# Verification and severity estimation

You are the lead reviewer consolidating `concerns` and `dismissed_concerns` generated by parallel Sashiko review stages.
Your task is to (1) deduplicate overlapping items across both lists while preserving every distinct failure mechanism and location, and (2) classify every consolidated item into one of two categories:
- **Category 1: Well-Justified** — either a **1a: Well-Justified Concern** (`findings` array) or a **1b: Well-Justified Dismissal** (`dismissed_concerns` array).
- **Category 2: Speculative or Contested** (`hard_cases` array) — routed to parallel per-finding `post-verification` stages for deep tool-assisted code inspection.

### Step 1: Deduplication and Boundary Preservation
1. Group `concerns` and `dismissed_concerns` that refer to the same underlying root cause AND the same function/lifecycle phase.
2. Do NOT merge a setup/initialization bug with a teardown/cleanup bug in a separate function; if an input concern combines setup and teardown bugs across separate functions, split them into separate items. Similarly, do NOT merge distinct races or bugs in separate functions/handlers, or bugs on different resources/fields (e.g., a git worktree leak vs. a database status row leak) even within the same function—either keep them as separate items or explicitly name ALL distinct resources, functions, and failure mechanisms in the item's title/description and explanation fields (`problem` and `severity_explanation` for `findings`, or `description` and `concern_arguments` for `hard_cases`).
3. SPECIFICITY REQUIREMENT: When merging overlapping items, preserve and consolidate the most specific details: exact function names, file paths, line numbers when known, and ALL distinct triggering conditions, callers, consequences, and racing tasks/handlers mentioned across the merged items. When a missing cleanup, swallowed error, or incomplete error-path unwind causes both an immediate failure and a downstream resource/state impact (e.g., leaked git worktree, stuck `in_progress` database row, or dropped stage output), explicitly state BOTH consequences in the item's explanation (`severity_explanation` for `findings`, or `concern_arguments` for `hard_cases`). Preserve and merge the `locations` arrays. Do not invent line numbers; use `null` when unknown.
4. Set `"preexisting": false` whenever the patch introduces, modifies, triggers, exposes, or relies on the buggy code path, caller/callee interaction, or error path (even if the underlying helper or check already existed). Mark `"preexisting": true` ONLY if the problem is in untouched code whose reachability, inputs, and behavior are completely unaffected by this commit/series.

### Step 2: Classification Rules (1: Well-Justified vs. 2: Speculative or Contested)
Classify every consolidated item using these strict signal rules. **Repetition is NOT justification: multiple overlapping items are never enough for Category 1 (1a or 1b) unless their reasoning is backed by specific, concrete code proof.**

1. **Category 1a — Well-Justified Concern (`findings` array):**
   - **Mandatory prerequisite & strong signals:** Concrete, self-contained code proof directly visible in the target diff, prefetched context, and cited `locations`, with **no** competing `dismissed_concern`, **no** reliance on unverified assumptions about unseen code (such as callers, callees, or the rest of an unmodified/partially-shown function or builder chain truncated at the edge of a diff hunk), and **no** potential resolution in subsequent patches of this series (`=== Patch Series Context ===`). Do NOT place suspected build, compilation, syntax, type-checking, borrow-checker, or style/refactoring noise into `findings`; route any such concern to `hard_cases` so `post-verification` can evaluate and discard it. Multiple deduplicated `concerns` with no attempts to dismiss is a strong signal for 1a **only when grounded in concrete code proof** of the complete function/builder chain.
   - **Action:** Validate it directly and emit it in `findings`. Assign a calibrated `severity` (`Critical`, `High`, `Medium`, or `Low`) strictly following `severity.md`, state all consequences (both immediate and downstream), triggering paths, and reachability at the start of `severity_explanation` (preserving every distinct function, handler, resource, and mechanism), formulate a concise bug title (`problem`) under 80 characters starting with a Sashiko component prefix (e.g. `workflow:`, `db:`, `reviewer:`, `toolbox:`, `api:`, `cli:`), set `"preexisting"`, and include `locations`.

2. **Category 1b — Well-Justified Dismissal (`dismissed_concerns` array):**
   - **Mandatory prerequisite & strong signals:** Concrete disproving `code_snippet` in `locations` (showing the exact local guard, lock, bounds check, cleanup path, or lifecycle invariant that prevents the bug), with **no** competing `concern` and **no** reliance on unverified assumptions about external callers, callees, or configurations. Multiple overlapping `dismissed_concerns` for the same code is NOT a signal that the dismissal is safe—it indicates multiple analysts independently found the code suspicious; when multiple stages flag and dismiss the same non-trivial mechanism using assumptions about caller behavior or external state rather than a direct local guard in the same function, classify into `hard_cases` with `"signal_reason": "speculative_dismissal"`.
   - **Disqualifiers for Category 1b (NEVER classify as 1b):**
     - **Single-caller or happy-path-only proofs:** A dismissal that proves an invariant (such as non-empty input, valid UTF-8 boundary, prior authorization, or external serialization) in only one caller or normal happy-path mode without verifying ALL callers and entry points in the tree (`git_grep` across CLI, daemon, worker, HTTP API, and webhook paths) is INVALID.
     - **Asymmetry or cleared-state rationalizations:** A dismissal that rationalizes an unpaired init/claim/cleanup call, a swallowed error, or clearing/overwriting state before downstream consumers read it as a "harmless no-op" or "intentional behavior" without concrete code proving that exact state or omission is safely handled is INVALID.
     - **Indirect state checks or "harmless side-effect" rationalizations:** A dismissal that rationalizes an unintended state mutation, duplicate event emission, or unverified fallback as "harmless" or "benign" without concrete code proof in the downstream consumer is INVALID.
   - **Action:** Place in `dismissed_concerns` (dropped from further verification).
   - **CRITICAL INVARIANT:** Never place any item that was raised as a `concern` into `dismissed_concerns` in this stage. If a raised `concern` is contested by a `dismissed_concern` or appears questionable, it MUST be placed in `hard_cases` for tool-based `post-verification`.

3. **Category 2 — Speculative or Contested (`hard_cases` array — routed to parallel `post-verification`):**
   - **Strong signals:**
     - **Mixed signals (`"mixed_signals"`):** Similar or overlapping `concerns` and `dismissed_concerns` exist for the same root cause, function, or code path.
     - **Speculative or assumption-based dismissal (`"speculative_dismissal"`):** Even when NO stage raised a `concern` (and even when multiple stages emitted overlapping `dismissed_concerns`), inspect every standalone dismissal critically! Multiple overlapping dismissals are NOT enough if they are not justified by specific code. If one or more `dismissed_concerns` identified a plausible bug (such as a missing cleanup on error/cancellation, unlocked shared state access, UTF-8 slice boundary hazard, swallowed error, or missing authorization/validation check) and dismissed it using an assumption not proven by the cited `code_snippet` or a vague argument (for example: assuming an unseen caller validates or cleans up on error, or citing only one caller), you MUST classify the item into `hard_cases` with `"signal_reason": "speculative_dismissal"`.
     - **Speculative or incomplete concern (`"speculative_concern"` or `"insufficient_evidence"`):** One or more overlapping `concerns` whose argument is vague, relies on assumptions not based on specific code in the diff/locations, claims that a method/guard/call (e.g. `.include_file(...)` or `.skip_if(...)`) is missing from a function or builder chain whose full body is not shown in the diff (`"signal_reason": "insufficient_evidence"`), mixes a partially inaccurate premise with a potentially real underlying bug in the same code path, or raises a suspected build/compiler/linter, documentation snippet (`designs/*.md`, `README.md`, `prompts/*.md`), or benchmark/commit-message meta-concern.
     - **Series interaction (`"series_interaction"`):** Any concern that could plausibly be resolved, wired up, or refactored by a subsequent patch listed in `=== Patch Series Context ===`.
   - **Action:** Emit into `hard_cases` with `"estimated_severity"` (`Critical`, `High`, `Medium`, or `Low`), `"signal_reason"`, `"concern_arguments"`, `"dismissal_arguments"`, a concrete `"verification_question"` specifying what code `post-verification` must inspect with tools, `"preexisting"`, and `"locations"`."#;

const STAGE_POST_VERIFICATION_INSTRUCTION: &str = r#"# Per-finding post-verification and conflict resolution

You are the lead reviewer performing deep, tool-assisted codebase verification of a speculative or contested candidate issue (`hard_cases`) identified during initial verification.
For each candidate in `hard_cases`, use the available Git and file tools (`git_read_files`, `git_grep`, `git_diff`, `git_show`, `git_blame`) to answer each candidate's `verification_question` and inspect the actual code in the worktree.
1. Dismiss in `dismissed_concerns` any candidate that alleges a build, compilation, syntax, type-checking, borrow-checker, lifetime, missing-import, unresolved-symbol, missing-trait-bound, or linter error (carry forward the candidate's target `file`, `function_or_symbol`, and `code_snippet` in `locations`).
2. Dismiss in `dismissed_concerns` any candidate claiming that a prompt or workflow commit failed to add or modify test cases in the `benchmarks/` directory, claiming that local patch re-review results in the commit message are insufficient for targeted false-positive prompt refinements in `third_party/prompts/`, or demanding additional commit-message rationale for trivial or self-explanatory commits (such as adding a mailing list to track, configuring an email policy or git remote, simple deployment/config updates, or typo fixes) where the intent is already clear (in `locations`, carry forward the candidate's target location or quote the relevant commit message / diff line with `"file"` and `"function_or_symbol"` set to the target path/symbol or `"commit_message"`).
3. **SYMMETRICAL PROOF BAR & ALL-CALLERS VERIFICATION:** For all other candidates, both `concern_arguments` and `dismissal_arguments` are untrusted hypotheses. Only dismiss a code-behavior candidate in `dismissed_concerns` if concrete, verifiable code in the repository proves the exact failure mechanism, branch, caller, and configuration cannot occur across ALL callers, entry points, and modes. Citing a single caller (such as only the CLI path or one HTTP handler) does NOT disprove a panic, race, missing validation, or state leak in a helper function unless `git_grep` across all callers (including CLI, daemon, worker, HTTP API, and webhook paths) proves every caller upholds the invariant. Do NOT give code the benefit of the doubt.
4. **LOCAL BOUNDARY & ASYMMETRY RULE:** Do not discard a defect within the modified code of the patch by assuming that surrounding caller systems or parallel execution will safely mask or prevent the issue, or by rationalizing an unpaired claim/cleanup call, swallowed error, or overwritten state as a "harmless no-op" or "intentional behavior", unless you can point to specific code in the repository that concretely proves the failure mode is structurally impossible.
5. **PROMOTING SPECULATIVE DISMISSALS:** When `"signal_reason"` is `"speculative_dismissal"`, a previous analyst spotted the candidate bug described in `concern_arguments` and dismissed it using `dismissal_arguments`. Inspect the actual code with tools: if the dismissal's assumption is false or incomplete (for example: another caller does NOT uphold the invariant; the caller does NOT clean up the worktree or database state on error or cancellation; the cited lock or transaction does NOT serialize against concurrent access; an init/claim call lacks its matching cleanup/release call; or an unintended state mutation affects downstream consumers), you MUST report the bug as a verified finding in `findings`.
6. **REFINING PARTIALLY INACCURATE PREMISES:** If a candidate concern contains a partially inaccurate premise while also identifying a real bug in the same code path, refine and report the valid underlying bug rather than discarding the entire candidate.
7. **SERIES VALIDATION RULE:** If other patches in this series are provided in the context, check whether each candidate is resolved, wired up, or refactored in the final state of the series (`Series End Commit`) using tools (`git_read_files` or `git_diff` with `revision` / `target_revision` set to the `Series End Commit`); do not trust promises in commit messages alone. If resolved by the end of the series, dismiss it in `dismissed_concerns` citing the resolving commit. When referring to other patches within this series in your explanation, DO NOT use ephemeral git hashes; refer to them by their patch subject (e.g., 'commit "auth: add max_bug_access claim"').
8. **DESIGN & DOCUMENTATION RULE:** If a candidate targets illustrative pseudo-code or abbreviated struct snippets in documentation (`designs/*.md`, `README.md`, `prompts/*.md`), inspect the actual Rust implementation in `src/` at the series head (`Series End Commit` / `HEAD`). If the actual Rust code properly enforces the invariant, dismiss the documentation concern in `dismissed_concerns` citing the enforcing Rust code.
9. **SEVERITY CALIBRATION AND COMPLETENESS:** Set `"preexisting": false` whenever the patch introduces, modifies, triggers, exposes, or relies on the buggy code path, caller/callee interaction, or error path; mark `"preexisting": true` ONLY if the problem is in untouched code whose reachability, inputs, and behavior are completely unaffected by this commit/series. For each verified issue, assign an accurate severity (`Critical`, `High`, `Medium`, or `Low`) strictly following `severity.md`: reason through all consequences (both immediate failures and downstream state/resource leaks), triggering paths, and reachability, state that reasoning at the start of `severity_explanation`, preserve all distinct function names, file paths, line numbers when known, triggering entry points, and consequences, and formulate a concise bug title (`problem`) under 80 characters starting with a Sashiko component prefix (e.g. `workflow:`, `db:`, `reviewer:`, `toolbox:`, `api:`, `cli:`)."#;

const STAGE_REPORT_INSTRUCTION: &str = r#"# Generate plain-text inline review report

Generate the plain-text inline review report following the exact formatting rules and structure in `github-summary-template.md`.
- Output ONLY a plain bulleted list of ALL findings ordered from highest severity to lowest (`- [CRITICAL] ...`, `- [HIGH] ...`, `- [MEDIUM] ...`, `- [LOW] ...`), or `No issues found.` if there are no findings.
- If any finding has `"preexisting": true`, state explicitly in its explanation that the issue was not introduced by this change.
- Do NOT include `Summary:` or `Findings:` headers (the summary is generated and displayed separately in the UI).
- Do NOT use backticks, markdown code blocks, or markdown headings. Wrap all lines at 78 characters or fewer."#;

const STAGE_SUMMARY_INSTRUCTION: &str = r#"# Summarize the proposed change

Provide a concise plain-text summary explaining what this commit/change does and why.
- Start with 1-2 sentences describing the core change and its rationale.
- User-Visible Effect Rule: If this commit has any user-visible effect (for example: adding or changing a cmdline option/subcommand/output, adding or changing a configuration setting, changing the format of generated emails or GitHub PR comments, changing REST API responses, or altering Web UI display), you MUST explicitly describe that user-visible effect and provide a concrete 'Before:' and 'After:' example showing how it looked or worked before vs. how it will look or work for a user after this change:

Before:
  <how it was or looked for a user before>

After:
  <how it will look or work for a user after>

- If the change is strictly internal with no user-visible effect (e.g. internal refactoring, unit test additions, or internal prompt tuning without output format changes), omit the 'Before:' / 'After:' section and keep the summary to 1-2 sentences.
- Use strictly plain text: no markdown, no backticks, no markdown headings ('#'), and no bullet points.
- Wrap prose lines at 78 characters or fewer (keep indented 'Before:' and 'After:' example lines concise as well, though long CLI commands, URLs, or output lines may exceed 78 characters when necessary).
- Summarize the change itself (do not list review findings or issues here)."#;

const CONCERN_JSON_SCHEMA_EXAMPLE: &str = r#"Return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations".
In each 'dismissed_concerns' object, "description" is the candidate concern that was investigated and disproved, "reasoning" is the step-by-step explanation of why it is not a bug (citing the exact guard, caller, or invariant), and "locations" MUST cite the concrete disproving code (file, function_or_symbol, line, verbatim code_snippet, and why_this_location_matters).
Use the 'dismissed_concerns' array ONLY for candidate concerns that you considered plausible, investigated, and disproved with concrete evidence. This is especially important when you first suspect a concern and then follow the evidence chain proving that it does NOT apply.

NO DISMISSAL WITHOUT VERIFIED PROOF: To place a candidate issue in 'dismissed_concerns' (or to discard a suspected issue), you MUST find concrete proof in the code ('file', 'function_or_symbol', 'line', and verbatim 'code_snippet' in 'locations') that explicitly invalidates the concern's reasoning. If the disproving code lives outside the diff (for example, in a caller, callee, helper, or configuration), you MUST verify that code first using tools ('git_read_files' or 'git_grep') and quote the verified disproving snippet in 'locations'. If you cannot find definitive code proof that the candidate issue is impossible, you MUST report it in 'concerns' (NOT 'dismissed_concerns') and make the condition explicit: if X is possible, then problem Y can occur.
- Citing a single caller (such as only the CLI path or one HTTP handler) does NOT disprove a panic, race, missing validation, or missing state transition in a helper function. A caller-based dismissal is valid ONLY if every caller in the tree ('git_grep' across all callers, including CLI, daemon, worker, HTTP API, and webhook paths) is verified to uphold the invariant; otherwise report it in 'concerns'.
- Never dismiss an unpaired lifecycle/state transition (e.g., claiming a task, worktree, or outbox row without a matching release/cleanup/terminal status on error or cancellation), a swallowed error, or clearing/overwriting state before downstream consumers read it by rationalizing that the side effect is a "harmless no-op" or "rare edge case".

SPECIFICITY REQUIREMENT: When reporting a concern or dismissed_concern, cite exact function name(s), file path(s), and line number(s) when known. Do not invent line numbers; use null when exact values are unknown.

Example Output:
```json
{
  "concerns": [
    {
      "type": "Concurrency Hazard",
      "description": "std::sync::MutexGuard held across .await in Worker::run",
      "reasoning": "1. lock() is acquired on line 42.\n2. async_call().await is invoked on line 45 while guard is still in scope.",
      "preexisting": false,
      "locations": [
        {
          "file": "src/worker/prompts.rs",
          "function_or_symbol": "Worker::run",
          "line": 45,
          "code_snippet": "let res = provider.call().await;",
          "why_this_location_matters": "Yielding to Tokio runtime while holding a synchronous MutexGuard can deadlock worker threads."
        }
      ]
    }
  ],
  "dismissed_concerns": [
    {
      "type": "Error Handling",
      "description": "Potential UTF-8 slice panic in format_subject",
      "reasoning": "Verified that char_indices() is used on line 88 to find a valid char boundary before slicing.",
      "locations": [
        {
          "file": "src/bin/sashiko-cli.rs",
          "function_or_symbol": "format_subject",
          "line": 88,
          "code_snippet": "let cutoff = s.char_indices().nth(max_len)...",
          "why_this_location_matters": "Proves slice index is always on a UTF-8 character boundary."
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
        guides: &["patterns/error-handling.md"],
        uses_commit_log: false,
        optional: false,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "concurrency",
        short: "Concurrency & Async",
        instruction: STAGE_CONCURRENCY_INSTRUCTION,
        guides: &["patterns/rust-async.md", "patterns/concurrency.md"],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "persistence",
        short: "DB & Persistence",
        instruction: STAGE_PERSISTENCE_INSTRUCTION,
        guides: &["subsystem/db-migrations.md"],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "llm-pipeline",
        short: "LLM Pipeline",
        instruction: STAGE_LLM_PIPELINE_INSTRUCTION,
        guides: &[
            "subsystem/workflow-engine.md",
            "subsystem/llm-stages.md",
            "subsystem/ai-providers.md",
        ],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "security",
        short: "Security Audit",
        instruction: STAGE_SECURITY_INSTRUCTION,
        guides: &[
            "prompt-injection.md",
            "subsystem/toolbox.md",
            "subsystem/api-auth.md",
            "subsystem/forge.md",
        ],
        uses_commit_log: false,
        optional: true,
        wants_series_context: false,
    },
    AnalysisStage {
        name: "interfaces-compat",
        short: "Interfaces & Compat",
        instruction: STAGE_INTERFACES_COMPAT_INSTRUCTION,
        guides: &["subsystem/settings.md", "subsystem/email-policy.md"],
        uses_commit_log: true,
        optional: true,
        wants_series_context: true,
    },
    AnalysisStage {
        name: "tests",
        short: "Test Audit",
        instruction: STAGE_TESTS_INSTRUCTION,
        guides: &[],
        uses_commit_log: true,
        optional: true,
        wants_series_context: true,
    },
];

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

pub static SUMMARY: ConsolidationStage = ConsolidationStage {
    name: "summary",
    short: "Change Summary",
    wants_series_context: false,
};

pub static CONSOLIDATION_STAGES: &[&ConsolidationStage] =
    &[&VERIFICATION, &POST_VERIFICATION, &REPORT, &SUMMARY];

fn series_context_placeholder(wants: bool) -> &'static str {
    if wants {
        SERIES_CONTEXT_PLACEHOLDER
    } else {
        ""
    }
}

fn with_series_context(
    template: PromptTemplate<SashikoPatchReviewState>,
    wants: bool,
) -> PromptTemplate<SashikoPatchReviewState> {
    if !wants {
        return template;
    }
    template.with_var("follow_up_series_section", |s: &SashikoPatchReviewState| {
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
    _state: &SashikoPatchReviewState,
) -> Result<(), String> {
    for (idx, concern) in output.concerns.iter().enumerate() {
        let Some(obj) = concern.as_object() else {
            return Err(format!("concerns[{idx}] must be a JSON object."));
        };
        let has_desc = obj
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_desc {
            return Err(format!(
                "concerns[{idx}] must have a non-empty 'description' string."
            ));
        }
        let has_reasoning = obj
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_reasoning {
            return Err(format!(
                "concerns[{idx}] must have a non-empty 'reasoning' string."
            ));
        }
        if !obj.get("preexisting").is_some_and(Value::is_boolean) {
            return Err(format!(
                "concerns[{idx}] must have a boolean 'preexisting' field (true or false)."
            ));
        }
        if !obj.get("locations").is_some_and(Value::is_array) {
            return Err(format!("concerns[{idx}] must have a 'locations' array."));
        }
    }

    for (idx, dismissed) in output.dismissed_concerns.iter().enumerate() {
        let Some(obj) = dismissed.as_object() else {
            return Err(format!("dismissed_concerns[{idx}] must be a JSON object."));
        };
        let has_desc = obj
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_desc {
            return Err(format!(
                "dismissed_concerns[{idx}] must have a non-empty 'description' string."
            ));
        }
        let has_reasoning = obj
            .get("reasoning")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty());
        if !has_reasoning {
            return Err(format!(
                "dismissed_concerns[{idx}] must have a non-empty 'reasoning' string."
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
        "\n\nPrevious attempt was rejected: {}. You MUST return ONLY a JSON object containing 'concerns' (each with 'type', non-empty 'description' and 'reasoning' strings, a boolean 'preexisting', and a 'locations' array) and 'dismissed_concerns' (each with 'type', non-empty 'description' and 'reasoning' strings, and a 'locations' array) arrays. If there are no concerns and no dismissed concerns, return `{{\"concerns\": [], \"dismissed_concerns\": []}}`.",
        violation
    )
}

fn validate_github_summary_format(
    content: &str,
    state: &SashikoPatchReviewState,
) -> Result<(), String> {
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
                "Do not use markdown headings ('#') in the plain-text review report. Follow github-summary-template.md."
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

pub fn format_sashiko_inline_findings(text: &str) -> String {
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

fn format_github_summary_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. Follow `github-summary-template.md`: return strictly plain text without backticks, markdown headings, or 'Summary:'/'Findings:' headers; wrap prose lines at 78 characters; separate individual findings with an empty line; and list every finding as a bullet starting with '- [<SEVERITY>]'.",
        violation
    )
}

fn validate_summary_format(content: &str, _state: &SashikoPatchReviewState) -> Result<(), String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err("The change summary cannot be empty.".to_string());
    }
    if trimmed.contains('`') {
        return Err(
            "The summary contains backticks ('`'). Use strictly plain text without backticks."
                .to_string(),
        );
    }
    for line in trimmed.lines() {
        let l = line.trim_start();
        let is_indented = line.starts_with("  ") || line.starts_with('\t');
        if !is_indented {
            if l.starts_with('#') || l.starts_with("Summary:") {
                return Err(
                    "Do not use markdown headings ('#') or 'Summary:' prefixes in the summary."
                        .to_string(),
                );
            }
            if line.chars().count() > 84 {
                return Err(format!(
                    "Line exceeds 78-character terminal width ({} chars): \"{}...\". Wrap prose lines at 78 characters.",
                    line.chars().count(),
                    line.chars().take(40).collect::<String>()
                ));
            }
        }
    }
    Ok(())
}

fn format_summary_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. Provide a plain-text summary (including Before: and After: examples if the change has a user-visible effect) without backticks, markdown headings, or 'Summary:' prefixes, wrapped at 78 characters.",
        violation
    )
}

// ---------------------------------------------------------------------------
// Stage Builders
// ---------------------------------------------------------------------------

pub fn prescreen_stage() -> Stage<SashikoPatchReviewState, PrescreenOutput> {
    Stage::builder("pre-screen")
        .system_prompt(PromptTemplate::<SashikoPatchReviewState>::new(
            "You are an AI assistant preparing a Sashiko codebase patch review.\nReview the provided Patch and select all potentially relevant component and pattern guides from the index below.\nCRITICAL BIAS RULE: You MUST err on the side of inclusion. Only exclude a guide if it is 100% irrelevant to the modified code. If there is any doubt, include the file.\n\nYou MUST respond with ONLY a JSON object, no other text. Example:\n```json\n{\"selected_prompts\": [\"workflow-engine.md\", \"rust-async.md\"]}\n```",
        ))
        .user_prompt(
            PromptTemplate::<SashikoPatchReviewState>::new(
                "<subsystem_guide_index>\n@include(\"subsystem/subsystem.md\")\n</subsystem_guide_index>\n\n<patch>\n{{target_commit_diff}}\n</patch>",
            )
            .with_var("target_commit_diff", |s: &SashikoPatchReviewState| {
                s.target_commit_diff.clone()
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
            "required": ["selected_prompts"],
            "additionalProperties": false
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
                .filter(|name| sanitize_guide_name(name))
                .collect();
            state.selected_guides = prompts;
        })
        .build()
}

pub fn planning_stage() -> Stage<SashikoPatchReviewState, PlanningOutput> {
    let optional_stages: Vec<&'static str> = ANALYSIS_STAGES
        .iter()
        .filter(|s| s.optional)
        .map(|s| s.name)
        .collect();
    let optional_list = optional_stages.join(", ");

    Stage::builder("planning")
        .system_prompt(PromptTemplate::<SashikoPatchReviewState>::new(format!(
            "You are an AI assistant planning a Sashiko patch review.\nThe core stages (goal, implementation, execution-flow) always run.\nSelect which optional specialized stages should also run based on the patch contents.\nAvailable optional stages: [{optional_list}]\n\nCRITICAL BIAS RULE: Err on the side of inclusion. Include any stage whose domain could plausibly be affected by the patch.\n\nRespond with ONLY a JSON object listing the relevant optional stages. Example:\n```json\n{{\"relevant_stages\": [\"concurrency\", \"persistence\"]}}\n```"
        )))
        .user_prompt(
            PromptTemplate::<SashikoPatchReviewState>::new(
                "<patch>\n{{target_commit_diff}}\n</patch>",
            )
            .with_var("target_commit_diff", |s: &SashikoPatchReviewState| {
                s.target_commit_diff.clone()
            }),
        )
        .output_format(OutputFormat::json_with_schema(json!({
            "type": "object",
            "properties": {
                "relevant_stages": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            },
            "required": ["relevant_stages"],
            "additionalProperties": false
        })))
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns: 1,
            ..Default::default()
        })
        .skip_if(|s| s.manual_stages.is_some())
        .reduce(|state, out: PlanningOutput| {
            let mut planned: Vec<String> = ANALYSIS_STAGES
                .iter()
                .filter(|s| !s.optional)
                .map(|s| s.name.to_string())
                .collect();

            for raw in out.relevant_stages {
                if let Some(def) = analysis_stage_by_name(&raw)
                    && !planned.iter().any(|p| p == def.name)
                {
                    planned.push(def.name.to_string());
                }
            }
            state.planned_stages = planned;
        })
        .build()
}

fn analysis_stage(
    def: &'static AnalysisStage,
    max_turns: usize,
    temperature: f32,
) -> Box<dyn ExecutableStage<SashikoPatchReviewState>> {
    let series_context = series_context_placeholder(def.wants_series_context);
    let mut user_template = PromptTemplate::<SashikoPatchReviewState>::new(format!(
        "{}{}\n\n{}",
        def.instruction, series_context, CONCERN_JSON_SCHEMA_EXAMPLE
    ));

    for guide in def.guides {
        user_template = user_template.include_file(*guide);
    }

    let user_template = with_series_context(user_template, def.wants_series_context);

    Box::new(
        Stage::builder(def.name)
            .system_prompt(sashiko_system_prompt(def.uses_commit_log))
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
                move |state: &mut SashikoPatchReviewState,
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
    state: &SashikoPatchReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<SashikoPatchReviewState>>> {
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
            None => tracing::warn!("Ignoring unknown Sashiko review stage {:?}", name),
        }
    }
    stages
}

pub fn verification_stage(
    max_turns: usize,
    temperature: f32,
) -> Stage<SashikoPatchReviewState, VerificationOutput> {
    let series_context = series_context_placeholder(VERIFICATION.wants_series_context);
    let user_template = with_series_context(
        PromptTemplate::<SashikoPatchReviewState>::new(format!(
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
- Each object in 'findings' (Category 1a: Well-Justified Concerns) MUST use the keys: "problem" (a short naming string under 80 characters starting with a Sashiko component prefix like 'workflow:', 'db:', 'reviewer:', 'toolbox:', 'api:', 'cli:', NEVER using backquotes), "severity" ("Low", "Medium", "High", "Critical", or "Unknown"), "severity_explanation" (detailed reasoning and proof), "preexisting" (boolean), and "locations" (array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters), and may include "stages" (array of stage names) and "prompts" (array of prompt files).
- Each object in 'hard_cases' (Category 2: Speculative or Contested) MUST use the keys: "type", "description", "estimated_severity" ("Low", "Medium", "High", "Critical", or "Unknown"), "signal_reason" ("mixed_signals", "speculative_concern", "speculative_dismissal", "series_interaction", or "other"), "concern_arguments" (consolidated arguments for why the bug can occur), "dismissal_arguments" (consolidated arguments/snippets from any competing or standalone dismissal, or "" if none), "verification_question" (the specific code question post-verification must answer with tools), "preexisting" (boolean), and "locations" (array of location objects), and may include "stages" and "prompts".
- Each object in 'dismissed_concerns' (Category 1b: Well-Justified Dismissals) MUST use the keys: "type", "description", "reasoning", and "locations", and may include "stages" and "prompts"."#
        ))
        .include_file("false-positive-guide.md")
        .include_file("severity.md")
        .include_files_from_state(|s: &SashikoPatchReviewState| {
            extra_prompt_paths_for_state(s, analysis_stage_by_name)
        }),
        VERIFICATION.wants_series_context,
    )
    .with_var("aggregated_concerns", |s: &SashikoPatchReviewState| {
        serde_json::to_string_pretty(&s.all_concerns).unwrap_or_default()
    })
    .with_var(
        "aggregated_dismissed_concerns",
        |s: &SashikoPatchReviewState| {
            serde_json::to_string_pretty(&s.all_dismissed_concerns).unwrap_or_default()
        },
    );

    Stage::builder(VERIFICATION.name)
        .system_prompt(sashiko_system_prompt(true))
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
        .skip_if(|s| s.all_concerns.is_empty() && s.all_dismissed_concerns.is_empty())
        .reduce_with_outcome(|state, mut out: VerificationOutput, outcome| {
            enrich_verification_output(state, &mut out, outcome, analysis_stage_by_name);
            record_verified_findings(state, out.findings);
            state.hard_cases = out.hard_cases;
            state
                .deduplicated_dismissed_concerns
                .extend(out.dismissed_concerns);
        })
        .build()
}

pub fn post_verification_stage(
    stage_name: &'static str,
    batch: Vec<Value>,
    max_turns: usize,
    temperature: f32,
) -> Stage<SashikoPatchReviewState, PostVerificationOutput> {
    let expected_items = batch.len().max(1);
    let candidate_json = serde_json::to_string_pretty(&batch).unwrap_or_default();
    let batch_for_prompts = batch.clone();
    let batch_for_reduce = batch;
    let series_context = series_context_placeholder(POST_VERIFICATION.wants_series_context);
    let user_template = with_series_context(
        PromptTemplate::<SashikoPatchReviewState>::new(format!(
            r#"{STAGE_POST_VERIFICATION_INSTRUCTION}

<false_positive_guide>
@include("false-positive-guide.md")
</false_positive_guide>

<severity_guidelines>
@include("severity.md")
</severity_guidelines>@includes

CRITICAL REVIEW DIRECTIVE: To dismiss a code-behavior candidate issue as a false positive, you must find concrete evidence in the code that proves the issue is invalid and quote that disproving code in `dismissed_concerns[].locations` (for policy-excluded build/linter or out-of-scope meta-concerns under rules 1-2, carry forward the candidate's target location or commit message snippet in `locations`). If you cannot find concrete proof of safety, you must validate and report the finding in `findings`.{series_context}

Candidate Hard Case(s) to Verify:
{{{{candidate_hard_cases}}}}

Return ONLY a JSON object with 'findings' and 'dismissed_concerns' arrays. Every candidate in this batch MUST be accounted for in either 'findings' (if validated) or 'dismissed_concerns' (if disproved by concrete code or excluded by rules 1-2; never return both empty arrays).
- Each object in 'findings' MUST use: "problem" (a short naming string under 80 characters starting with a Sashiko component prefix like 'workflow:', 'db:', 'reviewer:', 'toolbox:', 'api:', 'cli:', NEVER using backquotes), "severity" (Low, Medium, High, Critical, or Unknown), "severity_explanation" (detailed reasoning and proof), "preexisting" (boolean), "locations" (array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters).
- Each object in 'dismissed_concerns' MUST use: "description" (the candidate issue that was disproved), "reasoning" (step-by-step explanation of how the inspected code or policy rule disproves the candidate), and "locations" (a non-empty array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters, quoting the verbatim disproving code or target snippet)."#
        ))
        .include_file("false-positive-guide.md")
        .include_file("severity.md")
        .include_files_from_state(move |s: &SashikoPatchReviewState| {
            extra_prompt_paths_for_items(&s.selected_guides, &batch_for_prompts, analysis_stage_by_name)
        }),
        POST_VERIFICATION.wants_series_context,
    )
    .with_var("candidate_hard_cases", move |_: &SashikoPatchReviewState| {
        candidate_json.clone()
    });

    Stage::builder(stage_name)
        .system_prompt(sashiko_system_prompt(true))
        .user_prompt(user_template)
        .output_format(
            OutputFormat::json()
                .with_validator(
                    move |out: &PostVerificationOutput, _: &SashikoPatchReviewState| {
                        validate_post_verification_batch_output(out, expected_items)
                    },
                )
                .with_feedback_formatter(format_post_verification_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce_with_outcome(move |state, mut out: PostVerificationOutput, outcome| {
            enrich_post_verification_output(
                &state.selected_guides,
                &batch_for_reduce,
                &mut out,
                outcome,
                analysis_stage_by_name,
            );
            record_verified_findings(state, out.findings);
            state
                .deduplicated_dismissed_concerns
                .extend(out.dismissed_concerns);
        })
        .build()
}

pub fn post_verification_stage_for_batch(
    stage_name: &'static str,
    batch: Vec<Value>,
    max_turns: usize,
    temperature: f32,
) -> Box<dyn ExecutableStage<SashikoPatchReviewState>> {
    Box::new(post_verification_stage(
        stage_name,
        batch,
        max_turns,
        temperature,
    ))
}

pub fn resolve_post_verification_stages_with_options(
    state: &SashikoPatchReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<SashikoPatchReviewState>>> {
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

pub fn report_stage(max_turns: usize, temperature: f32) -> Stage<SashikoPatchReviewState, String> {
    Stage::builder(REPORT.name)
        .system_prompt(sashiko_system_prompt(true))
        .user_prompt(
            PromptTemplate::<SashikoPatchReviewState>::new(format!(
                r#"{STAGE_REPORT_INSTRUCTION}

<report_template>
@include("github-summary-template.md")
</report_template>

Findings:
{{{{findings}}}}

Return strictly plain text output (no markdown, no backticks, wrapped at 78 characters), not JSON."#
            ))
            .include_file("github-summary-template.md")
            .with_var("findings", |s: &SashikoPatchReviewState| {
                serde_json::to_string_pretty(&s.findings).unwrap_or_default()
            }),
        )
        .output_format(OutputFormat::text_with_validator(
            validate_github_summary_format,
            format_github_summary_feedback,
        ))
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            recitation_policy: RecitationPolicy::FallbackToFreeForm {
                reminder: "Do not quote large blocks of code verbatim. Summarize concisely."
                    .to_string(),
            },
            ..Default::default()
        })
        .skip_if(|s| s.skip_report || s.findings.is_empty())
        .reduce(|state, out: String| {
            state.review_inline = format_sashiko_inline_findings(&out);
        })
        .build()
}

pub fn summary_stage(
    _max_turns: usize,
    temperature: f32,
) -> Stage<SashikoPatchReviewState, String> {
    Stage::builder(SUMMARY.name)
        .system_prompt(sashiko_system_prompt(true))
        .user_prompt(PromptTemplate::<SashikoPatchReviewState>::new(
            STAGE_SUMMARY_INSTRUCTION,
        ))
        .output_format(OutputFormat::text_with_validator(
            validate_summary_format,
            format_summary_feedback,
        ))
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns: 1,
            temperature,
            recitation_policy: RecitationPolicy::FallbackToFreeForm {
                reminder:
                    "Summarize the change concisely in plain text (including Before: and After: examples if user-visible) without quoting large blocks verbatim."
                        .to_string(),
            },
            ..Default::default()
        })
        .skip_if(|s| s.skip_report)
        .reduce(|state, out: String| {
            state.summary = out.trim().to_string();
        })
        .build()
}

// ---------------------------------------------------------------------------
// Complete Sashiko Review Workflow Graph
// ---------------------------------------------------------------------------

pub fn build_sashiko_patch_review_workflow() -> Workflow<SashikoPatchReviewState> {
    build_sashiko_patch_review_workflow_with_options(20, 0.0)
}

/// Builds the Sashiko patch review workflow.
///
/// Unlike [`crate::workflows::linux_patch_review::build_linux_patch_review_workflow_with_options`],
/// this workflow ends with an unconditional [`summary_stage`] that generates a
/// 1-2 sentence commit summary even when zero findings are produced. Therefore,
/// short-circuiting uses `skip_if` on [`verification_stage`] (when both
/// `all_concerns` and `all_dismissed_concerns` are empty), 0-stage fan-out in
/// [`resolve_post_verification_stages_with_options`] (when `hard_cases` is
/// empty), and `skip_if` on [`report_stage`] (when `findings` is empty) rather
/// than `early_exit_if`, which would abort the workflow before `summary_stage`.
pub fn build_sashiko_patch_review_workflow_with_options(
    max_turns: usize,
    temperature: f32,
) -> Workflow<SashikoPatchReviewState> {
    Workflow::builder("sashiko_patch_review")
        .stage(prescreen_stage())
        .dynamic_parallel(
            planning_stage(),
            move |state| resolve_analysis_stages_with_options(state, max_turns, temperature),
            ParallelPolicy::BestEffort,
        )
        .dynamic_parallel(
            verification_stage(max_turns, temperature),
            move |state| {
                resolve_post_verification_stages_with_options(state, max_turns, temperature)
            },
            ParallelPolicy::BestEffort,
        )
        .stage(report_stage(max_turns, temperature))
        .stage(summary_stage(max_turns, temperature))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sashiko_analysis_stages_table() {
        assert_eq!(ANALYSIS_STAGES.len(), 9);
        let names: Vec<&str> = ANALYSIS_STAGES.iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![
                "goal",
                "implementation",
                "execution-flow",
                "concurrency",
                "persistence",
                "llm-pipeline",
                "security",
                "interfaces-compat",
                "tests",
            ]
        );
        // Hardware stage must not be present in Sashiko review workflow
        assert!(!names.contains(&"hardware"));

        // Core always-on stages
        assert!(!analysis_stage_by_name("goal").unwrap().optional);
        assert!(!analysis_stage_by_name("implementation").unwrap().optional);
        assert!(!analysis_stage_by_name("execution-flow").unwrap().optional);

        // Specialized planner-gated stages
        assert!(analysis_stage_by_name("concurrency").unwrap().optional);
        assert!(analysis_stage_by_name("persistence").unwrap().optional);
        assert!(analysis_stage_by_name("llm-pipeline").unwrap().optional);
        assert!(analysis_stage_by_name("security").unwrap().optional);
        assert!(
            analysis_stage_by_name("interfaces-compat")
                .unwrap()
                .optional
        );
        assert!(analysis_stage_by_name("tests").unwrap().optional);
    }

    #[test]
    fn test_sashiko_stage_lookup_and_labels() {
        assert_eq!(
            stage_short_label("Stage_LLM_Pipeline"),
            Some("LLM Pipeline")
        );
        assert_eq!(
            stage_short_label("interfaces_compat"),
            Some("Interfaces & Compat")
        );
        assert_eq!(
            stage_short_label("post-verification"),
            Some("Post-Verification")
        );
        assert_eq!(
            stage_short_label("post-verification-3"),
            Some("Post-Verification")
        );
        assert_eq!(stage_short_label("report"), Some("Report Generation"));
        assert_eq!(stage_short_label("summary"), Some("Change Summary"));
        assert!(is_known_stage("pre-screen"));
        assert!(is_known_stage("planning"));
        assert!(is_known_stage("persistence"));
        assert!(is_known_stage("verification"));
        assert!(is_known_stage("post-verification"));
        assert!(is_known_stage("post-verification-1"));
        assert!(is_known_stage("summary"));
        assert!(!is_known_stage("hardware"));
    }

    #[test]
    fn test_sashiko_github_summary_validator() {
        let mut state = SashikoPatchReviewState::default();
        assert!(validate_github_summary_format("No issues found.", &state).is_ok());
        assert!(validate_github_summary_format("   ", &state).is_err());
        assert!(validate_github_summary_format("Adds `backticks` here.", &state).is_err());
        assert!(
            validate_github_summary_format("# Top-level heading\nNo issues found.", &state)
                .is_err()
        );
        assert!(
            validate_github_summary_format("### Third-level heading\nNo issues found.", &state)
                .is_err()
        );
        assert!(
            validate_github_summary_format(
                "Summary: Adds project support.\n\nFindings:\n- [HIGH] Test issue.",
                &state
            )
            .is_err()
        );
        let long_line = "a".repeat(90);
        assert!(validate_github_summary_format(&long_line, &state).is_err());

        state
            .findings
            .push(json!({"severity": "High", "problem": "Test issue"}));
        assert!(
            validate_github_summary_format("1 finding: highest severity High.", &state).is_err()
        );
        assert!(
            validate_github_summary_format(
                "- [HIGH] In src/main.rs (main), missing error check allows invalid state.",
                &state
            )
            .is_ok()
        );

        assert!(
            validate_summary_format(
                "Adds support for separate patch summary generation and UI display.",
                &state
            )
            .is_ok()
        );
        assert!(
            validate_summary_format(
                "Adds fix_check_enabled setting under [linux_bug] to toggle periodic fix\nchecks independently of the bugs database.\n\nBefore:\n  Summary: old format\n  [linux_bug]\n  enabled = true\n\nAfter:\n  [linux_bug]\n  enabled = true\n  # Periodic fix checks are now configured separately:\n  fix_check_enabled = false\n  sashiko review --project sashiko --agent --settings /etc/sashiko/Settings.toml HEAD~1..HEAD",
                &state
            )
            .is_ok()
        );
        assert!(STAGE_SUMMARY_INSTRUCTION.contains("Before:"));
        assert!(STAGE_SUMMARY_INSTRUCTION.contains("After:"));
        assert!(!STAGE_SUMMARY_INSTRUCTION.contains('`'));
        assert!(validate_summary_format("   ", &state).is_err());
        assert!(validate_summary_format("Uses `backticks` in summary.", &state).is_err());
        assert!(validate_summary_format("Summary: prefixed summary.", &state).is_err());
        assert!(validate_summary_format("# Top-level heading in summary", &state).is_err());
        assert!(validate_summary_format(&long_line, &state).is_err());
    }

    #[test]
    fn test_build_sashiko_patch_review_workflow() {
        let wf = build_sashiko_patch_review_workflow();
        assert_eq!(wf.name, "sashiko_patch_review");
        assert_eq!(wf.steps.len(), 5);
    }

    #[test]
    fn test_format_sashiko_inline_findings_inserts_empty_lines() {
        let raw = "- [HIGH] First finding line one\n  first finding continuation.\n- [MEDIUM] Second finding line one\n  second finding continuation.\n- [LOW] Third finding.";
        let formatted = format_sashiko_inline_findings(raw);
        assert_eq!(
            formatted,
            "- [HIGH] First finding line one\n  first finding continuation.\n\n- [MEDIUM] Second finding line one\n  second finding continuation.\n\n- [LOW] Third finding."
        );
        // Idempotent when already separated by empty lines
        assert_eq!(format_sashiko_inline_findings(&formatted), formatted);
    }

    #[test]
    fn test_series_context_enabled_for_sashiko_wiring_and_verification_stages() {
        for stage_name in ["goal", "implementation", "interfaces-compat", "tests"] {
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
    async fn test_sashiko_verification_routes_preexisting_only_to_concerns_not_findings() {
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
                                    "problem": "db: pre-existing missing index on patches table",
                                    "severity": "Medium",
                                    "severity_explanation": "Existed before this commit.",
                                    "preexisting": true,
                                    "locations": []
                                },
                                {
                                    "problem": "api: newly introduced panic on empty header",
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
                    provider_metadata: None,
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

        let mut state = SashikoPatchReviewState {
            all_concerns: vec![json!({
                "stage": "persistence",
                "stages": ["persistence"],
                "prompts": ["subsystem/db-migrations.md"],
                "description": "candidate"
            })],
            ..Default::default()
        };

        let stage = verification_stage(1, 0.0);
        let rendered_ver_log = stage.user_prompt.render_for_log(&state);
        assert!(rendered_ver_log.contains("@subsystem/db-migrations.md"));
        stage.execute(&env, &mut state, None).await.unwrap();

        assert_eq!(state.findings.len(), 1);
        assert_eq!(
            state.findings[0]["problem"],
            "api: newly introduced panic on empty header"
        );
        assert_eq!(state.findings[0]["stage"], "persistence");
        assert_eq!(state.findings[0]["stages"], json!(["persistence"]));
        assert_eq!(
            state.findings[0]["prompts"],
            json!(["subsystem/db-migrations.md"])
        );
        assert_eq!(state.concerns.len(), 1);
        assert_eq!(
            state.concerns[0]["description"],
            "db: pre-existing missing index on patches table"
        );
        assert_eq!(state.concerns[0]["preexisting"], true);
        assert_eq!(state.concerns[0]["stage"], "persistence");
        assert_eq!(state.concerns[0]["stages"], json!(["persistence"]));
        assert_eq!(
            state.concerns[0]["prompts"],
            json!(["subsystem/db-migrations.md"])
        );

        // When report_preexisting is true, the verified pre-existing finding is also retained in findings,
        // and state.concerns appends without clearing earlier items.
        let mut state_with_preexisting = SashikoPatchReviewState {
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
            "db: pre-existing missing index on patches table"
        );
    }

    #[test]
    fn test_sashiko_stage_output_validators() {
        let state = SashikoPatchReviewState::default();
        let ver_stage = verification_stage(20, 0.0);

        // Missing top-level arrays or stray inner objects must fail.
        assert!(ver_stage.output_format.validate("{}", &state).is_err());
        assert!(
            ver_stage
                .output_format
                .validate(r#"{"file": "a.rs"}"#, &state)
                .is_err()
        );

        // Malformed findings or dismissed_concerns in verification must fail.
        let err = ver_stage
            .output_format
            .validate(
                r#"{"findings": [{"file": "a.rs"}], "hard_cases": [], "dismissed_concerns": []}"#,
                &state,
            )
            .expect_err("finding missing problem must fail");
        assert!(err.contains("findings[0]"));

        let err = ver_stage
            .output_format
            .validate(
                r#"{"findings": [{"problem": "workflow: bug", "severity": "High", "severity_explanation": "explain", "preexisting": false}], "hard_cases": [], "dismissed_concerns": []}"#,
                &state,
            )
            .expect_err("finding missing locations must fail");
        assert!(err.contains("locations"));

        // Valid outputs succeed.
        assert!(
            ver_stage
                .output_format
                .validate(
                    r#"{"findings": [{"problem": "workflow: missing check", "severity": "High", "severity_explanation": "Unchecked return value", "preexisting": false, "locations": []}], "hard_cases": [], "dismissed_concerns": []}"#,
                    &state
                )
                .is_ok()
        );

        let pv_ok = PostVerificationOutput {
            findings: vec![json!({
                "problem": "workflow: missing check",
                "severity": "High",
                "severity_explanation": "Unchecked return value",
                "preexisting": false,
                "locations": []
            })],
            dismissed_concerns: vec![],
        };
        assert!(validate_post_verification_batch_output(&pv_ok, 1).is_ok());
        assert!(validate_post_verification_batch_output(&pv_ok, 2).is_err());

        let pv_bad = PostVerificationOutput {
            findings: vec![json!({"file": "a.rs"})],
            dismissed_concerns: vec![],
        };
        assert!(validate_post_verification_batch_output(&pv_bad, 1).is_err());
    }

    #[test]
    fn test_sashiko_post_verification_stage_preserves_existing_state() {
        let batch = vec![json!({
            "type": "Concurrency Hazard",
            "description": "Candidate deadlock in foo()",
            "estimated_severity": "High",
        })];
        let stage = post_verification_stage("post-verification-1", batch, 10, 0.0);
        let mut state = SashikoPatchReviewState {
            findings: vec![json!({
                "problem": "workflow: earlier finding",
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
                    "problem": "workflow: new post-verified finding",
                    "severity": "High",
                    "preexisting": false,
                }),
                json!({
                    "problem": "db: post-verified pre-existing",
                    "severity": "Medium",
                    "severity_explanation": "Old missing index",
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
        assert_eq!(state.findings[0]["problem"], "workflow: earlier finding");
        assert_eq!(
            state.findings[1]["problem"],
            "workflow: new post-verified finding"
        );
        assert_eq!(state.concerns.len(), 2);
        assert_eq!(state.concerns[0]["description"], "Old issue");
        assert_eq!(
            state.concerns[1]["description"],
            "db: post-verified pre-existing"
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

    #[test]
    fn test_sashiko_prompt_templates_preserve_dismissal_and_sweep_guidance() {
        for required in [
            "the candidate concern that was investigated and disproved",
            "NO DISMISSAL WITHOUT VERIFIED PROOF",
            "MUST cite the concrete disproving code",
            "Citing a single caller",
            "unpaired lifecycle/state transition",
        ] {
            assert!(
                CONCERN_JSON_SCHEMA_EXAMPLE.contains(required),
                "Sashiko analysis stage guidance lost: {required}"
            );
        }
        for (name, instruction) in [
            ("implementation", STAGE_IMPLEMENTATION_INSTRUCTION),
            ("execution-flow", STAGE_EXECUTION_FLOW_INSTRUCTION),
            ("concurrency", STAGE_CONCURRENCY_INSTRUCTION),
            ("persistence", STAGE_PERSISTENCE_INSTRUCTION),
        ] {
            assert!(
                instruction.contains("Do not stop after finding several bugs"),
                "Sashiko {name} stage must include full-hunk sweep directive"
            );
        }
        assert!(
            STAGE_VERIFICATION_INSTRUCTION.contains("speculative_dismissal")
                && STAGE_VERIFICATION_INSTRUCTION
                    .contains("Single-caller or happy-path-only proofs")
                && STAGE_VERIFICATION_INSTRUCTION
                    .contains("Asymmetry or cleared-state rationalizations"),
            "Sashiko verification stage must classify speculative, single-caller, and asymmetry dismissals into hard_cases"
        );
        assert!(
            STAGE_POST_VERIFICATION_INSTRUCTION.contains("SYMMETRICAL PROOF BAR")
                && STAGE_POST_VERIFICATION_INSTRUCTION.contains("ALL-CALLERS VERIFICATION"),
            "Sashiko post-verification stage must enforce symmetrical proof bar and all-callers verification"
        );
    }
}
