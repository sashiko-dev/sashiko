# Selftests Subsystem Details

## Build System and Installation

When a new file is created in a selftests directory but not added to the
Makefile, tests fail with "No such file or directory" when run from an
installed location (via `make install`). Tests may appear to work when run
directly from the source tree because the file exists there.

The selftests build system uses several variables in each subsystem's Makefile
to control what gets installed:

| Variable | Purpose |
|----------|---------|
| `TEST_PROGS` | Executable test scripts that are run directly |
| `TEST_FILES` | Supporting files (libraries, data files, sourced scripts) |
| `TEST_GEN_FILES` | Generated binaries/files produced during build |
| `TEST_GEN_PROGS` | Generated executable test programs |

Key invariants:

- Any file referenced via `source <filename>` (bash) or `. <filename>` in
  test scripts must be added to `TEST_FILES`
- Any file referenced via `import <module>` (Python) in test scripts must be
  added to `TEST_FILES`
- Executable test scripts that are invoked directly go in `TEST_PROGS`
- Helper executables that are built during `make` go in `TEST_GEN_PROGS` or
  `TEST_GEN_FILES`

Common mistake: creating a new shared library or utility file (like
`_common.sh`, `utils.py`, `lib.sh`) that is sourced by test scripts but
forgetting to add it to `TEST_FILES`. The tests work in the source directory
but fail after `make install`.

## KVM Selftests: IRQ Chip Setup and `vm_create` vs `vm_create_with_one_vcpu`

Tests that use `KVM_IRQFD`, `KVM_IRQ_LINE`, or IRQ routing APIs after
`vm_create()` fail because `vm_create()` does not create vCPUs, and on arm64
VGIC finalization (`KVM_DEV_ARM_VGIC_CTRL_INIT`) requires all vCPUs to be
created first. On architectures without any in-kernel IRQ chip support (riscv,
loongarch), these ioctls fail with `-ENODEV`.

`vm_create(nr_runnable_vcpus)` allocates a VM and sizes memory for the given
number of vCPUs, but does **not** create any vCPUs. IRQ chip setup is
initiated during `vm_create()` via `kvm_arch_vm_post_create()`, but
finalization (via `kvm_arch_vm_finalize_vcpus()`) only happens in functions
that also create vCPUs, such as `vm_create_with_one_vcpu()` and
`__vm_create_with_vcpus()`.

`kvm_arch_has_default_irqchip()` returns whether the architecture sets up an
in-kernel IRQ chip by default:

| Architecture | Return value |
|--------------|-------------|
| x86 | `true` (creates IOAPIC/PIC/LAPIC via `vm_create_irqchip()`) |
| s390 | `true` |
| arm64 | `true` when GICv3 is supported and not disabled via `test_disable_default_vgic()` |
| riscv, loongarch | `false` (weak default in `lib/kvm_util.c`) |

Tests that need an in-kernel IRQ chip must:

1. Call `TEST_REQUIRE(kvm_arch_has_default_irqchip())` to skip on architectures
   that lack IRQ chip support.
2. Use `vm_create_with_one_vcpu()` (or `__vm_create_with_vcpus()`) rather than
   bare `vm_create()`, so that vCPUs are created and IRQ chip finalization
   completes before issuing IRQ-related ioctls.

```c
// WRONG: vm_create() does not create vCPUs or finalize the IRQ chip
vm = vm_create(1);
kvm_irqfd(vm, gsi, eventfd, 0);

// CORRECT: Skip unsupported architectures, then create VM with vCPU
TEST_REQUIRE(kvm_arch_has_default_irqchip());
vm = vm_create_with_one_vcpu(&vcpu, NULL);
kvm_irqfd(vm, gsi, eventfd, 0);
```

## The kselftest Harness and Result Protocol

`Documentation/dev-tools/kselftest.rst` covers running, installing and
contributing tests, and is the authority on the Makefile variables and the
per-directory `settings` timeout. It is supplied to you in the
`<in_tree_style_guides>` section, read from the tree under review; check
findings about how a test is wired in against that text rather than against
this guide.

One caveat about that document: its harness sections are `kernel-doc`
directives rather than prose, so the macros themselves are not in it. They live
in `tools/testing/selftests/kselftest_harness.h`, and the exit codes in
`tools/testing/selftests/kselftest.h`. Read those headers with `git_read_files`
when a finding turns on exactly what a macro does.

What follows is the part that decides whether a test reports the truth.

kselftests run in userspace and report results as TAP. The result of a test is
its **exit code**, and getting that wrong is the most common way a broken test
reports success.

| Exit code | Meaning | Constant |
|-----------|---------|----------|
| 0 | All tests passed | `KSFT_PASS` |
| 1 | A test failed | `KSFT_FAIL` |
| 2 | Test result is unknown | `KSFT_XFAIL` |
| 3 | Test was not run | `KSFT_XPASS` |
| 4 | Test was skipped | `KSFT_SKIP` |

