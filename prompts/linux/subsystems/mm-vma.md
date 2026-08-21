# MM VMA Operations

## SLAB_TYPESAFE_BY_RCU and VMA Recycling

Dereferencing a parent/owner pointer from a `SLAB_TYPESAFE_BY_RCU` object
after dropping the object's refcount causes use-after-free when the object has
been recycled to a different owner. The owner can exit and free its backing
structure in the window between the refcount drop and the dereference.

The VMA cache is created with `SLAB_TYPESAFE_BY_RCU` (see `vma_state_init()`
in `mm/vma_init.c`), which means a VMA's slab memory remains valid through an
RCU read-side critical section even after `vm_area_free()`, but the VMA can be
reallocated to a completely different `mm_struct` during that window.

**Per-VMA lock lookup protocol** (see `lock_vma_under_rcu()` and
`vma_start_read()` in `mm/mmap_lock.c`):
1. `mas_walk()` under `rcu_read_lock()` finds a VMA in the maple tree
2. `vma_start_read()` increments `vma->vm_refcnt`
3. If `vma->vm_mm != mm` (VMA was recycled), the refcount must be dropped --
   but `vma_refcount_put()` dereferences `vma->vm_mm` for `rcuwait_wake_up()`
4. The foreign `mm` must be stabilized with `mmgrab()` before calling
   `vma_refcount_put()`, then released with `mmdrop()` afterward

**REPORT as bugs**: Code paths in `lock_vma_under_rcu()`, `lock_next_vma()`,
or `vma_start_read()` that call `vma_refcount_put()` on a VMA whose `vm_mm`
does not match the caller's `mm` without first stabilizing the foreign `mm`
via `mmgrab()`.

## VMA Anonymous vs File-backed Classification

Using `vma->vm_file` to determine whether a VMA is file-backed causes
incorrect dispatch for VMAs that have a `vm_file` but are treated as
anonymous (e.g., private mappings of `/dev/zero`). This leads to BUG_ON
crashes, unaligned page offsets, or wrong code paths being taken.

**How VMA classification works** (see `include/linux/mm.h`):
- `vma_is_anonymous(vma)` returns `!vma->vm_ops` -- this is the canonical
  test for anonymous VMAs
- `vma_set_anonymous(vma)` sets `vma->vm_ops = NULL` but does NOT clear
  `vma->vm_file`
- A VMA can have `vma->vm_file != NULL` AND be anonymous (`vm_ops == NULL`)

**VMAs where `vm_file` is set but the VMA is anonymous:**
- Private mappings of `/dev/zero`: `mmap_zero_private_success()` in
  `drivers/char/mem.c` calls `vma_set_anonymous(vma)` for private mappings,
  leaving `vm_file` pointing to the `/dev/zero` file. Shared mappings take
  a different path via `shmem_zero_setup()` which sets
  `vm_ops = &shmem_anon_vm_ops`
- Any driver `mmap` handler that calls `vma_set_anonymous()` after the VMA
  is created with a file reference

**Correct usage:**
- To test "is this VMA file-backed?": use `!vma_is_anonymous(vma)`, NOT
  `vma->vm_file != NULL`
- To test "is this VMA anonymous?": use `vma_is_anonymous(vma)`, NOT
  `vma->vm_file == NULL`
- To access the backing file of a file-backed VMA: check
  `!vma_is_anonymous(vma)` first, then use `vma->vm_file`

**REPORT as bugs**: Code that uses `vma->vm_file` (or `!vma->vm_file`) as
a proxy for file-backed (or anonymous) VMA classification in dispatch logic,
conditionals, or assertions. The correct test is `vma_is_anonymous()`.

## VMA Split/Merge Critical Section

Page table structural changes performed outside the `vma_prepare()`/
`vma_complete()` critical section race with concurrent page faults (via VMA
lock) and rmap walks (via file/anon rmap locks). The result is use-after-free,
page table corruption, or re-establishment of state that was just torn down.

The VMA-modifying paths -- `__split_vma()`, `commit_merge()`, and
`vma_shrink()` in `mm/vma.c` -- share a critical section:

1. `vma_start_write()` (acquire per-VMA lock, before or at entry)
2. `vma_prepare()` (acquire file rmap `i_mmap_lock_write` and anon_vma lock)
3. Page table structural changes: `vma_adjust_trans_huge()`, `hugetlb_split()`
4. VMA range update (`vm_start`/`vm_end`/`vm_pgoff`)
5. `vma_complete()` (release locks acquired in step 2)

`__split_vma()` additionally calls `vm_ops->may_split()` before this sequence.

**Rules:**
- `vm_ops->may_split()` must only validate whether the split is permitted
  (e.g., alignment checks). It must not modify page tables or other shared
  state, because it runs before the VMA and rmap locks are acquired
- Any page table unsharing, splitting, or teardown required by a VMA split
  must happen between `vma_prepare()` and `vma_complete()`, where the VMA
  write lock and file/anon rmap write locks prevent concurrent page table
  walks (except hardware walks and `gup_fast()`)
- When calling helpers that normally acquire their own locks (e.g.,
  `hugetlb_unshare_pmds()`), use a `take_locks=false` path and assert
  that the needed locks are already held (see `hugetlb_split()` in
  `mm/hugetlb.c`)

## Per-VMA Lock Exclusion via vma_start_write()

`mmap_write_lock()` alone does NOT exclude per-VMA lock holders — per-VMA
read locks acquired **before** `mmap_write_lock()` remain held, because the
seqcount in `vma_start_read()` only prevents **new** acquisitions, not
revocation of existing ones. Only `vma_start_write(vma)` drains existing
per-VMA read lock holders. VMA-locked operations (MADV_DONTNEED,
page faults) modify page tables at all levels including PMDs via
PT_RECLAIM (`pmd_clear()` in `try_to_free_pte()`), so any
`mmap_write_lock` holder accessing page tables before `vma_start_write()`
races with per-VMA locked paths.

**`check_pmd_still_valid()` / `find_pmd_or_thp_or_none()`**: These
functions walk page tables (`mm_find_pmd` → PGD→P4D→PUD→PMD) and then
read the PMD value via `pmdp_get_lockless(pmd)` in `check_pmd_state()`.
A concurrent per-VMA locked `MADV_DONTNEED` can call `try_to_free_pte()`
→ `pmd_clear()` + `free_pte()` between the PMD read and subsequent use
of the result — the check succeeds, the caller proceeds assuming a valid
PMD, but the PMD has been cleared and the PTE page freed underneath it.
Code that calls these functions before `vma_start_write()` and then acts
on the result (e.g., proceeding to `pmd_lock` + `pmdp_collapse_flush` on
the assumption the PMD is still populated) is a bug — even though the
PMD *pointer* remains valid (it's in the PUD page which isn't freed),
the *value* and the PTE page it pointed to are gone.

**REPORT as bugs**: Functions holding `mmap_write_lock` that access page
tables or PTE pages before calling `vma_start_write(vma)`. When a patch
adds new per-VMA lock users (e.g., converting a path from `mmap_read_lock`
to per-VMA lock), trace callers holding `mmap_write_lock` that access
page tables for the same VMA and verify each calls `vma_start_write()`
before the access.

## VMA Flags Modification API

Key distinction: `vm_flags_set()` ORs (adds bits, never clears),
`vm_flags_reset()` replaces (sets to exact value), `vm_flags_init()` replaces
without locking (VMA not yet in tree). `vm_flags_clear()` removes specific
bits. `vm_flags_mod()` adds and removes in one operation. See
`include/linux/mm.h`.

**Common mistake:** `vm_flags_set(vma, new_flags)` to replace flags -- because
it ORs, stale flags silently survive. Use `vm_flags_reset()` for exact
replacement. Stale `VM_WRITE`/`VM_MAYWRITE` creates security holes.

## File Reference Ownership During mmap Callbacks

