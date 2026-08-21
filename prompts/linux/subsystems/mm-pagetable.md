# MM Page Table Operations

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

**Non-present PTE type dispatch** (see `check_pte()` in
`mm/page_vma_mapped.c`, `softleaf_type()` in `include/linux/leafops.h`):

Non-present PTEs encode several distinct swap entry types via the
`softleaf_type` / `swp_type` field. Each type has different semantics and must
be handled in the correct branch of any dispatch logic. Accepting an entry type
in the wrong branch causes semantic confusion (e.g., treating a
device-exclusive entry as a migration entry), which may silently produce wrong
behavior even if the types share the PFN-encoding property.

The distinct non-present PTE categories are:
- **Migration** (`SOFTLEAF_MIGRATION_READ`, `_READ_EXCLUSIVE`, `_WRITE`):
  page temporarily unmapped during folio migration; checked via
  `softleaf_is_migration()`
- **Device-private** (`SOFTLEAF_DEVICE_PRIVATE_READ`, `_WRITE`): page migrated
  to un-addressable device memory (HMM); checked via
  `softleaf_is_device_private()`
- **Device-exclusive** (`SOFTLEAF_DEVICE_EXCLUSIVE`): CPU access temporarily
  revoked for device atomic operations, page remains in host memory; checked
  via `softleaf_is_device_exclusive()`
- **HW poison** (`SOFTLEAF_HWPOISON`): page has uncorrectable memory error;
  checked via `softleaf_is_hwpoison()` / `is_hwpoison_entry()`
- **Marker** (`SOFTLEAF_MARKER`): metadata-only entry (e.g., uffd-wp marker,
  poison marker); checked via `softleaf_is_marker()`

When reviewing code that adds a new swap entry type or modifies dispatch logic
over non-present PTEs, verify that each branch accepts only the entry types
whose semantics match that branch's purpose. A common mistake is grouping
device-exclusive with migration (both involve temporarily unmapped pages with
PFNs) even though their refcount behavior, resolution paths, and semantics
are entirely different. `softleaf_has_pfn()` in `include/linux/leafops.h`
shows which types encode a PFN -- sharing this property does not make types
interchangeable in dispatch logic.

**Flag transfer on non-present-to-present PTE reconstruction:**

Every code path that converts a non-present PTE (swap, migration, or
device-exclusive entry) to a present PTE must carry over the soft-dirty
and uffd-wp bits. These bits have different encodings in swap PTEs vs
present PTEs, so they require explicit read-then-write transfer:
```c
if (pte_swp_soft_dirty(old_pte))
    newpte = pte_mksoft_dirty(newpte);
if (pte_swp_uffd_wp(old_pte))
    newpte = pte_mkuffd_wp(newpte);
```
This pattern is required in `do_swap_page()`, `restore_exclusive_pte()` in
`mm/memory.c`, `remove_migration_pte()`, `try_to_map_unused_to_zeropage()`
in `mm/migrate.c`, and `unuse_pte()` in `mm/swapfile.c`. When converting
between non-present entries (swap-to-swap), use the swap-side writers
instead: `pte_swp_mksoft_dirty()` and `pte_swp_mkuffd_wp()` (see
`copy_nonpresent_pte()` in `mm/memory.c`)

**Soft dirty vs hardware dirty in PTE move/remap:**

Soft dirty (`pte_mksoft_dirty()` / `pte_swp_mksoft_dirty()`) is a
userspace-visible tracking bit for `/proc/pid/pagemap` and CRIU, distinct
from hardware dirty (`pte_mkdirty()`). PTE move operations (mremap,
userfaultfd UFFDIO_MOVE) must set soft dirty on the destination to signal
the mapping changed, while preserving the source PTE's hardware dirty state.
Common mistakes:
- Using `pte_mkdirty()` when the intent is to mark the PTE as "touched" for
  userspace tracking -- this should be `pte_mksoft_dirty()`
- Handling present PTEs but forgetting `pte_swp_mksoft_dirty()` for swap PTEs
- Using `#ifdef CONFIG_MEM_SOFT_DIRTY` instead of
  `pgtable_supports_soft_dirty()`, which also handles runtime detection (e.g.,
  RISC-V ISA extensions)

