# Memory Allocation API Guidelines

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
- `__GFP_KSWAPD_RECLAIM` (present in `GFP_NOWAIT` and `GFP_ATOMIC`) triggers
  `wakeup_kswapd()` in `mm/vmscan.c`, which calls `wake_up_interruptible()`
  and enters the scheduler via `try_to_wake_up()`. This means even non-sleeping
  allocations can take scheduler and timer locks. Code that allocates under
  scheduler-internal locks (e.g., hrtimer base lock, runqueue lock) or with
  preemption disabled must strip `__GFP_KSWAPD_RECLAIM` or use bare flags like
  `__GFP_NOWARN` to avoid lock recursion.
- `current_gfp_context()` in `include/linux/sched/mm.h` strips `__GFP_IO`
  and/or `__GFP_FS` when the task runs under a scoped
  `memalloc_noio_save()` or `memalloc_nofs_save()` constraint. After
  narrowing, a `GFP_KERNEL` allocation becomes `GFP_NOIO` or `GFP_NOFS`,
  which still include `__GFP_DIRECT_RECLAIM` (can sleep). Testing the
  narrowed value against a composite constant like
  `(gfp & GFP_KERNEL) != GFP_KERNEL` misclassifies these as atomic,
  because the stripped `__GFP_IO`/`__GFP_FS` bits cause the comparison to
  fail. Use `gfpflags_allow_blocking(gfp)` to test `__GFP_DIRECT_RECLAIM`
  (can this allocation sleep?) or test `gfp & __GFP_RECLAIM` (can this
  allocation trigger reclaim/locks?). See `include/linux/gfp.h`.

**Placement constraints** (see "Page mobility and placement hints" in
`include/linux/gfp_types.h`):
- `GFP_ZONEMASK` (`__GFP_DMA | __GFP_HIGHMEM | __GFP_DMA32 | __GFP_MOVABLE`)
  selects the physical memory zone. Code that intercepts allocations and serves
  memory from a pre-allocated pool (e.g., KFENCE in `mm/kfence/core.c`, swiotlb
  in `kernel/dma/swiotlb.c`) must skip requests with zone constraints it cannot
  satisfy.
- `__GFP_THISNODE` forces the allocation to the requested NUMA node with no
  fallback. It is NOT part of `GFP_ZONEMASK` -- checking only `GFP_ZONEMASK`
  misses this constraint. Pool-based allocators on NUMA systems must also check
  `__GFP_THISNODE` when their pool pages may not reside on the caller's
  requested node.
- When stripping placement flags for validation, check against the mask:
  `GFP_ZONEMASK | __GFP_RECLAIMABLE | __GFP_WRITE | __GFP_HARDWALL |
  __GFP_THISNODE | __GFP_MOVABLE`

## __GFP_ACCOUNT

Incorrect memcg accounting lets a container allocate kernel memory without being
charged, bypassing its memory limit. Review any new `__GFP_ACCOUNT` usage or
`SLAB_ACCOUNT` cache creation.

- Slabs created with `SLAB_ACCOUNT` are charged to memcg automatically via
  `memcg_slab_post_alloc_hook()` in `mm/slub.c`, even without explicit
  `__GFP_ACCOUNT` in the allocation call.

**Validation:**
1. When using `__GFP_ACCOUNT`, ensure the correct memcg is charged
   - `old = set_active_memcg(memcg); work; set_active_memcg(old)`
2. Most usage does not need `set_active_memcg()`, but:
   - Kthreads switching context between many memcgs may need it
   - Helpers operating on objects (e.g., BPF maps) with stored memcg may need it
3. Ensure new `__GFP_ACCOUNT` usage is consistent with surrounding code.

## Mempool Allocation Guarantees

`mempool_alloc()` retries forever when `__GFP_DIRECT_RECLAIM` is set (GFP_KERNEL,
GFP_NOIO, GFP_NOFS) -- NULL checks are dead code. Without it (GFP_ATOMIC,
GFP_NOWAIT) it can fail -- missing NULL checks cause crashes. Match error
handling to the GFP flag (see `mempool_alloc_noprof()` in `mm/mempool.c`).

## Memblock Range Parameter Conventions

Memblock uses two conventions: `(base, size)` for `memblock_add()`,
`memblock_remove()`, etc., and `(start, end)` for `reserve_bootmem_region()`,
`__memblock_find_range_*()`. Both parameters are `phys_addr_t` -- no compiler
type safety. Common mistake: passing `end` where `size` is expected (or vice
versa) in loops computing both `start = region->base` and
`end = start + region->size`. Check the function's parameter name (`size` vs
`end`) at each call site.

## kmemleak Tracking Symmetry

