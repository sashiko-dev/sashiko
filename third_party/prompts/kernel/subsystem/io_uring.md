# io_uring Subsystem Details

## Zero-Copy Lifetime Management

Attaching `buf_node` to `req` instead of `notif` in zero-copy operations
causes use-after-free, because `req` completes before the network/block
layer finishes with the buffers. The `notif` (`struct io_kiocb`) is
completed via `io_tx_ubuf_complete()` only when transmission finishes, so
buffer references (`struct io_rsrc_node`) must be attached to it.

**Zero-copy operations**: `IORING_OP_SEND_ZC` and `IORING_OP_SENDMSG_ZC`.
Internally these set `MSG_ZEROCOPY` via `io_send_zc_prep()`.

**Buffer import attachment**: `io_import_reg_buf()` and
`io_import_reg_vec()` call `io_find_buf_node()`, which attaches `buf_node`
to the passed `io_kiocb` — first arg to `io_import_reg_buf()`, third arg
to `io_import_reg_vec()`.

```c
io_import_reg_buf(sr->notif, ...)                        // CORRECT
io_import_reg_vec(ITER_SOURCE, &msg_iter, sr->notif, ...) // CORRECT
io_import_reg_buf(req, ...)                               // WRONG
io_import_reg_vec(ITER_SOURCE, &msg_iter, req, ...)       // WRONG
```

When reviewing vectored zero-copy operations, compare with the non-vectored
equivalent for consistency. `IORING_OP_SEND_ZC` uses `io_send_zc_import()`
which correctly passes `sr->notif`.

**Notification flush ordering**: `io_notif_flush()` drops the notif
reference, so calling it twice causes use-after-free. After flushing,
immediately NULL the pointer (`zc->notif = NULL`). Both the fast path
(inline flush) and cleanup path (`io_send_zc_cleanup()` via
`io_clean_op()`) can flush. See `io_send_zc()` in `io_uring/net.c`.

**REPORT as bugs**: Buffer import passing `req` instead of `notif` in
zero-copy ops. `io_notif_flush()` not followed by NULLing the pointer.

## REQ_F_NEED_CLEANUP and Cleanup Flag Safety

Missing `REQ_F_NEED_CLEANUP` before an early-return error in a prep
function causes resource leaks. `io_clean_op()` checks this flag to call
`io_cold_defs[req->opcode].cleanup`; without it, resources are never freed.

**Invariant**: Set `REQ_F_NEED_CLEANUP` *immediately after* any allocation
whose release depends on the opcode's cleanup handler. No early return may
exist between allocation and flag-setting.

```c
open->filename = getname(fname);
if (IS_ERR(open->filename))
    return PTR_ERR(open->filename);
req->flags |= REQ_F_NEED_CLEANUP;  // CORRECT: before any validation
// ... validation that may return -EINVAL is safe here ...
```

**Clearing rules**: Only clear `REQ_F_NEED_CLEANUP` / `REQ_F_ASYNC_DATA`
after the resource is successfully recycled or freed. `io_netmsg_recycle()`
clears flags only inside the `io_alloc_cache_put()` success branch. See
`io_uring/net.c`.

**REPORT as bugs**: `REQ_F_NEED_CLEANUP` set after a code path that can
return early. Unconditional clearing of `REQ_F_NEED_CLEANUP` or
`REQ_F_ASYNC_DATA` before confirming the resource was recycled/freed.

## Async Data Lifecycle

`req->async_data` and `REQ_F_ASYNC_DATA` must always be in sync.
`io_clean_op()` checks only the flag; mismatches cause use-after-free or
double-free.

**Rules**:
- Use `io_uring_alloc_async_data(cache, req)` to allocate (sets flag on success)
- Use `io_req_async_data_free(req)` to free (clears pointer + flag)
- Use `io_req_async_data_clear(req, extra_flags)` for cache-returned data
- Never manually assign `req->async_data` without setting the flag, or
  `kfree()` it without using the helpers
