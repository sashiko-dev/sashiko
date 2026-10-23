# ZSWAP COMPRESSED SWAP CACHING

Zswap is a RAM-based compressed cache for swap pages in the Linux memory management subsystem. It intercepts folios being swapped out, compresses their pages using the crypto acomp API into zsmalloc memory pools indexed by swap-offset XArrays, and decompresses them on swap-in. When memory pressure occurs or pool limits are reached, a memcg-aware shrinker and background worker evict cold compressed entries back to the underlying swap device.

## Architecture & Data Flow

- When `zswap_store()` returns `false` for any reason (including `zswap_enabled == false`, cgroup/pool limit rejection, or mid-folio `zswap_store_page()` failure), the `check_old:` loop must iterate across all `index = 0` to `nr_pages - 1` and `xa_erase()` plus `zswap_entry_free()` any entry at `offset + index`. Broken by: returning `false` before `check_old:` or only erasing up to the failed `index`. [F71]
- `struct zswap_entry::objcg` is nullable (when `get_obj_cgroup_from_folio()` returns `NULL`), so every path accessing `entry->objcg`—including `mem_cgroup_from_entry()`, `zswap_entry_free()`, `zswap_writeback_entry()`, `zswap_store_page()`, and `zswap_load()`—must check that `objcg` is non-NULL before calling `obj_cgroup_*` helpers (`obj_cgroup_memcg()`, `obj_cgroup_charge_zswap()`, `obj_cgroup_uncharge_zswap()`, `count_objcg_events()`). [F36]
- Whenever `zswap_store()` fails (`!ret`) while `zswap_pool_reached_full` is true, it must queue `zswap_shrink_work`, and `shrink_worker()` must loop until `zswap_total_pages() <= zswap_accept_thr_pages()` (or `failures == MAX_RECLAIM_RETRIES`). Broken by: queuing `zswap_shrink_work` only when `cur_pages >= max_pages` or shrinking only one page/memcg per `shrink_worker()` run. [F69]
- Global synchronization primitives and counters accessed by external MM hooks when zswap is disabled or uninitialized—such as `zswap_shrink_lock` in `zswap_memcg_offline_cleanup()` and `zswap_stored_pages`—must be statically initialized at compile time (`DEFINE_SPINLOCK()` / `ATOMIC_LONG_INIT()`) rather than in `zswap_setup()`. [F26]
- Once a pool is created in `zswap_setup()`, `static_branch_enable(&zswap_ever_enabled)` must be called so `zswap_never_enabled()` remains `false` even if `zswap_enabled` is later disabled, preventing swap-in paths from attempting unsupported large-folio swap-ins. Broken by: disabling `zswap_ever_enabled` or checking `zswap_is_enabled()` instead of `zswap_never_enabled()`. [F95]
- When `zswap_setup()` fails to create the initial pool or fails core initialization, it must set `zswap_enabled = false` (and leave `zswap_has_pool` false or set `zswap_init_state = ZSWAP_INIT_FAILED`), and `zswap_enabled_param_set()` must reject enabling zswap unless `zswap_init_state == ZSWAP_INIT_SUCCEED` and `zswap_has_pool` is true. [F53]
- `zswap_pool_create()` must generate a unique pool name for `zs_create_pool()` using `atomic_inc_return(&zswap_pools_count)` so creating a new `zswap_pool` does not collide with an older pool still draining in `zswap_pools`. [F52]
- `zswap_swapon()` must compute the XArray count `nr` using ceiling division `DIV_ROUND_UP(nr_pages, ZSWAP_ADDRESS_SPACE_PAGES)` so every swap offset up to `nr_pages - 1` maps to an allocated `struct xarray` when `swap_zswap_tree()` indexes `zswap_trees[type]` by `swp_offset(swp) >> ZSWAP_ADDRESS_SPACE_SHIFT`. [F74]

## Synchronization

