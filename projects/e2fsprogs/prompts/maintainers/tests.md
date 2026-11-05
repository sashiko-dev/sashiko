+++
title = "Regression Test Suite"
paths = ["tests/"]
symbols = []
+++
# Regression Test Suite: maintainer notes

Conventions and false-positive prevention for `e2fsprogs` regression tests under
`tests/`.

- Test scripts at `tests/<prefix>_<name>/script` are **sourced** (not executed
  in a subshell) by `tests/test_one`: calling `exit 0` or `exit 1` terminates
  the entire test runner prematurely (`Medium`). Always require `return 0` when
  skipping a test (for example when an optional feature or platform prerequisite
  is unavailable) and `unset` temporary shell variables at the end of `script`.
- Test scripts generating images programmatically with `$MKE2FS` should pass
  `-F -o Linux` and avoid host-locale, timezone, UID, or page-size assumptions
  that make golden `expect` comparisons nondeterministic across build hosts
  (`Medium`).
- Prefer small programmatic `$MKE2FS` + `$DEBUGFS` reproducers over large binary
  `image.gz` blobs, and never report the intentionally corrupted filesystem
  structures constructed inside `tests/` as bugs.
