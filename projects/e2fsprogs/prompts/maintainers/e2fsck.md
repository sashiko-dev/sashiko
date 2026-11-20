+++
title = "e2fsck Filesystem Checker"
paths = ["e2fsck/"]
symbols = ["e2fsck_*", "fix_problem", "problem_context"]
+++
# e2fsck Filesystem Checker: maintainer notes

Repair safety, problem-table invariants, and corrupt-image resilience policy for
`e2fsck`.

- Never report filesystem corruption in `e2fsck` with raw `printf` or `fprintf`:
  define or reuse a `PR_<PASS>_*` code in `e2fsck/problem.h` and
  `e2fsck/problem.c` and invoke `fix_problem(ctx, code, &pctx)` so `-p` (preen),
  `-n` (no-change), and `-y` modes behave consistently.
- Entries in `problem_table` inside `e2fsck/problem.c` must remain strictly
  sorted in ascending order by problem code (`Medium` if unsorted, as unit tests
  reject out-of-order entries), and every `@`-expansion token in the prompt
  string must have its corresponding `problem_context` field populated before
  calling `fix_problem`.
- Be conservative with `PR_PREEN_OK`: automatic repairs under `e2fsck -p` must
  only perform safe, unambiguous metadata fixups and must never silently discard
  user file data or guess across ambiguous structural damage (`Critical` if
  automatic preen repair can destroy valid user data).
- Under `e2fsck -n` (`EXT2_FLAG_RW` cleared / `OPT_NO`), repair code paths must
  never mutate on-disk blocks or mark filesystem structures dirty for writeback.
- Corrupted or fuzzed images frequently contain cyclic extent trees, directory
  HTrees, extended attribute blocks, or orphan lists; verify traversal loops
  enforce depth or visited-block bounds rather than looping indefinitely
  (`Medium`).
