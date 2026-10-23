# LTC4282 HARDWARE MONITOR DRIVER

This unit is an I2C hardware monitoring (`hwmon`) and common clock framework driver (`drivers/hwmon/ltc4282.c`) for the Analog Devices LTC4282 High Current Hot Swap Controller. It configures operating parameters from firmware properties, registers an optional clock provider for the `CLKOUT` pin, and exposes voltage (`VSOURCE`, `VDD`, `VGPIO`), current, power, and energy telemetry, limits, and fault logs via sysfs and debugfs.

## Architecture & Data Flow

- `st->rsense`, `st->vfs_out`, `st->vdd`, `st->vsense_max`, and `st->power_max` are initialized in `ltc4282_setup` before `devm_hwmon_device_register_with_info` and `ltc4282_debugfs_init` and must remain read-only thereafter because runtime hwmon callbacks read them without acquiring `st->lock`. [F10]
- `ltc4282_setup` enforces `st->rsense >= CENTI` (100 nOhm) on `"adi,rsense-nano-ohms"` and divides `st->rsense` by `CENTI`, so `st->rsense` is always non-zero and stored in units of 0.1 micro-ohms (requiring a `DECA` (10) factor in current, power, and energy scaling formulas). [F14]
- `st->energy_en` must be kept synchronized under `st->lock` with the logical inverse of `LTC4282_METER_HALT_MASK` in `LTC4282_ADC_CTRL` across `ltc4282_get_defaults()` and `ltc4282_energy_enable_set()`, because `ltc4282_energy_show()` gates `ltc4282_read_energy()` on `st->energy_en` and returns `-ENODATA` when it is false. [F28]
- Because `ltc4282_regmap_config` sets `val_bits = 8`, 16-bit telemetry and history registers must use 2-byte `regmap_bulk_read()`/`regmap_bulk_write()` with `__be16` conversions, and the 48-bit `LTC4282_ENERGY` register must be read via a 6-byte `regmap_bulk_read()` into a `__be64` followed by `be64_to_cpu(raw) >> 16`. [F32]
- In `ltc4282_setup`, `ltc4282_get_defaults` must run before parsing optional firmware properties or calling `ltc4282_set_max_limits` so EEPROM-backed defaults for `st->energy_en`, `vin_mode`, `st->vsense_max`, and `st->in0_1_cache` are loaded before selective overrides and limit calculations. [F56]
- `ltc4282_out_rates` must remain sorted in ascending order (`LTC4282_CLKOUT_CNV` followed by `LTC4282_CLKOUT_SYSTEM`) because `ltc4282_determine_rate()` passes it to `find_closest()`. [F44]
- `ltc4282_readable_reg` and `ltc4282_writable_reg` must reject `LTC4282_RESERVED_1` (0x32) and `LTC4282_RESERVED_2` (0x33), and `ltc4282_writable_reg` must also reject writes to the read-only status registers `LTC4282_STATUS_LSB` (0x1e) and `LTC4282_STATUS_MSB` (0x1f). [F63]

## Object Lifetimes

- In `ltc4282_probe`, after initializing `st->map`, the driver must set `LTC4282_RESET_MASK` in `LTC4282_ADC_CTRL` and sleep for 3200 ms (`msleep(3200)`) before calling `ltc428_clks_setup` or `ltc4282_setup` to allow the hardware soft-reset and EEPROM reload to complete. [F55]
- In `ltc428_clks_setup`, the optional input clock must be acquired with `devm_clk_get_optional_enabled`, validated to have a rate within `[LTC4282_CLKIN_MIN, LTC4282_CLKIN_MAX]` (250 kHz to 15.5 MHz), and divided to 250 kHz by writing `rate / (2 * LTC4282_CLKIN_MIN)` into `LTC4282_CLK_DIV_MASK` of `LTC4282_CLK_DIV`. [F67]

## API Contracts