See `move_soft_dirty_pte()` in `mm/mremap.c` for the reference implementation
handling both present and swap cases.

## Special vs Normal Page Table Mappings

Marking a normal refcounted folio's page table entry as "special" causes
`vm_normal_page()` (and `vm_normal_page_pmd()` / `vm_normal_page_pud()`)
to return NULL, hiding the folio from page table walkers, GUP, and refcount
management. GUP-fast checks `pte_special()` / `pmd_special()` /
`pud_special()` early and bails out, falling back to slow GUP.

**Invariant** (see `__vm_normal_page()` in `mm/memory.c`):
- Normal refcounted folios must NOT have their page table entry marked
  special (`pte_mkspecial()` / `pmd_mkspecial()` / `pud_mkspecial()`)
- Only raw PFN mappings (VM_PFNMAP, VM_MIXEDMAP without struct page),
  devmap entries (`pte_mkdevmap()` / `pmd_mkdevmap()` / `pud_mkdevmap()`),
  and the shared zero folios may be marked special
- Use `folio_mk_pmd()` / `folio_mk_pud()` when constructing entries for
  normal refcounted folios; these helpers produce a plain huge entry
  without setting the special bit (see `include/linux/mm.h`)
- Use `pfn_pmd()` + `pmd_mkspecial()` or `pfn_pud()` + `pud_mkspecial()`
  only for raw PFN mappings

**Common mistake**: When a `vmf_insert_folio_*()` function reuses a
PFN-oriented helper (e.g., `vmf_insert_pfn_pud()`), the helper's
unconditional `pXd_mkspecial()` call applies to the folio mapping too.
The fix is to split the entry-construction logic so the folio path uses
`folio_mk_pXd()` without the special bit, while the PFN path retains
`pXd_mkspecial()` (see `insert_pmd()` and `insert_pud()` in
`mm/huge_memory.c` for the correct pattern using `struct folio_or_pfn`).

## Page Table Entry to Folio/Page Conversion Preconditions

Applying a present-entry conversion function to a non-present page table
entry (migration entry, swap entry, or poisoned entry) interprets swap
metadata bits as a physical page frame number, producing a bogus
`struct page *` that causes an invalid address dereference.

**Functions that require a present entry:**
- `pmd_folio(pmd)` / `pmd_page(pmd)` -- expand to `pfn_to_page(pmd_pfn(pmd))`,
  which is only valid when `pmd_present(pmd)` or `pmd_trans_huge(pmd)` or
  `pmd_devmap(pmd)` (see `include/linux/pgtable.h`)
- `pte_page(pte)` / `vm_normal_page()` -- only valid when `pte_present(pte)`

**Correct conversion for non-present entries:**
- Migration entries: `softleaf_from_pmd()` or `softleaf_from_pte()` to extract
  the `softleaf_t`, then `softleaf_to_folio()` to get the folio (see
  `include/linux/leafops.h`)
- Alternatively, if the folio comparison is not needed for a non-present
  entry (e.g., because a migration entry is locked and cannot refer to the
  target folio), skip the conversion entirely

**Review pattern:** When code handles multiple PMD/PTE states in a combined
conditional (e.g., `if (pmd_trans_huge(*pmd) || pmd_is_migration_entry(*pmd))`),
verify that subsequent operations like `pmd_folio()` or `pmd_page()` are
guarded to execute only on the present-entry cases. A common mistake is
adding a non-present entry type to an existing condition without adjusting
the folio extraction that follows.

## PTE Batching

Batching consecutive PTEs that map the same large folio into a single
`set_ptes()` call propagates the first PTE's permission bits to all entries
in the batch, because `set_ptes()` only advances the PFN and preserves all
other bits. If the batch includes PTEs with different permissions (e.g.,
writable vs read-only), the result silently overwrites the intended
permissions, causing security bypasses.

`folio_pte_batch()` in `mm/util.c` is a simplified wrapper that calls
`folio_pte_batch_flags()` in `mm/internal.h` with `flags=0`. With no flags,
differences in writable, dirty, and soft-dirty bits are ignored and PTEs
with different permissions are batched together.

**FPB flags** (defined as `fpb_t` in `mm/internal.h`) control which PTE bits
are compared vs ignored during batching:

