+++
title = "Tests and Samples"
paths = [
    "samples/",
    "tests/",
]
+++

# Tests and Samples

Applies to Twister test suites and example applications.

Source: doc/contribute/contributor_expectations.rst; doc/develop/twister/.

- A test for a bug fix must fail without the fix. Flag a test that cannot detect
  the bug it claims to cover, for example because it asserts nothing about the
  fixed behavior.
- Every sample and test is built through `sample.yaml` or `testcase.yaml`. Check
  that the metadata matches what the change needs: for example, a test requiring a
  Devicetree node or a Kconfig option should restrict its platforms or filters so
  it is not built where it cannot work.
