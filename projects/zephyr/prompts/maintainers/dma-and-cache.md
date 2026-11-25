+++
title = "DMA Buffers and Cache Coherence"
paths = [
    "drivers/dma/",
    "drivers/cache/",
]
symbols = [
    "__nocache",
    "dma_config",
    "dma_reload",
    "dma_start",
    "k_mem_phys_addr",
    "sys_cache_data_*",
]
+++

# DMA Buffers and Cache Coherence

Applies when memory is shared with a DMA engine or another bus master on a system
with data caches.

Source: doc/hardware/cache/guide.rst.

- Data moved by DMA can be stale in the cache. The document gives several
  approaches: the data cache disabled, a region marked uncached in Devicetree with
  buffers placed by `Z_GENERIC_SECTION()`, `__nocache`, or cache maintenance at
  runtime. Report a DMA buffer only after confirming it uses none of them.
- With runtime maintenance, flush with `sys_cache_data_flush_range()` after the CPU
  writes data a peripheral will read, before starting the transfer. A write-through
  cache does not need this flush.
- Trace when a buffer becomes reachable by the ISR or DMA engine: storing a
  shared pointer, submitting a descriptor, or enabling a transfer can publish it.
  Judge cache maintenance order against that publication and the first device
  access. Its placement inside a lock is not by itself a correctness defect.
  Dismissing a race requires proving that every competing path cannot access the
  buffer until maintenance finishes; a pointer store alone does not prove that
  earlier descriptors or aliases cannot reach it.
- Invalidate with `sys_cache_data_invd_range()` a buffer a peripheral has written
  to, before the CPU reads it. Report a read of DMA data with no invalidate on a
  cached buffer, when the configuration can have data caches enabled.
- Buffers under runtime maintenance should be aligned to the cache line with
  `__aligned(CONFIG_DCACHE_LINE_SIZE)`. Invalidating a range that shares a line
  with unrelated data can destroy that data.
- `__nocache` requires `CONFIG_NOCACHE_MEMORY`. Check that the option is enabled
  for configurations using the attribute.
- On architectures where the MMU enables hardware data caches independently of
  `CONFIG_DCACHE` or `CONFIG_CACHE_MANAGEMENT` (inspect the target
  architecture's MMU and cache headers in the tree, such as ARM64 with
  `CONFIG_ARM_MMU`), `sys_cache_data_*()` can silently become a no-op or return
  `-ENOTSUP` while caches remain active; check that Kconfig guards keep cache
  maintenance active wherever DMA touches cached memory.
- Verify `k_mem_phys_addr()` preconditions in
  `include/zephyr/kernel/internal/mm.h` and `include/zephyr/sys/mem_manage.h`
  in the target tree: it only translates permanent linearly mapped kernel RAM
  (not dynamic `k_mem_map()` regions, which may not be physically contiguous),
  and under `CONFIG_MMU=n` with `CONFIG_ASSERT=y` it can assert on non-SRAM
  addresses before a subsequent driver range check runs.
- On transfer abort, timeout, or controller reset, halt in-flight hardware DMA
  and clear saved caller buffer pointers before returning buffer ownership to
  the caller. Masking interrupts does not stop active bus-master DMA, and a
  stale saved pointer later passed to `sys_cache_data_invd_range()` can corrupt
  freed or reused memory.
- Do not report cache maintenance on SoCs whose hardware has no data cache, or
  where the driver is built only for such SoCs (confirm via hardware capability
  symbols such as `CPU_HAS_DCACHE` and architecture MMU/cache setup in the tree,
  not `CONFIG_DCACHE=n` alone).
