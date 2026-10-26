# Role
You're an expert Software Engineer with deep knowledge of Rust, Distributed Systems, Operating Systems and practical experience with infrastructure projects.

# Generic guidance
- You MUST commit changes to it after implementing each task or more often if it makes sense. Try to commit as often as possible. Every consistent and self-sufficient change must be committed.
- Sign all commits using the user's git configuration. Every commit **MUST** include a `Signed-off-by` line (e.g., using `git commit -s` which automatically uses the user's `user.name` and `user.email`). **NO EXCEPTIONS.** Do not use placeholder names or any other default unless explicitly configured in git.
- Make sure no lines in the commit message exceed 72 characters. Hard-wrap the commit message body to enforce this length (URLs and quoted log lines that cannot be wrapped are the only exceptions).
- **Never** use backticks to quote any code, functions and variables names, etc. in the commit message.
- Only standard git trailers belong in a commit message (`Signed-off-by`, `Fixes`, human `Co-authored-by`, `Link`); never emit tool-generated metadata tags or `Assisted-by:` / AI attribution trailers (what tools were used does not matter — the human signing off holds full responsibility and must never share or delegate it to an LLM).
- When a commit repairs a defect introduced by an identifiable earlier commit on `main`, add a Linux-style `Fixes:` trailer naming that commit: `Fixes: <12-char sha> ("<subject>")`, generated with `git log -1 --format='Fixes: %h ("%s")' --abbrev=12 <sha>`. Identify the introducing commit with `git blame` or `git log -S`; if it cannot be identified with confidence, omit the trailer rather than guess. If the defect is also tracked as a Sashiko bug, add a second line `Fixes: <project>-<uuid>`. Trailers form one block at the end of the message, immediately before `Signed-off-by`.
- Never start a new task with a non-clean git status.
- Make sure to not commit any logs or temporary files. Before committing, run `make check-pr`: it verifies `Signed-off-by` on the commit range (`RANGE=HEAD~1..HEAD` by default), runs `yamllint` only when YAML files changed, and runs `fmt`, `clippy` across feature profiles, and the tests only when Rust or Cargo files changed.
- Every change goes through the two-stage Sashiko-for-Sashiko self-review described under "Development Workflow / 2. Self-Review": the local `sashiko review --project sashiko --agent` loop before opening or updating a PR, and acting on every `sashiko-bot` finding posted on the PR afterwards.
- Once the task is done, no local changes should remain. Amend them to the previous commit, if it makes sense, make a standalone commit or get rid of them.
- Each commit should implement one consistent and self-sufficient change. Never create commits like "do X and Y", create 2 commits instead.
- For any non-trivial feature create a design document first, then review it and then implement it step by step.
- If not sure, ask the user, don't proceed without confidence. Also ask for confirmation for any high-level architecture decisions, propose options if applicable.
- Before starting any test or running the main binary, ensure no other `sashiko` processes are running to avoid port conflicts or database locking issues.
- When referring to Sashiko bugs (in PR descriptions, GitHub comments, commit messages, or conversations), **always** use the public `<project>-<uuid>` identifier (e.g. `sashiko-<uuid>` or `linux-<uuid>`). **Never** refer to bugs by internal SQLite row IDs (`Bug #<id>`), which are not user-visible and get auto-linked to unrelated GitHub issues or PRs.

# Development Workflow

## 1. Common Commands
Use `make` to run common development tasks:
- `make lint`: Run all linters (`clippy`, `fmt`, `yamllint`).
- `make lint-local` / `make lint-local-cache`: Run `clippy` on minimal local-review feature profiles (`--no-default-features` and `--features cache`).
- `make test`: Run unit and integration tests (`cargo test --all-features`).
- `make check-pr`: Run diff-aware PR checks (verifies `Signed-off-by` on `RANGE`, runs `yamllint` only when YAML files changed, and `fmt`, `clippy` across feature profiles, and `test` only when Rust/Cargo files changed).
- `make check-all`: Run the complete suite unconditionally (`lint`, `lint-local`, `lint-local-cache`, `test`, `check-db-invariants`).
- `make check-db-invariants`: Run lightweight database invariant checks.

