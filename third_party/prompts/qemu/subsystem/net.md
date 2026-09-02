# QEMU Network Device Review Guidelines

## Core Principles
Virtual network devices (e1000, virtio-net, vmxnet3, igb, ethlite) interface between guest network drivers and the host TAP/socket network backends.

## Critical Invariants to Verify

1. **Packet Bounds Validation**:
   - Guest transmit handlers receive a packet length (`tx_len`) or descriptor chain specifying packet bytes.
   - You MUST verify that `tx_len` does not exceed the maximum transmission unit or internal hardware buffer capacity (`BUFSZ_MAX`):
     ```c
     if (tx_len > sizeof(s->tx_buffer)) {
         qemu_log_mask(LOG_GUEST_ERROR, "%s: oversized packet %u\n", __func__, tx_len);
         return;
     }
     ```
   - Unchecked packet lengths cause out-of-bounds heap/stack reads when passing buffers to `qemu_send_packet()`.

2. **Receive Queue Full Conditions**:
   - Verify `can_receive()` callback returns `false` or `0` when receive rings/FIFOs are full.
   - When the guest drains receive descriptors, call `qemu_flush_queued_packets()` to resume network packet delivery.
