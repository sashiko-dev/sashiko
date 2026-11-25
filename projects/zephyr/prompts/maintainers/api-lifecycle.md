+++
title = "Public API Lifecycle"
paths = [
    "include/zephyr/",
]
symbols = [
    "__DEPRECATED_MACRO",
    "__deprecated",
]
+++

# Public API Lifecycle

Applies to changes in public headers. Check the API's status before applying
these rules, because they differ by status. The `@version` tag in the header
records it: `0.1.z` is experimental, `0.y.z` with `y` above 1 is unstable, and
`1.0.0` or later is stable.

Source: doc/develop/api/api_lifecycle.rst; doc/develop/api/overview.rst;
doc/releases/index.rst.

External issue metadata and Architecture working group approvals may be absent
from a local checkout. Treat them as unknown unless the supplied context can
establish compliance or a violation; a missing record in Git does not prove a
missing approval.

- Experimental APIs may change, and unstable APIs may be removed without
  deprecation.
- For stable APIs (`@version >= 1.0.0`, `doc/develop/api/overview.rst`), check
  that the header's `@version` (`X.Y.Z`) follows SemVer: backwards-compatible
  bug fixes increment `Z`, backwards-compatible additions or deprecations
  increment `Y` and reset `Z` to `0`, and breaking changes increment `X`.
- Long-standing public headers under `include/zephyr/` that were shipped across
  previous releases (`git_log`) without an explicit `@version` tag still require
  scrutiny before breaking function signatures or semantics; flag unannounced
  breaking signature changes (`Medium` or `Low` depending on blast radius).
- A breaking change to a stable API needs an RFC issue labeled Breaking API Change,
  and the pull request needs an entry in the API Changes section of the release
  notes. Check the active draft version in `doc/releases/index.rst`
  (`doc/releases/release-notes-<X.Y>.rst` and
  `doc/releases/migration-guide-<X.Y>.rst`, never an already-released file).
  User-visible behavioral changes, tightened static limits, or new Kconfig
  requirements also belong in the current draft migration guide and release
  notes. Report a missing RFC issue or label only when the supplied context
  establishes its absence; otherwise leave that external requirement unassessed.
- Deprecating a stable API takes at least two full releases, unless the Architecture
  working group approves a shorter period. The API is marked with `__deprecated` or
  `__DEPRECATED_MACRO`, or through a Kconfig option that restores its previous form.
  The deprecation is documented and added to the release notes, and in-tree users
  are migrated in the same change.
- Flag new in-tree use of an API marked deprecated.
- A change to an API behavior contract also updates its doxygen, including the
  complete `@retval` list, qualifiers such as `@isr_ok`, caller-side context
  constraints, accurate `@ingroup` membership, and backend-specific differences
  on shared architecture or subsystem headers.
