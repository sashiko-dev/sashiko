# SENSIRION SCD4X CARBON DIOXIDE SENSOR DRIVER

This component is an Industrial I/O (IIO) I2C driver for the Sensirion SCD40 and SCD41 carbon dioxide, temperature, and relative humidity sensors. It manages continuous measurement states, CRC-8 verified I2C command/response transfers, triggered buffer polling, power regulator control, and sysfs calibration attributes.

## Architecture & Data Flow

- In `scd4x_read()`, the pre-transfer `CMD_STOP_MEAS` condition and the post-transfer `CMD_START_MEAS` condition must whitelist the exact same commands executable during measurement (`CMD_GET_DATA_READY`, `CMD_READ_MEAS`, `CMD_GET_AMB_PRESSURE`) so every command that stops continuous measurement also restarts it. Broken by: updating only one of the two exemption checks or returning early after `CMD_STOP_MEAS` without restarting measurement. [F18]
- When `scd4x_write()` stops continuous measurement with `CMD_STOP_MEAS` (for any command other than `CMD_SET_AMB_PRESSURE`), either `scd4x_write()` itself or, for `CMD_FRC`, its caller `scd4x_write_and_fetch()` on both normal and `err` paths must send `CMD_START_MEAS` to return the sensor to continuous measurement mode. [F19]
- In `scd4x_read()` and `scd4x_write_and_fetch()`, after inflating `response_sz` via `response_sz += response_sz / 2`, the parsing loop must step source index `i` by 3 over `buf` while advancing destination pointer `rsp` by 2 bytes per iteration to strip each CRC byte at `buf[i + 2]`. [F30]
- In `scd4x_wait_meas_poll()`, the big-endian status word `bval` read by `CMD_GET_DATA_READY` must be converted via `val = be16_to_cpu(bval)` before testing the 11-bit readiness mask `(val & 0x7FF)` inside the bounded `do ... while (--tries)` polling loop. [F32]
- The `enum scd4x_channel_idx` values (`SCD4X_CO2`, `SCD4X_TEMP`, `SCD4X_HR`), the `address` and `scan_index` fields of input channels in `scd4x_channels`, and the 3-element buffers in `scd4x_read_meas()`, `scd4x_read_channel()`, and `scd4x_trigger_handler()` must stay synchronized with the 3-word `CMD_READ_MEAS` payload layout and `scd4x_scan_masks` (`0x07`). [F20]
- In `scd4x_trigger_handler()`, the stack `scan` struct passed to `iio_push_to_buffers_with_ts()` must use `aligned_s64` for `ts` and be zero-initialized (`= { }`), so its size is 16 bytes on all architectures and the 2-byte padding after `uint16_t data[3]` is not leaked uninitialized. [F40]
- In `scd4x_suspend()`, `CMD_STOP_MEAS` must succeed before `regulator_disable(state->vdd)` is called, and in `scd4x_resume()`, `regulator_enable(state->vdd)` must succeed before `CMD_START_MEAS` is sent. [F49]

## Synchronization

- In all runtime IIO, sysfs, and trigger callbacks (`scd4x_read_raw`, `scd4x_write_raw`, `calibration_auto_enable_show`, `calibration_auto_enable_store`, `calibration_forced_value_store`, and `scd4x_trigger_handler`), `state->lock` must be held across every call to `scd4x_read()`, `scd4x_write()`, `scd4x_write_and_fetch()`, and `scd4x_read_poll()`. [F6]
- The sensor I2C helpers (`scd4x_i2c_xfer`, `scd4x_send_command`, `scd4x_read`, `scd4x_write`, `scd4x_write_and_fetch`, `scd4x_read_meas`, `scd4x_wait_meas_poll`, `scd4x_read_poll`, and `scd4x_read_channel`) must remain lock-free with respect to `state->lock` because their runtime callers already hold `state->lock` across the entire operation. [F7]
- In `scd4x_read_raw()`, reading `IIO_CHAN_INFO_RAW` on an input channel must claim direct mode with `iio_device_claim_direct(indio_dev)` before acquiring `state->lock` and call `iio_device_release_direct(indio_dev)` after unlocking `state->lock` on both error and success paths. Broken by: returning early on `scd4x_read_channel()` failure before releasing direct mode, or claiming direct mode under `state->lock`. [F2]
- `scd4x_trigger_handler` must be registered as the threaded handler (`thread` argument, with `h` set to `NULL`) in `devm_iio_triggered_buffer_setup()`, because it acquires the `state->lock` mutex and calls `scd4x_read_poll()`, which performs blocking I2C transfers and `msleep_interruptible()` sleeps. [F50]
- In `scd4x_probe()`, `state->lock`, `state->client`, `scd4x_crc8_table`, regulator enablement (`state->vdd`), `devm_iio_triggered_buffer_setup()`, and starting continuous measurement (`CMD_START_MEAS`) must all be completed before calling `devm_iio_device_register(dev, indio_dev)`. [F9]