| Flag | Effect |
|------|--------|
| `FPB_RESPECT_WRITE` | Include the writable bit in comparison; PTEs with different write permissions will not batch |
| `FPB_RESPECT_DIRTY` | Include the dirty bit in comparison |
| `FPB_RESPECT_SOFT_DIRTY` | Include the soft-dirty bit in comparison |
| `FPB_MERGE_WRITE` | After batching, if any PTE was writable, set the writable bit on the output PTE |
| `FPB_MERGE_YOUNG_DIRTY` | After batching, merge young and dirty bits from all PTEs into the output |

- `folio_pte_batch()` (no flags): safe only when the caller does not stamp the
  first PTE's permission bits onto other entries (e.g., `zap_present_ptes()`
  which clears all PTEs, or `folio_unmap_pte_batch()` which unmaps)
- `folio_pte_batch_flags()` with `FPB_RESPECT_WRITE`: required when the caller
  uses `set_ptes()` to write the batched PTE value back (see `move_ptes()` in
  `mm/mremap.c`, `change_pte_range()` in `mm/mprotect.c`)

**REPORT as bugs**: Code that uses `folio_pte_batch()` (without flags) to
determine a batch count and then passes that count to `set_ptes()`, because
the first PTE's writable/dirty/soft-dirty bits will be stamped onto all
entries in the batch.

**Batched PTE operation boundaries:**

Passing an uncapped `max_nr` to `folio_pte_batch()` causes out-of-bounds reads
past the end of a page table. The `max_nr` parameter must be capped so that
scanning stays within a single page table and a single VMA. The standard
expression is `(pmd_addr_end(addr, vma->vm_end) - addr) >> PAGE_SHIFT`. Code
that reaches PTE-level iteration through the standard walker hierarchy
(`zap_pmd_range()` -> `zap_pte_range()`) receives a pre-capped `end`. Code
that operates directly at PTE level via `page_vma_mapped_walk()` must perform
its own PMD boundary capping.

**REPORT as bugs**: Any caller of `folio_pte_batch()` that derives `max_nr`
from `folio_nr_pages()` without capping at the PMD boundary.

## page_vma_mapped_walk() Non-Present Entries

Calling PTE accessor functions (`pte_young()`, `pte_dirty()`, `pte_write()`,
`ptep_clear_flush_young()`, etc.) on a non-present entry returned by
`page_vma_mapped_walk()` produces undefined results because swap entries
encode bits differently than present PTEs. This class of bug went undetected
because the non-present entries only appear with device-exclusive or
device-private ZONE_DEVICE pages.

`page_vma_mapped_walk()` in `mm/page_vma_mapped.c` can return `true` with
`pvmw.pte` pointing to non-present entries. The `check_pte()` helper accepts
three PTE types when `PVMW_MIGRATION` is not set:

| Entry type | `pte_present()` | How to identify |
|------------|-----------------|-----------------|
| Normal present PTE | true | `pte_present(ptep_get(pvmw.pte))` |
| Device-exclusive swap entry | false | `softleaf_is_device_exclusive(...)` |
| Device-private swap entry | false | `softleaf_is_device_private(...)` |

**Rules for rmap walk callbacks** (functions passed to `rmap_walk()` or using
`page_vma_mapped_walk()`):
- When `pvmw.pte` is set, always check `pte_present(ptep_get(pvmw.pte))`
  before calling present-PTE accessors (`pte_pfn()`, `pte_dirty()`,
  `pte_write()`, `pte_young()`, `pte_soft_dirty()`, `pte_uffd_wp()`)
- Non-present PFN swap PTEs (device-exclusive and device-private entries)
  require converting to `softleaf_t` first via `softleaf_from_pte()`, then
  using `softleaf_to_pfn()` for PFN and `softleaf_is_device_private_write()`
  for writability. Swap-PTE flag accessors (`pte_swp_soft_dirty()`,
  `pte_swp_uffd_wp()`) still accept `pte_t` but read different bit positions
  than their present-PTE counterparts (`pte_soft_dirty()`, `pte_uffd_wp()`),
  so using the wrong family silently reads wrong bits. See
  `try_to_migrate_one()` and `try_to_unmap_one()` in `mm/rmap.c` for the
  correct dispatching pattern
