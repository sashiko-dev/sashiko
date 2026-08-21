# NFS Server Subsystem

NFSD (fs/nfsd/) implements the Linux NFS server (v2/v3/v4.x). Key subsystems:
XDR codec, stateid/delegation state machine, file handle validation, client
lifecycle, callbacks, session slots. NFSv4 XDR has auto-generated code
(xdrgen).

## File Layout

| Files | Domain |
|-------|--------|
| nfs4xdr.c, nfs3xdr.c | XDR codec, page encoding |
| nfs4state.c, nfs4proc.c | NFSv4 state, operations, copy offload |
| nfs3proc.c, nfsproc.c | NFSv2/v3 operations |
| vfs.c, nfsfh.c, nfsfh.h | VFS interface, file handles, splice |
| nfs4callback.c | Callbacks |
| nfs4layouts.c | pNFS layouts |
| filecache.c, nfscache.c | File cache, DRC |
| export.c, nfsctl.c, netlink.c | Export mgmt, admin, netlink |
| nfs4recover.c | Grace period, reclaim |
| state.h, netns.h | Data structures |

## Trust Boundaries

```
XDR decode (untrusted) → fh_verify() → nfs4_preprocess_stateid_op() → VFS (trusted)
```

All client-supplied data is untrusted until validated. `fh_verify()` validates
file handles and permissions. `nfs4_preprocess_stateid_op()` validates stateids
for stateful operations.

## XDR Codec

XDR decode functions (`xdr_stream_decode_*()`) return error codes that must be
checked before using decoded values. Decoded lengths and counts are
client-controlled and require bounds checking before use as allocation sizes,
loop bounds, or array indices.

**Decode validation requirements:**
- Check return value before using decoded variable
- Bounds-check lengths before `kmalloc()` - use `check_mul_overflow()` for
  `count * sizeof(...)` calculations
- Validate array indices (e.g., slot indices against `maxreqs`, opnums)

**Encode requirements:**
- Check `xdr_reserve_space()` return for NULL before use
- Check `xdr_stream_encode_*()` return values
- Complete state changes only after encode succeeds - if encode fails after
  irreversible changes (close, revoke, rename), the operation cannot be retried safely
- Copy stateid before `nfs4_put_stid()`, not after (put may free)
- Subtract header overhead from client-supplied `maxcount`
- Reserve space for trailing fields (eof, count) in encode loops
- Encode all RFC-required fields unconditionally

**Replay cache:** `so_replay` / `rp_buf` must only be populated after encode
success. Unconditional `memcpy` to `rp_buf` after encode failure caches garbage.

**Trusted sources:** xdrgen code (nfs4xdr_gen.c) has built-in
validation. Metadata from `fh_dentry` after `fh_verify()` is trusted.

## Reference Counting

NFSD uses multiple reference count types with different semantics:

| Counter | Prevents | Used For |
|---------|----------|----------|
| `sc_count` | Freeing | Stateid lifetime |
| `cl_nfsdfs.cl_ref` | Freeing | nfsdfs client object lifetime |
| `cl_rpc_users` | Unhashing | Keeping client active during incoming RPC compounds |
| `cl_cb_inflight` | Client destruction | Tracking in-flight outgoing callbacks |

**`cl_rpc_users` vs `cl_nfsdfs.cl_ref` vs `cl_cb_inflight`:**
`cl_nfsdfs.cl_ref` only prevents freeing of the nfsdfs client object.
`cl_rpc_users` prevents unhashing — used for incoming RPC compounds and async
operations like copy workers that need the client to remain active past the
compound's lifetime. `cl_cb_inflight` tracks outgoing callbacks;
`nfsd4_run_cb()` increments it internally, and `destroy_client()` waits for
it to drain via `nfsd4_shutdown_callback()`. Do not confuse `cl_rpc_users`
with `cl_cb_inflight` — they protect different directions of communication.

**Assignment timing:** Assign resources to struct fields only after validation
completes. Use temp variables until validation passes. Pattern from
`nfsd_set_fh_dentry()` fix (b3da9b141578).

