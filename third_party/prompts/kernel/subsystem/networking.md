# Networking Subsystem Details

## SKB Buffer Operations

`skb_put()`, `skb_push()`, and `skb_pull()` modify the data boundaries of a
socket buffer. Passing untrusted or unchecked lengths causes a kernel panic
(DoS). The bounds checks fire before memory is corrupted, so the result is a
crash rather than a silent overflow, but it is still a bug.

- `skb_put(skb, len)` extends the tail. Panics via `skb_over_panic()` if
  `skb->tail > skb->end`.
- `skb_push(skb, len)` prepends to head. Panics via `skb_under_panic()` if
  `skb->data < skb->head`.
- `skb_pull(skb, len)` consumes from head. Returns NULL if `len > skb->len`.
  If the pull causes `skb->len` to drop below `skb->data_len` (meaning the
  linear region was exhausted), `__skb_pull()` calls `BUG()`.

Both `skb_over_panic()` and `skb_under_panic()` call `skb_panic()` which
calls `BUG()` (defined in `net/core/skbuff.c`).

## SKB Shared and Cloned Buffers

Modifying a shared or cloned SKB corrupts other users of the same buffer
data, leading to silent data corruption or crashes in unrelated code paths.

- `skb_shared(skb)` returns true when `refcount_read(&skb->users) != 1`
- `skb_cloned(skb)` returns true when the data area is shared with another SKB

`skb_unshare(skb, gfp)` returns an exclusive copy. If the buffer is cloned,
it copies the SKB via `skb_copy()` and frees the original unconditionally
(via `consume_skb()` on success, `kfree_skb()` on allocation failure). If not
cloned, it returns the original unchanged. Always use the returned pointer --
the input pointer may have been freed. A NULL return means allocation failed
and the original SKB has already been freed.

## Header Linearization

Packet headers may span paged fragments and cannot be safely dereferenced
without first ensuring the bytes are in the linear region (`skb->data`).
Dereferencing header pointers without linearization can cause page faults or
read garbage from unrelated memory.

`pskb_may_pull(skb, len)` guarantees at least `len` bytes are contiguous
in the linear part, pulling from fragments if necessary.

```c
if (!pskb_may_pull(skb, sizeof(struct iphdr)))
    return -EINVAL;

iph = ip_hdr(skb);  /* safe: header is now in linear region */
```

## Socket Locking vs Socket Release

Confusing `release_sock()` and `sock_release()` causes use-after-free
(calling the wrong one) or deadlocks (omitting the unlock).

- `release_sock(sk)` releases the socket lock acquired by `lock_sock()`.
  It processes the backlog queue and wakes waiters. The socket remains alive.
- `sock_release(sock)` closes and destroys the `struct socket`, releasing
  the protocol stack and associated inode via `__sock_release()`.

There is no function called `socket_release()` in the kernel.

After `release_sock()`, the socket is still valid but unlocked -- other
threads may now operate on it. After `sock_release()`, the socket structure
is freed and must not be accessed.

## Socket Reference Counting

Dropping a socket reference without holding one, or failing to take a
reference when storing a socket pointer, causes use-after-free crashes.

Socket lifetime is managed through `sk_refcnt`:

- `sock_hold(sk)` increments `sk->sk_refcnt` via `refcount_inc()`
- `sock_put(sk)` decrements `sk->sk_refcnt` and calls `sk_free()` when it
  reaches zero

A socket can outlive its file descriptor. Code that holds a pointer to a
socket outside the file descriptor's lifetime must hold a reference with
`sock_hold()` and release it with `sock_put()`.

## Netfilter Hook Ownership

Accessing an SKB after passing it to `NF_HOOK()` is a use-after-free. The
hook verdict determines what happens to the SKB:

| Verdict | Meaning | SKB Ownership |
|---------|---------|---------------|
| `NF_ACCEPT` | Continue processing | `okfn()` is called with the SKB |
| `NF_DROP` | Reject packet | Netfilter frees the SKB via `kfree_skb_reason()` |
| `NF_STOLEN` | Hook consumed packet | Hook took ownership |
| `NF_QUEUE` | Queue for userspace | `nf_queue()` takes ownership |

In all cases, the caller of `NF_HOOK()` or `NF_HOOK_COND()` loses ownership
of the SKB and must not access it after the call. The verdict dispatch is
implemented in `nf_hook_slow()` (`net/netfilter/core.c`).

Device pointers (`in`, `out`) passed to `NF_HOOK()` must remain valid
throughout hook traversal.

## Buffer Handoff Safety

