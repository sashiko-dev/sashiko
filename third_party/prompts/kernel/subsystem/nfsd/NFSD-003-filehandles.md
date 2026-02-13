# NFSD-003: File handle lifecycle

**Risk**: Use of unvalidated file handle, permission bypass

## Phase 1: Track all file handle operations

For each file handle variable in the changed code, document:
- Variable name and scope (function parameter, local, struct field)
- Initialization point (SVC_FH3_INIT, fh_init, fh_copy, or none)
- Verification point (fh_verify call location and parameters)
- Usage points (any access to fh_dentry, fh_inode, fh_export)
- Cleanup points (fh_put calls in each exit path)

Create a tracking table:
```
Handle | Init | Verify | Uses | Cleanup Paths
-------|------|--------|------|---------------
fhp    | L42  | L50    | L55,L60 | L65,L72,L80
...
```

## Phase 2: Verify lifecycle correctness

For each tracked file handle, prove:
1. Initialization occurs before any use
2. fh_verify() called before accessing fh_dentry/fh_export/fh_inode
3. Verification includes appropriate access flags (MAY_READ/WRITE/EXEC)
4. Version restrictions enforced (NFSv3 can't access v4 pseudo-root)
5. fh_put() called in ALL exit paths (success and error)

## Phase 3: Quantify and validate

Answer these counting questions:
- How many file handles are initialized in the change?
- How many fh_verify() calls are there?
- How many distinct code paths exist (success + all error paths)?
- How many fh_put() calls are there per handle?
- Do cleanup counts match the number of exit paths?

## Required sequence per handle

1. SVC_FH3_INIT(fhp) or equivalent initialization
2. fh_verify(rqstp, fhp, S_IFDIR, NFSD_MAY_EXEC) for validation
3. Use fh_dentry, fh_inode, fh_export
4. fh_put(fhp) in all exit paths
