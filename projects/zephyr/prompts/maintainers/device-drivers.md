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
- Keep standard specification register or descriptor definitions in shared
  subsystem headers and keep protocol-level retry loops in the subsystem stack
  rather than duplicating them inside one vendor driver.

## Blocking, timeouts, and interrupts

Source: doc/kernel/drivers/index.rst; include/zephyr/sys/clock.h;
include/zephyr/sys/util.h.

- A driver should support an interrupt-based implementation rather than polling,
  unless the hardware provides no interrupt.
- Calls through device APIs such as `i2c.h` or `spi.h` are usually synchronous, so
  they should block until the operation completes.
- Compute a single deadline at API entry using `k_timepoint_t`
  (`sys_timepoint_calc()` and `sys_timepoint_timeout()`) rather than restarting
  fresh per-step timeouts across multi-phase operations or retries, and prefer
  `WAIT_FOR()` over ad-hoc busy-wait loops.

## Hardware bounds, state, and return values

Source: the doxygen for each API in `include/zephyr/drivers/`.

- Check the return value of bus transfers. Report a value used after the transfer
  that should have produced it failed.
- Return the errno the API documents (for example `-ETIMEDOUT` versus
  `-EAGAIN`), since callers rely on specific codes.
- Do not permanently latch a device into a failed state after a recoverable
  configuration, negotiation, or tuning error unless hardware state is genuinely
  unrecoverable.
- When bounds-checking MMIO register offsets against a mapped window size,
  include the register access width without integer overflow (for example
  `size >= sizeof(uint32_t) && offset <= size - sizeof(uint32_t)`) and apply the
  check on every active access path, not only initialization.
- Check power-of-two assumptions on MSI vector counts and ring sizes (including
  `UINT32_MAX` wrap on non-power-of-two indices), and watch for zero-initialized
  table slots or `0` return sentinels colliding with valid ID `0` or empty
  success.
