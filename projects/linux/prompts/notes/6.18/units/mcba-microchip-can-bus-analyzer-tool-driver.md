# MCBA MICROCHIP CAN BUS ANALYZER TOOL DRIVER

The `mcba_usb` driver is the Linux SocketCAN USB driver for the Microchip CAN BUS Analyzer Tool (VID 0x04d8, PID 0x0a30). It bridges the Linux CAN networking subsystem (`can_priv` and `net_device`) with the hardware over USB bulk endpoints, translating outgoing CAN frames and configuration commands (bitrate, termination resistance, firmware version requests) into 19-byte USB messages and parsing incoming CAN frames and PIC keep-alive status messages.

## Architecture & Data Flow

- `struct can_priv can` must remain the first member of `struct mcba_priv` because both SocketCAN helpers (`open_candev`, `close_candev`, `register_candev`) and driver callbacks cast `netdev_priv(netdev)` directly to their respective struct pointers. [F24]
- The `echo_skb_max` count passed to `alloc_candev()` in `mcba_usb_probe()` must be at least `ARRAY_SIZE(priv->tx_context)` (`MCBA_MAX_TX_URBS`) because `ctx->ndx` from `priv->tx_context` directly indexes `priv->can.echo_skb` in `can_put_echo_skb()`, `can_get_echo_skb()`, and `can_free_echo_skb()`. [F65]
- Every wire message struct cast to or from `struct mcba_usb_msg` (`mcba_usb_msg_can`, `mcba_usb_msg_ka_usb`, `mcba_usb_msg_ka_can`, `mcba_usb_msg_change_bitrate`, `mcba_usb_msg_termination`, `mcba_usb_msg_fw_ver`) must be `__packed` and match `sizeof(struct mcba_usb_msg)` (`MCBA_USB_TX_BUFF_SIZE`, 19 bytes). [F25]
- Because `mcba_usb_xmit()` unconditionally copies `MCBA_USB_TX_BUFF_SIZE` (19) bytes from `usb_msg` into the USB DMA buffer, every caller building a stack `mcba_usb_msg_*` variant (`mcba_usb_start_xmit`, `mcba_usb_xmit_change_bitrate`, `mcba_usb_xmit_read_fw_ver`, `mcba_set_termination`) must zero-initialize the entire struct. [F16]
- Every URB flagged with `URB_NO_TRANSFER_DMA_MAP` in `mcba_usb_xmit()` and `mcba_usb_start()` must have `urb->transfer_dma` set from `usb_alloc_coherent()` and its buffer released via `usb_free_coherent()` with the matching size and DMA address on TX completion (`mcba_usb_write_bulk_callback`), RX teardown (`mcba_urb_unlink`), or submit failure. [F59]
- Because multiple 19-byte `struct mcba_usb_msg` packets can be stacked in one RX transfer buffer, `mcba_usb_read_bulk_callback()` must iterate while `pos < urb->actual_length`, `break` if `pos + sizeof(struct mcba_usb_msg) > urb->actual_length`, and advance `pos` by `sizeof(struct mcba_usb_msg)` every iteration. Broken by: adding a `continue` before advancing `pos`. [F38]
- `mcba_usb_process_can()` must clamp the wire DLC via `can_cc_dlc2len(msg->dlc & MCBA_DLC_MASK)` before copying `cf->len` bytes from `msg->data` to `cf->data`, and must not copy payload bytes when `msg->dlc & MCBA_DLC_RTR_MASK` is set. [F30]
- In `mcba_usb_process_ka_can()`, the `else if` chain updating `priv->can.state` must evaluate `msg->tx_bus_off` (`CAN_STATE_BUS_OFF`), then `MCBA_CAN_STATE_ERR_PSV_TH` (127, `CAN_STATE_ERROR_PASSIVE`), and finally `MCBA_CAN_STATE_WRN_TH` (95, `CAN_STATE_ERROR_WARNING`) in descending order of severity. [F34]

## Synchronization

