# MEDIATEK ETHERNET PCS DRIVER

This unit implements the `pcs-mtk-lynxi` phylink PCS driver library for MediaTek LynxI SGMII, 1000BASE-X, and 2500BASE-X Ethernet Physical Coding Sublayer blocks used by MediaTek Ethernet MACs (`mtk_eth_soc`) and MT7531 DSA switches (`mt7530-mdio`). It provides lifecycle helpers (`mtk_pcs_lynxi_create()` and `mtk_pcs_lynxi_destroy()`) and exposes `mtk_pcs_lynxi_ops` callbacks for link state polling, mode configuration, autonegotiation restart, link-up speed/duplex forcing, and disablement over `regmap`.

## Architecture & Data Flow

- `SGMSYS_PCS_CONTROL_1` and `SGMSYS_PCS_ADVERTISE` each pack two 16-bit Clause 22 MII registers into one 32-bit `regmap` word (`SGMII_BMCR`/`SGMII_ADVERTISE` in bits 15:0, `SGMII_BMSR`/`SGMII_LPA` in bits 31:16); reads of `BMSR` or `LPA` in `mtk_pcs_lynxi_get_state()` must extract bits 31:16 via `FIELD_GET()`, and writes to `BMCR` or `ADVERTISE` must mask to bits 15:0. [F22]
- `mtk_pcs_lynxi_config()` and `mtk_pcs_lynxi_link_up()` must update disjoint bitmasks of `SGMSYS_SGMII_MODE` via `regmap_update_bits()` (`SGMII_REMOTE_FAULT_DIS | SGMII_SPEED_DUPLEX_AN | SGMII_IF_MODE_SGMII` in `mtk_pcs_lynxi_config()` versus `SGMII_DUPLEX_HALF | SGMII_SPEED_MASK` in `mtk_pcs_lynxi_link_up()`). Broken by: using `regmap_write()` on `SGMSYS_SGMII_MODE` in either callback. [F38]
- In `mtk_pcs_lynxi_config()`, `SGMII_IF_MODE_SGMII` and `SGMII_SPEED_DUPLEX_AN` in `sgm_mode` must only be set when `interface == PHY_INTERFACE_MODE_SGMII` and must be cleared via `regmap_update_bits()` on `SGMSYS_SGMII_MODE` when configuring `PHY_INTERFACE_MODE_1000BASEX` or `PHY_INTERFACE_MODE_2500BASEX`. [F25]
- `mpcs->interface` must be initialized to `PHY_INTERFACE_MODE_NA` in `mtk_pcs_lynxi_create()` and reset to `PHY_INTERFACE_MODE_NA` in `mtk_pcs_lynxi_disable()`, because `mtk_pcs_lynxi_config()` only runs the PHYA power-down, `SGMII_SW_RESET`, `ana_rgc3` speed, and `SGMSYS_PCS_LINK_TIMER` setup when `mpcs->interface != interface`. [F16]
- The `phy_interface_t` modes populated in `mpcs->pcs.supported_interfaces` by `mtk_pcs_lynxi_create()` (`PHY_INTERFACE_MODE_SGMII`, `PHY_INTERFACE_MODE_1000BASEX`, `PHY_INTERFACE_MODE_2500BASEX`) must be handled in lockstep by `mtk_pcs_lynxi_inband_caps()` and the `sgm_mode`/`rgc3` selection branches in `mtk_pcs_lynxi_config()`. [F21]

## Synchronization

- `mtk_pcs_lynxi_get_state()`, `mtk_pcs_lynxi_config()`, `mtk_pcs_lynxi_restart_an()`, and `mtk_pcs_lynxi_link_up()` must never run in atomic context or hold a spinlock/`rcu_read_lock` across `mpcs->regmap` calls or `mtk_pcs_lynxi_config()`, because `mtk_pcs_lynxi_config()` unconditionally calls `usleep_range(50, 100)` before writing `SGMSYS_QPHY_PWR_STATE_CTRL` and `mpcs->regmap` on MT7531 DSA switches is backed by a sleeping MDIO bus mutex (`mt7530_mdio_regmap_lock`). [F39, F41]
- `struct mtk_pcs_lynxi` has no internal lock protecting `mpcs->interface` or multi-register configuration sequences across `mtk_pcs_lynxi_ops` callbacks; callers must serialize `mtk_pcs_lynxi_config()` and `mtk_pcs_lynxi_disable()` externally (as `phylink` does), while individual register accesses rely on `mpcs->regmap` locking. [F6]

## Object Lifetimes

- `mtk_pcs_lynxi_create()` stores `regmap` in `mpcs->regmap` as a borrowed pointer without taking a reference; the caller must keep the underlying `struct regmap` valid across all `mtk_pcs_lynxi_ops` callbacks until the `phylink_pcs` instance is inactive and destroyed. [F2]

