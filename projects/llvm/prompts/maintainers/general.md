+++
title = "General"
paths = ["*"]
symbols = []
+++
# General: maintainer notes

Generic code-quality, testing, and commit-message review guidance for
LLVM/clang changes.

- Do not flag formatting, naming, `const` placement, or include ordering: clang-format and the style guides cover them, and CI enforces them.
- Do not ask for a release note for internal refactoring, test-only changes, or a fix that produces no output change. Do ask for one when clang starts accepting or rejecting previously diagnosed code, gains a flag, or changes user-visible diagnostics.
- A change that fixes behaviour needs a regression test in the matching `test/` tree. Name the specific missing case instead of asking generically for tests.
- Keep one logical change per commit; call out unrelated refactoring mixed into a fix only when it obscures the fix or changes behaviour.
- Never suggest bypassing the verifier or the assertion infrastructure to make a test pass; assertions encode IR and AST invariants the rest of the compiler relies on.
- Check that the commit title and message describe the actual change, including the language mode or target it affects, and do not claim NFC when behaviour changes.