Allocation/free APIs must pair symmetrically for kmemleak: `kmalloc()` with
`kfree()`/`kfree_rcu()`, and `kmem_cache_alloc()` with `kmem_cache_free()`.
Mixing mismatched alloc/free APIs causes "Trying to color unknown object"
warnings or false leak reports.

When allocations occur under memory pressure without `__GFP_DIRECT_RECLAIM`,
kmemleak may fail to allocate internal metadata objects, leaving the pointer
untracked. Calling `kmemleak_not_leak()`, `kmemleak_ignore()`, or
`kmemleak_no_scan()` on untracked objects generates kernel warnings. When an
allocation path can drop kmemleak registration under pressure, be cautious
when unconditionally mutating kmemleak object state.

## Quick Checks for Callers

- **NUMA node ID validation before `NODE_DATA()`**: `NODE_DATA(nid)` has no
  bounds check. User-provided node IDs need: `nid >= 0 && nid < MAX_NUMNODES
  && node_state(nid, N_MEMORY)`. See `do_pages_move()` in `mm/migrate.c`
- **`get_node(s, numa_mem_id())`** can return NULL on systems with memory-less
  nodes (see `get_node()` in `mm/slub.c`). A missing NULL check causes a
  NULL-pointer dereference that only triggers on NUMA systems with memory-less
  nodes.
- **Node mask selection for allocation loops**: `for_each_online_node()`
  includes memoryless nodes. Use `for_each_node_state(nid, N_MEMORY)` for
  memory allocation. During early boot, `N_MEMORY` may not be populated yet
  (`free_area_init()` in `mm/mm_init.c` sets it); use memblock ranges instead.
- **NUMA node count vs node ID range**: `num_node_state()` returns a count,
  not an upper bound on IDs (IDs can be sparse). Use `nr_node_ids` as the
  upper bound for raw iteration, or `for_each_node_state(nid, N_MEMORY)`.
- **NUMA mempolicy-aware vs node-specific allocation**: `alloc_pages_node()`
  / `__alloc_pages_node()` bypass task NUMA policy (`mbind()`,
  `set_mempolicy()`). Replacing `alloc_pages()` / `folio_alloc()` with
  `_node` variants silently drops mempolicy — invisible in testing, pages
  land on wrong nodes. Branch: mempolicy-aware for `NUMA_NO_NODE`,
  node-specific for explicit node. See `___kmalloc_large_node()` in
  `mm/slub.c`.
- **GFP flag propagation in allocation helpers**: when a function wraps
  an allocation and adds its own GFP flags (e.g., `__GFP_ZERO`,
  `__GFP_NOWARN`), it must preserve the caller's flags via bitwise OR,
  not replace them. Replacing the caller's `GFP_KERNEL` with
  `GFP_KERNEL | __GFP_ZERO` is correct; replacing it with just
  `__GFP_ZERO` drops reclaim and IO flags.
- **`__GFP_MOVABLE` mobility contract**: pages allocated with
  `__GFP_MOVABLE` MUST be reclaimable or migratable. Common mistake:
  `movable_operations` registered conditionally (`#ifdef CONFIG_COMPACTION`)
  while `__GFP_MOVABLE` passed unconditionally. **REPORT as bugs**:
  `__GFP_MOVABLE` on pages with no migration support.
- **User page zeroing on cache-aliasing architectures**: `__GFP_ZERO` uses
  `clear_page()` which skips the dcache flush that `clear_user_highpage()`/
  `folio_zero_user()` provides. On cache-aliasing architectures, user-mapped
  pages need the flush. Use `user_alloc_needs_zeroing()` to check. Any
  optimization replacing `clear_user_highpage()` with `__GFP_ZERO` is wrong
  on these architectures.
- **Early boot memory allocation failures**: functions executed only early in the boot
  process (e.g., marked with `__init`) usually do not need to handle memory
  allocation failures gracefully. At this stage, physical memory should be
  available, and an allocation failure typically means the system cannot boot
  anyway. Complex error handling, cleanup logic, or returning `-ENOMEM` in
  these functions is often unnecessary dead code.
- **NOWAIT error code translation**: NOWAIT callers expect `-EAGAIN` (retry
  in blocking context), not `-ENOMEM` (fatal). When downgrading GFP to
  NOWAIT, translate allocation failure to `-EAGAIN`. See
  `__filemap_get_folio()` `FGP_NOWAIT` in `mm/filemap.c`.
- **GFP_KERNEL under locks in reclaim-reachable paths**: `GFP_KERNEL` can
  trigger direct reclaim, re-entering MM through swap-out, writeback, or slab
  shrinking. Deadlock if the allocation holds a lock reclaim also acquires.
  Move allocations outside the critical section or use `GFP_NOWAIT`/`GFP_ATOMIC`.
