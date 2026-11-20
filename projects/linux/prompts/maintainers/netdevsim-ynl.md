+++
title = "Netdevsim and YNL Tooling"
paths = ["drivers/net/netdevsim/", "tools/net/ynl/"]
symbols = []
+++
# Netdevsim and YNL Tooling: maintainer notes

Netdev maintainer policy for netdevsim (a test-only simulated driver) and the YNL user-space tooling and code generator: missing coverage is not a defect.

- netdevsim is a test driver for upstream selftests, not a user or a uAPI (Documentation/process/maintainer-netdev.rst, "netdevsim" section). Do not report that a simulated feature is missing, partial or less faithful than real hardware unless the patch claims to add that feature or a selftest in the same series depends on it. Bugs in code netdevsim does have (memory safety, locking, crashes, regressions of behaviour that already worked) are reported as usual.
- Netdev applies fixes that touch only drivers/net/netdevsim/ to net-next with the Fixes: tag removed. Do not flag such a fix for targeting net-next or for lacking a Fixes: tag; this overrides the general.md Fixes: suggestion for netdevsim-only fixes.
- In tools/net/ynl/, an operation, notification or attribute that YNL deliberately does not generate or support is not a bug. The tree shows the gap is deliberate when tools/net/ynl/generated/Makefile excludes it (an --exclude-op or GENS_UNSUP entry) or pyynl raises an explicit "not supported" error for it. Do not report such a gap, and do not suggest sending support for it to net as a fix. Bugs in code YNL does generate or parse are reported as usual.
