# MM Reclaim, Swap, and Migration

## Writeback Tags

Incorrect tag handling causes data loss (dirty pages skipped during sync) or
writeback livelock (sync never completes because new dirty pages keep appearing).
Review any code that starts writeback or implements `->writepages`.

Page cache tags defined as `PAGECACHE_TAG_*` in `include/linux/fs.h`:

| Tag | XA Mark | Purpose |
|-----|---------|---------|
| PAGECACHE_TAG_DIRTY | XA_MARK_0 | Folio has dirty data needing writeback |
| PAGECACHE_TAG_WRITEBACK | XA_MARK_1 | Folio is currently under IO |
| PAGECACHE_TAG_TOWRITE | XA_MARK_2 | Folio tagged for current writeback pass |

**Tag lifecycle:**
1. `folio_mark_dirty()` sets PAGECACHE_TAG_DIRTY
2. `tag_pages_for_writeback()` in `mm/page-writeback.c` copies DIRTY to TOWRITE
   for data-integrity syncs, preventing livelocks from new dirty pages
3. `folio_start_writeback()` (macro for `__folio_start_writeback(folio, false)`,
   defined in `include/linux/page-flags.h`):
   - Sets PAGECACHE_TAG_WRITEBACK
   - Clears PAGECACHE_TAG_DIRTY if the folio's dirty flag is not set
   - Clears PAGECACHE_TAG_TOWRITE (because `keep_write` is false)
4. To preserve PAGECACHE_TAG_TOWRITE, call `__folio_start_writeback(folio, true)`

**Tag selection** (see `wbc_to_tag()` in `include/linux/writeback.h`):
- `wbc_to_tag()` returns PAGECACHE_TAG_TOWRITE for `WB_SYNC_ALL` or
  `tagged_writepages` mode, PAGECACHE_TAG_DIRTY otherwise
- Data-integrity syncs (`WB_SYNC_ALL`) iterate TOWRITE so pages dirtied after
  the sync starts are not included

## Cgroup Writeback Domain Abstraction

Code receiving a `dirty_throttle_control *dtc` must use `dtc_dom(dtc)` for the
domain, not `global_wb_domain` directly. `balance_dirty_pages()` in
`mm/page-writeback.c` selects between global (`gdtc`) and memcg (`mdtc`)
domains based on `pos_ratio`; hardcoding global values produces wrong
throttling when the memcg domain is selected.

**REPORT as bugs**: `global_wb_domain` field access in functions/traces that
receive a `dtc` parameter, except code explicitly operating on the global
domain (e.g., `global_dirty_limits()`).

## Swap Cache Residency

A folio enters the swap cache via `add_to_swap()` during reclaim and remains
until explicitly removed. `folio_free_swap()` in `mm/swapfile.c` removes it
only when `folio_swapcache_freeable(folio)` (swapcache, not under writeback,
storage not suspended) AND `!folio_swapped(folio)` (no swap references remain).

**Large folio swapin conflicts:** mTHP swapin fails with `-EEXIST` when any
subpage's swap slot is already occupied by a smaller folio from a racing
swapin. The path must fall back to smaller order or retry (see
`shmem_swapin_folio()` in `mm/shmem.c`).

**Swap entry reuse (ABA problem):** swap entries are recycled -- the same
`swp_entry_t` can be reassigned to a different folio. `pte_same()` on swap
PTEs only confirms the entry value, not folio identity. After locking a folio
from `swap_cache_get_folio()` on a swap entry, verify
`folio_test_swapcache(folio)` and `folio->swap.val` still match. After
acquiring the PTE lock when an earlier lookup returned NULL, check
`SWAP_HAS_CACHE` in `si->swap_map[swp_offset(entry)]`. See `move_swap_pte()`
in `mm/userfaultfd.c`.

## Swap Device Lifetime

Accessing a `swap_info_struct` without a reference allows `swapoff` to free it
concurrently, causing use-after-free. `swapoff()` calls `percpu_ref_kill()` on
`si->users` followed by `synchronize_rcu()` before freeing structures.

- `get_swap_device(entry)` in `mm/swapfile.c` validates the entry and takes a
  `percpu_ref` on `si->users`. Returns NULL if the device is being swapped off.
  Must be paired with `put_swap_device(si)` (in `include/linux/swap.h`)
- `__swap_entry_to_info(entry)` in `mm/swap.h` returns the `swap_info_struct`
  pointer WITHOUT taking a reference -- only safe when the caller already holds
  a reference or is inside an RCU read-side section
- `__read_swap_cache_async()` in `mm/swap_state.c` uses
  `__swap_entry_to_info()` internally without pinning the device. All callers
  must hold a device reference for the entry being read
