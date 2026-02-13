# SunRPC Subsystem Delta

## Overview

SunRPC (net/sunrpc/) provides the RPC transport layer for NFS and related
services. Includes client (rpc_clnt, rpc_task, xprt) and server (svc_serv,
svc_rqst, svc_xprt) infrastructure, with TCP, UDP, and RDMA transports
plus RPCSEC_GSS authentication.

**Generic analysis**: Reference ../callstack.md for caller/callee traversal,
lock validation, and resource lifetime tracking.

## File Applicability

| Files | Domain |
|-------|--------|
| svc.c, svc_xprt.c, clnt.c, sched.c, xprt.c | Core |
| svcsock.c, xprtsock.c | Socket |
| xprtrdma/svc_rdma_*.c, xprtrdma/*.c | RDMA |
| auth_gss/*.c, svcauth_gss.c, gss_krb5_*.c | GSS |

---

## Core Infrastructure Patterns [SUNRPC-CORE]

#### SUNRPC-CORE-001: Transport references for async work

**Risk**: Use-after-free

**Details**: Hold svc_xprt_get/xprt_get before queue_work; release in handler

#### SUNRPC-CORE-002: Transport state flag checks

**Risk**: Use-after-free, invalid operations

**Details**: Check XPT_CLOSE/XPT_DEAD (server) or XPRT_CONNECTED/XPRT_CLOSING
(client) before transport access

#### SUNRPC-CORE-003: Thread pool shutdown (server)

**Risk**: Controller hang

**Details**: svc_exit_thread() required on all thread stop paths to clear
SP_VICTIM_REMAINS; when kthread_stop() wins race, controller must call it

#### SUNRPC-CORE-004: Page array bounds (server)

**Risk**: Buffer overflow

**Details**: Check rq_next_page < rq_page_end before svc_rqst_replace_page()

#### SUNRPC-CORE-005: XPT_BUSY lifecycle (server)

**Risk**: Transport stuck forever

**Details**: Every svc_handle_xprt() exit must call svc_xprt_received() to
clear XPT_BUSY, including reservation failures

#### SUNRPC-CORE-006: Deferred request context (server)

**Risk**: Double-free

**Details**: When deferring, set dr->xprt_ctxt = rqstp->rq_xprt_ctxt then
rqstp->rq_xprt_ctxt = NULL; reverse on revisit

#### SUNRPC-CORE-007: RPC task state machine (client)

**Risk**: Task stall or corruption

**Details**: Each call_* state must set tk_action for next state; do not
modify tk_status when tk_action is NULL (task exiting)

#### SUNRPC-CORE-008: Slot release (client)

**Risk**: Slot exhaustion

**Details**: xprt_release() must occur on all paths after xprt_reserve()

#### SUNRPC-CORE-009: Congestion window locking (client)

**Risk**: Race condition

**Details**: Hold transport_lock when updating xprt->cong

#### SUNRPC-CORE-010: Timeout overflow (client)

**Risk**: Integer overflow

**Details**: Clamp rq_timeout after shift: if (rq_timeout > to_maxval)
rq_timeout = to_maxval

#### SUNRPC-CORE-011: rq_flags atomicity (server)

**Risk**: Race with svc_xprt_enqueue

**Details**: Use set_bit/clear_bit, not __set_bit/__clear_bit on rq_flags

#### SUNRPC-CORE-012: Transport reassignment (client)

**Risk**: Reference leak

**Details**: Release old xprt reference before assigning new transport

---

## Socket Transport Patterns [SUNRPC-SOCK]

#### SUNRPC-SOCK-001: Short read/write handling

**Risk**: Data corruption, partial processing

**Details**: TCP may return fewer bytes; loop until complete or use
MSG_WAITALL for fixed-size reads (e.g., 4-byte record marker)

#### SUNRPC-SOCK-002: Record marker bounds

**Risk**: Memory exhaustion, overflow

**Details**: Validate incoming record size before allocation

#### SUNRPC-SOCK-003: Listener callback inheritance (server)

**Risk**: Use-after-free

**Details**: Child sockets inherit sk_user_data from listener; callbacks
must check sk->sk_state == TCP_LISTEN BEFORE dereferencing sk_user_data

#### SUNRPC-SOCK-004: Callback teardown (client)

**Risk**: Race with in-flight callback

**Details**: lock_sock(); xs_restore_old_callbacks(); sk->sk_user_data = NULL;
release_sock(); then sock_release()

#### SUNRPC-SOCK-005: Cork balance (server)

**Risk**: Cork leaked

**Details**: tcp_sock_set_cork(sk, false) on all paths including errors

#### SUNRPC-SOCK-006: Reconnection backoff (client)

**Risk**: Connection storm

**Details**: Use xprt_reconnect_delay() and queue_delayed_work, not
immediate queue_work

#### SUNRPC-SOCK-007: NOFS allocation (client)

**Risk**: Deadlock in reclaim

**Details**: Use memalloc_nofs_save/restore around socket operations

#### SUNRPC-SOCK-008: State flag barriers

**Risk**: Inconsistent state visibility

**Details**: Use smp_mb__after_atomic() between related flag changes

#### SUNRPC-SOCK-009: New XPRT_SOCK_* flags (client)

**Risk**: Flag persists across reset

**Details**: Add clear_bit in xs_sock_reset_state_flags() for new flags

#### SUNRPC-SOCK-010: Write space callback (server)

**Risk**: Deadlock

**Details**: svc_write_space() must call svc_xprt_enqueue() when write
space available; read/write coupling means server stops reading when
it cannot write

---

## RDMA Transport Patterns [SUNRPC-RDMA]

#### SUNRPC-RDMA-001: DMA mapping lifecycle

**Risk**: Use-after-free, data corruption

**Details**: DMA mappings must persist until completion callback fires;
check ib_dma_map_sg() return (mr_nents == 0 is failure); unmap in
completion handler or after synchronous wait

#### SUNRPC-RDMA-002: MR/FRWR invalidation before reuse

**Risk**: Data corruption, protocol error

**Details**: Memory Regions must complete invalidation before reuse;
frwr_unmap_sync() blocks, frwr_unmap_async() does not—do not reuse MR
immediately after async invalidate

#### SUNRPC-RDMA-003: Completion status check

**Risk**: Garbage data, crash

**Details**: Only wr_cqe and status reliable in work completion; check
wc->status == IB_WC_SUCCESS before accessing byte_len or other fields

#### SUNRPC-RDMA-004: Device removal handling

**Risk**: Crash

**Details**: On IB_WC_WR_FLUSH_ERR, device may be gone; do not call
ib_dma_* functions in flush error paths

#### SUNRPC-RDMA-005: Post-send UAF (server)

**Risk**: Use-after-free

**Details**: Completion handler can fire immediately after ib_post_send();
copy context fields to stack before posting if needed for error handling
or tracing

#### SUNRPC-RDMA-006: SQ accounting and flag ordering (server)

**Risk**: Racing reallocation

**Details**: set_bit(XPT_CLOSE) BEFORE svc_rdma_*_ctxt_put() to prevent
racing completion from reallocating freed context

#### SUNRPC-RDMA-007: CM event reference balance (client)

**Risk**: Reference underflow

**Details**: ESTABLISHED takes reference via rpcrdma_ep_get();
DISCONNECTED releases via rpcrdma_ep_put(); DEVICE_REMOVAL/ADDR_CHANGE
may fire before ESTABLISHED—track whether reference was taken

#### SUNRPC-RDMA-008: Reconnect DMA remapping (client)

**Risk**: LOCAL_PROT_ERR on reconnect

**Details**: If teardown calls rpcrdma_regbuf_dma_unmap(), verify
reconnect path remaps before posting receives

#### SUNRPC-RDMA-009: Credit flow ordering (client)

**Risk**: RNR (Receiver Not Ready)

**Details**: Call rpcrdma_post_recvs() BEFORE rpcrdma_update_cwnd();
opening credits wakes senders who need posted receives

---

## GSS Authentication Patterns [SUNRPC-GSS]

#### SUNRPC-GSS-001: Sequence window locking (server)

**Risk**: Replay attack

**Details**: Hold sd_lock for all sequence window operations (check,
advance, set bit); verify arithmetic handles overflow near MAXSEQ and
underflow in sd_max - GSS_SEQ_WIN

#### SUNRPC-GSS-002: Context cache reference counting (server)

**Risk**: Use-after-free

**Details**: gss_svc_searchbyctx() returns referenced entry; use context,
then cache_put(); do not access after put

#### SUNRPC-GSS-003: Cryptographic result checking

**Risk**: Authentication bypass, forged requests

**Details**: Check gss_verify_mic/gss_wrap/gss_unwrap return status;
abort request on any GSS_S_* error

#### SUNRPC-GSS-004: Buffer slack sizing (client)

**Risk**: Buffer overrun

**Details**: For Kerberos v2 (RFC 4121), au_ralign != au_rslack because
checksum follows cleartext; account for GSS_KRB5_TOK_HDR_LEN + checksum

#### SUNRPC-GSS-005: Credential lifecycle

**Risk**: Use-after-free

**Details**: Server: get_group_info() before using rsci->cred, release
via free_svc_cred(); Client: put_rpccred() only after all use complete

#### SUNRPC-GSS-006: Upcall matching

**Risk**: Wrong context returned

**Details**: __gss_find_upcall() must match uid + service + in-flight
state; insufficient criteria can pair wrong upcall/downcall

#### SUNRPC-GSS-007: Deferred request verification (server)

**Risk**: Double verification overhead

**Details**: Skip re-verification for rqstp->rq_deferred requests;
already verified on first pass

---

## Network Namespace

All SunRPC code must respect network namespace boundaries:

- Server transport: xprt->xpt_net, serv->sv_net
- Client transport: xprt->xprt_net, clnt->cl_xprt->xprt_net
- Socket creation: sock_create_kern(net, ...)
- RDMA: rdma_create_id(net, ...), rdma_dev_access_netns()
- Never use &init_net or current->nsproxy->net_ns

---

## Quick Reference

**Transport state flags:**
- Server: XPT_BUSY, XPT_CLOSE, XPT_DEAD, XPT_DATA, XPT_CONN
- Client: XPRT_CONNECTED, XPRT_CONNECTING, XPRT_CLOSING

**Reference counting functions:**
- Server transport: svc_xprt_get/put
- Client transport: xprt_get/put
- RPC task: rpc_get_task/put_task
- GSS context: gss_get_ctx/put_ctx

**Common invariants:**
- Check CLOSE/DEAD flags before transport access
- Hold reference before queuing async work
- Release resources on all error paths
- Validate GSS results before proceeding

---

## Stop Conditions

Flag for expert review when:
- Service/pool allocation or thread management modified
- New XPT_*/XPRT_* flags introduced
- FSM state transitions or slot allocation algorithm changed
- Callback registration/deregistration affected
- FRWR registration/invalidation sequencing modified
- CM event handler or completion ordering changed
- GSS context establishment or sequence window logic changed
- Cryptographic algorithm selection or upcall mechanism modified
- Changes exceed 100 lines touching multiple core files
