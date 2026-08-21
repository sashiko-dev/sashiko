# Race Condition Tracing and Kernel Locking Guide

This document teaches a systematic method for finding race conditions in
Linux kernel code by tracing parallel execution timelines on shared data,
and provides the locking reference needed to verify correctness.

## 1. The Core Mental Model

A race condition is a concrete scenario where two CPUs execute instructions
in a specific interleaved order that produces a wrong outcome. To find
races, build these timelines explicitly.

At every point in one CPU's code path, ask: **"what could the other CPU be
doing right now?"** The answer is: anything in its own code path, at any
point, unless a synchronization mechanism forces an ordering.

## 2. Step-by-Step Tracing Method

### Step 1: Identify the Shared Data

Shared data includes global variables, fields inside globally-reachable
objects (e.g., `inode->i_size`), reference counts, state flags, and list
linkage pointers. Any struct member accessed from more than one code path
is potentially shared.

### Step 2: Enumerate All Code Paths

For each shared variable, list every code path that reads or writes it:
syscall handlers, interrupt handlers (hardirq, softirq), workqueue
callbacks, timer functions, tasklets, module init/exit, network receive
paths (NAPI poll). Each pair of paths is a potential race to analyze.

### Step 3: Build the Timeline

Take two code paths, lay them side by side. Find an interleaving that
produces a wrong outcome.

Example — TOCTOU race on a list:
```
CPU 0 (delete)                    CPU 1 (add)
──────────────                    ──────────
list_empty() returns TRUE
  → decides to skip delete
                                  spin_lock_bh(&lock)
                                  list_add(&rt->rt6i_uncached, &list)
                                  spin_unlock_bh(&lock)
return (skips deletion!)
→ rt is on the list but should have been removed → use-after-free on free
```

The check (`list_empty`) and action (`list_del_init`) are not in the same
atomic region.

### Step 4: Lockset Analysis

For each shared variable V, compute the intersection of locks held across
all accesses by any thread. If the intersection is empty, no single lock
protects V across all accesses — potential race.

```
Path A: spin_lock(&obj->lock); obj->counter++; spin_unlock(&obj->lock);
Path B: obj->counter--;  // no lock held

L(A) = {obj->lock}, L(B) = {}
C(obj->counter) = {obj->lock} ∩ {} = {} → RACE
```

The lock must also be the right *type* for the execution contexts — see
Lock Context Compatibility (§4).

### Step 5: Check Object Lifetime

At every point where a path releases a lock or drops a reference, ask:
"Could another path still hold a raw pointer to this object?"

```
CPU 0 (lookup)                    CPU 1 (remove)
──────────────                    ──────────────
lock → find item → unlock
                                  lock → list_del → unlock → kfree(item)
item->data ← USE-AFTER-FREE
```

Fix: take a reference count under the lock before releasing it.

## 3. The Four Questions at Every Access Point

At every line that reads or writes a shared variable:

**Q1: What lock is held?** Trace backward through the call chain. Look for
`lockdep_assert_held()`, `__`-prefixed functions (convention: caller holds
lock), direct lock calls up the stack.

**Q2: What other path could access this data right now?** Another CPU
running the same syscall on a different object, an interrupt on this CPU,
a timer/workqueue, a concurrent `close()` or module unload.

**Q3: Is the lock type strong enough?** A `spin_lock()` protecting data
also accessed from an IRQ handler is insufficient — see §4.

**Q4: Is the object still alive?** Was the pointer obtained under a lock
or RCU read-side section that is still held? Was a reference count taken?
If neither, the object may have been freed.

## 4. Lock Context Compatibility

Using the wrong lock type for the execution context causes deadlocks
(sleeping in atomic context), missed wakeups, or priority inversion. This
table shows which lock variants provide mutual exclusion against code in
each context.

