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
    AnalysisStage, ConflictResolutionOutput, ConsolidationStage, LinuxPatchReviewState,
    PlanningOutput, PrescreenOutput, SERIES_CONTEXT_PLACEHOLDER, StageConcernsOutput,
    VerificationOutput, format_concerns_feedback, format_conflict_resolution_feedback,
    format_verification_feedback, validate_concerns_output, validate_conflict_resolution_output,
    validate_verification_output,
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
- Benchmark Backing for Linux Review-Quality Changes (HIGH Severity): Do NOT demand benchmark data, measurements, or manual test procedures in commit messages for ordinary code, CLI, UI, or bug-fix commits where correctness is clear, nor for changes to Sashiko's own self-review prompts (`prompts/sashiko/`, `sashiko_patch_review.rs`) since `benchmarks/` only covers Linux kernel reviews. However, if a change can meaningfully affect overall Linux AI review quality across the board (such as `third_party/prompts/`, `linux_patch_review.rs`, `linux_bug.rs`, generic workflow graph structure, model parameters, or shared verification/deduplication rules), it MUST be backed up by benchmark evaluation data (`benchmarks/`). If such a Linux review-quality change lacks benchmark validation or risks degrading detection rate or precision, flag it as a High severity issue.
- Unix-Only Target Environment: Sashiko exclusively targets Linux/Unix environments. NEVER report non-Unix or Windows compilation/portability issues (such as `tokio::signal::unix`, `rustix`, `/dev/ptmx`, `libc`, or POSIX signals/paths) as concerns.
- Never Vibe-Guess Build or Compilation Bugs: Build verification (`cargo check`, `cargo test`, `cargo clippy`) is deterministic. NEVER report alleged build failures, syntax errors, missing imports (`use`), unresolved symbols/types/methods/macros, type mismatches, missing trait bounds, borrow-checker/lifetime errors, or Cargo build issues.
- Global UX & Regressions: If the change can affect the user experience globally (CLI ergonomics, review output clarity/false-positive rate, progress display, or web UI/API behavior), apply maximum scrutiny and reject regressions.
- Architectural Boundaries: Check whether the change violates instance isolation, leaks project-specific assumptions into generic engines, or introduces subtle regressions in daemon/worker coordination.
- Commit Message Audit (Mandatory): Inspect the commit message header, body, and trailers in the patch:
  1. Signed-off-by with Real Name: Verify a `Signed-off-by: Real Name <email>` trailer is present and uses a real human name (first and last name), NOT a single-word handle, cryptic nickname, username, or AI/bot placeholder.
  2. Substantive Description (What & Why): Verify the commit body clearly explains both *what* changed and *why* it is needed (rationale/motivation). Flag missing bodies on non-trivial commits or descriptions that merely parrot the diff without explaining why.
  3. Commit Message Formatting: Flag backticks (`) used to quote code/functions/variables/filenames in the commit message or internal metadata tags (such as `TAG=` or `CONV=`). For line length, do NOT nitpick minor overruns (e.g. 73-80 characters) and allow reasonable exceptions (such as quoting code, compiler/log output, URLs, or file paths); only flag genuinely unwrapped prose lines that exceed ~85 characters."#;

const STAGE_IMPLEMENTATION_INSTRUCTION: &str = r#"# Verify implementation against intent

Verify that the code changes faithfully and completely implement what the commit message and design claim.
- Check for incomplete refactors at runtime boundaries: if a new enum variant, CLI flag, or configuration field is added, verify that wildcard/catch-all match arms (`_ => ...`), subprocess boundaries (`reviewer.rs`, `sashiko-cli`), and serialization paths handle it properly. Do NOT vibe-guess compile-time errors (such as non-exhaustive match arms on closed enums, missing imports, unresolved symbols/types, type mismatches, or borrow-checker errors) — build verification is deterministic.
- Check edge cases: empty inputs, missing optional fields, zero/boundary values, and fallback behavior.
- Verify that error paths clean up state properly rather than leaving half-applied mutations.
- Never report Windows or non-Unix portability concerns; Sashiko is strictly a Linux/Unix system."#;

const STAGE_EXECUTION_FLOW_INSTRUCTION: &str = r#"# Trace execution flow and panic safety

Trace the execution paths through every modified function and caller.
- Audit strictly for panic vectors on untrusted or runtime inputs: `.unwrap()`, `.expect()`, direct slice/array indexing (`[i]`), or string slicing (`&s[..n]`) that could land inside a multi-byte UTF-8 character.
- Audit for silently swallowed errors (`let _ = ...`, `.ok()`, `.unwrap_or_default()`) on critical operations such as database status updates, worktree cleanup, or structured LLM output parsing.
- Check numeric casts (`as`) and arithmetic for potential truncation or underflow/overflow.
- Never vibe-guess or report compile-time/build errors (borrow-checker, lifetime/move, type mismatch, unresolved import/symbol, or missing trait bound errors); focus strictly on runtime behavior and panics."#;

const STAGE_CONCURRENCY_INSTRUCTION: &str = r#"# Audit async Tokio discipline and concurrency

Audit all async execution, locking, child process management, and shared state access:
- Check for synchronous blocking I/O (`std::fs`, synchronous `git` CLI commands, heavy CPU loops, `std::thread::sleep`) inside `async fn` on Tokio worker threads without `spawn_blocking`.
- Check for `std::sync::MutexGuard` or `RwLockGuard` held across `.await` points.
- Check child process management (`tokio::process::Command`): verify `.kill_on_drop(true)` when subject to timeouts, and verify stdout/stderr are drained concurrently (`wait_with_output`) to prevent 64 KB pipe buffer deadlocks.
- Check cancellation safety in `tokio::select!` and `timeout` blocks: ensure dropped futures do not leak git worktrees or leave SQLite rows stuck in `in_progress`.
- Check cross-process contention on `sashiko.db` and shared `review_trees/` git repositories."#;

const STAGE_PERSISTENCE_INSTRUCTION: &str = r#"# Audit database schema, queries, and transactions

Data integrity matters second only to UX. Audit all SQLite/libsql operations in `src/db.rs` and schema migrations in `src/migrations/` against two mandatory questions:
1. Will it work with an existing/old database? Verify that schema changes are strictly additive, migrations run atomically inside transactions (`PRAGMA user_version`), and existing rows/queries remain valid on upgrade without data loss.
2. Will it scale? Verify that queries on high-cardinality tables (`patches`, `messages`, `reviews`, `findings`, `ai_interactions`) are backed by indexes, list queries include explicit `LIMIT` clauses, write transactions are kept short (never held across network I/O or LLM calls), and task claims use atomic `UPDATE ... WHERE status = ...` rather than TOCTOU `SELECT` then `UPDATE`."#;

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
- REST API & UX: verify API response shapes remain backwards-compatible and global user-facing behavior does not regress.
- Target OS: Sashiko runs exclusively on Linux/Unix. Never flag Unix-specific APIs or lack of Windows support."#;

const STAGE_TESTS_INSTRUCTION: &str = r#"# Audit test coverage and determinism

Evaluate the tests accompanying this change (or check whether new tests are required):
- Verify that non-trivial logic, bug fixes, parser edge cases, or database queries include unit or integration tests when appropriate. Do NOT demand tests or manual test descriptions for trivial changes or where existing coverage is sufficient.
- Check test determinism and isolation: tests must not depend on wall-clock timing races, shared hardcoded TCP ports, or mutable global filesystem paths outside `tempfile::TempDir`.
- Standard Unix pseudo-devices (such as `/dev/null` or `/dev/ptmx` for PTY tests) are standard on Linux/Unix and must NOT be flagged as non-Unix portability or filesystem isolation violations.
- Verify that environment variable mutations in tests (`std::env::set_var`) restore previous values or are properly isolated."#;

const STAGE_DEDUPLICATION_INSTRUCTION: &str = r#"# Deduplicate concerns and dismissed concerns

You are consolidating the independent findings from parallel Sashiko review stages.
1. Merge duplicate concerns that describe the same underlying defect into a single, comprehensive concern, preserving the most precise file/line locations and combining complementary reasoning.
2. Merge duplicate dismissed concerns.
3. Do not discard distinct concerns or invent new locations."#;

const STAGE_CONFLICT_RESOLUTION_INSTRUCTION: &str = r#"# Resolve conflicts between concerns and dismissed concerns

Compare the consolidated concerns against the consolidated dismissed concerns.
- If stage A raised a concern and stage B explicitly investigated the exact same code path and proved with concrete code evidence that it is safe (a dismissed concern), evaluate both arguments rigorously.
- Only drop a concern if the dismissed concern provides concrete, verifiable proof from the codebase that the issue cannot occur. Do NOT give code the benefit of the doubt."#;

const STAGE_VERIFICATION_INSTRUCTION: &str = r#"# Verify remaining concerns and calibrate severity

For each remaining concern, use the available Git and file tools to inspect the actual code in the worktree and verify whether the defect is real.
- Drop any concern that alleges a build, compilation, syntax, type-checking, borrow-checker, lifetime, missing-import, unresolved-symbol, missing-trait-bound, or linter error. Build correctness is verified deterministically by the compiler; LLMs must never vibe-guess build failures.
- If concrete code proves the concern is a false positive, drop it.
- For each verified issue, assign an accurate severity (`Critical`, `High`, `Medium`, or `Low`) strictly following `severity.md`, and formulate a concise bug title (`problem`) under 80 characters starting with a Sashiko component prefix (e.g. `workflow:`, `db:`, `reviewer:`, `toolbox:`, `api:`, `cli:`)."#;

const STAGE_REPORT_INSTRUCTION: &str = r#"# Generate plain-text inline review report

Generate the plain-text inline review report following the exact formatting rules and structure in `github-summary-template.md`.
- Output ONLY a plain bulleted list of ALL findings ordered from highest severity to lowest (`- [CRITICAL] ...`, `- [HIGH] ...`, `- [MEDIUM] ...`, `- [LOW] ...`), or `No issues found.` if there are no findings.
- Do NOT include `Summary:` or `Findings:` headers (the summary is generated and displayed separately in the UI).
- Do NOT use backticks, markdown code blocks, or markdown headings. Wrap all lines at 78 characters or fewer."#;

const STAGE_SUMMARY_INSTRUCTION: &str = r#"# Summarize the proposed change

Provide a concise 1-2 sentence plain-text summary explaining what this commit/change does and why.
- Use strictly plain text: no markdown, no backticks (`), no markdown headings (`#`), and no bullet points.
- Wrap lines at 78 characters or fewer.
- Summarize the change itself (do not list review findings or issues here)."#;