## API Contracts

- In `mtk_pcs_lynxi_config()` and `mtk_pcs_lynxi_link_up()`, `neg_mode` is a `PHYLINK_PCS_NEG_*` constant rather than `MLO_AN_*`: `PHYLINK_PCS_NEG_INBAND_ENABLED` (`PHYLINK_PCS_NEG_INBAND | PHYLINK_PCS_NEG_ENABLED`) must be tested with `==` or `!=` rather than bitwise `&` (forcing speed and duplex in `mtk_pcs_lynxi_link_up()` whenever `neg_mode != PHYLINK_PCS_NEG_INBAND_ENABLED`), whereas `neg_mode & PHYLINK_PCS_NEG_INBAND` tests for any in-band mode regardless of autonegotiation. Broken by: passing `neg_mode` to `phylink_autoneg_inband()` or testing `PHYLINK_PCS_NEG_INBAND_ENABLED` with `&`. [F23, F47]
- In `mtk_pcs_lynxi_inband_caps()`, `PHY_INTERFACE_MODE_2500BASEX` must be handled in a separate `switch` case returning only `LINK_INBAND_DISABLE`, while `PHY_INTERFACE_MODE_1000BASEX` and `PHY_INTERFACE_MODE_SGMII` return `LINK_INBAND_DISABLE | LINK_INBAND_ENABLE` and all other modes return `0`. [F24]
- `mtk_pcs_lynxi_config()` must return a positive non-zero value (`changed || mode_changed`) whenever `SGMSYS_PCS_ADVERTISE` (updated via `regmap_update_bits_check()`) or `mpcs->interface` changes, `0` when neither changes, and a negative errno on failure. [F46]
- `mtk_pcs_lynxi_destroy()` only checks for a `NULL` `pcs` pointer before downcasting via `pcs_to_mtk_pcs_lynxi()` (`container_of(pcs, struct mtk_pcs_lynxi, pcs)`) and calling `kfree()`; `mtk_pcs_lynxi_create()` must return `NULL` (never an `ERR_PTR`) on failure, and `mtk_pcs_lynxi_destroy()` must only be passed an embedded `&mpcs->pcs` from `mtk_pcs_lynxi_create()`. [F29]
- `mtk_pcs_lynxi_ops` must always populate `.pcs_config`, `.pcs_get_state`, and `.pcs_an_restart` because `phylink_pcs_config()`, `phylink_mac_pcs_get_state()`, and `phylink_pcs_an_restart()` invoke them without NULL checks. [F43]

## Error Handling

- `mtk_pcs_lynxi_config()` must validate both `phylink_mii_c22_pcs_encode_advertisement()` and (when `mpcs->interface != interface`) `phylink_get_link_timer_ns()` for negative error codes before setting `SGMII_PHYA_PWD` in `SGMSYS_QPHY_PWR_STATE_CTRL`, resetting the PCS, or updating `mpcs->interface`. Broken by: moving either check after `regmap_set_bits(mpcs->regmap, SGMSYS_QPHY_PWR_STATE_CTRL, SGMII_PHYA_PWD)` with an early return that leaves PHYA powered down. [F20]

## Known Pitfalls

- When `mtk_pcs_lynxi_config()` releases PHYA power-down after configuring the PCS, it must sleep (`usleep_range(50, 100)`) and write `0` to the entire `SGMSYS_QPHY_PWR_STATE_CTRL` register via `regmap_write()` rather than only clearing `SGMII_PHYA_PWD` with `regmap_clear_bits()`. [F17]
- In `mtk_pcs_lynxi_link_up()`, the forced-speed conditional chain (`SPEED_10`, `SPEED_100`, `else SGMII_SPEED_1000`) when `neg_mode != PHYLINK_PCS_NEG_INBAND_ENABLED` must default all speeds other than 10 and 100 (including `SPEED_1000` and `SPEED_2500`) to `SGMII_SPEED_1000`. Broken by: replacing the `else` branch with an explicit `SPEED_1000` check that skips `SPEED_2500`. [F26]
- `mtk_pcs_lynxi_config()` must convert the nanosecond link timer duration from `phylink_get_link_timer_ns()` into 16 ns (`2 * 8ns`) hardware units via `SGMII_LINK_TIMER_VAL(ns)` before writing `SGMSYS_PCS_LINK_TIMER`, and any non-constant `ns` passed to `SGMII_LINK_TIMER_VAL(ns)` must be verified non-negative and `< 16777216` (`(SGMII_LINK_TIMER_MASK + 1) * 2 * 8`) because runtime `FIELD_PREP(SGMII_LINK_TIMER_MASK, (ns) / 2 / 8)` masks with `GENMASK(19, 0)` without overflow or sign checking. [F28, F50]