**Refcount pairs:**
- `nfs4_get_stid` / `nfs4_put_stid`
- `nfsd_file_get` / `nfsd_file_put`
- `exp_get` / `exp_put`
- `fh_put` (copy semantics)
- `nfsd_net_try_get` / `nfsd_net_put`
- Async copy: `cl_rpc_users` / `put_client_renew`
- `nfs4_get_stateowner` / `nfs4_put_stateowner`

Transfer semantics (function "steals" a reference) are acceptable when documented.

**Stateowner refcounting (`so_count`):** Hash/list membership on `cl_ownerstr_hashtbl`
and `cl_openowners` is NOT a counted reference — `hash_openowner()` does no
`atomic_inc`. The `so_count=1` set by `alloc_stateowner()` is the creation
reference transferred to the caller. Stateids take additional counted refs via
`init_open_stateid()` → `nfs4_get_stateowner()` (stored in `stp->st_stateowner`).
Lookups via `find_openstateowner_str()` / `find_lockowner_str_locked()` bump
`so_count` for the caller.

`release_openowner()` consumes exactly one caller-provided reference: it unhashes,
drains stateids (each `nfs4_free_ol_stateid` drops its owner ref via
`nfs4_put_stateowner`), then issues one final `nfs4_put_stateowner` which reaches
`so_count=0` and frees the owner. Both callers (`find_or_alloc_open_stateowner` and
`__destroy_client`) hold a pin (`find_openstateowner_str` ref or explicit
`nfs4_get_stateowner`) that `release_openowner` consumes — do NOT add an extra
`nfs4_put_stateowner` after `release_openowner` returns.

## File Handle Lifecycle

`fh_dentry`, `fh_export`, and `d_inode()` are NULL/invalid before `fh_verify()`
succeeds. Access before verification causes NULL dereference or stale data use.

**Permission flags:** `NFSD_MAY_*` flags must match the operation:
- `MAY_READ` before read operations
- `MAY_WRITE` before write operations
- `MAY_EXEC` for directory traversal
- `MAY_SATTR` for setattr

Common bug: using `MAY_READ` before `vfs_write()` (wrong flag for operation).

**File type enforcement:** Pass `S_IFREG`/`S_IFDIR` to `fh_verify()` when the
caller assumes a specific file type. Using `0` skips the check.

Request-scoped handles in args structs are released by the framework. The
COMPOUND framework manages cstate handle lifecycle.

## NFSv4 Stateid Lifecycle

Stateids track open files, locks, delegations, and layouts. Each type has
specific locking requirements.

**Stateid types and their locks (for `sc_status`):**
- Open/lock stateids: `cl_lock` (also `st_mutex` for open stateids)
- Delegations: `deleg_lock`
- Layouts: `ls_lock`

**SC_STATUS_CLOSED:** State-modifying operations must check `sc_status` under
the appropriate lock before proceeding. Check-and-modify must be atomic.

**Generation numbers:** Use `nfs4_inc_and_copy_stateid()` instead of manual
`si_generation` increment. State-modifying operations must bump generation.

**Delegation callbacks:** Delegation callbacks require
`refcount_inc(&dp->dl_stid.sc_count)` before `nfsd4_run_cb()` to keep the
delegation alive. Client lifetime during callbacks is protected by
`cl_cb_inflight` (incremented inside `nfsd4_run_cb()` itself), NOT by
`cl_rpc_users`. `cl_rpc_users` tracks active incoming NFS compound operations
and is unrelated to outgoing callback dispatch. `destroy_client()` calls
`nfsd4_shutdown_callback()` which waits on `cl_cb_inflight` to drain.

**Stateid file verification:** Stateids must be validated against file handles:
- CLAIM_DELEG_CUR must verify filehandle via `fh_match()` against `sc_file->fi_fhandle`
- Lock stateids must be verified against their open stateid's file
- Stateid-only lookup without file verification allows access to wrong files

**Third-party leases:** Before casting `fl_owner` to `nfs4_delegation`, check
`fl->fl_lmops == &nfsd_lease_mng_ops`. Non-NFSD leases have different
`fl_owner` types.

**Write-attrs delegations:** `FMODE_NOCMTIME` is only valid for
`OPEN_DELEGATE_WRITE_ATTRS_DELEG`. `dl_atime`/`dl_mtime` access on other
delegation types is invalid.