Accessing an SKB after handing it to another subsystem is a use-after-free.
Once an SKB is passed to another subsystem (queued, enqueued, handed to a
protocol handler), the caller loses ownership. The receiver may free it at
any time, including before the handoff function returns.

## Byte Order Conversions

Byte order mismatches cause silent data corruption -- packets are
misrouted, ports are mismatched, and protocol fields are misinterpreted.

Network protocols use big-endian byte order. The kernel uses `__be16`,
`__be32`, and `__be64` types (defined in `include/uapi/linux/types.h`)
to annotate network-order values. Common byte order bugs:

- Comparing a `__be16` port with a host-order constant without `htons()`
- Performing arithmetic on network-order values without converting first
- Double-converting (applying `htons()` to an already network-order value)

Sparse catches these at build time via `__bitwise` type annotations
(active when `__CHECKER__` is defined; run with `make C=1`).

## RCU Protection for Routing

Accessing a dst entry outside its RCU read-side critical section causes
use-after-free because the entry may be freed by the RCU grace period.

Routing table lookups (FIB lookups, dst entries) are protected by RCU.
`ip_route_input_noref()` performs an RCU-protected lookup and stores a
noref dst on the SKB. It manages its own internal `rcu_read_lock()` /
`rcu_read_unlock()`. If the dst needs to survive beyond that internal RCU
section, the caller must hold an outer `rcu_read_lock()` and upgrade via
`skb_dst_force()`. This pattern is implemented in `ip_route_input()`
(`include/net/route.h`):

```c
rcu_read_lock();
reason = ip_route_input_noref(skb, dst, src, dscp, devin);
if (!reason) {
    skb_dst_force(skb);  /* upgrade to refcounted dst */
    if (!skb_dst(skb))
        reason = SKB_DROP_REASON_NOT_SPECIFIED;
}
rcu_read_unlock();
```

`skb_dst_set_noref()` stores an RCU-protected dst entry without taking a
reference -- it warns if neither `rcu_read_lock()` nor `rcu_read_lock_bh()`
is held. If the dst needs to survive beyond the RCU read-side critical
section, use `skb_dst_force()` to upgrade to a refcounted reference.
`skb_dst_force()` returns false if the dst could not be held (already
freed).

## Per-CPU Network Statistics

Incorrect synchronization on per-CPU network statistics causes torn reads
on 32-bit systems (64-bit counters read as two halves from different
updates) or lost increments when preempted by BH processing.

The SNMP stat macros in `include/net/snmp.h` handle this:

- `SNMP_INC_STATS()` / `SNMP_ADD_STATS()` use `this_cpu_inc()` /
  `this_cpu_add()`, safe for single-word (`unsigned long`) counters
- `SNMP_ADD_STATS64()` / `SNMP_UPD_PO_STATS64()` wrap updates in
  `local_bh_disable()` / `local_bh_enable()` and use `u64_stats`
  seqcounts on 32-bit systems (`#if BITS_PER_LONG==32`) where a 64-bit
  update is not atomic
- The double-underscore variants (`__SNMP_ADD_STATS64()`) omit the
  `local_bh_disable()` wrapper and must only be called from BH-disabled
  or process context that cannot be preempted by BH

Driver-specific statistics should follow the guidelines in
`Documentation/networking/statistics.rst`.

## Packet Type Constants

Misinterpreting `skb->pkt_type` causes packets to be delivered to the
wrong handler or silently dropped. The field classifies received packets:

| Constant | Value | Meaning |
|----------|-------|---------|
| `PACKET_HOST` | 0 | Destined for this host |
| `PACKET_BROADCAST` | 1 | Link-layer broadcast |
| `PACKET_MULTICAST` | 2 | Link-layer multicast |
| `PACKET_OTHERHOST` | 3 | Destined for another host (promiscuous) |
| `PACKET_OUTGOING` | 4 | Outgoing of any type |
| `PACKET_LOOPBACK` | 5 | MC/BRD frame looped back |

These are defined in `include/uapi/linux/if_packet.h`.

## Special Port Constants

Failing to exclude the `VMADDR_PORT_ANY` sentinel from port iteration
causes the vsock subsystem to bind to the wildcard port, breaking port
allocation.

`VMADDR_PORT_ANY` is defined as `-1U` (0xFFFFFFFF) in
`include/uapi/linux/vm_sockets.h` for the vsock subsystem. Port allocation
logic that iterates or wraps around port ranges must explicitly exclude this
sentinel value to avoid binding to the wildcard port.

## TCP Receive Buffer and Window Clamping

