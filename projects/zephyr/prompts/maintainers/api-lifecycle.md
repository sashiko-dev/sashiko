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

Source: doc/develop/api/api_lifecycle.rst.

External issue metadata and Architecture working group approvals may be absent
from a local checkout. Treat them as unknown unless the supplied context can
establish compliance or a violation; a missing record in Git does not prove a
missing approval.

- Experimental APIs may change, and unstable APIs may be removed without
  deprecation.
- A breaking change to a stable API needs an RFC issue labeled Breaking API Change,
  and the pull request needs an entry in the API Changes section of the release
  notes. Check the release notes in the tree. Report a missing RFC issue or label
  only when the supplied context establishes its absence; otherwise leave that
  external requirement unassessed.
- Deprecating a stable API takes at least two full releases, unless the Architecture
  working group approves a shorter period. The API is marked with `__deprecated` or
  `__DEPRECATED_MACRO`, or through a Kconfig option that restores its previous form.
  The deprecation is documented and added to the release notes, and in-tree users
  are migrated in the same change.
- Flag new in-tree use of an API marked deprecated.
- A change to an API behavior contract also updates its doxygen, including the
  `@retval` list and qualifiers such as `@isr_ok`.
