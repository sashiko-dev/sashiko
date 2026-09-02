# QEMU Error Handling Review Guidelines

## Core Principles
QEMU uses the GLib-inspired `Error *` / `Error **errp` pattern for uniform, structured error reporting.

## Critical Invariants to Verify

1. **`ERRP_GUARD()` Usage**:
   - If a function inspects `*errp` (e.g., checking `if (*errp) ...`), it MUST declare `ERRP_GUARD();` at the beginning of the function body.
   - Without `ERRP_GUARD()`, if the caller passed `NULL` or `&error_fatal`, dereferencing `*errp` results in a segmentation fault.

2. **Error Memory Leaks**:
   - If an error is caught and discarded or converted to an alternative recovery path, it must be explicitly freed:
     ```c
     Error *local_err = NULL;
     if (!do_something(&local_err)) {
         error_free(local_err);
         // fallback action
     }
     ```
   - Overwriting an existing non-null `*errp` pointer without freeing it leaks the previous error object.

3. **Boolean Return Values**:
   - Functions that accept `Error **errp` should return `bool` (`true` on success, `false` on failure) rather than void,
     allowing callers to write clean `if (!foo(..., errp)) return;` checks.