const CONCERN_JSON_SCHEMA_EXAMPLE: &str = r#"Return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays.
Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations".
Each object in the 'dismissed_concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "locations".
Do not invent line numbers; use null when exact values are unknown.

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
        wants_series_context: false,
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
        wants_series_context: false,
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

pub static DEDUPLICATION: ConsolidationStage = ConsolidationStage {
    name: "deduplication",
    short: "Deduplication",
    wants_series_context: false,
};

pub static CONFLICT_RESOLUTION: ConsolidationStage = ConsolidationStage {
    name: "conflict-resolution",
    short: "Conflict Resolution",
    wants_series_context: false,
};

pub static VERIFICATION: ConsolidationStage = ConsolidationStage {
    name: "verification",
    short: "Severity Estimation",
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

pub static CONSOLIDATION_STAGES: &[&ConsolidationStage] = &[
    &DEDUPLICATION,
    &CONFLICT_RESOLUTION,
    &VERIFICATION,
    &REPORT,
    &SUMMARY,
];

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
        if l.starts_with('#') || l.starts_with("Summary:") {
            return Err(
                "Do not use markdown headings ('#') or 'Summary:' prefixes in the summary."
                    .to_string(),
            );
        }
        if line.chars().count() > 84 {
            return Err(format!(
                "Line exceeds 78-character terminal width ({} chars): \"{}...\". Wrap lines at 78 characters.",
                line.chars().count(),
                line.chars().take(40).collect::<String>()
            ));
        }
    }
    Ok(())
}

