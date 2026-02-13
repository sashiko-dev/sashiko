# NFSD-002: Reference counting balance

**Risk**: Use-after-free, double-free, resource leak

**Applies**: Generic pattern ../../callstack.md to NFSD-specific objects

## NFSD uses three reference counting patterns

### 1. refcount_t (preferred for new code)

- NFSv4 stateids (`sc_count`), file objects (`fi_ref`), file cache (`nf_ref`)
- Functions: `refcount_set()`, `refcount_inc()`, `refcount_dec_and_test()`
- Example: `put_nfs4_file()` at state.h:831

### 2. atomic_t (legacy, still widely used)

- Sessions (`se_ref`), clients (`cl_rpc_users`), state owners (`so_count`)
- File access tracking (`fi_access[2]`), statistics counters
- Functions: `atomic_set()`, `atomic_inc()`, `atomic_dec_and_test()`
- Example: `nfsd4_put_session()` at nfs4state.c:253

### 3. kref (for complex destruction)

- Blocked locks (`nbl_kref`), debugfs client refs (`cl_ref`)
- Functions: `kref_init()`, `kref_get()`, `kref_put()`
- Example: blocked lock cleanup at nfs4state.c:321-334

## Special cases

- **cache_head-based**: `exp_get()` / `exp_put()` for exports (export.h:125-134)
- **File handles**: `fh_copy()` / `fh_put()` - not typical refcounting (nfsfh.c)
- **Per-CPU refs**: `nfsd_net_try_get()` / `nfsd_net_put()` for namespaces (nfssvc.c:207-214)

## Critical rule

Resources assigned to struct fields AFTER all error checks complete

## Example vulnerability

Early assignment in nfsd_set_fh_dentry() (b3da9b141578)

## Pattern to detect

```c
// WRONG - assignment before error check
obj->resource = get_resource();  // refcount acquired and stored
if (error_condition) {
    put_resource(obj->resource);  // Cleanup but field still set
    return error;                  // Leaves dangling pointer
}

// CORRECT - temp variable pattern
tmp = get_resource();             // Acquire to temp variable
if (error_condition) {
    put_resource(tmp);             // Release temp on error
    return error;
}
obj->resource = tmp;              // Assign only after validation
```

## Verification checklist

- [ ] Every `*_get()`, `refcount_inc()`, `atomic_inc()`, `kref_get()` has matching release
- [ ] Error paths release all acquired references before returning
- [ ] Resources not assigned to struct fields until all checks pass
- [ ] No double-free on error paths (check if field already contains value)
- [ ] Locks held when using `*_dec_and_lock()` variants
