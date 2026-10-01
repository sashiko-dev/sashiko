# Contributing to Sashiko

We welcome contributions, bug reports, prompt improvements, and architectural feedback from the community.

## Communication Channels

- **GitHub Issues & Pull Requests:** Use GitHub Issues for bug reports and feature requests, and submit Pull Requests for code or prompt changes.
- **Mailing List:** Join `sashiko@lists.linux.dev` ([lore archive](https://lore.kernel.org/sashiko)) for Sashiko-related announcements and broader AI-review discussions, including general feedback, architectural ideas, and prompt discussions. Automated patch reviews are sent from and should be replied to `sashiko-reviews@lists.linux.dev`.

## Developer Certificate of Origin (DCO) & Commit Hygiene

This project uses the Developer Certificate of Origin (DCO). Every commit **must** include a `Signed-off-by` trailer certifying that you wrote the code or have the right to contribute it.

Add this line automatically using the `-s` flag when committing:

```bash
git commit -s
```

Follow these commit guidelines:

- **Atomic commits:** Each commit should implement one consistent, self-sufficient change that compiles and passes all tests on its own.
- **Line length:** Hard-wrap commit message titles and bodies at 72 characters.
- **Plain text:** Do not use markdown backticks to quote symbols, functions, or file names in commit messages, and only include standard git trailers (such as `Signed-off-by`).

## Local Verification (`make check-pr`)

Before opening or updating a pull request, ensure your code compiles cleanly without warnings and passes the full PR check suite:

```bash
make check-pr
```

`make check-pr` runs:
- `make sob` — validates `Signed-off-by` tags across the commit range.
- `make lint` / `make lint-local` / `make lint-local-cache` — checks `cargo fmt`, `cargo clippy` across feature profiles (`server`, `--no-default-features`, and `--features cache`), and `yamllint`.
- `make test` / `make test-local` / `make test-local-cache` — runs the unit and integration test suites across all Cargo feature profiles.

If you are modifying Linux kernel review prompts (`third_party/prompts/`) or review workflows, also validate your changes against the benchmark suite (see the [Benchmarking Guide](docs/benchmarking.md)).

## Sashiko-for-Sashiko Review Workflow

Sashiko reviews changes to its own repository using the `--project sashiko` profile, driven by first-party prompt guides under [`prompts/sashiko/`](prompts/sashiko/README.md) and the workflow in [`src/workflows/sashiko_patch_review.rs`](src/workflows/sashiko_patch_review.rs).

While deterministic checks (compilation, borrow checking, formatting, clippy lints) are enforced by `make check-pr`, Sashiko-for-Sashiko audits semantic and architectural invariants across subsystems (`prompts/sashiko/subsystem/*.md`) and cross-cutting patterns (`prompts/sashiko/patterns/*.md`), including:

- User experience and CLI/UI consistency
- SQLite schema migrations and query scaling
- Email delivery and embargo safety
- Untrusted input boundaries and prompt injection defenses
- Tokio/async concurrency discipline and resource limits
- Commit message hygiene

Every contribution goes through a **two-stage Sashiko-for-Sashiko review process**:

### Stage 1: Local Review (Before Opening or Updating a PR)

After `make check-pr` passes, run Sashiko self-review locally on your commits before pushing:

```bash
# Review the latest commit interactively
sashiko review --project sashiko HEAD

# Review a branch range (use --agent for concise, machine-readable JSON output)
sashiko review --project sashiko --agent origin/main..HEAD
```

If Sashiko reports any valid findings or concerns:
1. Fix the issue in your commit(s).
2. Re-run `make check-pr`.
3. Re-run `sashiko review --project sashiko` until the review is clean.

### Stage 2: Upstream GitHub PR Review (`sashiko-bot`)

When you open or update a pull request on GitHub, the upstream Sashiko service (`sashiko.sashiko.dev`) automatically reviews the proposed commits and posts its report as a PR comment from `sashiko-bot`.

**Contributors and coding agents are required to act on every finding reported by `sashiko-bot`:**

1. **Valid finding:**
   - Fix the defect in code.
   - Re-run `make check-pr` and local self-review (`sashiko review --project sashiko`).
   - Push the updated branch and reply on the PR confirming the resolution.
2. **False positive:**
   - Reply on the PR with concrete code evidence explaining why the finding does not apply.
   - Where appropriate, propose an accompanying or follow-up update to the Sashiko-for-Sashiko prompts (`prompts/sashiko/`) or workflow (`src/workflows/sashiko_patch_review.rs`) so future reviews do not repeat the false positive.

## Coding Standards & AI Coding Agents

Detailed Rust coding standards, architectural rules, and LLM workflow design principles are documented in [GEMINI.md](GEMINI.md) (also linked via [AGENTS.md](AGENTS.md)).

We also provide specialized agent skills under `skills/` to automate development workflows:

- **`review-pr`** (`skills/review-pr/SKILL.md`): Performs deep code reviews against [GEMINI.md](GEMINI.md) and design documents (`designs/`), generating categorized findings with suggested diffs.
- **`sashiko-feature`** (`skills/sashiko-feature/SKILL.md`): Guides end-to-end feature implementation, from design document creation and codebase investigation to iterative `make check-pr` and self-review verification.
