# Locking Subsystem Details

## Lock Context Compatibility

Using the wrong lock type for the execution context causes deadlocks (sleeping
in atomic context), missed wakeups, or priority inversion. The table below
shows which lock types provide correct protection for data shared across the
listed contexts.

The "vs" columns indicate whether the lock variant provides mutual exclusion
against code running in that context. For example, `spin_lock_bh` is safe
vs process and softirq context, but not vs hardirq context. The Sleeps
column indicates whether acquiring the lock may sleep, which determines
what other operations are legal while holding it (sleeping locks cannot be
held in atomic context or under a spinlock).

| Lock Variant | vs Process | vs Softirq | vs Hardirq | Sleeps |
|---|---|---|---|---|
| `spin_lock` | Yes | No | No | No |
| `spin_lock_bh` | Yes | Yes | No | No |
| `spin_lock_irq` | Yes | Yes | Yes | No |
| `spin_lock_irqsave` | Yes | Yes | Yes | No |
| `mutex` / `rwsem` | Yes | No | No | Yes |

**Notes:**
- `spin_lock` does not mask softirqs or hardirqs; data can still be
  accessed concurrently by softirq or hardirq handlers on the same CPU
  - Nesting `spin_lock(lock_b)` inside `spin_lock_irq`/`_bh`/`_irqsave(lock_a)` keeps
    whatever masking was in place from lock_a while lock_b is held. The locks
    MUST be nested for this to work.
- `spin_lock_bh` disables softirqs; use when the lock is shared between
  process context and softirq context
- See the IRQ-Safe Lock Variants section for guidance on choosing between
  `spin_lock_irq` and `spin_lock_irqsave`
- `mutex` and `rwsem` can only be used in process context because they may
  sleep; never hold a spinlock while acquiring a mutex/rwsem

## Preemption, Migration, and IRQ Context

Confusing these three CPU-affinity mechanisms causes use-after-free on per-CPU
data, races where code assumes it stays on the same CPU, or deadlocks from
sleeping with preemption disabled.

- **Preemption disabled** (`preempt_disable()`/`preempt_enable()`): the current
  task stays on this CPU and will not be scheduled out, but IRQs can still
  occur. Per-CPU data access is safe. `spin_lock()` implicitly disables
  preemption on non-RT kernels (see PREEMPT_RT section for differences).
- **Migration disabled** (`migrate_disable()`/`migrate_enable()`): the task can
  be preempted (another task runs on this CPU) but will return to the same CPU.
  Useful when per-CPU data access does not need to be atomic but must stay on
  the same CPU across a longer operation.
