+++
title = "Sashiko General Maintainer Preferences"
paths = ["*"]
symbols = []
+++

## Commit Message Hygiene

- Every commit MUST carry a valid real-name `Signed-off-by: Full Name <email>`
  trailer matching the commit author or committer (`git commit -s`). Flag
  missing sign-offs, single-word handles/nicknames, or bot/placeholder
  identities.
- Commit messages must never use markdown backticks (`` ` ``) to quote code,
  symbols, types, functions, or file names, and must not contain internal
  metadata tags (such as `TAG=` or `CONV=`).
- Do NOT nitpick minor commit message line-length overruns (such as 73–80
  characters) and allow reasonable exceptions for URLs, file paths, or quoted
  compiler/log lines; only flag genuinely unwrapped prose lines that exceed ~85
  characters.
- Only standard git trailers (`Signed-off-by`, `Fixes`, human `Co-authored-by`,
  `Reviewed-by`, `Acked-by`, `Tested-by`, `Reported-by`, `Link`) are permitted.
  Reject any `Assisted-by:`, `Generated-by:`, `AI-assisted-by:`, or AI/LLM
  `Co-authored-by:` trailers.
- When a commit repairs a defect introduced by an identifiable earlier commit
  already in repository history (prior to `Baseline SHA`), require a
  `Fixes: <12-char sha> ("<subject>")` trailer immediately before
  `Signed-off-by:` (and `Fixes: <project>-<uuid>` on a separate line when
  fixing a tracked Sashiko bug). Do not demand `Fixes:` on new features,
  refactorings, documentation/prompt/config changes, or commits where no
  introducing commit prior to `Baseline SHA` can be identified with confidence,
  and never ask for a `Fixes:` trailer naming an unmerged commit in the same
  series.
- Never reference Sashiko bugs by internal SQLite row IDs (`Bug #<id>`); always
  use the public `<project>-<uuid>` identifier.

## Benchmark Evidence Policy

- Require benchmark evaluation results or targeted patch re-review results in
  the commit message ONLY when a change can meaningfully affect overall Linux
  AI patch-review or initial bug-discovery quality across the board (for
  example, changes to `projects/linux/prompts/*.md` procedure guides,
  `projects/linux/prompts/maintainers/general.md`, `linux_patch_review.rs`
  detection/classification heuristics, `linux_bug.rs` discovery/verification
  stages, generic workflow graph semantics, or model parameters).
- Do NOT demand benchmark data or manual test logs in commit messages for:
  - ordinary Rust code, CLI, UI, or bug-fix commits where correctness is clear
    from the diff and tests;
  - changes to Sashiko's own self-review prompts (`projects/sashiko/prompts/`,
    `src/workflows/sashiko_patch_review.rs`);
  - subsystem maintainer notes (`projects/*/prompts/maintainers/*.md`) that
    record subjective subsystem preferences, conventions, or narrowly-scoped
    policies;
  - generated code-notes snapshots (`projects/*/prompts/notes/<set>/`);
  - structural lineage/provenance bookkeeping (`source_ids`, `review_map`) or
    telemetry plumbing that preserves bug-detection heuristics;
  - post-triage maintenance or filtering of pre-existing issues out of patch
    review reports.
- Never report that a prompt or workflow commit failed to modify or add entries
  in `projects/*/benchmarks/*.json` (benchmark JSON files are static
  ground-truth corpora of known positive bugs, not negative test suites).

## Rust Coding and Target Environment Conventions

- Sashiko exclusively targets Linux/Unix environments; never report Windows or
  non-Unix portability concerns as issues.
- Never guess or report compilation, borrow-checker, type-check, formatting, or
  clippy lint failures; deterministic build checks across feature profiles are
  enforced by `make check-pr`.
- Prefer type-driven state (`enum`s, newtypes) over raw string state values.
- Avoid `.unwrap()` and `.expect()` in production code paths (outside
  `#[cfg(test)]`) unless the invariant is statically proven infallible (such as
  a constant regex or a lock that cannot be poisoned in context) and documented.
