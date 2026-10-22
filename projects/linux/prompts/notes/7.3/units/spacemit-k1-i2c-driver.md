# SPACEMIT K1 I2C DRIVER

This component is the Linux kernel I2C bus controller driver for the SpacemiT K1 SoC (`drivers/i2c/busses/i2c-k1.c`). It binds to `spacemit,k1-i2c` platform devices via Device Tree, manages MMIO registers and clocks (`func` and `bus`), and executes interrupt-driven standard-mode (100 kHz) and fast-mode (400 kHz) I2C message transfers registered with the core I2C subsystem.

## Architecture & Data Flow

- `spacemit_i2c_xfer()` must call `spacemit_i2c_init()` and `spacemit_i2c_enable()` on every transfer, and `spacemit_i2c_init()` must set `SPACEMIT_RCR_SDA_GLITCH_NOFIX` in `SPACEMIT_IRCR` so the hardware SDA glitch filter does not delay repeated-start signals. `spacemit_i2c_err_check()`, `spacemit_i2c_disable()`, `spacemit_i2c_reset()`, and `spacemit_i2c_conditionally_reset_bus()` mask interrupts, clear `SPACEMIT_CR_IUE`, or overwrite `SPACEMIT_ICR` and `SPACEMIT_IRCR` without preserving `SPACEMIT_RCR_SDA_GLITCH_NOFIX`. Broken by: moving `spacemit_i2c_init()` to `spacemit_i2c_probe()`. [F17, F43]
- In `spacemit_i2c_conditionally_reset_bus()`, the early return before resetting the controller must test that both `SPACEMIT_BMR_SDA` and `SPACEMIT_BMR_SCL` are simultaneously set in `SPACEMIT_IBMR`, so that bus recovery runs if either line is held low. Broken by: replacing `(status & SPACEMIT_BMR_SDA) && (status & SPACEMIT_BMR_SCL)` with `status & (SPACEMIT_BMR_SDA | SPACEMIT_BMR_SCL)`. [F28]
- `spacemit_i2c_probe()` must clamp `i2c->clock_freq` from the `clock-frequency` property into the inclusive range `[SPACEMIT_I2C_MAX_STANDARD_MODE_FREQ, SPACEMIT_I2C_MAX_FAST_MODE_FREQ]` (defaulting `0` to `SPACEMIT_I2C_MAX_FAST_MODE_FREQ`), guaranteeing a non-zero divisor in `spacemit_i2c_calc_timeout()`. [F23]
- `spacemit_i2c_probe()` must store a `struct spacemit_i2c_dev *` in `i2c_set_adapdata(&i2c->adapt, i2c)` before calling `i2c_add_numbered_adapter(&i2c->adapt)` and in `platform_set_drvdata(pdev, i2c)`, matching the `void *` downcasts in `spacemit_i2c_xfer()` (`i2c_get_adapdata()`) and `spacemit_i2c_remove()` (`platform_get_drvdata()`). [F37]
- `spacemit_i2c_of_match` must remain terminated by a zeroed `{ /* sentinel */ }` entry. [F26]

## API Contracts

- Clearing interrupt status bits in `SPACEMIT_ISR` must go through `spacemit_i2c_clear_int_status()`, which masks the value with `SPACEMIT_I2C_INT_STATUS_MASK` so that non-interrupt status bits (`SPACEMIT_SR_ACKNAK`, `SPACEMIT_SR_UB`, `SPACEMIT_SR_IBB`, `SPACEMIT_SR_EBB`) are not written to `SPACEMIT_ISR`. Broken by: writing raw `status` or `~0U` directly to `i2c->base + SPACEMIT_ISR`. [F16]
- `spacemit_i2c_xfer()` must assign `i2c->msgs = msgs` and `i2c->msg_num = num` (where the I2C core guarantees `num >= 1`) before calling `spacemit_i2c_calc_timeout()`, because `spacemit_i2c_calc_timeout()` divides by `i2c->msg_num` and `spacemit_i2c_is_last_msg()` evaluates `i2c->msg_num - 1` on `u32 msg_num`. [F35]
- `spacemit_i2c_calc_timeout()` must store `i2c->adapt.timeout` in jiffies (via `usecs_to_jiffies()`), because both `wait_for_completion_timeout()` in `spacemit_i2c_xfer_msg()` and `__i2c_transfer()` in the I2C core consume `adapt.timeout` as a jiffies duration. [F52]
- `spacemit_i2c_func()` must exclude `I2C_FUNC_SMBUS_QUICK` from `I2C_FUNC_SMBUS_EMUL` unless zero-length message STOP generation (`SPACEMIT_CR_STOP` for `msg->len == 0`) is supported in `spacemit_i2c_start()` and `spacemit_i2c_irq_handler()`. [F51]

## Error Handling

- In `spacemit_i2c_xfer()`, the call to `spacemit_i2c_xfer_msg()` and its `ret < 0` check must remain nested inside the `if (!ret)` success branch of `spacemit_i2c_wait_bus_idle()` so that the `else` branch runs `spacemit_i2c_check_bus_release()` whenever `spacemit_i2c_wait_bus_idle()` returns a negative error. Broken by: flattening into `if (!ret) ret = spacemit_i2c_xfer_msg(i2c); else if (ret < 0) ... else spacemit_i2c_check_bus_release(i2c);`. [F27]
- In `spacemit_i2c_handle_err()` and `spacemit_i2c_xfer()`, non-timeout/non-arbitration transfer errors (such as `-ENXIO` or `-EIO`) must only be logged with `dev_dbg()`, reserving `dev_err()` for `-ETIMEDOUT` and `-EAGAIN` and `dev_warn_ratelimited()` for bus reset failure in `spacemit_i2c_conditionally_reset_bus()`. [F54]
