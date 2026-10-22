# TOSHIBA HDD ACTIVE PROTECTION SENSOR DRIVER

The Toshiba HDD Active Protection Sensor (HAPS) driver is an x86 platform ACPI driver (`TOS620A`) that manages the accelerometer-based hard drive protection sensor on Toshiba laptops. It evaluates ACPI methods (`_STA`, `PTLV`, `RSSS`) to query availability, set sensor protection levels, and reset protection state, while exposing `protection_level` and `reset_protection` sysfs attributes to userspace and forwarding ACPI notifications as netlink events.

## Architecture & Data Flow
- `toshiba_haps_add` must initialize `haps->acpi_dev`, `haps->protection_level`, `acpi_dev->driver_data`, and `dev_set_drvdata(&acpi_dev->dev, haps)` before calling `sysfs_create_group`, because sysfs callbacks (`protection_level_show`, etc.) dereference `dev_get_drvdata(dev)` while PM callbacks (`toshiba_haps_suspend`, etc.) dereference `acpi_driver_data(to_acpi_device(device))` without null checks. Broken by: removing either driver-data assignment without unifying all accessors, or registering sysfs attributes before populating driver data. [F9]
- `haps_attributes` (used by `haps_attr_group`) must remain terminated by a trailing `NULL` entry and `haps_device_ids` by an empty sentinel entry (`{"", 0}`), as `sysfs_create_group` and ACPI device matching iterate until a sentinel. [F17]

## Synchronization
- `toshiba_haps_protection_level`, `toshiba_haps_reset_protection`, and `toshiba_haps_available` evaluate ACPI methods (`acpi_execute_simple_method`, `acpi_evaluate_object`, `acpi_evaluate_integer`) which may sleep, and must only be called from sleepable process context without holding any spinlock or RCU read lock. Broken by: holding a `spinlock_t` across these helpers in `protection_level_store` or `toshiba_haps_resume`. [F27]

## API Contracts
- `toshiba_haps_available` uses boolean return polarity (`1` when `_STA` succeeds and `hdd_present` is non-zero, `0` on `ACPI_FAILURE` or `!hdd_present`), opposite to `toshiba_haps_protection_level` and `toshiba_haps_reset_protection` which return `0` on success and `-EIO` on `ACPI_FAILURE`. Broken by: checking `toshiba_haps_available(...) < 0`, or returning a negative errno from `toshiba_haps_available` without updating `if (!toshiba_haps_available(...))` in `toshiba_haps_add`. [F18]

## Error Handling
- `toshiba_haps_reset_protection` and `toshiba_haps_protection_level` must check their `acpi_status` with `ACPI_FAILURE(status)` and translate any failure into `-EIO` (returning `0` on success) before returning to sysfs, probe, or PM callers. Broken by: returning a raw positive non-zero `acpi_status`. [F7]

## External Contracts
- `protection_level_store` must validate `level` is in `0..3` (`-EINVAL` otherwise) and set `haps->protection_level = level` only after `toshiba_haps_protection_level` returns `0`, and `toshiba_haps_suspend` must pass `0` to `toshiba_haps_protection_level` without mutating `haps->protection_level` (never inside the helper) so `toshiba_haps_resume` restores the active level before `toshiba_haps_reset_protection`; `reset_protection_store` (`kstrtoint`) must reject values `!= 1` with `-EINVAL` before `toshiba_haps_reset_protection`, and both stores must return `count` on success. [F8, F24, F30]
