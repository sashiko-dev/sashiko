# NFSD-001: XDR input trust boundaries

**Risk**: Buffer overflow from untrusted network input

## NFSD-specific untrusted data sources

- All data decoded from XDR streams (xdr_stream_decode_*)
- String lengths in NFSv3/v4 compound arguments
- Array/vector counts from READ/WRITE operations
- Offsets and lengths in I/O operations
- Client-provided file handles (before fh_verify)
- Callback arguments from client (nfs4callback.c)
- Administrative API inputs (nfsctl.c, netlink.c, /proc interfaces)

## Trusted data sources (no defensive checks needed)

- Data from fh_dentry/fh_export (after fh_verify)
- Auto-generated xdrgen output (nfs3xdr_gen.c)
- Internal state machine values
- Kernel VFS layer returns

## Example vulnerability

nfsd_iter_read() missing max_read check (7ac3be9e56d8)
