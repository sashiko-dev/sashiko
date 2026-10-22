# OCELOT ETHERNET SWITCH DRIVER

This component provides the shared SoC headers (`include/soc/mscc/ocelot*.h`) defining hardware registers, data structures, VCAP TCAM filter models, and API declarations for the Microsemi/Microchip Ocelot family of Ethernet switches, along with the DSA tagging protocol drivers (`net/dsa/tag_ocelot.c` and `net/dsa/tag_ocelot_8021q.c`). The DSA tag drivers handle frame injection and extraction headers (IFH/EFH) for NPI ports as well as 802.1Q VLAN-based tagging with deferred kthread transmission for PTP and link-local control frames.

## Architecture & Data Flow

- In `struct ocelot`, front-panel ports are `0 .. num_phys_ports - 1` and the CPU port module is at index `num_phys_ports` (`BIT(num_phys_ports)` in port masks). Because `0` is a valid physical port and `ports[]` entries can be `NULL` for disabled/unpopulated ports, `ocelot->npi` must be `-1` whenever no NPI port is active, and loops over `0 .. num_phys_ports - 1` must null-check `ocelot->ports[port]`. [F25, F26, F50]
- In `struct ocelot_8021q_tagger_private`, `struct ocelot_8021q_tagger_data data` must remain the first member because `ocelot_connect` assigns `priv` to `void *ds->tagger_data` while `ocelot_8021q_tagger_data()` casts `ds->tagger_data` directly to `struct ocelot_8021q_tagger_data *`. [F65]
- In `struct ocelot` and `struct ocelot_vcap_filter`, `bridges` and `ingress_port_mask` must remain `unsigned long` rather than narrower integer types because callers pass `&ocelot->bridges` and `&filter->ingress_port_mask` directly to `unsigned long *` bitmap operations (`set_bit`, `clear_bit`, `find_first_zero_bit`, `for_each_set_bit`). [F68]
- Every `struct ocelot_vcap_filter` installed in a VCAP block must only be linked on `ocelot->block[block_id].rules` via `filter->list`, using `filter->is_trap = true` to mark trapping rules rather than adding a secondary `list_head` or linking into `ocelot->traps`. [F5]
- Per-port PTP configuration (`trap_proto`, `ptp_cmd`) and VLAN state (`pvid_vlan`, `vlan_aware`) must live per port in `struct ocelot_port` rather than in switch-wide `struct ocelot` fields so port-level updates do not overwrite sibling ports. [F43]
- Driver-internal VCAP filter cookies in `include/soc/mscc/ocelot_vcap.h` (`OCELOT_VCAP_ES0_*`, `OCELOT_VCAP_IS1_*`, `OCELOT_VCAP_IS2_*`) must be unique per block, partitioning per-port cookies into disjoint strides of `ocelot->num_phys_ports` and placing global `VCAP_IS2` trap cookies at `ocelot->num_phys_ports * 2 + k`. [F54]
- The `ANA_PGID_PGID` table partitions indices into unicast (`0 .. num_phys_ports - 1`), dynamic multicast (`num_phys_ports + 1 .. PGID_BLACKHOLE - 1` via `for_each_nonreserved_multicast_dest_pgid`), reserved (`PGID_BLACKHOLE..PGID_BC`, `57..63`), LAG (`PGID_AGGR..PGID_SRC - 1`, `64..79`), and source (`PGID_SRC + port`, `80+`). [F51]
- Every `enum ocelot_reg` encodes a 1-based `enum ocelot_target` (`ANA = 1 .. TARGET_MAX - 1`) at `<< TARGET_OFFSET` (24) and a 0-based per-target index in `REG_MASK` (`GENMASK(23, 0)`), which `REG(reg, offset)` uses to initialize `ocelot->map[target][reg & REG_MASK]`. [F52]
- 2-step PTP TX timestamp identifiers (`ts_id`) are bounded by `OCELOT_MAX_PTP_ID` (63), allowing 0-based IDs `0 .. 62` while keeping the 6-bit value `63` reserved. [F53]
- In `ocelot_rcv`, when `dsa_port_is_vlan_filtering(dp)` is true and `eth_hdr(skb)->h_proto` matches `htons(vlan_tpid)`, the 802.1Q header must be popped (`__skb_vlan_pop`) and replaced with `vlan_tci` from the extraction header via `__vlan_hwaccel_put_tag`; in VLAN-unaware mode the VLAN header must remain unmodified. [F38]

