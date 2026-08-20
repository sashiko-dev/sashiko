# Linux Kernel Technical Patterns

## Core Execution Flow Invariants

- Trace full execution flow and gather context across the call chain to verify behavior.
- Never make assumptions based solely on return types, comments, or error handling patterns; verify the code by tracing execution paths.
- Changing a `WARN_ON()` statement changes console output and diagnostic reporting without altering execution flow; `BUG_ON()` terminates execution and triggers a kernel oops or panic.
- Kernel documentation and inline comments can occasionally be outdated or incomplete:
  - Verify the actual implementation in code rather than relying solely on comments.
  - Inspect `#ifdef`/`#else` branches where differing configurations implement distinct semantics.
- Do not recommend defensive checks unless addressing a verifiable bug reachable in practice.

### Error Handling

- A condition guarded by `WARN_ON()` or `BUG_ON()` is assumed unreachable in normal operation unless evidence demonstrates reachable triggering inputs.
- Error cleanup paths must unwind all acquired resources in reverse order of allocation without double freeing.

### Bounds and Validation

- Bounds checks are required for untrusted external inputs (userspace, network, hardware descriptors).
- Defensive bounds checks on trusted internal kernel data structures must be avoided unless an actual overflow or out-of-bounds access path is demonstrable.

### Kernel Execution Context Rules

- **Preemption disabled**: Per-CPU variables are safe from thread preemption, but can still be interrupted by hard/soft IRQs.
- **Migration disabled**: Execution remains on the current CPU, but preemption remains possible unless in an atomic section.
- **`typeof()` safety**: `typeof()` operates at compile time and is safe to use in `container_of()` before runtime object initialization.
- **`READ_ONCE()`**: `READ_ONCE()` is not required when the data structure access is serialized by an enclosing lock held by the thread.
- **`likely()` / `unlikely()`**: Compiler branch prediction hints do not alter functional behavior.

### RCU Lifecycle Invariants

- When removing an object from an RCU-protected data structure:
  - The object must be unlinked/removed from the shared structure **before** invoking `call_rcu()`, `synchronize_rcu()`, or `kfree_rcu()`.
  - Unlinking an object from inside the RCU callback function is invalid and leads to use-after-free, as concurrent readers may still traverse the data structure during grace period transitions.
- `call_rcu()` defers callback invocation until after a grace period completes, ensuring all reader critical sections have ended.

### `list_head` API Invariants

- Insertion functions (such as `list_add()`, `list_add_tail()`) initialize the `new` list node pointers, but require the list `head` to have been previously initialized (`INIT_LIST_HEAD` or `LIST_HEAD`).
- Nodes removed via `list_del()` or `list_del_init()` must be properly freed, re-initialized, or returned to avoid resource leaks.

### Resource Management and Lifecycle

- Every allocated resource must adhere to balanced lifecycle transitions: allocation -> initialization -> usage -> cleanup -> free.
- `refcount_t` instances cannot be incremented once their value reaches zero (`refcount_inc` on 0 triggers a warning and saturates).
- `refcount_dec_and_test()` returns true only when the decrement transitions the counter to zero.
- Global and static variables are implicitly zero-initialized by the BSS segment.
- Lock structures embedded within statically defined parent structures require explicit macro initialization (e.g., `__SPIN_LOCK_INITIALIZER(lockname)`, `__MUTEX_INITIALIZER(mutexname)`) rather than generic zero-initialization.
- When tearing down resources referenced by struct members, set pointers to NULL if the containing struct remains live, preventing stale pointer dereferences.

### Pointer and Error Semantics

- Values produced by `ERR_PTR()` encode negative error values into a pointer type; evaluating `if (ptr)` is true for error pointers, but dereferencing an error pointer without checking `IS_ERR(ptr)` causes a kernel oops.
- Do not flag `ptr = foo->bar` as an unsafe dereference of `bar`; it only dereferences `foo` to read the member value.