- Before `shrink_memcg_cb()` unlocks `l->lock` to call `zswap_writeback_entry()`, it must rotate `item` to the tail of `l->list` (`list_move_tail()`) and copy `entry->swpentry` to the stack, and it must return `LRU_REMOVED_RETRY`, `LRU_RETRY`, or `LRU_STOP` so `list_lru` knows the lock was dropped and advances past failed entries. Broken by: reading `entry->swpentry` after `spin_unlock(&l->lock)`, omitting `list_move_tail()`, or returning `LRU_ROTATE`/`LRU_SKIP`. [F65]
- All modifications to `zswap_pools` must use RCU list primitives under `spin_lock_bh(&zswap_pools_lock)`, and lockless readers (`zswap_pool_current_get()`, `zswap_pool_find_get()`) must hold `rcu_read_lock()` and pin any pool used outside RCU via `zswap_pool_tryget()`. `zswap_compressor_param_set()` must release `zswap_pools_lock` before calling `zswap_pool_create()`, `param_set_charp()`, or `percpu_ref_kill()`, which sleep or do `GFP_KERNEL` allocations. [F19, F82]
- Because `__zswap_pool_empty()` is a `percpu_ref` release callback that may run in atomic/softirq context, it must only unlink `pool->list` via `list_del_rcu()` under `spin_lock_bh(&zswap_pools_lock)` and defer blocking teardown to `__zswap_pool_release()` via `schedule_work(&pool->release_work)`, which calls `synchronize_rcu()` in process context before `percpu_ref_exit()` and `zswap_pool_destroy()`. [F18, F80]
- Runtime inspection and transitions of `zswap_init_state` and lazy invocation of `zswap_setup()` in `zswap_enabled_param_set()` and `zswap_compressor_param_set()` must be serialized under `zswap_init_lock`. [F25]

## Object Lifetimes

- Whenever `zswap_next_shrink` is updated under `zswap_shrink_lock` in `shrink_worker()` or `zswap_memcg_offline_cleanup()`, `mem_cgroup_iter()` must loop until `zswap_next_shrink` is `NULL` or online, and `shrink_worker()` must take an extra reference with `mem_cgroup_tryget_online()` and drop it with `mem_cgroup_put()` after `shrink_memcg()`. [F9]
- In `zswap_compressor_param_set()`, reusing an existing pool from `zswap_pool_find_get()` requires calling `percpu_ref_resurrect(&pool->ref)` and dropping the lookup reference with `zswap_pool_put(pool)`. When `param_set_charp()` succeeds (`!ret`), `put_pool` must capture the old `zswap_pool_current()` before `pool` is added at the head of `zswap_pools`; when `ret != 0` and `pool != NULL`, `pool` must be re-linked/added to the tail via `list_add_tail_rcu()` and assigned to `put_pool` before `percpu_ref_kill(&put_pool->ref)`. [F11, F72]
- Because `zswap_entry_cache` is created with `KMEM_CACHE(zswap_entry, 0)` without zeroing, `zswap_store_page()` must explicitly initialize every `struct zswap_entry` field before adding `entry` to `zswap_list_lru`, and error paths before initialization must call `zswap_entry_cache_free()` rather than `zswap_entry_free()`. [F38]

## API Contracts

