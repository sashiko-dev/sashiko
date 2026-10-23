# SENSIRION SCD4X CARBON DIOXIDE SENSOR DRIVER

This component is an Industrial I/O (IIO) I2C driver for the Sensirion SCD40 and SCD41 carbon dioxide, temperature, and relative humidity sensors. It manages continuous measurement states, CRC-8 verified I2C command/response transfers, triggered buffer polling, power regulator control, and sysfs calibration attributes.

## Architecture & Data Flow
- Continuous measurement mode must be restored after commands that require stopping it: `scd4x_read()` must whitelist the exact same commands (`CMD_GET_DATA_READY`, `CMD_READ_MEAS`, `CMD_GET_AMB_PRESSURE`) in its pre-transfer `CMD_STOP_MEAS` check and post-transfer `CMD_START_MEAS` check; likewise, when `scd4x_write()` sends `CMD_STOP_MEAS` (for any command except `CMD_SET_AMB_PRESSURE`), either `scd4x_write()` or—for `CMD_FRC`—`scd4x_write_and_fetch()` (on both normal and `err` paths) must send `CMD_START_MEAS`. [F18, F19]
- In `scd4x_suspend()`, `CMD_STOP_MEAS` must succeed before calling `regulator_disable(state->vdd)` (do not disable the regulator if stopping measurement fails), and in `scd4x_resume()`, `regulator_enable(state->vdd)` must succeed before sending `CMD_START_MEAS`. [F49]
- In `scd4x_wait_meas_poll()`, the big-endian status word `bval` from `CMD_GET_DATA_READY` must be converted via `val = be16_to_cpu(bval)` before testing `(val & 0x7FF)` inside the bounded `do ... while (--tries)` loop. [F32]
- In `scd4x_trigger_handler()`, the stack `scan` struct passed to `iio_push_to_buffers_with_ts()` must use `aligned_s64` for `ts` and be zero-initialized (`= { }`) so its size is 16 bytes across architectures and the 2-byte padding after `uint16_t data[3]` is not leaked uninitialized. [F40]
- `enum scd4x_channel_idx` (`SCD4X_CO2`, `SCD4X_TEMP`, `SCD4X_HR`), the `address` and `scan_index` fields of input channels in `scd4x_channels`, `scd4x_scan_masks` (`0x07`), and the 3-element buffers in `scd4x_read_meas()`, `scd4x_read_channel()`, and `scd4x_trigger_handler()` must stay synchronized with the 3-word `CMD_READ_MEAS` payload; callers of `scd4x_read_meas()` and `scd4x_read_poll()` must always pass a `uint16_t` buffer of at least 3 elements. [F20, F38]

## Synchronization
- All runtime IIO, sysfs, and trigger callbacks (`scd4x_read_raw`, `scd4x_write_raw`, `calibration_auto_enable_show`, `calibration_auto_enable_store`, `calibration_forced_value_store`, `scd4x_trigger_handler`) must hold `state->lock` across every call to `scd4x_read`, `scd4x_write`, `scd4x_write_and_fetch`, and `scd4x_read_poll`, while the sensor I2C/measurement helpers (`scd4x_i2c_xfer`, `scd4x_send_command`, `scd4x_read`, `scd4x_write`, `scd4x_write_and_fetch`, `scd4x_read_meas`, `scd4x_wait_meas_poll`, `scd4x_read_poll`, `scd4x_read_channel`) must remain lock-free. [F6, F7]
- In `scd4x_read_raw()`, reading `IIO_CHAN_INFO_RAW` on an input channel must call `iio_device_claim_direct(indio_dev)` before acquiring `state->lock`, and must call `iio_device_release_direct(indio_dev)` after unlocking `state->lock` on both success and error paths. [F2]
- `scd4x_trigger_handler()` must be registered as the threaded handler (`thread` argument, with `h` set to `NULL`) in `devm_iio_triggered_buffer_setup()`, because it acquires the `state->lock` mutex and calls `scd4x_read_poll()`, which performs blocking I2C transfers and `msleep_interruptible()` sleeps. [F50]
- In `scd4x_probe()`, `state->lock`, `state->client`, `scd4x_crc8_table`, regulator enablement (`state->vdd`), `devm_iio_triggered_buffer_setup()`, and starting continuous measurement (`CMD_START_MEAS`) must all be completed before calling `devm_iio_device_register(dev, indio_dev)`. [F9]

