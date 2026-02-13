# Scheduler Subsystem Details

## Runqueue Locking

Incorrect locking of runqueues causes deadlocks, data corruption in the
RB-tree, and use-after-free of task structs when concurrent CPUs observe
inconsistent state.

- Runqueue locks are `raw_spinlock_t` (never sleeps, even on PREEMPT_RT)
- Multi-runqueue operations must lock in consistent order to prevent deadlock
- `double_rq_lock()` / `double_rq_unlock()` handle ordering automatically
  (swaps to ascending order internally via `rq_order_less()`)
- `lockdep_assert_rq_held()` validates rq lock is held in accessors
- `task_rq(p)` expands to `cpu_rq(task_cpu(p))`. Without pinning or holding
  `pi_lock` / rq lock, the task can migrate after `task_cpu(p)` is read,
  so the caller gets CPU A's runqueue while the task is now on CPU B.
- Never release rq lock with a task in inconsistent state -- `p->on_rq`, the
  RB-tree, and `p->__state` must all agree before the lock is dropped.
  Other CPUs observe these fields immediately after unlock:
  - Task in tree but `on_rq == 0`: `try_to_wake_up()` sees `on_rq == 0` and
    calls `activate_task()` -- double enqueue corrupts the RB-tree
  - `on_rq == 1` but not in tree: `try_to_wake_up()` sees `on_rq == 1` and
    skips enqueue, but `pick_next_task` never finds the task -- permanently lost
  - `TASK_RUNNING` with `on_rq == 0`: `try_to_wake_up()` sees `TASK_RUNNING`
    and returns early (no wakeup needed) -- task hangs forever with no way to
    recover

## Task State Transitions

A lost wakeup leaves a task sleeping forever with no way to recover; a
missed state barrier lets a wakeup race past the condition check.

- `set_current_state()` uses `smp_store_mb()` so the state write is ordered
  relative to subsequent memory accesses (the condition check)
- Voluntary sleep pattern: set state BEFORE checking the condition, otherwise
  a wakeup between the check and the state change is lost

```c
// CORRECT -- state set before condition check
set_current_state(TASK_UNINTERRUPTIBLE);
if (!condition)
    schedule();
set_current_state(TASK_RUNNING);

// WRONG -- lost wakeup if condition changes between check and set_current_state
if (!condition) {
    set_current_state(TASK_UNINTERRUPTIBLE);  // wakeup already happened
    schedule();                                // sleeps forever
}
```

- `TASK_DEAD` requires special handling -- task cannot be rescheduled
- `wake_up_process()` calls `try_to_wake_up()` with `TASK_NORMAL`
  (`TASK_INTERRUPTIBLE | TASK_UNINTERRUPTIBLE`); safe to call regardless of
  the target task's state (returns 0 if not in a wakeable state)

## CPU Affinity and Migration

Migrating a task to a disallowed CPU or migrating without proper locks
causes crashes, violates cpuset constraints, or corrupts per-CPU data.

- `set_cpus_allowed_ptr()` changes a task's allowed CPU mask
- `kthread_bind()` restricts a kthread to a specific CPU (must be called
  before the kthread is started)
- Migration must respect `cpumask_subset()` against allowed CPUs
- Check `is_migration_disabled()` before migrating a task
- `stop_one_cpu()` may be needed for forced migration
- CPU hotplug requires special care -- tasks must not be migrated to a CPU
  being taken offline
- `migrate_disable()` prevents migration but does NOT disable preemption
  or prevent sleeping; tasks can block while migration-disabled

## CFS (SCHED_NORMAL / SCHED_BATCH / SCHED_IDLE)

### EEVDF Algorithm

Errors in eligibility checks, deadline computation, or vruntime accounting
cause starvation, unfair scheduling, or runqueue corruption.

- Selects the eligible task (lag >= 0, meaning owed service) with the
  earliest virtual deadline
- Virtual deadline: `vd = vruntime + vslice` where `vslice = calc_delta_fair(slice, se)`
  computes `slice * NICE_0_LOAD / weight` (source comment: `vd_i = ve_i + r_i/w_i`)
- Base slice: `sysctl_sched_base_slice` (default 700us); deadline recomputed
  by `place_entity()` on enqueue

### Key Fields

- `se->vruntime`: per-entity monotonic counter of weighted CPU time; must be
  normalized when migrating between CPUs
- `se->min_vruntime`: augmented RB-tree field tracking the minimum `vruntime`
  in a subtree; used for O(log n) eligibility pruning in `__pick_eevdf()`
- `se->vlag`: tracks service deficit/surplus; `vlag = V - vruntime` where V
  is the weighted average vruntime (`avg_vruntime()`); preserved across
  enqueue/dequeue by `place_entity()`; when `se->on_rq` and
  `cfs_rq->curr == se`, the same union field is used as `vprot`
- `se->on_rq`: set to 1 when the entity is enqueued; with delayed dequeue
  (`sched_delayed`), an entity can remain `on_rq == 1` after a logical
  dequeue until eligibility is exhausted
- Load weight: derived from nice value via `sched_prio_to_weight[]`; ~10%
  change per nice level

### RB-Tree Structure

