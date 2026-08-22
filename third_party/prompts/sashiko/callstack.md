# Sashiko Call-Path Analysis

Trace the complete path affected by the patch rather than stopping at the
modified function.

For inbound work, follow parsing and authentication through queue submission,
persistence, patch extraction, baseline/worktree creation, review execution,
and result publication. For outbound work, trace the returned error and state
changes back to the operator-visible API or log.

At each async boundary record:

- who owns the task, channel, child process, temporary directory, and database
  transition;
- which values cross the boundary and whether they remain tied to the same
  repository, patchset, base, head, and review attempt;
- what happens on cancellation, receiver closure, timeout, retry, and partial
  success;
- whether cleanup is awaited and whether a later retry can safely repeat it.

Read concrete callers and callees before dismissing a concern. A comment or
expected deployment topology is not proof that a path is unreachable.
