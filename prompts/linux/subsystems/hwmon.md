# Hardware Monitoring Subsystem Details

## Coding Style and Guidelines

- Code must follow guidelines in `Documentation/hwmon/submitting-patches.rst`.
- Enum values in this subsystem are traditionally lowercase. Uppercase is
  permitted, but not mandatory.

## Hardware Monitoring API Scope and Target Directory

HWMON is an API in Linux, not just a physical layout. Hardware monitoring
drivers should reside in the `drivers/hwmon/` directory.

Registering hardware monitoring devices from outside `drivers/hwmon/` violates
layering, increases driver complexity, and bypasses maintainer review.

- If the main functionality of a chip is not hardware monitoring (such as network
  interface controllers, DRM controllers, or platform-specific multi-function
  devices), its hardware monitoring functionality should be implemented as an
  auxiliary device driver, and the hardware monitoring driver should reside in
  `drivers/hwmon/`.
- A hardware monitoring device supporting secondary functionality (such as GPIO
  or LED) should be implemented as a hardware monitoring driver. The secondary
  functionality should be implemented as an auxiliary device, with the driver
  residing in the appropriate subsystem directory.

## Registration and Locking Invariants

- New drivers must use `hwmon_device_register_with_info()` or
  `devm_hwmon_device_register_with_info()` to register with the
  hardware monitoring subsystem. Bare sysfs attribute groups without info
  structures are legacy.
- The hardware monitoring subsystem core does NOT serialize sysfs read/write
  or thermal subsystem operations. `hwmon_attr_show()` and `hwmon_attr_store()`
  invoke driver callbacks (`->read()`, `->write()`, `->read_string()`) without
  holding subsystem locks.
- Drivers are entirely responsible for their own serialization (typically using
  a driver-private mutex or regmap lock) when accessing hardware registers,
  performing multi-byte transactions, or mutating shared driver data.

## Sensor Value Conversions

- Standard unit conventions (see `Documentation/hwmon/sysfs-interface.rst`):
  temperatures in millidegree Celsius, voltages in millivolts, currents in
  milliamps, fan speeds in RPM, power in microwatts.
- Integer scaling arithmetic for sensor conversions must guard against overflow
  when multiplying raw register values prior to division (e.g. use `DIV_ROUND_CLOSEST`
  or `mul_u64_u32_div` for 64-bit intermediate products).