| Lock Variant | Wait Type | vs Process | vs Softirq | vs Hardirq | Sleeps |
|---|---|---|---|---|---|
| `raw_spin_lock` | LD_WAIT_SPIN | Yes | No | No | No |
| `raw_spin_lock_irqsave` | LD_WAIT_SPIN | Yes | Yes | Yes | No |
| `spin_lock` | LD_WAIT_CONFIG | Yes | No | No | No (non-RT) / Yes (RT) |
| `spin_lock_bh` | LD_WAIT_CONFIG | Yes | Yes | No | No (non-RT) / Yes (RT) |
| `spin_lock_irq` | LD_WAIT_CONFIG | Yes | Yes | Yes | No (non-RT) / Yes (RT) |
| `spin_lock_irqsave` | LD_WAIT_CONFIG | Yes | Yes | Yes | No (non-RT) / Yes (RT) |
| `local_lock` | LD_WAIT_CONFIG | Per-CPU | No | No | No (non-RT) / Yes (RT) |
| `local_trylock` | LD_WAIT_CONFIG | Per-CPU | No | No | No (non-RT) / Yes (RT) |
| `mutex` / `rwsem` | LD_WAIT_SLEEP | Yes | No | No | Yes |

- `spin_lock` does not mask softirqs or hardirqs
- `spin_lock_bh` disables softirqs; use when shared between process and
  softirq context
- `mutex`/`rwsem` can only be used in process context (they sleep);
  never hold a spinlock while acquiring a mutex/rwsem
- Nesting `spin_lock(B)` inside `spin_lock_irq`/`_bh`/`_irqsave(A)` keeps
  whatever masking was in place from A while B is held; the locks MUST
  remain nested

## 5. Lock Nesting Compatibility

Lockdep enforces wait-type nesting: inner lock wait type must be ≤ outer.
`CONFIG_PROVE_RAW_LOCK_NESTING` (default `y`) enforces this even on
non-RT kernels. `!IS_ENABLED(CONFIG_PREEMPT_RT)` guards do NOT suppress
these checks. Any violation is a bug. Report it.

| Outer lock held | Can nest | CANNOT nest (lockdep BUG) |
|---|---|---|
| `raw_spinlock_t` | `raw_spinlock_t` | `spinlock_t`, `local_lock`, `local_trylock`, `mutex`, `rwsem` |
| `spinlock_t` | `raw_spinlock_t`, `spinlock_t`, `local_lock`, `local_trylock` | `mutex`, `rwsem` |
| `local_lock` / `local_trylock` | `raw_spinlock_t`, `spinlock_t`, `local_lock`, `local_trylock` | `mutex`, `rwsem` |
| `mutex` / `rwsem` | all | — |

Fix for intentional violations: `DEFINE_WAIT_OVERRIDE_MAP(map,
LD_WAIT_CONFIG)` with `lock_map_acquire_try(&map)` /
`lock_map_release(&map)`. This tells lockdep the nesting is intentional
(e.g., because the LD_WAIT_CONFIG path is unreachable on PREEMPT_RT).

To check: if code acquires `spinlock_t`, `local_lock`, or `local_trylock`,
check whether ANY caller holds `raw_spinlock_t`. If code holds
`raw_spinlock_t`, check whether ANY callee acquires `spinlock_t`,
`local_lock`, or `local_trylock`.

## 6. Multi-Variable Races

The most subtle races involve multiple variables that must be updated
atomically together. Each individual access may be locked, but the
relationship between variables is unprotected.

```
CPU 0                                CPU 1
──────                               ──────
lock → set state=ACTIVE → unlock
                                     lock → read state=ACTIVE
                                             call handler → OLD handler!
                                     unlock
lock → set handler=new → unlock
```

Both variables must be updated in the same critical section.

When you find an unlocked access to a shared variable, do not dismiss it
because "the window is tiny." If the ordering violation can occur at all,
it is a bug.

## 7. Memory Ordering

Even with `READ_ONCE()`/`WRITE_ONCE()`, the CPU may reorder stores and
loads. When publishing a pointer to initialized data:

```c
// WRONG — Store 2 may become visible before Store 1:
data->field = 42;                      // Store 1
WRITE_ONCE(global_ptr, data);          // Store 2

// RIGHT — release barrier ensures Store 1 completes before Store 2:
data->field = 42;
smp_store_release(&global_ptr, data);

// Consumer must use acquire:
p = smp_load_acquire(&global_ptr);
if (p) x = p->field;                  // guaranteed to see 42
```

`rcu_assign_pointer()` and `rcu_dereference()` include these barriers.

### Barrier Types

- `smp_mb()`: full barrier, orders all loads and stores
- `smp_rmb()`: read barrier, orders loads only
- `smp_wmb()`: write barrier, orders stores only
- Barriers enforce ordering, not completion: they prevent CPU and compiler
  reordering across the barrier point
- Barriers must be paired between CPUs: producer's `smp_wmb()` pairs
  with consumer's `smp_rmb()`
- Common pattern (from `Documentation/memory-barriers.txt`):
  ```
  producer:                       consumer:
    my_data = value;                if (event_indicated) {
    smp_wmb();                          smp_rmb();
    event_indicated = 1;                do_something(my_data);
                                    }
  ```
- `atomic_read()`/`atomic_set()` are relaxed — no ordering. RMW ops
  that return values (`atomic_add_return()`, `atomic_cmpxchg()`) provide
  full ordering. Use `smp_load_acquire()`/`smp_store_release()` or the
  `_acquire`/`_release` atomic variants for plain loads/stores that need
  ordering.

Assume the patch author's barrier usage is correct unless clearly wrong
(e.g., missing a paired barrier, using `smp_wmb()` where `smp_mb()` is
needed). Subtle barrier bugs require deep architecture knowledge.

## 8. Interrupt Timelines

Interrupts create concurrency on a single CPU. The handler preempts
whatever was running:

```
CPU 0 (process context)
──────────────────────
spin_lock(&data_lock)        ← acquired, IRQs not disabled
shared_counter++
  ← IRQ fires on THIS CPU
  ├─ IRQ handler: spin_lock(&data_lock) → DEADLOCK
  │   (we hold the lock but can't continue)
  └─ Never returns
```

If the same lock is used from both process and hardirq context, ALL
process-context acquisitions must use `spin_lock_irqsave()`. If shared
with softirq only, process context must use `spin_lock_bh()`.

## 9. RCU

### Read-Side Critical Sections

- `rcu_read_lock()` marks an RCU read-side critical section; must not
  sleep inside. Use SRCU (`srcu_read_lock()`/`srcu_read_unlock()` with a
  domain-specific `struct srcu_struct`) when sleeping in read sections is
  required
- Holding `spin_lock()` or `raw_spin_lock()` implicitly provides RCU
  read-side protection (disables preemption on non-RT; on PREEMPT_RT,
  `spin_lock()` calls `rcu_read_lock()` internally)

### Writer-Side Lifetime

After `rcu_assign_pointer()` replaces a pointer, the old pointer must
be freed via `call_rcu()`, `kfree_rcu()`, or after `synchronize_rcu()`.
Direct `kfree()` is always a bug:

```
CPU 0 (Writer)                    CPU 1 (Reader)
──────────────                    ──────────────
                                  rcu_read_lock()
                                  p = rcu_dereference(ptr) → old
rcu_assign_pointer(ptr, new)
kfree(old) ← BUG!
                                  use(p->val) → USE-AFTER-FREE
                                  rcu_read_unlock()
```

Use `call_rcu(&old->rcu_head, free_fn)` — the callback runs only after
all pre-existing RCU read-side sections complete.

- `synchronize_rcu()` blocks until all pre-existing RCU read-side
  critical sections complete (a full grace period)
- `call_rcu(head, callback)` defers callback until after a grace period;
  does not block

