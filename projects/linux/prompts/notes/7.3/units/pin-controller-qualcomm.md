# PIN CONTROLLER - QUALCOMM

This unit implements Qualcomm pin controller drivers in `drivers/pinctrl/qcom`, including the Low Power Island (LPASS LPI) core driver (`pinctrl-lpass-lpi.c` and `pinctrl-lpass-lpi.h`), LPASS LPI SoC variant drivers (SM8250, SM8350, SM8450, SM8550), and SoC-specific Top-Level Mode Multiplexer (TLMM) drivers (APQ8064, APQ8084, Glymur, IPQ4019, IPQ5018, IPQ5332, IPQ5424, IPQ6018, IPQ8064, IPQ8074, IPQ9574, MDM9607, MDM9615, Milos, SM6115, SM6125, SM6350, SM6375, SM7150, SM8150, SM8250, SM8350, SM8450) that register static pin, group, mux, tile, and wakeup IRQ tables via `msm_pinctrl_probe()`.

## Architecture & Data Flow

- In `struct lpi_pinctrl`, `slew_base` is only mapped in `lpi_pinctrl_probe()` when `LPI_FLAG_SLEW_RATE_SAME_REG` is not set in `lpi_pinctrl_variant_data::flags`; any code accessing slew-rate registers (such as `lpi_config_set_slew_rate()`) must check `LPI_FLAG_SLEW_RATE_SAME_REG` and use `tlmm_base` instead of dereferencing `slew_base`. [F10]
- In `struct lpi_pinctrl`, `tlmm_base` and `slew_base` are `char __iomem *` (1-byte stride), so `lpi_gpio_read()`, `lpi_gpio_write()`, and `lpi_config_set_slew_rate()` must pass unscaled byte offsets (`pin_offset`, `LPI_TLMM_REG_OFFSET * pin`, and register offsets) when adding to those base pointers. [F50]
- `lpi_config_set_slew_rate()` must reject `slew > LPI_SLEW_RATE_MAX` before executing `sval |= slew << slew_offset`, because `slew` is not masked to `LPI_SLEW_RATE_MASK` when ORed into `sval`. Broken by: increasing `LPI_SLEW_RATE_MAX` without widening `LPI_SLEW_RATE_MASK` or removing the bounds check. [F46]
- In `pinctrl-lpass-lpi.c`, `MAX_NR_GPIO` (which sizes the `ever_gpio` bitmap in `struct lpi_pinctrl`) must be at least as large as `data->npins` for every registered `struct lpi_pinctrl_variant_data`, because `lpi_pinctrl_probe()` rejects `data->npins > MAX_NR_GPIO` with `-EINVAL`. [F29]
- In every SoC TLMM and LPASS LPI driver, a pin group name `"gpio<id>"` must be listed in `<fname>_groups[]` if and only if function `<fname>` is present in `PINGROUP(id, ...)` (or `LPI_PINGROUP(id, ...)` / `LPI_PINGROUP_OFFSET(id, ...)`). [F25]
- Every enum constant in `enum <soc>_functions` or `enum lpass_lpi_functions` whose value is `< ARRAY_SIZE(<soc>_functions)` must have a designated initializer entry (`MSM_PIN_FUNCTION`, `MSM_GPIO_PIN_FUNCTION`, or `LPI_FUNCTION`) in `<soc>_functions[]`, and unindexed placeholders (`msm_mux__`, `LPI_MUX__`) must be last in the enum. [F91]
- In `LPI_PINGROUP`, `LPI_PINGROUP_OFFSET`, and each SoC `PINGROUP` macro, `.funcs` is initialized from an anonymous `(int[]){ ... }` compound literal that decays to `unsigned int *`, so `.nfuncs` must equal the exact element count of that compound literal; in TLMM `PINGROUP` definitions, `.nfuncs` must also stay between `2` and `16` because `msm_pinmux_set_mux()` computes `GENMASK(g->mux_bit + order_base_2(g->nfuncs) - 1, g->mux_bit)` with `.mux_bit = 2` and `.drv_bit = 6`. [F48, F115]
- In TLMM drivers (`pinctrl-sm6115.c` through `pinctrl-sm8450.c`), `<soc>_groups[]` must use dense 0-based indexing synchronized 1-to-1 with `<soc>_pins[]`, keeping dummy `PINGROUP` entries for missing pins because `pinctrl-msm.c` indexes `pctrl->soc->groups[offset]` directly by GPIO offset and `hwirq`. [F97]
- In multi-tile TLMM drivers (`pinctrl-sm6115.c`, `pinctrl-sm6125.c`, `pinctrl-sm7150.c`, `pinctrl-sm8150.c`, `pinctrl-sm8250.c`), tile enum values (`NORTH`, `SOUTH`, `EAST`, `WEST`) used in `struct msm_pingroup::tile` must match the 0-based order of `<soc>_tiles[]` and stay `< ntiles` (and `< MAX_NR_TILES`, `4`), and special pin macros (`SDC_PINGROUP`, `SDC_QDSD_PINGROUP`, `UFS_RESET`) must set `.tile` to the specific tile enum containing each pin's register offset rather than hardcoding a tile. [F99, F121]
- Register offsets passed to `PINGROUP`, `SDC_PINGROUP`, `SDC_QDSD_PINGROUP`, and `UFS_RESET` must be relative to the OS-accessible TLMM or tile base address mapped from Device Tree, and all pins of the same SDC controller (`clk`, `cmd`, `data`) sharing a control register must specify the identical `ctl` offset. [F122]
- In all wakeup IRQ mapping arrays (`sm6115_mpm_map`, `sm6125_mpm_map`, `sm6350_pdc_map`, `sm6375_mpm_map`, `sm7150_pdc_map`, `sm8150_pdc_map`, `sm8250_pdc_map`, `sm8350_pdc_map`, `sm8450_pdc_map`), every entry's `.gpio` field must be `< ngpios` (and `< MAX_NR_GPIO`, `300`). Broken by: swapping `.gpio` and `.wakeirq` in `struct msm_gpio_wakeirq_map` or adding `.gpio >= MAX_NR_GPIO`. [F102]

