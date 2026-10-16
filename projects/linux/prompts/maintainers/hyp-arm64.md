+++
title = "ARM64 Hyp (EL2)"
paths = ["arch/arm64/kvm/hyp/", "arch/arm64/include/asm/kvm*.h", "drivers/iommu/arm/arm-smmu-v3/pkvm/"]
symbols = ["__hyp_*"]
+++
# ARM64 Hyp (EL2): maintainer notes

These notes cover review priorities and house-style conventions for the ARM64 hypervisor code running at EL2 (nVHE and pKVM).

- Judge pKVM findings against its threat model. Breaking guest confidentiality or integrity against the host, hypervisor integrity against host or guests, or host availability against guests is a bug. Anything outside that model is not.
- Host availability against itself is out of scope. If the host kernel can panic itself, or trigger a hyp panic from its own privileged code paths, that is not a bug. Report it as a hardening improvement, and only when the fix is cheap.
- Misbehaving firmware or hardware (EL3, SMMU, GIC) is beyond the trust boundary. Do not report EL2 panics caused by such faults as bugs.
- A hyp panic that a guest can trigger (through DMA, hypercall side effects or shared memory) is a bug.
- A hyp panic that host userspace can trigger (through a syscall or ioctl that leads to a hypercall) is a bug, because it breaks the Linux rule that userspace must not crash the kernel.
- Treat two questions about an EL2 assertion separately. Correctness asks whether the assertion is the right kind of check. Severity asks whether triggering it is a bug. If a WARN_ON should be an error return but only the host kernel can reach it, it is a hardening improvement. If a guest or host userspace can reach it, it is a bug.
- If an EL2-internal invariant can be broken because of a bug elsewhere, report that other bug. Do not demote the local WARN_ON that defends the invariant.
- Do not use CONFIG_BUG=n (an EXPERT-only opt-out) as grounds to escalate a defensive WARN_ON into a NULL-dereference finding.
- BUG() is the house style in arch/arm64/kvm/hyp/nvhe/. Do not flag swapping BUG() for hyp_panic() (or the reverse) as a regression.
- A redundant kern_hyp_va() on a pointer it leaves unchanged deserves at most a cleanup note, not a bug report. The cleanup is still worth suggesting, because a redundant call hides the host-to-EL2 boundary that each kern_hyp_va() call site marks.
