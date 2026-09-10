# Async and Concurrency Boundaries

Check Tokio task ownership, channel capacity and closure, shared-state locking,
deadline propagation, cancellation, and shutdown ordering. For a long-lived
task, or one whose cancellation can strand external state or a child process,
identify its supervisor or shutdown owner and prove the cleanup path. A bounded
detached task is not a leak merely because its JoinHandle is dropped. Never hold
a synchronous or async mutex across unrelated slow work unless the protected
invariant requires it.

For races, name both operations, their owners, the shared state, and an actual
interleaving. Verify whether a repository, remote, worktree, patchset, quota, or
database lock already serializes that interleaving before reporting it.
