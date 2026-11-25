+++
title = "Tests and Samples"
paths = [
    "samples/",
    "tests/",
]
+++

# Tests and Samples

Applies to Twister test suites and example applications.

Source: doc/contribute/contributor_expectations.rst; doc/develop/test/ztest.rst;
doc/develop/twister/index.rst.

## Test rigor and mutation sensitivity

- A test for a bug fix must fail without the fix. Flag a test that cannot detect
  the bug it claims to cover: for example, negative-only assertions with no
  positive control proving valid inputs succeed, a `testcase.yaml` or `prj.conf`
  that compiles out the fixed code (such as `CONFIG_SMP=n` when fixing an SMP or
  inner-shareable broadcast defect), a concurrency or fallback branch that never
  actually executes, or a test that relies on a Twister timeout instead of
  failing fast.
- Suites must be independent of test order and repetition under
  `CONFIG_ZTEST_SHUFFLE=y` and `CONFIG_ZTEST_REPEAT=y`
  (`doc/develop/test/ztest.rst`): reset mutated state in `before` (or
  `ZTEST_RULE`) and unregister or free suite-level resources in `teardown`
  matching `setup`.
- Avoid priority inversions between cooperative and preemptible test threads
  that prevent intended overlap, use explicit synchronization rendezvous rather
  than `k_sleep()` guesses, and cancel or flush work items and join worker
  threads before test stack-local synchronization objects go out of scope
  (including on assertion failure paths).

## Twister metadata, userspace tests, and samples

- Every sample and test is built through `sample.yaml` or `testcase.yaml`. Check
  that the metadata matches what the change needs: for example, a test requiring a
  Devicetree node or a Kconfig option should restrict its platforms or filters so
  it is not built where it cannot work, and tests for SMP fixes should include
  SMP targets.
- Tests that intentionally trigger fatal CPU exceptions must set
  `ignore_faults: true` in `testcase.yaml`.
- Prefer `-DFILE_SUFFIX` over hardcoded board overlay paths when selecting
  scenario overlays, and use `K_APPMEM_PARTITION_DEFINE` / `K_APP_BMEM` (or
  `ZTEST_BMEM` / `ZTEST_DMEM`) rather than hand-aligned buffers for userspace
  test memory.
- Do not add board-specific overlays or `integration_platforms` entries to
  generic hardware-agnostic samples (`Low` severity).
