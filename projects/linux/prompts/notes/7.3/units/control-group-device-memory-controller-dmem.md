# CONTROL GROUP - DEVICE MEMORY CONTROLLER (DMEM)

The Device Memory Controller (`dmem`) is a cgroup subsystem that tracks and enforces hierarchical memory limits (`min`, `low`, `max`, and current usage) for device-specific memory regions registered by drivers. Drivers register named memory regions (`dmem_cgroup_region`), charge and uncharge byte allocations against the current task's cgroup pool (`dmem_cgroup_pool_state`), and query the controller during memory pressure to check whether evicting from a candidate cgroup pool is allowed under effective protection watermarks.

## Architecture & Data Flow

- `dmem_cgroup_pool_state.cnt` stores unscaled byte counts in `struct page_counter` across `dmem_cgroup_try_charge`, `dmem_cgroup_uncharge`, `set_resource_min`, `set_resource_low`, `set_resource_max`, and `dmemcg_parse_limit`, even though `page_counter` APIs name their parameters `nr_pages`. Broken by: using `page_counter_memparse` or shifting/dividing values by `PAGE_SHIFT` or `PAGE_SIZE`. [F45]
- In `get_cg_pool_locked()`, the first upward pass (`for (p = dmemcs; p; p = parent_dmemcs(p))`) must return `pool` directly only when `p == dmemcs && pool->inited`, and must `break` to the second pass when `pool->inited` holds on an ancestor (`p != dmemcs`). [F40]
- `dmem_cgroup_state_evict_valuable()` must call `dmem_cgroup_calculate_protection(limit_pool, test_pool)`—which walks `css_for_each_descendant_pre` from `limit_pool` down to `test_pool` invoking `page_counter_calculate_protection`—before reading `test_pool->cnt.emin` or `test_pool->cnt.elow`. [F26]
- In `dmem_cgroup_state_evict_valuable()`, effective protection watermarks `ctest->emin` and `ctest->elow` are inclusive protected bounds: eviction from `test_pool` is disallowed when `used <= min` (or `used <= low` when `!ignore_low`) and allowed only when `used` is strictly greater than the active watermark. [F34]
- In `struct dmem_cgroup_pool_state`, `css_node` links the pool into `dmemcg_state.pools` whereas `region_node` links it into `dmem_cgroup_region.pools`; any `container_of` or `list_for_each_entry*` on `dmemcg_state.pools` must use `css_node` and any on `dmem_cgroup_region.pools` must use `region_node`. [F49]
- Per-cgroup limit files (`min`, `low`, `max` in `files[]`) must carry `CFTYPE_NOT_ON_ROOT` and `dmemcs_offline` must reset all pool limits via `reset_all_resource_limits`, because root pools have no parent limit enforcement and offline cgroups must not retain `min`/`low` protection while residual charges drain. [F62]
- The `files[]` `struct cftype` array passed as `legacy_cftypes` and `dfl_cftypes` in `dmem_cgrp_subsys` must end with a zero-initialized sentinel element `{ }`. [F35]

## Synchronization

- Because `get_cg_pool_locked` may drop `dmemcg_lock` on `-ENOMEM` after `alloc_pool_single` links a child `dmem_cgroup_pool_state` into `dmemcs->pools` before its ancestor `cnt.parent` chain is wired, lockless readers in `get_cg_pool_unlocked` must check `READ_ONCE(pool->inited)` and fall back to `get_cg_pool_locked` when false. [F10]
- In `struct dmemcg_state`, the `pools` list (`dmem_cgroup_pool_state.css_node`) must be modified under `dmemcg_lock` using RCU list primitives (`list_add_tail_rcu` in `alloc_pool_single` and `list_del_rcu` in `dmem_cgroup_unregister_region`) while the CSS is live, and traversed under `rcu_read_lock()` or `dmemcg_lock`. [F7]
- `dmemcg_get_region_by_name()` must be called under `rcu_read_lock()` (or `dmemcg_lock`) and obtain a non-zero reference via `kref_get_unless_zero(&region->ref)` before returning `region`, paired with `call_rcu(&cgregion->rcu, dmemcg_free_rcu)` in `dmemcg_free_region()` when the last `kref` is dropped. [F55]
- `dmem_cgroup_calculate_protection()` must hold `rcu_read_lock()` across the entire `css_for_each_descendant_pre()` traversal and the inner `list_for_each_entry_rcu()` scan of `dmemcg_iter->pools` through the `page_counter_calculate_protection()` call. [F57]
- `dmemcg_lock` is acquired with non-IRQ-saving `spin_lock()`/`spin_unlock()`, so no function that acquires `dmemcg_lock` (`dmemcs_free()`, `dmem_cgroup_unregister_region()`, `dmem_cgroup_register_region()`, `get_cg_pool_unlocked()`, `dmem_cgroup_try_charge()`) may be invoked from hardirq or softirq context. [F53]