**Multi-client delegation conflicts:** Same-client short-circuit must still
break other clients' delegations. `if (same_client) return` without breaking
other-client delegations is a bug.

**VFS during delegation release:** `notify_change()` during
`nfs4_unlock_deleg_lease()` needs `ATTR_DELEG` in `ia.ia_valid` to prevent
re-breaking the delegation being released.

**Layout stateids:** `ls_lock` is a spinlock. Sleeping operations (segment
manipulation, allocation) require `ls_mutex` instead.

Destructor field access during final put is safe (refcount guarantees exclusion).

## Error Code Mapping

NFS error codes must be properly mapped between internal errno values and
wire protocol values.

**NFSv3 status:** Procedures must return `nfsd3_map_status(resp->status)`,
not bare `rpc_success`. NFSv2 needs `nfserrno()` conversion.

**Internal errors:** `PTR_ERR()` values must be converted via `nfserrno()`
before reaching the RPC layer. Negative errno values must not reach the wire.

**Version-specific errors:** NFSv4-only errors (e.g., `nfserr_delay`) in
shared code (vfs.c, nfsfh.c) cause problems when reached from v2/v3 paths.
Session errors must not be returned from pre-v4.1 paths. Note: NFSERR_INVAL
is not defined in NFSv2 (RFC 1094); `nfserr_file_open` is invalid for
non-regular files.

**Double mapping:** `nfserrno()` converts negative errno to NFS status.
Applying it to an already-converted `__be32` corrupts the value.

**EOPENSTALE:** Do not convert directly to `nfserr_stale`. `EOPENSTALE`
signals that retry is needed at a higher level.

## Locking

**Lock hierarchy (outer to inner):**
```
nn->client_lock → nn->deleg_lock → nn->s2s_cp_lock → fp->fi_lock → clp->cl_lock → stp->st_mutex
```

All except `st_mutex` are spinlocks. `nn->nfsd_ssc_lock` is outside the main
hierarchy but mutually exclusive with `s2s_cp_lock` - never hold both.

**Lock scope:**
- Per-namespace (`nn->client_lock`): `cl_time`, `cl_lru`, `cl_idhash`, `grace_ended`
- Per-client (`clp->cl_lock`): `cl_openowners`, `cl_sessions`, `cl_revoked`, `cl_flags` (including `NFSD4_CLIENT_RECLAIM_COMPLETE`)

**TOCTOU in stateid lookup:** Gap between `find_stateid_locked()` under
`cl_lock` and `mutex_lock(&stp->st_mutex)` requires subsequent
`nfsd4_verify_open_stid()` check to detect concurrent unhash.

**VFS callback paths:** `nfs4_put_stid()` under VFS break callback (holding
`flc_lock`) may acquire `cl_lock` via `refcount_dec_and_lock()`, causing
deadlock. Use `refcount_dec()` when refcount cannot reach zero.

## Client State Machine

Client states (`cl_state` enum): NFSD4_ACTIVE → NFSD4_COURTESY → NFSD4_EXPIRABLE.
Client confirmation is tracked separately via `NFSD4_CLIENT_CONFIRMED` bit in `cl_flags`.

**Valid transitions:**
- NFSD4_ACTIVE → NFSD4_COURTESY (lease expired, no state conflicts; `nfs4_get_client_reaplist`)
- NFSD4_COURTESY → NFSD4_ACTIVE (client reconnects)
- NFSD4_COURTESY → NFSD4_EXPIRABLE (conflict; `try_to_expire_client` via `cmpxchg`)
- NFSD4_EXPIRABLE → destroyed (laundromat reaps)

NFSD4_EXPIRABLE cannot return to NFSD4_ACTIVE. Only NFSD4_COURTESY may return to
NFSD4_ACTIVE on reconnect.

**COURTESY clients:** Transition to COURTESY requires laundromat integration.
`cl_time` must be set and laundromat must check and expire after timeout.

**Admin interfaces:** sysfs/procfs writes must hold `nfsd_mutex` or check
`nn->nfsd_serv` to avoid use-after-free races.

**Child stateids:** Parent destruction must free child stateids (copynotify).
`release_openowner()` must call `nfs4_free_cpntf_statelist()`.

