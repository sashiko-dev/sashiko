# Severity Levels

Assign a severity to every finding. Take it seriously: critical must mean
critical, and high must mean very damaging. Use Medium as the default and move
up or down from there against the definitions below.

Sashiko is not an operating system kernel. It is a service that reads untrusted
patches, spends money on model calls, writes to a database, and sends mail to
public mailing lists under its own name. Calibrate against *those* consequences,
not against memory corruption.

## Calibrating the level (reason before you label)

State this reasoning at the start of `severity_explanation` so the label is
auditable.

- **Consequence**: what actually happens when the bug triggers. Irreversible
  external side effects outrank internal ones. A wrong row in the database can
  be fixed; an email sent to a public list cannot be unsent.
- **Triggering path**: the concrete path that reaches the bug, naming the
  preconditions an input or caller must satisfy. If you cannot lay one out
  because it rests on an assumption you might be misreading, still report the
  finding and mark it speculative.
- **Reachability**: raise the level if the bug is reachable from content
  Sashiko does not control — a patch from a mailing list, a pull request, a
  webhook payload, or a model's own output. Do not lower a finding because you
  believe it is unreachable: reachability is hard to establish from a diff, and
  a wrong call buries a real bug.

A speculative finding is the one case where the level is capped, at Medium,
because the open question is whether the bug is real at all. It is always
reported, never dropped. That is the only reason to lower a level. Reachability
never is.

## Critical

- **Definition**: irreversible external damage, loss of data, or a security
  boundary that stops holding.
- **Question to ask**: if this triggers, is there anything we can do afterwards
  to undo it? If no, it is critical.
- **Examples**:
    - Anything that can send mail that policy should have muted, widen the
      recipient set, or defeat `dry_run` — this is the highest-consequence
      failure in the system.
    - Posting review content publicly that should have been withheld, including
      an embargo that stops being honoured.
    - A capability check missing or bypassable on a mutating endpoint;
      `read_only` mode not actually preventing a mutation.
    - A secret (API token, webhook secret, local operator token, JWT signing
      key) reaching a log, an error message, an LLM prompt, or the database.
    - Loss or corruption of review, finding or bug data, including a migration
      that destroys or mangles existing rows.
    - Untrusted patch content escaping its fence: prompt injection that can
      steer which files are inlined into a prompt, path traversal out of the
      worktree or prompt directory, or command injection into a git invocation.
    - A feedback loop that can generate unbounded outbound mail or model calls.

## High

- **Definition**: the service stops working, or produces materially wrong review
  output, and a human has to intervene.
- **Question to ask**: with non-trivial probability, does this take the service
  down, wedge it, or make its output untrustworthy?
- **Examples**:
    - A panic, deadlock or hang on a path a normal review reaches. A worker is a
      subprocess and a panic there is recoverable; a panic in the daemon is not.
    - A review silently producing no findings when it should have produced some
      — for example a workflow stage whose output is dropped, a prompt file that
      no longer resolves, or an early exit on the wrong condition. Silence is
      the failure mode this system is least able to notice about itself.
    - A migration that is not backward compatible, or that is not idempotent and
      so fails on re-run.
    - Losing work: a review that completes but is not persisted, an outbox row
      that is never delivered and never retried, or one delivered twice.
    - Unbounded growth in tokens, memory, worktrees, child processes or rows,
      where an operator would have to step in.
    - Breaking a public interface without a compatibility path: a `Settings`
      key removed or renamed under `deny_unknown_fields`, a CLI flag renamed, an
      API response field removed, or a change to the shape of JSON already
      stored in the database.
    - Missing benchmark validation data for a change that can meaningfully affect
      overall AI review quality, detection rate, or false-positive rate across the
      board (e.g. global prompts, stage instructions, workflow graph, planner
      logic, or verification/deduplication rules).

## Medium

- **Definition**: real and worth fixing, but recoverable and contained.
- **Examples**:
    - A resource leaked on an error path that a restart clears.
    - A retry that is not idempotent where the duplicate is harmless.
    - Degraded review quality: a prompt or schema change that makes a stage
      vaguer, an ambiguous field name, a missing escape hatch in an enum, a
      stage given less context than its task needs.
    - An output produced by a stage and consumed by nothing.
    - Incorrect metrics, counts or progress reporting.
    - The commit message and the code disagreeing in a way that would mislead
      the next reader.
    - Missing `Signed-off-by` trailer or using a cryptic nickname/handle instead
      of a real human name.
    - Missing or inadequate commit description (fails to explain *what* and
      *why* for a non-trivial change).
    - Missing test coverage for behaviour the change introduces.
    - A performance regression a user would notice but work around.

## Low

- **Definition**: no visible effect on behaviour.
- **Question to ask**: is there any real-world effect? If no, it is low.
  Otherwise it is at least medium.
- **Examples**:
    - Typos in comments or user-facing strings.
    - Confusing naming, or a comment that no longer matches its code.
    - Commit message formatting violations: genuinely unwrapped prose lines
      exceeding ~85 characters (do not flag 73-80 char lines or reasonable
      exceptions like quoted code, URLs, or paths), backticks quoting code or
      symbols in the commit message, or internal metadata tags (`TAG=`, `CONV=`).
    - Missing documentation.
    - Negligible performance differences.

> Build/compilation errors (syntax, missing imports, unresolved symbols/types,
> type mismatches, borrow-checker/lifetime errors, missing trait bounds, or
> non-exhaustive `match` arms on closed enums), Rust source formatting, import
> ordering, and anything `cargo check`, `cargo test`, `cargo fmt`, or `cargo
> clippy` reports on source files are not findings at all. They are verified
> deterministically before a human ever sees them — never vibe-guess build bugs.
> However, commit message issues (missing real-name `Signed-off-by`, missing
> description of what/why, unwrapped prose lines > 85 chars, backticks in commit
> message) are NOT checked by `cargo fmt` and MUST be reported.
