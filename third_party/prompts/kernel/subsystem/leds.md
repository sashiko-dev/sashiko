# LED Subsystem Details

## Commit Message Prefix and Naming Conventions

Using non-standard commit message prefixes or naming schemes hinders Git history categorization and embeds implementation details into driver names, creating rigid and non-reusable drivers.

- **Commit Message Capitalization**: Always capitalize the description after the subsystem prefix for MFD, LED, and Backlight subsystems.
  - Prefix format: `leds: <Driver>: <Capitalized description>` (e.g. `leds: qwerty: Add support for Qwerty LED`).
- **Naming Conventions**: Choose clear data structure names. For a driver's private data structure, avoid generic terms like `info` or `priv`.
  - Private data structure name: Prefer using the device name (e.g. `struct qwerty_led`) and `ddata` for the variable instance, rather than generic names like `info` or `priv`.

## LED Registration & Managed Resources

Failing to use managed registration APIs (`devm_*`) in probe functions or incorrect lifecycle cleanup in `.remove()` callbacks introduces resource leaks and memory safety hazards.

- **Prefer Managed APIs**: Use `devm_led_classdev_register()` or `devm_led_classdev_register_ext()` instead of unmanaged `led_classdev_register()`.
- **Prevent Use-After-Free**: Unregistering an unmanaged LED *after* freeing private driver data in `.remove()` or failing to unregister it entirely causes a use-after-free or a kernel crash if user-space sysfs attributes are accessed after device removal.
- **Probe Cleanup**: In the `.probe()` error path, ensure that all manually registered LED classdevs are cleaned up if a subsequent step fails.

```c
// WRONG: Manual lifecycle management with bad removal ordering
static int my_led_probe(struct platform_device *pdev) {
    struct my_led_data *ddata = devm_kzalloc(&pdev->dev, sizeof(*ddata), GFP_KERNEL);
    ...
    // Unmanaged registration
    ret = led_classdev_register(&pdev->dev, &ddata->cdev);
    if (ret)
        return ret;

    platform_set_drvdata(pdev, ddata);
    return 0;
}

static void my_led_remove(struct platform_device *pdev) {
    struct my_led_data *ddata = platform_get_drvdata(pdev);

    kfree(ddata); // WRONG: Freeing private data BEFORE unregistering LED cdev
    led_classdev_unregister(&ddata->cdev);
}

// CORRECT: Managed registration automatically handles lifecycle
static int my_led_probe(struct platform_device *pdev) {
    struct my_led_data *ddata = devm_kzalloc(&pdev->dev, sizeof(*ddata), GFP_KERNEL);
    ...
    // Managed registration handles both error paths and driver removal safely
    ret = devm_led_classdev_register(&pdev->dev, &ddata->cdev);
    if (ret)
        return ret;

    return 0;
}
```

## Device Tree Property Selection (Color/Function vs. Label)

Using the deprecated `label` property in modern LED Device Tree bindings violates standard naming guidelines and prevents user-space from correctly identifying the LED's hardware role.

- **Prefer Color and Function**: Use `color` and `function` DT properties to define the LED instead of `label`.
- **System Layout Consistency**: Modern user-space LED managers rely on standard sysfs directories named `<color>:<function>` (e.g. `/sys/class/leds/green:status`). The `label` property is deprecated and should only be used in legacy drivers or for backwards compatibility.

## Trigger Registration & Teardown Symmetry

Registering custom LED triggers without symmetric teardown leaves dangling references in the global triggers list, leading to memory corruption or crashes when another device attempts to access the triggers.

- **Symmetric Cleanup**: Ensure that any custom triggers registered by the driver via `led_trigger_register()` are symmetrically unregistered using `led_trigger_unregister()` on exit or probe failure.
- **Prefer Managed Triggers**: Use `devm_led_trigger_register()` where possible to automate the cleanup lifecycle and prevent leak bugs.

## Quick Checks

- **Success Logging**: Verify that the driver does not print success log messages (such as `"LED registered successfully"` or `"Probe success"`). Only log errors or warnings.
- **Deferred Probing**: Always use `dev_err_probe()` to report probe failures (such as missing regulators or gpios) to correctly handle `-EPROBE_DEFER` and clean up the kernel log.
