# HID Subsystem Details

This document provides a knowledge reference for reviewing code in the HID
subsystem, covering both low-level transport drivers and high-level device
drivers.

---

## Low-Level Transport Drivers

Low-level (LL) transport drivers (e.g., USB-HID, I2C-HID, Bluetooth-HIDP)
manage the physical hardware, raw data transport, and device registration.

### Device Lifecycle and Registration

Failing to follow the correct allocation and registration sequence in transport
drivers leads to memory leaks, double-frees, or use-after-free (UAF) during
device hot-unplug or driver unbind.

*   **Allocation**: Transport drivers must allocate the `hid_device` structure
    using `hid_allocate_device()`. There is no managed (`devm_`) version.
*   **Initialization**: Before registration, the transport driver must fully
    initialize the `hid_device` fields, including `name`, `phys`, `uniq`,
    `ll_driver`, `bus`, `vendor`, `product`, `version`, `country`, `dev.parent`,
    and `driver_data`.
*   **Registration**: Register the device with the HID core using
    `hid_add_device()`.
*   **Unregistration and Destruction**: Use `hid_destroy_device()` to
    unregister and free the device. This function must be called if
    `hid_add_device()` fails during initialization, or when the device is
    physically removed or the transport module is unloaded.
*   **No Managed Lifecycle**: The HID core does not provide managed helpers for
    the `hid_device` structure itself. Transport drivers must manually manage
    its lifecycle.
*   **REPORT as bugs**: Transport drivers that fail to call
    `hid_destroy_device()` on probe failure (after allocation) or on device
    removal, or drivers that attempt to use `devm` to manage the `hid_device`
    structure itself.

See `hid_allocate_device()`, `hid_add_device()`, and `hid_destroy_device()` in
`drivers/hid/hid-core.c`.

### Low-Level Driver Callbacks (`struct hid_ll_driver`)

Transport drivers must implement `struct hid_ll_driver` to allow the HID core
and device drivers to communicate with the hardware.

*   **Mandatory Callback**: The `raw_request` callback is **mandatory**.
    `hid_add_device()` will fail with `-EINVAL` if it is missing. This callback
    must perform synchronous transfers and must not block using the core's
    `->wait()` helper.
*   **Descriptor Registration**: The `parse` callback must retrieve the raw
    report descriptor from the device and pass it to the core by calling
    `hid_parse_report()`. If `parse` succeeds but `hid_parse_report()` was not
    called (leaving `hid->dev_rdesc` as `NULL`), `hid_add_device()` will fail
    with `-ENODEV`.
*   **Serialized Open/Close**: The HID core guarantees that the `open` and
    `close` callbacks are serialized and non-nested per device. The core
    protects these calls using `hdev->ll_open_lock` (a mutex) and tracks the
    reference count in `hdev->ll_open_count`. Transport drivers do **not** need
    to implement their own refcounting or serialization for concurrent
    open/close requests.
*   **Asynchronous Output**: The `output_report` callback (used for
    high-throughput data on the interrupt channel) must be asynchronous and
    must **not** trigger synchronous `SET_REPORT` calls.

See `hid_add_device()`, `hid_hw_open()`, and `hid_hw_close()` in
`drivers/hid/hid-core.c`.

### Data Path and Input Reporting

Using the wrong input reporting API in transport drivers can lead to
out-of-bounds reads in the HID core.

*   **Safe Reporting**: Transport drivers should prefer
    `hid_safe_input_report()` over `hid_input_report()`.
    `hid_safe_input_report()` explicitly takes the allocated size of the data
    buffer (`bufsize`) in addition to the useful data size (`size`), allowing
    the core to perform boundary checks.
*   **Synchronous Requests**: Responses to synchronous requests (like
    `raw_request`) must be intercepted by the transport driver and returned
    directly to the caller. They must **NOT** be passed to
    `hid_input_report()` or `hid_safe_input_report()`.

See `hid_input_report()` and `hid_safe_input_report()` in
`drivers/hid/hid-core.c`.

---

## HID Device Drivers

HID device drivers (high-level drivers) bind to `hid_device` structures to
handle specific device behaviors, parse reports, and expose input interfaces.

### Driver Lifecycle (Probe and Remove)

Incorrect error handling in `probe()` or missing cleanup in `remove()` leads to
resource leaks, hardware left in an active state, or use-after-free.

*   **Probe Sequence**: A custom HID driver's `probe()` callback must call
    `hid_parse()` (or `hid_open_report()`) to parse the report descriptor,
    followed by `hid_hw_start()` to start the hardware.
