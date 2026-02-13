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

## Quick Checks

- **New shared files**: When a commit creates a file that is sourced or
  imported by test scripts, verify it is added to `TEST_FILES` in the Makefile
- **`TEST_PROGS` vs `TEST_FILES`**: Executable tests go in `TEST_PROGS`;
  supporting files go in `TEST_FILES`. Mixing these up causes either execution
  failures or missing installations
- **KVM IRQ chip tests**: When tests use `KVM_IRQFD`, `KVM_IRQ_LINE`, or IRQ
  routing, verify `vm_create_with_one_vcpu()` is used and
  `TEST_REQUIRE(kvm_arch_has_default_irqchip())` is present