The critical case: a test whose prerequisites are unavailable must exit with
`KSFT_SKIP` (4), **not** 0. Exiting 0 reports a pass for something that never
ran, which hides a regression for as long as the prerequisite stays missing.
For shell tests, `ksft_exit_skip` or an explicit `exit $ksft_skip` with
`ksft_skip=4` is the idiom.

Common reasons to skip rather than fail: not running as root, a required
`CONFIG_` option absent, a kernel feature not present on the running kernel,
missing hardware, or an unavailable dependency such as a specific `iproute2`
version.

### C tests: `kselftest.h` and `kselftest_harness.h`

`kselftest.h` provides the reporting API directly:

- `ksft_print_header()`, `ksft_set_plan(n)` -- emit the TAP header and plan.
  The plan count must match the number of results actually reported.
- `ksft_test_result_pass/fail/skip/xfail(fmt, ...)` -- one call per test.
- `ksft_exit_pass()`, `ksft_exit_fail()`, `ksft_exit_skip(fmt, ...)` -- exit
  with the right code and final TAP summary.
- `ksft_exit_fail_msg(fmt, ...)` for a fatal setup failure.

`kselftest_harness.h` provides a fixture-based framework instead, and manages
plan counting and exit codes itself. Its full macro list is in that header;
three properties matter for review:

- `ASSERT_*` aborts the test, `EXPECT_*` records a failure and continues. The
  same rule as KUnit applies: anything the following lines depend on must be an
  `ASSERT_*`, particularly a non-NULL check before a dereference.
- `FIXTURE_TEARDOWN` runs even when a test fails, so it is the right place for
  cleanup. Cleanup written at the end of the test body is skipped on the paths
  that need it most.
- `TEST_HARNESS_MAIN` supplies `main()`. A harness-based test that writes its
  own `main()` and calls tests directly bypasses the result reporting, so
  failures may not reach the exit code.

Do not mix the two styles: a file that includes `kselftest_harness.h` should
not also hand-roll `ksft_test_result_*` calls, since the harness is already
counting.

### Shell tests

Source `tools/testing/selftests/kselftest/ktap_helpers.sh` where available
rather than hand-printing TAP lines. Shell tests must still exit 4 to skip.
Note that `set -e` interacts badly with tests that check failing commands
deliberately, and that a cleanup `trap` is the only reliable way to release
resources when a test aborts partway.

## Per-Directory Support Files

A test directory carries more than its Makefile:

- **`config`** -- the `CONFIG_` fragment listing kernel options the tests need.
  A new test that depends on a config option must add it here, or automated
  runners will build a kernel that cannot run the test. This is frequently
  forgotten and produces mysterious skips or failures in CI.
- **`settings`** -- optional, holds `timeout=<seconds>` for suites that need
  longer than the default. A long-running test without it is killed and
  reported as a failure. `kselftest.rst` documents the current default; do not
  quote a number from memory.
- **`.gitignore`** -- generated binaries must be listed, or `git status` is
  dirty after a build.

## Portability and Environment

- Do not assume root. Check the capability actually needed and skip when it is
  absent, rather than failing.
- Do not hardcode page size, `HZ`, core count, architecture word size, or
  network interface names. Query them.
- Cross-compilation must work: respect `$(CC)`, `$(CFLAGS)` and `$(LDLIBS)`
  from the selftest Makefile machinery rather than invoking `gcc` directly.
- Tests must be runnable individually and in any order. State left behind
  (network namespaces, mounts, loaded modules, sysctl changes, created files)
  must be cleaned up on both the success and failure paths.
- Avoid `sleep` as synchronisation. Poll for the condition instead, with a
  bounded retry, or the test will flake on loaded machines.

## Quick Checks

- **New shared files**: When a commit creates a file that is sourced or
  imported by test scripts, verify it is added to `TEST_FILES` in the Makefile
- **`TEST_PROGS` vs `TEST_FILES`**: Executable tests go in `TEST_PROGS`;
  supporting files go in `TEST_FILES`. Mixing these up causes either execution
  failures or missing installations
- **KVM IRQ chip tests**: When tests use `KVM_IRQFD`, `KVM_IRQ_LINE`, or IRQ
  routing, verify `vm_create_with_one_vcpu()` is used and
  `TEST_REQUIRE(kvm_arch_has_default_irqchip())` is present
- **Skip vs pass**: A test that cannot run its prerequisites must exit 4
  (`KSFT_SKIP`), never 0. Exiting 0 reports a pass for a test that never ran
- **`EXPECT_*` before a dereference**: should be `ASSERT_*`, which aborts
- **New `CONFIG_` dependency**: must be added to the directory's `config`
  fragment, or CI builds a kernel that cannot run the test
- **Long-running suite**: needs `timeout=` in `settings`, or it is killed at 45
  seconds and reported as a failure
- **Cleanup on the failure path**: namespaces, mounts, modules and temporary
  files must be released when a test aborts, not only when it succeeds
- **Root assumed**: check the specific capability and skip when absent
