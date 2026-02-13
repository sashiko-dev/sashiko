# NFSD-010: Security-critical input validation

**Risk**: Authentication bypass, unauthorized access

## Check

- File handle validity checked (fh_verify)
- Export table properly consulted
- Permission checks use MAY_READ/MAY_WRITE/MAY_EXEC flags
- Pseudo-filesystem not accessible to NFSv2/v3
- Client credentials properly validated
- Stateid validity verified for stateful operations
