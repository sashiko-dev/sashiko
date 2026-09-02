# QEMU VirtIO Subsystem Review Guidelines

## Core Principles
VirtIO implements standardized para-virtualized devices (net, block, scsi, gpu, console) communicating via shared memory virtqueues.

## Critical Invariants to Verify

1. **VirtQueue Element and Descriptor Verification**:
   - Verify descriptors using `virtqueue_pop()`:
     - Check descriptor chain length against `VIRTQUEUE_MAX_SIZE` (prevent infinite loops from cyclic descriptor chains).
     - Check `elem->in_num` and `elem->out_num` before indexing `elem->in_sg` or `elem->out_sg`.
   - Never write status or response bytes to an `in_sg` buffer without verifying that `iov_len` is sufficiently large:
     ```c
     if (elem->in_sg[idx].iov_len < sizeof(struct response_header)) {
         // handle error: truncate or drop request safely
     }
     ```

2. **Ring Buffer and Index Bounds**:
   - Guest controls `avail_idx` and `used_idx`.
   - Ensure difference computations between available and used indices use modulo arithmetic (`uint16_t` wrap-around) properly.

3. **Hotplug and Unrealize Teardown**:
   - `virtio_cleanup()` and `virtio_del_queue()` must be called during device unrealization.
   - Any in-flight asynchronous requests in bottom halves, worker threads, or block jobs must be completed or canceled before freeing VirtIODevice state.