## 10. Preemption, Migration, IRQ, and CPU Hotplug

- **Preemption disabled** (`preempt_disable()`/`preempt_enable()`): task
  stays on this CPU, won't be scheduled out. IRQs can still occur.
  Per-CPU data access is safe. `spin_lock()` implicitly disables
  preemption on non-RT.
- **Migration disabled** (`migrate_disable()`): task can be preempted but
  returns to the same CPU. Use when per-CPU access doesn't need atomicity
  but must stay on the same CPU.
- **IRQs disabled** (`local_irq_disable()`/`local_irq_save()`): no
  hardware interrupts on this CPU. Implies preemption disabled (since the
  scheduler's timer tick is an IRQ).
- **CPU hotplug** (`cpus_read_lock()`/`cpus_read_unlock()`): prevents
  CPUs from going online/offline. Required when using per-CPU resources
  allocated in hotplug callbacks (`cpuhp_setup_state()`). Neither
  preemption nor migration disable prevents hotunplug — they only pin
  the task. `cpus_read_lock()` acquires `cpu_hotplug_lock` as a
  `percpu_rw_semaphore` — it sleeps, cannot be held in atomic context.
  When sleeping is needed in a per-CPU critical section, alternatives:
  (a) `cpus_read_lock()`, (b) a mutex within each per-CPU structure
  serializing with teardown callback, or (c) a refcount on the per-CPU
  structure.

**None of preemption, migration, or IRQ disable prevent CPU hotplug.**
When code switches from `get_cpu_ptr()` to `raw_cpu_ptr()` to accommodate
sleepable APIs, verify CPU hotplug teardown is still safe.

## 11. IRQ-Safe Lock Variants

- `spin_lock_irq()`/`spin_unlock_irq()`: disables IRQs on lock,
  re-enables on unlock. Only safe when caller knows IRQs are enabled.
- `spin_lock_irqsave(lock, flags)`/`spin_unlock_irqrestore(lock, flags)`:
  saves/restores IRQ state. Safe regardless of current state. Use when
  calling context is unknown.
- Nesting: `spin_lock_irqsave(lock1, flags1)` inside
  `spin_lock_irqsave(lock2, flags2)` is safe as long as lock ordering is
  respected (no ABBA deadlocks)
- All holders of a lock shared with IRQ context must disable IRQs to
  take it safely. Plain `spin_lock()` (without IRQ masking) is safe from
  code paths that are only reachable when IRQs are already off.
  `spin_trylock()` avoids the deadlock scenario (deadlock only occurs when
  `spin_lock()` is called with IRQs off on a CPU that already holds the
  lock).

## 12. PREEMPT_RT Differences

On PREEMPT_RT, `spinlock_t` becomes an rt_mutex (sleepable, preemptible).
Code that disables preemption or IRQs while holding `spinlock_t` triggers
lockdep warnings on RT.

- `spinlock_t` on RT: sleeps on contention, holder can be preempted.
  Still cannot nest `mutex`/`rwsem` inside (wait-context hierarchy
  `LD_WAIT_CONFIG` < `LD_WAIT_SLEEP`). Must not be held in hardirq
  context or with preemption/IRQs explicitly disabled.
- `raw_spinlock_t`: true spinning lock even on RT. Use for hardirq
  context, scheduler, interrupt controller, low-level timer code.
- `local_lock` / `local_lock_irqsave()`: on non-RT, these disable
  preemption or IRQs respectively (no actual lock). On RT, they map to
  `spinlock_t` + `migrate_disable()`, so `local_lock` can sleep.
  Guard with `!preemptible()`, NOT `in_nmi() ||
  in_hardirq()` — the latter misses `preempt_disable()` sections.
  `preemptible()` checks `preempt_count() == 0 && !irqs_disabled()`.
  `in_hardirq()` only detects hardware interrupt context; it misses
  `preempt_disable()` sections and other non-preemptible contexts (e.g.,
  BPF tracepoint callbacks).
- `spin_lock_irq()` on `spinlock_t` does NOT disable IRQs on RT (it
  acquires the underlying rt_mutex without masking interrupts).
- `local_irq_disable()` still disables IRQs on RT.
- `raw_spinlock_t` code must not acquire `spinlock_t`, `local_lock`,
  or `local_trylock` — see Lock Nesting (§5).

## 13. Seqlocks

Reader-writer mechanism optimized for read-heavy, write-rare data.
Readers never block writers (no writer starvation). Readers speculatively
read, then check a sequence counter; retry if a writer was active.
Writers increment the sequence counter before and after the update, and
must serialize against each other. Use seqlocks when the protected data
is small enough that retrying reads is cheap. Review both the reader and
writer sides together.

**Read side**: all code between `read_seqbegin()` and `read_seqretry()`
may re-execute. The critical section must have no side effects (no
allocations, writes to shared state, or I/O) and must not dereference
pointers that the writer could free; use `rcu_dereference()` for
pointer-following under RCU.

**Write side**: `write_seqcount_begin()`/`write_seqcount_end()` must be
correctly paired. Unbalanced count causes infinite reader retries (missing
end) or missed retries (missing begin).

Two variants: `seqlock_t` bundles a `seqcount_spinlock_t` with a
`spinlock_t` for writer serialization. Bare `seqcount_t` when writer
serialization is provided by an external lock.

`raw_write_seqcount_begin()`/`raw_write_seqcount_end()` skip the lockdep
assertion that the write-serializing lock is held; only valid when
serialization is provided by a different mechanism.

## 14. Lock Annotations

**Nesting classes**: `mutex_lock_nested(lock, subclass)` or
`spin_lock_nested(lock, subclass)` to distinguish same-type locks at
different nesting levels (e.g., parent/child inode locks). Up to
`MAX_LOCKDEP_SUBCLASSES` (8) levels. Subclass 0 is the default; higher
values indicate deeper nesting.

**Sparse annotations**: `__must_hold(lock)`, `__acquires(lock)`,
`__releases(lock)`. Mismatched annotations cause `sparse` context
imbalance warnings. Verify annotations match actual lock behavior.

## 15. Lockdep Lock Pinning

`lockdep_pin_lock(lock)` finds the `held_lock` entry for `lock` in the
current task's lock stack and increments its `pin_count`. When any held
lock is released, lockdep checks `hlock->pin_count` via
`find_held_lock()` and warns if non-zero. Matching uses
`match_held_lock()`, which compares by lock instance first. For
`ww_mutex`-based locks (e.g., `dma_resv`) where multiple instances with
a `nest_lock` share a single `held_lock` entry via reference counting
(`references > 0`), matching falls back to lock class comparison.
Pinning that entry and then unlocking any of the folded instances
triggers a "releasing a pinned lock" warning.

```c
// WRONG: Pinning a ww_mutex-based lock when other instances may be unlocked
lockdep_pin_lock(&vm->resv->lock.base);
ttm_bo_validate(...);  // May lock/unlock OTHER bos' dma_resv locks
lockdep_unpin_lock(&vm->resv->lock.base);  // Lockdep warned during validate!
```

Safe when: the pinned region only manipulates the specific pinned instance,
invokes no callbacks that might release it or other instances sharing the
`held_lock` entry, and does not iterate over lists of objects that share
the lock class.

Alternatives when pinning is not safe: use a flag or pointer variable to
track the state that pinning was meant to enforce (e.g.,
`vm->validating = current`), or use `lockdep_assert_held()` at critical
points instead of continuous pinning.

## 16. Worked Example

```c
static LIST_HEAD(conn_list);
static DEFINE_SPINLOCK(list_lock);

// Path A: add_connection (process context)
//   spin_lock(&list_lock); list_add(&c->list, &conn_list); spin_unlock();
// Path B: receive_data (softirq context)
//   list_for_each_entry(c, &conn_list, list) { ... }  ← NO LOCK
// Path C: remove_connection (process context)
//   spin_lock(&list_lock); list_del(&c->list); spin_unlock(); kfree(c);
```

**Lockset analysis** on Path B vs Path C:
- Path B accesses `conn_list`: L(B) = {} (no lock)
- Path C accesses `conn_list`: L(C) = {list_lock}
- C(conn_list) = {} → RACE

**Timeline (B vs C)**:
```
CPU 0 (Path B: softirq)           CPU 1 (Path C: process)
────────────────────               ────────────────────
c = first entry, no match
next_c = c->list.next
                                   lock → list_del(c) → unlock → kfree(c)
c = next_c ← FREED MEMORY
```

**Four bugs found:**
1. Path B has no list lock → race with Path C on list traversal
2. Path C uses `spin_lock()` but Path B runs in softirq → must use
   `spin_lock_bh()` to prevent softirq preemption on same CPU
3. Path B needs `rcu_read_lock()` + `list_for_each_entry_rcu()`, OR
   `spin_lock_bh(&list_lock)`
4. After fixing with RCU, Path C's `kfree(c)` must become
   `kfree_rcu(c, rcu_head)` or follow `synchronize_rcu()`

## 17. Tracing Algorithm and Quick Checks

### The Algorithm

1. Find all shared data — variables accessed from multiple code paths
2. For each, list every path and its execution context
3. For each pair, compute lockset intersection — empty = potential race
4. Build the interleaved timeline — find a specific wrong outcome
5. Check lock context compatibility (§4)
6. Check object lifetimes — reference or RCU held after lock release?
7. Check memory ordering — publish patterns need acquire/release
8. Check TOCTOU — condition and action in the same atomic region?

### Quick Checks

- **Lock drop and reacquire**: all prior validation is stale. Re-check
  pointers, refcounts, conditions after reacquiring.
- **Functions returning with different locks**: verify the caller knows
  which lock is held on return and releases the correct one.
- **Reassigning locked objects**: verify old object's lock is released
  before acquiring the new object's lock.
- **`raw_spinlock_t` for hardirq on RT**: `spinlock_t` in IRQ handlers
  triggers lockdep splat on RT.
- **Context guards on RT**: use `!preemptible()`, not `in_nmi() ||
  in_hardirq()` — the latter misses `preempt_disable()` sections.
- **Completion variables**: use `wait_for_completion()`/`complete()`
  instead of open-coded spinlock polling loops.
- **`percpu_rw_semaphore`**: for read-heavy patterns where reads vastly
  outnumber writes, avoids cache-line bouncing.
- **Reclaim-reachable lock ordering**: locks held during `GFP_KERNEL`
  allocation must order above all reclaim-path locks (`->writepages`,
  shrinkers, `folio_lock()`). Lockdep detects this via the `__GFP_FS` /
  `__GFP_IO` / `__GFP_RECLAIM` flags and `memalloc_nofs` / `memalloc_noio`
  scope annotations. Use `memalloc_nofs_save()` when holding locks
  conflicting with filesystem reclaim.
- **Never dismiss a race because the window is small.** If the ordering
  violation can occur at all, it is a bug.
- **A validation check before the exclusion point is NOT protection.**
  If code checks shared state then acquires exclusion, the check is
  TOCTOU — a concurrent path can modify/free the data between the check
  and exclusion. Do not dismiss because "the check would detect it."
- **A single abort path does not make a race safe.** When evaluating
  whether a race is "handled," you will find one recovery point and
  stop looking. This is wrong. You must trace every instruction between
  the race window and the recovery point. If any intermediate
  instruction dereferences, locks, or depends on the contested resource,
  the race causes a crash before the recovery ever executes.
- **Subsystem guide directives are authoritative.** When a guide says
  "Do NOT dismiss X" or "REPORT as bugs", do not override with your
  own reasoning. Report it.
