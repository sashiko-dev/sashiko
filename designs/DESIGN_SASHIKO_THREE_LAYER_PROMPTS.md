# Three-Layer Prompt Architecture for Sashiko Self-Review

## 1. Motivation

Sashiko reviews changes to its own repository using the `--project sashiko`
profile (`projects/sashiko/prompts/` and `src/workflows/sashiko_patch_review.rs`).
While the Linux kernel review profile (`projects/linux/prompts/` and
`src/workflows/linux_patch_review.rs`) was migrated to a three-layer prompt
architecture in commits `04263a20a779` through `95f88317bb06`, the Sashiko
profile still relied on the earlier hand-written guide model:

1. **Hand-written subsystem and pattern guides (`subsystem/*.md`, `patterns/*.md`):**
   Fourteen long markdown files mixed maintainer policy, coding preferences,
   and false-positive suppressions with detailed descriptions of internal
   implementation facts (struct layouts, line numbers, lock orders, helper
   signatures). Several guides had already drifted from the codebase (for
   example, `subsystem/git-ops.md` instructed reviewers to check for
   `Command::new("git")` after `src/git_cmd.rs` outlawed raw `git` constructors,
   and `subsystem/workflow-engine.md` claimed `linux_patch_review.rs` was the
   sole consumer of the workflow engine).
2. **LLM `pre-screen` stage (`prescreen_stage`):**
   Every patch review paid an extra LLM round-trip to read
   `subsystem/subsystem.md` and select up to three subsystem guides, while
   also attaching static `guides` arrays to `ANALYSIS_STAGES`.
3. **Unused `<maintainer_notes>` and unframed `<code_notes>`:**
   `Worker::run` (`src/worker/prompts.rs`) already ran deterministic maintainer
   note selection (`select_maintainer_notes`) and code note selection
   (`select_code_notes`) for every project, but `sashiko_system_prompt` did not
   inject `{{maintainer_notes}}` or include the subordinate-snapshot framing
   for `<code_notes>`, and `projects/sashiko/prompts/notes/` had no generated
   note set.

This design brings `--project sashiko` onto the same three-layer prompt
architecture as Linux.

---

## 2. Three-Layer Architecture

### Layer 1: Universal Review Procedures (`projects/sashiko/prompts/*.md`)

Top-level markdown files carry universal review methodology, severity
classification, false-positive discipline, and output formatting:

- `review-core.md` — Core review methodology, git/worktree tool usage, diff
  context rules, and anti-assumption discipline. The volatile 30-row module
  map table is removed in favor of generated code notes (`<code_notes>`) and
  scoped maintainer notes (`<maintainer_notes>`).
- `severity.md` — Severity levels (`Critical`, `High`, `Medium`, `Low`) for a
  Rust async service and CLI tool.
- `false-positive-guide.md` — High-bar verification rules and Rust/Sashiko
  anti-hallucination checks.
- `github-summary-template.md` — Formatting rules for inline and PR review
  summaries.
- `prompt-injection.md` — Prompt-injection and untrusted-input audit rules
  loaded by the `security` analysis stage.

### Layer 2: Maintainer Notes (`projects/sashiko/prompts/maintainers/*.md`)

Human-curated files with `+++` TOML front matter (`title`, `paths`, `symbols`)
selected deterministically from the target commit diff by
`select_maintainer_notes` (`src/notes/maintainer_notes.rs`) and rendered into
`sashiko_system_prompt` under `<maintainer_notes>`:

- `general.md` (`paths = ["*"]`) — Repository-wide maintainer conventions
  (commit hygiene, DCO sign-off, 72-char body wrap, no backticks or AI
  attribution trailers in commit messages, `Fixes:` trailer rules, benchmark
  evidence rules for Linux review-quality changes, feature-gate parity across
  `server`, `cache`, and `--no-default-features`).
- `ai-providers.md` — Provider decorator ordering, error classification, quota
  and token-estimation policy, secret redaction, and subprocess CLI isolation
  (`src/ai/`).
- `api-auth.md` — Principal capabilities, `BugAccess` SQL parity, ConstantTimeEq
  token checks, and fail-closed redaction (`src/server.rs`, `src/api.rs`,
  `src/auth.rs`, `src/access.rs`).
- `db.md` — Additive SQLite migrations, `Database` method conventions, bounded
  batch queries, and transaction error propagation (`src/db.rs`,
  `src/migrations/`).
- `email.md` — Outbound email safety, `EmailPolicyConfig` defaults, embargo
  enforcement, and recipient filtering (`src/email_policy.rs`,
  `src/email_router.rs`, `src/worker/email.rs`, `src/patchwork.rs`,
  `src/worker/patchwork.rs`).
- `forge.md` — Forge webhook authentication, installation token scoping, fork PR
  trust boundaries, and comment size limits (`src/forge.rs`,
  `src/worker/forge.rs`).
- `git.md` — Mandatory `git_cmd` constructors (`in_dir`, `in_dir_async`,
  `detached_async`), `--end-of-options` argument separation, and worktree/mutex
  discipline (`src/git_cmd.rs`, `src/git_ops.rs`, `src/git_ops/`,
  `src/baseline.rs`, `src/prerequisites.rs`).
- `notes.md` — Policy for reviewing changes to `projects/*/prompts/maintainers/*.md`
  and `projects/*/prompts/notes/` (strict separation of human policy from code
  facts, narrow `paths`/`symbols` triggers, and subordinate `<code_notes>`
  framing).
- `rust-async.md` — Tokio async/blocking discipline, bounded channels and
  collections, UTF-8 char-boundary slicing, and lock discipline across `.await`
  points (`src/`).
