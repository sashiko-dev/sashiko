# PCI SUBSYSTEM [RUST]

This component provides safe Rust abstractions for the Linux kernel PCI subsystem (`include/linux/pci.h`) and C helper wrappers (`rust/helpers/pci.c`). It enables Rust modules to register PCI drivers (`Adapter`, `Driver`, `module_pci_driver!`), define match tables (`DeviceId`, `IdTable`, `pci_device_table!`, `Class`, `ClassMask`, `Vendor`), interact with `struct pci_dev` (`Device`) across lifecycle contexts (`Normal`, `Core`, `Bound`), and manage BAR MMIO mappings (`Bar` via `Devres`) and PCI IRQ vectors (`irq_vector`, `request_irq`, `request_threaded_irq`).

## Architecture & Data Flow

- `AlwaysRefCounted` (`pci_dev_get` / `pci_dev_put`) must only be implemented for `Device<device::Normal>` (never `Core`, `Bound`, or `CoreInternal`), and converting any `&Device<Ctx>` into an `ARef` must yield `ARef<Device<device::Normal>>` so `Bound` or `Core` guarantees cannot outlive `probe`/`unbind` or device unbind. [F4]
- `Device<Ctx>` must remain a `#[repr(transparent)]` wrapper around `Opaque<bindings::pci_dev>` with only zero-sized `PhantomData<Ctx>` so pointer casts between `*mut bindings::pci_dev` and `Device<Ctx>` in `Adapter::probe_callback`, `Adapter::remove_callback`, `AlwaysRefCounted::dec_ref`, and `TryFrom::try_from` preserve layout. [F28]
- `pci::DeviceId` must remain `#[repr(transparent)]` over `bindings::pci_device_id` with `RawDeviceIdIndex::DRIVER_DATA_OFFSET` equal to `offset_of!(bindings::pci_device_id, driver_data)` and `DeviceId::index` returning `self.0.driver_data` so `IdArray` byte-offset writes and `Adapter::probe_callback` pointer casts stay layout-compatible. [F38]
- Unlike `Class` and `Vendor` which transparently wrap any `u32` or `u16`, `ClassMask` is a `#[repr(u32)]` enum restricted to discriminants `0xffffff` (`Full`) and `0xffff00` (`ClassSubclass`); raw `u32` values must be validated via `ClassMask::try_from` rather than transmuted, and every `ClassMask` variant must have a match arm in `ClassMask::try_from` because its `_ => Err(EINVAL)` wildcard disables exhaustiveness checks. [F31, F42]
- `Bar::new` must verify that `pdev.resource_len(num)?` is non-zero, `pci_request_region` returns `0`, `pci_iomap` returns a non-zero address, and `IoRaw::new` succeeds before constructing `Bar`, because `Bar::deref` and `Bar::release` rely on the type invariant that `self.io` holds a valid mapped MMIO region. [F48]

## Synchronization

- Because `Device<device::Normal>` is `Sync`, all its methods must be thread-safe without driver-core locking. Device mutations (`enable_device_mem`, `set_master`) and `crate::dma::Device` must only be implemented on `Device<device::Core>` (restricted to `Driver::probe` and `Driver::unbind`), while bound resource registrations (`iomap_region_sized`, `iomap_region`, `irq_vector`, `request_irq`, `request_threaded_irq`) must require `Device<device::Bound>`, with `Bar::new` wrapped in `Devres::new` so MMIO access is revoked on unbind. [F9, F50, F60]
- The `pci::Driver` trait must require `: Send` because the `Pin<KBox<Self>>` returned by `Driver::probe` is stored via `set_drvdata` in `Adapter::probe_callback` and reclaimed and dropped in `Adapter::remove_callback`, which is not guaranteed to run on the same thread. [F45]

## Object Lifetimes

