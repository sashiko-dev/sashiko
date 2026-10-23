# HIGH-RESOLUTION TIMERS, TIMER WHEEL, CLOCKEVENTS

This unit implements high-resolution timers (`hrtimer`), the jiffy-based timer wheel (`timer_list`), migratable timer hierarchies (`tmigr`), clock event devices (`clock_event_device`), and sleep/timeout APIs.

## Architecture & Data Flow

- When `detach_if_pending()` or `detach_timer()` is called with `clear_pending == false`, a detached timer retains a non-NULL `timer->entry.pprev` and must be re-enqueued into a `timer_base` or have `timer->entry.pprev = NULL` cleared before dropping `base->lock`. [F14]
- `enqueue_timer()` must update `base->vectors[idx]`, `base->pending_map`, `timer_set_idx(timer, idx)` (`TIMER_ARRAYMASK`), and `base->next_expiry` (storing the rounded `bucket_expiry` from `calc_wheel_index()`, never raw `timer->expires`) in lockstep; `detach_if_pending()` must clear `idx` in `pending_map` and set `base->next_expiry_recalc = true` when emptying `base->vectors + idx`. [F56, F72]
- In `calc_wheel_index()`, `delta = expires - clk` must be checked as signed (`(long)delta < 0`) so past expiries queue at `clk & LVL_MASK` with `*bucket_expiry = clk`, while positive deltas `>= WHEEL_TIMEOUT_CUTOFF` clamp to `clk + WHEEL_TIMEOUT_MAX` before `calc_index()`. `calc_index()` rounds `expires` up via `(expires >> LVL_SHIFT(lvl)) + 1`, requiring `LVL_START(n)` to use `LVL_SIZE - 1`. [F89, F121, F122]
- `next_pending_bucket()` searches `[offset, offset + LVL_SIZE)` in `pending_map` starting at `start = offset + (clk & LVL_MASK)`, wrapping to `[offset, start)` and adding `LVL_SIZE - start`. In `timer_recalc_next_expiry()`, advancing `clk` to the next level shifts right by `LVL_CLK_SHIFT` and adds `(clk & LVL_CLK_MASK) ? 1 : 0`. [F90, F123]
- In `__run_timers()`, after `collect_expired_timers()` sets `base->clk = base->next_expiry`, `base->clk++` and `timer_recalc_next_expiry(base)` must run before `expire_timers()` drops `base->lock`. In `__forward_timer_base()`, `base->clk` only advances when `time_after(basej, base->clk)` and never to `base->next_expiry` if `time_before(base->next_expiry, base->clk)`. [F106, F115]
- In `next_timer_interrupt()`, if `!base->timers_pending` after `timer_recalc_next_expiry(base)`, `base->next_expiry` advances to `basej + TIMER_NEXT_MAX_DELTA`; thus `fetch_next_timer_interrupt()` must check `timers_pending` before writing `tevt->local` or `tevt->global`. Past expiries (`time_before(nextevt, basej)` or `next_tmigr < basem` in `timer_use_tmigr()`) must be clamped before subtraction, and `(nextevt - basej)` cast to `u64` before `* TICK_NSEC`. [F75, F101, F113, F114]
- Selection and expiration of `timer_bases[BASE_DEF]` in `get_timer_cpu_base()`, `get_timer_this_cpu_base()`, and `run_timer_softirq()` depend solely on `IS_ENABLED(CONFIG_NO_HZ_COMMON)` (never `is_timers_nohz_active()`). `TIMER_DEFERRABLE` overrides `TIMER_PINNED` to select `BASE_DEF`, and `trigger_dyntick_cpu()` must return on `TIMER_DEFERRABLE` before checking `base->is_idle`. [F35, F99]
- When `!hrtimer_base_is_online(this_cpu_base)`, `hrtimer_suitable_target()` and `__hrtimer_start_range_ns()` must never select `this_cpu_base`, and `__hrtimer_start_range_ns()` must kick `new_cpu_base->cpu` via `smp_call_function_single_async()` if `enqueue_hrtimer()` returns `first`. [F66]
- `hrtimer_cpu_base::clock_base[]` is indexed by `enum hrtimer_base_type` (via `hrtimer_clockid_to_base()` plus `HRTIMER_MAX_CLOCK_BASES / 2` for softirq, or `base->index`), never raw `clockid_t`. [F87]
- `hrtimer_wakeup()` must clear `t->task = NULL` before `wake_up_process(task)` because sleepers test `!t->task` to detect expiry. In `do_nanosleep()`, retry loops after `hrtimer_sleeper_start_expires()` must set `mode = HRTIMER_MODE_ABS`, and `__run_hrtimer()` must clear `timer->is_rel = false` under `CONFIG_TIME_LOW_RES`. [F12, F71]
- In `__get_next_timer_interrupt()`, after `timer_use_tmigr()` when `idle_is_possible` is true, if `idle` is non-NULL, `time_after(nextevt, basej + 1)` must be re-verified before setting `base_local->is_idle = true`, and if `!base_local->is_idle`, `tmigr_cpu_activate()` must undo `tmigr_cpu_deactivate()`. [F77]
- In `tmigr_update_events()`, when skipping requeue because `evt->nextevt.expires == nextexp && !ignore`, `evt->cpu = first_childevt->cpu` must still be updated. In `tmigr_handle_remote()`, when `!tmigr_check_migrator()`, it may return early only if `READ_ONCE(tmc->wakeup) == KTIME_MAX`; otherwise it must write `data.firstexp` to `tmc->wakeup` under `tmc->lock`. [F78, F81]
- In `__clockevents_switch_state()`, `CLOCK_EVT_STATE_DETACHED` must fall through to `CLOCK_EVT_STATE_SHUTDOWN` to call `dev->set_state_shutdown(dev)`. When `__clockevents_update_freq()` calls `clockevents_config()`, it must reprogram oneshot (`clockevents_program_event()`) or periodic (`__clockevents_switch_state()`) hardware. [F84, F100]
- In `cev_delta2ns()`, `latch` must be cast to `u64` before `<< evt->shift`, saturated to `~0ULL` if `(clc >> evt->shift) != (u64)latch`, and `rnd = (u64)evt->mult - 1` added only when `~0ULL - clc > rnd` and (`!ismax` or `evt->mult <= (1ULL << evt->shift)`). `clockevents_program_event()` and `clockevents_program_min_delta()` must widen `delta` to `unsigned long long` before `* dev->mult`. [F117, F118, F119]

