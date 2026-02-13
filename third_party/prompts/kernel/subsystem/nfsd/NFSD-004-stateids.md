# NFSD-004: NFSv4 stateid lifecycle and state management

**Risk**: Use-after-free, stateid reuse, unauthorized state access,
delegation race, resource leak

**When to check**: Any change involving nfs4_stid, nfs4_ol_stateid,
nfs4_delegation, nfs4_layout_stateid, or stateid lookup/validation

## Phase 1: Track all stateid operations

For each stateid variable in the changed code, document:
- Variable name and type (nfs4_stid, nfs4_ol_stateid,
  nfs4_delegation, nfs4_layout_stateid)
- Allocation point (nfs4_alloc_stid or subtype allocator)
- Initialization (refcount_set, stateid generation, sc_type)
- Lookup/validation (nfsd4_lookup_stateid, find_stateid_*)
- Reference acquisition (refcount_inc on sc_count)
- Usage points (access to sc_file, sc_client, type-specific fields)
- Release points (nfs4_put_stid calls in each exit path)
- Deallocation (final refcount_dec_and_test triggers destructor)

Use TodoWrite to track each stateid through its lifecycle.

For each stateid variable, create a todo item with the following
checklist:
- [ ] Document type (nfs4_stid, nfs4_ol_stateid,
      nfs4_delegation, nfs4_layout_stateid)
- [ ] Identify allocation point (nfs4_alloc_stid or subtype
      allocator)
- [ ] Verify initialization (refcount_set, stateid generation,
      sc_type)
- [ ] Track lookup/validation (nfsd4_lookup_stateid,
      find_stateid_*)
- [ ] Count reference acquisitions (refcount_inc on sc_count)
- [ ] Identify all usage points (sc_file, sc_client,
      type-specific fields)
- [ ] Verify release points (nfs4_put_stid in each exit path)
- [ ] Confirm deallocation (final refcount_dec_and_test
      triggers destructor)

Example todo format:
```
TodoWrite: Track stateid 'stp' (nfs4_ol_stateid)
- Allocated: line 120 via nfs4_alloc_stid()
- Initialized: line 125 (refcount=1, sc_type=OPEN)
- Lookup: line 140 via nfsd4_lookup_stateid()
- Used: lines 150, 155 (sc_file access)
- Released: lines 160 (error path), 170 (success path)
- Status: All paths have matching put
```

## Phase 2: Verify lifecycle correctness

For each tracked stateid, prove:
1. Allocated with proper sc_type (OPEN, LOCK, DELEG, LAYOUT)
2. Refcount initialized (refcount_set(&stid->sc_count, 1))
3. Stateid generation incremented on state changes
4. Lookup uses appropriate lock (client_lock or state_lock)
5. Reference held during all accesses to stateid fields
6. nfs4_put_stid() called in ALL exit paths
7. Type-specific fields (st_stateowner, dl_stid) properly managed
8. No use after final put (triggers idr_remove and kfree_rcu)

## Phase 3: Validate reference counting

Answer these counting questions:
- How many stateid allocations occur?
- How many refcount_inc() calls per stateid?
- How many nfs4_put_stid() calls per stateid?
- Do reference increments match decrements?
- Are references held across lock release points?
- Does each lookup have a matching put?

**If you cannot answer these questions, stop and repeat the
previous phase.**

## Required sequence per stateid

1. Allocation: nfs4_alloc_stid() or type-specific allocator
2. Initialization: Set sc_type, sc_client, sc_file
3. Lookup (if existing): nfsd4_lookup_stateid() increments refcount
4. Validation: Check sc_status != SC_STATUS_CLOSED
5. Use: Access sc_file, sc_client, or type-specific fields
6. Release: nfs4_put_stid() in all exit paths
7. Destruction: Automatic when refcount reaches zero

## Stateid types and their reference patterns

### nfs4_ol_stateid (OPEN/LOCK state)

- Parent: nfs4_stid.sc_count via refcount_t
- Owners: st_stateowner reference (get/put via nfs4_get_stateowner)
- File: st_stid.sc_file reference (get/put via nfs4_file)
- Seqid: Must increment on OPEN_DOWNGRADE, LOCK, LOCKU per RFC 8881

### nfs4_delegation

- Parent: nfs4_stid.sc_count via refcount_t
- File: dl_stid.sc_file reference
- Recall list: dl_recalled list management
- Callbacks: Must hold reference during CB_RECALL

### nfs4_layout_stateid