## Synchronization

- Register-based frame injection via `ocelot_can_inject` and `ocelot_port_inject_frame` must be serialized across CPUs by holding `ocelot->inj_lock` (via `ocelot_lock_inj_grp` and `ocelot_unlock_inj_grp`) continuously across the readiness check and frame write. [F10]
- Callers of `ocelot->ops->cut_through_fwd` and `ocelot->ops->tas_guard_bands_update` must hold `ocelot->fwd_domain_lock`, whereas `ocelot->ops->tas_clock_adjust` must be invoked without holding `ocelot->fwd_domain_lock` or `ocelot->ptp_clock_lock` because it acquires `fwd_domain_lock` and then `ptp_clock_lock`. [F13]
- Indirect hardware statistics counter reads via `SYS_STAT_CFG` and `ocelot->ops->update_stats` must hold `ocelot->stat_view_lock`, while accesses to the software `ocelot->stats` array must hold `ocelot->stats_lock` (which nests inside `stat_view_lock`). [F14]
- In `net/dsa/tag_ocelot_8021q.c`, `ocelot_defer_xmit` is called from `.xmit` (`ocelot_xmit`) in atomic transmit context, so it must allocate `struct felix_deferred_xmit_work` with `GFP_ATOMIC` and queue it to `priv->xmit_worker` via `kthread_queue_work` without sleeping. [F80]
- Updates to the 64-bit counters in `struct ocelot_ts_stats` (`pkts`, `onestep_pkts_unconfirmed`, `lost`, `err`) must be bracketed by `u64_stats_update_begin`/`u64_stats_update_end` on `ts_stats->syncp`, and readers (`ocelot_port_get_ts_stats`) must use `u64_stats_fetch_begin`/`u64_stats_fetch_retry`. [F15]

## Object Lifetimes

- `ocelot_port_txtstamp_request` queues the cloned two-step PTP packet `*clone` onto `ocelot_port->tx_skbs` (and increments `ocelot->ptp_skbs_in_flight` under `ocelot->ts_id_lock`), so `*clone` is owned by `tx_skbs` on success while the caller receives a borrowed pointer for `OCELOT_SKB_CB(skb)->clone`. Broken by: calling `kfree_skb(*clone)` on TX drop without unlinking from `tx_skbs` and decrementing `ptp_skbs_in_flight` under `ts_id_lock`. [F6]

## API Contracts