mmap uses split ownership: `ksys_mmap_pgoff()` holds one file reference
(fput at end), VMA gets its own via `get_file()` in `__mmap_new_file_vma()`.
When a callback replaces the file (`f_op->mmap_prepare()` replacing
`desc->vm_file`, or legacy `f_op->mmap()` replacing `vma->vm_file`), the
replacement already carries its own reference.

**REPORT as bugs**: unconditional `get_file()` on a file that may have been
swapped by a callback -- the replacement gets a leaked extra reference. See
`map->file_doesnt_need_get` in `call_mmap_prepare()` in `mm/vma.c` and
`shmem_zero_setup()` in `mm/shmem.c`.

## Quick Checks

- **mmap_lock ordering**: Taking the wrong lock type deadlocks or corrupts the
  VMA tree. Write lock (`mmap_write_lock()`) for VMA structural changes
  (insert/delete/split/merge, modifying vm_flags/vm_page_prot). Read lock
  (`mmap_read_lock()`) for VMA lookup, page fault handling, read-only traversal.
  See the "Lock ordering in mm" comment block at the top of `mm/rmap.c`
- **Failable mmap lock reacquisition**: `mmap_write_lock_killable()` /
  `mmap_read_lock_killable()` return `-EINTR` on kill. Ignoring the return
  means continuing without the lock. Check in retry loops and lock upgrade
  sequences. See `__get_user_pages_locked()` in `mm/gup.c`
- **VMA merge anon_vma propagation**: merging an unfaulted VMA with a
  faulted one requires `dup_anon_vma()` (see `vma_expand()` in `mm/vma.c`).
  Merge-time `anon_vma` property checks (e.g., `list_is_singular()` in
  `is_mergeable_anon_vma()`) must apply to the VMA that **has** the
  `anon_vma`, not unconditionally to the destination -- the three cases
  (dst unfaulted/src faulted, dst faulted/src unfaulted, both faulted) are
  asymmetric. See `vma_is_fork_child()` in `mm/vma.c`
- **VMA interval tree uses pgoff, not PFN**: `mapping->i_mmap` is keyed by
  `vm_pgoff`; `vma_address()` expects `pgoff_t`. Passing a raw PFN searches
  the wrong coordinate space. **REPORT as bugs**: raw PFN to
  `vma_interval_tree_foreach()` or `vma_address()`
- **VMA merge/modify error handling**: `vma_modify()`/`vma_merge_new_range()` may
  return error or a different VMA. Original VMA may be freed on success.
  On failure, `vmg->start/end/pgoff` may be mutated and not restored —
  save originals or check `vmg_nomem()`. See `madvise_walk_vmas()` in
  `mm/madvise.c`
- **VMA flag ordering vs merging**: flags not in `VM_IGNORE_MERGE` must be
  set in proposed `vm_flags` *before* `vma_merge_new_range()`. Setting
  flags post-merge via `vm_flags_set()` silently breaks future merges
  (`is_mergeable_vma()` XORs flags). See `ksm_vma_flags()` in `mm/ksm.c`
- **VMA merge side effects vs page table operations**: `vma_complete()`
  triggers `uprobe_mmap()` which installs PTEs. Callers that subsequently
  move/overwrite page tables must set `skip_vma_uprobe` in
  `struct vma_merge_struct` (see `mm/vma.h`), or orphaned PTEs leak memory
- **Fork-time VMA flag divergence**: `dup_mmap()` clears `__VM_UFFD_FLAGS`
  and `VM_LOCKED_MASK` on the child VMA. Fork-time flag checks (e.g.,
  `vma_needs_copy()` checking `VM_UFFD_WP`) must use the destination VMA,
  not the source. Combined mask checks must verify all flags have the same
  source-vs-destination semantics
- **VM_ACCOUNT preservation during VMA manipulation**: clearing `VM_ACCOUNT`
  on a surviving VMA (e.g., `MREMAP_DONTUNMAP`, partial unmap) leaks
  committed memory permanently — `do_vmi_munmap()` only uncharges VMAs
  with `VM_ACCOUNT`. Review `vm_flags_clear()` calls including `VM_ACCOUNT`
