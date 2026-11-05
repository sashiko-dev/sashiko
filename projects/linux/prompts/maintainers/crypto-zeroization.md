+++
title = "Crypto Zeroization"
paths = ["lib/crypto/", "crypto/", "include/crypto/", "arch/*/crypto/"]
symbols = ["memzero_explicit", "kfree_sensitive"]
+++
# Crypto Zeroization: maintainer notes

How the crypto library maintainers rate missing zeroization of key material.

- Zeroizing stack or scratch copies of key material (keys, key schedules, IVs, intermediate cipher state) is best-effort hardening against data remanence: registers and their spilled copies are never cleared anyway. Report a copy that goes out of scope without memzero_explicit(), or a memzero_explicit() replaced by memset(), as Low at most. Do not report it when the code before the patch already left the same data uncleared (check with git_show on the parent).
- The Low cap does not apply when the data actually reaches another context: copied to userspace or the network, returned to the caller, or freed without clearing into memory another user can read. Rate those as information leaks under severity.md.
- Clearing an output buffer after an authentication failure (for example the memset() of dst in an AEAD decrypt error path) keeps callers from using inauthentic data; it is a functional requirement, not zeroization, and the Low cap does not cover it. Do not ask for it to become memzero_explicit().
