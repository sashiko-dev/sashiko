# Alignment Helpers

## Generic Alignment Semantics

Misreading alignment helpers causes silent boundary bugs.

- `ALIGN(x, a)` rounds `x` **up** to the first `a` boundary at or after `x`.
  If `x` is already aligned, the result stays `x`.
- `ALIGN_DOWN(x, a)` rounds `x` **down** to the containing `a` boundary.
- `IS_ALIGNED(x, a)` tests whether `x` is already aligned to `a`.
- The alignment argument `a` is expected to be a power of two.
- See `include/linux/align.h` for these alignment helpers.

## Page Alignment Helpers

Mixing page helpers with PFNs, or pageblock helpers with byte addresses,
silently computes wrong boundaries.

- `PAGE_ALIGN(addr)` is `ALIGN(addr, PAGE_SIZE)`.
- Downward page alignment is performed using `ALIGN_DOWN(addr, PAGE_SIZE)`.
- `PAGE_ALIGNED(addr)` checks `PAGE_SIZE` alignment via `IS_ALIGNED(addr, PAGE_SIZE)`.
- These helpers operate on addresses, not PFNs.
- See `include/linux/mm.h`.

## Pageblock Alignment Helpers

Confusing pageblock start, end, and upward rounding causes off-by-one
pageblock errors.

- `pageblock_nr_pages` is `1UL << pageblock_order`.
- Pageblock helpers operate on PFNs, not byte addresses.
- The core helpers are defined as:

```c
#define pageblock_align(pfn)       ALIGN((pfn), pageblock_nr_pages)
#define pageblock_aligned(pfn)     IS_ALIGNED((pfn), pageblock_nr_pages)
#define pageblock_start_pfn(pfn)   ALIGN_DOWN((pfn), pageblock_nr_pages)
#define pageblock_end_pfn(pfn)     ALIGN((pfn) + 1, pageblock_nr_pages)
```

- `pageblock_start_pfn(pfn)` returns the inclusive start PFN of the pageblock
  containing `pfn`.
- `pageblock_end_pfn(pfn)` returns the exclusive end PFN of the pageblock
  containing `pfn`, i.e. the next boundary.
- `pageblock_align(pfn)` rounds upward to the first pageblock boundary at or
  after `pfn`; for an unaligned PFN it is therefore different from
  `pageblock_start_pfn(pfn)`.
- See `pageblock_order`, `pageblock_nr_pages`, and the helper macros in
  `include/linux/pageblock-flags.h`.

Assume `pageblock_nr_pages == 512` for the numeric example below.

| PFN | `pageblock_start_pfn()` | `pageblock_align()` | `pageblock_end_pfn()` |
|-----|-------------------------|---------------------|-----------------------|
| 512 | 512 | 512 | 1024 |
| 513 | 512 | 1024 | 1024 |

**REPORT as bugs**: treating `pageblock_end_pfn(pfn)` as inclusive, or using
`pageblock_align(pfn)` where `pageblock_start_pfn(pfn)` is needed.

## Quick Checks

- **Already-aligned input**: `ALIGN(x, a)` keeps an aligned `x` unchanged; it
  does not mean "strictly next boundary".
- **Unit mismatch**: `PAGE_*` helpers operate on addresses, while
  `pageblock_*` helpers operate on PFNs.
- **Exclusive end semantics**: `pageblock_end_pfn()` returns the boundary after
  the containing pageblock.
