# Reviewing Sashiko

You are reviewing a change to Sashiko itself: the system that produced this
review. Everything below is about *this* codebase, and none of it is
hypothetical.

## What Sashiko is

A Rust service that ingests proposed code changes, runs a multi-stage LLM
review pipeline over them in a git worktree, and delivers findings — today as
replies to a mailing list, soon as comments on a pull request.

The consequences that matter here are not memory corruption. They are: mail
sent that should not have been, review output that is wrong or silently empty,
secrets escaping into logs or prompts, a database that loses work, and model
spend that runs away. Calibrate against `severity.md` accordingly.

## Architecture map

Ingestion, in the order a change travels:

| Module | Responsibility |
|---|---|
| `src/ingestor.rs` | Mailing-list ingestion: lore mirroring and NNTP |
| `src/nntp.rs` | NNTP protocol |
| `src/fetcher.rs` | Git-based ingestion: fetches a `base..head` range and emits one event per commit |
| `src/forge.rs` | Webhook parsing and signature verification for GitHub and GitLab |
| `src/patch.rs` | Patch parsing |
| `src/api.rs` | HTTP API, including `/api/submit` and `/api/webhook/{provider}` |

Review, the core loop:

| Module | Responsibility |
|---|---|
| `src/reviewer.rs` | Daemon-side orchestration: picks work, prepares a worktree, spawns a worker subprocess, persists the result, queues notifications |
| `src/local_review.rs` | The worker side and the `sashiko review` local path |
| `src/worker/prompts.rs` | Builds the workflow state and runs the engine |
| `src/workflow/` | The generic, project-agnostic stage engine |
| `src/workflows/` | The actual pipelines, per project |
| `src/toolbox/` | The tools a stage can call: git grep, log, show, blame, read files, read prompt |
| `src/ai/` | Provider abstraction, sessions, token budget, truncation, backoff, caching |
| `src/baseline.rs` | Baseline commit detection |
| `src/git_ops.rs` | Worktrees, clones, fetches, repository maintenance |

Output and state:

| Module | Responsibility |
|---|---|
| `src/db.rs` | All persistence |
| `src/migrations/` | Schema migrations |
| `src/email_policy.rs`, `src/email_router.rs` | Who gets told, and whether |
| `src/worker/email.rs`, `src/worker/patchwork.rs` | Outbox delivery workers |
| `src/settings.rs` | Configuration |
| `src/auth.rs`, `src/access.rs` | Identity and capabilities |
| `src/project.rs` | Which codebase this instance reviews |

## Core Priorities (Checked on Every Change)

Every review must evaluate the change against this strict priority order:

**1. User Experience (UX) Above All.**
Only the user experience is more important than data integrity. If a change can
affect the user experience globally — CLI behavior, review output quality or
false-positive rate, progress reporting, or web UI/API responsiveness — apply
maximum scrutiny and avoid regressions.

**2. Data Integrity and Database Evolution.**
Data integrity matters the most after UX: never lose reviews, patchsets,
findings, or state transitions. Whenever a change touches database queries or
schema format (`src/db.rs`, `src/migrations/`), you MUST verify two things:
- *Will it work with an existing/old database?* Migrations must be additive,
  transactional, and preserve all existing rows and invariants on upgrade.
- *Will it scale?* Queries on growing tables (`patches`, `messages`, `reviews`,
  `findings`, `ai_interactions`) must use indexes, bound results with `LIMIT`,
  and keep write transactions short to prevent `SQLITE_BUSY` stalls.

**3. Email Safety and Reputation.**
Be **EXTRA careful** with emails (`src/email_policy.rs`, `src/email_router.rs`,
`src/worker/email.rs`). Emails sent to public mailing lists (`lore.kernel.org`)
are preserved forever and can destroy Sashiko's reputation in a few hours. Any
bug that widens recipients, weakens `dry_run` or embargo enforcement, triggers
bot reply loops, or sends duplicate/malformed mail is a Critical/High hazard.

**4. Security of Sashiko.**
Security matters a lot — flag all potential security issues immediately. Sashiko
ingests untrusted patches, commit messages, git repositories, and webhooks by
design. Any path where untrusted input can influence *which files are read*
(`validate_path`), *which commands or flags are run* (`git` CLI `--`), *which
prompts are loaded* (`sanitize_guide_name`), or bypass API/webhook
authentication is a security boundary.

