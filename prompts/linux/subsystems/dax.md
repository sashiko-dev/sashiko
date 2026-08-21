# DAX Subsystem Details

## DAX Mapping and Fault Handling

Direct Access (DAX) bypasses the page cache to perform direct memory operations
on persistent memory and byte-addressable devices.

- **Page Cache Bypass**: DAX mappings do not use standard page cache pages.
  Code that assumes `page_to_pfn()`, `folio_page()`, or `struct page` metadata
  exists on all DAX mappings will fail for device-DAX or fsdax without page
  struct backing (e.g., `MEMORY_DEVICE_FS_DAX` vs `MEMORY_DEVICE_GENERIC`).
- **DAX Fault Handlers**: Filesystem DAX fault handlers (e.g., in `fs/dax.c`)
  manage mapping invalidation, write faults, and dirty tracking directly in the
  inode address space's XArray using exceptional entries.

## Synchronization and Locking

- **XArray Entry Locking**: DAX entries stored in the inode address space XArray
  use entry-lock bits to coordinate concurrent faults and truncate/punch-hole
  operations. Always use DAX entry locking helpers (`dax_lock_mapping_entry()`,
  `dax_unlock_mapping_entry()`) rather than raw pointer access.
- **Truncate and Hole Punch Coordination**: Unmapping and invalidating DAX ranges
  must hold the appropriate filesystem invalidate locks and flush pending
  entries before truncating extents.

## Persistence and Memory Ordering

- **Cache Flushing for Durability**: Writes to DAX mappings require explicit CPU
  cache flushing (e.g., `dax_flush()`, `memcpy_flushcache()`) and memory barriers
  to guarantee data is committed to the persistent domain before acknowledging sync I/O.
- **Dirty Page Tracking**: DAX dirty tracking operates on XArray entry flags.
  `dax_writeback_mapping_range()` traverses the dirty XArray entries to perform
  flushing during `fsync`/`sync`.

## Quick Checks

- Verify cache flushing occurs before returning success on synchronous write paths
- Verify DAX XArray entry locking is held during entry modifications
- Verify huge page (PMD/PUD) fault handlers properly split or invalidate entries on size mismatch
- Do not assume `struct page` exists or is valid for all DAX mappings without checking the mapping type
