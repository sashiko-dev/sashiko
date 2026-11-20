# Userspace Filesystem Library and Utility Domain Invariants

Domain invariants for userspace filesystem libraries, consistency checkers,
offline/online transformation tools, and FUSE drivers.

## ONDISK: On-Disk Format, Feature Flags, Checksums, and Allocation Units

- ONDISK-1: Which compatible, incompatible, and read-only-compatible feature flags gate each on-disk field or structure format, and what fallback applies when a feature flag is absent?
- ONDISK-2: Which metadata structures carry per-object checksums seeded from the filesystem UUID, and in what exact order must fields and checksums be updated before serialization?
- ONDISK-3: Where do interfaces operate in logical blocks, physical blocks, or allocation clusters, and how are unit conversions kept consistent across bitmaps, extents, and free-space counters?

## ABI: Shared-Library Compatibility and Error Tables

- ABI-1: Which headers, functions, and structures form the public shared-library ABI that must preserve binary layout, calling conventions, and symbol behavior across releases?
- ABI-2: How are library error codes allocated, ordered, and returned without writing directly to standard output/error streams or shifting existing numeric error values?

## REPAIR: Consistency Checking, Preen Safety, and Problem Reporting

- REPAIR-1: How are corruption findings routed through structured problem descriptors and context records so interactive, preen, and read-only modes make consistent repair decisions?
- REPAIR-2: Which repairs are safe to perform automatically during unattended preen runs versus requiring explicit operator confirmation because repair could discard user data?
- REPAIR-3: When multi-pass checking modifies inodes, extents, directories, or bitmaps in an earlier pass, how are in-memory bitmaps and summary counts reconciled in later passes?

## IOCHAN: I/O Channels, Caching, Undo Logging, and Flush Ordering

- IOCHAN-1: How do layered I/O channels (cached, undo-logged, sparse/image, read-only, and test channels) order writes, barriers, and flushes before closing a filesystem handle?
- IOCHAN-2: When an offline transformation fails mid-way, what undo log or journal rollback invariants allow the operator to restore the pre-operation device state?
