# Git, Filesystem, and Subprocess Boundaries

Check every subprocess status and stderr path. Ensure stdin is closed when the
child expects end-of-file, timeout paths kill and reap the child, and output
cannot grow without a bound appropriate to the command.

Git operations run against shared repository state. Preserve protocol
restrictions, safe argument separation, worktree locks, exact commit identity,
and cleanup of only Sashiko-owned paths. A temporary directory dropping does
not by itself clean Git's worktree metadata.
