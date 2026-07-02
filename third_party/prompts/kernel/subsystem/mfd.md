# Multi-Function Devices (MFD) Subsystem Details

## Commit Message Prefix and Naming Conventions

Using non-standard commit message prefixes or naming schemes hinders Git history categorization and embeds implementation details into driver names, creating rigid and non-reusable drivers.

- **Commit Message Capitalization**: Always capitalize the description after the subsystem prefix for MFD, LED, and Backlight subsystems.
  - Prefix format: `mfd: <Driver>: <Capitalized description>` (e.g. `mfd: max77650: Add support for MAX77650`).
- **Naming Conventions**: Do not hard-code implementation details in driver, struct, or device names. Avoid including the string "mfd" or the driver's own filename in names.
  - Private data structure name: Prefer using the device name (e.g. `struct max77650`) and `ddata` for the variable instance, rather than generic names like `info` or `priv`.

## MFD API Scope & Target Directory

MFD is an API in Linux, not just a physical layout. Core drivers registering multiple children must live in `drivers/mfd/` and only they should call `mfd_add_devices()`.

Placing MFD logic outside `drivers/mfd/` or using it for single-function devices violates layering, increases driver complexity, and bypasses maintainer review.

- Do not use the MFD API for simple devices with a single function. Use it only for devices registering multiple children in different subsystems via the MFD API or `of_platform_populate()`.
- Core MFD drivers must be located in `drivers/mfd/`.
- Call `mfd_add_devices()` or `devm_mfd_add_devices()` only from within `drivers/mfd/`.
- For simple MFDs, consider if standard DT compatible properties like `"simple-mfd"` or `"simple-pm-bus"` can be used instead of writing a custom driver.

## Parent-Child Data Sharing & Bespoke Accessors

The core MFD driver should only handle core resources (IRQs, regmap). Child drivers must directly access parent data using standard APIs (e.g. `dev_get_drvdata(pdev->dev.parent)`) instead of custom parent-child accessors.

Custom accessors and parent-initialized private resources introduce tight coupling, make refactoring fragile, and lead to silent use-after-free bugs if lifetime mismatches occur.

- Child drivers must retrieve parent driver data using standard APIs like `platform_get_drvdata()` or `dev_get_drvdata(pdev->dev.parent)`.
- Avoid writing bespoke accessors or helper functions in the parent to pass state to child devices.
- Initialize private resources (like `regmap` or clock handles) directly in the child driver that consumes them, rather than in the parent, unless the resource is genuinely shared by multiple child devices.

```c
// WRONG: Custom accessor and intermediate structure laying
struct my_child_priv {
    struct my_parent *parent;
};
...
struct my_parent *parent = my_parent_get_context(pdev->dev.parent);

// CORRECT: Direct standard access to parent's drvdata
struct my_parent_data *ddata = dev_get_drvdata(pdev->dev.parent);
```

## Child Platform Data & Match Data

Cell arrays (`mfd_cell` array) must be `static const`. Do not pass dynamic platform data via the `.data` field of device match tables (e.g. `of_device_id`).

Passing complex pointers through match data tables causes memory safety hazards and leads to initialization ordering races.

- **REPORT as bugs**: Platform data for child devices (e.g., `mfd_cell` arrays) passed via the `.data` field of `of_device_id`, `spi_device_id`, or similar match tables.
- Define `mfd_cell` arrays as `static const`.
- To pass device-variant information, store an `enum` or integer ID in the match table's `.data` field, and use a `switch` statement in the C probe code to select the correct `static const mfd_cell` array.
- For `mfd_cells`, do not create local copies for dynamic amendments; always use static references.

```c
// WRONG: Passing cell array pointer directly via match table
static const struct of_device_id my_mfd_dt_match[] = {
    { .compatible = "vendor,device-a", .data = &device_a_cells },
    { .compatible = "vendor,device-b", .data = &device_b_cells },
    { }
};

// CORRECT: Match using ID enum, select cell array in C code
enum my_device_type { TYPE_A, TYPE_B };

static const struct of_device_id my_mfd_dt_match[] = {
    { .compatible = "vendor,device-a", .data = (void *)TYPE_A },
    { .compatible = "vendor,device-b", .data = (void *)TYPE_B },
    { }
};
...
// Inside probe:
enum my_device_type type = (uintptr_t)device_get_match_data(dev);
switch (type) {
case TYPE_A:
    cells = device_a_cells;
    n_cells = ARRAY_SIZE(device_a_cells);
    break;
case TYPE_B:
    cells = device_b_cells;
    n_cells = ARRAY_SIZE(device_b_cells);
    break;
}
```

## Inter-Driver & Driver-Level Callbacks

Avoid inter-driver or driver-level callbacks between sibling child devices.

Direct dependencies between sibling drivers create initialization circular dependencies and lead to deadlocks or boot hangs.

- Sibling child drivers (e.g., RTC and Regulator drivers under the same MFD parent) must not make direct function calls to each other.
- Do not expose driver-level callbacks that bypass standard kernel subsystem APIs.

## Subdevice Auto-Indexing vs platform_device->id

Use `PLATFORM_DEVID_AUTO` for automatic cell indexing. If a specific device needs numbering starting from 0, use the `platform_device->id` field instead of the `mfd_cell` ID.

Using hard-coded IDs or mapping cell IDs to instance numbers leads to device naming collisions in sysfs and driver load failures.

- Prefer `PLATFORM_DEVID_AUTO` for automatic cell indexing when defining cells.
- Use `platform_device->id` if numbering is explicitly needed for a subdevice (e.g., serial or tty lines), not the `mfd_cell` ID.

## Quick Checks

- **Unwinding on probe failure**: In an MFD probe, if a child device fails to register, the entire probe must fail and unwind previously registered children.
  - If using managed `devm_mfd_add_devices()`, ensure the return value is propagated directly.
  - If using manual `mfd_add_devices()`, ensure the error path calls `mfd_remove_devices()`.
- **Header guidelines**: Do not include driver-specific header files in the global `include/linux/mfd/` directory if they are only used by the parent and its immediate children. Keep them local to `drivers/mfd/`.
