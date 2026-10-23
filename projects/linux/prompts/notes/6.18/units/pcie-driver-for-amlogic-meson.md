# PCIE DRIVER FOR AMLOGIC MESON

This component is the DesignWare PCIe Root Complex host controller platform driver for Amlogic Meson SoCs (AXG and G12A). It manages Meson-specific hardware initialization—including PHY power/reset, shared and exclusive reset lines, clocks, PERST# GPIO toggling, and LTSSM link startup/polling—while delegating standard Root Port setup to the DesignWare PCIe core (`dw_pcie_host_init`). It also provides a custom root bus config-space read accessor (`meson_pcie_rd_own_conf`) to work around a hardware erratum where `PCI_CLASS_DEVICE` cannot be programmed in the controller.

## Architecture & Data Flow

- `meson_pcie_probe` must assign `pci->dev = dev` before calling `meson_pcie_get_resets` or `meson_pcie_probe_clocks`, and must call `platform_set_drvdata(pdev, mp)` before `dw_pcie_host_init(&pci->pp)` invokes callbacks (`meson_pcie_host_init`, `meson_pcie_start_link`, `meson_pcie_link_up`) that dereference `to_meson_pcie(pci)`. [F12]
- When `(where & ~3) == PCI_CLASS_REVISION`, `meson_pcie_rd_own_conf` must align sub-dword (`size <= 2`) reads from `pci_generic_config_read` into dword position (`<< (8 * (where & 3))`), replace only bits `[31:8]` (`~0xffffff00`) with `PCI_CLASS_BRIDGE_PCI_NORMAL << 8`, and shift/mask back to `size` bytes so the revision byte in bits `[7:0]` is preserved. [F19]
- `meson_set_max_payload` and `meson_set_max_rd_req_size` both modify `offset + PCI_EXP_DEVCTL` via `dw_pcie_readl_dbi`/`dw_pcie_writel_dbi` and must mask only `PCI_EXP_DEVCTL_PAYLOAD` and `PCI_EXP_DEVCTL_READRQ` respectively so sequential calls in `meson_pcie_host_init` do not clobber each other or other `PCI_EXP_DEVCTL` bits. [F20]
- `meson_pcie_ltssm_enable` must enable LTSSM via a read-modify-write on `PCIE_CFG0` (`meson_cfg_readl` OR-ed with `APP_LTSSM_ENABLE` then `meson_cfg_writel`) to preserve other configuration bits when `meson_pcie_start_link` starts the link. [F21]
- `meson_pcie_link_up` polls `PCIE_CFG_STATUS12` and `PCIE_CFG_STATUS17` for at most `WAIT_LINKUP_TIMEOUT` (4000) iterations with `udelay(10)` per iteration, returning `true` only when `IS_SMLH_LINK_UP()`, `IS_RDLH_LINK_UP()`, `IS_LTSSM_UP()`, and `PM_CURRENT_STATE(state17) < PCIE_GEN3` are all satisfied; `speed_okay` must be initialized to `0` before the loop because the loop only sets `speed_okay = 1` and never clears it on subsequent iterations. [F14, F44]
- In `meson_cfg_readl()` and `meson_cfg_writel()`, `mp->cfg_base` is a byte-addressed `void __iomem *` pointer, so `reg` must always be a byte offset (such as `PCIE_CFG0`, `PCIE_CFG_STATUS12`, or `PCIE_CFG_STATUS17`) and `cfg_base` must not be retyped to `u32 __iomem *` without dividing offsets by 4. [F34]
- `meson_pcie_of_match` must remain terminated by an empty `{}` sentinel entry. [F24]

## Synchronization

- `meson_pcie_rd_own_conf` and `meson_pcie_link_up` must not sleep or acquire sleeping locks because both execute in atomic context under the PCI core `pci_lock` `raw_spinlock_t` during PCI configuration space accesses. Broken by: replacing `udelay(10)` with `usleep_range`/`msleep` or acquiring a mutex. [F8]
- `meson_pcie_start_link` and `meson_pcie_assert_reset` must only be called in sleepable process context with no spinlocks held because `meson_pcie_assert_reset` invokes `gpiod_set_value_cansleep`. [F9]

## Object Lifetimes

- In `meson_pcie_power_on`, if `phy_power_on(mp->phy)` fails after `phy_init(mp->phy)` succeeds, `phy_exit(mp->phy)` must be called before returning the error, and `meson_pcie_power_off` must tear down the PHY in reverse order (`phy_power_off` followed by `phy_exit`). [F1]
- In `meson_pcie_probe_clock`, `devm_add_action_or_reset(dev, meson_pcie_disable_clock, clk)` must be registered only after `clk_prepare_enable(clk)` succeeds so `clk_disable_unprepare` is automatically invoked on subsequent failure. [F3]

## API Contracts

