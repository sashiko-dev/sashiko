# QEMU Subsystem Guide Index

| Trigger Pattern | Target Guide | Description |
|---|---|---|
| `qom/`, `hw/core/`, `TypeInfo`, `OBJECT(`, `class_size` | `qom.md` | QOM Object Model, class inheritance, realization, reference counting |
| `system/memory*`, `MemoryRegionOps`, `memory_region_init_io`, `MMIO` | `memory.md` | Memory regions, MMIO read/write bounds, subpage operations |
| `system/dma*`, `dma_memory_read`, `dma_memory_write`, `AddressSpace` | `dma.md` | DMA access, physical memory mapping, bounce buffers |
| `hw/virtio/`, `virtqueue*`, `vhost`, `VRing` | `virtio.md` | VirtIO transport, virtqueues, descriptor chaining, guest ring indexing |
| `migration/`, `VMStateDescription`, `VMSTATE_`, `post_load` | `migration.md` | Migration stream serialization, subsection preconditions, post-load validation |
| `block/`, `include/block/`, `bdrv_`, `BlockDriverState` | `block.md` | Block layer, request lifecycle, coroutines, AioContext locking |
| `BQL`, `bql_lock`, `qemu_mutex_lock_iothread`, `coroutine`, `aio_` | `concurrency.md` | Big QEMU Lock, bottom halves, coroutines, thread safety |
| `Error **`, `error_setg`, `error_propagate`, `ERRP_GUARD` | `error-handling.md` | Error propagation, memory leak prevention, error conventions |
| `hw/net/`, `qemu_send_packet`, `NetClientState` | `net.md` | Network device emulation, packet buffer boundaries, descriptor rings |
| `hw/scsi/`, `hw/nvme/`, `hw/ide/`, `SCSIRequest` | `storage.md` | Storage device models, mutable guest parameters, command validation |
