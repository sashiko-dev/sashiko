# Bluetooth Subsystem Details

## Advertisement Instance State Tracking

Using an incorrect state indicator to determine whether an advertisement
instance is active causes advertising to be incorrectly enabled when it
should not be, or fail to be re-enabled after pause/resume cycles, leading
to silent functional failures.

Bluetooth HCI supports multiple advertisement instances (sets). Instance 0x00
has special handling that differs from other instances:

**Instance 0x00 (Legacy Instance):**
- NOT tracked in the `adv_instances` linked list; `hci_add_adv_instance()`
  in `net/bluetooth/hci_core.c` rejects `instance < 1`
- `hdev->cur_adv_instance == 0` does NOT mean instance 0x00 is active; it only
  indicates which instance is "current" (selected), not which is enabled
- The global `HCI_LE_ADV` flag tracks whether *any* advertising is active,
  not specifically instance 0x00
- Instance 0x00's enabled state is tracked by the `HCI_LE_ADV_0` device flag

**Common mistakes:**
- Using `!hdev->cur_adv_instance` as a proxy for "instance 0x00 is active"
- Using `hci_dev_test_flag(hdev, HCI_LE_ADV)` to infer instance 0x00 state

The only correct way to query instance 0x00's enabled state is
`hci_dev_test_flag(hdev, HCI_LE_ADV_0)`. The flag is set in
`hci_cc_le_set_ext_adv_enable()` in `net/bluetooth/hci_event.c` when
`set->handle == 0` and no `adv_info` exists, and cleared in the corresponding
disable path.

## MGMT Pending Command Lifecycle

Failure to properly manage `struct mgmt_pending_cmd` ownership causes memory
leaks (command never freed) or use-after-free/double-free (command freed twice
or accessed after free). The ownership model is subtle because it changes based
on which validation function is used.

`mgmt_pending_valid()` (in `net/bluetooth/mgmt_util.c`) atomically checks
whether a pending command is still in the pending list AND removes it from
the list. After a successful call to `mgmt_pending_valid()`, the completion
callback OWNS the command's memory and MUST free it via `mgmt_pending_free()`
on all exit paths.

**Critical rules:**

- After `mgmt_pending_valid()` succeeds, call `mgmt_pending_free(cmd)` before
  returning on EVERY path (error returns, success returns, early returns)
- Never call `mgmt_pending_remove()` after `mgmt_pending_valid()` --
  `mgmt_pending_remove()` calls `list_del()` then `mgmt_pending_free()`, so
  using it after `mgmt_pending_valid()` causes a double `list_del` and
  double free
- If `mgmt_pending_valid()` returns false or `err == -ECANCELED`, the callback
  does NOT own the memory and must not free it

```c
// WRONG
void complete(struct hci_dev *hdev, void *data, int err) {
    struct mgmt_pending_cmd *cmd = data;

    if (err == -ECANCELED || !mgmt_pending_valid(hdev, cmd))
        return;

    if (err) {
        mgmt_cmd_status(...);
        return;  // BUG: cmd not freed, leaks memory
    }

    mgmt_pending_remove(cmd);  // BUG: double list_del + double free
}
```

```c
// CORRECT
void complete(struct hci_dev *hdev, void *data, int err) {
    struct mgmt_pending_cmd *cmd = data;

    if (err == -ECANCELED || !mgmt_pending_valid(hdev, cmd))
        return;

    if (err) {
        mgmt_cmd_status(...);
        mgmt_pending_free(cmd);
        return;
    }

    // success handling...
    mgmt_pending_free(cmd);
}
```

## Variable-Length MGMT Command Structures

Copying a variable-length MGMT command structure to a fixed-size stack variable
causes stack-out-of-bounds access when the flexible array member is accessed.
`sizeof()` on a struct with a flexible array member returns only the size of
the fixed fields, not the variable-length data.

Some MGMT command structures (in `include/net/bluetooth/mgmt.h`) have flexible
array members:

- `struct mgmt_cp_set_mesh` -- has `u8 ad_types[]`
- `struct mgmt_cp_load_irks` -- has `struct mgmt_irk_info irks[]`
- `struct mgmt_cp_load_long_term_keys` -- has `struct mgmt_ltk_info keys[]`

When a completion callback needs to work with these structures from
`cmd->param`:

- Use the `DEFINE_FLEX()` macro (from `include/linux/overflow.h`) to allocate
  space on the stack with room for the flexible array
- Or work directly with the `cmd->param` pointer without copying
- Use `min(__struct_size(var), len)` to prevent reading beyond the parameter
  buffer

```c
// WRONG
struct mgmt_cp_set_mesh cp;
memcpy(&cp, cmd->param, sizeof(cp));  // sizeof excludes FAM
process(cp.ad_types, len);  // stack-out-of-bounds
```

```c
// CORRECT
DEFINE_FLEX(struct mgmt_cp_set_mesh, cp, ad_types, num_ad_types, MAX_SIZE);
memcpy(cp, cmd->param, min(__struct_size(cp), len));
```

## Quick Checks

- **Instance 0x00 special handling**: when code handles advertisement instance
  0x00, verify it does not assume standard `adv_instances` list tracking applies
- **State indicator accuracy**: when code checks if an advertisement instance
  is active, verify the field/flag being checked actually tracks enabled state,
  not just "current selection"
- **MGMT completion callbacks**: after `mgmt_pending_valid()` succeeds, verify
  `mgmt_pending_free()` is called on ALL exit paths
- **Variable-length MGMT structs**: verify structures with flexible array
  members are not copied to fixed-size stack variables