- Because `mcba_usb_free_ctx()` and `mcba_usb_get_free_ctx()` access `priv->tx_context` and `priv->free_ctx_cnt` concurrently without a lock, `mcba_usb_free_ctx()` must call `atomic_inc(&ctx->priv->free_ctx_cnt)` before setting `ctx->ndx = MCBA_CTX_FREE`, and call `netif_wake_queue()` only after setting `ctx->ndx = MCBA_CTX_FREE`. [F56]
- `mcba_usb_xmit()` is invoked from both the netdev TX softirq path (`mcba_usb_start_xmit`) and process-context command paths (`mcba_usb_xmit_cmd`), so its `usb_alloc_urb()`, `usb_alloc_coherent()`, and `usb_submit_urb()` calls must use `GFP_ATOMIC` and neither `mcba_usb_xmit()` nor `mcba_usb_get_free_ctx()` may sleep. [F54]
- `mcba_usb_read_bulk_callback()` and `mcba_usb_write_bulk_callback()` run in USB URB completion interrupt context, so `mcba_usb_read_bulk_callback()` must resubmit the URB with `GFP_ATOMIC` and neither callback nor `mcba_usb_process_rx()` or `mcba_usb_process_can()` may sleep. [F55]

## Object Lifetimes

- In `mcba_usb_start_xmit()`, `can_put_echo_skb()` takes ownership of `skb`, so all reads from `skb` (`cf`) must complete before calling `can_put_echo_skb()`, and any subsequent error path before `mcba_usb_xmit()` succeeds must release the slot via `can_free_echo_skb()` without calling `dev_kfree_skb(skb)`. [F1]
- `mcba_usb_xmit()` must synchronously copy `usb_msg` into its newly allocated `usb_alloc_coherent()` buffer before returning, and must never store `usb_msg` in the URB or `mcba_usb_ctx`, because callers (`mcba_usb_start_xmit`, `mcba_usb_xmit_change_bitrate`, `mcba_usb_xmit_read_fw_ver`, `mcba_set_termination`) pass stack-allocated message structs. [F44]
- Coherent RX DMA buffers allocated with `usb_alloc_coherent()` in `mcba_usb_start()` are not freed when RX URBs complete or are killed via `usb_kill_anchored_urbs()`, so each submitted buffer must be recorded in `priv->rxbuf` and `priv->rxbuf_dma` and freed with `usb_free_coherent()` in `mcba_urb_unlink()` after killing `priv->rx_submitted`. [F3]
- Because `struct mcba_priv` is embedded in the `net_device` allocated by `alloc_candev()`, `mcba_usb_disconnect()` must call `unregister_candev(priv->netdev)` and `mcba_urb_unlink(priv)` to stop and drain all anchored URBs and free `priv->rxbuf` before calling `free_candev(priv->netdev)`. [F4]
- In `mcba_usb_probe()`, `usb_find_common_endpoints()` must validate the bulk IN and OUT endpoints and `priv->rx_pipe` and `priv->tx_pipe` must be initialized along with `register_candev(netdev)` before calling `mcba_usb_start(priv)`, because `mcba_usb_start()` immediately submits RX and TX URBs whose callbacks access `priv->netdev`. [F45]
- In `mcba_usb_start()`, `mcba_init_ctx(priv)` must be called before `mcba_usb_xmit_read_fw_ver()`, because transmitting firmware version requests allocates entries from `priv->tx_context` via `mcba_usb_get_free_ctx()` and relies on `ctx->priv` in `mcba_usb_write_bulk_callback()`. [F46]

## API Contracts

- `mcba_usb_xmit()` transfers ownership of the caller-allocated `struct mcba_usb_ctx` to `mcba_usb_write_bulk_callback()` only when it returns 0; if `mcba_usb_xmit()` returns a negative error code, it cleans up its own URB and coherent buffer but leaves `ctx` allocated for the caller to release with `mcba_usb_free_ctx()`. [F2]
- `mcba_usb_get_free_ctx()` sets `ctx->can` from whether `cf` is non-NULL, and `mcba_usb_write_bulk_callback()` checks `ctx->can` before calling `can_get_echo_skb()`; thus command callers (`mcba_usb_xmit_cmd`) must pass `cf = NULL` while CAN TX callers (`mcba_usb_start_xmit`) must pass non-NULL `cf` and queue an echo skb at `ctx->ndx`. [F15]
- `mcba_usb_start_xmit()` must validate outgoing packets with `can_dev_dropped_skb(netdev, skb)` before acquiring a `mcba_usb_ctx` or reading `skb->data` as `struct can_frame`, and must return `NETDEV_TX_OK` without freeing `skb` when `can_dev_dropped_skb()` returns `true`. [F62]
- `mcba_netdev_ops` must wire `.ndo_change_mtu = can_change_mtu` so that the interface MTU remains locked to `CAN_MTU` and oversized `ETH_P_CANFD` or `ETH_P_CANXL` frames cannot reach `mcba_usb_start_xmit()` and overflow the 8-byte `usb_msg.data` buffer. [F61]
- Before calling `register_candev(netdev)` in `mcba_usb_probe()`, `priv->can.termination_const`, `priv->can.termination_const_cnt`, and `priv->can.do_set_termination` must all be populated together, and `priv->can.bitrate_const` and `priv->can.bitrate_const_cnt` must both be populated without `priv->can.bittiming_const`. [F67]
- `mcba_usb_open()` must call `open_candev(netdev)` and check its return value before starting the transmit queue with `netif_start_queue(netdev)`, and `mcba_usb_close()` must stop the queue and unlink URBs before calling `close_candev(netdev)`. [F69]

