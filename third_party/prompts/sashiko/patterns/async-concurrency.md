# Async and Concurrency Boundaries

Check Tokio task ownership, channel capacity and closure, shared-state locking,
deadline propagation, cancellation, and shutdown ordering. A task that owns a
resource must either be awaited or have an explicit cancellation and cleanup
path. Never hold a synchronous or async mutex across unrelated slow work unless
the protected invariant requires it.

For races, name both operations, their owners, the shared state, and an actual
interleaving. Verify whether a repository, remote, worktree, patchset, quota, or
database lock already serializes that interleaving before reporting it.