- **Cross-device readahead hazard**: readahead code that iterates page table
  entries (VMA readahead) may encounter swap entries from different devices than
  the target. The caller typically holds a device reference only for the target
  entry's device. Each entry from a different device must be separately pinned
  with `get_swap_device()` or skipped on failure (see `swap_vma_readahead()` in
  `mm/swap_state.c`)

## Dual Reclaim Paths: Classic LRU vs MGLRU

`mm/vmscan.c` has two parallel reclaim implementations that must maintain
identical vmstat, memcg event, and tracepoint coverage. MGLRU is runtime-
selectable, so bugs only manifest when the other path is active.

- Classic: `shrink_inactive_list()` / `shrink_active_list()`
- MGLRU: `evict_folios()` / `scan_folios()`

Both call `shrink_folio_list()` but each has its own post-reclaim stat
updates. When modifying vmstat counters, memcg events, or tracepoints in
one function, verify the corresponding change in the other. The pairing
is `shrink_inactive_list()` ↔ `evict_folios()` and `shrink_active_list()`
↔ `scan_folios()`.

## MGLRU Generation and Tier Bit Consistency

When a folio moves to a new generation, its tier bits (`LRU_REFS_FLAGS`,
defined as `LRU_REFS_MASK | BIT(PG_referenced)` in `include/linux/mmzone.h`)
must be cleared so tier tracking starts fresh. Stale tier bits inflate access
counts and distort eviction. All paths that update `LRU_GEN_MASK` in
`folio->flags` must also clear `LRU_REFS_FLAGS` via
`old_flags & ~(LRU_GEN_MASK | LRU_REFS_FLAGS)`. This is done in
`folio_update_gen()` and `folio_inc_gen()` in `mm/vmscan.c`.
`lru_gen_add_folio()` in `include/linux/mm_inline.h` sets the generation
but does not clear `LRU_REFS_FLAGS` -- it clears `LRU_GEN_MASK | BIT(PG_active)`.

**Review any code modifying `LRU_GEN_MASK` in `folio->flags`** to verify it
also handles `LRU_REFS_FLAGS`.

## Shmem Folio Cache Residency

Confusing `folio_test_swapbacked()` with `folio_test_swapcache()` causes
xarray corruption, incorrect VM statistics accounting, and wrong
branching in migration and reclaim paths, because shmem folios can be in
two different cache states that require different handling.

**The three folio cache states for shmem:**

| State | `swapbacked` | `swapcache` | `folio->mapping` | xarray location |
|-------|-------------|-------------|-------------------|-----------------|
| Shmem in page cache | true | false | shmem inode `address_space` | `mapping->i_pages` (single multi-order entry) |
| Shmem in swap cache | true | true | NULL | swap address space (N individual entries) |
| Anonymous in swap cache | true | true | `anon_vma` (with `FOLIO_MAPPING_ANON` flag) | swap address space (N individual entries) |

A shmem folio is in either the page cache or the swap cache, never both
simultaneously. Once moved to swap cache, `folio->mapping` is set to
NULL and the folio is no longer associated with the shmem inode mapping.

**`folio_test_swapbacked()` vs `folio_test_swapcache()`:**
- `folio_test_swapbacked()` tests `PG_swapbacked`: true for all anonymous
  and shmem folios (both page-cache-resident and swap-cache-resident).
  It indicates the folio *can use* swap as backing storage
- `folio_test_swapcache()` tests both `PG_swapbacked` AND `PG_swapcache`:
  true only when the folio is *currently in* the swap cache
- Using `folio_test_swapbacked()` as a proxy for "is in swap cache" is
  wrong because it also matches shmem folios that are in the page cache

**Xarray storage models:**
- **Page cache** (`mapping->i_pages`): stores a single multi-order xarray
  entry for a large folio. Operations use `xas_store()` once
- **Swap cache**: stores N individual entries, one per subpage of the
  folio. Operations must iterate all N slots

Code that branches on cache type to choose between single-entry and
multi-entry xarray operations must use `folio_test_swapcache()`, not
`folio_test_swapbacked()`. See `__folio_migrate_mapping()` in
`mm/migrate.c` which uses `folio_test_swapcache()` to select the
swap-cache-specific replacement path (`__swap_cache_replace_folio()`).

## Memcg Charge Lifecycle

Every `mem_cgroup_charge()` must have a corresponding `mem_cgroup_uncharge()`
on the free path. On migration, charge transfers via `mem_cgroup_migrate()` --
the old folio is NOT uncharged separately. `folio_unqueue_deferred_split()`
must precede uncharging to avoid accessing freed memcg data.

**Memcg lookup safety:**
- `folio_memcg()` returns NULL for uncharged folios; may return an offline
  memcg (folios retain `memcg_data` until reparented)
