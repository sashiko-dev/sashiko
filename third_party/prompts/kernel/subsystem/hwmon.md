# Hardware Monitoring Subsystem Details

## Coding style

- Code must follow guildelines in `Documentation/hwmon/submitting-patches.rst`.

- enum values in this subsystem are traditionally lowercase.
  Uppercase is permitted, but not mandatory.

## Arithmetic

- Check for overflows and underflows in arithmetc calculations

- Check for field overflows in bit field operations

## Hardware Monitoring API Scope & Target Directory

HWMON is an API in Linux, not just a physical layout. Hardware Monitoring
drivers should reside in the `drivers/hwmon/` directory.

Registering hardware monitoring devices from outside `drivers/hwmon/` violates
layering, increases driver complexity, and bypasses maintainer review.

- If the main functionality of a chip is not hardware monitoring (such as network
  interface controllers, drm controllers, or a platform specific multi-function
  devices), its hardware monitoring functionality should be implemented as
  auxiliary device driver, and the hardware monitoring driver should reside in
  `drivers/hwmon/`.
- A hardware monitoring device supporting secondary functionality (such as GPIO
  or LED) should be implemented as hardware monitoring driver. The secondary
  functionality should be implemented as auxiliary device, with the driver
  residing in the appropriate subsystem directory.

## API

- New drivers must use `hwmon_device_register_with_info()` or
  `devm_hwmon_device_register_with_info()` to register with the
  hardware monitoring subsystem.

- The hardware monitoring subsystem core serializes thermal subsystem and
  sysfs operations for attributes registered with the `info` parameter of
  `hwmon_device_register_with_info()` and
  `devm_hwmon_device_register_with_info()`.
  Drivers must implement locking required for interrupt handling and for
  attributes registered by any other means. Drivers should use `hwmon_lock()`
  and `hwmon_unlock()` for this purpose.