- **IRQs disabled** (`local_irq_disable()`/`local_irq_save()`): no hardware
  interrupts will be delivered on this CPU. Implies preemption disabled (since
  the scheduler's timer tick is an IRQ). Required when sharing data between
  process/softirq context and hardirq handlers without a lock.
  - `local_irq_disable(); spin_lock(lock_b); critical section; spin_unlock(lock_b); local_irq_enable();`
    - critical section runs with IRQs off, just like `spin_lock_irq()` would provide

## IRQ-Safe Lock Variants

Using the wrong variant causes deadlock: if a hardirq handler takes a lock
that was already held by the interrupted code on the same CPU, the CPU spins
forever waiting for itself. Use `_irqsave` when a lock is shared with
interrupt handlers.

- `spin_lock_irq()`/`spin_unlock_irq()`: disables IRQs on lock, re-enables on
  unlock. Only safe when the caller knows IRQs are currently enabled.
- `spin_lock_irqsave(lock, flags)`/`spin_unlock_irqrestore(lock, flags)`:
  saves current IRQ state, restores on unlock. Safe regardless of whether IRQs
  were already disabled. Use this when the calling context is unknown.
- Nesting: `spin_lock_irqsave(lock1, flags1)` inside
  `spin_lock_irqsave(lock2, flags2)` is safe as long as lock ordering is
  respected (no ABBA deadlocks)

**When IRQs are already off:**
- All holders of a lock shared with IRQ context must disable IRQs (or use
  `_irqsave`) to take it safely
- Plain `spin_lock()` (without IRQ masking) is safe from code paths that are
  only reachable when IRQs are already off
- `spin_trylock()` avoids the deadlock scenario: the deadlock only occurs when
  `spin_lock()` is called with IRQs off on a CPU that already holds the lock

## PREEMPT_RT Differences

Code that works on non-RT kernels may deadlock or crash on PREEMPT_RT because
`spinlock_t` becomes an rt_mutex-based preemptible lock. Any code path that
disables preemption or IRQs while holding a `spinlock_t` will trigger a
lockdep splat (`BUG: Invalid wait context`) on RT.

- `spinlock_t`: becomes an rt_mutex-based lock on RT. Acquisition may sleep on
  contention, and the holder can be preempted, but the critical section still
  cannot acquire a mutex or other sleeping lock (the lockdep wait-context
  hierarchy `LD_WAIT_CONFIG` < `LD_WAIT_SLEEP` forbids it). Must not be held
  in hardirq context or with preemption/IRQs explicitly disabled.
- `raw_spinlock_t`: remains a true spinning lock on RT. Never sleeps. Use for
  code that must run in hardirq context or with IRQs disabled even on RT
  (e.g., scheduler, interrupt controller, low-level timer code).
- `local_irq_disable()`: still disables IRQs on RT. Note that
  `spin_lock_irq()` on a `spinlock_t` does NOT disable IRQs on RT (it
  acquires the underlying rt_mutex without masking interrupts;
  see `spin_lock_irq()` in `include/linux/spinlock_rt.h`)

## Seqlocks and Seqcounts

A seqlock is a reader-writer synchronization mechanism optimized for data
that is read frequently and written rarely. Readers never block writers (no
writer starvation).

How it works: readers speculatively read the data, then check a sequence
counter to detect whether a writer was active during the read. If so, the
read is retried. Writers increment the sequence counter before and after the
update, and must serialize against each other.

Two variants exist: `seqlock_t` bundles a `seqcount_spinlock_t` with a
`spinlock_t` that provides writer serialization automatically (see
`include/linux/seqlock_types.h`). A bare `seqcount_t` can be used when
writer serialization is provided by an external lock.

Use seqlocks when the protected data is small enough that retrying reads is
cheap. Incorrect usage causes readers to use partially-updated data (missed
retry) or infinite retry loops (side effects in the critical section that
prevent convergence). Review both the reader and writer sides together.

**Read side** (see `include/linux/seqlock.h`):
- ALL code between `read_seqbegin()` and `read_seqretry()` is the critical
  section. If the writer was active, the entire section re-executes.
- The critical section must not have side effects (no allocations, no
  writes to shared state, no I/O) because it may run multiple times
- The critical section must not dereference pointers that could be freed
  by the writer; use `rcu_dereference()` for pointer-following under RCU

**Write side**:
- `write_seqcount_begin()`/`write_seqcount_end()` must be correctly paired;
  an unbalanced sequence count causes readers to either never retry (missing
  begin) or always retry in an infinite loop (missing end)
- `raw_write_seqcount_begin()`/`raw_write_seqcount_end()` skip the lockdep
  assertion that the write-serializing lock is held; only valid when
  serialization is provided by a different mechanism

## Lock Nesting and Annotations

Taking the same lock type at multiple nesting levels without telling lockdep
causes false-positive lockdep splats (or hides real deadlocks). Incorrect
sparse annotations hide lock imbalance bugs from static analysis.

**Nesting classes** (see `include/linux/lockdep_types.h`):
- When code takes the same lock type at different levels (e.g., parent inode
  lock then child inode lock), use `mutex_lock_nested(lock, subclass)` or
  `spin_lock_nested(lock, subclass)` to tell lockdep they are different
- Lockdep supports up to `MAX_LOCKDEP_SUBCLASSES` (8) nesting levels
- Subclass 0 is the default; higher values indicate deeper nesting

**Sparse annotations** (`include/linux/compiler_types.h`):
- `__must_hold(lock)`: function requires `lock` to be held on entry and exit
- `__acquires(lock)`: function acquires `lock` (held on exit, not on entry)
- `__releases(lock)`: function releases `lock` (held on entry, not on exit)
- Mismatched annotations cause `sparse` to report "context imbalance" warnings.
  Review any patch that adds or changes these annotations to verify they match
  the actual lock behavior.

## RCU and Locking

Misunderstanding the relationship between RCU and traditional locks causes
use-after-free (reading freed data in an RCU section) or unnecessary
performance overhead (holding spinlocks where RCU suffices).

- `rcu_read_lock()` marks an RCU read-side critical section. Code must not
  sleep inside; use SRCU if sleeping is required.
- `synchronize_rcu()` blocks until all pre-existing RCU read-side critical
  sections complete (a full grace period)
- `call_rcu(head, callback)` defers `callback` execution until after a grace
  period; does not block
- SRCU (`srcu_read_lock()`/`srcu_read_unlock()`) allows sleeping in read
  sections; uses a domain-specific `struct srcu_struct`
- Holding `spin_lock()` or `raw_spin_lock()` implicitly provides RCU
  read-side protection. On non-RT kernels this is because they disable
  preemption; on PREEMPT_RT, `spin_lock()` calls `rcu_read_lock()`
  internally to preserve this guarantee (see `__rt_spin_lock()` in
  `kernel/locking/spinlock_rt.c`).

## Memory Barriers

Assume the patch author's barrier usage is correct unless the usage is clearly
wrong (e.g., missing a paired barrier, using `smp_wmb()` where a full
`smp_mb()` is needed). Subtle barrier bugs require deep architecture knowledge
to confirm.

- `smp_mb()`: full memory barrier. All loads and stores before the barrier
  appear to complete before all loads and stores after it.
- `smp_rmb()`: read barrier. Orders loads only; no effect on stores.
- `smp_wmb()`: write barrier. Orders stores only; no effect on loads.
- Barriers enforce ordering, not completion: they prevent CPU and compiler
  reordering across the barrier point
- Barriers must be paired between CPUs: if CPU A uses `smp_wmb()` before
  setting a flag, CPU B needs `smp_rmb()` after reading that flag to
  guarantee it sees the data written before the flag
- Common pattern (from `Documentation/memory-barriers.txt`):
  ```
  producer:                       consumer:
    my_data = value;                if (event_indicated) {
    smp_wmb();                          smp_rmb();
    event_indicated = 1;                do_something(my_data);
                                    }
  ```
- `atomic_read()` and `atomic_set()` provide **relaxed ordering** only; they
  guarantee atomicity of the access but do not imply memory barriers.
  RMW operations that return values (e.g., `atomic_add_return()`,
  `atomic_cmpxchg()`) provide full ordering. Use `smp_load_acquire()` /
  `smp_store_release()` or the `_acquire`/`_release` atomic variants when
  ordering is needed for plain loads and stores.

## Lockdep Lock Pinning (`lockdep_pin_lock`/`lockdep_unpin_lock`)

Misusing `lockdep_pin_lock()` triggers false positive lockdep warnings
("releasing a pinned lock") that mask real bugs or cause CI failures.

**How lock pinning works** (see `lock_pin_lock()` in
`kernel/locking/lockdep.c`):
- `lockdep_pin_lock(lock)` finds the `held_lock` entry for `lock` in the
  current task's lock stack and increments its `pin_count`
- When any held lock is released, lockdep checks `hlock->pin_count` via
  `find_held_lock()` and warns if the pin_count is non-zero
- Matching uses `match_held_lock()`, which compares by lock instance first.
  For `ww_mutex`-based locks (e.g., `dma_resv`) where multiple instances
  with a `nest_lock` share a single `held_lock` entry via reference
  counting, matching falls back to lock class comparison

**Problematic pattern with ww_mutex locks:**

When multiple `ww_mutex` locks of the same class are held with the same
`nest_lock`, lockdep folds them into a single `held_lock` entry with
`references > 0`. Pinning that entry and then unlocking any of the folded
instances triggers the "releasing a pinned lock" warning.

```c
// WRONG: Pinning a ww_mutex-based lock when other instances may be unlocked
lockdep_pin_lock(&vm->resv->lock.base);
ttm_bo_validate(...);  // May lock/unlock OTHER bos' dma_resv locks
lockdep_unpin_lock(&vm->resv->lock.base);  // Lockdep warned during validate!
```

**When lock pinning is safe:**
- The pinned region only manipulates the specific pinned lock instance
- No subsystem callbacks are invoked that might release the pinned lock
  or, for `ww_mutex`-based locks, other instances sharing the `held_lock`
  entry
- No iteration over lists of objects that share the lock class

**Alternative approaches when pinning is not safe:**
- Use a simple flag or pointer variable to track the state that pinning was
  meant to enforce (e.g., `vm->validating = current`)
- Use `lockdep_assert_held()` checks at critical points instead of continuous
  pinning
- Document the invariant rather than trying to enforce it through pinning

## Quick Checks

Common locking review pitfalls. Missing any of these causes deadlocks, data
corruption, or crashes that may only reproduce under specific timing or on
RT/SMP configurations.

- **Lock drop and reacquire**: when a lock is dropped and retaken (e.g., to
  call a sleeping function), all data protected by that lock may have changed.
  Verify the code re-validates pointers, refcounts, and conditions after
  reacquiring. A common bug is using a stale pointer to the locked object
  after the lock was transiently dropped.
- **Functions returning with different locks held**: some functions acquire a
  new lock and release the original (e.g., lock rebalancing, hash table
  migration). Verify the caller knows which lock is held on return and
  releases the correct one.
- **Reassigning locked objects**: when code reassigns a pointer to a
  previously locked data structure (e.g., `obj = next_obj` in a loop),
  verify the old object's lock is properly released. A missed unlock leaves
  the lock held forever, causing deadlock on the next acquisition.
- **`raw_spinlock_t` for hardirq paths on RT**: on PREEMPT_RT, only
  `raw_spinlock_t` remains a true spinning lock. IRQ handlers that use
  `spinlock_t` will trigger a lockdep splat on RT kernels.
- **Completion variables for event waiting**: use `wait_for_completion()` /
  `complete()` instead of open-coded spinlock polling loops
- **`percpu_rw_semaphore` for read-heavy patterns**: when reads vastly
  outnumber writes, `percpu_rw_semaphore` avoids cache-line bouncing
  (see `include/linux/percpu-rwsem.h`)
