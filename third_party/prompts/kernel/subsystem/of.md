# Open Firmware (Device Tree) Subsystem Details

## OF Node Iterator Macros and Reference Counting

Using `of_node_put()` incorrectly in OF node iterator loops causes double-free
of OF node references, leading to use-after-free when the same node is later
accessed. The iterator macros in `include/linux/of.h` automatically manage
node reference counts during iteration.

The following iterator macros automatically call `of_node_put()` on the
previous node before advancing to the next:
- `for_each_child_of_node(parent, child)`
- `for_each_available_child_of_node(parent, child)`
- `for_each_reserved_child_of_node(parent, child)`
- `for_each_node_by_type(dn, type)`
- `for_each_compatible_node(dn, type, compatible)`
- `for_each_matching_node(dn, matches)`
- `for_each_matching_node_and_match(dn, matches, match)`
- `for_each_node_with_property(dn, prop_name)`

Each of these macros expands to a `for` loop whose increment expression passes
the current node as the `prev`/`from` argument to the underlying lookup
function (e.g. `of_get_next_child()`, `of_find_node_by_type()`). That function
drops the reference on `prev`/`from` and acquires a reference on the returned
node.

Reference handling semantics:
- The iterator macro holds a reference to the current node during each
  iteration.
- When the loop advances (including via `continue`), the macro automatically
  releases the current node's reference before acquiring the next.
- Manual `of_node_put()` is ONLY needed when exiting the loop early via
  `break` or `return`.
- Adding `of_node_put()` on every iteration creates a double-free.

```c
// CORRECT: Normal iteration, no manual put needed
for_each_child_of_node(parent, node) {
    if (!matches(node))
        continue;  // Macro handles the put
    process(node);
}

// CORRECT: Early break requires manual put
for_each_child_of_node(parent, node) {
    if (found(node)) {
        of_node_put(node);
        break;
    }
}

// CORRECT: Early return requires manual put
for_each_child_of_node(parent, node) {
    if (error_condition(node)) {
        of_node_put(node);
        return -EINVAL;
    }
}

// WRONG: Goto label that runs on every iteration
for_each_child_of_node(parent, node) {
    if (!matches(node))
        goto next;
    process(node);
next:
    of_node_put(node);  // Double-free!
}

// WRONG: Explicit put before continue
for_each_child_of_node(parent, node) {
    if (!matches(node)) {
        of_node_put(node);  // Double-free!
        continue;
    }
}
```

Scoped variants `for_each_child_of_node_scoped()` and
`for_each_available_child_of_node_scoped()` declare the iterator variable
with `__free(device_node)`, so the reference is automatically released when
the variable goes out of scope. These do NOT require manual `of_node_put()`
on early exit.

## OF Node Acquisition APIs

Failing to release OF node references acquired via explicit API calls causes
memory leaks. These APIs return a node with an incremented reference count
and require a matching `of_node_put()`:

- `of_find_node_by_path()`
- `of_find_node_by_name()`
- `of_find_node_by_type()`
- `of_find_compatible_node()`
- `of_find_matching_node_and_match()`
- `of_find_node_with_property()`
- `of_get_parent()`
- `of_get_next_parent()` -- acquires a reference on the returned parent and
  drops the reference on the input node
- `of_get_child_by_name()`
- `of_parse_phandle()`
- `of_find_node_by_phandle()`
- `of_node_get()` -- explicitly increments the reference count

`of_find_node_by_name()`, `of_find_node_by_type()`, `of_find_compatible_node()`,
`of_find_matching_node_and_match()`, and `of_find_node_with_property()` also
drop the reference on their `from` argument, so callers must not call
`of_node_put()` on `from` separately after invoking these functions.

## MSI Controller DT Binding Variants

Failing to handle all MSI controller DT binding variants causes MSI allocation
failures on platforms using the unsupported binding style.

Two binding styles exist for MSI controllers:

1. **`msi-map` binding** -- maps device IDs to MSI controller domains. Uses
   `msi-map-mask` to mask the input ID before lookup. Handled by `of_map_id()`
   (called from `of_msi_xlate()` in `drivers/of/irq.c`).

2. **`msi-parent` binding** -- identifies the MSI controller via a phandle.
   When `#msi-cells` is missing or zero, this indicates a 1:1 ID mapping
   (see `of_check_msi_parent()` in `drivers/of/irq.c`). When `#msi-cells`
   is present and non-zero, the phandle carries additional specifier arguments.

`of_msi_xlate()` walks the device hierarchy and checks for both `msi-map` and
`msi-parent` properties, returning the mapped MSI ID and optionally resolving
the MSI controller node. `of_msi_get_domain()` resolves the MSI irq domain
by iterating `msi-parent` phandles with `#msi-cells`.

## Quick Checks

- `of_node_put()` inside a `for_each_*` OF iterator loop must only execute on
  early exit paths (`break`/`return`). Normal iteration and `continue` do not
  need manual cleanup. The `_scoped` variants need no manual cleanup at all.
- `of_parse_phandle()` returns a node with an incremented reference count. The
  caller must call `of_node_put()` when done.
- `for_each_property_of_node()` iterates the property linked list directly and
  does not involve reference counting.
