# CXL Subsystem Details

## Resource Initialization for HMAT APIs

CXL memory resources passed to HMAT APIs silently produce wrong results if
they lack the `IORESOURCE_MEM` type flag. `hmat_get_extended_linear_cache_size()`
in `drivers/acpi/numa/hmat.c` calls `resource_contains()` to match the
backing resource against HMAT memory targets. `resource_contains()` in
`include/linux/ioport.h` returns `false` when `resource_type()` differs
between the two resources. A resource created without `IORESOURCE_MEM`
has type `0`, while the HMAT target resource has type `IORESOURCE_MEM`, so
every comparison fails. The function then returns `0` (success) with
`*cache_size = 0`, and `cxl_acpi_set_cache_size()` in `drivers/cxl/acpi.c`
stores that zero. The visible effect is that CXL regions with extended
linear cache report half their actual size, and MCE memory-error reporting
uses incorrect offsets.

- Use `DEFINE_RES_MEM(start, size)` for CXL HPA (Host Physical Address) ranges
- Do not use `DEFINE_RES(start, size, 0)` — this creates a resource with type
  `0`, which silently fails `resource_contains()` checks against
  `IORESOURCE_MEM` resources

```c
// WRONG - missing IORESOURCE_MEM flag, cache_size silently set to 0
struct resource res = DEFINE_RES(start, size, 0);
rc = hmat_get_extended_linear_cache_size(&res, nid, &cache_size);

// CORRECT - properly typed memory resource
struct resource res = DEFINE_RES_MEM(start, size);
rc = hmat_get_extended_linear_cache_size(&res, nid, &cache_size);
```

## Quick Checks

- **Resource type for physical memory**: When creating `struct resource` for
  CXL memory ranges, verify `DEFINE_RES_MEM()` is used rather than
  `DEFINE_RES()` with explicit flags