- In `meson_pcie_get_resets`, the `"apb"` reset (`mrst->apb`) must be acquired with `PCIE_SHARED_RESET` (`devm_reset_control_get_shared`) and deasserted via `reset_control_deassert(mrst->apb)` before `meson_pcie_reset` calls `reset_control_assert(mrst->apb)`, because asserting a shared reset requires a positive `deassert_count`. [F17]
- `meson_pcie_host_init` must assign `pp->bridge->ops = &meson_pci_ops` during the `dw_pcie_host_ops.init` callback so it overrides the default `pp->bridge->ops` set by `dw_pcie_host_get_resources` before `pci_host_probe` enumerates the root bus. Broken by: assigning `pp->bridge->ops` in `meson_pcie_probe` before `dw_pcie_host_init` allocates `pp->bridge`. [F48]
- `meson_pci_ops` must provide `.map_bus = dw_pcie_own_conf_map_bus` alongside `.read = meson_pcie_rd_own_conf` and `.write = pci_generic_config_write` because `pci_generic_config_read` and `pci_generic_config_write` unconditionally call `bus->ops->map_bus`. [F49]
- `meson_pcie_rd_own_conf` must return `PCIBIOS_*` status codes (`PCIBIOS_SUCCESSFUL` or the status from `pci_generic_config_read`) rather than negative errno values, and must abort before fabricating `PCI_CLASS_REVISION` if `pci_generic_config_read` does not return `PCIBIOS_SUCCESSFUL`. [F11]
- `meson_set_max_payload` and `meson_set_max_rd_req_size` accept `size` in bytes (a power of two in `[128, 4096]`) and must convert it via `meson_size_to_payload` into the 3-bit `PCI_EXP_DEVCTL` encoding (`fls(size) - 8`, or `1` on invalid input) before shifting with `PCIE_CAP_MAX_PAYLOAD_SIZE` or `PCIE_CAP_MAX_READ_REQ_SIZE`. Broken by: passing a pre-encoded bitfield or returning a negative errno from `meson_size_to_payload` that gets ORed into `PCI_EXP_DEVCTL`. [F52]
- Cleanup callbacks passed to `devm_add_action_or_reset()` (such as `meson_pcie_disable_clock` in `meson_pcie_probe_clock()`) must have the exact `void (*)(void *)` signature rather than casting `clk_disable_unprepare`, and the `void *` data argument must be the `struct clk *` pointer that `meson_pcie_disable_clock()` passes to `clk_disable_unprepare()`. [F32, F33]

## Error Handling

- In `meson_pcie_probe`, failure paths before `meson_pcie_power_on(mp)` succeeds must return directly, whereas every failure path after `meson_pcie_power_on(mp)` succeeds (`meson_pcie_reset`, `meson_pcie_probe_clocks`, `dw_pcie_host_init`) must jump to `err_phy` to call `meson_pcie_power_off(mp)` without overwriting `ret`. [F13]
- `meson_pcie_probe_clock` must convert negative errno returns from `clk_set_rate` and `clk_prepare_enable` into error-encoded pointers via `ERR_PTR(ret)` (never `NULL`) so `meson_pcie_probe_clocks` can uniformly check `IS_ERR` and extract the status with `PTR_ERR`. [F10]
- `meson_pcie_link_up` treats link-training timeouts after `WAIT_LINKUP_TIMEOUT` iterations as a recoverable hardware state by logging `dev_err` and returning `false` without `WARN_ON` or `BUG_ON`, as `dw_pcie_wait_for_link` and `dw_pcie_other_conf_map_bus` poll `dw_pcie_link_up` when no endpoint is ready. [F53]

## External Contracts

- `meson_pcie_get_mems()` must map the `"elbi"` memory resource into `pci->dbi_base` and `"cfg"` into `mp->cfg_base` before `dw_pcie_host_init()` (so `dw_pcie_get_resources()` does not look up a non-existent `"dbi"` resource), and `meson_pcie_probe_clocks()` must request the exact DT clock names `"port"`, `"general"`, and `"pclk"`. [F40]
- `mp->reset_gpio` is requested with `GPIOD_OUT_LOW` (logical deasserted) in `meson_pcie_probe()`, and `meson_pcie_assert_reset()` must pulse reset by driving logical `1` (asserted), waiting `udelay(500)`, and driving logical `0` (deasserted) via `gpiod_set_value_cansleep()`, delegating physical `PERST#` polarity to the Device Tree. [F41]

## Known Pitfalls

- `meson_cfg_writel(mp, val, reg)` takes the 32-bit value before the register offset on `mp->cfg_base`, whereas `dw_pcie_writel_dbi(pci, reg, val)` takes the register offset before the 32-bit value on `pci->dbi_base`. [F16]
- In `meson_pcie_rd_own_conf`, the bitmask expression `(1 << (size * 8)) - 1` must remain gated by `size <= 2` so that a 4-byte config read (`size == 4`) never shifts a 32-bit `int` literal by 32 bits. [F31]
- In `meson_pcie_link_up`, `smlh_up` (`IS_SMLH_LINK_UP`, bit 6) and `rdlh_up` (`IS_RDLH_LINK_UP`, bit 16) hold raw unshifted bitmasks in `u32` variables while `ltssm_up` (`IS_LTSSM_UP`) and `speed_okay` hold boolean `0` or `1` values, so combining them requires logical `&&` rather than bitwise `&`. [F27]
- In `meson_pcie_link_up`, the `do { ... } while (cnt < WAIT_LINKUP_TIMEOUT)` polling loop increments `cnt` and calls `udelay(10)` at the end of the loop body, so every non-returning path in the loop must reach `cnt++` rather than using `continue`. [F28]