- Allocate in `prep`, not `issue` — the data must exist before retry or
  cancellation. See `io_waitid_prep()` in `io_uring/waitid.c`.

All helpers are in `io_uring/io_uring.h`.

**REPORT as bugs**: Manual `req->async_data` assignment without
`REQ_F_ASYNC_DATA`. Direct `kfree(req->async_data)` bypassing helpers.
Async data allocated in `issue` rather than `prep` for cancellable ops.

## SQE Data Stability for uring_cmd

`IORING_OP_URING_CMD` delegates SQE interpretation to `f_op->uring_cmd()`,
which may access SQE fields long after prep. If the SQE slot is reused
before copying, the handler reads stale data.

**Rules**:
- Standard opcodes read all SQE fields during `prep`. `uring_cmd` is
  different: `ioucmd->sqe` points to the ring slot and is used at issue
  time and from async completions.
- On async punt, `io_uring_cmd_sqe_copy()` copies the SQE into
  `ac->sqes` and updates `ioucmd->sqe`. `REQ_F_SQE_COPIED` prevents
  double copies. Both copy and pointer update must happen together.
- SQE fields needed at issue must be cached during prep with
  `READ_ONCE()` (e.g., `ioucmd->cmd_op = READ_ONCE(sqe->cmd_op)`).
  Issue code must use cached values, not `cmd->sqe->`. See
  `io_uring_cmd_prep()` in `io_uring/uring_cmd.c` and
  `io_uring_cmd_sock()` in `io_uring/cmd_net.c`.

**REPORT as bugs**: `ioucmd->sqe` accessed after async punt without copy.
Issue code reading SQE fields through `cmd->sqe->` instead of cached values.

## CQE Sizing Modes: CQE32 vs CQE_MIXED

Wrong `cqe32` boolean to `io_get_cqe()` / `io_get_cqe_overflow()` causes
CQ tail mis-advancement and data corruption. The parameter means "mixed-mode
per-CQE 32B entry needing extra advancement," NOT "this CQE is 32 bytes."

The two modes are mutually exclusive (enforced in `io_uring_create()`):

| Ring mode | `cqe32` param | Why |
|---|---|---|
| `IORING_SETUP_CQE32` | `false` | Ring doubles all slots; handled internally |
| `IORING_SETUP_CQE_MIXED` + `IORING_CQE_F_32` | `true` | Per-CQE extra slot needed |
| `IORING_SETUP_CQE_MIXED` w/o `IORING_CQE_F_32` | `false` | Standard 16B entry |
| Default 16B ring | `false` | Standard 16B entry |

Derive `cqe32` from the per-CQE `IORING_CQE_F_32` flag, NOT ring-level
`IORING_SETUP_CQE32`. See `io_fill_cqe_req()` in `io_uring/io_uring.h`.

**REPORT as bugs**: `cqe32=true` based on `IORING_SETUP_CQE32` rather than
`IORING_CQE_F_32`.

## Multishot and CQE Posting

Wrong context for CQE posting or incorrect multishot return values causes
warnings, crashes, or hung requests.

**CQE posting**: `io_req_post_cqe()` requires task_work context with
`uring_lock` held, never io-wq. Any request calling it must have
`REQ_F_MULTISHOT` or `REQ_F_APOLL_MULTISHOT` set so
`io_wq_submit_work()` handles it. See `io_uring/io_uring.c`.

**Return values**: `REQ_F_APOLL_MULTISHOT` means the request *supports*
multishot; `issue_flags & IO_URING_F_MULTISHOT` means it's *executing* in
multishot context. Check the latter before returning multishot-specific
status codes (e.g., `IOU_STOP_MULTISHOT`).

**Async punt**: Multishot handlers must not return `-EAGAIN` for io-wq punt.
See `__io_read()` in `io_uring/rw.c`.

