+++
title = "KVM"
paths = ["virt/kvm/", "include/linux/kvm*"]
symbols = ["kvm_*"]
+++
# KVM: maintainer notes

Cross-architecture KVM maintainer policies on guest-visible ABI, assertions, new flags and review priorities.

- New guest-visible behaviour (a new ioctl, VM or vCPU capability, exit reason, or emulated instruction) must be off by default and discoverable through the architecture's standard enumeration interface (e.g. KVM_CHECK_EXTENSION / KVM_CAP_*, KVM_GET_SUPPORTED_CPUID2 on x86, ID-register feature bits on arm64); silently-on features break live migration and capability negotiation.
- Do not add WARN_ON / BUG_ON whose condition a guest or unprivileged host userspace can drive; use pr_warn_once(), return an error to userspace, or drop the assertion. Asserts are fine only for "the kernel itself is buggy" paths.
- Long-running loops over guest-driven counts are background context, not a checklist item: do not demand cond_resched() or any specific yield mechanism, since the right mitigation is flow-specific (yielding under mmu_lock can hurt fault throughput, dropping the lock can starve the guest).
- New memslot and vCPU flags should default to set-once (immutable); making a flag mutable requires an explicit justification.
- Do not flag pre-existing logic when a patch only mechanically updates a helper signature or argument.
