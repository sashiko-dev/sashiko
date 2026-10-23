+++
title = "ARM GICv5"
paths = ["drivers/irqchip/irq-gic-v5*", "arch/arm64/kvm/vgic/vgic-v5*", "arch/arm64/kvm/hyp/vgic-v5*", "include/linux/irqchip/arm-gic-v5.h", "Documentation/virt/kvm/devices/arm-vgic-v5.rst"]
symbols = ["gicv5_*", "GICV5_*", "ICC_PPI_*", "ICH_PPI_*", "ICC_ICSR*", "ICC_IAFFIDR*", "ICH_CONTEXTR_EL2", "IRS_IDR*", "FEAT_GCIE*", "GCIE_LEGACY", "gsb_sys", "gsb_ack"]
+++
# ARM GICv5: maintainer notes

Review conventions and policies for the GICv5 irqchip driver and the KVM GICv5 vGIC.

- A commit message that justifies GICv5 behaviour by citing GICv3 behaviour should be questioned; GICv5 is a redesign, and v3-shaped reasoning or remedies are not accepted as justification.
- Support for the IMPLEMENTATION DEFINED PPIs above the 64 architected ones was removed from KVM on purpose because it could not be tested; a patch that reintroduces 128-PPI handling needs an explicit justification.
- Do not add a second, redundant invalidate right after a function that already invalidated; one was removed upstream as noise, and adding it back is not considered safety.
- Do not use WARN() for firmware-described values: a malformed firmware description warrants pr_warn(FW_BUG ...), not a backtrace. Reserve WARN() for conditions that indicate a kernel bug.
- For KVM userspace register overrides, recommending that the guest-side guard be added to the userspace path is the fix that gets sent back; propose a validation step at the end of restore instead.
- When reporting divergences between guest and userspace register handlers, compare them exhaustively over the register table and say how many registers were compared, so the claim is something a maintainer can act on.
- A userspace write that is accepted and silently discarded is worse than one that is rejected: either the write takes effect or it is rejected.
- A device attribute documented as read-only must reject writes, and that rejection must be tested.
- For a new device attribute, every documented error code must be reachable and every reachable one documented; an unreachable documented error is a documentation bug.
- When unsigned sizing arithmetic is only safe because of a guard in a different function, say so in the review, since that separation makes the construct fragile even when it is currently safe.
