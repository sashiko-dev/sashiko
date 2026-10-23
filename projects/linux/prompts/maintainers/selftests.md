+++
title = "Selftests"
paths = ["tools/testing/selftests/"]
symbols = ["TEST_PROGS", "TEST_FILES", "TEST_GEN_FILES"]
+++
# Selftests: maintainer notes

Conventions and review expectations for new and modified kernel selftests under tools/testing/selftests/.

- Report results through the kselftest.h / kselftest_harness.h wrappers (ksft_print_header, ksft_set_plan, ksft_test_result_*, ksft_finished, ksft_exit_*) rather than hand-rolled printf pass/fail text or a bare exit()/return from main(); flag tests that do not.
- Use ksft_test_result_error() for setup/environment failures, keeping them distinct from failures of the behavior under test.
- Beyond a flat main() with sequential checks, prefer TEST()/TEST_F() with FIXTURE()/FIXTURE_SETUP()/FIXTURE_TEARDOWN() and the ASSERT_*/EXPECT_* operators over bespoke if-and-report blocks; use FIXTURE_VARIANT/FIXTURE_VARIANT_ADD instead of copy-pasting a test per parameter.
- Per kselftest policy, a missing prerequisite (config option, hardware feature, filesystem capability) is a skip, not a failure, and an unconfigured test should not break the top-level make run_tests for everyone else; a feature that is present but broken must still fail.
- Every skip must carry a reason string saying what was missing; flag silent skips.
- Reuse the subsystem's existing shared test helpers (e.g. mm/vm_util.h, net/lib.sh, or the subsystem's own *_util.h / lib.sh) instead of reimplementing sysfs/file I/O, namespace setup/teardown, polling loops or result reporting; flag tests that hand-roll something the shared helper already provides.
- For BPF selftests, use the documented DENYLIST mechanism (tools/testing/selftests/bpf/README.rst) rather than ad hoc per-architecture skips.
- Test the interface/contract rather than the current internal implementation detail, so the test does not need rewriting when the implementation changes.
- Prefer extending an existing test in the same directory with a new case or parameter over adding a new standalone test file for one narrow variant; ask the author whether the new file should be a case in the existing test.
- Flag hardcoded filenames, IPs or magic numbers where a loop, glob or table would cover future additions, and ask whether the specific value has a documented rationale.
- On patches fixing bugs in existing test code, the Fixes: tag must point at the commit that actually introduced the bug, not merely the commit that added the general area; CI bots on some lists (e.g. bpf) check this and flag wrong tags.
