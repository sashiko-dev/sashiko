# SMB/ksmbd Subsystem Details

## SMB Direct (RDMA) Credit Grant Ordering

Sending credit-granting messages before the negotiation response causes
protocol violations, connection failures, or undefined behavior on the
client side. The negotiate response must be the first message that grants
credits to the peer.

Credits control how many outstanding send requests each side may have.
Both the negotiate response (`smb_direct_send_negotiate_response()` in
`fs/smb/server/transport_rdma.c`) and data transfer messages
(`smb_direct_create_header()`) set `credits_granted` via
`manage_credits_prior_sending()`. The protocol requires that no data
transfer message carrying a nonzero `credits_granted` is sent before the
negotiate response.

Two work items on the `smbdirect_socket` structure
(`fs/smb/common/smbdirect/smbdirect_socket.h`) can trigger
credit-granting sends:

| Work item | Handler (ksmbd) | Effect |
|---|---|---|
| `recv_io.posted.refill_work` | `smb_direct_post_recv_credits()` | Posts receive buffers, then queues `idle.immediate_work` if any credits were posted |
| `idle.immediate_work` | `smb_direct_send_immediate_work()` | Calls `smb_direct_post_send_data()` with zero payload, which sends a data transfer PDU carrying `credits_granted` |

The chain is: the `recv_io.posted.refill_work` handler posts receive
buffers, then calls `queue_work(sc->workqueue, &sc->idle.immediate_work)`.
The `idle.immediate_work` handler sends an empty data transfer message
that grants credits to the peer.

**Disabled-work initialization pattern:**

`smbdirect_socket_init()` initializes all work items with a dummy
handler (`__smbdirect_socket_disabled_work`, which fires `WARN_ON_ONCE`)
and immediately disables them via `disable_work_sync()`. This ensures
that `queue_work()` calls on a disabled work item are silently dropped.
The real handlers are assigned later via `INIT_WORK()` at the correct
point in the protocol state machine:

```c
// In smb_direct_prepare_negotiation() sequence (transport_rdma.c):
// 1. refill_work gets its real handler and runs synchronously
INIT_WORK(&sc->recv_io.posted.refill_work, smb_direct_post_recv_credits);
smb_direct_post_recv_credits(&sc->recv_io.posted.refill_work);
// At this point idle.immediate_work is still disabled, so the
// queue_work() inside smb_direct_post_recv_credits() is a no-op.

// 2. Only then does idle.immediate_work get its real handler
INIT_WORK(&sc->idle.immediate_work, smb_direct_send_immediate_work);

// 3. Finally the negotiate response is sent (first credit grant)
ret = smb_direct_send_negotiate_response(sc, ret);
```

Any change that enables `idle.immediate_work` before the negotiate
response is sent -- or that bypasses the `disable_work` mechanism --
breaks the credit grant ordering invariant.

## Quick Checks

- A change to `smbdirect_socket_init()` that removes `disable_work_sync()`
  calls or reorders `INIT_WORK()` assignments can expose premature credit
  grants
- Converting a `delayed_work` to a `work_struct` (or vice versa) can
  change when a handler first becomes runnable and may open a timing window
  before the negotiate response
