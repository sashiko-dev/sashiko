# Service Manager (PID1) Subsystem Patterns

## When to Load
Load when patch touches:
- `src/core/` directory
- Unit types, Manager, Job, Transaction code
- Service execution, ExecContext, ExecRuntime

## Key Files
- `src/core/manager.c` - Main manager loop and state
- `src/core/unit.c` - Unit lifecycle
- `src/core/service.c` - Service unit implementation
- `src/core/execute.c` - Execution context
- `src/core/exec-invoke.c` - Process spawning and sandboxing
- `src/core/load-fragment.c` - Unit file parsing
- `src/core/dbus-*.c` - D-Bus interfaces

## Critical PID1 Rules

### CORE-001: No Threading in PID1
**Risk**: Deadlock, undefined behavior

**Reason**: Cannot mix threads with `clone()`/`clone3()` due to malloc lock.
`fork()` synchronizes malloc locks, but `clone()` does not.

**Check**:
- [ ] No pthread_ calls in src/core/
- [ ] No thread_local in PID1 context (except TLS caches)
- [ ] Use forking processes instead of threads

### CORE-002: No NSS Calls from PID1
**Risk**: Deadlock

**Reason**: NSS lookups (user/host names) may trigger service starts,
creating circular dependency.

**Check**:
- [ ] No getpwnam(), getgrnam(), gethostbyname() in PID1
- [ ] Use numeric UIDs/GIDs or pre-resolved values
- [ ] No synchronous D-Bus calls to services from PID1

### CORE-003: No Synchronous Service Calls
**Risk**: Deadlock

**Check**:
- [ ] PID1 never waits synchronously for service responses
- [ ] All service communication is async

## Unit Lifecycle

### CORE-004: Unit State Machine
States: `UNIT_STUB` → `UNIT_LOADED` → active states → `UNIT_FAILED`/`UNIT_INACTIVE`

**Check**:
- [ ] State transitions are valid
- [ ] No skipping required states
- [ ] Cleanup happens in correct states

### CORE-005: Unit Reference Counting
```c
Unit *u;
u = unit_ref(existing_unit);  /* Increment reference */
/* ... use u ... */
unit_unref(u);  /* Decrement reference */
```

**Check**:
- [ ] Every unit_ref() has matching unit_unref()
- [ ] No use of unit after unref
- [ ] References held during async operations

### CORE-006: Job Lifecycle
Jobs represent pending state changes.

**Check**:
- [ ] Jobs properly linked/unlinked from unit
- [ ] Job completion triggers correct callbacks
- [ ] No job leaks on error paths

## Execution Context

### CORE-007: ExecContext Serialization
PID1 → memfd → systemd-executor → exec

```c
/* In PID1: serialize context */
exec_serialize(context, ...);

/* In executor: deserialize */
exec_deserialize(context, ...);
```

**Check**:
- [ ] All fields serialized that executor needs
- [ ] Deserialization handles missing/malformed data
- [ ] No pointers serialized (only values)

### CORE-008: Sandbox Application Order
In systemd-executor, sandboxing must be applied in correct order:

1. User/Group switching
2. Namespace setup
3. Seccomp filters (last, as they restrict syscalls)

**Check**:
- [ ] Seccomp applied after all other setup
- [ ] Namespace setup before mounts
- [ ] Capabilities dropped at right time

## D-Bus Integration

### CORE-009: D-Bus Object Lifecycle
```c
/* Object registration */
r = bus_unit_implement(u);

/* Object must outlive D-Bus references */
/* Unregister before freeing */
bus_unit_unimplement(u);
```

**Check**:
- [ ] Objects registered before use
- [ ] Objects unregistered before free
- [ ] Async callbacks check object validity

### CORE-010: D-Bus Message Handling
**Check**:
- [ ] All message reads validated
- [ ] Partial reads handled correctly
- [ ] Errors returned via D-Bus error replies

## Memory Safety in PID1

### CORE-011: Hash Table Safety
```c
HASHMAP_FOREACH(u, m->units) {
        /* Don't modify hashmap while iterating! */
        if (should_remove(u))
                /* Mark for later removal */
}
```

**Check**:
- [ ] No hashmap_remove() during HASHMAP_FOREACH
- [ ] Use hashmap_foreach_remove() if removing
- [ ] Iterator invalidation considered

### CORE-012: Event Source Cleanup
```c
_cleanup_(sd_event_source_unrefp) sd_event_source *s = NULL;
r = sd_event_add_io(e, &s, fd, EPOLLIN, callback, userdata);
```

**Check**:
- [ ] Event sources have cleanup attributes
- [ ] Sources disabled/unrefd before associated data freed
- [ ] Callbacks check for destruction in progress

## Configuration Parsing

### CORE-013: Unit File Setting Implementation
Three interfaces must be implemented for new settings:

1. `src/core/load-fragment.c` - INI file parsing
2. `src/core/dbus-*.c` - D-Bus property
3. `src/shared/bus-unit-util.c` - systemctl interface

**Check**:
- [ ] All three interfaces implemented consistently
- [ ] Fuzzer corpus updated (`test/fuzz/fuzz-unit-file/`)
- [ ] Man page documentation added

### CORE-014: Configuration Validation
**Check**:
- [ ] Invalid values rejected with clear errors
- [ ] Defaults documented and sensible
- [ ] Boundary conditions tested

## Error Handling

### CORE-015: Boot Failure Resilience
PID1 must not crash during boot.

**Check**:
- [ ] OOM handled gracefully (emergency mode)
- [ ] Missing files don't crash
- [ ] Invalid config doesn't crash

### CORE-016: Reload Safety
daemon-reload must be safe and reversible.

**Check**:
- [ ] Failed reload doesn't corrupt state
- [ ] Resources properly cleaned up on reload
- [ ] Running services not affected by reload failures

## Quick Checks

- [ ] No threading in PID1 code
- [ ] No NSS calls from PID1
- [ ] Unit references balanced
- [ ] Event sources cleaned up
- [ ] D-Bus objects lifecycle correct
- [ ] Hashmap iteration safe
- [ ] Boot can survive failures
