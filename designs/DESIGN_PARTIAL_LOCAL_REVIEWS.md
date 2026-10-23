# Partial local reviews

## Problem

The collector already preserves completed patch reviews when other patches
fail. However, a failed patch's workflow can leave useful analysis concerns or
verified findings in state while the worker returns only an error. The local
report loses that failed patch's completed work.

## Result contract

The `sashiko review` command returns a result for every attempted patch. A
completed patch retains its normal findings and inline review. A failed patch
carries its review error and status in the `patches` array; the combined result
is marked partial. Concerns still awaiting verification are placed in
`unverified_concerns`, separate from verified `findings`. The combined result
has `partial: true` and a top-level error summary when any patch failed. Its
text and JSON modes both print the combined result; the command still exits with
its error status.

An incomplete review never reports "No issues found" for the failed patch.
Recovery is enabled only by the local Git review entry point. Daemon workers
retain their existing workflow, error propagation, and retry behavior; they do
not produce partial review results or publish unfinished findings.

## Local-only boundary

An explicit typed failure policy defaults to aborting the review. The internal
worker command used by the daemon keeps that default, including in agent mode.
The local Git review entry point opts into preserving partial results and
passes that policy through the worker configuration and review state.

The ordinary Linux and Sashiko workflow builders retain their original stages.
Only a local worker wraps verification, reporting, and summaries for recovery.
Dynamic post-verification stages consult the same policy before wrapping their
batches.
Workers using the default policy propagate execution errors directly, preserving
the daemon's existing failure handling. Local-only result fields are omitted
from daemon worker output.

## Verification progress and retry selection

Use a typed verification state: pending, post-verification with completed and
remaining case counts, or complete. Verification output starts tracking its
hard cases; each successful post-verification batch resolves its input cases.
Failed batches leave those cases pending. Successful verification that needs
no post-verification, or a clean analysis that needs no verification at all,
counts as complete.

The worker returns this progress directly as a Rust value. Retry selection keeps
the typed progress score alongside each partial payload instead of recovering
application state from JSON.

Rank partial attempts by verification phase before finding counts, completed
post-verification cases, and candidate counts. Raw concerns from an earlier
verification failure must not outrank a later consolidated attempt just because
it disproved false positives. Completed verification stops whole-review retries
even if report or summary generation fails. Locally skipped verification marks
clean Sashiko reviews complete before summary generation; summary errors use
the same recovery policy as report errors. Daemon stages remain unwrapped.

Agent output retains unverified candidates, including their reasoning, locations,
and patch association, separately from findings and pre-existing concerns.

## Flow

1. On an unrecoverable workflow error, the worker converts its current state
   into a `WorkerResult` containing the error and completed state. Recoverable
   consolidation errors follow the continuation path below.
2. A patch retry retains the most useful partial result until verification
   completes or retries are exhausted. The patch result keeps its error and
   partial marker.
3. The patch collector waits for all patch futures and aggregates both full
   and partial results. It annotates candidate concerns with patch index and
   subject, as it already does for verified findings.
4. Text output prints the error, verified findings, and a clearly labeled
   unverified concerns section. JSON output exposes the same fields.

## Continue after a validation error

Verification now consolidates the raw analysis results and resolves conflicts,
then sends hard cases to parallel post-verification stages. A failed verification
stage records its error and preserves its input concerns and dismissals as
unverified candidates. Without verification output, there are no hard cases to
dispatch. If a post-verification stage fails, its batch remains unverified while
other batches retain their findings and dismissals. A report-stage failure keeps
verified findings. Once verification and all post-verification batches complete,
the local runner returns the partial result without restarting the whole patch.

The failed stage and any stage that requires its missing output are skipped.
The workflow continues through every later stage whose input is available.  The
result remains marked partial and keeps its error status even if a later
validation stage succeeds. A failure before analysis has produced concerns still
ends that patch's workflow, and other patches continue independently.

## Validation

Unit tests cover worker state extraction after a stage error, retry snapshot
selection, and the incomplete progress state. Boundary tests exercise the same
failures under both policies: default workers return errors, local workers
retain partial results, and only local post-verification batches recover.
`make check-pr` must pass before the implementation is committed.
