# Memory Management Subsystem Details

## PTE State Consistency

Incorrect PTE flag combinations cause data corruption (dirty data silently
dropped), security holes (writable pages that should be read-only), and kernel
crashes on architectures that trap invalid combinations. Review any code that
constructs or modifies PTEs for these invariants.

**Invariants** (software-enforced, not hardware):
- Writable PTEs must be dirty: a clean+writable PTE is invalid
  - For shared mappings, `can_change_shared_pte_writable()` in `mm/mprotect.c`
    enforces this by only returning true when `pte_dirty(pte)` (clean shared
    PTEs need a write-fault for filesystem writenotify)
  - For private/anonymous mappings, code paths use `pte_mkwrite(pte_mkdirty(entry))`
    to set both together (see `do_anonymous_page()` in `mm/memory.c`,
    `migrate_vma_insert_page()` in `mm/migrate_device.c`)
  - **Exception -- MADV_FREE**: `madvise_free_pte_range()` in `mm/madvise.c`
    clears the dirty bit via `clear_young_dirty_ptes()` but preserves write
    permission, intentionally creating a clean+writable PTE. This allows the
    page to be reclaimed without writeback (it's clean and lazyfree), but if
    the process writes new data before reclaim, the page becomes dirty again
    without a full write-protect fault. On x86, `pte_mkclean()` only clears
    `_PAGE_DIRTY_BITS` and does not touch `_PAGE_RW`, so hardware sets dirty
    directly with no fault at all. On arm64, `pte_mkclean()` sets `PTE_RDONLY`
    but preserves `PTE_WRITE`; with FEAT_HAFDBS hardware clears `PTE_RDONLY`
    on write (no fault), without it a minor fault resolves quickly since
    `pte_write()` is still true
- Dirty implies accessed as a software convention: `pte_mkdirty()` does NOT
  set the accessed bit (x86, arm64), so code paths must set both explicitly
- Non-accessed+writable is invalid on architectures without hardware A/D bit
  management (on x86, hardware sets accessed automatically on first access)

**Migration entries** (`include/linux/swapops.h`):
- Encode A/D bits via `SWP_MIG_YOUNG_BIT` and `SWP_MIG_DIRTY_BIT`
- Only available when `migration_entry_supports_ad()` returns true (depends on
  whether the architecture's swap offset has enough free bits; controlled by
  `swap_migration_ad_supported` in `mm/swapfile.c`)
- `make_migration_entry_young()` / `make_migration_entry_dirty()` preserve
  original PTE state into the migration entry
- `remove_migration_pte()` in `mm/migrate.c` restores A/D bits: dirty is set
  only if both the migration entry AND the folio are dirty (avoids re-dirtying
  a folio that was cleaned during migration)

**NUMA balancing** (see `change_pte_range()` in `mm/mprotect.c`):
- Skips PTEs already `pte_protnone()` to avoid double-faulting
- Checks `folio_can_map_prot_numa()` before applying NUMA hint faults

**Swap entries** (see `try_to_unmap_one()` in `mm/rmap.c`):
- Only exclusive, soft-dirty, and uffd-wp flags survive in swap PTEs;
  all other PTE state is lost on swap-out
- `pte_swp_clear_flags()` in `include/linux/swapops.h` strips these flags
  to extract the bare swap entry for comparison (see `pte_same_as_swp()`
  in `mm/swapfile.c`)

## Large Folio State Tracking

Misunderstanding which flags are per-page vs per-folio leads to bugs where code
checks or sets state on the wrong struct page. A common mistake is assuming all
flags work like small pages when operating on subpages of a large folio.

The Tracking Level column indicates where the state lives. **Per-folio** means a
single value on the head page applies to the entire folio. **Per-page** means
each subpage carries its own independent value. **PTE-level** means the state is
in the page table entry, not in struct page at all. **Mixed** means the
granularity depends on how the folio is mapped.

| State | Tracking Level | Details |
|-------|---------------|---------|
| PageAnonExclusive | **Mixed** | Per-page for PTE-mapped THP; per-folio (head page) for PMD-mapped and HugeTLB (see `PG_anon_exclusive` in `include/linux/page-flags.h` and `RMAP_EXCLUSIVE` handling in `__folio_add_rmap()` in `mm/rmap.c`) |
| PG_hwpoison | **Per-page** | Marks the specific corrupted subpage (`PF_ANY`); distinct from `PG_has_hwpoisoned` (`PF_SECOND`) which only indicates at least one subpage is poisoned. Both needed: per-page flag identifies which page, per-folio flag enables fast folio-level check |
| PG_dirty | **Per-folio** | Single flag on head page via `PF_HEAD` policy; PTE-level dirty bits tracked separately in page table entries |
| Accessed/young | **PTE-level** | Tracked in page table entries, not in struct page; folio-level `PG_referenced` on head page is a separate LRU aging flag |
| Reference count | **Per-folio** | Single `_refcount` on head page shared by all subpages (see `folio_ref_count()` in `include/linux/page_ref.h`) |
| Mapcount | **Per-page** | Each subpage has `_mapcount` by default; `CONFIG_NO_PAGE_MAPCOUNT` (experimental) eliminates per-page mapcounts, using only folio-level `_large_mapcount` and `_entire_mapcount` (see `include/linux/mm_types.h`) |

**Page flag policies** control which struct page within a folio carries each flag.
Using the wrong page silently reads stale data or corrupts unrelated state. See
the "Page flags policies wrt compound pages" comment block in `include/linux/page-flags.h`:
- `PF_HEAD`: flag operations redirect to head page (most flags)
- `PF_ANY`: flag is relevant for head, tail, and small pages
- `PF_NO_TAIL`: modifications only on head/small pages, reads allowed on tail
- `PF_SECOND`: flag stored in the first tail page (e.g., `PG_has_hwpoisoned`,
  `PG_large_rmappable`, `PG_partially_mapped`)

## GFP Flags Context

Using the wrong GFP flag causes sleeping in atomic context (deadlock/BUG),
filesystem or IO recursion (deadlock), or silent allocation failures when the
caller assumes success. Verify the allocation context matches the flag.

The Reclaim column indicates which memory reclaim mechanisms are available.
"kswapd only" means the allocation wakes the background kswapd thread but never
blocks waiting for reclaim to complete. "Full" means the caller may also perform
direct reclaim synchronously, blocking until pages are freed.

| Flag | Sleeps | Reclaim | Key Flags | Use Case |
|------|--------|---------|-----------|----------|
| GFP_ATOMIC | No | kswapd only | `__GFP_HIGH \| __GFP_KSWAPD_RECLAIM` | IRQ/spinlock context, lower watermark access |
| GFP_KERNEL | Yes | Full (direct + kswapd) | `__GFP_RECLAIM \| __GFP_IO \| __GFP_FS` | Normal kernel allocation |
| GFP_NOWAIT | No | kswapd only | `__GFP_KSWAPD_RECLAIM \| __GFP_NOWARN` | Non-sleeping, likely to fail |
| GFP_NOIO | Yes | Direct + kswapd, no IO | `__GFP_RECLAIM` | Avoid block IO recursion |
| GFP_NOFS | Yes | Direct + kswapd, no FS | `__GFP_RECLAIM \| __GFP_IO` | Avoid filesystem recursion |

See "Useful GFP flag combinations" in `include/linux/gfp_types.h`.

**Notes:**
- `__GFP_RECLAIM` = `__GFP_DIRECT_RECLAIM | __GFP_KSWAPD_RECLAIM`
- GFP_NOIO can still direct-reclaim clean page cache and slab pages (no physical IO)
- Prefer `memalloc_nofs_save()`/`memalloc_noio_save()` over GFP_NOFS/GFP_NOIO

## __GFP_ACCOUNT

Incorrect memcg accounting lets a container allocate kernel memory without being
charged, bypassing its memory limit. Review any new `__GFP_ACCOUNT` usage or
`SLAB_ACCOUNT` cache creation.

- Slabs created with `SLAB_ACCOUNT` are charged to memcg automatically via
  `memcg_slab_post_alloc_hook()` in `mm/slub.c`, even without explicit
  `__GFP_ACCOUNT` in the allocation call

**Validation:**
1. When using `__GFP_ACCOUNT`, ensure the correct memcg is charged
   - `old = set_active_memcg(memcg); work; set_active_memcg(old)`
2. Most usage does not need `set_active_memcg()`, but:
   - Kthreads switching context between many memcgs may need it
   - Helpers operating on objects (e.g., BPF maps) with stored memcg may need it
3. Ensure new `__GFP_ACCOUNT` usage is consistent with surrounding code

## Mempool Allocation Guarantees

Callers that assume `mempool_alloc()` always succeeds will NULL-deref if they
pass a flag without `__GFP_DIRECT_RECLAIM`. Conversely, NULL checks after a call
with `GFP_KERNEL` are dead code. Match the error handling to the flag.

`mempool_alloc()` cannot fail when `__GFP_DIRECT_RECLAIM` is set -- it retries
forever via the `repeat_alloc` loop after failing both the underlying allocator
and the pool reserve (see `mempool_alloc_noprof()` in `mm/mempool.c`).

**Cannot fail (retry forever):** GFP_KERNEL, GFP_NOIO, GFP_NOFS (all include
`__GFP_DIRECT_RECLAIM` via `__GFP_RECLAIM`)

**Can fail:** GFP_ATOMIC, GFP_NOWAIT (no `__GFP_DIRECT_RECLAIM`)

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

## Page Cache Batch Iteration: find_get_entries vs find_lock_entries

Callers that iterate page cache entries using `find_get_entries()` and handle
multi-order entries (large folio swap entries) must account for the fact that
the returned indices may not be the canonical base index of the entry. Getting
this wrong causes infinite retry loops in truncation paths.

**Key difference:**

| Function | Filters multi-order boundary crossings | `indices[i]` value |
|----------|----------------------------------------|--------------------|
| `find_lock_entries()` | Yes -- skips entries whose base is before `*start` or extends beyond `end` | `xas.xa_index` (may not be canonical base) |
| `find_get_entries()` | No -- returns all entries in range without filtering | `xas.xa_index` (may not be canonical base) |

**Why `indices[i]` may not be the canonical base:**

`find_get_entry()` calls `xas_find()` which calls `xas_load()` which calls
`xas_descend()`. When `xas_descend()` encounters a sibling entry, it follows
it to the canonical slot and updates `xas->xa_offset`, but does **not** update
`xas->xa_index` (see `xas_descend()` in `lib/xarray.c`). So after loading a
multi-order entry, `xas.xa_index` retains the original search position, not
the entry's aligned base.

Example: iterating from index 18 finds a multi-order entry at base 16
(order 3, 8 pages spanning [16, 23]):
- `xas_descend` resolves sibling at offset 18 to canonical offset 16
- `xas.xa_offset = 16` but `xas.xa_index` remains 18
- `indices[i] = 18`, not 16

**To compute the canonical base**, callers must do what `find_lock_entries()`
does:
```c
nr = 1 << xas_get_order(&xas);
base = xas.xa_index & ~(nr - 1);   /* or round_down(xas.xa_index, nr) */
```

## kmemleak Tracking Symmetry

Freeing an object that was never registered with kmemleak causes a warning
"Trying to color unknown object ... as Black" when `CONFIG_DEBUG_KMEMLEAK` is
enabled. The kmemleak subsystem expects symmetric registration and
deregistration.

**When kmemleak registration is skipped:**

SLUB skips `kmemleak_alloc_recursive()` when `gfpflags_allow_spinning(flags)`
returns false (see `slab_post_alloc_hook()` in `mm/slub.c`).
`gfpflags_allow_spinning()` in `include/linux/gfp.h` checks
`!!(gfp_flags & __GFP_RECLAIM)`, i.e., whether at least one of
`__GFP_DIRECT_RECLAIM` or `__GFP_KSWAPD_RECLAIM` is set. Since `GFP_ATOMIC`
includes `__GFP_KSWAPD_RECLAIM`, kmemleak IS called for `GFP_ATOMIC`
allocations. The only standard API that skips kmemleak is `kmalloc_nolock()`,
which passes flags without any reclaim bits.

**Symmetric API requirement:**

| Allocation | Free | Tracking |
|------------|------|----------|
| `kmalloc(GFP_KERNEL)` | `kfree()`, `kfree_rcu()` | Symmetric (both tracked) |
| `kmalloc_nolock()` | `kfree_nolock()` | Symmetric (both skip tracking) |

`kfree_nolock()` deliberately skips `kmemleak_free_recursive()` (see
`kfree_nolock()` in `mm/slub.c`). Conversely, `kfree_rcu()` defers freeing
via `kvfree_call_rcu()` in `mm/slab_common.c`, which calls `kmemleak_ignore()`
to mark the object as not-a-leak before the grace period. If the object was
never registered, `kmemleak_ignore()` triggers the "unknown object" warning
via `paint_ptr()` in `mm/kmemleak.c`.

**REPORT as bugs**: Code that allocates with `kmalloc_nolock()` but frees with
`kfree()` or `kfree_rcu()`, as this creates a kmemleak tracking imbalance.
The reverse (allocating with `kmalloc()` and freeing with `kfree_nolock()`)
also skips kmemleak deregistration, causing false leak reports.

## Quick Checks

Common MM review pitfalls. Missing any of these typically results in data
corruption, use-after-free, or deadlock that only reproduces under memory
pressure or on NUMA systems.

- **TLB flushes after PTE modifications**: Missing a TLB flush after making a
  PTE less permissive lets userspace keep stale write access, causing data
  corruption or security bypass. Required for writable-to-readonly and
  present-to-not-present transitions. Not needed for not-present-to-present or
  more-permissive transitions (callers pair `ptep_set_access_flags()` with
  `update_mmu_cache()`). See `change_pte_range()` in `mm/mprotect.c` and
  `zap_pte_range()` in `mm/memory.c`
- **mmap_lock ordering**: Taking the wrong lock type deadlocks or corrupts the
  VMA tree. Write lock (`mmap_write_lock()`) for VMA structural changes
  (insert/delete/split/merge, modifying vm_flags/vm_page_prot). Read lock
  (`mmap_read_lock()`) for VMA lookup, page fault handling, read-only traversal.
  See the "Lock ordering in mm" comment block at the top of `mm/rmap.c`
- **Page reference before mapping**: Mapping a page without holding a reference
  causes use-after-free when the page is freed while still mapped. `folio_get()`
  must precede `set_pte_at()` and rmap operations.
  `validate_page_before_insert()` in `mm/memory.c` rejects pages with zero
  refcount
- **Compound page tail pages**: Accessing flags directly on a tail page reads
  the `compound_head` pointer instead of flags. Use `compound_head()` to get
  the head page first. See the page flag policies in the Large Folio section
  for which operations are valid on tail pages
- **`get_node(s, numa_mem_id())`** can return NULL on systems with memory-less
  nodes (see `get_node()` and `get_barn()` in `mm/slub.c`). A missing NULL
  check causes a NULL-pointer dereference that only triggers on NUMA systems
  with memory-less nodes
