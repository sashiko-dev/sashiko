# D-Bus Subsystem Patterns

## When to Load
Load when patch touches:
- `src/libsystemd/sd-bus/` - sd-bus library
- `src/shared/bus-*.c` - Bus utilities
- `src/core/dbus-*.c` - PID1 D-Bus interfaces
- Any `sd_bus_*` API usage

## Key Files
- `src/libsystemd/sd-bus/` - Core sd-bus implementation
- `src/shared/bus-util.c` - Common bus utilities
- `src/shared/bus-unit-util.c` - Unit-related bus operations
- `src/core/dbus-manager.c` - Manager D-Bus interface
- `src/core/dbus-unit.c` - Unit D-Bus interface

## Message Patterns

### DBUS-001: Message Read Validation
**Risk**: Type confusion, buffer overread

```c
r = sd_bus_message_read(m, "s", &str);
if (r < 0)
        return sd_bus_error_set_errno(error, r);
```

**Check**:
- [ ] Return value checked
- [ ] Format string matches message signature
- [ ] Partial reads handled

### DBUS-002: Message Lifetime
**Risk**: Use-after-free

Message data is only valid while message is referenced:
```c
const char *str;
r = sd_bus_message_read(m, "s", &str);
/* str points into message - don't store without copying! */

/* CORRECT - copy if needed beyond message lifetime */
_cleanup_free_ char *copy = strdup(str);
```

**Check**:
- [ ] Message data not stored without copying
- [ ] Message ref held while data in use
- [ ] Pointers don't outlive message

### DBUS-003: Array/Container Iteration
```c
r = sd_bus_message_enter_container(m, 'a', "s");
if (r < 0)
        return r;

while ((r = sd_bus_message_read(m, "s", &str)) > 0) {
        /* Process str */
}
if (r < 0)
        return r;

r = sd_bus_message_exit_container(m);
if (r < 0)
        return r;
```

**Check**:
- [ ] Container entered before iteration
- [ ] Loop exit condition correct (r > 0)
- [ ] Container exited after iteration
- [ ] Error handling at each step

## Connection Patterns

### DBUS-004: Connection Lifecycle
```c
_cleanup_(sd_bus_flush_close_unrefp) sd_bus *bus = NULL;

r = sd_bus_open_system(&bus);
if (r < 0)
        return r;

/* Use bus... */
/* Automatic cleanup flushes, closes, unrefs */
```

**Check**:
- [ ] Connection has cleanup attribute
- [ ] Connection flushed before close (or use flush_close_unref)
- [ ] No use after unref

### DBUS-005: Slot/Callback Lifetime
**Risk**: Use-after-free in callback

```c
_cleanup_(sd_bus_slot_unrefp) sd_bus_slot *slot = NULL;

r = sd_bus_match_signal(bus, &slot, service, path, interface, member,
                        callback, userdata);
```

**Critical**: `userdata` must outlive the slot!

**Check**:
- [ ] Slot unrefd before userdata freed
- [ ] Callback checks if userdata still valid
- [ ] Slot stored if callback needed long-term

### DBUS-006: Async Method Calls
```c
r = sd_bus_call_method_async(bus, &slot,
                             dest, path, interface, member,
                             callback, userdata, types, ...);
```

**Check**:
- [ ] Callback handles all outcomes (success, error, timeout)
- [ ] Userdata valid when callback fires
- [ ] Slot tracked if cancellation needed

## Error Handling

### DBUS-007: Error Reply Format
```c
/* Return error to caller */
return sd_bus_error_set_errno(error, r);

/* With custom message */
return sd_bus_error_set_errnof(error, r, "Failed to %s: %m", operation);

/* Well-known D-Bus error */
return sd_bus_error_set_const(error, SD_BUS_ERROR_INVALID_ARGS,
                              "Invalid argument");
```

**Check**:
- [ ] Error set before returning negative
- [ ] Error message useful for debugging
- [ ] Appropriate error type used

### DBUS-008: Error Handling in Callbacks
```c
static int method_callback(sd_bus_message *m, void *userdata, sd_bus_error *error) {
        int r;

        r = do_something();
        if (r < 0)
                return sd_bus_error_set_errno(error, r);

        return sd_bus_reply_method_return(m, "");
}
```

**Check**:
- [ ] All error paths set sd_bus_error
- [ ] Success path returns reply
- [ ] No silent error swallowing

## Object Patterns

### DBUS-009: Object Vtable Registration
```c
static const sd_bus_vtable manager_vtable[] = {
        SD_BUS_VTABLE_START(0),
        SD_BUS_METHOD("Reload", NULL, NULL, method_reload, 0),
        SD_BUS_PROPERTY("Version", "s", property_get_version, 0, 0),
        SD_BUS_VTABLE_END
};

r = sd_bus_add_object_vtable(bus, &slot, path,
                             interface, manager_vtable, userdata);
```

**Check**:
- [ ] Vtable properly terminated
- [ ] Callbacks handle NULL userdata if possible
- [ ] Slot tracked for cleanup

### DBUS-010: Object Path Lifecycle
**Risk**: Stale object paths

```c
/* Register object */
r = sd_bus_add_object_vtable(bus, &slot, path, ...);

/* Object must be unregistered before underlying data freed */
slot = sd_bus_slot_unref(slot);
/* Now safe to free userdata */
```

**Check**:
- [ ] Object unregistered before data freed
- [ ] Async operations cancelled before cleanup
- [ ] No dangling object paths

## Property Patterns

### DBUS-011: Property Get Implementation
```c
static int property_get_state(sd_bus *bus, const char *path,
                              const char *interface, const char *property,
                              sd_bus_message *reply, void *userdata,
                              sd_bus_error *error) {
        Unit *u = userdata;

        return sd_bus_message_append(reply, "s",
                                     unit_state_to_string(u->state));
}
```

**Check**:
- [ ] Reply type matches property signature
- [ ] Userdata properly typed
- [ ] Thread safety if applicable

### DBUS-012: Property Change Notification
```c
/* Emit PropertiesChanged signal */
r = sd_bus_emit_properties_changed(bus, path, interface,
                                   "PropertyName", NULL);
```

**Check**:
- [ ] Properties changed signal emitted when needed
- [ ] Only changed properties listed
- [ ] Signal not emitted before object registered

## PID1 Specific

### DBUS-013: No Blocking Calls from PID1
**Risk**: Deadlock

PID1 must never block waiting for D-Bus responses from services
it manages.

**Check**:
- [ ] No sd_bus_call() in PID1 (use async)
- [ ] Timeouts set appropriately
- [ ] Callbacks handle timeout

### DBUS-014: Credential Checking
```c
r = sd_bus_query_sender_creds(m, SD_BUS_CREDS_PID|SD_BUS_CREDS_UID, &creds);
if (r < 0)
        return r;

r = sd_bus_creds_get_uid(creds, &uid);
```

**Check**:
- [ ] Credentials queried for privileged operations
- [ ] Appropriate credentials checked (UID, PID, etc.)
- [ ] PolicyKit consulted if needed

## Quick Checks

- [ ] Message reads validated
- [ ] Message data copied if stored
- [ ] Containers entered/exited correctly
- [ ] Callbacks check userdata validity
- [ ] Slots unrefd before userdata freed
- [ ] Errors set on all failure paths
- [ ] No blocking calls from PID1
- [ ] Objects unregistered before data freed