- `settings.md` — `Settings` layering (`Settings.toml` synced with defaults,
  secret masking in `Debug`, `redact_secret` URL sanitization)
  (`src/settings.rs`, `Settings.toml`).
- `toolbox.md` — Read-only worktree confinement, path traversal rejection, and
  bounded output for agent tools (`src/toolbox/`).
- `workflows.md` — Declarative workflow isolation (`Stage::execute_isolated`),
  reducer accumulation invariants, `PromptTemplate` `Template`/`Included`
  boundary, single-pass variable substitution, and non-fatal tool errors
  (`src/workflow/`, `src/workflows/`, `src/worker/prompts.rs`).

All maintainer notes follow two strict invariants:
1. **Policy, not code facts:** They state what maintainers require, forbid, or
   deliberately suppress (with concrete safe preconditions), never restating
   volatile implementation facts that belong in `<code_notes>`.
2. **Narrow triggers:** Only `general.md` uses `paths = ["*"]`; every other
   file scopes `paths` to specific directory prefixes (trailing `/`) or exact
   file paths and uses specific, non-generic `symbols`.

### Layer 3: Automatic Code Notes (`projects/sashiko/prompts/notes/`)

Source-verified codebase facts are generated from Sashiko's repository via
`sashiko --project sashiko notes build` and stored under
`projects/sashiko/prompts/notes/0.6/` (`manifest.json` and `units/*.md`, with
`.facts.json` sidecars ignored by git):

1. **`component_map` Unit Source (`src/notes/profile.rs`, `src/notes/units.rs`):**
   Because Sashiko's top-level `src/*.rs` files represent distinct subsystems,
   `directory` unit discovery at `depth = 1` would produce 44 fragmented single-file
   units. Supporting `source = "component_map"` in `[units]` maps explicit
   unit IDs to file and directory prefixes, grouping all 143 files (`src/**`,
   `src/migrations/**.sql`, `build.rs`) into 12 cohesive units with zero
   unassigned files:
   - `workflow-engine` (`src/workflow/`)
   - `workflows` (`src/workflows/`)
   - `notes` (`src/notes/`)
   - `ai` (`src/ai/`)
   - `toolbox` (`src/toolbox/`)
   - `db` (`src/db.rs`, `src/migrations/`)
   - `api-server` (`src/server.rs`, `src/api.rs`, `src/auth.rs`, `src/access.rs`)
   - `ingestion-forge` (`src/ingestor.rs`, `src/nntp.rs`, `src/mbox.rs`,
     `src/patch.rs`, `src/fetcher.rs`, `src/forge.rs`, `src/backfill.rs`)
   - `email-delivery` (`src/email_policy.rs`, `src/email_router.rs`,
     `src/patchwork.rs`, `src/worker/email.rs`, `src/worker/patchwork.rs`,
     `src/worker/forge.rs`)
   - `review-runtime` (`src/reviewer.rs`, `src/local_review.rs`, `src/worker/`,
     `src/benchmark.rs`)
   - `git-baseline` (`src/git_cmd.rs`, `src/git_ops.rs`, `src/git_ops/`,
     `src/baseline.rs`, `src/prerequisites.rs`, `src/maintainers.rs`)
   - `cli-config` (`build.rs`, `src/main.rs`, `src/lib.rs`, `src/bin/`,
     `src/settings.rs`, `src/project.rs`, `src/prompt_bundle.rs`,
     `src/prompt_fingerprint.rs`, `src/prompt_namespaces.rs`, `src/events.rs`,
     `src/logging.rs`, `src/metrics.rs`, `src/compression.rs`, `src/utils.rs`,
     `src/test_support.rs`)
2. **Research Agenda (`projects/sashiko/prompts/notes/agenda/`):**
   Combines `universal.md`, `lang/rust.md`, and `domain/sashiko.md` (covering
   prompt/template trust boundaries, daemon-worker IPC and outbox coordination,
   and schema/feature-profile compatibility).
3. **Subordinate Prompt Framing:**
   `sashiko_system_prompt` frames `<maintainer_notes>` as binding project policy
   and `<code_notes>` as reference background from a fixed snapshot that is
   strictly subordinate to the repository source at `Target Commit SHA` and
   `Baseline SHA`.

---

## 3. Workflow and Test Changes

1. **`src/workflows/sashiko_patch_review.rs`:**
   - Remove `prescreen_stage` and `prescreen_stage_with_effort`, reducing the
     workflow graph from 5 top-level steps to 4 (`planning`, `analysis`,
     `verification_and_post_verification`, `report_and_summary`).
   - Update `sashiko_system_prompt` to inject `{{maintainer_notes}}{{code_notes}}`
     with the same `<maintainer_notes>` and subordinate `<code_notes>` framing
     as `linux_system_prompt`.
   - Clear `subsystem/*.md` and `patterns/*.md` from `ANALYSIS_STAGES` (keeping
     `guides: &["prompt-injection.md"]` on `security`).
2. **`src/prompt_bundle.rs`:**
   - Update `test_sashiko_prompt_bundle_integrity` to verify bundled procedures
     and `sashiko/maintainers/*.md`, matching `test_kernel_prompt_bundle_integrity`.
3. **Retire Legacy Guides & Update Docs:**
   - Remove `projects/sashiko/prompts/subsystem/` and
     `projects/sashiko/prompts/patterns/`.
   - Update `projects/sashiko/prompts/README.md`, `CONTRIBUTING.md`, and
     `GEMINI.md` to document the three-layer layout.
