# Power Management Subsystem Details

## Runtime PM Return Value Contracts

Misunderstanding which return values are possible from each Runtime PM API
leads to incorrect error handling, missed wake-ups, or treating success as
failure. The three base functions have distinct return semantics that callers
must respect.

All Runtime PM wrapper functions route through one of three base
implementations in `drivers/base/power/runtime.c`:

**`__pm_runtime_suspend()` -- suspend path:**
Used by `pm_runtime_suspend()`, `pm_runtime_autosuspend()`,
`pm_runtime_put_sync_suspend()`, `pm_runtime_put_sync_autosuspend()`,
and `__pm_runtime_put_autosuspend()`.
- Returns **1** when the device was already `RPM_SUSPENDED`
- Returns **0** on successful suspension or when an async request was queued
- Returns negative errno on failure (`-EAGAIN`, `-EBUSY`, `-EACCES`, etc.)
- The **1** return is a success case, not an error

**`__pm_runtime_idle()` -- idle path:**
Used by `pm_runtime_idle()`, `pm_request_idle()`, `pm_runtime_put()`,
and `pm_runtime_put_sync()`.
- `pm_runtime_idle()` and `pm_request_idle()` (called without `RPM_GET_PUT`)
  cannot return **1** -- when the device is already `RPM_SUSPENDED`, `rpm_idle()`
  returns `-EAGAIN` because the status is not `RPM_ACTIVE`
- `pm_runtime_put()` and `pm_runtime_put_sync()` (called with `RPM_GET_PUT`)
  can return **1** when the device is already `RPM_SUSPENDED`, via a special
  case in `rpm_idle()` that permits the put path to succeed in that state
- Returns **0** on success (idle check passed, auto-suspend scheduled or
  suspend completed)

**`__pm_runtime_resume()` -- resume path:**
Used by `pm_runtime_resume()`, `pm_request_resume()`, `pm_runtime_get_sync()`,
and `pm_runtime_get()`.
- Returns **1** when the device was already `RPM_ACTIVE`
- Returns **0** on successful resume or when an async request was queued
- Returns negative errno on failure

## Concurrency and Locking

Races between concurrent Runtime PM operations cause confusing error returns
that appear spurious if callers do not expect them. All Runtime PM state
transitions are serialized by a single per-device spinlock:
`dev->power.lock` in `struct dev_pm_info` (defined in `include/linux/pm.h`).

**Usage counter races**: `pm_runtime_put()` may drop the usage counter to
zero, but another thread may call `pm_runtime_get()` before `rpm_idle()` runs.
The subsequent idle check sees a non-zero usage counter and returns `-EAGAIN`.
This is expected behavior, not a bug.

**Child count races**: `rpm_check_suspend_allowed()` returns `-EBUSY` when
`child_count` is non-zero and `ignore_children` is not set. A child device may
resume concurrently, incrementing the parent's child count after a caller's
initial eligibility check.

**State transition races**: When one thread is already suspending or resuming
a device, concurrent attempts return `-EINPROGRESS` (async callers) or
`-EAGAIN` (sync callers in some states), or block until the in-progress
operation completes.

## Hibernation Mode Handling

Skipping device resume in `thaw()` callbacks based solely on
`pm_hibernate_is_recovering()` breaks hybrid sleep mode, leaving devices in an
inconsistent state. The system appears to wake but device functionality fails.

`pm_hibernate_is_recovering()` (defined in `drivers/base/power/main.c`)
returns true when `pm_transition.event == PM_EVENT_RECOVER`, which indicates
the system was creating a hibernation image but the process **failed** and
devices need to be recovered to an active state. It does NOT indicate normal
restoration from a saved image.

Linux hibernation has three `thaw()` scenarios:

- **Image creation succeeded (normal path)**: `pm_transition` is `PMSG_THAW`.
  `pm_hibernate_is_recovering()` returns **false**. The system will proceed
  to power off (or enter suspend for hybrid sleep), so `thaw()` callbacks may
  skip full device resume.
- **Image creation failed (error recovery)**: `pm_transition` is
  `PMSG_RECOVER`. `pm_hibernate_is_recovering()` returns **true**. Devices
  must be fully resumed because the system will continue running.
- **Hybrid sleep wake**: The system created a hibernation image, then entered
  suspend (S3/s0i3) instead of powering off. On wake from suspend, `thaw()`
  is called. `pm_hibernate_is_recovering()` returns **false** (no error
  occurred), but `pm_hibernation_mode_is_suspend()` (defined in
  `kernel/power/hibernate.c`) returns **true**. Devices must be fully resumed
  because the system will continue running.

**Critical pattern for `thaw()` callbacks**: Code that optimizes `thaw()` by
skipping device resume must handle all three cases:

```c
// WRONG: Breaks hybrid sleep -- skips resume when waking from suspend
if (!pm_hibernate_is_recovering())
    return 0;

// CORRECT: Skip resume only when image was created successfully
// and the system is not in hybrid sleep mode
if (!pm_hibernate_is_recovering() && !pm_hibernation_mode_is_suspend())
    return 0;
```

See `amdgpu_pmops_thaw()` in `drivers/gpu/drm/amd/amdgpu/amdgpu_drv.c` for
a reference implementation of this pattern.