- Non-present PFN swap PTEs represent pages that are "old" and "clean" from
  the CPU's perspective; MMU notifiers handle device-side access tracking

## Large Folio PTE Installation

When `pte_range_none()` returns false during large folio installation (some
PTEs already populated), the handler must ensure forward progress:

- **Page cache folios** (`finish_fault()`): fall back to single-PTE install
- **Freshly allocated anon folios** (`do_anonymous_page()`): release folio,
  retry at smaller size via `alloc_anon_folio()`
- **PMD-level** (`do_set_pmd()`): return `VM_FAULT_FALLBACK`

**REPORT as bugs**: returning `VM_FAULT_NOPAGE` (retry) when
`pte_range_none()` fails for a page cache folio without falling back to
single-PTE -- this creates a livelock (hung process, no warning).

## Page Table Walker Callbacks

`pmd_entry` in `struct mm_walk_ops` (see `include/linux/pagewalk.h`) receives
every non-empty PMD including `pmd_trans_huge()`. Failing to handle THP PMDs
causes silent data skipping or crashes from treating a huge-page PFN as a
page table pointer. When `pmd_entry` is defined without `pte_entry`, the
walker does NOT descend to PTEs -- the callback must walk PTEs internally.

Return values: `0` = continue, `> 0` = stop (returned to caller), `< 0` =
error. `walk_lock` specifies locking: `PGWALK_RDLOCK` (mmap_lock read),
`PGWALK_WRLOCK` (walker write-locks VMAs), `PGWALK_WRLOCK_VERIFY` /
`PGWALK_VMA_RDLOCK_VERIFY` (assert already locked).

In `pmd_entry` callbacks: read PMD locklessly with `pmdp_get_lockless()`,
reread under `pmd_lock()` for THP; check `pte_offset_map_lock()` return for
NULL; call `folio_get()` before releasing PTL if returning a folio reference.

## Quick Checks

- **TLB flushes after PTE modifications**: Missing a TLB flush after making a
  PTE less permissive lets userspace keep stale write access, causing data
  corruption or security bypass. Required for writable-to-readonly and
  present-to-not-present transitions. Not needed for not-present-to-present or
  more-permissive transitions (callers pair `ptep_set_access_flags()` with
  `update_mmu_cache()`). See `change_pte_range()` in `mm/mprotect.c` and
  `zap_pte_range()` in `mm/memory.c`
- **VM_WRITE gate for writable PTEs**: writable PTEs require `VM_WRITE` in
  `vma->vm_flags`. Use `maybe_mkwrite()` (`include/linux/mm.h`). Verify in
  fork/COW, userfaultfd install, and any PTE construction path — VMA
  permissions can change via `mprotect()` between mapping and installation
- **VMA flag and PTE/PMD flag consistency**: clearing a `vm_flags` bit
  (e.g., `VM_UFFD_WP`, `VM_SOFT_DIRTY`) requires clearing the corresponding
  PTE/PMD bits across all forms (present, swap, PTE markers). Error-prone
  when VMA flag clearing and page table walk are in different code paths.
  See `clear_uffd_wp_pmd()` in `mm/huge_memory.c`
- **`flush_tlb_batched_pending()` after PTL re-acquisition**: after dropping
  and re-acquiring PTL, call `flush_tlb_batched_pending(mm)` — reclaim on
  another CPU may have batched TLB flushes while the lock was released.
  See `flush_tlb_batched_pending()` in `mm/rmap.c`
- **Page table removal vs GUP-fast**: clearing a PUD/PMD to free a page
  table page requires `tlb_remove_table_sync_one()` or `tlb_remove_table()`
  before reuse. GUP-fast walks locklessly under `local_irq_save()` and
  can follow stale entries into freed page tables without synchronization.
  See `mm/mmu_gather.c`