- Operations that charge/record/uncharge must use the resolved online ancestor
  from `mem_cgroup_id_get_online()` consistently. Refactorings replacing an
  explicit memcg parameter with `folio_memcg()` introduce a mismatch (counter
  targets online ancestor, recorded ID is the offline memcg), causing permanent
  counter leaks when cgroups are deleted under pressure
- `mem_cgroup_from_id()` returns an RCU-protected pointer valid only under
  `rcu_read_lock()`. Use `mem_cgroup_tryget()` before `rcu_read_unlock()` to
  extend lifetime. `get_mem_cgroup_from_*()` functions acquire a reference
  internally

**Per-CPU stock drain:** charges are batched in per-CPU stocks. Destroying a
memcg requires `drain_all_stock()` (`mm/memcontrol.c`) -- missing this
prevents cgroup deletion.

## Folio Migration and Sleeping Constraints

`folio_mc_copy()` in `mm/util.c` calls `cond_resched()` between pages -- safe
for order-0 (loop exits before resched) but sleeps for large folios. This
makes `filemap_migrate_folio()` / `migrate_folio()` / `__migrate_folio()`
sleeping operations for large folios.

**REPORT as bugs**: `migrate_folio` callbacks (in `address_space_operations`)
that hold a spinlock while calling these functions. Use non-blocking state
flags instead (e.g., `BH_Migrate` in `__buffer_migrate_folio()` in
`mm/migrate.c`).

## Folio Isolation for Migration

Not every folio that qualifies for migration is added to the isolation list:
device-coherent folios skip it, `folio_isolate_lru()` can fail, etc.
`collect_longterm_unpinnable_folios()` in `mm/gup.c` returns a count of all
unpinnable folios, not just those listed.

**REPORT as bugs**: using `list_empty()` on a migration list as proxy for
"no qualifying items" when the collection has early-continue paths. Use an
explicit count instead.

## Quick Checks

- **Bounded iteration under LRU locks**: skipping LRU entries without
  advancing the termination counter creates unbounded spinlock-held scans.
  Skip paths must either advance the counter or have an independent bound
  (e.g., `SWAP_CLUSTER_MAX_SKIPPED`). Applies to any spinlock-held list
  filtering loop
- **Migration lock scope across unmap and remap phases**: if
  `TTU_RMAP_LOCKED` is passed to `try_to_migrate()`, `i_mmap_rwsem` must
  stay held until `remove_migration_ptes()` with `RMP_LOCKED`. Dropping
  between phases creates ABBA deadlock (`folio_lock` → `i_mmap_rwsem` vs
  reverse). Anon vs file-backed use different locks — fixes for one may
  break the other. See `unmap_and_move_huge_page()` in `mm/migrate.c`
- **kswapd order-dropping and watermark checks**: `kswapd_shrink_node()`
  drops `sc->order` to 0 after reclaiming `compact_gap(order)` pages. Watermark
  checks in `pgdat_balanced()`/`balance_pgdat()` that use stricter high-order
  metrics must check `order != 0`, not a static mode flag. Ignoring the
  dynamic order drop causes massive overreclaim
- **`folio_putback_lru()` requires valid memcg**: after
  `mem_cgroup_migrate()` clears the source folio's `memcg_data`,
  `folio_putback_lru()` triggers a memcg assert. Use plain `folio_put()`
  for the source folio. See `migrate_folio_move()` in `mm/migrate.c`
- **Swap allocator local lock scope**: `folio_alloc_swap()` runs under
  `local_lock()` (preemption disabled). No sleeping operations reachable
  from this scope. Drop the lock for sleeping ops. Silent in typical testing;
  fires under `CONFIG_DEBUG_PREEMPT` or PREEMPT_RT
- **Zone skip criteria consistency in vmscan**: zone-skip logic must be
  consistent across `balance_pgdat()`, `pgdat_balanced()`,
  `allow_direct_reclaim()`, and `skip_throttle_noprogress()`. If one counts a
  zone another skips, `kswapd_failures` escape hatch may never fire, causing
  infinite loops in `throttle_direct_reclaim()`
- **Counter-gated tracking list removal**: list membership gated by a
  resource counter (e.g., `shmem_swaplist` requires `info->swapped > 0`).
  Error paths must check the counter before `list_del_init()` — the object
  may already be on the list from a prior operation. Unconditional removal
  causes iterators to loop forever unable to find remaining resources
- **List iteration with lock drop**: `list_for_each_entry_safe` is not safe
  when the lock is dropped mid-iteration. Concurrent `list_del_init()` makes
  the element self-referential → infinite loop. After reacquiring, check
  `list_empty()` and restart from head. See `shmem_unuse()` in `mm/shmem.c`
