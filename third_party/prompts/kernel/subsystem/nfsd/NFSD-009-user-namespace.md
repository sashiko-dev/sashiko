# NFSD-009: User namespace conversion

**Risk**: Incorrect UID/GID, permission bypass in containers

## Check

- from_kuid_munged() / from_kgid_munged() for all ID conversions
- Consistent namespace used throughout operation
- Init namespace not assumed for all operations
- ACL handling considers namespace mapping