## Synchronization

- In `drivers/pinctrl/qcom/pinctrl-lpass-lpi.c`, every read-modify-write update to `LPI_GPIO_CFG_REG` or `LPI_SLEW_RATE_CTL_REG` (in `lpi_gpio_set_mux()`, `lpi_config_set_slew_rate()`, and `lpi_config_set()`) must be serialized under `pctrl->lock` because the pinctrl core does not serialize concurrent mux, config, and slew-rate callbacks. [F6]

## Object Lifetimes

- In `lpi_pinctrl_probe()`, `platform_set_drvdata(pdev, pctrl)` must be called before `devm_pinctrl_register()` and `devm_gpiochip_add_data()`, because `lpi_config_get()` and `lpi_config_set()` retrieve `struct lpi_pinctrl *` via `dev_get_drvdata(pctldev->dev)` and `lpi_pinctrl_remove()` retrieves it via `platform_get_drvdata(pdev)`. [F1]
- In `lpi_pinctrl_probe()`, `pctrl->ctrl = devm_pinctrl_register(...)` must complete before `lpi_build_pin_desc_groups()` and `devm_gpiochip_add_data()`, because `lpi_build_pin_desc_groups()` passes `pctrl->ctrl` to `pinctrl_generic_add_group()` and the `gpio_chip` callbacks (`lpi_gpio_direction_input()`, `lpi_gpio_direction_output()`) pass `state->ctrl` to `lpi_config_set()` and dereference `pctldev->desc`. [F2]
- In `lpi_build_pin_desc_groups()`, `pinctrl_generic_add_group()` stores the `pins` pointer directly without copying, so `pins` must point to `&pin_info->number` inside the static `pctrl->desc.pins` array and `num_pins` must remain `1`. Broken by: passing the address of a stack-local pin index variable or passing `num_pins > 1`. [F51]
- SoC descriptor tables (`struct msm_pinctrl_soc_data`, `struct lpi_pinctrl_variant_data`, and their nested pin, group, function, tile, or wakeirq arrays) must remain non-`__initconst` / non-`__initdata` static storage because `msm_pinctrl_probe()` and `lpi_pinctrl_probe()` store borrowed pointers to them in `pctrl->soc` and `pctrl->data` for runtime and remove use. [F78]
- Every LPASS LPI driver (`pinctrl-sm8250-lpass-lpi.c` through `pinctrl-sm8550-lpass-lpi.c`) must set `.data` to its `struct lpi_pinctrl_variant_data` in `lpi_pinctrl_of_match[]` and wire both `.probe = lpi_pinctrl_probe` and `.remove = lpi_pinctrl_remove` on `lpi_pinctrl_driver`. [F128]