## 2. Self-Review (Sashiko for Sashiko)
Sashiko reviews changes to its own repository using the `--project sashiko` profile in a two-stage development loop:
- **Workflow & Prompts:** Defined by `src/workflows/sashiko_patch_review.rs` and first-party prompt guides under `projects/sashiko/prompts/` (distinct from the Linux review prompts in `projects/linux/prompts/`).
- **Scope:** Audits Sashiko-specific invariants using review procedures (`projects/sashiko/prompts/*.md`), maintainer notes (`projects/sashiko/prompts/maintainers/*.md`), and source-verified code notes (`projects/sashiko/prompts/notes/`), including UX, SQLite migrations and query scaling, email delivery safety, untrusted input boundaries, Tokio/async discipline, and commit message hygiene. Deterministic checks (compilation, borrow checking, formatting, clippy lints) are handled by `make check-pr`.
- **Stage 1 — Local Agentic Loop:** After `make check-pr` passes, run `sashiko review --project sashiko --agent <commit>` (or `cargo run --bin sashiko -- review --project sashiko --agent <commit>`) locally before opening or updating a PR so the review returns concise, machine-friendly JSON output without interactive prompts or redundant formatting stages. Fix any valid findings, amend or commit, and re-verify until both `make check-pr` and the local self-review pass cleanly.
- **Stage 2 — Upstream GitHub PR Review:** When a PR is opened or updated on GitHub, the upstream Sashiko instance (`sashiko.sashiko.dev`) automatically reviews the PR and posts findings as PR comments (`sashiko-bot`; read them with `gh pr view <num> --json comments,reviews`). PR authors and agents are **required to act on every finding**:
  1. **Valid finding:** Fix the defect in code, re-run `make check-pr` and local self-review, push the updated branch, and reply on the PR confirming the resolution.
  2. **False positive:** Reply on the PR explaining with concrete code evidence why the finding is a false positive, and ideally propose an accompanying or follow-up improvement to the Sashiko-for-Sashiko prompts or workflow (`projects/sashiko/prompts/`, `src/workflows/sashiko_patch_review.rs`) so future reviews do not repeat the false positive.

# Rust Coding Standards

- **Toolchain:** Target stable Rust (edition 2024); do not use unstable/nightly features.
- **Safety & Error Handling:** Prioritize safe Rust; document the safety invariant on any `unsafe` block. Use `Result<T, E>` and `?` for recoverable errors. Avoid `.unwrap()` and `.expect()` in production code unless statically proven infallible (and documented why).
- **Type-Driven State:** Never rely on raw text/string values to represent application state. Leverage Rust's type system (`enum`s, newtypes, traits) so invalid states are unrepresentable at compile time.
- **Complexity & Reuse (DRY):** Keep functions focused (soft limit ~50 lines, cyclomatic complexity < 15) and extract shared logic rather than duplicating code. Be mindful of blocking operations in async contexts (`tokio::task::spawn_blocking` when needed).
- **Comments (Statements, Not Questions):** Comments must be declarative statements explaining *why* something is done or clarifying non-obvious invariants, never rhetorical questions.
- **Never Run The Suite From Git:** Do not run `cargo test`, `make test`, or `make check-pr` under `git rebase --exec`, `git bisect run`, or a git hook. This checkout is a linked worktree, and git exports `GIT_DIR` to every command it starts, which points test fixtures at the real repository instead of their temporary directories. To verify a series commit by commit, clone into a throwaway directory (`git clone . /tmp/verify && cd /tmp/verify`) and run the checks there.
- **Spawning Git (One Constructor):** Never write `Command::new("git")`. Build every git invocation with `git_cmd::in_dir`, `git_cmd::in_dir_async`, or `git_cmd::detached_async` (for the rare command that names every path it touches). Git reads `GIT_DIR`, `GIT_WORK_TREE`, and their relatives before looking at the working directory; the `git_cmd` constructors strip those environment variables. A test in `src/git_cmd.rs` fails the build if a raw constructor appears anywhere else in `src/`.

# Project Map

## Core Application (`src/`)
- `main.rs`: Application entry point (`sashiko` server daemon, `sashiko init`, `sashiko review`, `sashiko repo sync`, `sashiko benchmark`).
- `bin/`: CLI binary (`sashiko-cli.rs`).
- `lib.rs`: Shared library root and `ReviewStatus` types.
- `benchmark.rs`: Built-in benchmark runner, per-run database/artifact preservation, and structured AI judge telemetry (`#[cfg(feature = "server")]`).
- `worker/`: Background workers (`bug_worker.rs`, `email.rs`, `forge.rs`, `patchwork.rs`, `repack.rs`, `sync.rs`, `prefetch.rs`, `prompts.rs`).
- `workflow/`: Core state-machine LLM workflow engine.
- `workflows/`: Declarative review and bug pipelines per project (`linux_patch_review.rs`, `linux_bug.rs`, `sashiko_patch_review.rs`, `qemu_patch_review.rs`), plus shared security/normalization helpers (`guard.rs`) and the review-map lineage builder (`review_map.rs`).
- `toolbox/`: Agent tools for inspecting git repositories and worktrees.
- `ai/`: LLM provider integrations, session runner, and response caching.
- `auth.rs` & `access.rs`: Authentication (`LocalToken`, JWTs) and capability/subsystem bug access control (`Principal`, `BugAccess`).
- `maintainers.rs`: `MAINTAINERS` file parser and subsystem/maintainer lookup index.
- `ingestor.rs`, `fetcher.rs`, `mbox.rs`, `nntp.rs`, `patchwork.rs`, `backfill.rs`: Mailing list, Lore mbox, NNTP, and Patchwork ingestion pipelines.
- `reviewer.rs`: Patchset review worker orchestration.
- `local_review.rs`: Local review execution (`sashiko review`).
- `git_cmd.rs` & `git_ops.rs`: Safe git process spawning and repository operations.
- `repo_sync.rs`: Provisioning of the tree a daemon reviews (`sashiko repo sync`) and the daemon's startup check for it.
- `patch.rs`, `baseline.rs`, `prerequisites.rs`: Patch parsing, baseline detection, and prerequisite series resolution.
- `forge.rs`: Webhook integration and review posting for external forges (GitHub, GitLab).
- `email_router.rs` & `email_policy.rs`: Outbound email routing and policy enforcement.
- `db.rs` & `migrations/`: SQLite/libsql database layer (`#[cfg(feature = "server")]` for `Database`), schema migrations, and shared wire models.
- `api.rs`: Shared HTTP protocol request/response types (unconditional).
- `server.rs`: Axum HTTP API server handlers (`#[cfg(feature = "server")]`).
- `settings.rs`: Application settings and ACL configuration.
- `project.rs` & `prompt_bundle.rs`: Target project profiles (`linux`, `sashiko`, `qemu`) and prompt bundle loading.
- `events.rs`, `metrics.rs`, `logging.rs`, `compression.rs`, `utils.rs`: Internal event bus, metrics, logging, and utilities.

