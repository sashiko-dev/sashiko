+++
title = "Zephyr Repository-Wide Maintainer Conventions"
paths = [
    "*",
]
+++

# Zephyr Repository-Wide Conventions

Cross-cutting rules that apply to every Zephyr change.

## Commit format, series structure, and provenance

Source: doc/contribute/guidelines.rst (Commit Message Guidelines, Licensing,
Copyright and License Notices, DCO Sign-Off);
doc/contribute/contributor_expectations.rst; .gitlint.

- A commit title has the form `area: summary`, is shorter than 72 characters, and
  is followed by a blank line. Use `git_log` with the changed file in the `paths`
  array to check area prefixes in its history. Flag an area that does not match
  the code changed. gitlint rejects `subsys:` and `treewide:` as the leading area.
- The body must not be empty and should concisely explain the problem, why this
  approach was chosen, and key assumptions or impact. Flag both a body that only
  restates the title and an overly verbose body that walks through mechanical
  implementation details already obvious from the diff (`Low` severity).
- Keep commit claims aligned with actual reachability and final code: state when
  a fix only affects test-only configurations (for example symbols with
  `depends on TEST` such as `CONFIG_IRQ_OFFLOAD`) or defensive hardening of an
  unreachable path, because maintainers rely on commit messages for backport
  decisions. Flag a `Fixes:` trailer or live-bug claim on purely defensive
  hardening, or a commit message that drifted out of sync with the final code
  after refactors.
- Do not reference PR review feedback (such as "address review comments") or
  leave `"cherry picked from commit ..."` trailers in commit messages (`Low`
  severity).
- Every commit carries a `Signed-off-by:` trailer with the author's real name and
  email; flag a missing or broken one at `High` severity.
- Each commit must stand alone and keep `git bisect` green: order a bug fix
  before the test that verifies it, squash incremental skeleton or fixup commits
  for a new driver or feature into cohesive commits, and split independent bug
  fixes or unrelated reformatting into separate commits.
- Zephyr uses Apache-2.0 (`SPDX-License-Identifier: Apache-2.0`). Flag files
  with incompatible license headers (such as GPL) or commits indicating code was
  copied from GPL-licensed sources without documented approval. Do not add new
  copyright lines for small fixes to large existing files (`Low` severity).

## Layering and C coding guidelines

Source: doc/contribute/style/code.rst; doc/contribute/coding_guidelines/index.rst;
doc/contribute/guidelines.rst.

- Drivers and SoC code must include public headers under `include/zephyr/`
  rather than reaching into private kernel headers (`kernel/include/`) or
  internal architecture headers.
- Rule A.1: function and structure declarations in headers are not conditionally
  compiled. Fields inside a structure may be, to save memory. This keeps
  `if (IS_ENABLED(CONFIG_FOO))` usable.
- Rules A.4 and A.5 restrict which C library functions kernel and embedded code
  may call; avoid `<stdio.h>` input functions such as `sscanf()` in kernel,
  driver, or subsystem code because they break minimal libc builds. Host tooling
  and native-only test code are exempt from Rule A.5.
- Rule 11.8: do not cast away `const` (for example `(void *)dev` passed as
  callback user data); report unforced `const`-dropping casts at `Low` severity,
  or `High` when the cast allows mutating read-only memory. Do not report a
  `(void *)` cast forced by an unchanged existing Zephyr API parameter type
  (such as `k_mem_phys_addr(void *)`) unless the callee mutates the pointed-to
  read-only memory (`High`).
- Rule 13.5: avoid expressions with persistent side effects (such as `atomic_*`
  updates or resource release calls) in the right-hand operand of `&&` or `||`,
  where short-circuit evaluation skips them.
- Rule 20.7: parenthesize macro parameters in public and driver macros so
  compound argument expressions cannot bind unexpectedly.

## Tests and documentation

Source: doc/contribute/contributor_expectations.rst.

- Pull requests include tests or samples when adding a feature, changing an API
  behavior contract, or fixing a hardware-agnostic bug. For a bug fix, the test
  fails without the fix and passes with it. Flag missing coverage only for these
  cases, at most Low severity.
- A functional change must update the documentation it affects. A new API must
  come with an example of its use.