- Parent: nfs4_stid.sc_count via refcount_t
- File: ls_stid.sc_file reference
- Layouts: nfsd4_layout_ops callbacks manage layout memory

## Stateid generation and seqid rules (RFC 8881 section 9.1.4)

Stateid generation must increment to prevent reuse attacks:

```c
// Generation increment required for:
// - OPEN_DOWNGRADE
// - LOCK/LOCKU operations
// - Any state-modifying operation

// CORRECT: Use nfs4_inc_and_copy_stateid (increments internally)
nfs4_inc_and_copy_stateid(&resp->stateid, &stp->st_stid);

// WRONG: Double increment
stp->st_stid.sc_stateid.si_generation++;
nfs4_inc_and_copy_stateid(&resp->stateid, &stp->st_stid);
// Bug: Generation incremented twice, violates protocol

// WRONG: No increment before copy
nfs4_copy_stateid(&resp->stateid, &stp->st_stid);
// Bug: Client may confuse old and new state
```

## Delegation state management

### Delegation recall sequencing

```c
// CORRECT: Hold reference during callback
refcount_inc(&dp->dl_stid.sc_count);
nfsd4_run_cb(&dp->dl_recall);
// Reference released in callback completion

// WRONG: No reference during async callback
nfsd4_run_cb(&dp->dl_recall);
// Bug: Delegation may be freed before callback completes
```

### Delegation revocation

```c
// Revocation must:
// 1. Set SC_STATUS_REVOKED under appropriate lock
// 2. Remove from client's delegation list
// 3. Clean up file references
// 4. Not leave stale state accessible

spin_lock(&clp->cl_lock);
dp->dl_stid.sc_status |= SC_STATUS_REVOKED;
list_del_init(&dp->dl_perclnt);
spin_unlock(&clp->cl_lock);
nfs4_put_stid(&dp->dl_stid);
```

## Lock state transitions

Lock state changes must be atomic where required:

```c
// CORRECT: Atomic state transition
mutex_lock(&stp->st_mutex);
if (stp->st_stid.sc_status & SC_STATUS_CLOSED) {
    mutex_unlock(&stp->st_mutex);
    return nfserr_bad_stateid;
}
// Perform state change
stp->st_stid.sc_stateid.si_generation++;
mutex_unlock(&stp->st_mutex);

// WRONG: Check and modify not atomic
if (!(stp->st_stid.sc_status & SC_STATUS_CLOSED)) {
    // Window for race here
    stp->st_stid.sc_stateid.si_generation++;
}
```

## Critical rules

- Never access stateid fields without holding a reference
- Increment refcount before releasing client_lock/state_lock
- Stateid generation must increment on state-modifying operations
- SC_STATUS_CLOSED prevents further operations
- SC_STATUS_REVOKED marks forcibly invalidated state
- idr_remove() only in final destructor, never in put path
- Delegation callbacks must hold delegation reference

## Example vulnerability pattern

```c
// WRONG - use after put
stp = nfsd4_lookup_stateid(...);  // refcount incremented
if (error_condition) {
    nfs4_put_stid(&stp->st_stid);  // refcount decremented, may free
    status = error;
}
// Bug: stp may be freed if error_condition was true and refcount hit 0
if (stp->st_stid.sc_type == NFS4_DELEG_STID)  // Use-after-free!
    handle_delegation(stp);

// CORRECT - put only after last use
stp = nfsd4_lookup_stateid(...);  // refcount incremented
if (error_condition) {
    status = error;
    goto out_put_stid;
}
// Use while reference held - safe
if (stp->st_stid.sc_type == NFS4_DELEG_STID)
    handle_delegation(stp);
status = nfs_ok;

out_put_stid:
    nfs4_put_stid(&stp->st_stid);  // Release after all uses
    return status;
```

## Verification checklist

- [ ] Every stateid allocation has matching cleanup path
- [ ] Every lookup/refcount_inc has matching nfs4_put_stid
- [ ] No stateid field access after final put
- [ ] Stateid generation incremented when required by RFC 8881
- [ ] SC_STATUS_CLOSED checked before operations
- [ ] SC_STATUS_REVOKED set atomically during revocation
- [ ] References held across lock release boundaries
- [ ] Type-specific cleanup (owners, files) properly ordered
- [ ] Delegation callbacks hold reference during execution
- [ ] Lock state transitions are atomic where required
- [ ] Revocation cleans up all associated state

**If any checklist items cannot be verified, repeat from Phase 1.**
