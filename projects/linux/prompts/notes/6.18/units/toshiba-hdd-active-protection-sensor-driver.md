# TOSHIBA HDD ACTIVE PROTECTION SENSOR DRIVER

The Toshiba HDD Active Protection Sensor (HAPS) driver is an x86 platform ACPI driver (`TOS620A`) that manages the accelerometer-based hard drive protection sensor on Toshiba laptops. It evaluates ACPI methods (`_STA`, `PTLV`, `RSSS`) to query availability, set sensor protection levels, and reset protection state, while exposing `protection_level` and `reset_protection` sysfs attributes to userspace and forwarding ACPI notifications as netlink events.

## Architecture & Data Flow

- `toshiba_haps_add` must initialize `haps->acpi_dev`, `haps->protection_level`, `acpi_dev->driver_data`, and `dev_set_drvdata(&acpi_dev->dev, haps)` before calling `sysfs_create_group`, because sysfs callbacks dereference `dev_get_drvdata(dev)` while PM callbacks dereference `acpi_driver_data(to_acpi_device(device))` without null checks. Broken by: removing either driver-data assignment without unifying all accessors, or registering sysfs attributes before populating driver data. [F9]
- `haps_attributes` must remain terminated by a trailing `NULL` entry and `haps_device_ids` by an empty sentinel entry (`{"", 0}`), because `sysfs_create_group` and ACPI device matching iterate these arrays until a sentinel is reached. [F17]

## Synchronization

- `toshiba_haps_protection_level`, `toshiba_haps_reset_protection`, and `toshiba_haps_available` evaluate ACPI methods via `acpi_execute_simple_method`, `acpi_evaluate_object`, and `acpi_evaluate_integer`, which may sleep and must only be called from sleepable process context without holding any spinlock or RCU read lock. Broken by: holding a `spinlock_t` across these helpers in `protection_level_store` or `toshiba_haps_resume`. [F27]
- In `toshiba_haps_remove`, `sysfs_remove_group(&device->dev.kobj, &haps_attr_group)` must be called before tearing down or freeing `struct toshiba_haps_dev` state or clearing `device->dev` driver data. [F5]

## API Contracts

- `toshiba_haps_available` uses boolean return polarity (`1` when `_STA` succeeds and `hdd_present` is non-zero, `0` on `ACPI_FAILURE` or `!hdd_present`), opposite to `toshiba_haps_protection_level` and `toshiba_haps_reset_protection` which return `0` on success and `-EIO` on `ACPI_FAILURE`. Broken by: checking `toshiba_haps_available(...) < 0` for errors, or returning a negative errno from `toshiba_haps_available` without updating `if (!toshiba_haps_available(...))` in `toshiba_haps_add`. [F18]
- `toshiba_haps_add` must gate device initialization on `toshiba_haps_available(acpi_dev->handle)` before calling `toshiba_haps_protection_level` (`PTLV`) or registering `haps_attr_group`, and `toshiba_haps_available` must treat both `ACPI_FAILURE` from `acpi_evaluate_integer(handle, "_STA", ...)` and a zero `hdd_present` result as expected absence (`return 0` so `toshiba_haps_add` returns `-ENODEV`) without emitting `WARN*` or `BUG*` assertions. [F23, F31]

## Error Handling

- `toshiba_haps_add` must assign the singleton `toshiba_haps` pointer only after all fallible initialization steps (`kzalloc`, `toshiba_haps_protection_level`, and `sysfs_create_group`) succeed, and `toshiba_haps_remove` must clear `toshiba_haps = NULL`, because a non-null `toshiba_haps` causes `toshiba_haps_add` to fail immediately with `-EBUSY`. Broken by: setting `toshiba_haps = haps` before fallible steps without clearing it on the error path. [F10]
- ACPI evaluation helpers `toshiba_haps_reset_protection` and `toshiba_haps_protection_level` must check their `acpi_status` with `ACPI_FAILURE(status)` and translate any ACPI failure into `-EIO` (returning `0` on success) before returning to sysfs, probe, or PM callers. Broken by: returning a raw `acpi_status` (positive non-zero code) instead of `-EIO`. [F7]

## External Contracts

- `protection_level_store` must validate `level` in `0..3` (else `-EINVAL`) and update `haps->protection_level = level` only after `toshiba_haps_protection_level` (`PTLV`) returns `0`, while `toshiba_haps_suspend` must pass `0` to `toshiba_haps_protection_level` without mutating `haps->protection_level` so `toshiba_haps_resume` restores the saved level before `toshiba_haps_reset_protection`; `reset_protection_store` must parse with `kstrtoint` and reject values `!= 1` with `-EINVAL` before calling `toshiba_haps_reset_protection`, and both stores must return `count` on success. [F8, F24, F30]
