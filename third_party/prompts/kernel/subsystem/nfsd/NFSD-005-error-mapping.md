# NFSD-005: NFS error code mapping

**Risk**: Protocol violation, client confusion

## Check

- All NFS procedures call version-specific status mapping before
  returning
- Errors from VFS/internal APIs properly mapped to nfserr_* codes
- Don't leak protocol-specific error codes to wrong NFS versions

## Required patterns

```c
// NFSv2 (nfsproc.c):
status = nfsd_operation(...);
return nfserrno(status);

// NFSv3 (nfs3proc.c):
status = nfsd_operation(...);
return nfsd3_map_status(status);  // NOT optional!

// NFSv4 (nfs4proc.c):
// Use nfserr_* codes directly, but ensure NFSv2/v3-specific
// codes are not returned
```
