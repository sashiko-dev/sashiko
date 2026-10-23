# QUALCOMM CPUFREQ DRIVER MSM8996/APQ8096

This platform driver reads Qualcomm SoC IDs from SMEM and speed-bin/PVS fuse data from NVMEM to configure supported hardware versions, OPP property names, and power domains for CPU devices across Kryo, Krait, and IPQ SoCs. Once OPP tables and optional power domains are configured per CPU, it registers the generic `cpufreq-dt` platform device to perform actual frequency scaling.

## Architecture & Data Flow

- Every SoC compatible entry in `qcom_cpufreq_match_list` that binds to `match_data_kryo` must have all of its runtime SMEM `msm_id` values handled in `qcom_cpufreq_kryo_name_version()`, because the `default` branch of `switch (msm_id)` calls `BUG()`. [F25]
- `qcom_cpufreq_probe()` must allocate `struct qcom_cpufreq_drv` with `struct_size(drv, cpus, num_possible_cpus())` so `drv->cpus[]` can be indexed directly by CPU ID, while per-CPU loops in `qcom_cpufreq_probe()`, `qcom_cpufreq_remove()`, and `qcom_cpufreq_suspend()` must iterate with `for_each_present_cpu()` (not `for_each_possible_cpu()`, as `get_cpu_device()` returns `NULL` for non-present possible CPUs). [F48, F63]
- In any `struct qcom_cpufreq_match_data` with a non-NULL `pd_names` array (such as `match_data_msm8909` and `match_data_qcs404`), `num_pd_names` must equal the number of elements in `pd_names` passed to `dev_pm_domain_attach_list()` in `qcom_cpufreq_probe()`. [F26]
- In `get_krait_bin_format_a()`, the fallback checks for `*speed == 0xf` and `*pvs == 0x7` must remain two sequential `if` statements (not `else if`), so that if the backup fuse bits (`pte_efuse >> 4` or `pte_efuse >> 13`) also equal `0xf` or `0x7`, the second `if` resets `*speed` or `*pvs` to `0`. [F36]
- In `qcom_cpufreq_krait_name_version()`, `pvs_ver` must be initialized to `0` before `switch (len)` because the `case 4:` path calls `get_krait_bin_format_a()` (which does not set `pvs_ver`) and then passes `pvs_ver` to `snprintf(*pvs_name, ..., "speed%d-pvs%d-v%d", speed, pvs, pvs_ver)`. [F39]
- In `qcom_cpufreq_krait_name_version()`, the `speed` shift exponent in `drv->versions = (1 << speed)` must remain bounded to `< 31` (`get_krait_bin_format_a()` and `get_krait_bin_format_b()` mask `*speed` with `0xf` or `0x7` or reset it to `0` from `u32 pte_efuse`). [F44]
- In `qcom_cpufreq_ipq8064_name_version()`, `drv->versions` must be derived from the SMEM `msm_id` (`IPQ8062_VERSION`, `IPQ8064_VERSION`, or `IPQ8065_VERSION`) and `*pvs_name` must be formatted as `"speed0-pvs%d"`, ignoring the `speed` value extracted by `get_krait_bin_format_a()`. [F23]
- In `qcom_cpufreq_ipq6018_name_version()`, `QCOM_ID_IPQ6000` (and unknown `msm_id` values in `default:`) must set `drv->versions = IPQ6000_VERSION` (`BIT(2)`) rather than shifting `*speedbin`, because the 1-bit IPQ6018 speed-bin fuse cannot represent IPQ6000's 1.2 GHz limit. [F41]

## Object Lifetimes

- In `qcom_cpufreq_probe()` and `qcom_cpufreq_init()`, Device Tree node pointers `np` are declared with `__free(device_node)` to release their reference counts automatically on scope exit, so no return path in either function may call `of_node_put(np)` manually. [F2]
- In `qcom_cpufreq_probe()`, per-CPU OPP configurations (`dev_pm_opp_set_config()`) and PM domain attachments (`dev_pm_domain_attach_list()`) must be completed for all present CPUs before registering `cpufreq_dt_pdev`, and `qcom_cpufreq_remove()` must unregister `cpufreq_dt_pdev` before detaching PM domains and clearing OPP configs. [F50]
- Because SMEM and NVMEM providers may return `-EPROBE_DEFER`, `qcom_cpufreq_init()` must only match the root DT node and register `qcom_cpufreq_driver` and `cpufreq_pdev` (unrolling driver registration on failure), while `qcom_cpufreq_exit()` must unregister `cpufreq_pdev` before `qcom_cpufreq_driver`. [F51]

## API Contracts