**Buffer recycling**: Call `io_kbuf_recycle()` before returning to poll
waiting. See `io_recv()` in `io_uring/net.c`.

**REPORT as bugs**: `io_req_post_cqe()` without `REQ_F_MULTISHOT` /
`REQ_F_APOLL_MULTISHOT`. Multishot status without `IO_URING_F_MULTISHOT`
check. Multishot handler returning `-EAGAIN` for async punt.

## Provided Buffer Ring Semantics

Provided buffer rings (`struct io_uring_buf` in `buf_ring`) are in
userspace-shared memory. Incorrect access or commit ordering causes data
corruption, infinite loops, or dangling pointers.

**Shared memory**: All `buf->len`, `buf->addr`, `buf->bid` reads must use
`READ_ONCE()` into a local; writes must use `WRITE_ONCE()`. Legacy
`struct io_buffer` (kernel-only lists) needs no annotations. See
`io_ring_buffer_select()` and `io_kbuf_inc_commit()` in `io_uring/kbuf.c`.

**Auto-commit vs explicit commit** (`io_should_commit()` in
`io_uring/kbuf.c`):
- `IO_URING_F_UNLOCKED`: always auto-commit
- Non-pollable, non-uring_cmd: auto-commit
- Pollable or `IORING_OP_URING_CMD`: skip (operation commits explicitly)

New opcodes with explicit commit must be exempted in `io_should_commit()`.

**Address capture**: Capture `buf->addr` before `io_kbuf_commit()`, which
may modify buffer metadata.

**Retry**: On partial completion, commit via `io_kbuf_commit()` and set
`REQ_F_BL_NO_RECYCLE` before returning. See `io_net_kbuf_recyle()` in
`io_uring/net.c`.

**Incremental consumption**: `io_kbuf_inc_commit()` must terminate on
zero-length buffers.

**REPORT as bugs**: `buf_ring` field access without `READ_ONCE()` /
`WRITE_ONCE()`. Explicit-commit opcode not exempted in `io_should_commit()`.
`REQ_F_BL_NO_RECYCLE` set without committing.

## Registered Buffer Management

Wrong offset calculations for pre-registered buffer bvecs cause silent data
corruption or out-of-bounds access.

**Rules**:
- Never assume `imu->ubuf` is page/folio-aligned. Use
  `imu->bvec[0].bv_offset` for sub-folio offset, not address masking.
  See `io_vec_fill_bvec()` in `io_uring/rsrc.c`:
  ```c
  offset = buf_addr - imu->ubuf;
  offset += imu->bvec[0].bv_offset;  // CORRECT
  ```
- During registration with folio coalescing,
  `data.first_folio_page_idx << PAGE_SHIFT` accounts for the first page's
  position within its folio. See `io_sqe_buffer_register()` in
  `io_uring/rsrc.c`.
- Use `unpin_user_folio()`, never `unpin_user_page()` — compound folios
  cause a BUG in `sanity_check_pinned_pages()`. See `io_release_ubuf()`.

**REPORT as bugs**: Offset derived by masking virtual address instead of
`imu->bvec[0].bv_offset`. `unpin_user_page()` in io_uring buffer code.

## msg_ring Cross-Ring Request Lifetime

msg_ring-allocated `io_kiocb` freed before the RCU grace period causes
use-after-free. `io_msg_data_remote()` allocates via `kmem_cache_alloc()`
and posts to a remote ring; `io_msg_tw_complete()` frees via `kfree_rcu()`.
See `io_uring/msg_ring.c`.

**Rules**:
- Free via `kfree_rcu(req, rcu_head)`, never `kmem_cache_free()` / `kfree()`
- Never place into `io_alloc_cache` (bypasses RCU guarantees)
- Set `req->tctx = NULL` on remote requests — the submitter may exit.
  See `io_msg_remote_post()`.

**REPORT as bugs**: msg_ring request in `io_alloc_cache`, freed without
`kfree_rcu()`, or with non-NULL `req->tctx`.

