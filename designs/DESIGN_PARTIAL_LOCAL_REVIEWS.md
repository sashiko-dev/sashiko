# Partial local reviews

## Problem

An error in one patch's workflow currently aborts the local review result. The
collector also discards completed patches because it propagates the first patch
error before building the report. A failed consolidation stage can leave useful
analysis concerns in workflow state, but the worker returns only an error. The
terminal then prints no report.

## Result contract

The `sashiko review` command returns a result for every attempted patch. A
completed patch retains its normal findings and inline review. A failed patch
carries its review error and status in the `patches` array; the combined result
is marked partial. Concerns still awaiting verification are placed in
`unverified_concerns`, separate from verified `findings`. The combined result
has `partial: true` and a top-level error summary when any patch failed. Its
text and JSON modes both print the combined result; the command still exits with
its error status.

An incomplete review never reports "No issues found" for the failed patch. The
daemon's existing error handling continues to treat a partial worker result as a
failure, so unfinished findings are not automatically published.

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

## Continue after a consolidation error

Deduplication and conflict resolution are recoverable once analysis has produced
concerns. A failed stage records its error and places its input concerns in
workflow state as unverified candidates. If deduplication fails, conflict
resolution is skipped because it requires deduplicated input. The verification
stage receives the union of normal patch concerns and these unverified
candidates. A successful verification clears the candidates and produces
findings; a failed verification leaves the candidates visible in the partial
report. Once verification completes, the local runner returns the partial result
without restarting the whole patch. A report-stage failure retains verified
findings.

The failed stage and any stage that requires its missing output are skipped.
The workflow continues through every later stage whose input is available.  The
result remains marked partial and keeps its error status even if a later
validation stage succeeds. A failure before analysis has produced concerns still
ends that patch's workflow, and other patches continue independently.

## Validation

Unit tests cover worker state extraction after a stage error, retry snapshot
selection, and the incomplete progress state. `make check-pr` must pass before
the implementation is committed.