- **`vma_start_write()` before page table access under `mmap_write_lock`**:
  `mmap_write_lock` does NOT exclude per-VMA lock readers (e.g., madvise
  under `lock_vma_under_rcu()`). Call `vma_start_write(vma)` before
  checking/modifying page tables to drain per-VMA lock holders. See
  `collapse_huge_page()` in `mm/khugepaged.c`.
  **Critical: PTE-level zap operations cross granularity boundaries.**
  VMA-locked `MADV_DONTNEED` calls `zap_page_range_single_batched()` →
  `unmap_page_range()` → `zap_pmd_range()` → `zap_pte_range()` →
  `try_to_free_pte()` (in `mm/pt_reclaim.c`) → `pmd_clear()`. When all
  PTEs in a page table are zapped, PT_RECLAIM frees the PTE page **and
  clears the PMD entry**. Code that read the PMD before
  `vma_start_write()` now holds a stale pointer to freed memory — this is
  use-after-free (kernel panic), not just stale data. Do NOT dismiss
  PMD-level accesses before `vma_start_write()` as "different granularity"
  from PTE-level zap operations — the zap path modifies PMDs too
- **Page fault path lock constraints**: `->fault`/`->page_mkwrite` run
  under `mmap_lock`, nested below `i_rwsem` and `sb_start_write`. Fault
  handlers must not wait on freeze protection (ABBA deadlock). Copy user
  data with `copy_folio_from_iter_atomic()` and retry outside the lock.
  See `generic_perform_write()` in `mm/filemap.c`
- **`pte_unmap_unlock` pointer must be within the kmap'd PTE page**: After a
  PTE iteration loop, the iterated pointer may point one-past-the-end of the
  PTE page. On `CONFIG_HIGHPTE` systems, `pte_unmap()` calls
  `kunmap_local()`, which derives the page address via `PAGE_MASK`. If the
  pointer crosses a page boundary, it unmaps the wrong page. Save the start
  pointer from `pte_offset_map_lock()` or pass `ptep - 1` after the loop.
  Only triggers on 32-bit HIGHMEM architectures
- **`pte_unmap()` LIFO ordering**: multiple PTE mappings must be unmapped
  in reverse order. Invisible on 64-bit; triggers WARNING on 32-bit HIGHPTE
  where `pte_unmap()` calls `kunmap_local()`
- **`pmd_present()` after `pmd_trans_huge_lock()`**: succeeds for both
  present THP PMDs and non-present PMD leaf entries (migration, device-private).
  Must check `pmd_present()` before `pmd_folio()`/`pmd_page()` or any
  function assuming a present PMD
- **Page table state after lock drop and retry**: after dropping and
  reacquiring PTL, concurrent threads may have repopulated empty entries.
  Decisions to free page table structures must be re-validated. See
  `zap_pte_range()` `direct_reclaim` flag in `mm/memory.c` and
  `try_to_free_pte()` in `mm/pt_reclaim.c`
- **Kernel page table population synchronization**: `pgd_populate()` /
  `p4d_populate()` do NOT sync to other processes' kernel page tables. Use
  `pgd_populate_kernel()` / `p4d_populate_kernel()` which call
  `arch_sync_kernel_mappings()`. Affects vmemmap, percpu, KASAN shadow
- **Lazy MMU mode pairing and hazards**: (1) PTE reads after writes inside
  lazy mode may return stale data — bracket with leave/enter. (2) Error
  paths must not skip `arch_leave_lazy_mmu_mode()` — use `break` not
  `return`. No-op on most configs; bugs only manifest on Xen PV, sparc,
  powerpc book3s64, arm64
- **Lazy MMU mode implies possible atomic context**: disables preemption
  on some architectures (sparc, powerpc). `pte_fn_t` callbacks and PTE
  loops inside lazy MMU mode must not sleep. Allocations need
  `GFP_ATOMIC`/`GFP_NOWAIT` or pre-allocation. Invisible on x86/arm64
- **Non-present PTE swap entry type dispatch**: see the full section in
  PTE State Consistency above. Verify each dispatch branch accepts only
  semantically matching entry types — do not group device-exclusive with
  migration despite both having PFNs
- **`arch_sync_kernel_mappings()` on error paths**: loops that accumulate
  `pgtbl_mod_mask` and call `arch_sync_kernel_mappings()` after must use
  `break` (not `return`) on errors. Early `return` skips the sync, leaving
  other processes' kernel page tables stale. See `__apply_to_page_range()`
  and `vmap_range_noflush()`