- `Adapter::probe_callback` must store the `Pin<KBox<T>>` returned by `T::probe` via `pdev.as_ref().set_drvdata(data)` only on success, `Adapter::register` must unconditionally set `(*pdrv.get()).remove` to `Some(Self::remove_callback)` even when `T::unbind` uses its default no-op implementation, and `Adapter::remove_callback` (or any callback accessing drvdata) must pass the exact `ForeignOwnable` type `Pin<KBox<T>>` to `drvdata_obtain`/`drvdata_borrow`, reclaiming ownership via `drvdata_obtain` and invoking `T::unbind(pdev, data.as_ref())` before `data` is dropped. [F39, F54, F65]
- `Bar` must hold an owned `ARef<Device>` (`self.pdev`) for its entire lifetime so that the underlying `struct pci_dev` remains reference-counted via `pci_dev_get` until after `Bar::drop` calls `pci_iounmap` and `pci_release_region` on `self.pdev.as_raw()`. [F2]

## API Contracts

- In `DeviceId` constructors (`from_id`, `from_class`, `from_class_and_vendor`), unconstrained ID fields (`vendor`, `device`, `subvendor`, `subdevice`) must be initialized to `DeviceId::PCI_ANY_ID` (`!0`), while unused `class`, `class_mask`, `driver_data`, and `override_only` fields are initialized to `0`. [F17]
- `Bar::new`, `Device::resource_start`, and `Device::resource_len` must validate the `u32` BAR index via `Bar::index_is_valid` (`index < bindings::PCI_NUM_RESOURCES`) and convert it to `i32` with fallible `try_from`/`try_into` before passing it to C PCI resource helpers. [F35]

## Error Handling

- `Bar::new` must request the PCI region via `bindings::pci_request_region` before mapping it with `bindings::pci_iomap`, and clean up in reverse order: if `pci_iomap` returns null (`ioptr == 0`), it must call `bindings::pci_release_region(pdev.as_raw(), num)` and return `Err(ENOMEM)` (not check `IS_ERR`); if `IoRaw::new(ioptr, len as usize)` fails or when `Bar::drop` runs, `Bar::do_release(pdev, ioptr, num)` must call `bindings::pci_iounmap` before `bindings::pci_release_region`. [F12, F13, F51]
- `Device::irq_vector` must check the signed `c_int` return value of `bindings::pci_irq_vector(self.as_raw(), index)` for `< 0` and convert negative values via `Error::from_errno(irq)` before casting `irq as u32` to construct `IrqRequest::new`. [F15]
- `TryFrom<&device::Device<Ctx>> for &Device<Ctx>` must verify `bindings::dev_is_pci(dev.as_raw())` and return `Err(EINVAL)` when false before using `container_of!(dev.as_raw(), bindings::pci_dev, dev)` to downcast to `&Device<Ctx>`. [F16]

## External Contracts

- `Adapter::register` must initialize `name`, `probe`, `remove`, and `id_table` on `struct pci_driver` before calling `bindings::__pci_register_driver`, because `__pci_register_driver` copies `drv->name` into `drv->driver.name` and immediately probes matching devices using `id_table` and `probe`. [F64]
- `Bar::new` must pass `0` as the `maxlen` argument to `bindings::pci_iomap` and pass the full byte length `len as usize` from `Device::resource_len` to `IoRaw::new`, so the complete BAR is mapped for runtime `try_read*`/`try_write*` offsets `>= SIZE` while `IoRaw::new` verifies `len >= SIZE`. [F69]
- In `rust/helpers/pci.c`, `rust_helper_pci_irq_vector` must be compiled only under `#ifndef CONFIG_PCI_MSI` because `include/linux/pci.h` declares `pci_irq_vector` as an external C function when `CONFIG_PCI_MSI` is enabled and as `static inline` only when `CONFIG_PCI_MSI` is disabled. [F58]

## Known Pitfalls

- `Class` stores PCI class codes in 24-bit format (base class in bits 23-16, subclass in bits 15-8, programming interface in bits 7-0), and `Class::to_24bit_class` shifts 16-bit C `PCI_CLASS_*` constants (`val <= 0xFFFF`) left by 8 bits to align with `ClassMask::Full` (`0xffffff`) and `ClassMask::ClassSubclass` (`0xffff00`). Broken by: passing a 16-bit `PCI_CLASS_*` constant directly to `Class::from_raw`, or passing a 24-bit class with base class `0x00` and non-zero interface (`0 < val <= 0xFFFF`) to `Class::to_24bit_class`. [F29]