- Callers of `zswap_store()`, `zswap_load()`, and `zswap_folio_swapin()` must hold the folio lock on the passed folio, and `zswap_store()` additionally requires `folio_test_swapcache(folio)`. [F24]
- `zswap_load()` only supports single-page (order-0) folios and must reject any large folio (`folio_test_large(folio)`) by unlocking the folio and returning `-EINVAL` before looking up or decompressing a single-page `zswap_entry`. [F60]
- `swap_zswap_tree(swp)` selects the XArray chunk via `swp_offset(swp) >> ZSWAP_ADDRESS_SPACE_SHIFT`, while XArray calls (`xa_load()`, `xa_store()`, `xa_erase()`) on that tree must pass the unshifted `swp_offset(swp)` as the index, and loops over multi-page folios must re-evaluate `swap_zswap_tree()` for each page's `offset + index`. [F42]
- `shrink_memcg_cb()` must treat its `void *arg` (`encountered_page_in_swapcache`) as nullable and check that it is non-NULL before dereferencing it or returning `LRU_STOP` on `-EEXIST`, because `zswap_shrinker_scan()` passes `&encountered_page_in_swapcache` whereas `shrink_memcg()` passes `NULL`. [F32]
- `zswap_shrinker_scan()` must return `SHRINK_STOP` (and not `0`) whenever shrinking is disabled, aborted due to `encountered_page_in_swapcache`, or `list_lru_shrink_walk()` reclaims zero objects (`!shrink_ret`). [F34]
- `obj_cgroup_charge_zswap()` and `obj_cgroup_uncharge_zswap()` take the compressed object size in bytes (`entry->length`), whereas `zswap_shrinker_count()` must convert `memcg_page_state(memcg, MEMCG_ZSWAP_B)` from bytes to pages via `>> PAGE_SHIFT` so `nr_backing` matches the page units of `MEMCG_ZSWAPPED` and `nr_freeable` in `mult_frac()`. [F40]
- `zswap_store()` must obtain the `obj_cgroup` from the folio being swapped out via `get_obj_cgroup_from_folio(folio)` and store it on `entry->objcg` so memory charges, uncharges, `list_lru` tracking, and VM events (`ZSWPOUT`/`ZSWPIN`/`ZSWPWB`) are billed to the folio's cgroup rather than `current`. [F99]
- `zswap_store()` must preallocate the per-memcg `list_lru` via `memcg_list_lru_alloc(memcg, &zswap_list_lru, GFP_KERNEL)` before calling `zswap_store_page()`, so that `list_lru_add()` inside `zswap_lru_add()` under `rcu_read_lock()` cannot fail. [F102]
- In `zswap_compressor_param_set()`, `kp->arg` points to `&zswap_compressor` (`char **`) and must be dereferenced as `*(char **)kp->arg`, and the user-supplied string must be stripped with `strstrip()` and (when `zswap_init_state == ZSWAP_INIT_SUCCEED`) validated with `crypto_has_acomp()` before looking up or creating a `zswap_pool` (whose `tfm_name` is bounded to `CRYPTO_MAX_ALG_NAME` via `strscpy()`). [F62, F77]
- `zswap_init()` must be registered with `late_initcall()`, and `zswap_enabled_param_set()` must return `param_set_bool(val, kp)` immediately without calling `zswap_setup()` when `system_state != SYSTEM_RUNNING`. [F105]

## Error Handling

- In `zswap_compress()`, the `unsigned long` handle returned by `zs_malloc()` encodes allocation failures as error values rather than `0`, so callers must test `IS_ERR_VALUE(handle)` and extract the errno via `PTR_ERR((void *)handle)` before calling `zs_obj_write()` or storing `entry->handle`. Broken by: checking `if (!handle)`. [F28]
- In `zswap_store_page()`, the return value `old` from `xa_store()` must be checked with `xa_is_err(old)` before testing `if (old)` and calling `zswap_entry_free(old)`, because `xa_store()` returns XArray error pointers on allocation failure and the displaced `struct zswap_entry *` on overwrite. [F29]

## External Contracts

- In `include/linux/zswap.h`, the `!CONFIG_ZSWAP` stub for `zswap_never_enabled()` must return `true` (while `zswap_is_enabled()` returns `false`), matching the runtime state when zswap has never been enabled. [F56]

## Known Pitfalls

- In `zswap_shrinker_count()`, subtracting `nr_disk_swapins` from unsigned `nr_freeable` must clamp `nr_remain` to `0` when `nr_freeable >= nr_disk_swapins_cur` and subtract `nr_disk_swapins_cur - nr_remain` so `nr_freeable` never underflows below zero. [F61]
- When scaling `nr_freeable` by the backing-to-stored page ratio in `zswap_shrinker_count()`, `mult_frac(nr_freeable, nr_backing, nr_stored)` must be used instead of `(nr_freeable * nr_backing) / nr_stored` to avoid overflowing `unsigned long`. [F75]