## SQPOLL Thread Safety

Bare `sqd->thread` access causes use-after-free — the `task_struct` is
freed via RCU after thread exit. `sqd->thread` is `__rcu`-annotated
(`io_uring/sqpoll.h`).

**Access rules**:
- Under `sqd->lock`: `sqpoll_task_locked(sqd)`
- Under RCU: `rcu_dereference(sqd->thread)`
- Assignment: `rcu_assign_pointer(sqd->thread, tsk)`
- For signaling: use `req->tctx->task`, not `sqd->thread`. See
  `io_req_normal_work_add()` in `io_uring/io_uring.c`.

**Task ownership**: After `wake_up_new_task()` in
`io_sq_offload_create()`, the thread owns its reference. The creator must
NOT call `put_task_struct()` after start.

**REPORT as bugs**: Bare `sqd->thread` read. `put_task_struct()` after
thread start. `sqd->thread` used for signaling.

## DEFER_TASKRUN Task Work Draining

With `IORING_SETUP_DEFER_TASKRUN`, task work goes to `ctx->work_llist`. In
non-submitter contexts (e.g., `io_ring_exit_work()`), only
`io_move_task_work_from_local()` can drain it. Calling it once before a
cancel loop leaves new work (generated during cancellation) undrained,
causing 100% CPU spin.

**Rule**: Any cancel loop calling `io_uring_try_cancel_requests()` outside
the submitter must call `io_move_task_work_from_local()` on every iteration.
See `io_ring_exit_work()` in `io_uring/io_uring.c`.

**REPORT as bugs**: `io_move_task_work_from_local()` called only once before
a cancel loop instead of on each iteration.

## IOPOLL Completion and Reissue

IOPOLL posts CQEs when `iopoll_completed` is set. Early return without
setting it makes the request invisible to polling, causing a hang.

**Rules**:
- `io_complete_rw_iopoll()` must always reach
  `smp_store_release(&req->iopoll_completed, 1)`. Never return early.
  See `io_uring/rw.c`.
- IOPOLL reissue (`-EAGAIN`): set `REQ_F_REISSUE | REQ_F_BL_NO_RECYCLE`
  and fall through. Actual reissue is handled by flush logic.
- Non-IOPOLL uses `io_req_task_queue_reissue()`. IOPOLL must not.

**REPORT as bugs**: Early return in `io_complete_rw_iopoll()` skipping
`iopoll_completed`. IOPOLL path calling `io_req_task_queue_reissue()`.

## kiocb Flag Lifecycle on Retry

`kiocb->ki_flags |= file->f_iocb_flags` (merge) instead of `=`
(overwrite) causes stale flags (`IOCB_NOWAIT`, `IOCB_WAITQ`) to persist
across retries.

**Rules**:
- Overwrite: `kiocb->ki_flags = file->f_iocb_flags` in
  `io_rw_init_file()`. See `io_uring/rw.c`.
- Persistent state goes in `req->flags` (e.g., `REQ_F_HAS_METADATA`), not
  `kiocb->ki_flags`. Late flags (e.g., `IOCB_HAS_METADATA`) are set after
  the overwrite, guarded by the `req->flags` bit.

**REPORT as bugs**: `|=` instead of `=` for `file->f_iocb_flags` in
`io_rw_init_file()`.

## Timeout Cancellation and Lock Ordering

Queuing task_work while holding `ctx->timeout_lock` (raw spinlock) causes
lock ordering violations — completion may call `io_eventfd_signal()` which
takes a regular spinlock, invalid on PREEMPT_RT.

**Two-phase pattern**:
1. Under `timeout_lock`: `io_kill_timeout()` cancels hrtimers and moves
   timeouts to a local list (never completes them).
2. After unlock: `io_flush_killed_timeouts()` calls
   `io_req_queue_tw_complete()`.

