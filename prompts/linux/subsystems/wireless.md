# Wireless Subsystem Details

## mac80211 MLO Callback Structure

Incorrect placement of `BSS_CHANGED_*` event handling between mac80211
callbacks causes `WARN_ON_ONCE` splats at runtime, functional regressions
(association failures, beacon processing errors), or silent hardware
misbehavior that is difficult to diagnose.

mac80211 Multi-Link Operation (MLO) support splits the legacy
`bss_info_changed()` callback into two callbacks in `struct ieee80211_ops`
(defined in `include/net/mac80211.h`):

- `vif_cfg_changed()`: handles VIF-global configuration changes from
  `struct ieee80211_vif_cfg` that apply to the entire virtual interface
- `link_info_changed()`: handles per-link configuration changes from
  `struct ieee80211_bss_conf` for an individual link

If neither `vif_cfg_changed()` nor `link_info_changed()` is implemented,
mac80211 falls back to calling `bss_info_changed()` for all events.

The authoritative split is defined by `BSS_CHANGED_VIF_CFG_FLAGS` in
`net/mac80211/main.c`. Flags listed in that macro are VIF-global; all
others are link-specific. Passing a VIF-global flag to
`ieee80211_link_info_change_notify()` triggers `WARN_ON_ONCE`, and passing
a link-specific flag to `ieee80211_vif_cfg_change_notify()` also triggers
`WARN_ON_ONCE`.

**VIF-global events** (belong in `vif_cfg_changed()`):

| Event | Data location |
|-------|---------------|
| `BSS_CHANGED_ASSOC` | `vif->cfg.assoc` |
| `BSS_CHANGED_IDLE` | `vif->cfg.idle` |
| `BSS_CHANGED_PS` | `vif->cfg.ps` |
| `BSS_CHANGED_IBSS` | `vif->cfg.ibss_joined` |
| `BSS_CHANGED_ARP_FILTER` | `vif->cfg.arp_addr_list` |
| `BSS_CHANGED_SSID` | `vif->cfg.ssid` |
| `BSS_CHANGED_MLD_VALID_LINKS` | `vif->valid_links` |
| `BSS_CHANGED_MLD_TTLM` | MLD TID-to-link mapping |

Column: "Data location" shows the `struct ieee80211_vif` field that
carries the changed value.

**Link-specific events** (belong in `link_info_changed()`):

| Event | Rationale |
|-------|-----------|
| `BSS_CHANGED_BSSID` | BSSID is a per-link attribute in `struct ieee80211_bss_conf` |
| `BSS_CHANGED_BEACON_INFO` | Beacon parameters (e.g. `dtim_period`) are per-link |
| `BSS_CHANGED_BEACON` | Beacon data is per-link |
| `BSS_CHANGED_BEACON_INT` | Beacon interval is per-link |
| `BSS_CHANGED_ERP_CTS_PROT` | ERP protection is per-link |
| `BSS_CHANGED_ERP_PREAMBLE` | Preamble type is per-link |
| `BSS_CHANGED_ERP_SLOT` | Slot timing is per-link |
| `BSS_CHANGED_HT` | HT parameters are per-link |
| `BSS_CHANGED_BASIC_RATES` | Basic rates are per-link |
| `BSS_CHANGED_TXPOWER` | TX power is per-link |
| `BSS_CHANGED_BANDWIDTH` | Channel bandwidth is per-link |
| `BSS_CHANGED_HE_BSS_COLOR` | BSS color is per-link |
| `BSS_CHANGED_TPE` | Transmit power envelope is per-link |

## Quick Checks

- **VIF vs. link callback classification**: the canonical source is
  `BSS_CHANGED_VIF_CFG_FLAGS` in `net/mac80211/main.c`; any flag in that
  macro is VIF-global, everything else is link-specific
- **MLO event migration**: when converting a legacy `bss_info_changed()`
  driver to MLO, each `BSS_CHANGED_*` flag must be routed to the correct
  callback; misrouting triggers `WARN_ON_ONCE` at runtime
- **Power save is VIF-global**: `BSS_CHANGED_PS` reads from
  `vif->cfg.ps` (in `struct ieee80211_vif_cfg`) and belongs in
  `vif_cfg_changed()`, not `link_info_changed()`
