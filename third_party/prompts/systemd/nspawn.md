# systemd-nspawn Container Patterns

## When to Load
Load when patch touches:
- `src/nspawn/` directory
- Container mount setup
- Container network configuration
- pivot_root operations

## Key Files
- `src/nspawn/nspawn.c` - Main container logic
- `src/nspawn/nspawn-mount.c` - Mount setup
- `src/nspawn/nspawn-mount.h` - Mount types and flags
- `src/nspawn/nspawn-network.c` - Network namespace setup
- `src/nspawn/nspawn-cgroup.c` - Cgroup setup
- `src/nspawn/nspawn-seccomp.c` - Seccomp filters

## Mount Patterns

### NSPAWN-001: CustomMount Types
```c
typedef enum MountSettingsMask {
        MOUNT_FATAL              = 1 << 0,  /* Fail if mount fails */
        MOUNT_USE_USERNS         = 1 << 1,  /* Use user namespace */
        MOUNT_IN_USERNS          = 1 << 2,  /* Already in user namespace */
        MOUNT_APPLY_APIVFS_RO    = 1 << 3,  /* Apply read-only API VFS */
        MOUNT_APPLY_APIVFS_NETNS = 1 << 4,  /* Apply network namespace API VFS */
        /* ... */
} MountSettingsMask;
```

**Check**:
- [ ] Correct flags for mount context
- [ ] MOUNT_FATAL set appropriately
- [ ] User namespace flags consistent

### NSPAWN-002: Mount Order
Mounts must be applied in specific order:

1. Base filesystem mounts (rootfs)
2. API VFS mounts (/proc, /sys, /dev)
3. Custom bind mounts
4. Overlay mounts
5. Tmpfs mounts

**Check**:
- [ ] Dependencies mounted first
- [ ] No mount-under-mount issues
- [ ] Unmount order is reverse

### NSPAWN-003: pivot_root Handling
Located in `src/nspawn/nspawn-mount.c`:

```c
int setup_pivot_root(const char *directory,
                     const char *pivot_root_new,
                     const char *pivot_root_old)
```

**Requirements**:
- New root must be a mount point
- Old root can be unmounted afterward
- Current directory handled correctly

**Check**:
- [ ] pivot_root target is mount point
- [ ] Old root properly unmounted
- [ ] Error handling for pivot failure

## Network Patterns

### NSPAWN-004: Network Namespace Setup
```c
/* veth pair creation */
r = netlink_add_veth(host_ifname, container_ifname, ...);

/* Move interface to container namespace */
r = netlink_set_link_namespace(ifindex, netns_fd);
```

**Check**:
- [ ] Interface names unique
- [ ] Namespace FD valid when used
- [ ] Cleanup on partial failure

### NSPAWN-005: Network Interface Ownership
Host side of veth stays in host namespace.
Container side moves to container namespace.

**Check**:
- [ ] Only container interface moved
- [ ] Host interface configured after container interface moved
- [ ] Proper cleanup if container fails to start

## User Namespace Patterns

### NSPAWN-006: UID/GID Mapping
```c
/* /proc/PID/uid_map format: container_id host_id count */
/* /proc/PID/gid_map format: container_id host_id count */
```

**Check**:
- [ ] Mappings don't overlap incorrectly
- [ ] Root in container maps to appropriate host UID
- [ ] /etc/subuid and /etc/subgid consulted if needed

### NSPAWN-007: User Namespace + Mount Namespace
When using user namespaces:
- Mount namespace inherits user namespace ownership
- CL_SLAVE flag may be needed for mount copies
- Some mounts require MS_BIND with user namespace

**Check**:
- [ ] Mount flags appropriate for user namespace context
- [ ] Ownership of mount namespace verified
- [ ] Privilege checks account for user namespace

## Seccomp Patterns

### NSPAWN-008: Seccomp Filter Timing
Seccomp filters must be applied LAST, after all setup.

**Check**:
- [ ] All namespace setup complete before seccomp
- [ ] All mounts complete before seccomp
- [ ] No syscalls blocked that setup requires

### NSPAWN-009: Seccomp Compatibility
Some operations need syscalls that might be filtered:
- pivot_root
- mount/umount
- unshare
- clone

**Check**:
- [ ] Syscalls needed for setup are allowed
- [ ] Filters don't break expected container operations

## Resource Cleanup

### NSPAWN-010: Container Teardown
On failure or exit:
1. Kill container processes
2. Unmount filesystems (reverse order)
3. Remove network interfaces
4. Clean up cgroup
5. Release namespace FDs

**Check**:
- [ ] All resources cleaned up on error
- [ ] No leaked mount points
- [ ] No leaked network interfaces
- [ ] No leaked cgroup

### NSPAWN-011: Partial Failure Handling
```c
/* Mount setup with cleanup */
_cleanup_(custom_mount_freep) CustomMount *m = NULL;

m = custom_mount_prepare(...);
if (m < 0)
        return m;  /* Automatic cleanup */

r = mount_custom(m, ...);
if (r < 0)
        return r;  /* m cleaned up */

/* Success - transfer ownership */
TAKE_PTR(m);
```

**Check**:
- [ ] Partial mounts unmounted on failure
- [ ] Resources tracked for cleanup
- [ ] Ownership transferred on success

## Common Pitfalls

### Pitfall 1: Mount Propagation Leak
```c
/* BAD - mounts leak to host */
mount("/some/path", dest, ...);

/* GOOD - isolate first */
mount(NULL, "/", NULL, MS_SLAVE|MS_REC, NULL);
mount("/some/path", dest, ...);
```

### Pitfall 2: Network Interface Leak
```c
/* BAD - interface orphaned on error */
r = create_veth_pair(&host_if, &container_if);
if (r < 0)
        return r;

r = do_something_else();
if (r < 0)
        return r;  /* veth pair leaked! */

/* GOOD - track for cleanup */
_cleanup_(destroy_veth) int host_if = -1;
r = create_veth_pair(&host_if, &container_if);
```

### Pitfall 3: pivot_root Without Mount Point
```c
/* BAD - new_root not a mount point */
pivot_root(new_root, put_old);

/* GOOD - ensure mount point first */
mount(new_root, new_root, NULL, MS_BIND, NULL);
pivot_root(new_root, put_old);
```

## Quick Checks

- [ ] Mount order is correct
- [ ] pivot_root target is mount point
- [ ] Network interfaces cleaned up on failure
- [ ] User namespace flags consistent
- [ ] Seccomp applied last
- [ ] All resources tracked for cleanup
- [ ] Mount propagation isolated
