+++
title = "Git Subprocess Spawning and Worktree Isolation Policy"
paths = [
    "src/baseline.rs",
    "src/git_cmd.rs",
    "src/git_ops.rs",
    "src/git_ops/",
    "src/prerequisites.rs",
    "src/repo_sync.rs",
]
symbols = [
    "GitWorktree",
    "detached_async",
    "git_cmd",
    "in_dir",
    "in_dir_async",
]
+++

## Mandatory `git_cmd` Constructors

- Never construct a git subprocess with `std::process::Command::new("git")` or
  `tokio::process::Command::new("git")` anywhere in `src/` outside
  `src/git_cmd.rs`. Always use `git_cmd::in_dir`, `git_cmd::in_dir_async`, or
  `git_cmd::detached_async`, which strip inherited `GIT_DIR`, `GIT_WORK_TREE`,
  and related environment variables so linked worktrees and git hooks cannot
  hijack repository operations.
- When passing revision strings, branch names, header values, or file paths
  derived from external input (patches, mboxes, webhooks, or LLM tool calls) to
  git subcommands, reject leading `-` flags or separate positional arguments
  with `--end-of-options` / `--` so values cannot be interpreted as git flags.
- Temporary git worktrees created for patch application or review must clean up
  both their filesystem directory and git's worktree administrative registration
  on drop or teardown, and serialize repository-level mutations that contend on
  git refs or index locks. Do not report a "leaked worktree directory on panic
  or cancellation" finding against callers of `GitWorktree::new` or
  `new_scratch_clone` unless the diff removes or bypasses `TempDir` ownership
  (for example via `TempDir::keep`, `disable_cleanup`, or `GitWorktree::from_path`).