## Error Handling

- Although `mcba_usb_xmit()` is declared with return type `netdev_tx_t`, it returns `0` on success and negative errno codes on failure, so `mcba_usb_start_xmit()` must catch any non-zero return in an `int`, release the echo skb and `mcba_usb_ctx`, and return `NETDEV_TX_OK`. Broken by: returning `mcba_usb_xmit()`'s result directly from `mcba_usb_start_xmit()`. [F14]
- Both `mcba_usb_xmit()` and `mcba_usb_start()` must anchor each URB with `usb_anchor_urb()` before `usb_submit_urb()`, call `usb_free_urb()` after successful submission so the USB core owns the final reference, and on `usb_submit_urb()` failure call `usb_unanchor_urb()`, `usb_free_coherent()`, and `usb_free_urb()`. [F6]
- In `mcba_usb_read_bulk_callback()`, the `switch (urb->status)` block must return without resubmitting on `-ENOENT`, `-EPIPE`, `-EPROTO`, and `-ESHUTDOWN`, jump to `resubmit_urb` bypassing message parsing on other non-zero statuses, and only enter the `while (pos < urb->actual_length)` loop when `urb->status == 0`. [F36]

## External Contracts

- `mcba_set_termination()` and `mcba_usb_process_ka_usb()` must translate between SocketCAN termination values (`MCBA_TERMINATION_ENABLED` = 120, `MCBA_TERMINATION_DISABLED` = 0) and the hardware's inverted active-low wire values (`MCBA_VER_TERMINATION_ON` = 0, `MCBA_VER_TERMINATION_OFF` = 1). [F18]
- `mcba_net_set_bittiming()`, `mcba_usb_xmit_change_bitrate()`, and `convert_can2host_bitrate()` must convert between SocketCAN's `priv->can.bittiming.bitrate` in bps and the USB protocol's big-endian `__be16` bitrate in kbps, adding `333` bps when the device reports `33` or `83` kbps. [F19]
- `mcba_usb_process_rx()` must keep explicit no-op `case` branches for `MBCA_CMD_NOTHING_TO_SEND` and `MBCA_CMD_TRANSMIT_MESSAGE_RSP` in its `switch (msg->cmd_id)` statement so those routine device messages do not fall into `default:`. [F37]

## Known Pitfalls

- Multi-byte fields in `mcba_usb_msg_can`, `mcba_usb_msg_ka_can`, and `mcba_usb_msg_change_bitrate` (`sid`, `eid`, `bitrate`, `can_bitrate`, `rx_lost`) lie at potentially unaligned byte offsets inside `__packed` 19-byte records, so they must be accessed via `get_unaligned_be16()`, `put_unaligned_be16()`, or `get_unaligned_le16()`. [F32]
- In `mcba_usb_start_xmit()`, `usb_msg.dlc` must be assigned `cf->len` and used as the length argument to `memcpy(usb_msg.data, cf->data, usb_msg.dlc)` before `MCBA_DLC_RTR_MASK` (`0x40`) is ORed into `usb_msg.dlc` for `CAN_RTR_FLAG` frames. [F17]
- In `mcba_usb_process_can()`, reconstructing bits 28..16 of an extended 29-bit `cf->can_id` via `(sid & 0xffe0) << 13` and `(sid & 3) << 16` requires the 16-bit `sid` operand to remain at least 32 bits wide during the left shift rather than being truncated back to `u16`. [F40]
- In `mcba_usb_read_bulk_callback()`, `pos` is a byte offset incremented by `sizeof(struct mcba_usb_msg)`, so pointer arithmetic on `urb->transfer_buffer + pos` must be performed on the untyped `void *` (or `u8 *`) before casting to `struct mcba_usb_msg *`. [F43]