## PM Callback Conditional Compilation

Assigning PM callbacks unconditionally in `dev_pm_ops` produces dead code when
`CONFIG_PM_SLEEP=n` or `CONFIG_PM=n`. The callback functions and everything
they reference get linked into the kernel even though they can never execute.
With `__maybe_unused` or section annotations, this may also generate compiler
warnings.

Drivers must use wrapper macros (defined in `include/linux/pm.h`) to set
callback pointers to NULL when PM support is disabled, eliminating dead code:

| Wrapper | Expands to non-NULL when | Use for |
|---------|--------------------------|---------|
| `pm_sleep_ptr()` | `CONFIG_PM_SLEEP=y` | Sleep callbacks (suspend/resume/freeze/thaw/poweroff/restore) |
| `pm_ptr()` | `CONFIG_PM=y` | Runtime PM callbacks and `dev_pm_ops` structure pointers |

```c
// WRONG: Dead code when CONFIG_PM_SLEEP=n
static const struct dev_pm_ops foo_pm_ops = {
    .thaw = foo_thaw,
};
.driver.pm = &foo_pm_ops,

// CORRECT: Wrapped with pm_sleep_ptr/pm_ptr
static const struct dev_pm_ops foo_pm_ops = {
    .thaw = pm_sleep_ptr(foo_thaw),
    .runtime_suspend = foo_runtime_suspend,
};
.driver.pm = pm_ptr(&foo_pm_ops),
```

## Async vs Synchronous Runtime PM Put

Using asynchronous `pm_runtime_put()` where synchronous suspend is required
causes race conditions: the pending async idle/suspend work can be cancelled
by `pm_runtime_disable()` (called during device removal via
`__pm_runtime_barrier()` in `drivers/base/power/runtime.c`), leaving hardware
in an incorrect power state.

`pm_runtime_put()` queues an async idle notification via `__pm_runtime_idle()`
with `RPM_ASYNC`. `pm_runtime_put_sync()` runs the idle check synchronously
in the caller's context.

```c
// WRONG: Async put may be cancelled by pm_runtime_disable() in removal path
pm_runtime_put(&dev->auxdev.dev);
device_del(&dev->auxdev.dev);

// CORRECT: Synchronous put ensures idle/suspend completes before deletion
pm_runtime_put_sync(&dev->auxdev.dev);
device_del(&dev->auxdev.dev);
```

Use `pm_runtime_put_sync()` instead of `pm_runtime_put()` when:
- Device removal follows immediately (`device_del()`, `device_unregister()`,
  `auxiliary_device_delete()`)
- `pm_runtime_disable()` follows immediately
- Hardware ordering constraints require the device to be idle/suspended
  before the next operation

## Runtime PM in IRQ Handlers

Using `pm_runtime_get_noresume()` in interrupt handlers allows hardware
access on suspended devices, causing invalid register reads (typically
returning 0xffffffff from powered-off hardware). For shared IRQs, this
causes spurious interrupt handling when the interrupt belongs to another
device on the same line.

`pm_runtime_get_noresume()` (defined in `include/linux/pm_runtime.h`) only
increments the usage counter via `atomic_inc()`. It does not check
`runtime_status` and does not initiate a resume. The device may be
`RPM_SUSPENDED` when the caller proceeds to access hardware.

`pm_runtime_get_if_active()` (defined in `drivers/base/power/runtime.c`)
atomically checks whether the device is `RPM_ACTIVE` and increments the usage
counter only if so. Returns **1** on success (device active, counter
incremented), **0** if device is not active, or **-EINVAL** if Runtime PM is
disabled.

```c
// CORRECT: Check if device is active before hardware access
int ret = pm_runtime_get_if_active(dev);
if (ret <= 0)
    return IRQ_NONE;  // Device not active, not our interrupt

status = readl(base + STATUS_REG);
if (status == ~0u) {
    pm_runtime_put(dev);
    return IRQ_NONE;
}

// ... handle interrupt ...
pm_runtime_put(dev);
return IRQ_HANDLED;
```

When devices use shared IRQs (`IRQF_SHARED`), the runtime suspend callback
must call `synchronize_irq()` before powering down hardware to ensure no
IRQ handler is executing mid-flight.

## Quick Checks

- **Hybrid sleep in `thaw()`**: If `pm_hibernate_is_recovering()` is used to
  conditionally skip resume, verify `pm_hibernation_mode_is_suspend()` is
  also checked
- **`pm_sleep_ptr()` wrappers**: When PM sleep APIs are added to callbacks,
  verify the `dev_pm_ops` structure uses `pm_sleep_ptr()` for those callbacks
  and `pm_ptr()` for the structure pointer
- **Sync before removal**: `pm_runtime_put()` followed by `device_del()` or
  `device_unregister()` should use `pm_runtime_put_sync()` instead
- **IRQ handler PM access**: IRQ handlers should use
  `pm_runtime_get_if_active()`, not `pm_runtime_get_noresume()`, before
  accessing hardware registers
- **`synchronize_irq()` in suspend**: Drivers using `IRQF_SHARED` must call
  `synchronize_irq()` in their runtime suspend callback before powering down
