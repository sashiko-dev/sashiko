+++
title = "ARM64 KVM (EL1/Host)"
paths = ["arch/arm64/kvm/"]
symbols = []
+++
# ARM64 KVM (EL1/Host): maintainer notes

These notes cover policy positions for patches to the ARM64 KVM host implementation.

- KVM must not implement workarounds that accommodate guest configurations violating the ARM ARM; flag patches that add logic to handle non-unique guest CPU affinities or that relax identification register validation.
- Do not expose ID_AA64* register fields to userspace for hardware features that KVM does not explicitly support or fully implement.
- Do not advertise a KVM capability whose ioctl statically fails in the current mode (compare x86 gating KVM_CAP_PRE_FAULT_MEMORY on tdp_enabled).