- `qcom_cpufreq_probe()` must initialize `pvs_name` to point to the writable 18-byte stack buffer `pvs_name_buffer` (`sizeof("speedXX-pvsXX-vXX")`) before calling `drv->data->get_version()`, and every `get_version` callback must either format a NUL-terminated string into `*pvs_name` bounded by `sizeof(pvs_name_buffer)` (as in `qcom_cpufreq_krait_name_version()` and `qcom_cpufreq_ipq8064_name_version()`) or explicitly set `*pvs_name = NULL`. [F20, F47]
- `qcom_cpufreq_probe()` owns the `struct nvmem_cell *speedbin_nvmem` reference acquired from `of_nvmem_cell_get()` and lends it as a borrowed reference to `drv->data->get_version()`, releasing it with `nvmem_cell_put(speedbin_nvmem)` on both success and error returns of `get_version()`. Broken by: a `get_version` callback calling `nvmem_cell_put()`, or an error return in `qcom_cpufreq_probe()` skipping it. [F3]
- In `struct qcom_cpufreq_match_data`, `get_version` and `pd_names` are independently optional: `qcom_cpufreq_probe()` must guard NVMEM cell lookup and `dev_pm_opp_set_config()` on `drv->data->get_version` (and only set `config.prop_name` when `pvs_name` is non-NULL) while guarding `dev_pm_domain_attach_list()` on `drv->data->pd_names`. [F57]
- `drv->versions` is passed to `dev_pm_opp_set_config()` as a 32-bit `supported_hw` bitmask (`config.supported_hw_count = 1`), so enum constants in `enum ipq806x_versions` and `enum ipq8074_versions` must be shifted with `BIT()`, whereas `IPQ6000_VERSION` is already defined as `BIT(2)` and must be assigned without an extra shift. [F17]
- Callers of `get_krait_bin_format_a()` and `get_krait_bin_format_b()` must first validate the NVMEM cell byte length `len` returned by `nvmem_cell_read()` (`len == 4` for `get_krait_bin_format_a()` which reads `*((u32 *)buf)`, and `len == 8` for `get_krait_bin_format_b()` which reads `*((u32 *)buf)` and `*(((u32 *)buf) + 1)`). [F61]
- When attaching CPU power domains via `dev_pm_domain_attach_list()` in `qcom_cpufreq_probe()`, `.pd_flags` must include `PD_FLAG_DEV_LINK_ON | PD_FLAG_REQUIRED_OPP`, and `qcom_cpufreq_suspend()` must call `device_set_awake_path()` on each `pd_list->pd_devs[i]` across all present CPUs. [F62]

## Error Handling

- `qcom_cpufreq_drv_cpu.opp_token` must remain a signed `int` and `drv` must be zero-allocated (`devm_kzalloc()`) because `dev_pm_opp_set_config()` returns negative errnos on failure and `free_opp` / `qcom_cpufreq_remove()` pass every present CPU's `opp_token` and `pd_list` to `dev_pm_opp_clear_config()` and `dev_pm_domain_detach_list()`. Broken by: unsigned `opp_token`, unzeroed `drv`, or storing an `ERR_PTR` into `drv->cpus[cpu].pd_list`. [F15]
- `nvmem_cell_read()` returns an `ERR_PTR()` on failure and a heap-allocated buffer on success, so every caller must check `IS_ERR(speedbin)` and return `PTR_ERR(speedbin)` before dereferencing, and must `kfree(speedbin)` on all subsequent return paths (including `len_error` and `exit`). [F18]
- In `get_krait_bin_format_a()` and `get_krait_bin_format_b()`, unblown speed or PVS fuses (all-ones `0xf`/`0x7` in format A, or cleared blow-status `BIT(3)`/`BIT(21)` in format B) must log `dev_warn()` and default `*speed` or `*pvs` to `0` rather than returning an error. [F69]
- In `qcom_cpufreq_ipq8064_name_version()`, `qcom_cpufreq_ipq6018_name_version()`, and `qcom_cpufreq_ipq8074_name_version()`, an unrecognized SMEM `msm_id` must log `dev_err()`, set `drv->versions` to the family's lowest-frequency fallback version bitmask, and return `0`, whereas `qcom_cpufreq_kryo_name_version()` calls `BUG()`. [F70]

## External Contracts

- Every root machine compatible string added to `qcom_cpufreq_match_list` in `drivers/cpufreq/qcom-cpufreq-nvmem.c` must also be listed in `blocklist[]` in `drivers/cpufreq/cpufreq-dt-platdev.c`. [F67]
- `qcom_cpufreq_probe()` must verify that CPU 0's OPP descriptor node (`dev_pm_opp_of_get_opp_desc_node()`) matches either `"operating-points-v2-kryo-cpu"` or `"operating-points-v2-krait-cpu"` via `of_device_is_compatible()` before proceeding. [F59]
- `qcom_cpufreq_match_list` must remain terminated by an empty `{}` sentinel entry and exported via `MODULE_DEVICE_TABLE(of, qcom_cpufreq_match_list)`. [F60]

## Known Pitfalls

- Because `qcom_cpufreq_match_list` is marked `__initconst` while `qcom_cpufreq_probe()` may be deferred past `__init` memory teardown, `qcom_cpufreq_init()` must copy the matched `struct of_device_id` via `platform_device_register_data(..., match, sizeof(*match))`, and all `struct qcom_cpufreq_match_data` targets must remain non-`__initconst`. [F58]
- In `qcom_cpufreq_kryo_name_version()`, `u32 drv->versions` is populated via `1 << *speedbin` (or `1 << (*speedbin + 4)`), so any SoC whose NVMEM `*speedbin` byte can be >= 31 (or >= 27 on SG variants) must map `*speedbin` explicitly (as `QCOM_ID_IPQ5424` does for `0x3b`) rather than shifting by `*speedbin`. [F45]
- In `get_krait_bin_format_b()`, all fields from the first 32-bit fuse word (`redundant_sel`, `*pvs_ver`, `*pvs`, `*speed`, and `SPEED_BIN_BLOW_STATUS` at `BIT(3)`) must be evaluated before `pte_efuse` is overwritten with `*(((u32 *)buf) + 1)` and masked with `BIT(21)` for `PVS_BLOW_STATUS`. [F37]
