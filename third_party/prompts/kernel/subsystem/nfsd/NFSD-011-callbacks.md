# NFSD-011: NFSv4 callback client operations

**Risk**: Callback failure, client unresponsiveness, resource exhaustion

**Applies to**: fs/nfsd/nfs4callback.c and related callback infrastructure

The NFSv4 protocol requires the server to initiate RPCs to clients for:
- Delegation recalls (CB_RECALL)
- Layout recalls (CB_LAYOUTRECALL)
- Client notification (CB_NOTIFY, CB_NOTIFY_DEVICEID)
- Session management (CB_SEQUENCE)
- State reclaim coordination (CB_RECALL_ANY, CB_RECALL_SLOT)

## Phase 1: Track callback lifecycle

For each callback operation in the changed code, document:
- Callback type (CB_RECALL, CB_LAYOUTRECALL, etc.)
- Triggering condition (what causes this callback to be sent)
- Client connection setup (nfsd4_create_callback_queue, set_backchannel_*)
- Reference counting on client/session during callback
- Callback completion handling (nfsd4_cb_done, nfsd4_cb_release)
- Timeout and retry behavior
- Resource cleanup on failure

Create a tracking table:
```
Callback | Trigger | Connection | Refs Held | Completion | Timeout | Cleanup
---------|---------|------------|-----------|------------|---------|--------
CB_RECALL| L200    | backchan   | clp,dp    | L250       | 60s     | L260
...
```

## Phase 2: Verify callback correctness

For each tracked callback, prove:
1. Client connection validated before sending (cb_conn state check)
2. Necessary references held during callback lifetime
3. Callback serialization uses appropriate cb_seq or session sequence
4. Timeout handling doesn't leak resources
5. Callback failure doesn't corrupt server state
6. Retries follow backoff policy and eventually give up
7. Client locks (cl_lock) held when accessing cb_conn or callback state

## Phase 3: Verify error handling

Answer these questions about callback failures:
- What happens if the client is unreachable?
- How many retries occur before giving up?
- Are delegations/layouts properly revoked on timeout?
- Does callback failure affect other clients?
- Are resources (RPC clients, connections) properly cleaned up?
- Is the client state properly updated to reflect callback failure?

## Critical patterns for callback operations

```c
// CORRECT - hold reference during callback
refcount_inc(&clp->cl_rpc_users);     // Prevent client destruction
callback_op = prepare_callback(...);
nfsd4_run_cb(callback_op);            // Async operation
// Release in callback completion handler

// WRONG - no reference held
callback_op = prepare_callback(...);
nfsd4_run_cb(callback_op);            // Client might be freed before callback completes
```

## Callback-specific locking rules

- `clp->cl_lock` protects:
  - Callback connection state (cl_cb_conn)
  - Callback sequence numbers (cl_cb_seq_nr)
  - Callback state flags (NFSD4_CLIENT_CB_UPDATE, etc.)

- Callback work must increment `cl_rpc_users` before queuing
- RPC client (`clp->cl_cb_conn.cb_client`) protected by cl_lock
- Session backchannel (`ses->se_cb_conn`) protected by se_lock

## Common callback vulnerabilities

### 1. Reference counting errors

- Callback queued without incrementing cl_rpc_users
- Client freed while callback in flight
- Delegation/layout freed before callback completes

### 2. Connection state races

- Callback sent to stale connection after client reconnects
- Multiple threads updating cb_conn simultaneously
- Backchannel state not synchronized with client state

### 3. Timeout handling

- Resources not cleaned up after callback timeout
- Retry logic causes unbounded resource consumption
- Client not marked as unresponsive after repeated failures

### 4. State synchronization

- Delegation/layout state inconsistent with callback result
- Client state transitions not reflected in callback behavior
- Session state not updated after CB_SEQUENCE

## Verification checklist

- [ ] Every callback holds cl_rpc_users reference during execution
- [ ] Callback connection state validated before sending
- [ ] Timeout/failure paths clean up all resources
- [ ] Client state properly updated based on callback result
- [ ] Locks held when accessing client callback fields (cl_cb_conn, etc.)
- [ ] Callback serialization (cb_seq) prevents out-of-order operations
- [ ] Session backchannel state synchronized with session lifecycle
- [ ] RPC client properly initialized and cleaned up
- [ ] Callback work queue (callback_wq) properly managed

## Integration with delegation/layout management

- CB_RECALL triggered by: delegation break, server shutdown, client eviction
- Must hold delegation reference during recall operation
- Delegation state updated atomically with callback completion
- Layout recalls follow similar pattern for pNFS operations

## Recent callback-related bug patterns

Check for these common issues:
- Callback sent after client already destroyed
- Connection setup race with client disconnect
- Memory leak in callback work structure allocation
- Incorrect sequence number tracking across reconnects
- Failure to update cl_cb_state after callback timeout
