# Sashiko Patch Review Protocol

Review the proposed change as a regression analysis of Sashiko, an async Rust
service that ingests untrusted patch and forge data, manages Git repositories
and worktrees, invokes subprocesses, persists review state, and coordinates AI
providers.

For every finding:

1. identify the changed function and the concrete triggering path;
2. inspect relevant callers, callees, configuration defaults, and cleanup;
3. distinguish an introduced regression from pre-existing behavior;
4. prove the consequence with code rather than a hypothetical concern;
5. check tests for the boundary that actually failed;
6. report the exact file and symbol without inventing a line number.

Preserve established Linux-kernel behavior unless the patch explicitly and
safely changes it. Do not require external model calls for ordinary tests.
