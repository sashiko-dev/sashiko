# OMNIVISION OV02C10 SENSOR DRIVER

The OmniVision OV02C10 driver is an I2C V4L2 camera sensor subdevice driver supporting MIPI CSI-2 1-lane and 2-lane configurations at 1928x1092 resolution (10-bit SGRBG10 Bayer format). It integrates with the V4L2 subdevice, media controller, V4L2 control, and CCI regmap frameworks to configure sensor mode registers, blanking, exposure, analog/digital gains, and test patterns while managing power via runtime PM, clocks, regulators, and reset GPIOs.

## Architecture & Data Flow

- Because `v4l2_i2c_subdev_init()` stores `&ov02c10->sd` (at a non-zero offset in `struct ov02c10`) as device drvdata, any function receiving `struct device *` or `struct i2c_client *` must recover `struct ov02c10 *` via `to_ov02c10(dev_get_drvdata(dev))` or `to_ov02c10(i2c_get_clientdata(client))`, never by casting drvdata directly or calling `i2c_set_clientdata(client, ov02c10)`. [F42]
- `ov02c10_check_hwcfg()` must validate that `bus_cfg.bus.mipi_csi2.num_data_lanes` matches a supported entry in `struct ov02c10_mode::lane_settings` and `lane_settings_length` (currently 1 or 2) before storing `ov02c10->mipi_lanes`, because `ov02c10_enable_streams()` indexes both arrays at `ov02c10->mipi_lanes - 1` without further bounds checks. [F29]
- `ov02c10_check_hwcfg()` must succeed before `ov02c10_init_controls()` runs so that `ov02c10->link_freq_index` is derived from a non-empty `link_freq_bitmap` via `ffs(link_freq_bitmap) - 1` and bounded to `[0, ARRAY_SIZE(link_freq_menu_items) - 1]` before indexing `link_freq_menu_items`. [F30]
- In every `struct ov02c10_mode` entry in `supported_modes`, `sequence_length` must equal `ARRAY_SIZE(reg_sequence)` and each `lane_settings_length[i]` must equal `ARRAY_SIZE(lane_settings[i])`, because `ov02c10_enable_streams()` passes them directly to `regmap_multi_reg_write()`. [F32]
- In `ov02c10_init_controls()`, `ov02c10_set_format()`, and `ov02c10_set_ctrl()`, every `ov02c10_mode` in `supported_modes` must satisfy `vts_min >= height`, `hts >= width`, and `vts_min * mipi_lanes <= OV02C10_VTS_MAX`, with `V4L2_CID_VBLANK` capped at `OV02C10_VTS_MAX - mode->height` for `OV02C10_REG_VTS`. [F41]
- When applying an active format in `ov02c10_set_format()`, `ov02c10->vblank` range and value (via `__v4l2_ctrl_modify_range()` and `__v4l2_ctrl_s_ctrl()`, which cascades to `ov02c10->exposure` in `ov02c10_set_ctrl()`) and `ov02c10->hblank` range must be updated from `mode` and `ov02c10->mipi_lanes`. [F27]
- In `ov02c10_test_pattern()`, the `!pattern` case must return after clearing `BIT(7)` (`OV02C10_TEST_PATTERN_ENABLE`) in `OV02C10_REG_TEST_PATTERN`, while non-zero `pattern` values map `ov02c10_test_pattern_menu` indices by writing `(pattern - 1)` to mask `0x03` and setting `BIT(7)` via `cci_update_bits()`; thus `ov02c10_test_pattern_menu` cannot exceed 5 entries (indices 0..4) unless the `0x03` mask is widened. [F31, F36]

## Synchronization

- Because `ov02c10_probe()` aliases `ov02c10->sd.state_lock` to `ov02c10->ctrl_handler.lock` before `v4l2_subdev_init_finalize()`, callbacks holding the active `v4l2_subdev_state` lock (`ov02c10_set_format()`, `ov02c10_enable_streams()`) or control lock (`ov02c10_set_ctrl()`) must use unlocked `__v4l2_ctrl_*` helpers rather than locking `v4l2_ctrl_*` variants (`v4l2_ctrl_modify_range()`, `v4l2_ctrl_s_ctrl()`, `v4l2_ctrl_handler_setup()`) to avoid self-deadlock. [F8]

