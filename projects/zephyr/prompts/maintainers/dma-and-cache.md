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
- Invalidate with `sys_cache_data_invd_range()` a buffer a peripheral has written
  to, before the CPU reads it. Report a read of DMA data with no invalidate on a
  cached buffer, when the configuration can have data caches enabled.
- Buffers under runtime maintenance should be aligned to the cache line with
  `__aligned(CONFIG_DCACHE_LINE_SIZE)`. Invalidating a range that shares a line
  with unrelated data can destroy that data.
- `__nocache` requires `CONFIG_NOCACHE_MEMORY`. Check that the option is enabled
  for configurations using the attribute.
- Do not report cache maintenance on SoCs without a data cache, or where the driver
  is built only for such SoCs. Confirm that through Kconfig before reporting.
