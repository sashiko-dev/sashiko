# Workqueue API Guidelines

## Workqueue Fundamentals

Workqueues defer work to asynchronous kernel worker threads running in process
context. Work items can sleep, allocate memory with `GFP_KERNEL`, and acquire
mutexes.

| Type | Structure | Initialization | Enqueue | Sync Cancel / Flush |
|------|-----------|----------------|---------|---------------------|
| Standard Work | `struct work_struct` | `INIT_WORK(work, func)` | `queue_work(wq, work)` / `schedule_work(work)` | `cancel_work_sync(work)`, `flush_work(work)` |
| Delayed Work | `struct delayed_work` | `INIT_DELAYED_WORK(dwork, func)` | `queue_delayed_work(wq, dwork, delay)` / `schedule_delayed_work(dwork, delay)` | `cancel_delayed_work_sync(dwork)`, `flush_delayed_work(dwork)` |

## Synchronization and Cancellation Semantics

- **`cancel_work_sync()` / `cancel_delayed_work_sync()`**: Cancels pending execution
  and waits for any currently running work handler on other CPUs to finish. Always
  use sync cancellation prior to freeing the enclosing data structure.
- **`flush_work()` / `flush_delayed_work()`**: Waits for pending and running instances
  to complete, but does NOT prevent new instances from being enqueued concurrently.
- **`destroy_workqueue()`**: Flushes all queued work and tears down the workqueue.
  Callers must guarantee no new work items will be enqueued to the workqueue before
  or during `destroy_workqueue()`.

## Deadlock Prevention

- **Self-Flushing Deadlock**: Calling `flush_work()`, `cancel_work_sync()`, or
  `destroy_workqueue()` from within the work handler itself causes a self-deadlock.
- **Lock Ordering Deadlock**: If a work handler acquires lock `L`, the caller MUST NOT
  hold lock `L` across `cancel_work_sync()`, `flush_work()`, or `destroy_workqueue()`.
  Doing so creates a classic AB-BA deadlock.
- **Forward Progress on Single-Threaded Workqueues**: In single-threaded workqueues
  (or workqueues created with `max_active = 1`), queuing work `B` from work `A` and
  blocking on `B`'s completion deadlocks because `B` cannot execute until `A` finishes.

## Teardown and Lifetime Rules

1. **Stop Producers First**: Before cancelling work or destroying a workqueue, disable
   interrupts, unregister event sources, or mark state flags to stop new work from
   being submitted.
2. **Synchronous Cancellation**: Cancel work items with `cancel_work_sync()` or
   `cancel_delayed_work_sync()` before freeing the memory containing `struct work_struct`.
3. **Workqueue Destruction**: Call `destroy_workqueue()` only after all work producers
   are stopped.

## Quick Checks

- Freeing memory containing `struct work_struct` without calling `cancel_work_sync()` -> Use-after-free
- `cancel_work_sync()` or `flush_work()` called with locks held that the work callback acquires -> Deadlock
- `cancel_work_sync()` called from within the work callback itself -> Deadlock
- Non-sync `cancel_delayed_work()` or `cancel_work()` immediately followed by `kfree()` -> Race / UAF
- `queue_work()` called on a workqueue that is currently being destroyed or already destroyed -> Crash
