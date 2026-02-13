# Sysfs Subsystem Details

## Attribute Group Visibility and Conditional Existence

Calling `kernfs_find_and_get()` on an attribute that was never created (because
`is_visible()` returned 0) returns NULL. In `sysfs_group_attrs_change_owner()`,
this causes `-ENOENT` to propagate up through `sysfs_group_change_owner()` and
`sysfs_groups_change_owner()`, failing operations like network namespace
migration that change sysfs ownership.

When a `struct attribute_group` has an `is_visible` callback that returns 0 for
some attributes, those attributes are never created during
`sysfs_create_group()` (via `create_files()` in `fs/sysfs/group.c`). This
differs from attributes that exist but return `-EOPNOTSUPP` from their `show()`
function: a non-existent attribute cannot be found, modified, or have its
ownership changed. Binary attributes use a separate `is_bin_visible` callback
with the same semantics.

For named groups (where `grp->name` is set), `is_visible` and `is_bin_visible`
can also return `SYSFS_GROUP_INVISIBLE` to suppress creation of the entire
group directory, not just individual attributes.

**Key invariant**: Any code that iterates `grp->attrs[]` or `grp->bin_attrs[]`
and performs operations requiring the attribute to exist in the filesystem must
check `is_visible` / `is_bin_visible` and skip attributes that return 0.

**Function that requires this check:**
- `sysfs_group_attrs_change_owner()` in `fs/sysfs/group.c` - iterates both
  regular and binary attributes to change ownership via `kernfs_setattr()`.
  This function already performs the check in v6.19.

**Review trigger**: When a patch adds `is_visible` or `is_bin_visible`
callbacks to an existing `attribute_group`, or migrates visibility checks from
`show()` functions to `is_visible()`:
- Identify all sysfs core functions that iterate over the group's arrays
- Verify each iterator checks `is_visible` / `is_bin_visible` before calling
  `kernfs_find_and_get()` or other lookup functions
- Check for callers that trigger group iteration (e.g., namespace changes
  via `__dev_change_net_namespace()` -> `netdev_change_owner()` ->
  `device_change_owner()` -> `sysfs_change_owner()` ->
  `sysfs_groups_change_owner()`)

```c
// WRONG: assumes all attributes exist
for (i = 0, attr = grp->attrs; *attr; i++, attr++) {
    kn = kernfs_find_and_get(parent, (*attr)->name);
    // kn is NULL if is_visible returned 0 -> returns -ENOENT
}

// CORRECT: skip non-existent attributes
for (i = 0, attr = grp->attrs; *attr; i++, attr++) {
    if (grp->is_visible) {
        mode = grp->is_visible(kobj, *attr, i);
        if (mode & SYSFS_GROUP_INVISIBLE)
            break;
        if (!mode)
            continue;
    }
    kn = kernfs_find_and_get(parent, (*attr)->name);
}
```

## Kobject Initialization and Cleanup

Failing to clean up previously allocated kobjects when a later allocation fails
leaks kernel memory that persists for the system's lifetime. In `__init` or
`__initcall` functions, these leaks are unrecoverable.

When creating kobjects in loops or hierarchies (parent with multiple children),
every error path must clean up all resources allocated before the failure:

- `kobject_create_and_add()` allocations must be balanced with `kobject_put()`
  on error paths
- Each kobject must be individually freed with `kobject_put()` on error;
  children hold a reference on their parent (via `kobject_add()`), and
  `kobject_cleanup()` calls `kobject_put(parent)` when a child is released,
  so freeing all children will eventually drop the parent's refcount
- `sysfs_create_group()` failures must also clean up the kobject that was
  passed to it

**Loop-based initialization:**

Initialization loops that create multiple kobjects or sysfs groups must use a
centralized error cleanup path (typically via `goto`). Direct `return -ERRNO`
inside the loop leaks all resources from prior iterations.

```c
// WRONG: leaks parent and previous children on failure
for (int i = 0; i < N; i++) {
    children[i] = kobject_create_and_add(name, parent);
    if (!children[i])
        return -ENOMEM;  // parent and children 0..i-1 leaked
    ret = sysfs_create_group(children[i], &grp);
    if (ret)
        return ret;  // children[i] also leaked
}

// CORRECT: centralized cleanup frees each child individually
for (int i = 0; i < N; i++) {
    children[i] = kobject_create_and_add(name, parent);
    if (!children[i]) {
        ret = -ENOMEM;
        goto err_cleanup;
    }
    ret = sysfs_create_group(children[i], &grp);
    if (ret)
        goto err_cleanup;
}
return 0;

err_cleanup:
    for (int j = i; j >= 0; j--)
        kobject_put(children[j]);
    kobject_put(parent);
    return ret;
```

## Quick Checks

- **`is_visible()` returning 0**: attribute is not created, not just hidden.
  Callers cannot find, modify, or change ownership of non-existent attributes.
- **`is_bin_visible()` returning 0**: same semantics as `is_visible()` but for
  binary attributes in `grp->bin_attrs`.
- **`SYSFS_GROUP_INVISIBLE`**: when returned by `is_visible` or
  `is_bin_visible` for a named group, suppresses the entire group directory.
- **Group iteration in `fs/sysfs/group.c`**: any new function that iterates
  `grp->attrs` or `grp->bin_attrs` must respect `is_visible` / `is_bin_visible`
  if attributes can be conditionally absent.
- **Kobject cleanup in loops**: initialization functions that create kobjects
  in loops must individually free each kobject on any allocation failure.