## Synchronization

- Lock hierarchy: `timer_bases[BASE_LOCAL].lock` before `timer_bases[BASE_GLOBAL].lock`, `timer_base` locks before any `tmigr` lock, `tmigr_cpu->lock` before `tmigr_group->lock`, and child `tmigr_group->lock` before parent `tmigr_group->lock`. [F28]
- On `CONFIG_PREEMPT_RT`, `timer_base->expiry_lock` and `hrtimer_cpu_base->softirq_expiry_lock` nest outside `base->lock` and `cpu_base->lock`: `del_timer_wait_running()` must bracket `spin_lock_bh(&base->expiry_lock)` with `base->timer_waiters` inc/dec, and `timer_sync_wait_running()` / `hrtimer_sync_wait_running()` must unlock the raw spinlock before dropping and reacquiring the expiry lock. [F17, F37]
- In `expire_timers()`, `base->running_timer = NULL` must be cleared only after re-acquiring `base->lock` following `call_timer_fn()` (and before `timer_sync_wait_running()` drops `base->lock`). [F30]
- Timer shutdown (`__timer_delete()` and `__try_to_del_timer_sync()` with `shutdown=true`) must acquire `base->lock` even when `!timer_pending(timer)` to set `timer->function = NULL`, and `__mod_timer()` / `add_timer_on()` must abort under `base->lock` if `!timer->function`. [F31]
- In `__mod_timer()`, a precalculated `idx` and `bucket_expiry` from `calc_wheel_index()` may only be passed to `enqueue_timer()` if `idx != UINT_MAX && clk == base->clk` still holds after any base switch and `forward_timer_base(base)`. [F73]
- Runtime updates to `timer_base->next_expiry` under `base->lock` must use `WRITE_ONCE()`, and lockless reads in `__run_timer_base()` and `run_local_timers()` must use `READ_ONCE()`. [F34]
- In `timers_dead_cpu()`, each iteration must obtain `new_base = get_cpu_ptr(&timer_bases[b])` (never `this_cpu_ptr()`), lock `new_base->lock` before `old_base->lock`, and unlock/put in reverse order. [F20, F150]
- Every `atomic_try_cmpxchg()` on `tmigr_group::migr_state` in `tmigr_active_up()` and `tmigr_inactive_up()` must increment `newstate.seq`, and `tmigr_inactive_up()` must read `group->migr_state` with `atomic_read_acquire()` (and `smp_mb__after_atomic()` on cmpxchg failure) before reading `child->migr_state`. [F26]
- In `tmigr_requires_handle_remote_up()` and `tmigr_requires_handle_remote()`, `u64` fields `group->next_expiry` and `tmc->wakeup` may be read locklessly with `READ_ONCE()` only when `IS_ENABLED(CONFIG_64BIT)`; 32-bit requires holding `group->lock` or `tmc->lock`. [F93]

