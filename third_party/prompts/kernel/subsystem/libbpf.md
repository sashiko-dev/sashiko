# Libbpf Public API Error Handling

## errno Convention

Public libbpf API functions (marked with `LIBBPF_API` in `tools/lib/bpf/`
headers — `libbpf.h`, `bpf.h`, `btf.h`, `libbpf_legacy.h`) must set `errno` on all
error paths. Userspace callers rely on `errno` being set when a function
returns an error — returning a negative value or NULL without setting errno
silently breaks error handling.

Three wrapper functions in `tools/lib/bpf/libbpf_internal.h` enforce this:

- `libbpf_err(ret)`: for integer-returning APIs. If `ret < 0`, sets
  `errno = -ret` and returns `ret`.
- `libbpf_err_ptr(err)`: for pointer-returning APIs with a known error code.
  Sets `errno = -err` and returns `NULL`.
- `libbpf_ptr(ret)`: for pointer-returning APIs wrapping internal functions
  that return `ERR_PTR()`. If `ret` is an error pointer, sets errno and
  returns `NULL`; otherwise returns `ret` unchanged.

## Which Functions Need Wrappers

- Public APIs (`LIBBPF_API` or listed in `libbpf.map`) — all error returns
  must use the wrappers
- Internal/static functions — do NOT use the wrappers (they use kernel-style
  negative error codes or `ERR_PTR` internally)

The wrapper must be on the `return` statement itself, not applied earlier in
the function, because errno must be set immediately before returning to the
caller.

## Return Type Patterns

### Integer returns

```c
// Direct error
return libbpf_err(-EINVAL);

// Propagated error from internal function
err = internal_func();
if (err)
    return libbpf_err(err);
```

### Pointer returns (from error codes)

```c
// Known error code
return libbpf_err_ptr(-ENOMEM);

// Stored error value
return libbpf_err_ptr(err);
```

### Pointer returns (from ERR_PTR-returning internals)

```c
// Internal function that returns ERR_PTR on failure
return libbpf_ptr(internal_func_returning_ptr());
```

## LIBBPF-001: Missing errno on Public API Error Paths

When a public libbpf API function is added or modified, verify that every
error return path uses the appropriate wrapper. Common mistakes:

- `return -EINVAL;` instead of `return libbpf_err(-EINVAL);`
- `return NULL;` instead of `return libbpf_err_ptr(-ESOMETHING);`
- `return ERR_PTR(-EINVAL);` instead of `return libbpf_err_ptr(-EINVAL);`
  (public APIs must never return `ERR_PTR` — callers check for `NULL`)
- Wrapping the error value earlier in the function but then returning a
  different error code unwrapped on a later path

Internal/static functions should NOT use these wrappers — only the public
API boundary needs them.

**REPORT as bugs**: Public libbpf API functions that return error values
(negative int or NULL) without going through `libbpf_err()`,
`libbpf_err_ptr()`, or `libbpf_ptr()`.
