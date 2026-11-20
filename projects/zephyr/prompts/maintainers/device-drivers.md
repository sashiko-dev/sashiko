+++
title = "Device Drivers"
paths = [
    "drivers/",
    "include/zephyr/drivers/",
]
symbols = [
    "DEVICE_API*",
    "DEVICE_DT_DEFINE",
    "DEVICE_DT_GET*",
    "DEVICE_DT_INST_DEFINE",
    "DEVICE_DT_INST_GET",
    "device_init",
    "device_is_ready",
]
+++

# Device Drivers

Applies to in-tree drivers and the public driver APIs.

## The driver model

Source: doc/kernel/drivers/index.rst.

- Driver API instances are declared with `DEVICE_API()`, so that
  `DEVICE_API_GET()` and `DEVICE_API_IS()` work. Functionality the subsystem API
  cannot expose uses the device-specific extension mechanism in that document.
- `config` holds read-only data set at build time, and `data` holds per-instance
  runtime state in RAM. Flag runtime state kept in `config`, and static variables
  holding per-instance state in a driver that supports multiple instances.
- Per-instance interrupt setup uses a per-instance config function, as the
  document's example shows.

## Getting devices from Devicetree

Source: doc/build/dts/howtos.rst.

- `DEVICE_DT_GET()` fetches a device pointer at build time, but the device may not
  be initialized or may have failed to initialize. Check `device_is_ready()`
  before use. For `_dt_spec` types use the matching helper, such as
  `gpio_is_ready_dt()` or `spi_is_ready_dt()`.
- A driver with multiple instances defines `DT_DRV_COMPAT` without quotes and
  instantiates through `DT_INST_FOREACH_STATUS_OKAY()`. That macro does not add a
  semicolon, so the instantiation macro must end in one.

## Standalone devices

Source: doc/contribute/guidelines.rst (Specialized driver requirements).

- A driver for a standalone device uses the Zephyr bus APIs (SPI, I2C, and so on)
  so that it works on any SoC. An SoC-specific fast path must keep a regular
  path for all other SoCs, and exceptions need Architecture WG approval.

## Blocking and interrupts

Source: doc/kernel/drivers/index.rst.

- A driver should support an interrupt-based implementation rather than polling,
  unless the hardware provides no interrupt.
- Calls through device APIs such as `i2c.h` or `spi.h` are usually synchronous, so
  they should block until the operation completes.

## Return values

Source: the doxygen for each API in `include/zephyr/drivers/`.

- Check the return value of bus transfers. Report a value used after the transfer
  that should have produced it failed.
- Return the errno the API documents, since callers rely on specific codes.