## Object Lifetimes

- In `expire_timers()` and `call_timer_fn()`, `fn(timer)` may free `timer`, so `timer->function`, `timer->flags & TIMER_IRQSAFE`, and `lockdep_copy_map(&lockdep_map, &timer->lockdep_map)` must be read before calling `fn(timer)` and `timer` never dereferenced after. `__TIMER_LOCKDEP_MAP_INITIALIZER(_kn)` must pass `&_kn` to `STATIC_LOCKDEP_MAP_INIT()`. [F2, F172]
- Stack timers (`timer_list`, `hrtimer`, `hrtimer_sleeper`) must be initialized with `*_on_stack()`, cancelled with `timer_delete_sync()` or `hrtimer_cancel()`, and cleaned up with `timer_destroy_on_stack()` or `destroy_hrtimer_on_stack()` before returning. [F165]
- Callers of `clockevents_exchange_device(old, new)` must hold `try_module_get(new->owner)` because it calls `module_put(old->owner)`, and `clockevents_replace()` must call `module_put(newdev->owner)` when a candidate `newdev` is superseded. [F7]
- `tmigr_group` instances and `tmc->tmgroup` persist across CPU offline, so `tmigr_cpu_prepare()` must return early if `tmc->tmgroup` is already non-NULL. [F9]

## API Contracts

