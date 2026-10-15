# Linux Kernel Domain and Subsystem Contracts

Invariants specific to the Linux kernel environment, covering device lifecycle and devres rules, hardware interaction and power management, execution contexts and locking hierarchies, RCU and lockless concurrency, and subsystem contracts across networking, VFS, memory management, and UAPI.

Derived by `sashiko notes agenda` from 99995 fixes reachable from 3a8660878839faadb4f1a6dd72c3179c1df56787 (81580 code bugs, 104 themes).

## DRV: Device Lifecycles, Hardware Access, Bindings, and PM

- DRV-1: In what exact sequence must memory, locks, queues, and hardware be initialized—and stray interrupts masked—before registering device interfaces, and how is reverse teardown ordered?
- DRV-2: Which resources are managed by device-managed allocation (devres) versus manual deallocation or reference-counted release callbacks, and how is mixing them prevented across unbind?
- DRV-3: How are hardware capabilities, silicon revisions, and firmware flags probed before advertising features or accessing optional registers, and how are quirks scoped to exact device IDs while keeping shared static tables const?
- DRV-4: How do Device Tree, ACPI, and driver match tables enforce schema compliance, balanced node references, active-low GPIO polarity, sentinel termination, and non-init section placement?
- DRV-5: What hardware programming rules govern MMIO register widths, endianness, serialized read-modify-write, write-1-to-clear bits, posted-write flushes, and bounded polling timeouts?
- DRV-6: How are runtime PM, clock, regulator, and reset references balanced across all paths, and how are DMA, workers, and wake IRQs quiesced and restored across sleep and resume phases?

## CTX: Execution Contexts, Locking, RCU, and Architecture ABI

- CTX-1: What execution context (process, hardirq, softirq, NMI, RCU read-side, spinlock, raw spinlock, or memory reclaim) applies to each callback, and how are sleeping calls, blocking allocations, and user-memory accesses prevented in atomic contexts?
- CTX-2: Which locks are shared with interrupt handlers or bottom halves (requiring IRQ/BH-saving or raw spinlock variants), and how is preemption disabled during per-CPU state access?
- CTX-3: What is the global lock ordering hierarchy across subsystem, VFS, MM, and PM locks, and how are synchronous workqueue flushes, timer cancellations, and waitqueues coordinated without deadlocking?
- CTX-4: Where objects or lists are accessed under RCU or locklessly, how are read-side critical sections matched, non-zero reference increments enforced, and grace periods awaited after unlinking?
- CTX-5: How do DMA mappings, scatterlists, and page table walks maintain address space boundaries (virtual, physical, DMA, highmem), cacheline alignment, and ordered cache/TLB invalidations?
- CTX-6: What architecture ABI, inline assembly clobber/constraint, unwind metadata, CPU hotplug mask, and `noinstr` rules govern low-level entry, virtualization, and code-patching paths?

## SUBSYS: Networking, VFS, MM, and UAPI Subsystem Contracts

- SUBSYS-1: What mandatory callbacks, vtable immutability rules, parameter units (bytes vs pages/sectors, bit numbers vs masks), and Kconfig/export guards are required by the target subsystem framework?
- SUBSYS-2: In networking paths, how are SKB linearization, clone unsharing, headroom/tailroom, NAPI budgets, XDP frame lifetimes, GSO/checksum offloads, and network namespace/VRF scoping maintained?
- SUBSYS-3: In VFS, block, and filesystem paths, how are on-disk metadata validated, bio splits and queue freezes balanced, and pagecache folios (head vs tail pages, variable orders) synchronized with writeback and truncation?
- SUBSYS-4: Across UAPI boundaries (ioctl, netlink, sysfs, procfs, syscalls), how are untrusted lengths and offsets validated with speculation barriers, 32-bit compat handled, and structure padding zeroed to prevent infoleaks?
- SUBSYS-5: How are privilege checks and resource accounting bound to the opening or submitting task's credentials, user namespaces, and cgroups rather than current worker context?
- SUBSYS-6: How does error handling distinguish genuine kernel bugs from recoverable runtime, hardware, or user-induced errors so that warnings are not triggered and hot-path logs are ratelimited?
