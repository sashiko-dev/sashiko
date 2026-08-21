# I/O Accessors Subsystem Details

## MMIO Accessor Semantics and Endianness

Using inconsistent I/O accessor variants when accessing the same FIFO or buffer
causes data corruption on big-endian systems. The bulk transfer may appear
correct while partial/remainder transfers produce corrupted data due to
unwanted byte-swapping.

Linux provides two families of MMIO accessors with different endianness
behavior (defined in `include/asm-generic/io.h`, architectures may override):

**Register I/O (with byteswapping):**
- `readl()` / `writel()` - 32-bit
- `readw()` / `writew()` - 16-bit
- `readb()` / `writeb()` - 8-bit
- Behavior: Perform CPU-to-device endianness conversion via
  `__cpu_to_leN()`/`__leN_to_cpu()` wrappers around `__raw_readN()`/`__raw_writeN()`,
  plus memory barriers (`__io_br()`/`__io_ar()` for reads, `__io_bw()`/`__io_aw()`
  for writes). On big-endian CPUs, these byteswap to produce little-endian data
  for the device register.
- Use case: Hardware control/status registers with defined bit layouts

**FIFO/Stream I/O (without byteswapping):**
- `readsl()` / `writesl()` - 32-bit stream
- `readsw()` / `writesw()` - 16-bit stream
- `readsb()` / `writesb()` - 8-bit stream
- Behavior: Map directly to `__raw_readN()`/`__raw_writeN()` (e.g.,
  `__raw_readl()`, `__raw_writew()`) and preserve byte order between memory
  and FIFO. No byteswapping and no per-access barriers regardless of CPU
  endianness.
- Use case: Data FIFOs, DMA buffers, stream-oriented hardware

Code that uses `writesl()` or `readsl()` for bulk FIFO transfers but
`writel()` or `readl()` for remainder/partial transfers to the same FIFO
address is an endianness portability bug — the remainder bytes get
byte-swapped on big-endian systems while the bulk data does not. See
`i3c_writel_fifo()` and `i3c_readl_fifo()` in `drivers/i3c/internals.h`
for the correct pattern.

```c
// WRONG: Mixed accessor semantics
writesl(fifo_addr, buffer, len / 4);
if (len & 3) {
    u32 tmp = 0;
    memcpy(&tmp, buffer + (len & ~3), len & 3);
    writel(tmp, fifo_addr);  // BUG: byteswaps on big-endian
}
```

```c
// CORRECT: Consistent FIFO semantics
writesl(fifo_addr, buffer, len / 4);
if (len & 3) {
    u32 tmp = 0;
    memcpy(&tmp, buffer + (len & ~3), len & 3);
    writesl(fifo_addr, &tmp, 1);  // Consistent: no byteswap
}
```

The same pattern applies to reads:

```c
// WRONG
readsl(fifo_addr, buffer, len / 4);
if (len & 3) {
    u32 tmp = readl(fifo_addr);  // BUG: byteswaps on big-endian
    memcpy(buffer + (len & ~3), &tmp, len & 3);
}
```

```c
// CORRECT
readsl(fifo_addr, buffer, len / 4);
if (len & 3) {
    u32 tmp;
    readsl(fifo_addr, &tmp, 1);  // Consistent: no byteswap
    memcpy(buffer + (len & ~3), &tmp, len & 3);
}
```

## Quick Checks

- **Bulk vs remainder accessor consistency**: When code handles bulk transfers
  separately from partial/remainder transfers, verify both use the same I/O
  accessor family (`writesl`/`readsl` vs `writel`/`readl`).
- **FIFO identification**: Determine if the target address is a FIFO/buffer
  (stream data) or a register (control/status). FIFOs should use stream
  accessors exclusively.
- **Big-endian testing**: Flag FIFO helper functions that mix accessor types
  as likely to fail on big-endian architectures.
