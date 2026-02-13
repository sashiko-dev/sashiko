# CLEANUP-001: Cleanup Attribute Compatibility

## Risk Level: HIGH
## Type: Memory Safety (use-after-free, double-free, invalid-free)

## Trigger
Load when patch adds or modifies `_cleanup_*` variables and their allocation.

## The Problem

systemd uses GCC cleanup attributes (`__attribute__((cleanup))`) extensively.
The cleanup function is called automatically when the variable goes out of
scope. If the cleanup function cannot handle all possible values the variable
might hold, undefined behavior occurs.

## Common Issues

### Issue 1: Cleanup doesn't handle all allocator returns

```c
/* WRONG - kvfree() may not handle all krealloc() returns */
_cleanup_free_ void *p = NULL;
p = some_function_returning_error_or_ptr();
if (IS_ERR(p))
        return PTR_ERR(p);  /* BUG: free(ERR_PTR) is undefined! */
```

### Issue 2: Wrong cleanup for the type

```c
/* WRONG - strv needs strv_free, not free */
_cleanup_free_ char **strv = NULL;
strv = strv_new("a", "b", NULL);
/* BUG: free() only frees outer array, leaks strings! */

/* CORRECT */
_cleanup_strv_free_ char **strv = NULL;
```

### Issue 3: LIFO order violation

```c
/* WRONG - lock defined after resource that needs it */
_cleanup_free_ Object *obj = NULL;  /* Defined first = cleaned last */
_cleanup_(mutex_unlockp) pthread_mutex_t *m = &lock;  /* Defined second = cleaned first */

obj = alloc_object_under_lock();
return 0;
/* Cleanup order: unlock, THEN free
 * BUG: if free() needs lock held, this is wrong! */

/* CORRECT - lock first */
_cleanup_(mutex_unlockp) pthread_mutex_t *m = &lock;  /* First */
_cleanup_free_ Object *obj = alloc_object_under_lock();  /* Second */
/* Cleanup order: free (under lock), THEN unlock */
```

## Verification Steps

1. **Identify cleanup function**:
   - What function does `_cleanup_X_` call?
   - What values can it safely handle?

2. **Identify allocator**:
   - What function assigns to this variable?
   - What values can it return (NULL, valid ptr, error codes)?

3. **Check compatibility**:
   - Does cleanup handle ALL possible values from allocator?
   - Are early returns safe?

4. **Check LIFO order**:
   - Are dependencies defined in correct order?
   - Would reverse cleanup cause issues?

## Safe Patterns

```c
/* Pattern 1: Initialize to safe value */
_cleanup_free_ char *p = NULL;  /* free(NULL) is safe */
_cleanup_close_ int fd = -EBADF;  /* safe_close(-EBADF) is safe */

/* Pattern 2: Check before use */
_cleanup_free_ char *p = NULL;
p = strdup(input);
if (!p)
        return -ENOMEM;  /* free(NULL) called - safe */
/* Now p is always valid until TAKE_PTR() */

/* Pattern 3: Ownership transfer */
_cleanup_free_ char *p = strdup(input);
if (!p)
        return -ENOMEM;

hashmap_put(h, key, p);
TAKE_PTR(p);  /* Prevents double-free */
```

## Output

After analysis, report:
- Variable name
- Cleanup function and what it handles
- Allocator and what it returns
- Whether they are compatible
- LIFO order issues if any