Calling `tcp_clamp_window()` when the receive queue is empty
(`sk_rmem_alloc == 0`) sets `sk_rcvbuf` to zero, permanently stalling the
connection because `tcp_can_ingest()` rejects all incoming packets.

`tcp_clamp_window()` (in `net/ipv4/tcp_input.c`) adjusts the receive
buffer based on current memory usage via
`min(atomic_read(&sk->sk_rmem_alloc), rmem2)`. With `sk_rmem_alloc == 0`,
this produces a zero `sk_rcvbuf`. Once `sk_rcvbuf` is zero,
`tcp_can_ingest()` (which checks `rmem + skb->len <= sk->sk_rcvbuf`)
rejects all packets and the receiver cannot advertise a non-zero window.

`tcp_prune_queue()` guards against this by returning early when
`sk_rmem_alloc` is zero:

```c
if (!atomic_read(&sk->sk_rmem_alloc))
    return -1;  /* nothing to prune, avoid clamping empty queue */
```

Any code path that can reach `tcp_clamp_window()` must preserve this
invariant: `tcp_clamp_window()` must not be called when
`sk_rmem_alloc == 0`. Patches that change receive buffer checks
(`sk_rcvbuf`, `sk_rmem_alloc` comparisons) or introduce helper functions
that replace such checks can break this invariant if the new code allows
reaching `tcp_clamp_window()` with empty queues where the old code did not.

## SKB Control Block Lifetime

The `skb->cb` field is a 48-byte scratch area (`char cb[48]` in
`include/linux/skbuff.h`) shared across network layers. Each layer (IP,
netfilter, qdisc, driver) may overwrite it. Storing data in `skb->cb`
during packet construction and reading it from an SKB destructor or other
async callback causes data corruption, NULL pointer dereferences, or panics
because the cb contents will have been overwritten by intermediate layers.

```c
// WRONG: cb may be corrupted before destructor runs
struct my_metadata {
    u32 count;
    struct list_head list;
};
#define MY_CB(skb) ((struct my_metadata *)((skb)->cb))

void my_destructor(struct sk_buff *skb) {
    struct my_metadata *meta = MY_CB(skb);  // cb may be garbage
    process_list(&meta->list);               // crash or corruption
}
```

Safe alternatives for data that must survive until destruction:

- `skb_shinfo(skb)->destructor_arg`: stable throughout SKB lifetime, used
  by `skb_uarg()` and pointer-tagging helpers in `include/linux/skbuff.h`
- Separately allocated memory referenced from `destructor_arg`

```c
// CORRECT: using destructor_arg for destructor-accessible data
void my_init(struct sk_buff *skb, u64 addr) {
    skb_shinfo(skb)->destructor_arg = (void *)(addr | 1UL);  // tagged
}

void my_destructor(struct sk_buff *skb) {
    uintptr_t arg = (uintptr_t)skb_shinfo(skb)->destructor_arg;
    u64 addr = arg & ~1UL;  // safe: destructor_arg is preserved
    process_addr(addr);
}
```

`skb->cb` is safe within a single layer's processing (e.g., during qdisc
enqueue/dequeue) where the data is consumed before the SKB moves to the
next layer.

## PHY Initialization Completeness

Incomplete PHY `config_init` functions cause the PHY to malfunction in
certain interface modes (RGMII, MII, GMII, MII-Lite). The PHY may fail to
link, operate at reduced speeds, or exhibit data corruption. Hardware
strapping alone is often insufficient -- software must also configure
mode-selection registers.

PHY `config_init` functions must configure mode-selection registers based on
`phydev->interface`. Use `phy_interface_is_rgmii(phydev)`
(`include/linux/phy.h`) to detect RGMII mode variants (RGMII, RGMII-ID,
RGMII-RXID, RGMII-TXID).

For Broadcom PHYs: the RGMII Enable bit
(`MII_BCM54XX_AUXCTL_SHDWSEL_MISC_RGMII_EN`, defined in
`include/linux/brcmphy.h`) must be set via software when RGMII mode is
configured, even if the hardware strapping indicates RGMII. Use
`bcm54xx_auxctl_write()` (`drivers/net/phy/bcm-phy-lib.c`) to configure
shadow register 0x07.

RGMII modes require TX/RX delay configuration based on the specific
interface mode variant:

- `PHY_INTERFACE_MODE_RGMII`: no internal delays
- `PHY_INTERFACE_MODE_RGMII_ID`: internal delays on both TX and RX
- `PHY_INTERFACE_MODE_RGMII_RXID`: internal delay on RX only
- `PHY_INTERFACE_MODE_RGMII_TXID`: internal delay on TX only