## Object Lifetimes
- In `scd4x_probe()`, `scd4x_disable_regulator` must be registered via `devm_add_action_or_reset()` immediately after `regulator_enable(state->vdd)` and before `scd4x_stop_meas` and `devm_iio_device_register()`, ensuring devres LIFO teardown unregisters the IIO device and sends `CMD_STOP_MEAS` while `state->vdd` is still powered. [F1]
- In `scd4x_probe()`, `scd4x_scan_masks` assigned to `indio_dev->available_scan_masks` must retain `static` storage duration and remain a zero-terminated `unsigned long` array, because the IIO core accesses it after `scd4x_probe()` returns. [F41]

## API Contracts
- `scd4x_read()` and `scd4x_write_and_fetch()` take an unexpanded payload byte size `response_sz` (an even multiple of 2, at most 6 bytes), inflate it via `response_sz += response_sz / 2` to read into the 9-byte `buf[SCD4X_READ_BUF_SIZE]`, and step source index `i` by 3 while advancing destination `rsp` by 2 to strip CRC bytes at `buf[i + 2]` without byte-swapping; callers must pass `sizeof()` of the payload (not wire length or element count) and apply `be16_to_cpu()` to each returned 16-bit word. [F16, F30, F37]
- `scd4x_read_channel()` returns a non-negative 16-bit sensor reading (`buf[chan]`) on success and a negative errno on failure; callers such as `scd4x_read_raw()` must check `if (ret < 0)` rather than `if (ret)`. [F12]
- Sysfs callbacks in `scd4x_attrs` (`calibration_auto_enable_show`, `calibration_auto_enable_store`, `calibration_forced_value_store`) must obtain the IIO device via `dev_to_iio_dev(dev)` (not `dev_get_drvdata(dev)`), whereas devm callbacks (`scd4x_stop_meas`, `scd4x_disable_regulator`) receive `struct scd4x_state *` directly as their `void *` argument. [F42]
- `scd4x_read_raw()` must store output values into `*val` (and `*val2` for multi-part types) and return an `IIO_VAL_*` constant (`IIO_VAL_INT`, `IIO_VAL_INT_PLUS_MICRO`, or `IIO_VAL_FRACTIONAL`) on success or a negative errno on failure, never `0`. [F54]
- When `scd4x_read_avail()` returns `IIO_AVAIL_RANGE` with `*type = IIO_VAL_INT`, `*vals` must point to a 3-element integer array ordered as `[min, step, max]` (such as `scd4x_pressure_calibbias_available`). [F55]

## Error Handling
- `scd4x_trigger_handler()` must always unlock `state->lock` and call `iio_trigger_notify_done(indio_dev->trig)` at the `out` label before returning `IRQ_HANDLED`, even when `scd4x_read_poll()` fails. [F3]
- Callers of `scd4x_read()`, `scd4x_write_and_fetch()`, `scd4x_read_meas()`, and `scd4x_read_poll()` must check for a non-zero error return before reading the output buffer (`response`, `meas`, or `buf`), which is left unpopulated on I2C or CRC failure. [F11]
- In `scd4x_write_and_fetch()` and `scd4x_wait_meas_poll()`, the error-recovery call `scd4x_send_command(state, CMD_START_MEAS)` must not overwrite the original failure return value (`ret` in `scd4x_write_and_fetch()` or `-ETIMEDOUT` in `scd4x_wait_meas_poll()`). [F14]
- Every direct call to `i2c_master_send()` or `i2c_master_recv()` in `scd4x_i2c_xfer()` and `scd4x_write_and_fetch()` must check both `ret < 0` and `ret != expected_size` (converting short transfers to `-EIO`) so callers receive `0` or a negative errno, never a positive byte count. [F13]

## External Contracts
- `scd4x_write_raw()` (for `IIO_PRESSURE`) and `calibration_forced_value_store()` must reject values outside `[SCD4X_PRESSURE_COMP_MIN_MBAR, SCD4X_PRESSURE_COMP_MAX_MBAR]` and `[SCD4X_FRC_MIN_PPM, SCD4X_FRC_MAX_PPM]` with `-EINVAL`, matching `scd4x_pressure_calibbias_available` and `calibration_forced_value_available`. [F26]

## Known Pitfalls
- In `scd4x_read_raw()` under `case IIO_CHAN_INFO_RAW`, the `if (chan->output)` check for the `IIO_PRESSURE` channel must execute before `scd4x_read_channel(state, chan->address)`, because the `IIO_PRESSURE` entry in `scd4x_channels` has a zero-initialized `.address` equal to `SCD4X_CO2` (`0`). [F31]
- In `scd4x_send_command()`, the recursive calls to `scd4x_send_command(state, CMD_STOP_MEAS)` and `scd4x_send_command(state, CMD_START_MEAS)` must remain guarded by `(cmd != CMD_STOP_MEAS) && (cmd != CMD_START_MEAS)` so recursion terminates at depth 1. [F28]
