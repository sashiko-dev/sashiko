+++
title = "Networking, BPF, and RDMA Fixes Policy"
paths = ["net/", "drivers/net/", "include/net/", "drivers/infiniband/", "include/rdma/", "include/uapi/rdma/", "kernel/bpf/", "tools/lib/bpf/", "include/linux/bpf*.h"]
symbols = []
+++
# Networking, BPF, and RDMA Fixes Policy: maintainer notes

Policy for suggesting missing `Fixes:` trailers in networking, BPF, and RDMA subsystems (`Documentation/process/maintainer-netdev.rst`), where adding `Fixes:` queues a commit for automated `-stable` backporting unless maintainers intentionally omit it for non-backportable changes.

- In networking, BPF, and RDMA subsystems (`Documentation/process/maintainer-netdev.rst`), adding a `Fixes:` trailer queues a commit for automated `-stable` backporting unless `Cc: stable` is intentionally omitted for non-backportable changes.
- When `Cc: stable@vger.kernel.org` is not present in the commit message, do not suggest adding a missing `Fixes:` trailer on defensive or adversarial-input hardening (such as validating untrusted firmware or hardware descriptors where existing code trusted the device), non-regression behavior adjustments, long-standing non-regression quirks that do not cause crashes, memory corruption, leaks, or data loss, or fixes that depend on new helpers, fields, or bindings introduced by earlier commits in `Baseline SHA..Target Commit SHA` and therefore cannot be backported standalone; this overrides the `general.md` missing-`Fixes:` suggestion for these subsystems.
- Still verify existing `Fixes:` trailers and suggest missing `Fixes:` on standalone bug fixes for regressions or crashes, memory corruption, leaks, or data loss as usual.
