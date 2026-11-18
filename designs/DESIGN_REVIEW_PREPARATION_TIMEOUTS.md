# Bounded and observable review preparation

## Problem

The daemon marks a claimed patchset `In Review` before selecting a baseline and applying its patches. The AI execution timeout starts later. Some preparation operations, including worktree checkout and patch application, can therefore wait indefinitely while holding a review permit. Baseline logs are saved only after preparation returns, leaving no public indication of the unfinished stage.

## Scope

Bound preparation independently of AI execution, retry preparation timeouts with a fresh worktree, and publish structured progress as preparation runs. Keep baseline ordering, prerequisite precedence, ordinary application failures, AI timeouts, and AI retry counts unchanged. This is a safeguard against unbounded preparation, not a claim to identify the cause of any particular production hang.

## Deadline and retries

Add `review.preparation_timeout_seconds`, defaulting to 1800 seconds per attempt, and `review.preparation_max_retries`, defaulting to three additional attempts, matching the AI retry default. The timeout must be positive. A separate budget avoids coupling repository preparation to model latency and AI retries. The conservative default accommodates slow kernel checkouts and initial remote fetches; operators can increase it for larger repositories.

Start the deadline before loading preparation inputs and resolving candidate baselines. It covers prerequisite resolution, remote and commit resolution, worktree creation, patch application, and recording the selected baseline. Each timeout records the stage that was interrupted. After bounded cleanup, check that the patchset claim is still active, wait with bounded backoff, and retry the complete preparation with a new worktree. Do not retry ordinary application failures or restart an AI review that has already begun.

On exhausted preparation retries, finish the patchset as `Failed`, with a fixed timeout explanation, rather than `Failed To Apply`. A preparation error remains distinguishable from a series that cannot apply to any candidate. Cancellation or a newer claim must prevent further attempts and stale status/progress writes. An ordinary unsuccessful baseline search retains its existing `Failed To Apply` outcome.

## Cancellation and cleanup

Separate worktree allocation from population so the attempt owner retains the worktree while cancellable Git operations run. During daemon preparation, Git commands own process groups so cancellation also stops their hooks and transports. Observe process exit concurrently with pipe I/O, terminate any remaining descendants before reaping the leader, and never signal a reaped process ID. Keep interactive Git calls outside this scope in the caller's foreground group. A timed-out future is dropped before cleanup starts, so the interrupted operation is not resumed on a retry. Never reuse a partially applied checkout.

Allow at most 120 seconds for cleanup, including the worktree metadata lock. Pause the preparation deadline during per-candidate cleanup so it cannot truncate this separate budget. Cleanup failure or timeout stops retries and produces a fixed public cleanup-failure reason. Attempt metadata removal even if directory deletion fails, while retaining the original failure. Filesystem cleanup already delegated to the blocking pool may finish later; do not claim that an operating-system I/O stall is synchronously interruptible. Existing startup cleanup remains the last-resort recovery mechanism.

Shared remote configuration and reference mutations must finish with their locks still held even if the preparation attempt times out, because killing a writer can strand Git's lockfiles. Timestamp mutations likewise retain the remote mutex until the filesystem work completes. Shield these mutations in owned tasks registered with the preparation attempt, and wait only for that attempt's tasks during cleanup before retrying. Remove the worktree before waiting for shared writes so a slow writer cannot prevent checkout cleanup. If an owned writer does not finish within the cleanup budget, stop retries rather than deleting a potentially live lockfile. Resolve remote HEAD through a separately bounded read-only query outside configuration locks, then perform the local mutation under the configuration lock. Own this entire post-fetch finalization through timestamp publication so cancellation cannot leave fetched refs without their default HEAD while a retry skips fetching because refs already exist. This preserves local-ref reuse without adding routine network lookups to that path.

Query the symbolic remote HEAD first, requesting branch advertisements only for transports that provide an object ID without a branch name. Bound captured stdout and stderr to 1 MiB each and limit advertisement parsing to 16,384 records. Preserve ref names as bytes through parsing and Git invocation, so non-UTF-8 names do not discard or corrupt a valid HEAD. On daemon shutdown, send isolated shared Git writers and their hooks SIGTERM so Git's lockfile-cleanup handlers can run; ordinary cancellable process groups retain their SIGKILL policy.

## Public progress

Use typed stages for loading inputs, selecting baselines, resolving prerequisites, fetching a baseline, resolving a commit, checking a baseline, creating a worktree, applying prerequisites, applying target patches, recording a baseline, and cleanup. Record the current attempt and attempt limit, the attempt and stage start timestamps, timeout duration, retry time when applicable, terminal outcome, and earlier timed-out stages.

Store a structured preparation entry alongside the existing entries in `baseline_logs`. Existing API responses already carry this field, so no schema migration or new endpoint is required. Publish on stage transitions and retry/terminal transitions, not on every timer tick. Bound progress writes so an observability failure cannot suspend preparation indefinitely. Preserve completed baseline attempts and timeout history across retries.

The web UI shows preparation progress separately from baseline attempt results, including elapsed stage time, attempt count, backoff, and timeout history. It must not render an interrupted or superseded run as actively running when the patchset has left `In Review`. Continue polling while the patchset is queued or `In Review`: reruns pass through `Pending`, and a terminal preparation entry may precede baseline/status publication or belong to the previous run. Refresh baseline details when their metadata changes, without rebuilding an unchanged page on every poll. Preparation failures are labelled as review preparation failures, not ingestion failures. Older baseline-log arrays continue to render normally.

New public diagnostics are generated only from fixed stage/status labels and numeric metadata. Do not interpolate Git stdout/stderr, command lines, local paths, exception messages, or arbitrary remote data. This does not change the existing baseline diagnostic policy. All writes remain fenced by the patchset claim generation and existing embargo protections continue to apply.

## Implementation and validation

Keep this design in a separate commit. Implement cancellation-safe worktree ownership, then the bounded preparation/retry driver and its progress/UI integration in self-contained commits.

Use synthetic repositories and controlled stalled operations for regression tests. Cover timeout followed by successful retry, retry exhaustion, zero retries, ordinary application failure without timeout retries, fresh worktrees, killed Git children, bounded cleanup failure, cancellation/supersession, claim-fenced progress, retained timeout history, and fixed public messages. Test configuration defaults and rejection of a zero timeout. Verify legacy and current UI log rendering. Run `make check-pr` and local Sashiko self-review before proposing the PR. Do not commit production patch examples or diagnostic artifacts.

## Design review

An attempt deadline bounds the complete preparation path instead of relying on every nested call to have its own timeout. Separate cleanup ownership avoids losing the partially created worktree when that deadline expires. Fresh retries avoid replaying patches over a dirty checkout. Typed progress and fixed diagnostic strings provide useful public evidence without exposing process output. Claim-fenced writes prevent an old timeout or retry from overwriting a replacement run. Cleanup has its own small budget and cannot silently extend every preparation retry indefinitely.
