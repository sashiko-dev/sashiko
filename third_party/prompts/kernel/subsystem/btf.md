# BTF Special Fields in BPF Maps

## Overview

BPF map values can contain special BTF-typed fields (spin locks, timers,
kptrs, list heads, etc.). These fields require special handling during map
copy and update operations because they hold kernel resources that cannot
be naively memcpy'd.

Two functions enforce this:

- `check_and_init_map_value(map, dst)`: called after copying a map value
  to a temporary/userspace buffer. Reinitializes special fields in the copy
  so kernel pointers and locks are not leaked to userspace.
- `bpf_obj_free_fields(map->record, ptr)`: called when overwriting or
  freeing a map value. Releases resources held by the old value (cancels
  timers, drops kptr references, frees list heads, unpins uptrs).

## Which Map Types Support Special Fields

Not all map types can have special BTF fields. The allowlists are enforced
in `map_check_btf()` (`kernel/bpf/syscall.c`). A map type not listed for a
given field type will get `-EOPNOTSUPP` at map creation time.

| Field Type | Allowed Map Types |
|-----------|-------------------|
| `BPF_SPIN_LOCK`, `BPF_RES_SPIN_LOCK` | HASH, ARRAY, CGROUP_STORAGE, SK_STORAGE, INODE_STORAGE, TASK_STORAGE, CGRP_STORAGE |
| `BPF_TIMER`, `BPF_WORKQUEUE`, `BPF_TASK_WORK` | HASH, LRU_HASH, ARRAY |
| `BPF_KPTR_UNREF`, `BPF_KPTR_REF`, `BPF_KPTR_PERCPU`, `BPF_REFCOUNT` | HASH, PERCPU_HASH, LRU_HASH, LRU_PERCPU_HASH, ARRAY, PERCPU_ARRAY, SK_STORAGE, INODE_STORAGE, TASK_STORAGE, CGRP_STORAGE |
| `BPF_UPTR` | TASK_STORAGE |
| `BPF_LIST_HEAD`, `BPF_RB_ROOT` | HASH, LRU_HASH, ARRAY |

If a map type is not in any of these allowlists, it cannot have special BTF
fields and missing `check_and_init_map_value` / `bpf_obj_free_fields` calls
are not bugs.

## Required Handling in Map Operations

### Lookup (kernel to userspace copy)

When `copy_map_value()` or `copy_map_value_long()` copies a map value into
a buffer that will be returned to userspace, `check_and_init_map_value()`
must be called on the destination buffer afterward. This zeroes out special
fields so kernel addresses and lock state are not exposed.

Reference implementations:
- Syscall path: `bpf_map_copy_value()` in `kernel/bpf/syscall.c` — calls
  `map->ops->map_lookup_elem()` for the pointer, then `copy_map_value()` +
  `check_and_init_map_value()` on the destination buffer
- Percpu: `bpf_percpu_array_copy()`, `bpf_percpu_hash_copy()` — handle
  their own copy + init internally
- Batch: `generic_map_lookup_batch()` — delegates to `bpf_map_copy_value()`

### Update (userspace to kernel copy)

When `copy_map_value()` or `copy_map_value_long()` overwrites an existing
map value with userspace data, `bpf_obj_free_fields()` must be called to
release resources held by the old value before or after the copy overwrites
them. Without this, timers keep firing, kptr references leak, and list
entries become unreachable.

Reference implementations (these call `bpf_obj_free_fields()` internally):
- Regular: `array_map_update_elem()`, `htab_map_update_elem()` (via
  `check_and_free_fields()`)
- Percpu: `bpf_percpu_array_update()`, `bpf_percpu_hash_update()` (via
  `pcpu_copy_value()`)
- Batch: `generic_map_update_batch()` — delegates to the map's
  `map_update_elem` callback

## BPF-001: Missing BTF Field Handling in Map Copy/Update

When a new map operation or map type is added that uses `copy_map_value()`
or `copy_map_value_long()`, verify:

1. Does the map type support special BTF fields? (check the table above)
2. For lookups: is `check_and_init_map_value()` called on the destination
   after copying?
3. For updates: is `bpf_obj_free_fields()` called to clean up the old value?
4. Compare with the reference implementation for the same operation type.

Percpu and non-percpu variants of the same map type may have different
allowlists — verify the exact `BPF_MAP_TYPE_*` enum value.

**REPORT as bugs**: Map operations on field-capable map types that copy
values with `copy_map_value()` without the corresponding
`check_and_init_map_value()` (lookups) or `bpf_obj_free_fields()` (updates).
Do not report for map types that are not in the `map_check_btf()` allowlists.