The skew enable bit (`MII_BCM54XX_AUXCTL_SHDWSEL_MISC_RGMII_SKEW_EN`, defined in
`include/linux/brcmphy.h`) controls whether internal delays are applied.

A `config_init` function that configures features (LEDs, clocks, special
modes) but omits interface mode register writes is likely incomplete if the
PHY supports multiple interface modes. Compare against existing PHYs in the
same driver family to verify all required initialization steps are present.

## Virtio Network Header Alignment

Misaligned fields in virtio_net header structures cause severe performance
degradation (up to 50% throughput loss) without any functional failures or
crashes, making this bug class difficult to detect through testing.

`virtio_net_hdr_v1` (`include/uapi/linux/virtio_net.h`) is 12 bytes with
2-byte alignment. The structure contains `__u8`, `__virtio16` (which is
`__u16 __bitwise`), and `__le16` fields. All structures that embed
`virtio_net_hdr_v1` inherit this 2-byte alignment.

Adding `__le32` or `__u32` fields after an embedded `virtio_net_hdr_v1`
places the 4-byte field at a 2-byte aligned offset, causing misaligned
memory accesses:

```c
// WRONG: hash_value at offset 12 (2-byte aligned, needs 4-byte)
struct virtio_net_hdr_v1_hash {
    struct virtio_net_hdr_v1 hdr;  // 12 bytes, 2-byte aligned
    __le32 hash_value;              // offset 12, misaligned!
    __le16 hash_report;
    __le16 padding;
};

// CORRECT: split 4-byte field into two 2-byte fields
struct virtio_net_hdr_v1_hash {
    struct virtio_net_hdr_v1 hdr;
    __le16 hash_value_lo;           // offset 12, 2-byte aligned
    __le16 hash_value_hi;           // offset 14, 2-byte aligned
    __le16 hash_report;
    __le16 padding;
};
```

Use `BUILD_BUG_ON` assertions when casting between header formats to catch
alignment mismatches at compile time:

```c
BUILD_BUG_ON(__alignof__(struct_a) != __alignof__(struct_b));
```

For new fields added after an embedded struct, the field offset must satisfy
`offset % sizeof(field_type) == 0`. The alignment is inherited from the
embedded struct, not the desired field type.

## XFRM/IPsec Packet Family Determination

Using the wrong source for protocol family in XFRM code causes
protocol-specific header accessors (`ip_hdr()`, `ipv6_hdr()`) to be called
on packets of the wrong type, leading to incorrect packet parsing, silent
data corruption, or crashes.

`struct xfrm_state` (`include/net/xfrm.h`) contains multiple family-related
fields that may not match the actual packet in cross-family tunnels (e.g.,
IPv6-over-IPv4) and dual-stack configurations:

- `x->props.family`: the outer/tunnel address family
- `x->inner_mode.family`: primary inner address family
- `x->inner_mode_iaf.family`: alternative inner address family (dual-stack)
- `x->outer_mode.family`: outer mode address family

These are fields of `struct xfrm_mode` (which has `u8 encap`, `u8 family`,
`u8 flags`).

The most reliable source for the actual packet family is the packet itself
via `skb_dst(skb)->ops->family` (`struct dst_ops` in
`include/net/dst_ops.h`). The xfrm state fields indicate configured
families, not necessarily the family of the packet currently being
processed.

```c
// WRONG: relies on state field that may not match actual packet
switch (x->inner_mode.family) {
case AF_INET:
    iph = ip_hdr(skb);  /* crashes if packet is IPv6 */
    ...
}

// CORRECT: consult the actual packet's destination entry
switch (skb_dst(skb)->ops->family) {
case AF_INET:
    iph = ip_hdr(skb);
    ...
}
```

Inconsistent family sources within a single file or subsystem suggest bugs.
Be particularly suspicious of `x->props.family` when accessing inner packet
properties in tunnel mode.

## Quick Checks

- Validate packet lengths before `skb_put()` / `skb_push()` / `skb_pull()`
- Call `pskb_may_pull()` before dereferencing protocol headers
- Check `skb_shared()` / `skb_cloned()` before modifying SKB data
- Verify `htons()` / `ntohs()` conversions on all port and protocol comparisons
- Hold `rcu_read_lock()` during routing table lookups and dst access
- Use BH-safe stat update macros for per-CPU network counters
- Do not access an SKB after handing it to another subsystem
- Do not store destructor-needed data in `skb->cb`
