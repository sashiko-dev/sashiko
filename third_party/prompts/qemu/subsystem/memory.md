# QEMU Memory Regions and MMIO Review Guidelines

## Core Principles
Guest MMIO read and write callbacks represent the primary attack surface between an untrusted guest OS and the host emulator.

## Critical Invariants to Verify

1. **Bounds Checking and Address Arithmetic**:
   - `addr` passed to read/write callbacks is an offset within the memory region.
   - Verify that `addr` is checked before indexing state arrays:
     ```c
     if (addr / 4 >= ARRAY_SIZE(s->regs)) {
         qemu_log_mask(LOG_GUEST_ERROR, "%s: out of bounds offset 0x%"HWADDR_PRIx"\n", __func__, addr);
         return 0;
     }
     ```
   - Beware of integer truncation when casting `addr` to `uint32_t` or `int`.
   - Ensure `addr + size` checks do not overflow 64-bit bounds.

2. **Access Sizes**:
   - Verify `MemoryRegionOps` specifies appropriate `.valid` and `.impl` size constraints:
     - `.valid.min_access_size` and `.valid.max_access_size`
     - `.impl.min_access_size` and `.impl.max_access_size`
   - If a device supports sub-word or byte access to 32-bit registers, verify that unaligned access handlers shift and mask correctly.

3. **Reentrancy and State Mutation**:
   - If an MMIO write handler triggers an interrupt, updates a timer, or performs synchronous I/O, ensure it does not corrupt device internal state if invoked reentrantly by another vCPU.