## Object Lifetimes

- In `ov02c10_probe()`, `v4l2_i2c_subdev_init(&ov02c10->sd, client, &ov02c10_subdev_ops)` must be called before `ov02c10_get_pm_resources()`, `ov02c10_power_on()`, `ov02c10_power_off()`, or enabling runtime PM, because those functions recover `struct ov02c10 *` from `dev_get_drvdata(dev)` via `to_ov02c10()`. [F11]
- In `ov02c10_probe()`, `ov02c10_init_controls()`, `media_entity_pads_init()`, and `ov02c10->sd.state_lock = ov02c10->ctrl_handler.lock` must all complete before `v4l2_subdev_init_finalize(&ov02c10->sd)` and `v4l2_async_register_subdev_sensor(&ov02c10->sd)`. [F46]
- During teardown in `ov02c10_remove()` (and on `ov02c10_probe()` error paths), `v4l2_async_unregister_subdev(sd)` must precede `v4l2_subdev_cleanup(sd)`, which must precede `media_entity_cleanup()`, `v4l2_ctrl_handler_free(sd->ctrl_handler)`, disabling runtime PM, and (if `!pm_runtime_status_suspended(ov02c10->dev)`) calling `ov02c10_power_off(ov02c10->dev)` and `pm_runtime_set_suspended()` before devres releases resources. [F13, F47]
- `ov02c10_enable_streams()` must acquire a runtime PM reference via `pm_runtime_resume_and_get()`, write `mode->reg_sequence` first, `mode->lane_settings[ov02c10->mipi_lanes - 1]` second, and `__v4l2_ctrl_handler_setup(ov02c10->sd.ctrl_handler)` third before asserting `OV02C10_REG_STREAM_CONTROL`, and release the reference via `pm_runtime_put()` on any error or in `ov02c10_disable_streams()`. [F25, F49]
- `ov02c10_power_on()` enables `ov02c10->img_clk`, enables `ov02c10->supplies`, and deasserts `ov02c10->reset` (sets to 0), rolling back `ov02c10->img_clk` via `clk_disable_unprepare()` if `regulator_bulk_enable()` fails; `ov02c10_power_off()` tears them down in reverse order (asserts reset to 1, disables regulators, disables `img_clk`). Broken by: adding a fallible step after `regulator_bulk_enable()` without disabling both regulators and `img_clk` on failure. [F4]
- In `ov02c10_check_hwcfg()`, the endpoint reference from `fwnode_graph_get_endpoint_by_id()` must be released with `fwnode_handle_put(ep)` and the parsed endpoint data from `v4l2_fwnode_endpoint_alloc_parse()` must be freed with `v4l2_fwnode_endpoint_free(&bus_cfg)` on both success and error paths. [F51]

## API Contracts