- `ocelot_rcv` in both `net/dsa/tag_ocelot.c` and `net/dsa/tag_ocelot_8021q.c` must return `NULL` without freeing `skb` when `dsa_conduit_find_user` returns `NULL` or a frame is dropped, because the caller `dsa_switch_rcv` frees `skb` whenever `rcv` returns `NULL`. [F2]
- `ocelot_vcap_filter_add` takes ownership of the caller-allocated `struct ocelot_vcap_filter *rule` into `ocelot->block[rule->block_id].rules` only when it returns 0; on non-zero return the caller must `kfree(rule)`, whereas once added, `ocelot_vcap_filter_del` unlinks and `kfree`s it. [F4]
- `ocelot_regmap_init` returns an `ERR_PTR`-encoded `struct regmap *` on failure (never `NULL`), whereas absent optional entries in `ocelot->targets[]` must be stored as `NULL`; callers must check `IS_ERR()` and substitute `NULL` for optional targets before storing into `ocelot->targets[]`. [F24]
- In `include/soc/mscc/ocelot.h`, `ocelot_write*`, `ocelot_port_writel`, and `ocelot_target_write*` take `(..., val, reg, ...)`, whereas `ocelot_field_write` and `ocelot_fields_write` take `(..., reg, val)`. [F28]
- `ocelot_read*`, `ocelot_write*`, `ocelot_rmw*`, and `ocelot_port_readl`/`writel` require a target-encoded `enum ocelot_reg` (`target << TARGET_OFFSET`), whereas target-less `VCAP_*` constants (`VCAP_CORE_UPDATE_CTRL`..`VCAP_CONST_IF_CNT`) must only be used with `ocelot_target_read*`/`ocelot_target_write*`. [F29]
- In `struct ocelot_vcap_filter`, byte-array key fields (`ocelot_vcap_u16`, `ocelot_vcap_u48`, `ocelot_vcap_ipv4`, e.g. `key.etype.etype`) store values and masks in big-endian byte order, whereas `ocelot_vcap_vid` (`vlan.vid`) and `ocelot_vcap_udp_tcp` (`sport`, `dport`) store `u16` values and masks in host byte order. [F37]
- In `struct ocelot_vcap_filter`, `filter->action` is an anonymous union discriminated by `filter->block_id` (`VCAP_ES0`, `VCAP_IS1`, `VCAP_IS2`), and `filter->key` is a union discriminated by `filter->key_type` (where `OCELOT_VCAP_KEY_ANY` has no active `key` member). [F35]
- In `struct ocelot_vcap_filter`, `VCAP_IS1` and `VCAP_IS2` match ingress ports via the bitmask `ingress_port_mask` (`BIT(port)`), whereas `VCAP_ES0` matches ports via `ingress_port` and `egress_port` (`struct ocelot_vcap_port`), where `.value` is a port index and `.mask` is `GENMASK(key_length - 1, 0)`. [F36]
- VCAP filter lookup and management paths must store and pass rule cookies as `unsigned long` (matching `struct ocelot_vcap_id::cookie` and `ocelot_vcap_block_find_filter_by_id()`) rather than narrowing to `int` or `u32`, because TC-offloaded rules use 64-bit pointer-sized cookies on 64-bit architectures. [F62]
- In `ocelot_xmit_common`, `ocelot_xmit_get_vlan_info` must be called before `skb_push(skb, OCELOT_TAG_LEN)` and `skb_push(skb, OCELOT_SHORT_PREFIX_LEN)`, and `injection` must be saved before pushing the short prefix, because `ocelot_xmit_get_vlan_info` strips the 802.1Q tag at `skb->data`. [F66]
- In `ocelot_xmit_common()`, `skb->priority` must be mapped through `netdev_get_prio_tc_map(netdev, skb->priority)` whenever `netdev_get_num_tc(netdev)` is non-zero before passing `qos_class` to `ocelot_ifh_set_qos_class()`. [F87]
- In `ocelot_xmit` (`net/dsa/tag_ocelot_8021q.c`), packets with a non-zero `ocelot_ptp_rew_op(skb)` or a link-local destination MAC (`is_link_local_ether_addr(hdr->h_dest)`) must be diverted to `ocelot_defer_xmit(dp, skb)` instead of `dsa_8021q_xmit`. [F61]
- On link down (`ocelot_phylink_mac_link_down`), only `DEV_MAC_ENA_CFG_RX_ENA` (never `DEV_MAC_ENA_CFG_TX_ENA`) may be cleared in `DEV_MAC_ENA_CFG` via `ocelot_port_rmwl` before port flushing, and callers must pass the switch's `OCELOT_QUIRK_*` flags to `ocelot_phylink_mac_link_down` and `ocelot_phylink_mac_link_up`. [F78]
- When configuring port maximum frame length with `ocelot_port_set_maxlen` and `DEV_MAC_TAGS_CFG`, `DEV_MAC_TAGS_CFG_VLAN_LEN_AWR_ENA` and `DEV_MAC_TAGS_CFG_VLAN_DBL_AWR_ENA` must be enabled and `sdu` must be passed without VLAN tag overhead so hardware automatically expands `DEV_MAC_MAXLEN_CFG` for tagged frames. [F77]
- Switch drivers must initialize variant-specific bounds in `struct ocelot` (`num_flooding_pgids`, `num_mact_rows`, `num_phys_ports`) before `ocelot_init()`, and shared code must bound `ANA_FLOODING` writes by `ocelot->num_flooding_pgids` and MAC table dumps by `ocelot->num_mact_rows`. [F75]
- In `ocelot_mact_learn_streamdata`, negative values (`< 0`) for `sfid` or `ssid` indicate that `ANA_TABLES_STREAMDATA_SFID_VALID` or `ANA_TABLES_STREAMDATA_SSID_VALID` must remain cleared, whereas non-negative values enable and program the respective index. [F32]
- `struct ocelot_policer` requires `rate` in kilobits per second and `burst` in bytes, so callers of `ocelot_port_policer_add()` and `ocelot_vcap_policer_add()` must convert byte-per-second rates from TC/DSA offload structures before populating `struct ocelot_policer`. [F96]
- In `include/soc/mscc/ocelot.h`, the `CONFIG_BRIDGE_MRP` declarations and `#else` inline stubs for `ocelot_mrp_add`, `ocelot_mrp_del`, `ocelot_mrp_add_ring_role`, and `ocelot_mrp_del_ring_role` must keep identical parameter and return types. [F46]