*   **Hardware Stop**: If `hid_hw_start()` succeeds, the driver **MUST** call
    `hid_hw_stop()` if any subsequent step in `probe()` fails, and in the
    `remove()` callback.
*   **Core Cleanup on Probe Failure**: If `probe()` fails *after* `hid_parse()`
    but *before* `hid_hw_start()`, the core automatically calls
    `hid_close_report()`. The driver does NOT need to call it manually.
*   **Devres Integration**: The HID core opens a devres group
    (`devres_open_group`) before calling `probe()`. If `probe()` fails, this
    group is automatically released, freeing all `devm_` allocated resources.
    Drivers can safely use `devm_kzalloc()` for their private data
    (`hid_set_drvdata()`) during `probe()`.
*   **Asynchronous Resources**: Timers, workqueues, or threaded IRQs started
    during `probe()` must be synchronously stopped/cancelled in both the
    `probe()` error path (after they were started) and the `remove()` callback.
*   **USB Transport Guard (Gotcha)**: Many drivers assume the parent device is
    a USB interface and call `to_usb_interface(hdev->dev.parent)`. If the
    driver is bound to a non-USB device (I2C, Bluetooth, BPF), this performs an
    invalid cast and causes a kernel crash.
    *   *Rule*: If a driver expects to work only with USB devices, explicitly
        reject non-USB devices during `probe()` (`if (!hid_is_usb(hdev))`
        return -ENODEV;`). If the driver supports multiple transports, always
        check `hid_is_usb(hdev)` before calling USB-specific APIs or casting
        the parent device.
*   **Devres LIFO Trap & Dummy `remove()` (Gotcha)**: If a driver uses devres
    actions (`devm_add_action_or_reset`) to manage `hid_hw_start()` /
    `hid_hw_stop()`, it **must** provide a dummy/empty `.remove()` callback.
    *   *Why*: If `.remove()` is completely omitted, the core's default
        behavior is to call `hid_hw_stop()` in the unbind phase. This causes a
        double-stop and, worse, stops the HID device *before* devres releases
        other resources (violating the LIFO release order, where e.g., an I2C
        adapter must be unregistered before HID is stopped).
    *   *Rule*: Implement an empty `static void custom_remove(struct hid_device
        *hdev) {}` to override the core's default unbind behavior when managing
        hardware start/stop via devres.

```c
// CORRECT (Standard Probe/Remove Pattern)
static int custom_probe(struct hid_device *hdev, const struct hid_device_id *id)
{
	struct custom_data *data;
	int ret;

	/* 1. Guard against non-USB transports if using USB APIs */
	if (!hid_is_usb(hdev))
		return -ENODEV;

	data = devm_kzalloc(&hdev->dev, sizeof(*data), GFP_KERNEL);
	if (!data)
		return -ENOMEM;
	hid_set_drvdata(hdev, data);

	/* 2. Parse descriptor */
	ret = hid_parse(hdev);
	if (ret)
		return ret; /* Core will call hid_close_report() */

	/* 3. Start hardware */
	ret = hid_hw_start(hdev, HID_CONNECT_DEFAULT);
	if (ret)
		return ret; /* Core will call hid_close_report() */

	timer_setup(&data->timer, custom_timer_handler, 0);

	ret = custom_init_extra(hdev);
	if (ret)
		goto err_stop;

	return 0;

err_stop:
	/* 4. Clean up manually started resources on failure */
	timer_delete_sync(&data->timer);
	hid_hw_stop(hdev);
	return ret;
}

static void custom_remove(struct hid_device *hdev)
{
	struct custom_data *data = hid_get_drvdata(hdev);

	timer_delete_sync(&data->timer);
	hid_hw_stop(hdev);
}
```

*   **REPORT as bugs**: HID drivers that fail to call `hid_hw_stop()` in their
    `probe()` error paths after `hid_hw_start()` succeeded, or drivers that
    fail to stop timers/workqueues in error paths or `remove()`.

See `__hid_device_probe()` and `hid_device_remove()` in
`drivers/hid/hid-core.c`.

### Report Descriptor Fixups (`report_fixup`)

Memory leaks occur if a HID driver dynamically allocates a replacement report
descriptor during `report_fixup` and fails to free it.

*   **In-place Modification**: If the fixup only modifies existing bytes in
    the descriptor, it should modify the passed `rdesc` buffer in-place and
    return it. The core passes a temporary writeable copy of the descriptor to
    `report_fixup()`.
*   **Dynamic Allocation**: If the driver must allocate a new buffer (e.g., to
    expand the descriptor):
    *   The core makes its own copy of the returned buffer anyway in
        `hid_open_report()`, so the driver-allocated buffer is only needed
        during the `report_fixup()` call itself unless the driver needs to keep
        it.
    *   If the driver stores the allocated buffer in its private data (e.g., to
        return it in `report_fixup()`), the driver **MUST** free it in
        `remove()` AND in the `probe()` error path if probe fails.
*   **REPORT as bugs**: HID drivers that allocate a replacement descriptor
    (e.g., via `kmemdup` or `krealloc`) during initialization and fail to free
    it in the `probe()` error path or `remove()` callback.

```c
// WRONG (Leaks desc_ptr if probe fails after uclogic_params_get_desc)
static int uclogic_probe(struct hid_device *hdev, const struct hid_device_id *id)
{
	...
	rc = uclogic_params_get_desc(&drvdata->params,
				     &drvdata->desc_ptr, /* Allocates memory */
				     &drvdata->desc_size);
	if (rc)
		goto failure;

	rc = hid_parse(hdev);
	if (rc)
		goto failure; /* Leaks drvdata->desc_ptr! */

	rc = hid_hw_start(hdev, HID_CONNECT_DEFAULT);
	if (rc)
		goto failure; /* Leaks drvdata->desc_ptr! */

	return 0;
failure:
	if (params_initialized)
		uclogic_params_cleanup(&drvdata->params); /* Does NOT free drvdata->desc_ptr */
	return rc;
}
```

See `hid_open_report()` in `drivers/hid/hid-core.c` for how `report_fixup` is
invoked and how the core copies the result.

### Report and Field Validation (Security)

Malicious devices or fuzzers (e.g., syzkaller) can present report descriptors
that declare fewer fields or usages than the driver expects, leading to NULL
pointer dereferences or out-of-bounds accesses.

*   **Field Count Validation**: Never assume a report has a fixed number of
    fields. Always validate `report->maxfield` before accessing
    `report->field[N]`.
*   **Usage Count Validation**: Always validate `field->maxusage` before
    accessing `field->usage[N]` or `field->value[N]`.
*   **REPORT as bugs**: Drivers that access report fields or usages by index
    without validating the counts first.

```c
// CORRECT (Validating report fields before access)
static void custom_report_callback(struct hid_device *hdev, struct hid_report *report)
{
	/* We expect at least 2 fields in the report */
	if (report->maxfield < 2) {
		hid_err(hdev, "invalid report field count\n");
		return;
	}

	/* Safely access field[0] and field[1] */
	process_field(report->field[0]);
	process_field(report->field[1]);
}
```

### Interaction with the Input Subsystem

The `hid-input` bridge (`drivers/hid/hid-input.c`) translates HID report
usages into Linux input events.

*   **Lifecycle Coordination**: 
    *   *Connection*: If `HID_CONNECT_HIDINPUT` is set in the connect mask
        passed to `hid_hw_start()`, the core calls `hidinput_connect()`. This
        allocates `struct hid_input` structures, maps usages to input
        capabilities, and registers the `input_dev` nodes.
    *   *Disconnection*: `hid_hw_stop()` triggers `hidinput_disconnect()`,
        which unregisters all input devices, synchronously cancels the LED
        worker (`cancel_work_sync(&hid->led_work)`), and frees the `hid_input`
        structures.
*   **Usage Mapping**: Before registration, the core configures input device
    capabilities using driver callbacks:
    *   `input_mapping()`: Invoked for each usage. Return `0` to use generic
        mapping; `> 0` to indicate the driver mapped it (skipping generic
        mapping); `< 0` to ignore the usage entirely (no capability registered,
        events dropped).
    *   `input_mapped()`: Invoked after mapping. Returning `< 0` acts as a
        veto, aborting further generic handling of that usage.
    *   `input_configured()`: Invoked after all usages are mapped but before
        `input_dev` registration. This is the correct place to perform
        input-specific setup (e.g., renaming, setting specific input bits).
*   **Startup Race (Gotcha)**: As soon as `hid_hw_start()` is called with
    `HID_CONNECT_HIDINPUT`, the input device is registered and visible to
    userspace. While within `probe()`, an I/O lock blocks incoming hardware HID
    events until `hid_device_io_start()` runs (automatically called by the core
    after `probe()` returns). However, userspace can immediately open the input
    device or make requests (such as toggling LEDs or ioctls) as soon as it is
    registered. **All driver private data needed by userspace callbacks must be
    complete BEFORE calling `hid_hw_start()`.**
*   **Silent Usage Hiding**: Returning a positive value from `input_mapping()`
    without setting `usage->type`, `usage->code`, or capability bits in
    `input_dev` is the standard pattern used by drivers to silently hide or
    ignore a HID usage from userspace. Do NOT report this as a bug or silent
    failure.
*   **Device Node Splitting**: By default, `hid-generic` sets
    `HID_QUIRK_INPUT_PER_APP`, which logically assembles reports matching one
    device application together into an input node. `HID_QUIRK_MULTI_INPUT` is
    unconditional and splits at any report ID change.
    *   *Rule*: Do NOT blindly suggest adding `HID_QUIRK_MULTI_INPUT` for
        composite devices without analyzing the actual device report
        descriptor, as splitting per report ID can break devices where
        multiple report IDs belong to the same logical application.

### Teardown, Unbinding, and Use-After-Free (UAF) Prevention

The unbinding of a HID driver must be handled with extreme care to prevent UAF
vulnerabilities.

*   **Mandatory `hid_hw_stop()` in `remove()`**: If a HID driver implements a
    custom `remove()` callback, it **must** call `hid_hw_stop()`.
    *   *Why*: The core does not automatically call `hid_hw_stop()` if a custom
        `remove()` is present.
    *   *Consequence*: Omitting `hid_hw_stop()` leaves the input device,
        `hidraw` device, and other interfaces registered and active, while the
        core still proceeds to release the driver's devres group, freeing the
        driver's private data. Subsequent userspace interaction will trigger
        callbacks that access the freed private data, leading to a major UAF.
*   **Asynchronous Resource Reclamation**: Drivers must synchronously stop and
    cancel all custom asynchronous resources (timers, workqueues, etc.) in
    their `remove()` callback **before** returning or freeing any buffers
    they access.
    *   *Why*: The core's devres cleanup automatically frees memory allocated
        via `devm_`, but it cannot automatically cancel custom timers or work
        structures.
    *   *Consequence*: Pending timers or work items may run after the driver is
        unbound and its private data is freed, causing a UAF.
    *   *Cleanup Order*: You must call `hid_hw_stop()` (or unregister the input
        device) **before** cancelling workers and freeing buffers. This ensures
        no new work can be queued by userspace before you begin reclaiming
        resources.
*   **Core Safety Guarantees**:
    *   *Data Path*: Once a driver is unbound, the core sets `hdev->driver =
        NULL`. The input report path (`__hid_input_report()`) checks this and
        immediately drops incoming reports with `-ENODEV`, preventing UAF in
        the read path.
    *   *Interface Refcounting*: Interfaces like `hidraw` use reference
        counting. When a device is unplugged, the device node is destroyed,
        but the underlying `struct hidraw` remains allocated until the last
        userspace client closes its file descriptor. The parent-child relationship
        in the device model ensures the `hid_device` remains allocated as long
        as child device nodes exist.

See `hid_device_remove()` and `__hid_input_report()` in
`drivers/hid/hid-core.c`, and `drop_ref()` in `drivers/hid/hidraw.c`.

---

## Quick Checks

*   **Identity**: Ensure `hid_device` fields (vendor, product, bus) are
    initialized before calling `hid_add_device()`.
*   **Hardware Stop**: Verify `hid_hw_stop()` is called in `remove()` if
    `hid_hw_start()` was called in `probe()`.
*   **Probe Error Paths**: Check every `goto` or `return` in `probe()` after
    `hid_hw_start()` succeeds to ensure `hid_hw_stop()` is called.
*   **Timer/Work Cleanup**: Verify all timers (`timer_shutdown` or
    `timer_delete_sync`) and workqueues (`cancel_work_sync`,
    `destroy_workqueue`) started by the driver are stopped in both `remove()`
    and `probe()` error paths.
*   **Descriptor Leak**: If the driver implements `report_fixup` and returns a
    dynamically allocated buffer stored in private data, verify it is freed in
    both `remove()` and `probe()` error paths.
*   **Safe Input**: Verify transport drivers use `hid_safe_input_report()` when
    feeding data from interrupts.
*   **Input Mapping Returns**: In `input_mapping()`, verify that the return
    value matches the intent (negative for ignore, positive for mapped/hidden,
    zero for generic). Do not flag positive returns without mapped bits as bugs.
*   **Driver Initialization Order**: Verify all driver private data and state
    are fully initialized *before* calling `hid_hw_start()`.
*   **USB Transport Guard**: Verify the driver calls `hid_is_usb(hdev)` before
    calling USB-specific parent device accessors.
*   **Devres Stop Dummy `remove`**: If using devres for hardware stop, verify
    the driver implements a dummy `.remove` callback.
*   **Report Field Validation**: Verify the driver validates `maxfield` and
    `maxusage` before accessing report arrays by index.
