# QEMU False Positive Elimination Guide

To avoid noise and focus strictly on genuine defects, verify against the following rules before raising a concern.

## 1. Do Not Flag Specification-Mandated Behavior
- Many hardware devices specify that unimplemented registers read as zero or ignore writes.
  Do not flag `case UNIMPLEMENTED_REG: return 0;` or `qemu_log_mask(LOG_UNIMP, ...)` as bugs.
- Devices that clamp guest values to architectural limits (e.g. `val = MIN(val, MAX_ENTRIES)`)
  are correctly adhering to device specifications.

## 2. Abort on Initialization vs Runtime Failures
- Calling `abort()`, `g_assert()`, or `error_setg(&error_fatal, ...)` during machine creation
  or device initialization (before the VM starts) is acceptable for impossible internal configurations.
- Flag aborts and assertions ONLY if they can be triggered at runtime by guest action (MMIO, DMA, guest packets, or hotplug).

## 3. Opaque Pointers and Context Lifetimes
- In `MemoryRegionOps` callbacks (`read`, `write`), the `void *opaque` argument is populated
  at initialization time by `memory_region_init_io()`. It is guaranteed non-null for the lifetime
  of the device. Do not flag missing `if (!opaque)` checks.
- Similarly, in timer and BH callbacks, `opaque` is set at timer creation and is non-null.

## 4. Migration Compatibility and Subsections
- Subsections that send new state only when non-default are intentional compatibility mechanisms.
  Do not flag a missing field in the top-level `VMStateDescription` if it is properly encapsulated in a subsection.

## 5. Pre-existing Code Boundary Rule
- Only report issues that are introduced or worsened by the proposed patch.
  If an existing function already had an issue and the patch merely renames or refactors surrounding code
  without altering the logic or triggering condition, mark it as `preexisting: true`.