## Error Handling

- In `net/dsa/tag_ocelot_8021q.c`, `ocelot_rcv` must initialize both `src_port` and `switch_id` to `-1` before calling `dsa_8021q_rcv`, because `dsa_8021q_rcv` only populates `*source_port` and `*switch_id` when their incoming values equal `-1`. [F16]
- In `ocelot_rcv` (`net/dsa/tag_ocelot.c`), after pulling `OCELOT_SHORT_PREFIX_LEN` and `OCELOT_TAG_LEN` bytes, `src_port` unpacked from the extraction header must be validated via `dsa_conduit_find_user(netdev, 0, src_port)` and silently dropped (returning `NULL` without logging) when no user port exists. [F48]

## External Contracts

- `ocelot_xmit_common` must push `OCELOT_TAG_LEN` (16) bytes for the injection header and zero it with `memset` before packing bitfields, then push `OCELOT_SHORT_PREFIX_LEN` (4) bytes for the big-endian short prefix (`0x8880000a` in `ocelot_xmit`, `0x88800005` in `seville_xmit`), matching `.needed_headroom = OCELOT_TOTAL_TAG_LEN`. [F47]

## Known Pitfalls

- Any `struct ocelot_vcap_filter` passed to `ocelot_vcap_filter_add`, `ocelot_vcap_filter_del`, or `ocelot_vcap_filter_replace` must have its `struct ocelot_vcap_id id` field (including trailing alignment padding after `bool tc_offload`) zero-initialized before setting `id.cookie` and `id.tc_offload`. [F33]
- In `ocelot_rcv`, after pulling `OCELOT_SHORT_PREFIX_LEN` and `OCELOT_TAG_LEN`, `skb_reset_mac_header(skb)`, `skb_reset_mac_len(skb)`, and `skb_postpull_rcsum(skb, start, OCELOT_TOTAL_TAG_LEN)` must be called using the pre-pull `start = skb->data` pointer to keep MAC header offsets and `skb->csum` synchronized. [F39]
- In `struct ocelot_vcap_key_ipv6`, bytes `0..7` of `sip` and `dip` (`struct ocelot_vcap_u128`) are ignored by VCAP IS2 half-key programming, which only matches the lower 64 bits (`value[8..15]` and `mask[8..15]`) of `sip`. [F55]