The RB-tree is sorted by `se->deadline` and augmented with `se->min_vruntime`
per node, enabling O(log n) eligibility pruning in `__pick_eevdf()`. A subtree
can be skipped entirely if its `min_vruntime` shows no eligible entities
(checked via `vruntime_eligible()`).

### PELT (Per-Entity Load Tracking)

Stale or incorrect load averages cause bad load balancing decisions, wrong
frequency selection, and capacity estimation errors.

- `update_load_avg()` must be called BEFORE any entity state change
  (enqueue/dequeue/migration) to maintain hierarchy consistency
- Tracks a decaying average of utilization per entity and per `cfs_rq`
- On migration: `DO_ATTACH` attaches load to new CPU, `DO_DETACH` detaches
  from old CPU

### CFS Bandwidth

Incorrect throttle/unthrottle sequencing causes tasks to be permanently
throttled or to exceed their bandwidth quota.

- Group throttling uses `cfs_rq->runtime_remaining`
- Must properly dequeue on throttle, enqueue on unthrottle
- Hierarchical: parent throttle affects all children

## Real-Time (SCHED_FIFO / SCHED_RR)

Incorrect priority mapping or missing bandwidth limits can starve all
non-RT tasks and hang the system.

- Priority range: 1-99 (userspace); higher number = higher priority.
  Kernel internal priority is inverted (`MAX_RT_PRIO - 1 - rt_priority`)
- RT bandwidth: `sched_rt_runtime_us` / `sched_rt_period_us` prevent CPU
  monopolization (default 95% limit -- 950ms per 1000ms period)
- Tasks throttled when bandwidth exhausted; check `sched_rt_runtime()`

## Deadline (SCHED_DEADLINE)

Violating the admission control invariant causes deadline misses or
overcommits the CPU, breaking real-time guarantees for all DL tasks.

- Invariant: `runtime <= deadline <= period`
- Admission control: total bandwidth `sum(runtime_i / period_i)` must not
  exceed `M * (rt_runtime / rt_period)` where M is the count of active CPUs
  in the root domain (computed by `dl_bw_cpus()`)
- CBS (Constant Bandwidth Server) enforces bandwidth isolation
- Global Earliest Deadline First (GEDF): on an M-CPU system, the M tasks
  with earliest deadlines should be running
- Throttling: task blocked when runtime exhausted, unblocked at next period
  (replenishment)
- DL tasks tracked per root domain for migration decisions

## sched_ext (SCHED_EXT -- BPF Extensible Scheduler)

Bugs in BPF scheduler ops can stall tasks, corrupt dispatch queues, or
trigger the watchdog failsafe causing a revert to CFS.

- Scheduler behavior defined by BPF programs via ops callbacks (`select_cpu`,
  `enqueue`, `dispatch`, etc.)
- Dispatch queues (DSQ): tasks queued in local (per-CPU), global, or custom
  DSQs; can use FIFO or PRIQ (vtime) ordering but not mixed
- `ops_state` tracking: atomic `p->scx.ops_state` prevents concurrent BPF
  operations on the same task; transitions: NONE -> QUEUEING -> QUEUED -> DISPATCHING
- Direct dispatch: optimization allowing enqueue path to dispatch directly to
  local DSQ via per-CPU `direct_dispatch_task` marker
- Failsafe: watchdog timeout, `scx_error()`, and SysRq-S all trigger
  automatic revert to CFS

## Priority Inheritance

Broken PI chains cause unbounded priority inversion, where a high-priority
task blocks indefinitely behind a low-priority lock holder.

- When a high-priority task blocks on a lock held by a low-priority task,
  the lock holder temporarily inherits the blocked task's priority
- PI chain must be updated atomically; checked for cycles to detect deadlock
- RT mutex is the primary use case
- `pi_lock` protects PI state; must be held when traversing the chain

## CPU Hotplug Lifecycle

Per-CPU scheduler resources (hrtimers, callbacks, servers) that fire on an
offline CPU trigger warnings or access invalid per-CPU data structures. The
scheduler uses `sched_cpu_deactivate()` and `sched_cpu_dying()` as teardown
callbacks during CPU offline; these must stop or cancel any armed per-CPU
timers before the CPU exits the scheduler domain.

- `sched_cpu_deactivate()`: removes the CPU from active scheduling (clears
  `cpu_active_mask`, sets `balance_push`, tears down sched domains)
- `sched_cpu_dying()`: called while the CPU is still online but dying; stops
  timers that need the CPU to still be in valid state (e.g.,
  `dl_server_stop()` cancels the dl_server hrtimer here)
- When modifying mechanisms that gate per-CPU timer arming (removing checks,
  changing conditions), verify the timer cannot be armed during the CPU
  offline window -- the gating mechanism may have been providing implicit
  hotplug coordination
- `dl_server` uses an hrtimer (`dl_timer` in `struct sched_dl_entity`) that
  fires to service fair tasks; if the timer fires after the CPU is removed
  from `cpu_present_mask`, `cpudl_set()` triggers a `WARN_ON`
- `dl_server_start()` checks `cpu_online()` and returns early if the CPU is
  offline

## Quick Checks

- `preempt_disable()` / `preempt_enable()` must always be balanced
- `set_task_cpu()` only safe during migration with proper locks held
- Never call `schedule()` with preemption disabled or non-rq locks held
- Never enqueue a task that is already on a runqueue
