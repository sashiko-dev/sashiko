# NFC VIRTUAL NCI DEVICE DRIVER

The NFC virtual NCI device driver (`drivers/nfc/virtual_ncidev.c`) exposes the `/dev/virtual_nci` misc character device to let userspace simulate an NFC Controller Interface (NCI) hardware device. Opening the device allocates and registers a per-file `nci_dev` instance (`virtual_nci_ops`), forwarding NCI frames sent by the kernel to userspace via `read()` and injecting frames written by userspace via `write()` into `nci_recv_frame()`.

## Architecture & Data Flow

- Under `vdev->mtx`, `virtual_nci_send()` must reject frames when `!vdev->running` or `vdev->send_buff` is non-NULL, and `virtual_nci_close()` (invoked by `nci_unregister_device()` during `virtual_ncidev_close()`) must set `vdev->running = false` and free `vdev->send_buff`. [F16]
- Whenever `vdev->send_buff` is freed or consumed—whether in `virtual_nci_close()`, on error/teardown paths, or after draining it (`vdev->send_buff->len == 0`) in `virtual_ncidev_read()`—`vdev->send_buff` must be set to `NULL` under `vdev->mtx`, because `virtual_nci_send()`, `virtual_ncidev_read()`, and `virtual_nci_close()` test `vdev->send_buff` for non-NULL. [F17]

## Synchronization

- In `virtual_ncidev_read()`, `vdev->mtx` must be dropped before `wait_event_interruptible(vdev->wq, vdev->send_buff)` and reacquired afterward inside a `while (!vdev->send_buff)` loop so `vdev->send_buff` is revalidated under the lock before reading, pulling, or consuming it. Broken by: replacing `while (!vdev->send_buff)` with `if (!vdev->send_buff)` or accessing `vdev->send_buff` without `vdev->mtx`. [F10]
- `vdev->mtx` must remain a sleeping lock (`struct mutex`) in process context because `virtual_nci_send()` calls `skb_copy(skb, GFP_KERNEL)` and `virtual_ncidev_read()` calls `copy_to_user()` while holding `vdev->mtx`. Broken by: converting `vdev->mtx` to `spinlock_t` or disabling preemption around those calls. [F36]

## Object Lifetimes

- In `virtual_nci_send()`, `nci_send_frame()` transfers ownership of the caller-supplied `skb` to `ndev->ops->send` while `virtual_nci_send()` stores a separate `skb_copy()` in `vdev->send_buff`; every return path in `virtual_nci_send()` must either free `skb` (`kfree_skb(skb)` on error) or consume it (`consume_skb(skb)` on success). [F11]
- In `virtual_ncidev_close()`, `nci_unregister_device(vdev->ndev)` must complete before `nci_free_device(vdev->ndev)`, `mutex_destroy(&vdev->mtx)`, and `kfree(vdev)`, because `nci_unregister_device()` flushes NCI workqueues and invokes `virtual_nci_close()`, which locks `vdev->mtx` and frees `vdev->send_buff`. [F5]
- Per-file `struct virtual_nci_dev` resources (`vdev` and `vdev->ndev`) are tied to file open/close lifetime and must be manually allocated in `virtual_ncidev_open()` and manually freed on open failure and in `virtual_ncidev_close()`. Broken by: using `devm_kzalloc(miscdev.this_device, ...)` or omitting `nci_free_device(vdev->ndev)` / `kfree(vdev)`. [F33]

## API Contracts

- `nci_allocate_device()` returns `NULL` (not an `ERR_PTR`) when allocation fails or when `virtual_nci_ops` lacks `.open`, `.close`, or `.send` callbacks or `VIRTUAL_NFC_PROTOCOLS` is `0` / not a valid `NFC_PROTO_*_MASK` bitmask; `virtual_ncidev_open()` must check `!vdev->ndev` (never `IS_ERR(vdev->ndev)`) and free `vdev` before returning `-ENOMEM`. [F15, F41]

## Error Handling

- In `virtual_ncidev_write()`, error paths before `nci_recv_frame(vdev->ndev, skb)` must call `kfree_skb(skb)`, whereas paths after `nci_recv_frame()` must never free `skb` because `nci_recv_frame()` unconditionally consumes or frees `skb` even when returning `-ENXIO`. [F12]
- In `virtual_ncidev_open()`, `vdev` must be zero-initialized via `kzalloc()`, and `mutex_init(&vdev->mtx)`, `init_waitqueue_head(&vdev->wq)`, and `nci_set_drvdata(vdev->ndev, vdev)` must run before `nci_register_device(vdev->ndev)`; if `nci_register_device()` fails, clean up with `nci_free_device()`, `mutex_destroy()`, and `kfree(vdev)` without calling `nci_unregister_device()`. [F14]

## External Contracts

- `virtual_ncidev_read()` must consume `vdev->send_buff` incrementally under `vdev->mtx` by pulling `actual_len = min_t(size_t, count, vdev->send_buff->len)` bytes via `skb_pull()` only after `copy_to_user()` succeeds, and freeing/nulling `vdev->send_buff` only when `vdev->send_buff->len == 0`. Broken by: freeing `vdev->send_buff` on a partial read (`count < vdev->send_buff->len`) or calling `skb_pull()` before `copy_to_user()` succeeds. [F43]
- `virtual_ncidev_ioctl()` implements UAPI command `IOCTL_GET_NCIDEV_IDX` (`0`) by copying `vdev->ndev->nfc_dev->idx` (`sizeof(nfc_dev->idx)` bytes) to `(void __user *)arg` and must return `-ENOTTY` for any unknown command. [F42]

## Known Pitfalls

- In `virtual_ncidev_write()`, `alloc_skb(count, GFP_KERNEL)` and `copy_from_user(skb_put(skb, count), buf, count)` do not NUL-terminate `skb->data`, so any NUL-byte check on `skb->data` must bound the scan to `count` via `strnlen(skb->data, count)`. Broken by: calling unbounded `strlen(skb->data)` or passing a bound larger than `count`. [F29]
