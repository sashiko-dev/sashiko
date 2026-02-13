# NFSD-006: Locking correctness

**Risk**: Deadlock, race condition, data corruption

## NFSD-specific lock ordering rules

The NFSD subsystem uses a hierarchical locking design with strict
ordering requirements. Locks are organized by scope:

## 1. Global locks (module-wide, defined in source files)

- `state_lock` (nfs4state.c:102) - Protects del_recall_lru, file
  hash table, and sc_status for delegation stateids
- `nfsd_devid_lock` (nfs4layouts.c:48) - Protects pNFS device ID
  mappings
- `nfsd_notifier_lock` (nfssvc.c:431) - Protects nfsd_serv during
  network events
- `nfsd_gc_lock` (filecache.c:561) - Disables shrinker during
  garbage collection
- `blocked_delegations_lock` (nfs4state.c:1072) - Protects
  blocked delegations bloom filter
- `nfsd_session_list_lock` (nfs4state.c:1945) - Protects global
  session list
- `nfsd_mutex` (nfssvc.c:71) - Protects nn->nfsd_serv pointer and
  svc_serv members (sv_temp_socks, sv_permsocks); also protects
  global variables during nfsd startup

## 2. Per-namespace locks (in struct nfsd_net, netns.h)

### Spinlocks

- `client_lock` (netns.h:109) - Protects client_lru, close_lru,
  del_recall_lru, and session hash table
- `blocked_locks_lock` (netns.h:112) - Protects blocked_locks_lru
- `s2s_cp_lock` (netns.h:148) - Protects server-to-server copy
  state (s2s_cp_stateids IDR)
- `nfsd_ssc_lock` (netns.h:194) - Protects server-to-server copy
  mounts
- `local_clients_lock` (netns.h:217) - Protects local_clients list
  (CONFIG_NFS_LOCALIO)

### Seqlocks

- `writeverf_lock` (netns.h:128) - Protects write verifier for
  NFSv3 COMMIT

## 3. Per-object locks (in data structure fields)

### Stateid structures

- `sc_lock` (state.h:145, in nfs4_stid) - Protects stateid fields
- `st_mutex` (state.h:718, in nfs4_ol_stateid) - Protects
  open/lock stateid and sc_status for open stateids
- `ls_lock` (state.h:730, in nfs4_layout_stateid) - Protects
  layout stateid fields
- `ls_mutex` (state.h:737, in nfs4_layout_stateid) - Protects
  layout operations

### Client structures

- `cl_lock` (state.h:500, in nfs4_client) - Protects all client
  info needed by callbacks; also protects sc_status for open and
  lock stateids
- `async_lock` (state.h:522, in nfs4_client) - Protects
  async_copies list

### Session/File structures

- `se_lock` (state.h:369, in nfsd4_session) - Protects session
  fields
- `fi_lock` (state.h:660, in nfs4_file) - Protects file state
  including delegations, stateids, access counts

### Other

- `cn_lock` (nfs4recover.c:646, in cld_net) - Protects client
  tracking upcall info
- `cache_lock` (nfscache.c:35, in nfsd_drc_bucket) - Protects DRC
  bucket
- `lock` (filecache.c:66, in nfsd_fcache_disposal) - Protects
  freeme list

## Critical lock ordering hierarchy

NFSD's primary lock ordering rule:
```
nn->client_lock (outer) → state_lock (inner)
```

**NEVER acquire client_lock while holding state_lock** - this
creates ABBA deadlock potential.

Typical nesting patterns (acquire in this order):
1. `nn->client_lock` (outermost for client operations)
2. `state_lock` (for delegation and file state)
3. `fp->fi_lock` (for file-level operations)
4. `clp->cl_lock` (for client-level state)

## Lock assertions - verify with lockdep_assert_held()

When accessing delegation/file state, verify:
- `lockdep_assert_held(&state_lock)` for:
  - File delegation state (`nfs4_file`, `nfs4_delegation`)
  - Layout state (`nfs4_layout_stateid`)
  - sc_status on delegation stateids

When accessing client/session state, verify:
- `lockdep_assert_held(&nn->client_lock)` for:
  - Client/session structures (`nfs4_client`, `nfsd4_session`)
  - State owner structures (`nfs4_stateowner`)

When accessing open/lock stateid state, verify:
- `lockdep_assert_held(&clp->cl_lock)` for sc_status on open and
  lock stateids
- st_mutex also protects sc_status for open stateids (can use
  either)

## Critical section constraints

- Both `state_lock` and `client_lock` are spinlocks - no sleeping
  operations permitted
- Keep critical sections minimal (hot paths in nfs4state.c)
- RCU read-side critical sections properly bounded
- st_mutex and ls_mutex allow sleeping (but verify no spinlocks
  held)

## Common NFSD locking patterns

- File handle operations: No subsystem locks required (use VFS
  locks)
- Stateid lookup: Requires client_lock or state_lock depending on
  operation
- Delegation management: Always requires state_lock
- Client/session management: Always requires client_lock

## Lockdep subclasses (nfs4state.c:104-107)

NFSD uses lockdep subclassing to avoid false positives when
acquiring st_mutex on both open and lock stateids:
- `OPEN_STATEID_MUTEX = 0`
- `LOCK_STATEID_MUTEX = 1`

## Verification specific to NFSD

When reviewing code with multiple lock chains, verify:
- They follow `client_lock` → `state_lock` ordering
- lockdep_assert_held() matches actual lock protection needs
- No lock held when calling functions that may sleep (fh_verify,
  VFS operations)
- Per-object locks (cl_lock, fi_lock, se_lock) not held across
  global lock acquisitions
