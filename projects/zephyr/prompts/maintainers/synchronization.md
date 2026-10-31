+++
title = "Threads, Locks, and Workqueues"
paths = [
    "kernel/",
    "include/zephyr/spinlock.h",
]
symbols = [
    "K_KERNEL_STACK*",
    "K_MUTEX_DEFINE",
    "K_SPINLOCK",
    "K_THREAD_DEFINE",
    "K_THREAD_STACK*",
    "K_WORK_DEFINE",
    "atomic_*",
    "k_condvar_*",
    "k_mutex_*",
    "k_sched_lock",
    "k_sem_*",
    "k_spin_*",
    "k_spinlock",
    "k_thread_create",
    "k_work_*",
]
+++

# Threads, Locks, and Workqueues

Applies to code that shares state between threads or with interrupts. A
concurrency finding must name the two contexts that interleave and the data they
touch. "This could race" is not a finding.

## Spinlocks

Source: doc/kernel/services/synchronization/spinlocks.rst;
doc/kernel/services/smp/smp.rst; include/zephyr/spinlock.h.

- Use a spinlock for a short critical section shared between threads and ISRs, or
  between CPUs. A spinlock masks interrupts on the local CPU while held. An atomic
  operation needs no additional lock only when it protects the entire shared
  invariant. A separate check or related state still needs synchronization.
- Never block or sleep while holding a spinlock, and keep the critical section
  short. A call into a sleeping API inside the critical section is a defect.
- Spinlocks are not recursive. A context that already holds a lock deadlocks if it
  takes it again; check helpers called with the lock held.
- Distinct spinlocks may nest, but must follow a consistent order. Report an
  inversion by naming both paths.
- Every `k_spin_lock()` is released by `k_spin_unlock()` with its key, in properly
  nested order. Check every early return.
- A `K_SPINLOCK()` block must run to its end or leave through
  `K_SPINLOCK_BREAK`. Leaving it any other way, such as a `return` or `goto`,
  keeps the lock held.
- Prefer the spinlock API for SMP code. Check `irq_lock()` in the target tree
  before claiming that it cannot serialize other CPUs.

## Mutexes and semaphores

Source: doc/kernel/services/synchronization/mutexes.rst.

- Mutexes are for threads only. A mutex may be locked recursively and must be
  unlocked the same number of times.
- A mutex must not be freed or reused while locked. A dynamically allocated object
  must be unlocked before it is released.
- Check timeouts: a `k_mutex_lock()` or `k_sem_take()` with a finite timeout can
  fail, and the code must not enter the critical section when it does.

## Workqueues

Source: doc/kernel/services/threads/workqueue.rst.

- A handler that blocks stops every later item on that queue. Blocking in a
  handler on the system workqueue needs care, since unrelated subsystems share it.
- A pending work item must not be altered or re-initialized until the workqueue
  has processed it, and the data its handler needs must stay valid until then. A
  common defect is freeing the object containing a work item without canceling it.
- `k_work_flush()`, `k_work_cancel_sync()`, and `k_work_cancel_delayable_sync()`
  may only be called from threads, since they block. Their `k_work_sync` object
  should not be on the stack when the code must work with
  `CONFIG_KERNEL_COHERENCE`.
- A handler may resubmit its own work item. Check that shutdown paths cancel it.
- `k_work_is_pending()`, `k_work_busy_get()`, and
  `k_work_delayable_remaining_get()` return a snapshot that can be stale by the time
  it is read. Report a decision to submit or cancel based on one when a stale answer
  loses work. The handler should check a condition kept in shared state instead.

## Thread stacks

Source: doc/kernel/services/threads/index.rst.

- Use `K_KERNEL_STACK_*` only when the thread will never run in user mode. Use
  `K_THREAD_STACK_*` otherwise, or when it cannot be determined. A user-mode
  attempt to use a kernel-only stack fails.
