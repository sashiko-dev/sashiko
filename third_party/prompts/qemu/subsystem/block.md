# QEMU Block Layer and Storage Review Guidelines

## Core Principles
The QEMU block layer manages virtual disk images, block drivers (qcow2, raw, nbd), and I/O request queues using coroutines and asynchronous event loops.

## Critical Invariants to Verify

1. **Coroutine Safety**:
   - Block I/O functions that yield (`bdrv_co_*`, `qemu_coroutine_yield`) must be executed within coroutine context (`assert(qemu_in_coroutine())`).
   - Do NOT hold standard POSIX/glib mutexes across a coroutine yield; use `QemuCoMutex`.

2. **AioContext Locking and Thread Safety**:
   - Each `BlockDriverState` can belong to an `AioContext` (associated with an IOThread).
   - Manipulating block graphs, attaching children, or submitting jobs across threads requires proper context switching or lock management.

3. **Draining and Device Removal**:
   - Before detaching a block device or destroying its state, in-flight I/O requests must be completely drained using `bdrv_drained_begin()` / `bdrv_drained_end()`.
   - Never free private device structures while asynchronous block operations or mirror/backup jobs are running.
