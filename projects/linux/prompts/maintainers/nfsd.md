+++
title = "NFSD"
paths = ["fs/nfsd/", "fs/lockd/"]
symbols = []
+++
# NFSD: maintainer notes

These notes cover coding conventions and review expectations for the Linux NFS server (NFSD) and lockd.

- Order local variable declarations in reverse Christmas tree order.
- Follow the nfs_ok / nfserr_* convention for NFS status return values.
- New NFSv4 XDR code should use the xdrgen-generated codec (nfs4xdr_gen.c) rather than hand-written encoders and decoders.
- A function that takes over ("steals") a caller's reference is acceptable as long as that transfer is documented.
- Escalate to expert maintainer review any change that touches core infrastructure: XDR primitives, refcounting, fh_verify() semantics, stateid lifecycle, lock ordering, client state machine or grace period, callbacks, session slots/SEQUENCE, new RPC procedures or NFSv4 operations, user-namespace conversion, page array/splice, copy offload or server-to-server authentication, genetlink family/policy, resource limits, re-export or cross-mount handling, and any change over 100 lines that touches several core files.
