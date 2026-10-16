+++
title = "hwmon"
paths = ["drivers/hwmon/"]
symbols = ["hwmon_*", "asus-ec-sensors", "ec_board_info"]
+++
# hwmon: maintainer notes

Maintainer conventions and policies for Linux hardware monitoring (hwmon) drivers and patches.

- Patches must follow the guidelines in Documentation/hwmon/submitting-patches.rst.
- Enum values in hwmon are traditionally lowercase; uppercase is allowed but not required, so do not flag either.
- New hardware monitoring drivers belong in drivers/hwmon/; a new driver that registers hwmon devices from outside drivers/hwmon/ is rejected because it breaks layering, adds complexity and bypasses hwmon maintainer review.
- Existing drivers outside drivers/hwmon/ that register hwmon devices (e.g. under drivers/net/ or drivers/platform/) are not a bug; do not flag them.
- If a chip's main function is not hardware monitoring (NICs, DRM controllers, platform multi-function devices), its hwmon part should be an auxiliary device driver living in drivers/hwmon/.
- If a hardware monitoring chip has secondary functions (GPIO, LED, ...), it should be a hwmon driver, with the secondary functions implemented as auxiliary devices whose drivers live in the appropriate subsystem directory.
- New drivers must register using hwmon_device_register_with_info() or devm_hwmon_device_register_with_info(); older registration interfaces are not accepted for new code.
- When a driver needs its own locking, maintainers prefer hwmon_lock() and hwmon_unlock() over driver-private locks.
