+++
title = "QOM Object and Device Lifecycle Rules"
paths = [
    "hw/core/",
    "qom/",
]
symbols = [
    "TypeInfo",
    "class_init",
    "instance_init",
    "instance_finalize",
    "realize",
    "unrealize",
    "object_ref",
    "object_unref",
    "object_initialize_child",
    "object_property_add*",
    "device_cold_reset",
]
+++

# QOM & Device Lifecycle

The QEMU Object Model owns every device instance's birth and death. Most
resource bugs are asymmetry between the two.

## Lifecycle symmetry

- `class_init` configures the class; `instance_init` (or the legacy
  `InstanceInit`) must not depend on `realize` having run.
- `realize` acquires resources (MemoryRegions, chardev backends, file
  descriptors, timers, bottom halves, g_malloc'd buffers). `unrealize` must
  release **every** one of them, in an order that cannot race with a guest
  access still in flight: memory regions delisted before or atomically with
  tearing down their backing, timers deleted before the state they touch is
  freed, bottom halves cancelled.
- `instance_finalize` runs after `unrealize`; anything it frees must be
  allocated in `instance_init` or unconditionally in `realize`, or the free
  must be guarded by the same condition as the allocation.

## Reference counting

- `object_ref`/`object_unref` balance: a new ref acquired on a hot path must be
  dropped on every terminal path, including early returns and error returns.
- Child objects created with `object_initialize_child`/`object_property_add_*`
  are owned by the parent; flag manual `object_unref` on owned children as a
  double-free risk, and unowned children kept alive only by a leak as a UAF
  risk on unrealize.

## Hot-unplug

- The device can be unplugged at any moment the guest (or management) decides.
  Guest-triggerable work that outlives unrealize (queued DMA, pending timer
  callback) is a use-after-free vector; check the cancel/delete symmetry.
- Check that a device whose backend disappeared (chardev closed, block node
  ejected) stops touching the freed backend.
