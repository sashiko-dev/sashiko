# NFSD-007: XDR encode/decode failure handling

**Risk**: State corruption, incomplete responses

## Phase 1: Track all XDR operations

For each XDR encode/decode operation in the changed code, document:
- Function name and line number
- Type of operation (decode input, encode output, reserve buffer space)
- Return value variable (if checked)
- State changes that depend on this XDR operation
- Error handling path for XDR failure

Create a tracking table:
```
Operation | Line | Type | Return Check | State Changes | Error Path
----------|------|------|--------------|---------------|------------
decode_fh | L42  | decode | yes | none | L45
encode_op | L120 | encode | yes | close_file | L125
...
```

## Phase 2: Verify XDR operation correctness

For each tracked XDR operation, prove:
1. Decode operations checked before accessing decoded data
2. Encode operations checked before committing state changes
3. Buffer space reserved (xdr_reserve_space*) before encoding
4. XDR failures cause early return without state modification
5. Return values from xdr_stream_* functions are checked

## Phase 3: Verify state change ordering

Answer these questions about state modifications:
- How many state changes occur in the procedure?
- Which state changes happen before XDR encode?
- Which XDR encode operations must succeed for correctness?
- Are all state changes conditional on encode success?
- Do any state changes commit before checking encode status?

## Critical rule

State changes must not commit if subsequent XDR encode fails

## Required pattern for operations with state changes

```c
// Step 1: Decode and validate inputs
status = xdr_stream_decode_*(...);
if (status < 0)
    return nfserr_bad_xdr;

// Step 2: Perform operation, track state changes in local variables
tmp_state = perform_operation();

// Step 3: Encode response BEFORE committing state
status = xdr_stream_encode_*(...);
if (status < 0) {
    undo_operation(tmp_state);  // Rollback
    return nfserr_resource;      // XDR encode failed
}

// Step 4: Commit state changes only after encode succeeds
commit_state(tmp_state);
return nfs_ok;
```

## Example vulnerability

CLOSE operation after encode failure (27f9ab3c7674)

## Verification checklist

- [ ] Every xdr_stream_decode_* return value checked
- [ ] Every xdr_stream_encode_* return value checked
- [ ] xdr_reserve_space* called before encoding variable-length data
- [ ] State changes deferred until after encode succeeds
- [ ] XDR failures return appropriate error without state modification
- [ ] Buffer overflow prevented by proper space reservation
