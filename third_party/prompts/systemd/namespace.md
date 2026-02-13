# Namespace Subsystem Patterns

## When to Load
Load when patch touches:
- `src/core/namespace.c`, `src/core/namespace.h`
- `src/basic/namespace-util.c`, `src/basic/namespace-util.h`
- `src/nspawn/` mount namespace code
- Any code using `unshare()`, `setns()`, `clone()`, `CLONE_NEW*`

## Key Files
- `src/core/namespace.c` - Main namespace setup (~4000 lines)
- `src/core/namespace.h` - Types: `NamespaceParameters`, `BindMount`, etc.
- `src/basic/namespace-util.c` - Low-level operations
- `src/core/exec-invoke.c` - `apply_mount_namespace()`
- `src/nspawn/nspawn-mount.c` - Container mount handling

## Mount Namespace Patterns

### NS-001: Mount Namespace Creation Sequence
**Risk**: Incomplete isolation, mount leaks

**Correct sequence**:
```c
/* 1. Create new mount namespace */
if (unshare(CLONE_NEWNS) < 0)
        return log_debug_errno(errno, "Failed to unshare mount namespace: %m");

/* 2. Isolate from parent (stop receiving propagation) */
if (mount(NULL, "/", NULL, MS_SLAVE|MS_REC, NULL) < 0)
        return log_debug_errno(errno, "Failed to remount '/' as SLAVE: %m");

/* 3. Apply mount entries (bind mounts, tmpfs, etc.) */
/* ... */

/* 4. Set final propagation mode */
if (mount(NULL, "/", NULL, mount_propagation_flag | MS_REC, NULL) < 0)
        return log_debug_errno(errno, "Failed to set propagation: %m");
```

**Check**:
- [ ] MS_SLAVE|MS_REC applied before other mounts
- [ ] Error handling on each mount operation
- [ ] Propagation flags applied correctly

### NS-002: Namespace FD Handling
**Risk**: FD leaks, use-after-close

**Pattern**:
```c
/* Opening namespace FD */
fd = open("/proc/PID/ns/mnt", O_RDONLY|O_CLOEXEC);

/* Entering namespace */
if (setns(mntns_fd, CLONE_NEWNS) < 0)
        return -errno;
```

**Check**:
- [ ] O_CLOEXEC on all namespace FD opens
- [ ] FD closed on all error paths
- [ ] FD validity checked before setns()

### NS-003: Namespace Permission Checks
**Risk**: Security bypass, privilege escalation

**Check**:
- [ ] `may_mount()` called before mount operations in new namespace
- [ ] `ns_capable()` checked for cross-namespace operations
- [ ] User namespace ownership verified for mount namespace ops

## User Namespace Patterns

### NS-004: User Namespace + Mount Namespace Interaction
**Risk**: Privilege escalation, mount escape

When creating mount namespace in user namespace context:
```c
/* Order matters: user namespace first */
unshare(CLONE_NEWUSER);
/* Then mount namespace (will be owned by new user ns) */
unshare(CLONE_NEWNS);
```

**Check**:
- [ ] Correct ordering of namespace creation
- [ ] CL_SLAVE flag when crossing user namespace boundaries
- [ ] Mount tree locked if user namespace differs

## Mount Propagation

### NS-005: Propagation Flag Semantics
| Flag | Meaning |
|------|---------|
| MS_SHARED | Bidirectional propagation |
| MS_SLAVE | Receive from master, don't send back |
| MS_PRIVATE | No propagation |
| MS_UNBINDABLE | Can't be bind mounted |

**Check**:
- [ ] MS_SLAVE used for container isolation (receives updates, doesn't leak)
- [ ] MS_PRIVATE for complete isolation
- [ ] MS_SHARED only when bidirectional propagation needed

## detach_mount_namespace() Functions

### NS-006: detach_mount_namespace()
Located in `src/basic/namespace-util.c:417`

```c
int detach_mount_namespace(void) {
        if (unshare(CLONE_NEWNS) < 0)
                return log_debug_errno(...);

        if (mount(NULL, "/", NULL, MS_SLAVE|MS_REC, NULL) < 0)
                return log_debug_errno(...);

        if (mount(NULL, "/", NULL, MS_SHARED|MS_REC, NULL) < 0)
                return log_debug_errno(...);

        return 0;
}
```

**Purpose**: Create isolated mount namespace that doesn't leak back

### NS-007: detach_mount_namespace_harder()
Falls back to user namespace if direct approach fails.

**Check**:
- [ ] Graceful fallback when lacking privileges
- [ ] User namespace properly cleaned up on error

## pivot_root Patterns

### NS-008: pivot_root Sequence
Located in `src/nspawn/nspawn-mount.c:1381`

**Check**:
- [ ] New root is a mount point
- [ ] Old root can be unmounted after pivot
- [ ] MS_BIND, MS_MOVE used correctly around pivot

## Namespace Setup Ordering

### NS-009: Correct Namespace Order in Executor
From `src/core/exec-invoke.c`:

1. Network Namespace (if PrivateNetwork=)
2. IPC Namespace (if PrivateIPC=)
3. Cgroup Namespace (if delegated)
4. PID Namespace (if PrivatePIDs=)
5. Mount Namespace (if needed)
6. UTS Namespace (if ProtectHostname=)

**Critical**: PID namespace before mount namespace ensures /proc is mounted
with only processes in PID namespace visible.

## Common Pitfalls

### Pitfall 1: Missing Error Return
```c
/* BAD */
unshare(CLONE_NEWNS);  /* Error ignored! */

/* GOOD */
if (unshare(CLONE_NEWNS) < 0)
        return log_debug_errno(errno, "...");
```

### Pitfall 2: Wrong Propagation After Unshare
```c
/* BAD - MS_PRIVATE before MS_SLAVE */
mount(NULL, "/", NULL, MS_PRIVATE|MS_REC, NULL);

/* GOOD - MS_SLAVE first to stop propagation to parent */
mount(NULL, "/", NULL, MS_SLAVE|MS_REC, NULL);
```

### Pitfall 3: Namespace FD Leak
```c
/* BAD */
mntns_fd = open("/proc/self/ns/mnt", O_RDONLY);
if (error_condition)
        return -ERRNO;  /* FD leaked! */

/* GOOD */
_cleanup_close_ int mntns_fd = -EBADF;
mntns_fd = open("/proc/self/ns/mnt", O_RDONLY|O_CLOEXEC);
```

## Quick Checks

- [ ] All unshare() calls have error handling
- [ ] All namespace FDs have O_CLOEXEC and cleanup
- [ ] MS_SLAVE|MS_REC applied before other mounts
- [ ] Namespace order is correct for the use case
- [ ] Permission checks in place for privileged operations