**Double initialization:** Init functions reachable from multiple call paths
can trigger BUG_ON on second invocation.

## Grace Period and Lease Management

Grace period allows clients to reclaim state after server restart.

**During grace:**
- Non-reclaim operations (OPEN, LOCK, size-changing SETATTR) must return
  `nfserr_grace` if they would create new state
- Reclaim uses CLAIM_PREVIOUS, CLAIM_DELEGATE_PREV for opens, `lk_reclaim=true`
  for locks
- Reference: RFC 8881 section 8.4

**Lease timing:**
- Use `nn->nfsd4_lease` and `nn->nfsd4_grace` for durations, not hardcoded values
- Use `ktime_get_boottime_seconds()` for `cl_time`, not `ktime_get()` - lease
  times must survive suspend/resume

**Grace end:**
- Must account for clients with reclaims still in progress
- Clients that never issued RECLAIM_COMPLETE must be destroyed after grace ends
- Client records require `nfsd4_client_record_create()` for persistence across crashes

Reclaim operations (CLAIM_PREVIOUS, lk_reclaim) during grace are expected.

## User Namespace ID Conversion

NFSD kthreads have `init_user_ns` as `current_user_ns()`, which is wrong for
containerized clients.

**Correct namespace source:** Use `nfsd_user_namespace(rqstp)` for
`from_kuid`/`from_kgid` in request paths, not `init_user_ns` or
`current_user_ns()`. For inter-server socket creation, use `nn->net` instead
of `current->nsproxy->net_ns`.

**ID validation:** `make_kuid()`/`make_kgid()` results require
`uid_valid()`/`gid_valid()` validation. Mapping invalid IDs to
`GLOBAL_ROOT_UID` is privilege escalation.

**ACL encoding:** Each ACL entry needs `from_kuid_munged(ns, ...)` conversion.
Raw `kuid_t` values without conversion cause cross-namespace permission issues.

**Consistency:** Don't mix `init_user_ns` and `nfsd_user_namespace()` within
the same operation.

Host-only internal paths (module init, procfs) may use `init_user_ns`. The
idmap path handles namespace conversion internally.

## Callbacks

Callbacks are asynchronous RPCs from server to client.

**Reference requirements:** Client lifetime during callbacks is managed by
`cl_cb_inflight`, which `nfsd4_run_cb()` increments internally — callers do
NOT need to increment `cl_rpc_users` for callback dispatch. Delegation
callbacks need `sc_count` increment before `nfsd4_run_cb()` to keep the
delegation stid alive (see NFSv4 Stateid Lifecycle). Release handlers must
drop all acquired references.

**Connection state:** Check `cl_cb_state == NFSD4_CB_UP` under `cl_lock`
before callback dispatch. Access `cl_cb_client` only under the same lock hold.

**Sequence numbers:** `se_cb_seq_nr` (on `nfsd4_session`) modification requires
appropriate locking. Concurrent increment without locking causes duplicate
sequence numbers and BAD_SEQUENCE rejection.

**Client destruction:** `destroy_client()` must call `nfsd4_shutdown_callback()`
to drain in-flight callbacks before freeing.

**NFSv4.0 compatibility:** `cl_cb_session` is NULL for NFSv4.0 clients. Check
`cl_minorversion > 0` before `cl_cb_session` access.

Deferred release via RPC completion handler is acceptable.

## Session Slots

Sessions (NFSv4.1+) use slots for request ordering and replay detection.

**Slot index validation:** `xa_load(&session->se_slots, slotid)` access requires
prior check `slotid < se_fchannel.maxreqs`. Client-supplied slotid without
bounds check causes access to non-existent slots.

**Seqid validation order:** Compare request seqid with `sl_seqid` before
modification. Original value distinguishes: replay (match), new request (+1),
misordered (other).

**Replay security:** Replay path must check `same_creds()` before returning
cached reply. Without this, attackers can replay another client's response.

**Slot exclusivity:** Set `NFSD4_SLOT_INUSE` in `sl_flags` before compound
execution, clear after. All exit paths (error, deferral) must clear the flag.

**Session teardown:** Remove session from hash table and drain active compounds
before freeing slots.