## Cargo Feature Profiles
- `server` (enabled by default): Full daemon, HTTP API (`axum`), NNTP/forge ingestion, email delivery (`lettre`), SQLite/libsql persistence (`Database`), and `cache`.
- `cache`: Local SQLite AI response caching (`ai.response_cache` / `CachingAiProvider`).
- `bedrock` / `vertex`: Optional AWS Bedrock and Google Cloud Vertex AI provider backends.
- `--no-default-features`: Minimal build for `sashiko` local review (`sashiko init`, `sashiko review`) and `sashiko-cli` without `libsql`, `axum`, `lettre`, or `jsonwebtoken` (`--features cache` adds the local AI response cache).

## Configuration, Prompts & Docs
- `Settings.toml` & `projects/linux/mailing_lists.toml`: Main application and per-mailing-list tracking/delivery policy configuration (see `projects/linux/MAINTAINERS_GUIDE.md` for Linux maintainer onboarding and policy guidance).
- `projects/sashiko/prompts/`: First-party review prompts for reviewing Sashiko itself (`--project sashiko`): review procedures (`*.md`), maintainer notes (`maintainers/`), and code notes (`notes/`).
- `projects/linux/prompts/`: The Linux review prompts (bundled as `kernel/`): review procedures, maintainer notes (`maintainers/`) and code notes (`notes/`); its `README.md` describes the layout.
- `projects/qemu/prompts/`: The QEMU review prompts (`--project qemu`): review procedures (`*.md`) and maintainer notes (`maintainers/`); generated code notes (`notes/`) are not produced for QEMU yet.
- `static/`: Web UI assets (`static/index.html`, images).
- `docs/`: User and operator documentation (`configuration.md`, `daemon.md`, `sashiko-cli.md`, `llm-providers.md`, `benchmarking.md`, forge setup guides).
- `designs/`: Architecture and design documents.

# Benchmarking

**CRITICAL RULE:** Never run the full (`benchmark.json`) or small (`benchmark_small.json`) benchmarks without an explicit human request.

See `docs/benchmarking.md` for full setup and CLI options (`sashiko benchmark <path>` or `cargo run --bin sashiko -- benchmark <path>`). Available suites in `projects/linux/benchmarks/`:
- `benchmark.json` (999 entries, full suite)
- `benchmark_small.json` (99 entries, standard suite)
- `benchmark_tiny.json` (9 entries, quick iteration)
- `benchmark_missed.json` (upstream fixes for bugs missed by Sashiko after public launch)
- `benchmark_preexisting.json` (known Linux bugs for testing the pre-existing bug pipeline)

# LLM Workflow Design

When designing or modifying workflows and prompts in Sashiko:
- **Stage Design & Data Flow:** Keep each stage focused on a single problem with minimal but sufficient context (verify the task is solvable using only the data and tools provided, and avoid unused outputs). Use map-reduce (parallel expert stages followed by consolidation) for broad analyses, and short-circuit early when finding arrays are empty.
- **Negative Data Tracking:** When an LLM investigates a candidate concern and determines it is *not* a bug, output it explicitly as a `dismissed_concern` with concrete reasoning so later stages and humans do not re-verify it.
- **Prompt & Schema Design:** Use unambiguous field names and consistent vocabulary across prompts and JSON schemas. Always include an escape hatch in enums/classifiers (e.g., `"Other"`, `"Unknown"`, `"Not Applicable"`) so the model is never forced to hallucinate a rigid category.
- **"Anti-Charity" Directives:** Instruct the LLM not to give code the benefit of the doubt; dismissing an issue requires citing *concrete code* proving safety rather than assuming callers or surrounding systems handle it.
- **Idempotency & Custom Validators:** Stages must be idempotent across retries. When structural validation fails, return specific, actionable error strings telling the LLM *exactly* which rule it violated so the retry succeeds.
- **Observability:** Log all interactions with the LLM and capture every input and output string in full for end-to-end traceability.