- **Page table walker iterator advancement**: in `do { } while` page table
  loops, advance the pointer unconditionally in the `while` clause (e.g.,
  `} while (pte++, addr += PAGE_SIZE, addr != end)`), not inside a
  conditional body. Use `continue` to skip entries so `while` still advances.
  Placing `ptr++` inside an `if` stalls the walker when false
- **Mapcount symmetry for non-present PTE entries**: non-present swap PTEs
  holding a folio reference (device-private, device-exclusive, migration) must
  keep mapcount symmetric: if mapcount is maintained during creation, teardown
  (`zap_nonpresent_ptes()`) must remove it. Device-private/exclusive maintain
  mapcount; migration entries are managed by `try_to_migrate_one()` itself
- **PTE batch loop bounds**: loops batching consecutive PTEs must not rely
  solely on `pte_same()` against a synthetic expected PTE. On XEN PV,
  `pte_advance_pfn()` can produce `pte_none()` for PFNs without valid
  machine frames, causing false matches and overrunning the folio. Bound
  iteration independently using folio metadata (`folio_nr_pages()` etc.)
- **`pte_offset_map`/`pte_unmap` pairing**: `pte_unmap()` must receive the
  exact pointer from `pte_offset_map()`, never a pointer to a local `pte_t`
  copy. Both are `pte_t *` so the compiler won't warn. On `CONFIG_HIGHPTE`,
  passing a stack address to `pte_unmap()` unmaps the wrong mapping. Common
  mistake: `pte_t orig = ptep_get(pte)` then `pte_unmap(&orig)`
- **Memory hotplug lock for unprotected kernel page table walks**:
  walking `init_mm` kernel page tables over arbitrary or hotplug-removable
  direct-map/vmemmap ranges generally requires memory-hotplug exclusion via
  `get_online_mems()` / `put_online_mems()` in addition to `mmap_lock`.
  Memory hot-remove can free intermediate page-table pages for direct-map and
  vmemmap mappings, so a walker that is not otherwise protected can race with
  hot-remove and use freed page-table pages. Acquire the hotplug read lock
  before `mmap_lock` to preserve lock ordering. Do not apply this
  mechanically: first check whether the walked range is otherwise guaranteed
  to remain present, for example because it is tied to an object whose
  lifetime prevents the associated memory from being offlined/removed, or
  because the caller already holds the memory hotplug lock. In such cases,
  adding `get_online_mems()` inside the walker can be unnecessary or even
  wrong, especially if the function is callable from memory-hotplug paths
  already holding the write side of the lock
- **Bit-based locking barrier pairing**: when a bit flag is used for mutual
  exclusion (trylock pattern), the unlock must use `clear_bit_unlock()` (release
  semantics), not `clear_bit()` (relaxed, no barrier). The lock side must use
  `test_and_set_bit_lock()` (acquire semantics). Plain `clear_bit()` allows
  stores to be reordered past the unlock on weakly-ordered architectures
  (arm64). MM uses this for `PGDAT_RECLAIM_LOCKED` in `mm/vmscan.c`
- **`walk_page_range()` default skips `VM_PFNMAP` VMAs**: without
  `.test_walk`, default `walk_page_test()` skips `VM_PFNMAP` silently.
  Callers needing all VMAs must provide `.test_walk` returning 0. A 0
  return from `walk_page_range()` may mean "skipped", not "handled"
- **`ACTION_AGAIN` in page walk callbacks**: `ACTION_AGAIN` retries with
  no limit. `pte_offset_map_lock()` returns NULL non-transiently for
  migration entries — setting `ACTION_AGAIN` on this failure creates an
  infinite loop. Return 0 to skip gracefully. `walk_pte_range()` already
  handles retry internally; callbacks should not duplicate it
- **Page comparison for zeropage remapping must use `pages_identical()`**:
  raw `memchr_inv()`/`memcmp()` miss architecture metadata. On arm64 MTE,
  byte-identical pages with different tags cause mismatch faults after
  remapping to `ZERO_PAGE(0)`. `pages_identical()` has an arm64 override
  rejecting MTE-tagged pages. **REPORT as bugs**: `memchr_inv()`/`memcmp()`
  for zeropage/merge decisions
