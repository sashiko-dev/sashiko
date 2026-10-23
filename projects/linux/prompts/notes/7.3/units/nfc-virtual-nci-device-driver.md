# NFC VIRTUAL NCI DEVICE DRIVER

The NFC virtual NCI device driver (`drivers/nfc/virtual_ncidev.c`) exposes the `/dev/virtual_nci` misc character device to simulate an NFC Controller Interface (NCI) hardware device from userspace. Opening the device allocates and registers a per-file `nci_dev` instance (`virtual_nci_ops`), forwarding NCI frames sent by the kernel to userspace via `read()` and injecting frames written by userspace via `write()` into `nci_recv_frame()`.

## Architecture & Data Flow
- `virtual_nci_send()` must reject frames under `vdev->mtx` when `!vdev->running` or `vdev->send_buff` is non-NULL, and `virtual_nci_close()` (invoked via `nci_unregister_device()`) must set `vdev->running = false` and free `vdev->send_buff` under `vdev->mtx` so `virtual_ncidev_close()` does not leak buffers. [F16]
- `virtual_ncidev_read()` consumes `vdev->send_buff` incrementally under `vdev->mtx` by pulling `actual_len = min_t(size_t, count, vdev->send_buff->len)` bytes via `skb_pull()` only after `copy_to_user()` succeeds. Whenever `vdev->send_buff` is freed— either in `virtual_nci_close()` or when `vdev->send_buff->len == 0` in `virtual_ncidev_read()`—`vdev->send_buff` must be set to `NULL` under `vdev->mtx` because `virtual_nci_send()`, `virtual_ncidev_read()`, and `virtual_nci_close()` test it for non-NULL. [F17, F43]

## Synchronization
- In `virtual_ncidev_read()`, `vdev->mtx` must be dropped before `wait_event_interruptible(vdev->wq, vdev->send_buff)` and reacquired inside a `while (!vdev->send_buff)` loop before reading, pulling, or consuming `vdev->send_buff`. Broken by: replacing `while (!vdev->send_buff)` with `if (!vdev->send_buff)` or accessing `vdev->send_buff` without `vdev->mtx`. [F10]
- `vdev->mtx` must remain a sleeping lock (`struct mutex`) and no spinlock or preemption-disabling section may wrap `virtual_nci_send()` or `virtual_ncidev_read()` critical sections, because they call `skb_copy(skb, GFP_KERNEL)` and `copy_to_user()` while holding `vdev->mtx`. [F36]

## Object Lifetimes
- In `virtual_ncidev_close()`, `nci_unregister_device(vdev->ndev)` must complete before `nci_free_device(vdev->ndev)`, `mutex_destroy(&vdev->mtx)`, and `kfree(vdev)`, because `nci_unregister_device()` flushes NCI workqueues and invokes `virtual_nci_close()`, which locks `vdev->mtx` and frees `vdev->send_buff`. [F5]
- `nci_send_frame()` transfers ownership of `skb` to `ndev->ops->send`, and `virtual_nci_send()` stores a separate `skb_copy()` in `vdev->send_buff`; therefore `virtual_nci_send()` must always either free `skb` (`kfree_skb(skb)` on error paths) or consume it (`consume_skb(skb)` on success) before returning. [F11]

## API Contracts
- `nci_allocate_device()` returns `NULL` (not an `ERR_PTR`) on failure, and requires `virtual_nci_ops` to provide non-NULL `.open`, `.close`, and `.send` callbacks along with a non-zero `NFC_PROTO_*_MASK` protocol bitmask (`VIRTUAL_NFC_PROTOCOLS`). `virtual_ncidev_open()` must check `!vdev->ndev` (not `IS_ERR()`) and free `vdev` before returning `-ENOMEM`. [F15, F41]

## Error Handling
- In `virtual_ncidev_write()`, error paths before `nci_recv_frame(vdev->ndev, skb)` must call `kfree_skb(skb)`, whereas paths after `nci_recv_frame()` must never free `skb` because `nci_recv_frame()` unconditionally consumes or frees `skb` even when returning `-ENXIO`. [F12]

## External Contracts
- `virtual_ncidev_ioctl()` implements the `/dev/virtual_nci` UAPI command `IOCTL_GET_NCIDEV_IDX` (`0`) by copying `vdev->ndev->nfc_dev->idx` (`sizeof(nfc_dev->idx)` bytes) to `(void __user *)arg`, and must return `-ENOTTY` (not `-EINVAL`) for any other command. [F42]