## Object Lifetimes

- In `scd4x_probe()`, `scd4x_scan_masks` assigned to `indio_dev->available_scan_masks` must retain `static` storage duration and remain a zero-terminated `unsigned long` array, because the IIO core accesses `available_scan_masks` after `scd4x_probe()` returns. [F41]

## API Contracts

- `scd4x_read()` and `scd4x_write_and_fetch()` expand `response_sz` in place by `response_sz / 2` to read into the 9-byte stack buffer `buf[SCD4X_READ_BUF_SIZE]` and copy raw big-endian wire bytes (with CRC bytes stripped) into `response` without byte-swapping. Callers must pass the unexpanded payload byte size (an even multiple of 2 up to 6 bytes, excluding CRC) and apply `be16_to_cpu()` to each 16-bit word. [F16, F37]
- `scd4x_read_channel()` returns a non-negative 16-bit sensor reading (`buf[chan]`) on success and a negative errno on failure, so callers such as `scd4x_read_raw()` must test `ret < 0` rather than `if (ret)` before treating `ret` as an error. [F12]
- Callers of `scd4x_read_meas()` and `scd4x_read_poll()` must pass a `uint16_t` buffer of at least 3 elements, because `scd4x_read_meas()` unconditionally writes all 3 words (`ARRAY_SIZE(buf)`) into the decayed `uint16_t *meas` pointer. [F38]
- Sysfs callbacks in `scd4x_attrs` (`calibration_auto_enable_show`, `calibration_auto_enable_store`, `calibration_forced_value_store`) must downcast `dev` via `dev_to_iio_dev(dev)`, whereas devm callbacks (`scd4x_stop_meas`, `scd4x_disable_regulator`) receive `struct scd4x_state *` directly as their `void *` argument. [F42]
- `scd4x_read_raw()` must store output values into `*val` (and `*val2` for multi-part types) and return an `IIO_VAL_*` format constant (`IIO_VAL_INT`, `IIO_VAL_INT_PLUS_MICRO`, or `IIO_VAL_FRACTIONAL`) on success or a negative errno on failure, never `0`. [F54]
- When `scd4x_read_avail()` returns `IIO_AVAIL_RANGE` with `*type = IIO_VAL_INT`, `*vals` must point to a 3-element integer array ordered as `[min, step, max]` (such as `scd4x_pressure_calibbias_available`). [F55]

## Error Handling

- `scd4x_trigger_handler()` must always unlock `state->lock` and execute `iio_trigger_notify_done(indio_dev->trig)` at the `out` label before returning `IRQ_HANDLED`, even when `scd4x_read_poll()` returns an error. [F3]
- Callers of `scd4x_read()`, `scd4x_write_and_fetch()`, `scd4x_read_meas()`, and `scd4x_read_poll()` must check for a non-zero error return before reading the output buffer argument (`response`, `meas`, or `buf`), because those helpers leave the caller's buffer unpopulated on I2C or CRC failure. [F11]
- In `scd4x_write_and_fetch()` and `scd4x_wait_meas_poll()`, the error-recovery call `scd4x_send_command(state, CMD_START_MEAS)` must not overwrite the original failure status (`ret` in `scd4x_write_and_fetch()` or `-ETIMEDOUT` in `scd4x_wait_meas_poll()`). [F14]
- Every direct call to `i2c_master_send()` or `i2c_master_recv()` in `scd4x_i2c_xfer()` and `scd4x_write_and_fetch()` must check both `ret < 0` and `ret != expected_size` (converting short transfers to `-EIO`) so that callers receive `0` on full transfer or a negative errno. [F13]

## External Contracts

- `scd4x_write_raw()` (for `IIO_PRESSURE`) and `calibration_forced_value_store()` must reject values outside `[SCD4X_PRESSURE_COMP_MIN_MBAR, SCD4X_PRESSURE_COMP_MAX_MBAR]` and `[SCD4X_FRC_MIN_PPM, SCD4X_FRC_MAX_PPM]` with `-EINVAL`, matching `scd4x_pressure_calibbias_available` and `calibration_forced_value_available`. [F26]

## Known Pitfalls

- In `scd4x_read_raw()` under `case IIO_CHAN_INFO_RAW`, the `if (chan->output)` check for the `IIO_PRESSURE` channel must execute before the `scd4x_read_channel(state, chan->address)` path because the `IIO_PRESSURE` entry in `scd4x_channels` has a zero-initialized `.address` equal to `SCD4X_CO2`. [F31]
- In `scd4x_send_command()`, both recursive calls to `scd4x_send_command(state, CMD_STOP_MEAS)` and `scd4x_send_command(state, CMD_START_MEAS)` must remain guarded by `(cmd != CMD_STOP_MEAS) && (cmd != CMD_START_MEAS)` so recursion terminates at depth 1. [F28]
