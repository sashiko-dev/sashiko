# QEMU Concurrency, BQL, and Synchronization Guidelines

## Core Principles
QEMU utilizes a hybrid concurrency model: the Big QEMU Lock (BQL) protects device state and main-loop execution, while dedicated IOThreads, RCU, and coroutines handle parallel operations.

## Critical Invariants to Verify

1. **Big QEMU Lock (BQL) Invariants**:
   - `bql_lock()` / `bql_unlock()` (or `qemu_mutex_lock_iothread()` / `qemu_mutex_unlock_iothread()`).
   - Main-loop timers, bottom halves (BH), and MMIO callbacks execute under BQL unless explicitly running in an IOThread context.
   - Never perform blocking network calls or long-running computations under BQL, as this starves all guest vCPUs.
   - When calling device emulation logic from worker threads, assert or acquire BQL: `bql_lock()`.

2. **Bottom Halves (BH) and Timers**:
   - Bottom halves scheduled via `qemu_bh_schedule()` execute asynchronously in the event loop.
   - When destroying a device, cancel and delete pending BHs: `qemu_bh_cancel(s->bh); qemu_bh_delete(s->bh);`.
   - Ensure the callback function does not access device state after it has been freed.

3. **RCU (Read-Copy-Update) in QEMU**:
   - `rcu_read_lock()` / `rcu_read_unlock()` must protect RCU-guarded data structures (such as AddressSpace dispatch trees).
   - Modifications must use `atomic_rcu_read()` and `atomic_rcu_set()` with appropriate memory barriers.