fn format_summary_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious attempt was rejected: {}. Provide a concise 1-2 sentence plain-text summary without backticks, markdown headings, or 'Summary:' prefixes, wrapped at 78 characters.",
        violation
    )
}

fn append_stage_items(dest: &mut Vec<Value>, src: &[Value], stage: &str, default_type: &str) {
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
            map.insert("stage".to_string(), json!(stage));
        }
        dest.push(obj);
    }
}

fn append_stage_dismissed_concerns(dest: &mut Vec<Value>, src: &[Value], stage: &str) {
    for item in src {
        let mut obj = item.clone();
        if let Some(map) = obj.as_object_mut() {
            map.insert("stage".to_string(), json!(stage));
        }
        dest.push(obj);
    }
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
            "required": ["relevant_stages"]
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
            .reduce(
                move |state: &mut SashikoPatchReviewState, out: StageConcernsOutput| {
                    append_stage_items(&mut state.all_concerns, &out.concerns, def.name, "General");
                    append_stage_dismissed_concerns(
                        &mut state.all_dismissed_concerns,
                        &out.dismissed_concerns,
                        def.name,
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

pub fn deduplication_stage(
    max_turns: usize,
    temperature: f32,
) -> Stage<SashikoPatchReviewState, StageConcernsOutput> {
    Stage::builder(DEDUPLICATION.name)
        .system_prompt(sashiko_system_prompt(true))
        .user_prompt(
            PromptTemplate::<SashikoPatchReviewState>::new(format!(
                r#"{STAGE_DEDUPLICATION_INSTRUCTION}

Aggregated Concerns:
{{{{aggregated_concerns}}}}

Aggregated Dismissed Concerns:
{{{{aggregated_dismissed_concerns}}}}

{CONCERN_JSON_SCHEMA_EXAMPLE}"#
            ))
            .with_var("aggregated_concerns", |s: &SashikoPatchReviewState| {
                serde_json::to_string_pretty(&s.all_concerns).unwrap_or_default()
            })
            .with_var(
                "aggregated_dismissed_concerns",
                |s: &SashikoPatchReviewState| {
                    serde_json::to_string_pretty(&s.all_dismissed_concerns).unwrap_or_default()
                },
            ),
        )
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
        .skip_if(|s| s.all_concerns.is_empty())
        .reduce(|state, out: StageConcernsOutput| {
            state.deduplicated_concerns = out.concerns;
            state.deduplicated_dismissed_concerns = out.dismissed_concerns;
        })
        .build()
}

pub fn conflict_resolution_stage(
    max_turns: usize,
    temperature: f32,
) -> Stage<SashikoPatchReviewState, ConflictResolutionOutput> {
    Stage::builder(CONFLICT_RESOLUTION.name)
        .system_prompt(sashiko_system_prompt(true))
        .user_prompt(
            PromptTemplate::<SashikoPatchReviewState>::new(format!(
                r#"{STAGE_CONFLICT_RESOLUTION_INSTRUCTION}

Consolidated Concerns:
{{{{deduplicated_concerns}}}}

Consolidated Dismissed Concerns:
{{{{deduplicated_dismissed_concerns}}}}

Return ONLY a JSON object with a 'concerns' array containing the remaining concerns after resolving conflicts. Each object in the 'concerns' array MUST use exactly the following keys: "type", "description", "reasoning", "preexisting", "locations"."#
            ))
            .with_var("deduplicated_concerns", |s: &SashikoPatchReviewState| {
                serde_json::to_string_pretty(&s.deduplicated_concerns).unwrap_or_default()
            })
            .with_var(
                "deduplicated_dismissed_concerns",
                |s: &SashikoPatchReviewState| {
                    serde_json::to_string_pretty(&s.deduplicated_dismissed_concerns)
                        .unwrap_or_default()
                },
            ),
        )
        .output_format(
            OutputFormat::json()
                .with_validator(validate_conflict_resolution_output)
                .with_feedback_formatter(format_conflict_resolution_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .skip_if(|s| s.deduplicated_concerns.is_empty())
        .reduce(|state, out: ConflictResolutionOutput| {
            let mut new_concerns = Vec::new();
            let mut preexisting = Vec::new();
            for concern in out.concerns {
                let is_preexisting = concern
                    .get("preexisting")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if is_preexisting {
                    preexisting.push(concern);
                } else {
                    new_concerns.push(concern);
                }
            }
            state.patch_concerns = new_concerns;
            state.concerns = preexisting;
        })
        .build()
}

pub fn verification_stage(
    max_turns: usize,
    temperature: f32,
) -> Stage<SashikoPatchReviewState, VerificationOutput> {
    let series_context = series_context_placeholder(VERIFICATION.wants_series_context);
    Stage::builder(VERIFICATION.name)
        .system_prompt(sashiko_system_prompt(true))
        .user_prompt(with_series_context(
            PromptTemplate::<SashikoPatchReviewState>::new(format!(
                r#"{STAGE_VERIFICATION_INSTRUCTION}

CRITICAL REVIEW DIRECTIVE: To dismiss a concern as a false positive, you must find concrete evidence in the code that proves the concern is invalid. If you cannot find concrete proof of safety, you must retain the concern.{series_context}

Consolidated Concerns:
{{{{patch_concerns}}}}

Return ONLY a JSON object with a 'findings' array. Each object in the 'findings' array MUST use exactly the following keys: "problem" (a short naming string under 80 characters starting with a Sashiko component prefix like 'workflow:', 'db:', 'reviewer:', 'toolbox:', 'api:', 'cli:', NEVER using backquotes), "severity" (Low, Medium, High, or Critical), "severity_explanation" (detailed reasoning and proof), "preexisting" (boolean), "introduced_in_patch" (an integer or null: the series position, as in "[Patch N of M]", of the commit whose change first made the problem present, chosen from the series block (the commit under review on its header line, the preceding and subsequent lists below it): an earlier one when the problem was already present in the tree this commit was applied to, the one whose change made it so, not merely the last to touch the code; the one under review when its own diff introduces the problem; a subsequent one when only its change makes it a problem; null when "preexisting" is true or you cannot tell), "locations" (array of objects with file, function_or_symbol, line, code_snippet, and why_this_location_matters)."#
            ))
            .include_file("false-positive-guide.md")
            .include_file("severity.md")
            .with_var("patch_concerns", |s: &SashikoPatchReviewState| {
                serde_json::to_string_pretty(&s.patch_concerns).unwrap_or_default()
            }),
            VERIFICATION.wants_series_context,
        ))
        .output_format(
            OutputFormat::json()
                .with_validator(validate_verification_output)
                .with_feedback_formatter(format_verification_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .skip_if(|s| s.patch_concerns.is_empty())
        .reduce(|state, out: VerificationOutput| {
            let mut new_findings = Vec::new();
            for finding in out.findings {
                let is_preexisting = finding
                    .get("preexisting")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if is_preexisting {
                    let concern = json!({
                        "type": finding.get("problem").and_then(|v| v.as_str()).unwrap_or("Pre-existing Issue"),
                        "description": finding.get("problem").and_then(|v| v.as_str()).unwrap_or(""),
                        "reasoning": finding.get("severity_explanation").and_then(|v| v.as_str()).unwrap_or(""),
                        "preexisting": true,
                        "locations": finding.get("locations").cloned().unwrap_or(json!([])),
                    });
                    state.concerns.push(concern);
                }
                new_findings.push(finding);
            }
            state.findings = new_findings;
        })
        .build()
}

pub fn report_stage(max_turns: usize, temperature: f32) -> Stage<SashikoPatchReviewState, String> {
    Stage::builder(REPORT.name)
        .system_prompt(sashiko_system_prompt(true))
        .user_prompt(
            PromptTemplate::<SashikoPatchReviewState>::new(format!(
                r#"{STAGE_REPORT_INSTRUCTION}

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
        .skip_if(|s| s.findings.is_empty())
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
                    "Summarize the change concisely in 1-2 sentences without quoting verbatim."
                        .to_string(),
            },
            ..Default::default()
        })
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
        .stage(deduplication_stage(max_turns, temperature))
        .stage(conflict_resolution_stage(max_turns, temperature))
        .stage(verification_stage(max_turns, temperature))
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
        assert_eq!(stage_short_label("report"), Some("Report Generation"));
        assert_eq!(stage_short_label("summary"), Some("Change Summary"));
        assert!(is_known_stage("pre-screen"));
        assert!(is_known_stage("planning"));
        assert!(is_known_stage("persistence"));
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
        assert!(validate_summary_format("   ", &state).is_err());
        assert!(validate_summary_format("Uses `backticks` in summary.", &state).is_err());
        assert!(validate_summary_format("Summary: prefixed summary.", &state).is_err());
    }

    #[test]
    fn test_build_sashiko_patch_review_workflow() {
        let wf = build_sashiko_patch_review_workflow();
        assert_eq!(wf.name, "sashiko_patch_review");
        assert_eq!(wf.steps.len(), 7);
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
}