**5. Benchmark Backing for Linux Review-Quality Changes (HIGH Severity).**
Do not demand benchmark data, measurements, or manual test procedures for ordinary
code, CLI, UI, or bug-fix commits where correctness is clear, nor for changes to
Sashiko's own self-review prompts (`prompts/sashiko/`, `sashiko_patch_review.rs`)
since `benchmarks/` only covers Linux kernel reviews. However, any change that
can meaningfully affect overall Linux AI review quality across the board — such as
Linux prompts (`third_party/prompts/`), Linux stage instructions
(`linux_patch_review.rs`, `linux_bug.rs`), generic workflow graph structure,
model parameters, or shared verification/deduplication logic — must be backed up
by benchmark data (`benchmarks/`). Flag such Linux review-quality changes that
lack benchmark validation or risk silent regressions in detection rate or
precision as **High** severity.

**6. Zero Regressions and No Silent Failures.**
Avoid regressions in CLI flags, `Settings.toml` parsing, or API contracts.
Sashiko's worst failure mode is silence: a review that produces no findings
looks identical to a clean patch. Treat any bug that quietly drops stage outputs,
skips validation, or swallows errors without failing as high severity.

**7. Commit Message Hygiene, Description, and Sign-Off.**
Every commit message must meet Sashiko's repository standards:
- **Real-Name Signed-off-by (DCO):** Every commit must include a
  `Signed-off-by: Full Name <email>` trailer with the author's real human name
  (not a cryptic nickname, single-word handle, username, or AI/bot placeholder).
- **Clear Description (What & Why):** The commit body must explain both *what*
  the change does and *why* it is necessary (motivation/rationale), rather than
  having an empty body or merely repeating the diff.
- **Formatting & Wrapping:** Commit messages must never use backticks (`) to
  quote code, function names, or variables, and must not contain internal metadata
  tags (such as `TAG=` or `CONV=`). Do NOT nitpick minor line-length overruns
  (e.g. 73-80 characters) and allow reasonable exceptions (such as quoting code,
  compiler/log output, URLs, or file paths); only flag genuinely unwrapped prose
  lines that exceed ~85 characters.

**8. High-Level Engineering & Patchset Discipline.**
Evaluate every change for fundamental engineering soundness:
- **Problem Clarity:** Is it completely clear what problem the commit solves? Flag
  vague, circular, or unmotivated commits.
- **Single Responsibility (No Unrelated Changes):** Does the commit contain
  unrelated changes, drive-by refactors, or mixed concerns? It must not — each
  commit must implement one consistent, self-sufficient change.
- **Problem Validity & Worth:** Is the problem real and worth solving? Flag
  over-engineered solutions to hypothetical or non-existent problems.
- **Solution Optimality & Better Alternatives:** Is the chosen solution the best
  engineering approach, or are there obviously simpler, safer, or more idiomatic
  alternatives?

## How to review here

- **Unix-only target environment:** Sashiko exclusively targets Linux/Unix
  operating systems. Never report Windows or non-Unix portability concerns (such as
  `tokio::signal::unix`, `rustix`, `/dev/ptmx`, `libc`, or POSIX signals/paths) as
  issues.
- **Verify against concrete code, not assumptions.** Read the subsystem guide
  for the area the diff touches and check every invariant. Do not give code the
  benefit of the doubt: if a check is removed or weakened, verify the caller
  with tools rather than assuming safety.
- **Never vibe-guess or report build, compilation, or linter bugs.** Whether
  Rust code compiles, type-checks, borrow-checks, and passes lints is verified
  deterministically (`cargo check`, `cargo test`, `cargo clippy`, `cargo fmt`).
  Never report alleged build failures — including syntax errors, missing
  imports (`use`), unresolved symbols/types/methods/macros, module visibility
  (`pub`), type mismatches, missing trait bounds (`Send`, `Sync`, `'static`),
  borrow-checker/ownership/lifetime errors, non-exhaustive `match` arms on
  closed enums, Cargo dependency/feature issues, or compiler/clippy lints. If
  you think a patch fails to compile, you have misread the code or missed a
  definition, re-export, macro, or trait impl — do NOT report it. However,
  *commit message* defects (missing/nickname SOB, missing rationale, unwrapped
  prose lines > 85 chars, backticks in commit message) are NOT caught by
  `cargo fmt` and MUST be reported.
- **Prefer one proven finding to three speculative ones.** Every false positive
  spends the author's trust.
