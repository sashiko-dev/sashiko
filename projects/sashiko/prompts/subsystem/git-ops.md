# Git Operations and Worktree Lifecycle Invariants

This guide covers `src/git_ops.rs` and worktree management in `src/reviewer.rs`
and `src/local_review.rs`.

Sashiko runs concurrent reviews inside temporary git worktrees created from a
shared repository (`settings.git.repository_path`). Unsynchronized git
operations or leaked worktrees can corrupt repository state or exhaust disk
space.

## 1. Worktree Lifecycle and Startup Cleanup

- **Creation (`GitWorktree::new`)**: Creates a detached worktree under
  `settings.review.worktree_dir` (prefix `sashiko-worktree-`) via
  `git worktree add --detach --no-checkout <tmp> <sha>` followed by
  `git reset --hard <sha>`.
  - **Why `--no-checkout` + `reset`**: Avoids race conditions and index lock
    contention during worktree creation on large trees like the Linux kernel.
- **Startup Wipe Hazard**: `Reviewer::new` wipes and recreates
  `settings.review.worktree_dir` at daemon startup (`cleanup_worktree_dir`).
  - **Multi-Instance Invariant**: Two Sashiko daemon instances (e.g., a `linux`
    instance and a `sashiko` instance) *must never* share the same
    `review.worktree_dir`. Starting the second instance would delete active
    worktrees out from under running reviews in the first instance.
- **Drop & Prune**: `GitWorktree` owns its directory through a
  `tempfile::TempDir` (`dir: Option<TempDir>`), whose `Drop` runs
  `remove_dir_all` on the directory. The on-disk worktree directory is
  therefore reclaimed on every exit path, including an early `return`, a
  panic that unwinds the task, or a dropped/cancelled Tokio future.
  `impl Drop for GitWorktree` (and `GitWorktree::remove`) takes `self.dir`
  for both managed worktrees and unmanaged `new_scratch_clone` instances and
  offloads `dir.close()` to `spawn_blocking` when a runtime is available
  (logging a warning on any non-`NotFound` `std::io::Error`), so the
  recursive delete of a Linux-sized tree never blocks the async worker that
  dropped the future; `Drop` cannot run the async `git worktree remove`, so
  what a dropped managed worktree leaves behind is the administrative entry
  under `$GIT_DIR/worktrees/<name>`, which `prune_worktrees` removes at
  daemon startup. Do NOT report a "leaked
  worktree directory on panic or cancellation" finding against callers of
  `GitWorktree::new`/`new_scratch_clone` unless the diff removes or bypasses
  the `TempDir` ownership (for example by calling `TempDir::keep` or
  `disable_cleanup`, or by constructing the worktree with
  `GitWorktree::from_path`). Explicit `.remove().await` calls on error paths
  are preferred so the git registration is cleaned up eagerly, but their
  absence on a panic/cancellation path is a stale `$GIT_DIR/worktrees` entry,
  not a disk-space leak. Any early error return during worktree setup must
  still ensure the git worktree registration is cleaned up.

## 2. Git Protocol Restrictions (`GIT_PROTOCOL_RESTRICTIONS`)

When fetching from remotes or applying patches from untrusted sources:
- **Protocol Allowlist**: Git commands that may trigger network or submodule
  activity must pass `-c protocol.allow=never` or explicitly restrict protocols
  via `GIT_PROTOCOL_RESTRICTIONS` (`protocol.file.allow=never`, etc.) to prevent
  malicious `.gitmodules` or crafted refs from triggering arbitrary local file
  clones or SSRF.

## 3. Patch Application (`apply_patch`)

- **Synthetic Identity**: `git am` requires author/committer environment
  variables (`GIT_AUTHOR_NAME`, `GIT_COMMITTER_EMAIL`, etc.). `apply_patch` sets
  these explicitly to `"Sashiko Bot <sashiko@localhost>"` so patch application
  never fails on hosts without a global `~/.gitconfig`.
- **Clean Abort**: When `git am` fails to apply a patch, `git am --abort` must
  be executed before reusing or inspecting the worktree.

## 4. Repository Maintenance Locking (`gc` / `repack` / `commit-graph`)

Background workers (`src/worker/repack.rs`, `src/worker/sync.rs`) periodically
fetch remotes and rebuild the commit-graph.
- **Auto-GC Disabled**: `ensure_gc_disabled` sets `gc.auto=0` on the repository
  so concurrent `git` invocations during a review never trigger a spontaneous
  background `git gc` that locks or deletes loose objects while a worker is
  reading them.

## Checklist for Diffs Touching `src/git_ops.rs`

1. **No Shell Execution**: Are all `git` invocations built with
   `Command::new("git").args(...)` rather than shell interpolation?
2. **Option Separation**: Are commit hashes, ref names, or paths separated by
   `--` where appropriate so a ref starting with `-` cannot be parsed as a git
   flag?
3. **Cleanup on Failure**: If a multi-step git operation fails halfway, is the
   worktree or `git am` session cleaned up before returning `Err`?
4. **Worktree Isolation**: Does the change respect `worktree_dir` isolation and
   avoid mutating the base repository's `HEAD` or working tree?
