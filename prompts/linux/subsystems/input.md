# Input Subsystem Details

## Device Lifecycle and Registration

Failing to follow the correct allocation and registration sequence leads to
memory leaks, double-frees, or use-after-free during device hot-unplug or driver
unbind.

*   **Allocation**: Devices must be allocated using `input_allocate_device()` or
    `devm_input_allocate_device()`.
*   **Capabilities**: The core automatically adds `EV_SYN`/`SYN_REPORT`
    capability to all input devices. Manual addition is redundant.
*   **Pre-registration state**: `input_event()` can be called safely after
    allocation but before registration. It updates internal state (like
    `keybit` or the current value of an absolute axis) but will not propagate
    events to handlers. This allows drivers to request IRQs and handle initial
    state synchronization before the device is visible to userspace.
*   **Registration Visibility**: `input_register_device()` is the point where
    the device becomes visible to userspace. All driver private data and
    callbacks MUST be fully initialized (including `input_set_drvdata()`) BEFORE
    calling register. Once registered, callbacks can be invoked immediately by
    the core or via userspace ioctls.
*   **Registration Transition**: Once `input_register_device()` succeeds, the
    input core takes over part of the lifecycle management.
    *   **REPORT as bugs**: Any code that calls `input_free_device()` on a pointer
        that holds a successfully registered device. Note that some drivers use
        a unified failure path where they call `input_unregister_device(dev)`
        and then set `dev = NULL` before a shared `input_free_device(dev)`
        call. This "defensive NULL" pattern is safe and should NOT be reported
        as a bug, as `input_free_device(NULL)` is a no-op.
*   **Lifetime Guarantee**: The input core uses reference counting to manage the
    `input_dev` object. Even after `input_unregister_device()` is called, the
    memory for the device will remain valid until the last userspace reference
    (e.g., via `evdev`) is released.
