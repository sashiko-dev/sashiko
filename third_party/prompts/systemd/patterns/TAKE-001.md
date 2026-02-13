# TAKE-001: Ownership Transfer Patterns

## Risk Level: HIGH
## Type: Memory Safety (double-free, use-after-free)

## Trigger
Load when patch uses `TAKE_PTR()`, `TAKE_FD()`, or modifies ownership transfer.

## The Problem

When transferring ownership of a resource from a cleanup variable to another
owner (hashmap, return value, struct field), the cleanup attribute must be
inhibited to prevent double-free.

## TAKE_PTR() Semantics

```c
#define TAKE_PTR(ptr) ({                        \
        typeof(ptr) _ptr_ = (ptr);              \
        (ptr) = NULL;                           \
        _ptr_;                                  \
})
```

Sets the source to NULL and returns the original value.

## TAKE_FD() Semantics

```c
#define TAKE_FD(fd) ({                          \
        int _fd_ = (fd);                        \
        (fd) = -EBADF;                          \
        _fd_;                                   \
})
```

Sets the source to -EBADF and returns the original value.

## Common Issues

### Issue 1: Missing TAKE_* on ownership transfer

```c
/* WRONG - double free */
_cleanup_free_ char *p = strdup("hello");
hashmap_put(h, key, p);  /* Hashmap now "owns" p */
return 0;  /* BUG: _cleanup_free_ also frees p! */

/* CORRECT */
_cleanup_free_ char *p = strdup("hello");
hashmap_put(h, key, p);
TAKE_PTR(p);  /* Inhibit cleanup, hashmap owns it */
return 0;
```

### Issue 2: Use after TAKE_*

```c
/* WRONG - use after ownership transfer */
_cleanup_free_ char *p = strdup("hello");
result = TAKE_PTR(p);
log_debug("Value was: %s", p);  /* BUG: p is NULL now! */

/* CORRECT */
_cleanup_free_ char *p = strdup("hello");
log_debug("Value is: %s", p);  /* Use before transfer */
result = TAKE_PTR(p);
```

### Issue 3: Conditional ownership transfer

```c
/* TRICKY - conditional transfer */
_cleanup_free_ char *p = strdup("hello");

if (should_store) {
        hashmap_put(h, key, p);
        TAKE_PTR(p);  /* Only take if stored */
}
/* If not stored, cleanup frees p - correct! */
return 0;
```

### Issue 4: FD transfer without TAKE_FD

```c
/* WRONG */
_cleanup_close_ int fd = open(path, O_RDONLY|O_CLOEXEC);
*ret_fd = fd;
return 0;  /* BUG: fd gets closed, caller has invalid FD */

/* CORRECT */
_cleanup_close_ int fd = open(path, O_RDONLY|O_CLOEXEC);
*ret_fd = TAKE_FD(fd);
return 0;
```

## Verification Steps

1. **Identify ownership transfers**:
   - Return values from functions
   - Storage in containers (hashmap, list, struct)
   - Output parameters (ret_*)

2. **Check for TAKE_***:
   - Is TAKE_PTR/TAKE_FD used at transfer point?
   - Is it the last use of the variable?

3. **Check for use-after-take**:
   - Any access to variable after TAKE_*?
   - Remember: variable is NULL/-EBADF after TAKE_*

4. **Verify containers free resources**:
   - If stored in hashmap, does hashmap_free handle it?
   - Are there multiple owners?

## Safe Patterns

```c
/* Pattern 1: Return ownership */
_cleanup_free_ char *p = strdup(input);
if (!p)
        return NULL;

/* Process p... */

return TAKE_PTR(p);

/* Pattern 2: Store in container */
_cleanup_free_ char *p = strdup(input);
if (!p)
        return -ENOMEM;

r = hashmap_put(h, key, p);
if (r < 0)
        return r;  /* p freed by cleanup on error */

TAKE_PTR(p);  /* Success: hashmap owns it */
return 0;

/* Pattern 3: Output parameter */
int get_fd(int *ret_fd) {
        _cleanup_close_ int fd = open(...);
        if (fd < 0)
                return -errno;

        *ret_fd = TAKE_FD(fd);
        return 0;
}
```

## Output

After analysis, report:
- Ownership transfer points
- Whether TAKE_* is used correctly
- Any use-after-take issues
- Any missing TAKE_* on transfer
