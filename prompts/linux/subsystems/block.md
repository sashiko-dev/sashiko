# Block Layer Subsystem Details

## Queue Freezing Synchronization

Failing to hold a queue freeze during teardown or reconfiguration allows bios
to complete concurrently, causing use-after-free on queue state, stale
elevator data, or torn reads of `q->nr_requests`.

`blk_mq_freeze_queue()` waits for `q->q_usage_counter` (a `percpu_ref`) to
reach zero. Every bio submission path acquires this counter via
`blk_try_enter_queue()` (called from `blk_queue_enter()`), and releases it
via `blk_queue_exit()`. While the queue is frozen, new `blk_try_enter_queue()`
calls fail, ensuring no new bios enter the queue. Code protected by a freeze
can safely tear down or modify queue state without racing bio completion paths.

Both `blk_mq_freeze_queue()` and `blk_mq_unfreeze_queue()` return `void`.
Callers managing memory reclaim constraints around frozen sections typically
use scoped `memalloc_noio_save()` / `memalloc_noio_restore()`.

## Bio Operation Type Safety

Accessing bio data fields on a bio that has no data buffers (e.g., discard,
flush) causes a NULL pointer dereference. Code that handles bios must check
the operation type before accessing data fields.

| Operation | `bi_io_vec` | Has Data |
|-----------|-------------|----------|
| `REQ_OP_READ` | Valid | Yes |
| `REQ_OP_WRITE` | Valid | Yes |
| `REQ_OP_DISCARD` | NULL | No |
| `REQ_OP_FLUSH` | NULL | No |
| `REQ_OP_WRITE_ZEROES` | NULL | No |
| `REQ_OP_SECURE_ERASE` | NULL | No |

**Data field accesses that require guards:**
- Direct: `bio->bi_io_vec`, `bio->bi_vcnt`, `bio->bi_iter.bi_bvec_done`
- Indirect: `bio_first_bvec_all()`, `bio_last_bvec_all()`,
  `bio_for_each_bvec()`, `bio_for_each_segment()`

**Required guard:** `bio_has_data()` before accessing any data field. Note
that `op_is_write()` is NOT a valid guard — it checks bit 0 of the op code,
so it returns true for `REQ_OP_DISCARD` (3), `REQ_OP_SECURE_ERASE` (5), and
`REQ_OP_WRITE_ZEROES` (9), all of which have no data. `bio_has_data()`
correctly excludes these by checking for them explicitly (in addition to
requiring nonzero `bi_iter.bi_size`).

## Bio Mempool Allocation Guarantees

Treating a mempool-backed bio allocation failure path as reachable under
`GFP_NOIO`/`GFP_NOFS` is dead code. Conversely, omitting error handling for
`bio_kmalloc()` or `GFP_NOWAIT` allocations causes NULL dereferences under
memory pressure.

- `bio_alloc()` / `bio_alloc_bioset()` — mempool-backed; cannot fail when
  `__GFP_DIRECT_RECLAIM` is set (which `GFP_NOIO` and `GFP_NOFS` include).
  Failure paths are only reachable with `GFP_NOWAIT`/`GFP_ATOMIC`.
- `bvec_alloc()` — first tries slab allocation; if that fails and
  `__GFP_DIRECT_RECLAIM` is set, falls back to mempool (cannot fail).
- `bio_integrity_prep()` — allocates from mempool with `GFP_NOIO`; always
  returns `true`.
- `bio_integrity_alloc_buf()` — tries `kmalloc()` with `GFP_NOIO` minus
  `__GFP_DIRECT_RECLAIM`; on failure, falls back to `mempool_alloc()` with
  `GFP_NOFS` (cannot fail).
- `bio_kmalloc()` — uses plain `kmalloc()` with NO mempool backing. Can fail
  regardless of GFP flags.

## Elevator `depth_updated` Callback

Calling `depth_updated()` before `q->nr_requests` is written causes elevators
to compute internal limits (e.g., `async_depth` in mq-deadline and kyber,
`async_depths` array in BFQ) from stale values. Omitting `depth_updated()`
during `init_sched()` leaves these limits at zero until the first
`nr_requests` sysfs write.

The callback signature is defined in `struct elevator_mq_ops` (`include/linux/elevator.h`):
`void (*depth_updated)(struct blk_mq_hw_ctx *)`. All elevator state
derived from `q->nr_requests` is per-queue or per-hardware-context.

**Timing invariant:** In `blk_mq_update_nr_requests()` (`block/blk-mq.c`),
`q->nr_requests` is set before `depth_updated()` is called. Any change that
reorders this assignment relative to the callback will introduce stale-value
bugs in all three in-tree elevators (mq-deadline, BFQ, kyber).

**Initialization invariant:** All three in-tree elevators invoke their
`depth_updated()` implementation during queue/sched initialization:
- `bfq_init_queue()` calls `bfq_depth_updated()` (`block/bfq-iosched.c`)
- `dd_init_sched()` calls `dd_depth_updated()` (`block/mq-deadline.c`)
- `kyber_init_sched()` calls `kyber_depth_updated()` (`block/kyber-iosched.c`)

A new elevator that derives limits from `q->nr_requests` must also call
`depth_updated()` during initialization, not only at runtime.

## Quick Checks

- `REQ_OP_ZONE_APPEND` (7) has bit 0 set, so `op_is_write()` returns true
  and it does carry data — unlike DISCARD/WRITE_ZEROES/SECURE_ERASE.
- `bio_alloc_bioset()` can return NULL even with `__GFP_DIRECT_RECLAIM` if
  `nr_vecs > BIO_INLINE_VECS` and the bioset was not initialized with a bvec pool
  (triggers `WARN_ON_ONCE` and returns NULL).