## Object Lifetimes

- `dmem_cgroup_try_charge` acquires a CSS reference via `get_current_dmemcs()` and, on success, transfers both that CSS reference and the `page_counter` byte charge to `*ret_pool`, which the caller must release via `dmem_cgroup_uncharge` with the same `pool` and `size` (never `dmem_cgroup_pool_state_put`). [F1]
- When `dmem_cgroup_try_charge` fails with `-EAGAIN` and `ret_limit_pool` is non-NULL, it must take `css_get(&(*ret_limit_pool)->cs->css)` before `css_put(&cg->css)` at `err:` (since `cg->css` pins the ancestor hierarchy containing `fail`), and the caller must release `*ret_limit_pool` via `dmem_cgroup_pool_state_put`. [F2, F13]
- Each `dmem_cgroup_pool_state` is linked into both `dmemcs->pools` (`css_node`) and `region->pools` (`region_node`): `dmem_cgroup_unregister_region` must unlink `pool->css_node` under `dmemcg_lock` before `dmemcg_free_rcu` calls `free_cg_pool`, while `dmemcs_free` must unlink both `pool->css_node` and `pool->region_node` under `dmemcg_lock`. [F5]

## API Contracts

- `dmem_cgroup_try_charge` initializes `*ret_pool` (and `*ret_limit_pool` if non-NULL) to `NULL` on entry, sets `*ret_pool` only when returning `0` with `CONFIG_CGROUP_DMEM` enabled, and sets `*ret_limit_pool` only when returning `-EAGAIN`, leaving `*ret_limit_pool` as `NULL` on other errors. [F16]
- `dmem_cgroup_state_evict_valuable()` never clears `*ret_hit_low` to `false` (including in the `!CONFIG_CGROUP_DMEM` stub) and unconditionally dereferences `ret_hit_low` when `!ignore_low` and `used <= low`; callers must pass a non-NULL `ret_hit_low` initialized to `false` before each pass, and `if (used <= min) return false;` must be evaluated before `if (!ignore_low)` so `*ret_hit_low` is set to `true` only when `used > min`, `!ignore_low`, and `used <= low`. [F18, F39]
- `dmem_cgroup_state_evict_valuable()` must check `if (limit_pool == test_pool) return true;` before testing `if (limit_pool)` or dereferencing `test_pool`, both to allow a cgroup pool to evict from itself regardless of `min`/`low` watermarks and to handle `limit_pool == NULL && test_pool == NULL`. [F37]
- Every getter callback `fn` passed to `dmemcg_limit_show` must accept a `NULL` `struct dmem_cgroup_pool_state *` and return the default unconfigured value, because `find_cg_pool_locked` returns `NULL` when the cgroup has not yet allocated a pool for a region. [F48]
- Callers of `dmem_cgroup_uncharge` must pass the exact byte `size` that was charged to `@pool` by `dmem_cgroup_try_charge`. [F47]
- `dmem_cgroup_try_charge()`, `dmem_cgroup_register_region()`, `get_cg_pool_unlocked()`, and `dmemcg_limit_write()` may perform `GFP_KERNEL` allocations and must only be called from sleepable process context. [F52]

## External Contracts

- The `!CONFIG_CGROUP_DMEM` stubs in `include/linux/cgroup_dmem.h` and implementations in `kernel/cgroup/dmem.c` must stay synchronized, both zeroing `*ret_pool` and `*ret_limit_pool` in `dmem_cgroup_try_charge` and accepting `NULL` pools in `dmem_cgroup_uncharge` and `dmem_cgroup_pool_state_put`. [F30]
- Region names formatted by `dmem_cgroup_register_region` must not contain whitespace (`' '`, `'\t'`, or `'\n'`), because `dmem_cgroup_region_capacity_show` and `dmemcg_limit_show` emit `<region->name> <value>\n` lines and `dmemcg_limit_write` splits each line at the first space or tab via `strsep(&options, " \t")`. [F31]

## Known Pitfalls

- In `dmem_cgroup_calculate_protection()`, `found_pool` must be reset to `NULL` on each `css_for_each_descendant_pre()` iteration and assigned only when `pool->region == limit_pool->region`, rather than using the `list_for_each_entry_rcu()` cursor `pool` after the inner loop terminates. [F36]
- When `limit_pool` is `NULL` in `dmem_cgroup_state_evict_valuable()`, the ancestor walk `for (limit_pool = test_pool; pool_parent(limit_pool); limit_pool = pool_parent(limit_pool))` must stop while `pool_parent(limit_pool)` is non-NULL so `limit_pool` remains pointing to the root pool rather than advancing to `NULL`. [F38]
