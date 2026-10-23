# ANALOG DEVICES INC AD9467 DRIVER

This component is the Linux IIO SPI driver for the Analog Devices AD9467 and compatible high-speed ADCs (AD9265, AD9434, AD9643, AD9649, AD9652) using the AN-877 common SPI register map. It manages chip configuration over SPI, coordinates data capture and PRBS interface delay/phase calibration with an IIO backend (such as ADI AXI-ADC), and exposes IIO sysfs attributes and debugfs interfaces for test pattern control and calibration table inspection.

## Architecture & Data Flow

- Buffers passed to `spi_write()` in `ad9467_spi_write()` must reside in the `IIO_DMA_MINALIGN`-aligned `st->buf` at the end of `struct ad9467_state`, never on the stack, and no unaligned fields may be placed after `st->buf` in `struct ad9467_state`. [F64]
- In `st->calib_map`, a clear bit (`0`) represents a valid calibration point and a set bit (`1`) represents an invalid point: `ad9467_calibrate()` must initialize `st->calib_map` with `bitmap_fill()` and only clear `point + invert * test_points` when all channels report `stat == false` from `iio_backend_chan_status()`. Broken by: zero-initializing `st->calib_map` or setting bits on success, which inverts `ad9467_find_optimal_point()` and `ad9467_dump_calib_table()`. [F18]
- Enabling any ADC test pattern (in `ad9647_calibrate_prepare()` or `ad9467_chan_test_mode_write()`) requires setting output mode to `st->info->default_output_mode`, and disabling test mode requires restoring `st->info->default_output_mode | AN877_ADC_OUTPUT_MODE_TWOS_COMPLEMENT` via `ad9467_outputmode_set()`. [F21]
- In `ad9467_chan_test_mode_write()`, turning off test mode (`mode == AN877_ADC_TESTMODE_OFF`) must check the prior `chan->mode` for `AN877_ADC_TESTMODE_PN9_SEQ` or `AN877_ADC_TESTMODE_PN23_SEQ` to call `ad9467_backend_testmode_off()`, and `chan->mode` must only be updated to `mode` after all mode-transition calls succeed. Broken by: testing the newly parsed `mode` instead of `chan->mode`, or assigning `chan->mode = mode` before the branch. [F43]
- On multi-channel devices (`st->info->num_channels > 1`), `ad9467_testmode_set()` must select the target channel by writing `BIT(chan)` to `AN877_ADC_REG_CHAN_INDEX` before writing `AN877_ADC_REG_TEST_IO`, and then restore `AN877_ADC_REG_CHAN_INDEX` to `GENMASK(st->info->num_channels - 1, 0)` before syncing. [F59]
- Every `ad9467_chip_info` must set `test_points <= AD9647_MAX_TEST_POINTS` because `st->calib_map` has fixed capacity `AD9647_MAX_TEST_POINTS * 2` bits (doubled in `ad9467_probe()` when `AD9467_CAN_INVERT(st)` is true) and `ad9467_dump_calib_table()` formats `st->calib_map_size` bits into `map[AD9647_MAX_TEST_POINTS * 2 + 3]`. [F29]
- Every entry in `ad9467_chip_info.scale_table` must satisfy `(scale_table[i][0] * 1000000ULL) >> channels[0].scan_type.realbits < 1000000` so that `__ad9467_get_scale()` produces `*val == 0`, because `ad9467_set_scale()` immediately rejects any `val != 0` with `-EINVAL`. [F45]
- In `struct ad9467_chip_info`, `test_mask` must remain an `unsigned long` with `test_mask_len <= ARRAY_SIZE(ad9467_test_modes)`, and `test_mask` must only set bits that have non-NULL entries in `ad9467_test_modes[]` (excluding unpopulated gap indices `0xD` and `0xE` between `AN877_ADC_TESTMODE_MIXED_BIT_FREQUENCY` and `AN877_ADC_TESTMODE_RAMP`). Broken by: using `GENMASK(AN877_ADC_TESTMODE_RAMP, AN877_ADC_TESTMODE_OFF)` in `test_mask`. [F51]
- In `ad9467_probe()`, per-instance differences in `struct iio_info` callbacks must be handled by selecting between distinct `static const` structures (`ad9467_info` when `st->info->num_scales > 1`, or `ad9467_info_no_read_avail` otherwise) rather than mutating a shared `struct iio_info` at runtime. [F54]
- Every supported chip variant must have matching entries pointing to its `const struct ad9467_chip_info` table in both `ad9467_of_match` and `ad9467_ids` so that `spi_get_device_match_data(spi)` succeeds regardless of whether the device is instantiated via Device Tree or SPI ID matching. [F63]

## Synchronization

- After device registration, every code path calling `ad9467_spi_write()` and latching staged registers with `AN877_ADC_REG_TRANSFER` (including `ad9467_reg_access()`, `ad9467_set_scale()`, `ad9467_outputmode_set()`, `ad9467_testmode_set()`, and `ad9467_calibrate_apply()`) must hold `st->lock` across the entire sequence. [F5]
- After probe, all modifications and reads of `st->calib_map` in `ad9467_calibrate()` (via `bitmap_fill()` and non-atomic `__clear_bit()`) and `ad9467_dump_calib_table()` (via `test_bit()`) must be serialized under `st->lock`. [F7]
- In `ad9467_write_raw()` for `IIO_CHAN_INFO_SAMP_FREQ`, `iio_device_claim_direct()` must be acquired before calling `__ad9467_update_clock()` (which calls `clk_set_rate()` and then acquires `st->lock` for `ad9467_calibrate()`) and released with `iio_device_release_direct()` afterward. Broken by: acquiring `iio_device_claim_direct()` inside `st->lock`, or running runtime calibration without claiming IIO direct mode. [F66]
- In `ad9467_debugfs_init()`, `st->chan_test[chan].idx` and `st->chan_test[chan].st` must be initialized before `debugfs_create_file()` publishes `&st->chan_test[chan]` as `file->private_data` for `ad9467_chan_test_mode_fops`. [F10]