## API Contracts

- Every `lpi_pingroup` definition (via `LPI_PINGROUP` or `LPI_PINGROUP_OFFSET`) must place `LPI_MUX_gpio` at index `0` (`GPIO_FUNC`) of `lpi_pingroup::funcs`, because `lpi_gpio_set_mux()` writes the matched array index `i` directly into `LPI_GPIO_FUNCTION_MASK` and tests `i == GPIO_FUNC` for glitch-free GPIO output initialization. [F17]
- The `strength` variable in `lpi_config_set()` is expressed in milliamps (default `2` mA) and must be converted with `LPI_GPIO_DS_TO_VAL()` (`v / 2 - 1`) before writing `LPI_GPIO_OUT_STRENGTH_MASK`, whereas register reads must be converted back to milliamps with `lpi_regval_to_drive()` (`(val + 1) * 2`). Broken by: initializing `strength` to `0` or passing an already-encoded register value to `LPI_GPIO_DS_TO_VAL()`. [F16]
- In TLMM drivers defining `struct msm_pinctrl_soc_data`, all GPIO-capable pingroups (`PINGROUP` and `UFS_RESET`) must be placed at `groups[0 .. ngpios - 1]` indexed by GPIO number, while non-GPIO pingroups (`SDC_PINGROUP`, `SDC_QDSD_PINGROUP`) must be placed at indices `>= ngpios`. Broken by: setting `.ngpios = ARRAY_SIZE(<soc>_groups)` when `SDC_PINGROUP` or `SDC_QDSD_PINGROUP` entries are present. [F62]
- In TLMM drivers, the `gpio` entry in `<soc>_functions[]` (and `egpio` in `sm8450_functions[]`) must use `MSM_GPIO_PIN_FUNCTION()` rather than `MSM_PIN_FUNCTION()` so `PINFUNCTION_FLAG_GPIO` is set when `pin_request()` evaluates `ops->function_is_gpio()` under `msm_pinmux_ops.strict = true`. [F107]
- Any `reserved_gpios` array assigned to `struct msm_pinctrl_soc_data.reserved_gpios` (such as `ipq9574_reserved_gpios`) must be terminated by a negative sentinel (`-1`) and contain at most `ngpios` entries with every GPIO number `< ngpios`. [F64]

## Error Handling

- In `lpi_build_pin_desc_groups()`, the return value of `pinctrl_generic_add_group()` must be checked with `ret < 0` rather than `ret != 0` because it returns a non-negative group selector index on success, and if it fails at index `i`, the rollback loop must iterate `for (; i > 0; i--)` and remove group `i - 1` so only the `0 .. i - 1` added groups are removed without underflowing `0`. [F12, F32]
- `lpi_gpio_dbg_show_one()` must normalize a `NULL` `pctldev` argument to `state->ctrl` (`pctldev = pctldev ? : state->ctrl;`) before dereferencing `pctldev->desc->pins[offset]`, because `lpi_gpio_dbg_show()` invokes `lpi_gpio_dbg_show_one()` with `pctldev = NULL`. [F15]

## External Contracts

- In TLMM drivers (`pinctrl-sm6115.c` through `pinctrl-sm8450.c`), `struct msm_pinctrl_soc_data` must populate `.wakeirq_map` and `.nwakeirq_map` with the SoC's exact `{ gpio, wakeirq }` MPM or PDC interrupt mappings, and set `.wakeirq_dual_edge_errata = true` when the parent wakeup controller cannot detect both edges in hardware. [F125]

## Known Pitfalls

- In `SDC_PINGROUP` and `SDC_QDSD_PINGROUP` invocations across the SM6xxx/SM7xxx/SM8xxx TLMM drivers, `pull` must be in `[0, 30]` and `drv` must be in `[0, 29]` (never `-1`), because `pull_bit` and `drv_bit` are 5-bit unsigned bitfields shifted as `3 << bit` and `7 << bit` into a 32-bit `u32` register value. [F113]
