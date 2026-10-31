+++
title = "Zephyr Repository-Wide Maintainer Conventions"
paths = [
    "*",
]
+++

# Zephyr Repository-Wide Conventions

Cross-cutting rules that apply to every Zephyr change.

## Commit format and provenance

Source: doc/contribute/guidelines.rst (Commit Message Guidelines, DCO Sign-Off);
.gitlint.

- A commit title has the form `area: summary`, is shorter than 72 characters, and
  is followed by a blank line. Use `git_log` with the changed file in the `paths`
  array to check area prefixes in its history. Flag an area that does not match
  the code changed. gitlint rejects `subsys:` and `treewide:` as the leading area.
- The body must not be empty and should say what changed, why this approach was
  chosen, what was assumed, and how it was tested. Flag a body that only restates
  the title.
- Every commit carries a `Signed-off-by:` trailer with the author's real name and
  email; flag a missing one.
- Each commit must stand alone: doc/contribute/contributor_expectations.rst
  requires that every commit in a pull request builds and passes the relevant
  tests. Flag a commit that depends on a later commit to build.

## C style and language restrictions

Source: doc/contribute/style/code.rst; doc/contribute/coding_guidelines/index.rst.

- Style follows the Linux kernel coding style with Zephyr's exceptions, such as
  braces on every `if`, `else`, `do`, `while`, `for`, and `switch` body, and a
  100-column limit. CI enforces these; at most Low severity, and only for new
  code. The guide does not require fixing existing code the change does not touch.
- Rule A.1: function and structure declarations in headers are not conditionally
  compiled. Fields inside a structure may be, to save memory. This keeps
  `if (IS_ENABLED(CONFIG_FOO))` usable.
- Rules A.4 and A.5 restrict which C library functions kernel and embedded code
  may call. Host tooling and native-only test code are exempt from Rule A.5.

## Tests and documentation

Source: doc/contribute/contributor_expectations.rst.

- Pull requests include tests or samples when adding a feature, changing an API
  behavior contract, or fixing a hardware-agnostic bug. For a bug fix, the test
  fails without the fix and passes with it. Flag missing coverage only for these
  cases, at most Low severity.
- A functional change must update the documentation it affects. A new API must
  come with an example of its use.
