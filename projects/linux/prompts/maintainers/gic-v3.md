+++
title = "ARM GICv3/v4"
paths = ["drivers/irqchip/irq-gic-v3*", "drivers/irqchip/irq-gic-v4*", "include/linux/irqchip/arm-gic-v3.h", "include/linux/irqchip/arm-gic-v4.h", "arch/arm64/kvm/vgic/vgic-v3*", "arch/arm64/kvm/vgic/vgic-v4*", "arch/arm64/kvm/vgic/vgic-its*", "arch/arm64/kvm/vgic/vgic-mmio*"]
symbols = ["GICD_*", "GICR_*", "GITS_*", "ICH_LR*", "ICC_SGI1R*", "its_vpe*", "vgic_its*"]
+++
# ARM GICv3/v4: maintainer notes

Upstream positions on how GICv3/v4 and KVM vGIC changes should be shaped, beyond what the code itself dictates.

- For userspace-visible vGIC registers, upstream's settled position is that architectural compliance wins: when a computed read-side field misbehaves for userspace (for example before the device is configured), harden the computation rather than dropping the field from the userspace view. Do not cite the earlier, reverted removal of the GICR_TYPER "last" bit from the userspace view as precedent for hiding a field.
- The vGIC interrupt migration path deliberately does not preserve pending state across a change of target; patches that add pending-state transfer to make migration "lossless" add complexity for a property the architecture does not promise and are not wanted.