**Cached reply lifetime:** Cached reply data lives in `sl_data[]` (flexible
array) with length `sl_datalen`. Ensure cached data is invalidated properly
on session teardown to prevent use-after-free on replay.

## Page Array Management

NFSD manages page arrays for read/write data transfer.

**Key pointers:**
- `rq_pages`: Base of page array
- `rq_next_page`: Next available slot
- `rq_page_end`: Sentinel (one past end)
- `rq_maxpages`: Array size

**Read procedures:** Save `resp->pages` from `rqstp->rq_next_page` BEFORE the
read call. Read operations advance `rq_next_page`, so referencing it afterward
uses the wrong pointer. (fix 7978e9bea278)

**Bounds checking:** Loops advancing `rq_next_page` need
`rq_next_page < rq_page_end` guard. `rq_bvec` indexing needs `rq_maxpages`
bound. (fixes e1b495d02c53, 3be7f32878e7)

**COMPOUND page sync:** Individual NFSv4 operations must not manually sync
`page_ptr` / `rq_next_page`. `nfsd4_encode_operation()` centralizes page_ptr
sync after each operation - this is the correct pattern. (fix ed4a567a179e)

**READDIR recycling:** After READDIR completion, set
`rqstp->rq_next_page = xdr.page_ptr + 1` to recycle unused pages.
Page count: `(count + PAGE_SIZE - 1) >> PAGE_SHIFT`
(fixes 3c86794ac0e6, 76ed0dd96eeb)

**Splice continuation:** In `nfsd_splice_actor()`, when
`page == *(rq_next_page - 1)` AND offset is not page-aligned, the same page
is being continued - don't add it again. Check `svc_rqst_replace_page()` return
value. (fixes 27c934dd8832, 91e23b1c3982)

## Copy Offload

NFSv4.2 COPY operation supports async and server-to-server copies.

**Async copy completion:** IDR removal under `s2s_cp_lock` must precede the
final put. Stale IDR entries allow OFFLOAD_STATUS queries on freed state.

**Cancellation:** OFFLOAD_CANCEL needs atomic state transition under lock.
Window between check and cancel allows race with completion, causing
use-after-free or double-free.

**CB_OFFLOAD ordering:** Set result fields (`wr_bytes_written`, `wr_stable_how`)
before calling `nfsd4_run_cb()`. The callback may read results immediately.

**S2S credentials:** Inter-server copy caching RPC credentials must verify
validity before each chunk. GSS credentials may expire during long copies.

**COPY_NOTIFY validation:** `nfsd4_setup_inter_ssc()` must verify `cnr_stateid`
exists, belongs to the requesting client, and has not expired.

**Resource limits:** Async copy submission needs per-client or global limits
to prevent memory exhaustion from unbounded concurrent COPY operations.

Synchronous copy uses compound-scoped references directly (no extra ref needed).

## Security Validation

**Validation bypass:** New branches or early returns before `fh_verify()` skip
validation. New helper functions accessing `fh_dentry` must either call
`fh_verify()` directly or document a caller contract requiring prior verification.

**Stateid validation:** NFSv4 stateful operations (read, write, lock, setattr
with size) require `nfs4_preprocess_stateid_op()` before file access.

**Cross-export operations:** RENAME and LINK must validate both source and
target file handles via `fh_verify()`. Removing validation from either allows
unvalidated access.

**Pseudo-filesystem exposure:** NFSv4 pseudo-filesystem must not be accessible
from v2/v3 procedures. `fh_verify()` enforces this version gate.

Functions where caller already verified fh (documented contract) or where
COMPOUND framework pre-validates cstate handles don't need re-verification.

## Netlink Interface

NFSD uses genetlink for configuration (include/uapi/linux/nfsd_netlink.h,
fs/nfsd/netlink.h).

**Policy requirements:**
- Every `NFSD_A_*` enum needs a corresponding `nla_policy` entry
- String attributes need `NLA_NUL_STRING` with explicit `.len` bound
- Don't use `nla_data()` on strings without policy-guaranteed null termination
- Nested attributes need their own policy array for `nla_parse_nested()`

Attributes enforced as required by genetlink policy validation are safe.

