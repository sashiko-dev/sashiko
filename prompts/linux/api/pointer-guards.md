# Pointer Error Handling and Guard Invariants

## Error Pointer Semantics (IS_ERR / PTR_ERR / ERR_PTR)

The Linux kernel encodes negative error integers into pointer values within the
top memory page `(-4095UL .. -1UL)` (defined in `include/linux/err.h`).

| Helper | Input | Output / Return | Use Case |
|--------|-------|-----------------|----------|
| `ERR_PTR(err)` | `long err` (e.g. `-ENOMEM`) | `void *` (encoded error) | Return encoded error from pointer-returning function |
| `PTR_ERR(ptr)` | `const void *ptr` | `long` (decoded error) | Extract error code when `IS_ERR(ptr)` is true |
| `IS_ERR(ptr)` | `const void *ptr` | `bool` | Check if pointer is an encoded error |
| `IS_ERR_OR_NULL(ptr)` | `const void *ptr` | `bool` | Check if pointer is NULL or an encoded error |
| `PTR_ERR_OR_ZERO(ptr)` | `const void *ptr` | `int` | Returns `PTR_ERR(ptr)` if `IS_ERR(ptr)`, else 0 |
| `ERR_CAST(ptr)` | `const void *ptr` | `void *` (re-cast error) | Propagate error pointer across incompatible pointer types |

## Core Error Handling Rules

1. **Strict API Contract Matching**:
   - Functions returning `NULL` on error (e.g. `kmalloc()`, `dma_alloc_coherent()`)
     MUST be checked with `!ptr` or `ptr == NULL`, NEVER with `IS_ERR()`.
   - Functions returning `ERR_PTR` on error (e.g. `kthread_run()`, `device_create()`,
     `clk_get()`, `fwnode_create_software_node()`) MUST be checked with `IS_ERR()`,
     NEVER with `!ptr`.
   - Functions returning optional objects (valid pointer on success, `NULL` when
     disabled/optional, `ERR_PTR` on failure) MUST use `IS_ERR_OR_NULL()` or check
     `IS_ERR()` first before checking `NULL`.

2. **PTR_ERR(NULL) is 0**:
   - `PTR_ERR(NULL)` evaluates to 0. If code does `if (!ptr) return PTR_ERR(ptr);`,
     it returns success (0) instead of an error code.

3. **Avoid Defensive IS_ERR_OR_NULL**:
   - If an API is documented to return either a valid pointer or `ERR_PTR` (and
     never `NULL`), using `IS_ERR_OR_NULL()` is defensive code that masks bugs.
     Only use `IS_ERR_OR_NULL()` when `NULL` is a legitimate, documented return
     state (such as optional regulators, clocks, or GPIOs).

## Implicit Guard and Secondary State Coupling

Kernel code frequently avoids redundant NULL or error checks when secondary
state or control flow guarantees pointer validity:

- **State / Enum Guards**: When a struct field (e.g., `dev->state == RUNNING`) or
  boolean flag is only set after a pointer is successfully allocated and
  initialized, dereferencing the pointer under that state check is safe.
- **Initialization Pairings**: Subsystem registration helpers initialize related
  structures atomically; if one component is verified, coupled pointers can be
  assumed valid.
- **Teardown Symmetrical Guards**: Before flagging a potential NULL dereference,
  trace the lifecycle setters and clearing paths to verify if the guard condition
  and the pointer are modified under the same lock or lifecycle phase.
- **Do Not Suggest Defensive NULL Checks**: If existing code paths, calling
  contexts, or preceding subsystem checks already guarantee validity, do not
  request redundant `if (!ptr)` checks.

## Quick Checks

- Checking a `kmalloc` / `kzalloc` return value with `IS_ERR()` -> Bug (missed failure)
- Checking an `ERR_PTR` API with `!ptr` -> Crash (error pointer dereferenced)
- `if (!ptr) return PTR_ERR(ptr);` -> Bug (returns 0 / success on failure)
- Unnecessary `IS_ERR_OR_NULL()` on non-optional kernel APIs -> Defensive code
- Calling `PTR_ERR()` without verifying `IS_ERR()` -> Undefined / corrupted error code
