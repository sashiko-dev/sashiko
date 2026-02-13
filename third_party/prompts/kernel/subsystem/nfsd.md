# NFS Server Subsystem Details

## Overview

NFSD (fs/nfsd/) implements the Linux NFS server with support for NFSv2, NFSv3,
and NFSv4.x protocols. The subsystem is actively migrating NFSv3 XDR handling
to auto-generated code (xdrgen). Code is generally well-structured but complex
state management and reference counting require careful review.

## NFSD-Specific Patterns [NFSD]

Detailed review patterns are organized by topic. Load the relevant pattern
files based on what code areas are being modified:

**Input validation and protocol handling:**
- [NFSD-001: XDR input trust boundaries](nfsd/NFSD-001-xdr-input.md)
- [NFSD-005: NFS error code mapping](nfsd/NFSD-005-error-mapping.md)
- [NFSD-007: XDR encode/decode failure handling](nfsd/NFSD-007-xdr-encode-decode.md)

**Resource management:**
- [NFSD-002: Reference counting balance](nfsd/NFSD-002-refcounting.md)
- [NFSD-003: File handle lifecycle](nfsd/NFSD-003-filehandles.md)
- [NFSD-004: NFSv4 stateid lifecycle](nfsd/NFSD-004-stateids.md)

**Concurrency and state:**
- [NFSD-006: Locking correctness](nfsd/NFSD-006-locking.md)
- [NFSD-008: Client state transitions](nfsd/NFSD-008-client-state.md)
- [NFSD-013: Session slot state and SEQUENCE operations](nfsd/NFSD-013-session-slots.md)

**Security:**
- [NFSD-009: User namespace conversion](nfsd/NFSD-009-user-namespace.md)
- [NFSD-010: Security-critical input validation](nfsd/NFSD-010-security-validation.md)

**Advanced features:**
- [NFSD-011: NFSv4 callback client operations](nfsd/NFSD-011-callbacks.md)

## Quick Checks

**Code style (required for NFSD):**
- Automatic variables in reverse-christmas tree order
- Line length ≤ 68 characters in commit messages

**Common operations:**
- cpu_to_be32/be32_to_cpu for byte order conversions
- dget/dput pairs for dentry references
- Permission checks via fh_verify() before operations
- nfs_ok (0) returned on success
- nfserr_* constants returned on error

**XDR migration (ongoing):**
- NFSv3 procedures being migrated to use nfs3xdr_gen.c
- New code should use generated encode/decode functions
- Generated functions prefixed with nfs_svc_decode_* and nfs_svc_encode_*
- Check pc_decode/pc_encode in svc_procedure arrays

## Recent Bug Classes (Oct-Nov 2025)

These represent real vulnerabilities found recently - scrutinize similar patterns:

1. **Refcount leak from early assignment** (b3da9b141578)
   - Location: nfsd_set_fh_dentry()
   - Pattern: Assigning resource before error checks complete

2. **Missing bounds check in hot path** (7ac3be9e56d8)
   - Location: nfsd_iter_read()
   - Pattern: Loop count from network not validated

3. **XDR buffer space not reserved** (38dd5c11ff52)
   - Location: nfsd_splice_read()
   - Pattern: xdr_reserve_space_vec() called after encoding

4. **Encode failure causing state corruption** (27f9ab3c7674)
   - Location: nfsd4_close()
   - Pattern: State change committed despite encoding failure

5. **Protocol violation in caching** (48990a0923a7)
   - Location: nfsd4_sequence()
   - Pattern: Cached response when RFC requires fresh generation

## Security Focus Areas

**Highest priority for security review:**

1. **Untrusted input handling:**
   - All data from XDR decode is untrusted
   - String lengths, array counts, offsets must be validated
   - File handles must be verified before use

2. **Trust boundaries:**
   - Client callbacks (nfs4callback.c)
   - Userspace control interface (nfsctl.c)
   - Export table updates

3. **Permission enforcement:**
   - fh_verify() with appropriate MAY_* flags
   - Export access control (export.c)
   - File type restrictions (e.g., no symlink operations)

4. **State consistency:**
   - Stateid validation for NFSv4 operations
   - Proper locking around state changes
   - Reference counting prevents use-after-free

## Review Workflow Recommendations

1. **Identify changed files** - Check risk level above
2. **Understand change type:**
   - New feature? Full security review required
   - Bug fix? Verify fix is complete and doesn't introduce new bugs
   - Refactor? Check reference counting and locking unchanged
   - XDR migration? Verify generated functions used correctly
3. **Apply relevant patterns** - Load and apply patterns matching the change type
4. **Check error paths** - Most bugs are in error handling
5. **Verify cleanup** - All acquired resources freed?
6. **Consider performance** - Does change affect hot paths?
