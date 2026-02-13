# KHO (Kexec Handover) Subsystem Details

## Enabled State and Initialization

Calling serialization-side KHO APIs (such as `kho_add_subtree()` or
`kho_remove_subtree()`) when the subsystem is disabled causes a NULL
pointer dereference on `kho_out.fdt`, which is only allocated during
`kho_init()` when `kho_is_enabled()` is true. Deserialization-side APIs
like `kho_retrieve_subtree()` return `-ENOENT` safely, but preservation
APIs like `kho_preserve_folio()` silently add tracking state that will
never be used. All callers must gate KHO usage on `kho_is_enabled()`.

- `kho_is_enabled()` returns whether the KHO subsystem is active; the
  backing variable `kho_enable` is `__ro_after_init` and is set via the
  `kho=` boot parameter or `CONFIG_KEXEC_HANDOVER_ENABLE_DEFAULT`
- `is_kho_boot()` returns whether the running kernel was loaded via a
  KHO-enabled kexec (i.e., an incoming FDT was passed); only reliable
  after `kho_populate()` runs during early boot
- Check `kho_is_enabled()` at module init or at the entry point of any
  code path that uses KHO APIs; see `kho_test_init()` in
  `lib/test_kho.c` and `luo_early_startup()` in
  `kernel/liveupdate/luo_core.c` for in-tree examples

```c
// WRONG: Missing enabled check
static int __init my_kho_init(void)
{
    err = kho_add_subtree("my_node", fdt);
    // NULL deref on kho_out.fdt if KHO is disabled
}

// CORRECT: Check enabled state first
static int __init my_kho_init(void)
{
    if (!kho_is_enabled())
        return 0;

    err = kho_add_subtree("my_node", fdt);
}
```

## Preserve and Restore API Contracts

Mismatching preserve and restore calls corrupts page metadata, leaks
memory reservations across kexec, or causes the successor kernel to
interpret page state incorrectly (wrong order, wrong refcount).

- `kho_preserve_folio()` / `kho_unpreserve_folio()` operate on whole
  folios; the folio order is preserved across kexec and
  `kho_restore_folio()` recreates it as a compound page
- `kho_preserve_pages()` / `kho_unpreserve_pages()` operate on a
  contiguous range of order-0 pages; they must be restored with
  `kho_restore_pages()`, not `kho_restore_folio()`, because the restore
  path sets per-page refcounts differently (each page gets refcount 1
  vs. only the head page for folios)
- `kho_unpreserve_pages()` must be called with exactly the same `page`
  and `nr_pages` as the corresponding `kho_preserve_pages()` call;
  unpreserving arbitrary sub-ranges is not supported
- `kho_preserve_vmalloc()` / `kho_unpreserve_vmalloc()` preserve a
  vmalloc area; only `VM_ALLOC` and `VM_ALLOW_HUGE_VMAP` flags are
  supported (`KHO_VMALLOC_SUPPORTED_FLAGS` in
  `kernel/liveupdate/kexec_handover.c`); other flags cause `-EOPNOTSUPP`
- `kho_alloc_preserve()` allocates a zeroed power-of-two folio and
  preserves it in one step; pair with `kho_unpreserve_free()` to undo,
  or `kho_restore_free()` in the successor kernel to reclaim

## Subtree Lifecycle

Failing to preserve the FDT memory passed to `kho_add_subtree()` means
the successor kernel receives a dangling physical address, leading to
garbage data or faults when it calls `kho_retrieve_subtree()`.

- `kho_add_subtree()` records the physical address of a caller-owned FDT
  blob in the KHO root tree; the caller must separately preserve the
  pages backing that FDT (e.g., via `kho_preserve_folio()` or
  `kho_preserve_pages()`)
- `kho_remove_subtree()` removes a subtree by matching the physical
  address of the FDT pointer; it does not free or unpreserve the FDT
  memory -- the caller must do that
- `kho_retrieve_subtree()` is used in the successor kernel to look up a
  subtree by name; it returns the raw physical address, which must be
  converted via `phys_to_virt()` before use
- Subtree names must be unique; `kho_add_subtree()` returns `-EEXIST` if
  a node with the same name already exists

## Scratch Region Constraints

Preserving memory that overlaps a KHO scratch region corrupts the
contiguous area reserved for the successor kernel's early boot
allocations, potentially making the next kexec unbootable.

- `kho_preserve_folio()` and `kho_preserve_pages()` check for scratch
  overlap via `kho_scratch_overlap()` and return `-EINVAL` with a
  `WARN_ON` if the preserved range intersects scratch memory
- Scratch regions are reserved during `kho_reserve_scratch()` as
  CMA-backed contiguous areas scaled by `kho_scratch=` boot parameter
  (default 200% of memblock-reserved kernel memory)

## Quick Checks

- Verify every code path calling KHO serialization APIs
  (`kho_add_subtree()`, `kho_preserve_folio()`, etc.) is gated by
  `kho_is_enabled()` in the calling function or a verified ancestor
- Verify `kho_preserve_pages()` / `kho_unpreserve_pages()` are called
  with matching `page` and `nr_pages` arguments
- Verify FDT memory passed to `kho_add_subtree()` is independently
  preserved; `kho_add_subtree()` only records the physical address
- Verify error paths call the appropriate unpreserve function
  (`kho_unpreserve_folio()`, `kho_unpreserve_pages()`,
  `kho_unpreserve_vmalloc()`, or `kho_remove_subtree()` +
  `kho_unpreserve_*()`)
