# PCI Subsystem Details

## PCI Endpoint Error Return Conventions

Passing an error pointer to functions expecting a valid `struct pci_epc *`
or `struct pci_epf *` causes kernel crashes when the pointer is
dereferenced. For example, `pci_epf_destroy()` dereferences its argument
unconditionally (to call `device_unregister()`), so passing an ERR_PTR
value to it will crash. Note that `pci_epc_put()` is safe against ERR_PTR
values because it checks `IS_ERR_OR_NULL()` before proceeding.

The following PCI endpoint functions return `ERR_PTR()` on failure, not NULL:

- `pci_epc_get()` (`drivers/pci/endpoint/pci-epc-core.c`) - returns
  `ERR_PTR(-EINVAL)` on failure; use `IS_ERR()` to check, not `!ptr`
- `pci_epf_create()` (`drivers/pci/endpoint/pci-epf-core.c`) - returns
  `ERR_PTR(-ENOMEM)` or other error codes on failure; use `IS_ERR()` to
  check, not `!ptr`

## Legacy PCI MSI APIs

New code using the legacy MSI APIs will lack MSI-X support and the error
handling flexibility of the modern IRQ vector interface. The kernel source
(`drivers/pci/msi/api.c`) explicitly labels `pci_enable_msi()` and
`pci_disable_msi()` as "Legacy device driver API" and directs callers to
use `pci_alloc_irq_vectors()` / `pci_free_irq_vectors()` instead.

**Legacy APIs:**
- `pci_enable_msi()` / `pci_disable_msi()` -- superseded by the generic
  IRQ vector allocation interface

**Modern replacement:** Use `pci_alloc_irq_vectors()` and
`pci_free_irq_vectors()`:

```c
// WRONG - legacy API, does not support MSI-X
ret = pci_enable_msi(pdev);
if (ret)
    return ret;

// CORRECT - supports MSI, MSI-X, and legacy INTx interrupts
ret = pci_alloc_irq_vectors(pdev, 1, 1, PCI_IRQ_ALL_TYPES);
if (ret < 0)
    return ret;
```

## PCI IRQ Vector Cleanup in Error Paths

Failing to call `pci_free_irq_vectors()` in error paths after successful
`pci_alloc_irq_vectors()` leaks IRQ resources, preventing future allocations
and potentially exhausting system IRQ capacity.

Every error path after successful `pci_alloc_irq_vectors()` must call
`pci_free_irq_vectors()` before returning.

```c
// WRONG: IRQ vectors leaked on init failure
ret = pci_alloc_irq_vectors(pdev, 1, 1, PCI_IRQ_ALL_TYPES);
if (ret < 0)
    return ret;

ret = some_init(pdev);
if (ret)
    return ret;  // BUG: IRQ vectors not freed

// CORRECT: Proper cleanup on error
ret = pci_alloc_irq_vectors(pdev, 1, 1, PCI_IRQ_ALL_TYPES);
if (ret < 0)
    return ret;

ret = some_init(pdev);
if (ret)
    goto free_irq;

return 0;

free_irq:
    pci_free_irq_vectors(pdev);
    return ret;
```

## Device Naming Conventions

Renaming a parent or bus device from a driver causes confusion in sysfs,
breaks userspace tools expecting standard naming, and interferes with the
PCI subsystem's device management.

Drivers must NOT call `dev_set_name()` on their parent PCI device. The PCI
subsystem owns device naming.

```c
// WRONG: Driver renaming its parent PCI device
static int driver_probe(struct pci_dev *pdev)
{
    struct device *dev = &pdev->dev;
    dev_set_name(dev, "my-device");  // Don't do this
}

// CORRECT: Naming a newly created child device owned by the driver
struct device *child = kzalloc(sizeof(*child), GFP_KERNEL);
dev_set_name(child, "child-%d", id);
```

## Quick Checks

- **EPC/EPF return values**: verify that `pci_epc_get()` and
  `pci_epf_create()` returns are checked with `IS_ERR()`, not `!ptr`, and
  that error pointers are not passed to `pci_epf_destroy()`
- **Legacy MSI API**: flag uses of `pci_enable_msi()` and
  `pci_disable_msi()` in new code
- **IRQ vector cleanup**: after `pci_alloc_irq_vectors()` succeeds, verify
  all error paths call `pci_free_irq_vectors()`
- **Device naming**: verify `dev_set_name()` is not called on `&pdev->dev`
  or other bus-owned device structures
