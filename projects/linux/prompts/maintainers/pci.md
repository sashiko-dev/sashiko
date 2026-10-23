+++
title = "PCI"
paths = ["drivers/pci/"]
symbols = ["pci_epc_*", "pci_epf_*", "pci_ep_*"]
+++
# PCI: maintainer notes

Conventions the PCI subsystem expects from drivers and new code.

- New code should not use the legacy MSI API (pci_enable_msi() / pci_disable_msi()); flag such uses and ask for pci_alloc_irq_vectors() / pci_free_irq_vectors() instead.
- The PCI subsystem owns device naming: drivers must not rename their parent PCI device or other bus-owned devices (e.g. dev_set_name() on &pdev->dev); naming child devices the driver creates itself is fine.