*   **Unregistration and Handler Disconnection**: When a device is unregistered
    (via `input_unregister_device()`), the input core immediately disconnects all
    handlers (such as `evdev`).
    *   Handlers are notified that the device is "dead" (e.g., `evdev->exist = false`
        is set under mutex).
    *   Once disconnected, no new userspace requests (ioctls, opens, event writes)
        can reach the driver. Any pending synchronous operations are completed
        before the disconnect returns.
    *   Consequently, the lifecycle mismatch (where `input_dev` outlives the
        driver's private data) is **completely safe for all synchronous paths**.
        Drivers do not need defensive checks in callbacks like `open` or `close`
        to guard against unbind.
    *   **CRITICAL EXCEPTION**: This protection **only** applies to synchronous
        calls initiated by handlers. Asynchronous background tasks (such as
        force-feedback timers or driver-local workqueues) are NOT blocked by
        disconnection and must be explicitly stopped (see **Timer Lifetime**
        under *Force-Feedback Memory Management*).

See `input_allocate_device()` and `input_register_device()` in
`drivers/input/input.c`.

## Callback Timing and Serialization

Improper assumption of callback ordering or lack of serialization causes race
conditions during device open/close or concurrent event reporting, leading to
missed events or null pointer dereferences.

*   **Callback Readiness**: Drivers must be fully initialized and ready for
    `input_dev->open()` and `input_dev->close()` as soon as
    `input_register_device()` is called. The core may call `open()` before the
    registration function even returns if a userspace handler is already
    waiting.
*   **Serialization**: The input core serializes `open()` and `close()` using
    `input_dev->mutex`.
    *   If a driver needs to synchronize other methods (e.g., a custom `ioctl`
        or `setkeycode`) with the device's open/closed state, it must
        explicitly acquire `input_dev->mutex`.
*   **Teardown**: `input_unregister_device()` automatically calls the driver's
    `close()` method if the device is currently open.
*   **State Checks**: To check if a device is active (has users and is not
    inhibited), use `input_device_enabled()`. This helper MUST be called while
    holding `input_dev->mutex`.

## Force-Feedback Memory Management

Helpers like `input_ff_create_memless()` have specific ownership semantics for
private data that differ from the rest of the input core.

*   **Ownership Transfer**: `input_ff_create_memless(dev, data, play_effect)`
    takes ownership of the `data` pointer. This data will be automatically
    freed using `kfree()` when the input device is destroyed.
*   **Allocation Requirement**: Because the input core calls `kfree()` on the
    private data, it MUST be allocated using `kmalloc()` (or similar) and MUST
    NOT be managed by `devm`.
*   **Timer Lifetime (Force-Feedback)**: Force-feedback devices (especially
    `ff-memless`) use background timers to schedule haptic effects.
    *   The input core automatically stops these timers synchronously during
        `input_unregister_device()` (via the `ff->stop` callback).
    *   Drivers must still ensure that any local asynchronous work (e.g.,
        custom workqueues used to defer haptic writes) is explicitly cancelled
        in their `close()` or `remove()` callbacks to prevent UAF.
    *   **Context**: While the core guarantees that synchronous handler-initiated
        callbacks are safe after unregistration (see **Unregistration and
        Handler Disconnection**), asynchronous background tasks must be manually
        reclaimed because they are not governed by handler disconnection.
*   **REPORT as bugs**: Any driver that manually frees the data passed to
    `input_ff_create_memless()` or uses `devm_kzalloc()` for it, as this will
    lead to double-frees or manual-free-of-managed-memory.

## Managed Resources (devm) Integration

Mixing `devm` and non-`devm` resources out of order or misusing registration
wrappers leads to use-after-free during device removal because the release
order is violated (e.g., trying to access a freed private structure during
unregistration).

*   **No Registration Wrapper**: There is no `devm_input_register_device()`.
    Input devices allocated with `devm_input_allocate_device()` should be
    registered with the standard `input_register_device()`.
*   **Auto-Unregister**: The input core recognizes when it deals with a managed
    device and automatically sets up an action to call
    `input_unregister_device()` when the provider device is unbound.
*   **Redundant Parent**: When using `devm_input_allocate_device(dev)`, the core
    automatically sets `input_dev->dev.parent = dev`. Manually assigning the
    parent is redundant and should be avoided.
*   **Manual Cleanup of Managed Devices (Safety & Redundancy)**:
    *   **Freeing**: It is perfectly fine to call `input_free_device()` on a
        device allocated via `devm_input_allocate_device()` (e.g., in a failed
        `probe` path before registration). The input core detects this,
        automatically removes the devres tracking synchronously, and frees the
        device, preventing any double-free or leak when the devm cleanup
        eventually triggers.
    *   **Unregistering**: Explicitly calling `input_unregister_device()` on a
        managed device (usually in `remove()`) is usually redundant but is
        **technically safe** and should NOT be reported as a bug. The input core
        detects this, automatically removes the devres unregister action
        synchronously to prevent double-unregistration, and unregisters the
        device immediately.
*   **Analysis Protocol: Eliminating Manual Unregistration**:
    When reviewing a driver that explicitly calls `input_unregister_device()` on a
    managed device (usually in `remove()`), perform a deep analysis to see if it
    can be safely eliminated to simplify the code:
    1.  **Identify Non-Devm Async Resources**: Check if the driver manages any
        non-devm asynchronous resources (e.g., custom workqueues, timers, or
        non-devm IRQs) that are destroyed/freed in the `remove()` callback.
    2.  **Check Dependency**: Determine if these resources can receive new work
        or events triggered by input device callbacks (like `play_effect` or `open`).
        *   If yes, the input device **must** be unregistered *before* these
            resources are destroyed.
        *   Since the `remove()` callback runs *before* devm auto-unregistration
            triggers, a manual `input_unregister_device()` is **required** in
            `remove()` before the resource cleanup. It cannot be eliminated.
    3.  **Check Devm Ordering**: If all resources are devm-managed, verify if
        they are allocated/registered in the correct order (LIFO): the input device
        registration must happen *after* the allocation of any resource it depends
        on, so that the input device is unregistered *before* those resources
        are freed.
    4.  **Recommendation**: If there are no non-devm dependencies and devm ordering
        is correct, the manual `input_unregister_device()` call is truly redundant
        and you should recommend its removal.
*   **Ordering**: Drivers should avoid mixing managed and regular resources. If
    both are used, non-managed resources (like manual IRQ requests) should be
    acquired *after* managed ones and released *manually* before the managed
    cleanup triggers.

## Event Reporting and Synchronization

Failing to synchronize event packets causes inconsistent device state in
userspace or leads to events being "stuck" in the input core's buffers until the
next sync.

*   **Synchronization**: Every logical group of events (e.g., a coordinate pair
    or a set of button changes) must be followed by a call to `input_sync()`.
    Without this, userspace handlers may not receive the updated state.
*   **Report Helpers**: Use the specific reporting helpers like
    `input_report_key()`, `input_report_abs()`, or `input_report_rel()` instead
    of generic `input_event()` when the event type is known.
*   **Redundant Events**: The input core filters out redundant events (reporting
    the same value twice). However, drivers should still avoid unnecessary
    reporting to reduce overhead in the event delivery path.
*   **Locking**: `input_event()` and all reporting helpers acquire
    `input_dev->event_lock` (a spinlock) and disable interrupts locally.
    Drivers must ensure they do not hold any locks that could lead to a
    deadlock when calling these functions.

## Maintainer Style Preferences

The input subsystem maintainer prioritizes explicit error handling, modern
cleanup primitives, and standard kernel coding styles to reduce boilerplate and
"gotcha" bugs.

*   **Comments**: Prefer C-style comments `/* ... */` over C++ style `// ...`.
    Multi-line comments should follow the standard format:
    ```c
    /*
     * Line 1
     * Line 2
     */
    ```
*   **Capabilities**: Suggest using `input_set_capability(dev, type, code)`
    instead of direct bit manipulation (e.g., `__set_bit(code, dev->evbit)`) for
    single capabilities. Direct bit manipulation is acceptable in loops or
    other repetitive constructs.
*   **Cleanup**: Suggest using `guard(mutex)(&input_dev->mutex)` in new or
    significantly refactored code. Also encourage using `__free()` for local
    resource management, such as `u8 *buf __free(kfree) = ...` or
    `const struct firmware *fw __free(firmware) = NULL`.
*   **Error Naming**: Use `error` or `err` for variables that only hold
    negative error codes and 0 for success. Functions using these should
    explicitly `return 0;` in the success path.
*   **Explicit Failure Paths**: Do not use `return action(...);` where `action`
    returns an error code in functions with multiple failure points. Use the
    expanded form:
    ```c
    error = action(...);
    if (error) {
            /* report error if needed */
            return error;
    }

    return 0;
    ```

## Quick Checks

*   **Identity and Initialization**: Ensure `input_dev->name` is assigned.
    Verify that all private structure fields and `input_set_drvdata()` are
    initialized *before* calling `input_register_device()`.
*   **Hierarchy**: Verify `input_dev->dev.parent` is set. If the driver uses
    `devm_input_allocate_device()`, the parent is set automatically and manual
    assignment should be removed as redundant.
*   **Interrupt Timing**: While it is safe to report events via `input_event()`
    before registration, the driver must ensure the hardware is fully ready
    (powered on, clocks enabled, and registers accessible) before the
    interrupt handler is allowed to run.
*   **Mutex Guard**: If the driver calls `input_device_enabled()`, verify that it
    holds `input_dev->mutex`.
*   **Event Sync**: Verify that the event reporting path (ISR or worker thread)
    ends with a call to `input_sync()`.
*   **FF Memory**: If using `input_ff_create_memless()`, verify the private
    data is NOT `devm` allocated and NOT manually freed.
