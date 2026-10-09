# Severity Levels

Assign a severity to every finding. Take it seriously: `Critical` must be
reserved for catastrophic failures, and `High` must mean severe operational or
review-quality breakage. Use `Medium` as the default for real functional defects
and `Low` for defensive hardening, rare edge cases, and hygiene.

Sashiko is not an operating system kernel. It is a service that reads untrusted
patches, spends money on model calls, writes to a database, and posts reviews to
public mailing lists and forges under its own identity. Calibrate against
*those* consequences, not against theoretical perfection.

## Calibrating the level (reason before you label)

State this reasoning at the start of `severity_explanation` so the label is
auditable.

- **Blast radius and irreversibility**: What actually happens when the bug
  triggers, and how many users or repositories are affected? A single stale
  status badge or an occasional redundant comment on one PR is an annoyance
  (`Low` or `Medium`), whereas flooding public mailing lists with unmuted mail
  or corrupting stored review history is catastrophic (`Critical`).
- **Likelihood and preconditions**: Name the concrete path and preconditions
  required to trigger the bug. Do not automatically upgrade a finding's severity
  merely because the code path processes external patches, pull requests, or
  webhooks — virtually all of Sashiko processes external patches. Only escalate
  for untrusted input when an attacker or malformed payload can actively cross a
  security boundary, cause denial-of-service, or corrupt unrelated state. If a
  bug requires a narrow race window, an abnormal database state, or an unlikely
  coincidence, reflect that lower probability in the severity (`Medium` or
  `Low`).
- **Speculative findings**: If a finding rests on an unverified assumption, mark
  it speculative and cap its severity at `Medium` (or `Low` if the impact is
  minor).
- **Non-primary LLM providers**: Gemini, Anthropic (Claude), and OpenAI are the
  primary production model providers. Bugs, limitations, or feature gaps
  affecting only other or secondary LLM backends must be capped at `Medium` at
  most.

## Critical

- **Definition**: Severe security vulnerability, persistent data loss or
  corruption, or broad external disruption affecting many developers or mailing
  lists at once.
- **Question to ask**: Does this compromise security, permanently corrupt or
  destroy important data, or risk sending a significant volume of wrong/unwanted
  emails or disrupting many developers simultaneously? If no, it is **not**
  `Critical`.
- **Examples**:
    - Bypassing email delivery safeguards (`dry_run`, mute lists, recipient
      filtering, or opt-out policies) or creating a feedback loop that sends
      unwanted emails at scale to public mailing lists or developers.
    - Exposing secrets (API keys, webhook secrets, operator tokens, signing
      keys) in logs, error messages, LLM prompts, or API responses.
    - Remote code execution, command injection into `git` or shell invocations,
      path traversal escaping the worktree, or authentication/authorization
      bypasses on mutating endpoints.
    - Destructive database bugs: migrations or queries that permanently delete
      or corrupt existing review, patch, or finding data across the database.
    - Widespread cross-tenant or cross-project contamination that actively
      disrupts or misleads many developers using Sashiko at the same time.

## High

- **Definition**: Major functional breakage where the core service crashes,
  wedges, or systematically fails to deliver trustworthy reviews for standard
  workflows, requiring operator intervention.
- **Question to ask**: Under realistic operating conditions, does this take the
  daemon down, wedge the review queue, or systematically break review output on
  primary code paths?
- **Examples**:
    - An unhandled panic, deadlock, or infinite hang in the main server daemon
      or on a standard review path.
    - Systematic silent failure of the review pipeline on primary models
      (Gemini, Anthropic, OpenAI) — e.g. a workflow stage whose output is
      dropped or an early exit that causes valid patches to be marked reviewed
      with zero findings.
    - Non-idempotent or backward-incompatible database migrations that prevent
      the service from starting or upgrading cleanly.
    - Unbounded resource exhaustion (runaway token spend, leaked worktrees,
      unbounded child processes, or retry storms).
    - Breaking a public configuration (`Settings`), CLI flag, or API contract
      without a compatibility path.
    - Missing validation evidence in the commit message (neither benchmark suite
      results nor targeted patch re-review results for false-positive prompt
      refinements) for a change that can meaningfully affect overall Linux AI
      review quality, detection rate, or false-positive rate across the board
      (e.g. Linux prompts in `third_party/prompts/`, stage instructions,
      workflow graph, planner logic, or verification/deduplication rules). Note:
      `benchmarks/*.json` are static positive-bug suites and are never modified
      by prompt or workflow commits; do not flag a missing diff in `benchmarks/`.

## Medium

- **Definition**: Contained functional bugs, localized race conditions,
  incorrect state transitions for individual items, or issues in secondary
  integrations.
- **Examples**:
    - Localized race conditions or state-transition bugs affecting a single
      patchset or pull request (e.g. a status field not updating cleanly under
      concurrent worker execution, or a single superseded review comment not
      being suppressed).
    - Any defect, crash, or compatibility issue specific to LLM providers other
      than Gemini, Anthropic, and OpenAI.
    - Degraded prompt clarity, schema ambiguity, or missing enum escape hatches
      that reduce review quality without breaking the pipeline.
    - Resource leaks on rare error paths that clear on restart, or non-fatal
      error-handling omissions (`let _ = ...` where failure should be logged or
      propagated).
    - Commit hygiene and test gaps: missing `Signed-off-by`, missing commit
      rationale on non-trivial code changes (note: self-explanatory config,
      deployment, mailing-list, or email-policy additions do not need extra
      rationale), or missing unit test coverage for new behavior.

## Low

- **Definition**: Defensive coding improvements, improbable edge cases, minor
  cleanup omissions, or stylistic/documentation issues with negligible
  real-world impact.
- **Question to ask**: Does triggering this require extreme/unrealistic inputs
  (e.g. `i64::MIN` timestamps), or is the consequence limited to a minor
  cosmetic or bookkeeping discrepancy? If yes, use `Low`.
- **Examples**:
    - Defensive arithmetic or input hardening where real-world inputs will not
      trigger the failure (e.g. theoretical integer overflow on extreme dates).
    - Minor cleanup omissions when cancelling or deleting items (e.g. leaving an
      auxiliary outbox or metadata row in a terminal state that causes no
      external side effect).
    - Dead outputs, unused fields, confusing naming, stale comments, or typos.
    - Commit message formatting violations: genuinely unwrapped prose lines
      exceeding ~85 characters (do not flag 73-80 char lines or reasonable
      exceptions like quoted code, URLs, or paths), backticks quoting code or
      symbols in the commit message, or internal metadata tags (`TAG=`, `CONV=`).

> Build/compilation errors (syntax, missing imports, unresolved symbols/types,
> type mismatches, borrow-checker/lifetime errors, missing trait bounds, or
> non-exhaustive `match` arms on closed enums), Rust source formatting, import
> ordering, and anything `cargo check`, `cargo test`, `cargo fmt`, or `cargo
> clippy` reports on source files are not findings at all. They are verified
> deterministically before a human ever sees them — never vibe-guess build bugs.
> However, commit message issues (missing real-name `Signed-off-by`, missing
> description of what/why on non-trivial changes, unwrapped prose lines > 85
> chars, backticks in commit message) are NOT checked by `cargo fmt` and MUST
> be reported.
