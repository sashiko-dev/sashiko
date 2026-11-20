+++
title = "Guest Input Validation and Device Security Boundary"
paths = [
    "hw/",
]
symbols = [
    "MemoryRegionOps",
    "dma_memory_read",
    "dma_memory_read_relaxed",
    "dma_memory_write",
    "dma_memory_map",
    "pci_dma_read",
    "pci_dma_write",
]
+++

# Guest Input & Device Security

QEMU is a security boundary between guest and host. Everything the guest can
put into a register, a descriptor, or a queue is attacker-controlled input.

## The invariant

Every value that originates from the guest MUST be validated before it is used
as:

- an **index** into an array or a ring (`entry = &ring[guest_idx]`),
- a **size or length** for a copy, allocation, or loop bound,
- an **address** for a host-side access or a `dma_memory_read`/`dma_memory_write`,
- a **selector** into behavior that must not be reachable (privilege checks,
  feature-gated commands the device should reject).

"Validated" means an explicit check with an explicit rejection or clamp on the
same path, not an assumption about what the guest driver would ever send.

## Concrete patterns to check

- MMIO handlers: range checking of offsets, alignment handling, and RO/W1C
  semantics of register bits. Reads of uninitialized state, writes to
  read-only bits.
- PCI: BAR programming and config space writes reaching device logic unchecked.
- Descriptor rings: per-descriptor length, buffer address, and queue index
  bounds; chained/indirect descriptors validated against queue size, not
  assumed from the driver.
- Integer arithmetic on guest values: `addr + len` overflowing; truncating casts
  from 64-bit guest values; signed/unsigned confusion in length comparisons.
- Crypto or security-relevant device code: secret-dependent branches on guest
  data, side-channel considerations.
- Guest-triggerable `assert`/abort: an assertion the guest can reach is a host
  denial-of-service, not a safety property.

## What this is not

Host-internal control flow that no guest value can influence is not this
guide's subject; trace it under the normal correctness rules instead.