- **VMA iteration on external mm_struct**: call
  `check_stable_address_space(mm)` after mmap lock, before traversal.
  On `dup_mmap()` failure, maple tree slots contain `XA_ZERO_ENTRY`
  markers and the mm is flagged `MMF_UNSTABLE`. OOM reaper also sets
  `MMF_UNSTABLE`. See `unuse_mm()` in `mm/swapfile.c`
- **VMA operation results assigned to struct members**: `vma_merge_extend()`,
  `vma_merge_new_range()`, `copy_vma()` return NULL on failure. Assigning
  directly to a struct member (e.g., `vrm->vma = vma_merge_extend(...)`)
  clobbers the original VMA pointer before the NULL check. Assign to a local
  first, NULL-check, then update the struct member on success
- **VMA merge functions invalidate input on success**: `vma_merge_new_range()`,
  `vma_merge_existing_range()`, `vma_modify()` may free the original VMA on
  success. Callers must use the returned VMA, not the original. Discarding the
  return value and using the original is use-after-free
- **`vma_modify*()` error returns in VMA iteration loops**: `vma_modify_flags()`
  etc. return `ERR_PTR(-ENOMEM)` on merge/split failure. Assigning back to a
  VMA loop variable without `IS_ERR()` check dereferences the error pointer.
  Even when the merge is best-effort (VMA unchanged on failure), the error
  return corrupts iteration. Check `IS_ERR()` or use `give_up_on_oom`
- **VMA lock refcount balance on error paths**: `__vma_enter_locked()` adds
  `VMA_LOCK_OFFSET` to `vm_refcnt` then waits for readers. When using
  `TASK_KILLABLE`/`TASK_INTERRUPTIBLE`, the `-EINTR` path must subtract the
  offset back. Leaked offset permanently blocks VMA detach/free
- **VMA addresses used as boolean flags**: `vm_start` can legitimately be
  zero, so `if (addr_var)` to mean "was this set" silently fails for
  zero-address VMAs. Use an explicit `bool` flag or direct comparisons.
  Same for any `unsigned long` address/offset that can be zero
- **Maple state RCU lifetime**: `ma_state` caches RCU-protected node
  pointers. After `rcu_read_unlock()`, invalidate with `mas_set()` or
  `mas_reset()` before reuse. Easy to miss when `vma_start_read()` drops
  RCU internally on failure. See `lock_vma_under_rcu()` in `mm/mmap_lock.c`
- **`mm_struct` flexible array sizing**: trailing flexible array packs
  cpumask and mm_cid regions. Static definitions (`init_mm`, `efi_mm`) must
  use `MM_STRUCT_FLEXIBLE_ARRAY_INIT`. Adding a new region requires updating
  `mm_cache_init()` (dynamic), `MM_STRUCT_FLEXIBLE_ARRAY_INIT` (static), and
  all static `mm_struct` definitions
- **Memfd file creation API layering**: calling `shmem_file_setup()` or
  `hugetlb_file_setup()` directly for memfd produces files missing
  `O_LARGEFILE`, fmode flags, and security init. Use `memfd_alloc_file()`.
  **REPORT as bugs**: memfd creation via direct `shmem_file_setup()` /
  `hugetlb_file_setup()` (non-memfd callers like DRM/SGX/SysV are fine)
- **VMA lock vs mmap_lock assertions**: `mmap_assert_locked(mm)` fires when
  only a VMA lock is held. Paths reachable under per-VMA locks must use
  `vma_assert_locked(vma)` (accepts either VMA lock or mmap_lock). Legacy
  `mmap_assert_locked()` in page table walk/zap paths is likely incorrect
- **VM Committed Memory Accounting**: `security_vm_enough_memory_mm()` is not
  just a check -- on success it increments `vm_committed_as` via
  `vm_acct_memory()` in `mm/util.c`. Every error path after a successful call
  must invoke `vm_unacct_memory()`. A leaked charge permanently inflates
  `vm_committed_as`, causing `-ENOMEM` under strict overcommit
  (`vm.overcommit_memory=2`)