See `io_uring/timeout.c`.

**REPORT as bugs**: Task_work or completion calls while holding
`ctx->timeout_lock`.

## Quick Checks

- **Notif allocation before import**: `io_alloc_notif()` must precede
  buffer import in zero-copy paths.
- **Zero-copy flag detection**: `IORING_OP_SEND_ZC`, `IORING_OP_SENDMSG_ZC`,
  or `IORING_RECVSEND_FIXED_BUF` with zc opcode require buffer lifetime
  validation.
- **Bundle buffer put**: `io_put_kbufs()` takes current transfer count
  (`this_ret`), not cumulative total. See `io_recv_finish()` in
  `io_uring/net.c`.
- **CQ overflow critical section**: Set `ctx->cqe_sentinel = ctx->cqe_cached`
  before dropping CQ lock during overflow flush, forcing concurrent emitters
  through `io_cqe_cache_refill()`. See `__io_cqring_overflow_flush()`.
- **zcrx DMA lifecycle**: Create DMA mappings in `io_pp_zc_init()`, unmap in
  `io_pp_uninstall()`. See `io_uring/zcrx.c`.
- **Registration tags on failure**: Clear `node->tag` via
  `io_clear_table_tags()` before unregistering a failed all-or-nothing
  registration. See `io_uring/rsrc.c`.
- **Inflight tracking for MM-dependent requests**: Call
  `io_req_track_inflight()` in prep for requests needing the submitter's
  `mm`. See `io_futex_prep()` in `io_uring/futex.c`.
- **Task work tokens**: Never fabricate `io_tw_token_t` on stack. Use
  `io_req_queue_tw_complete()` outside task_work context, not
  `io_req_task_complete()`.
- **Buffer list upgrade safety**: Destroy old `io_buffer_list` and allocate
  fresh when upgrading to ring-mapped buffers. See `io_register_pbuf_ring()`
  in `io_uring/kbuf.c`.
- **Eventfd RCU freeing**: Use `io_eventfd_put()` (calls `call_rcu()`),
  never `io_eventfd_free()` directly. See `io_eventfd_do_signal()` in
  `io_uring/eventfd.c`.
- **Multishot timeout re-arm**: Store new value in `data->ts`, not just pass
  to `hrtimer_start()`. See `io_uring/timeout.c`.
- **Cross-ring cloning accounting**: Both rings must share `ctx->user` and
  `ctx->mm_account`. See `io_clone_buffers()` in `io_uring/rsrc.c`.
- **io_wq NULL after teardown**: `io_queue_iowq()` checks `!tctx->io_wq`
  and `PF_KTHREAD`. See `io_uring/io_uring.c`.
- **SQE flag hierarchy**: Gate `READ_ONCE(sqe->field)` on the broadest flag
  covering all variants. See `io_nop_prep()` in `io_uring/nop.c`.
- **SQE fields read before use**: `READ_ONCE()` SQE field into request
  before using it — `req->buf_index` may hold stale data from reuse. See
  `io_uring_cmd_prep()` in `io_uring/uring_cmd.c`.
- **RESIZE_RINGS and DEFER_TASKRUN**: `io_register_resize_rings()` requires
  `IORING_SETUP_DEFER_TASKRUN`. New ring-geometry mutations need the same
  mutual exclusion. See `io_uring/register.c`.
- **Poll event scope**: Generic poll code (`io_uring/poll.c`) must not
  interpret event bits as errors — `POLLERR` signals data availability for
  some sockets (e.g., `MSG_ERRQUEUE`). Operation-specific interpretation
  belongs in issue handlers.
- **REQ_F_ISREG correctness**: `io_file_get_flags()` uses `S_ISREG()`,
  currently correct because VFS no longer sets `S_IFREG` on anonymous
  inodes (commit `1e7ab6f67824`). Watch for VFS changes that reintroduce
  `S_IFREG` on non-disk-backed files.