- `__ltc4282_read_alarm()` clears the queried `mask` bits via `regmap_clear_bits` unless `reg` is `LTC4282_STATUS_LSB` or `LTC4282_FAULT_LOG` (`LTC4282_FAULT_LOG` bits are cleared only in `ltc4282_power_reset_hist`, `ltc4282_vdd_source_reset_hist`, and `ltc4282_curr_reset_hist`). Passing `LTC4282_STATUS_MSB` or another read-only/persistent register to `ltc4282_read_alarm()` or `__ltc4282_read_alarm()` requires excluding it in that guard. [F40, F64]
- `ltc4282_write_voltage_byte_cached`, `ltc4282_write_voltage_byte`, and `__ltc4282_in_write_history` require the target voltage (`val` or `lowest`/`highest`) and the full-scale parameter `fs` to be in the same unit (both mV or both uV). Broken by: passing `st->vsense_max` (in uV) with `fs = 40` instead of `40 * MILLI`, or mV `in` in `ltc4282_write_curr` with `40 * MILLI`. [F20]
- `__ltc4282_in_write_history` and `ltc4282_in_write_history` require `reg` to be a `*_LOWEST` register address (`LTC4282_VGPIO_LOWEST`, `LTC4282_VSOURCE_LOWEST`, or `LTC4282_VSENSE_LOWEST`) because `__ltc4282_in_write_history` writes `lowest` to `reg` and `highest` to `reg + 2`. [F12]
- In `ltc4282_read_voltage_byte_cached` and `ltc4282_write_voltage_byte_cached`, a non-NULL `cached_raw`/`cache_raw` pointer redirects the 8-bit limit access to the in-memory cache without touching `st->map`, whereas a NULL pointer (used by `ltc4282_read_voltage_byte` and `ltc4282_write_voltage_byte`) accesses hardware register `reg`. [F11]
- `ltc4282_recalc_rate` (implementing `clk_ops.recalc_rate`) returns an `unsigned long` rate in Hz and must return `0` (not a negative errno) when `regmap_read` fails or when `FIELD_GET(LTC4282_CLKOUT_MASK, clkdiv)` is `0`. [F22]

## Error Handling

- In `ltc428_clks_setup`, `devm_clk_get_optional_enabled` returns an `ERR_PTR` on error and `NULL` when the optional clock is absent, so `IS_ERR(clkin)` must be checked first and `!clkin` must return `0` before calling `clk_get_rate(clkin)`. [F17]

## Known Pitfalls

- `ltc4282_vdd_source_read_alm()` must return `*val = 0` without calling `__ltc4282_read_alarm()` when `st->in0_1_cache[channel].en` is false, because `__ltc4282_read_alarm()` clears `LTC4282_ADC_ALERT_LOG` bits on read and would consume the active sibling channel's alarm before the next ADC conversion. [F24]
- In `ltc4282_gpio_setup()`, configuring `"adi,gpio2-mode"` as `"adc_input"` (setting `LTC4282_GPIO_MODE_MASK` in `LTC4282_ILIM_ADJUST`) and enabling `"adi,gpio3-monitor-enable"` (clearing `LTC4282_GPIO_MODE_MASK`) are mutually exclusive and must be rejected with `-EINVAL`. `func` must be initialized to `NULL`, not reused after `"adi,gpio2-mode"` (and `"adc_input"` must not be added to `ltc4282_gpio1_modes`), and checked via `if (func && !strcmp(func, "adc_input"))`. [F19, F30, F42]
- In `ltc4282_read_power_word`, `ltc4282_write_power_word`, and `ltc4282_read_energy`, 64-bit numerator products must be guarded with `check_mul_overflow` and fall back to pre-dividing by `U16_MAX` or `DECA * MICRO` before multiplying the remaining factor. [F48]
- Divisions in `ltc4282_read_power_word`, `ltc4282_read_power_byte`, `ltc4282_read_energy`, `ltc4282_write_power_byte`, and `ltc4282_write_power_word` whose divisors can exceed `U32_MAX` must use `DIV64_U64_ROUND_CLOSEST` with a 64-bit widened divisor (such as `U16_MAX * (u64)st->rsense`) rather than `DIV_ROUND_CLOSEST_ULL`. [F49]
- `ltc4282_read_voltage_word` must widen `fs` to `u64` before multiplying by `be16_to_cpu(in)` and divide using `DIV_ROUND_CLOSEST_ULL`, because `ltc4282_read_current_word` passes `fs` as `DECA * 40 * MILLI` (400000), whose product with a 16-bit ADC code exceeds `U32_MAX`. [F46]
- In `ltc4282_setup()`, the local variable `val` must be initialized to `vin_mode` before reading `"adi,vin-mode-microvolt"` and mapped in-place to the `LTC4282_VIN_*` enum (0..3) inside the `if (!ret)` branch before indexing `ltc4282_vfs_milli[val]` and `ltc4282_vdd_milli[val]`. Broken by: reusing `val` or storing the mapped index in another variable before indexing those arrays. [F43]
- In `ltc4282_setup`, each firmware property check (such as `device_property_read_bool` for `"adi,fault-log-enable"`) and its corresponding `regmap` register modification must appear only once without duplicate conditional blocks issuing redundant I2C writes. [F61]
