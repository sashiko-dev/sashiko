+++
title = "General"
paths = ["*"]
symbols = []
+++
# General: maintainer notes

Generic review guidance for the binutils-gdb repository.

- Commit subjects use a component prefix followed by a colon (bfd:, gas:, ld:, gdb:, gdbserver:, sim:, binutils:) and describe the change, not the bug report.
- Do not flag missing ChangeLog entries: current practice relies on the git log, and ChangeLog files are no longer updated for every change.
- Formatting and whitespace are covered by the project style; report them only when they hide a behavior change.
- A behavior change needs a DejaGnu test under the matching testsuite/ directory. Name the missing case; do not ask generically for more tests.
- Do not ask for a NEWS entry unless the change is user-visible and the maintainers' convention requires one for that component.
- Keep the patch focused: unrelated refactoring mixed into a fix is worth a comment only when it obscures the fix or changes behavior.
- Never suggest weakening an assertion or a verifier check to make a test pass; they encode invariants the rest of the component relies on.
