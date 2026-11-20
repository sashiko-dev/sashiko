+++
title = "Networking Hardware Selftests"
paths = ["tools/testing/selftests/drivers/net/hw/"]
symbols = []
+++
# Networking Hardware Selftests: maintainer notes

Netdev policy for hardware tests under tools/testing/selftests/drivers/net/hw/, which run against real NICs in netdev CI.

- Netdev maintainers prefer letting a HW test case fail on a device that lacks an optional capability (a cipher or protocol variant, an offload mode, a feature flag, a queue or RSS limit) over adding per-device skip logic or driver skip lists: their CI decides which HW failures are regressions, and only regressions and crashes matter. For device capability gaps in this directory, this overrides the selftests.md rule that a missing hardware feature is a skip: do not report that such a case should SKIP instead of FAIL, and do not suggest a driver allowlist or denylist.
- Still report a case that can fail on a device that supports the capability and works correctly, a test that can crash, hang, or leave the device or the remote endpoint misconfigured, and missing prerequisites outside the device under test (kernel config, userspace tools, no remote endpoint configured), which follow selftests.md.
