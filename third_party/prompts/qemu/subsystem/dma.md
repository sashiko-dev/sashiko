# QEMU DMA and AddressSpace Review Guidelines

## Core Principles
Direct Memory Access (DMA) allows simulated devices to transfer data to and from guest RAM.

## Critical Invariants to Verify

1. **Guest-Specified Buffer Bounds**:
   - The guest specifies DMA source/destination addresses and transfer lengths.
   - Devices that accumulate guest data into an internal buffer before processing MUST verify:
     ```c
     if (len > sizeof(s->internal_buf)) {
         // handle error: reject transfer or clamp length
         return;
     }
     ```
   - Never pass unvalidated guest lengths into `dma_memory_read()` or `dma_memory_write()`.

2. **Mapping and Unmapping Symmetry**:
   - `dma_memory_map()` returns host-accessible memory for a guest physical address range.
   - Every successful map MUST be accompanied by a matching `dma_memory_unmap()` call.
   - If an error occurs midway through a multi-buffer transfer, all previously mapped segments must be unmapped on the error exit path.
   - Verify the `dir` parameter (`DMA_DIRECTION_TO_DEVICE` vs `DMA_DIRECTION_FROM_DEVICE`) matches across map and unmap calls.

3. **Scatter-Gather Lists (SGL)**:
   - When processing `QEMUSGList`, verify that `sgl->size` matches the expected byte count.
   - Ensure `qemu_sglist_destroy()` is invoked when freeing DMA requests to avoid memory leaks.
