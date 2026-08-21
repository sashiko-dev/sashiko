# Networking Core: SKB, Sockets, and Packet Flow

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

## UAPI Structure Alignment Inheritance

Adding fields with wider alignment requirements to a structure that embeds
a narrowly-aligned UAPI struct causes misaligned memory accesses. On some
architectures this traps; on others it silently degrades performance (up to
50% throughput loss for hot-path headers) without functional failures,
making the bug difficult to detect through testing.

UAPI structures use only the types present in their definition to determine
alignment. When a structure embeds another, the outer structure inherits
the inner structure's alignment, not the alignment of any new fields added
after it.

The virtio network headers illustrate this pattern. `virtio_net_hdr_v1`
(`include/uapi/linux/virtio_net.h`) contains only `__u8` and `__virtio16`
fields, giving it 2-byte alignment. The embedding chain
`virtio_net_hdr_v1` -> `virtio_net_hdr_v1_hash` ->
`virtio_net_hdr_v1_hash_tunnel` preserves 2-byte alignment throughout by
using only `__le16` fields after the embedded struct. A `__le32` or `__u32`
field placed after the 12-byte `virtio_net_hdr_v1` would sit at a 2-byte
aligned offset, violating the field's natural 4-byte alignment requirement.

When extending UAPI structures that embed other structures:

- Check the embedded struct's alignment (`__alignof__`) -- new fields must
  not require wider alignment than the embedding struct provides.
- If a wider value is needed, split it into smaller fields that match the
  inherited alignment (e.g., two `__le16` fields instead of one `__le32`).
- Use `BUILD_BUG_ON(__alignof__(outer) != __alignof__(inner))` to catch
  alignment mismatches at compile time when casting between related header
  formats. See `xmit_skb()` in `drivers/net/virtio_net.c` for an example.
- Verify that `offset % sizeof(field_type) == 0` for every new field,
  where offset accounts for the inherited alignment, not the desired field
  type.

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
- Hold `rcu_read_lock()` during routing table lookups and dst access
- Use BH-safe stat update macros for per-CPU network counters
- Do not access an SKB after handing it to another subsystem
- Do not store destructor-needed data in `skb->cb`
- For UAPI structures embedding other structs, verify new fields don't require wider alignment than the embedded struct provides
