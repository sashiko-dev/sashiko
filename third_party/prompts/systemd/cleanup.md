# Cleanup Attribute Patterns

## When to Load
Load when patch touches:
- Any `_cleanup_*` attributes
- `DEFINE_TRIVIAL_CLEANUP_FUNC` usage
- `TAKE_PTR()`, `TAKE_FD()` macros
- Error paths with resource management

## Key Macros

### Common Cleanup Attributes
| Macro | Calls | Safe for NULL? |
|-------|-------|----------------|
| `_cleanup_free_` | `free()` | Yes |
| `_cleanup_close_` | `safe_close()` | Yes (-EBADF) |
| `_cleanup_fclose_` | `safe_fclose()` | Yes |
| `_cleanup_closedir_` | `safe_closedir()` | Yes |
| `_cleanup_hashmap_free_` | `hashmap_free()` | Yes |
| `_cleanup_set_free_` | `set_free()` | Yes |
| `_cleanup_strv_free_` | `strv_free()` | Yes |
| `_cleanup_(sd_bus_unrefp)` | `sd_bus_unref()` | Yes |
| `_cleanup_(sd_event_unrefp)` | `sd_event_unref()` | Yes |

### Ownership Transfer
```c
/* Transfer pointer ownership */
result = TAKE_PTR(p);  /* p becomes NULL */

/* Transfer FD ownership */
result_fd = TAKE_FD(fd);  /* fd becomes -EBADF */
```

## Patterns

### CLEANUP-001: Cleanup Function Compatibility
**Risk**: Use-after-free, crash on cleanup

**Validation**:
1. Identify the cleanup function for each `_cleanup_` variable
2. Identify the allocator function
3. Verify cleanup handles ALL possible values:
   - NULL (most cleanups handle this)
   - Partially initialized objects
   - Error values (some allocators return -ERRNO in FD)

**Example - CORRECT**:
```c
_cleanup_free_ char *p = NULL;
p = strdup("hello");
if (!p)
        return -ENOMEM;  /* free(NULL) is safe */
```

**Example - RISKY**:
```c
_cleanup_close_ int fd = -EBADF;  /* Initialize to invalid */
fd = open(path, O_RDONLY);
if (fd < 0)
        return -errno;  /* safe_close(-EBADF) is safe */
```

### CLEANUP-002: LIFO Cleanup Order
**Risk**: Use-after-free, cleanup with wrong state

Cleanup runs in **reverse definition order** (last defined = first cleaned).

**Example - CORRECT**:
```c
_cleanup_close_ int fd = -EBADF;  /* Defined first, cleaned last */
_cleanup_free_ char *buf = NULL;  /* Defined second, cleaned first */

fd = open(path, O_RDONLY);
buf = malloc(SIZE);
/* On return: free(buf) then safe_close(fd) - correct! */
```

**Example - WRONG**:
```c
_cleanup_free_ char *buf = NULL;  /* Defined first */
_cleanup_close_ int fd = -EBADF;  /* Defined second */

fd = open(path, O_RDONLY);
buf = read_file(fd);  /* buf depends on fd */
/* On return: safe_close(fd) then free(buf) - fd closed first!
 * This is OK here, but shows the LIFO order */
```

**Critical case with locks/guards**:
```c
/* WRONG - resource defined before lock! */
_cleanup_free_ Object *obj = NULL;
_cleanup_(mutex_unlockp) Mutex *m = mutex_lock(&lock);
obj = allocate_under_lock();
/* cleanup order: unlock, then free - obj might need lock! */

/* CORRECT - lock first */
_cleanup_(mutex_unlockp) Mutex *m = mutex_lock(&lock);
_cleanup_free_ Object *obj = allocate_under_lock();
/* cleanup order: free (under lock), then unlock */
```

### CLEANUP-003: No Mixing goto and Cleanup
**Risk**: Confusion, double-free

**Rule**: In a function, either use goto cleanup pattern OR cleanup attributes,
never mix them.

**Example - WRONG**:
```c
_cleanup_free_ char *a = strdup("a");
char *b = strdup("b");

if (!a || !b)
        goto cleanup;  /* WRONG: mixing patterns */

cleanup:
        free(b);
```

**Example - CORRECT** (all cleanup attributes):
```c
_cleanup_free_ char *a = strdup("a");
_cleanup_free_ char *b = strdup("b");

if (!a || !b)
        return -ENOMEM;  /* Both cleaned up automatically */
```

### CLEANUP-004: Ownership Transfer with TAKE_PTR/TAKE_FD
**Risk**: Double-free, use-after-transfer

**When to use TAKE_PTR()**:
- Passing ownership to another structure
- Returning ownership to caller
- Storing in a container that will free it

```c
_cleanup_free_ char *p = strdup("hello");
if (!p)
        return -ENOMEM;

/* Transfer to hash table (hash table now owns it) */
r = hashmap_put(h, key, p);
if (r < 0)
        return r;  /* p still valid, will be freed */

TAKE_PTR(p);  /* Mark as transferred, prevent double-free */
return 0;
```

**TAKE_FD() for file descriptors**:
```c
_cleanup_close_ int fd = open(path, O_RDONLY|O_CLOEXEC);
if (fd < 0)
        return -errno;

/* Return FD to caller */
*ret_fd = TAKE_FD(fd);  /* fd becomes -EBADF */
return 0;
```

### CLEANUP-005: Early Return Safety
**Risk**: Cleanup of uninitialized or invalid values

**Always initialize cleanup variables**:
```c
/* CORRECT */
_cleanup_free_ char *p = NULL;  /* Initialize to NULL */
_cleanup_close_ int fd = -EBADF;  /* Initialize to invalid FD */

if (some_condition)
        return -EINVAL;  /* Safe: free(NULL), safe_close(-EBADF) */
```

**Check early return paths**:
```c
_cleanup_free_ char *p = NULL;

p = malloc(size);
if (!p)
        return -ENOMEM;

if (validate(p) < 0) {
        /* p is valid here, will be freed - CORRECT */
        return -EINVAL;
}

return 0;  /* TAKE_PTR if transferring ownership */
```

## Validation Checklist

For each `_cleanup_` variable in a patch:

1. **Initialization**:
   - [ ] Initialized to NULL or -EBADF (as appropriate)
   - [ ] Not left uninitialized

2. **Allocator compatibility**:
   - [ ] Allocator return type matches cleanup expectation
   - [ ] NULL return from allocator is handled

3. **LIFO order**:
   - [ ] Dependencies defined in correct order
   - [ ] Locks/guards defined before resources they protect

4. **Ownership transfer**:
   - [ ] TAKE_PTR/TAKE_FD used when transferring ownership
   - [ ] No use of variable after TAKE_*

5. **No mixing patterns**:
   - [ ] No goto cleanup in same function
   - [ ] Either all resources use attributes or none do

## Quick Checks

- [ ] All _cleanup_ vars initialized
- [ ] LIFO order is correct for dependencies
- [ ] TAKE_PTR/TAKE_FD used for ownership transfer
- [ ] No mixing goto with _cleanup_ attributes
- [ ] Cleanup function handles all possible values