- In `ov02c10_set_ctrl()`, control-range propagation (`__v4l2_ctrl_modify_range()` on `ov02c10->exposure` for `V4L2_CID_VBLANK`) must execute before `pm_runtime_get_if_in_use(ov02c10->dev)`; when `pm_runtime_get_if_in_use()` returns `0` (suspended), `ov02c10_set_ctrl()` must return `0` without touching hardware, and when it returns non-zero (`1`, active), CCI register writes may proceed and must be followed by `pm_runtime_put(ov02c10->dev)` before returning. [F21, F56]
- Register definitions passed to `cci_read()`, `cci_write()`, and `cci_update_bits()` must encode byte width using `CCI_REG8`, `CCI_REG16`, or `CCI_REG24`, whereas `struct reg_sequence` tables passed to `regmap_multi_reg_write()` must contain raw 16-bit register addresses and single-byte values. [F52]
- In `ov02c10_check_hwcfg()`, `struct v4l2_fwnode_endpoint bus_cfg` must have `.bus_type = V4L2_MBUS_CSI2_DPHY` initialized (with all other fields zeroed) before calling `v4l2_fwnode_endpoint_alloc_parse()`, so that `bus_cfg.bus.mipi_csi2.num_data_lanes` is populated. [F17]
- Every control registered on `ov02c10->ctrl_handler` in `ov02c10_init_controls()` must either have `V4L2_CTRL_FLAG_READ_ONLY` set (explicitly on `ov02c10->link_freq` and `ov02c10->hblank`, or by the V4L2 core on `V4L2_CID_PIXEL_RATE` and fwnode controls) or be handled in `ov02c10_set_ctrl()`. [F57]
- If `supported_modes` is extended beyond one entry, `ov02c10_set_ctrl()`, `ov02c10_enable_streams()`, and `ov02c10_set_format()` must replace their hardcoded `supported_modes[0]` lookup with the active mode bounded by `ARRAY_SIZE(supported_modes)`, matching `ov02c10_enum_frame_size()`. [F33]
- `ov02c10->reset` is acquired via `devm_gpiod_get_optional()` and may be `NULL`; `ov02c10_power_on()` and `ov02c10_power_off()` must drive it via `gpiod_set_value_cansleep()` (which tolerates `NULL` and sleeping GPIO controllers) rather than `gpiod_set_value()`, and `ov02c10_power_on()` checks `if (ov02c10->reset)` to skip the 2 ms and 5 ms reset `usleep_range()` delays when no GPIO is present. [F16, F54]

## Error Handling

- When passing `&ret` to `cci_write()` or `cci_update_bits()` (as in `ov02c10_test_pattern()` and `ov02c10_set_ctrl()`), `ret` must be initialized to `0` before the first call because the CCI helpers skip the register transfer when `*err` is already non-zero. [F14]
- In `ov02c10_init_controls()`, the `v4l2_ctrl_new_*()` helpers return `NULL` on failure and set sticky `ctrl_hdlr->error`; callers must null-check returned control pointers before modifying `flags` and check `ctrl_hdlr->error` after all controls are added before setting `ov02c10->sd.ctrl_handler`. [F15]
- In `ov02c10_probe()` and `ov02c10_check_hwcfg()`, the `ret` variable holding the failure status must not be overwritten along the `probe_error_*` or `check_hwcfg_error` cleanup labels before `return ret;`. [F18]
- Probe-time resource acquisition and fwnode endpoint checks that may defer (`ov02c10_check_hwcfg()`, `ov02c10_get_pm_resources()`, and clock/power acquisition in `ov02c10_probe()`) must use `dev_err_probe()`, while runtime hardware or I2C failures in `ov02c10_enable_streams()` and `ov02c10_power_on()` must log with `dev_err()` rather than `WARN_ON()`. [F63]

## External Contracts

- In `ov02c10_set_format()`, when `fmt->which == V4L2_SUBDEV_FORMAT_TRY`, the function must only update `fmt->format` and `*v4l2_subdev_state_get_format(sd_state, fmt->pad)` and return `0` without modifying active device controls (`ov02c10->vblank` or `ov02c10->hblank`). [F61]
- `ov02c10_enum_mbus_code()` and `ov02c10_enum_frame_size()` must validate untrusted UAPI `code->index` (`> 0`), `fse->index` (`>= ARRAY_SIZE(supported_modes)`), and `fse->code` (`!= MEDIA_BUS_FMT_SGRBG10_1X10`) and return `-EINVAL` before reading `supported_modes[fse->index]`. [F62]
- The optional reset GPIO is managed in logical active-high reset state (requested with `GPIOD_OUT_HIGH` in `ov02c10_get_pm_resources()` and set to `1` in `ov02c10_power_off()`), and `ov02c10_power_on()` must wait 2000–2200 us after enabling clock and regulators before deasserting reset to `0` and waiting 5000–5100 us. [F50]

## Known Pitfalls

- In `ov02c10_init_controls()`, `pixel_rate` must be computed using `div_u64()` on the 64-bit product `link_freq_menu_items[ov02c10->link_freq_index] * 2 * ov02c10->mipi_lanes` divided by `OV02C10_RGB_DEPTH` rather than native `/` or 32-bit multiplication. [F39]