## Object Lifetimes

- In `ad9467_iio_backend_get()`, `for_each_node_with_property(__back, "adi,adc-dev")` manages the reference on `__back` across `continue` branches while `of_parse_phandle()` returns a separate reference in `__me`, so `continue` paths must only `of_node_put(__me)` whereas early `return` paths must `of_node_put()` both `__me` and `__back`. [F42]
- In `ad9467_probe()`, the clock (`devm_clk_get_enabled`), powerdown GPIO, `ad9467_reset()`, `CHIP_ID` check, IIO backend acquisition and enablement (`devm_iio_backend_enable`), and initial `ad9467_calibrate()` must all complete before exposing the device via `devm_iio_device_register()`, while `ad9467_debugfs_init()` must run after registration. [F61]

## API Contracts

- `ad9467_find_optimal_point()` passes `nbits + start` as the upper bound to `for_each_clear_bitrange_from()` and stores `*val = start_cnt + cnt / 2` as an absolute bit index in `calib_map`, so `ad9467_calibrate()` must subtract `test_points` from `inv_val` when using the inverted pass (`start = test_points`). [F38]
- `ad9467_read_avail()` must expose `st->scales` pre-populated by `ad9467_scale_fill()` via `__ad9467_get_scale()` (not raw `st->info->scale_table` entries) with `*type = IIO_VAL_INT_PLUS_MICRO` and `*length = info->num_scales * 2` so that `in_voltage_scale_available` matches the values expected by `ad9467_get_scale()` and `ad9467_set_scale()`. [F68]
- For chip variants with `num_scales == 1` (such as `ad9649_chip_tbl`), channel definitions must omit `BIT(IIO_CHAN_INFO_SCALE)` from `.info_mask_shared_by_type_available`, `ad9467_probe()` must assign `ad9467_info_no_read_avail`, `ad9467_get_scale()` must bypass reading `AN877_ADC_REG_VREF`, and `ad9467_set_scale()` must return `-EOPNOTSUPP`. [F69]

## Error Handling

- `ad9467_spi_read()` returns a negative `int` errno on SPI failure or an 8-bit non-negative register value on success, so callers such as `ad9467_get_scale()` and `ad9467_reg_access()` must store its result in a signed `int` and check `ret < 0` before masking or exporting the value. [F11]
- `ad9467_find_optimal_point()` only initializes `*val` when its returned run length `cnt` is non-zero, so `ad9467_calibrate()` must keep `inv_cnt` initialized to `0` before the `retune:` loop and must not read `val` or `inv_val` unless the corresponding `cnt` or `inv_cnt` is greater than zero. [F15]
- Because AN-877 SPI configuration registers are double-buffered, writes to ADC configuration registers in `ad9467_reg_access()`, `ad9467_set_scale()`, `ad9467_outputmode_set()`, `ad9467_testmode_set()`, and `ad9467_calibrate_apply()` must check every `ad9467_spi_write()` for error before committing staged changes by writing `AN877_ADC_TRANSFER_SYNC` to `AN877_ADC_REG_TRANSFER`. [F12, F58]

## External Contracts

- In `ad9467_reset()`, the optional `"reset"` GPIO must be requested logically asserted via `devm_gpiod_get_optional(dev, "reset", GPIOD_OUT_HIGH)` and then de-asserted via `gpiod_set_value_cansleep(gpio, 0)` so active-low polarity in Device Tree leaves the ADC out of reset. Broken by: requesting with `GPIOD_OUT_LOW` and setting it to `1`. [F56]
- In `ad9467_iio_backend_get()`, if `devm_iio_backend_get(dev, NULL)` fails with `-ENOENT`, the driver must fall back to searching Device Tree nodes with the legacy `"adi,adc-dev"` phandle referencing `dev` and resolving the backend via `__devm_iio_backend_get_from_fwnode_lookup()`, whereas any error other than `-ENOENT` (such as `-EPROBE_DEFER`) must be returned immediately. [F71]
- Every `struct iio_chan_spec` in `drivers/iio/adc/ad9467.c` (defined via `AD9467_CHAN`) must set `.scan_type.sign` to lowercase `'s'` (or `'u'`), never uppercase `'S'`, to comply with the IIO sysfs `scan_elements` type ABI. [F67]

## Known Pitfalls

- `ad9467_chan_test_mode_write()` must zero-initialize `test_mode` and pass `sizeof(test_mode) - 1` as the available buffer size to `simple_write_to_buffer()` so that user input is always NUL-terminated before `sysfs_streq()` reads it. [F31]
- In `ad9467_chan_test_mode_read()`, `iio_backend_debugfs_print_chan_status()` must be passed the advanced pointer `buf + len` and the remaining capacity `sizeof(buf) - len`, and `ret` must be checked for errors before adding `ret` to `len`. [F52]
- In `__ad9467_get_scale()`, `info->scale_table[index][0]` must be multiplied by a 64-bit unsigned constant (`1000000ULL`) before right-shifting by `chan->scan_type.realbits`, and the shifted result is truncated to `unsigned int tmp` before dividing and taking modulo by `1000000`. Broken by: using a 32-bit constant (overflowing 32 bits) or dividing a 64-bit integer directly without `do_div()`/`div_u64_rem()` on 32-bit targets. [F44]