**Privilege checks:** Handlers modifying NFSD state need `capable(CAP_NET_ADMIN)`
or `ns_capable()` before any side effects.

**Namespace isolation:** Use `genl_info_net(info)` for network namespace, not
`&init_net` or global pointers. Use `ns_capable()` for namespace-relative
privilege checks.

**State synchronization:** Checks on `nn->nfsd_serv` or NFSD running state must be
protected by `nfsd_mutex` through the subsequent modification to prevent races.

## NFS Re-export

When NFSD exports an NFS-mounted filesystem, special handling is required.

**File handles:** On NFS superblocks (`s_magic == NFS_SUPER_MAGIC`), `i_ino`
is unstable across upstream reconnects. `fh_compose()` or file handle encoding
must embed the upstream file handle (`NFS_FH()`) instead of using `i_ino`.

**ESTALE handling:** Don't retry on `-ESTALE` from NFS-backed filesystems.
ESTALE indicates permanent handle invalidity; retrying cannot resolve it.

**Lock ordering:** Acquire upstream VFS lock (`vfs_lock_file()`) before
committing local NFSD state. Failed upstream lock must not leave stale
local state.

**Mount crossing:** `nfsd_cross_mnt()` or `follow_down()` must check if target
superblock is NFS before crossing mounts. Re-export requires different handle
encoding and credential handling.

**Dual grace periods:** Upstream grace (`-EAGAIN` from NFS client) and local
NFSD grace period are independent. Handle both.

**Credential double-mapping:** Re-export applies squash/security transforms
twice. `no_root_squash` on re-export with `root_squash` upstream causes issues.

**Export fsid:** Set `exp->ex_fsid` or `exp->ex_uuid` explicitly. Values
derived from NFS mount device numbers change across remounts.

Local filesystem exports (`s_magic != NFS_SUPER_MAGIC`) don't need re-export handling.

## Resource Limits

**Per-client limits required for:**
- `nfs4_alloc_stid()`, `alloc_init_deleg()`
- `create_session()`
- Async copy queue depth
- Work queue items (`cl_callback_wq` per-client, `laundry_wq`)

**Limit check ordering:** Verify limits before allocation, not after. Allocate
→ check → free under heavy load causes transient OOM.

**COMPOUND bounds:** Dispatch loops need upper bound on `args->opcnt`. Return
`nfserr_resource` when exceeded.

**Counter leak:** `atomic_inc()` on resource counters (`num_delegations`,
etc.) before allocation needs matching `atomic_dec()` on all
error paths.

**Expensive operations:** Cap READDIR/GETATTR `maxcount`. Limit per-entry
cost for ACLs, security labels, owner name idmap lookups.

Server-generated limits (e.g., session slot count after CREATE_SESSION) and
allocations bounded by fixed protocol maximums don't need additional checks.

## Code Style

- Reverse-christmas tree variable ordering
- `nfs_ok`/`nfserr_*` error convention
- `cpu_to_be32`/`be32_to_cpu` for byte order
- New NFSv4 XDR code should use nfs4xdr_gen.c (xdrgen)

## Expert Review Triggers

Flag for expert review when changes touch: XDR primitives or infrastructure;
refcount primitives; `fh_verify()` semantics; stateid lifecycle; lock ordering;
client state machine or grace period logic; callback dispatch/completion/retry;
session slot or SEQUENCE processing; new RPC procedure or NFSv4 op; namespace
conversion paths; page array or splice infrastructure; copy offload lifecycle
or S2S authentication; genetlink family or policy definitions; resource limits
or allocation patterns; re-export or cross-mount handling; changes exceeding
100 lines touching multiple core files.

## Quick Checks

- `fh_dentry` access without prior `fh_verify()` → NULL deref
- `nfsd4_run_cb()` without required references → use-after-free
- `xdr_reserve_space()` return unchecked → NULL deref
- Stateid lookup without `fh_match()` against filehandle → wrong file access
- `from_kuid()` with `init_user_ns` in request path → container escape
- Lock acquisition violating hierarchy → deadlock
- `sc_status` check/modify not atomic under lock → race condition
- State change before encode success confirmed → corruption on retry
- Session slot index unchecked against `maxreqs` → invalid slot access
- Async copy IDR removal after final put → stale state queries