- `schedule_timeout()`, `schedule_hrtimeout_range_clock()`, and `do_nanosleep()` must call `__set_current_state(TASK_RUNNING)` on every path that skips `schedule()`. `schedule_timeout()` clamps negative inputs/residuals to `0`. In `schedule_hrtimeout_range_clock(expires, delta, ...)`, `delta` is `u64` nanoseconds, `expires == NULL` sleeps infinitely (`-EINTR`), and `*expires == 0` returns `0` immediately. [F11, F59, F60, F189]
- `timer_delete_sync()` and `timer_shutdown_sync()` must not be called from hardirq context (or with preemption disabled on `PREEMPT_RT`) unless `TIMER_IRQSAFE` is set, nor while holding locks taken by the callback or IRQs. [F36]
- `do_init_timer()` masks `flags & TIMER_INIT_FLAGS` and does not set `timer->expires`; `add_timer*()` requires `timer->expires` set and `!timer_pending(timer)`. `add_timer_on()` and `add_timer_local()` persistently set `TIMER_PINNED` until `add_timer_global()` clears it, whereas `HRTIMER_MODE_PINNED` is ignored by `hrtimer_setup()` and only applies per `hrtimer_start_range_ns()`. [F54, F88, F184, F185]
- Callers of `fetch_next_timer_interrupt()` must preset `tevt->local = KTIME_MAX` and `tevt->global = KTIME_MAX`. [F53]
- In `tmigr_group`, `tmigr_cpu`, and `union tmigr_state`, `groupmask`, `childmask`, `active`, and `migrator` store bitmasks (`BIT(0)`..`BIT(7)`, or `TMIGR_NONE`), so child indices must be wrapped in `BIT()`, and `union tmigr_state` fields must be accessed via the packed struct rather than shifting `u32 state`. [F57, F91]
- `clockevents_switch_state()` and `clockevents_exchange_device()` require local IRQs disabled (plus `clockevents_lock` for exchange), and callers must not pre-set `dev->state_use_accessors` to the target state beforehand. `clockevents_update_freq()` must run on the device's target CPU with IRQs disabled, trying `tick_broadcast_update_freq()` before `__clockevents_update_freq()`. [F83, F144, F161]
- `clockevents_config()` only computes `mult`, `shift`, and delta bounds when `CLOCK_EVT_FEAT_ONESHOT` is set. On SMP, `clockevents_register_device()` requires a non-NULL `dev->cpumask != cpu_all_mask`. Active broadcast devices cannot be unbound via `clockevents_unbind_device()` (`-EBUSY`). [F137, F138, F141]

## Error Handling

- `hrtimer_try_to_cancel()` and `timer_delete_sync_try()` (`__try_to_del_timer_sync()`) return `-1` when the callback is running, `1` if deactivated, and `0` if inactive; callers must check `< 0` in a signed `int`. Similarly, `next_pending_bucket()` returns `-1` when empty, so `timer_recalc_next_expiry()` must check `pos >= 0` as signed `int`. [F44, F52]
- In `call_timer_fn()`, `TIMER_IRQSAFE` callbacks run with IRQs disabled and others with IRQs enabled; if `preempt_count()` changes across `fn(timer)`, `call_timer_fn()` must warn and restore `preempt_count_set(count)`. [F153, F191]
- In `tmigr_setup_groups()`, if `tmigr_get_group()` fails, the error path must `list_del(&group->list)` and `kfree(group)` for all entries in `stack` in reverse order and `kfree(stack)`. [F8]
- `clockevents_program_min_delta()` must abort on `clockevent_state_shutdown(dev)` and bound retry loops before returning `-ETIME`. Logging in `clockevents_increase_min_delta()` must use `printk_deferred()`, and `print_active_timers()` must drop `base->cpu_base->lock` before calling `print_timer()`. [F143, F167, F181]

## External Contracts

- `hrtimer_nanosleep()` must save `restart->nanosleep.clockid = t.timer.base->clockid` (not input `clockid`), and callers must preset `current->restart_block.nanosleep.type` (`TT_NONE` for `HRTIMER_MODE_ABS`). [F176, F177]
- `unbind_device_store()` must copy input via `sysfs_get_uname()`, and `dev_attr_unbind_device` must only be registered on `tick_percpu_dev`, never `tick_bc_dev`. [F95]

## Known Pitfalls

- Test relative hrtimer mode with `mode & HRTIMER_MODE_REL`, never `mode != HRTIMER_MODE_ABS` or `mode == HRTIMER_MODE_REL`. [F97]
- `clockevents_notify_released()` must use `while (!list_empty(&clockevents_released))` rather than `list_for_each_entry_safe()` because `tick_check_new_device()` can append to `clockevents_released`. [F102]
- Before passing `union tmigr_state::active` (`u8`) to `bitmap_weight()` or `find_first_bit()`, copy it to an `unsigned long` variable bounded by `BIT_CNT` (`8`). [F134]
- In `timer_debug_hint()`, add `timer_hints[i].offset` to `void *addr`, never typed `struct timer_list *timer`. In `usleep_range_state()`, zero `delta` if `max < min` and cast `(max - min)` to `u64` before `* NSEC_PER_USEC`. [F126, F128]
