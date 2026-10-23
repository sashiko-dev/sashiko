# UACCE ACCELERATOR FRAMEWORK

The Unified/User-space-access-intended Accelerator Framework (UACCE) provides a character device and sysfs interface for hardware accelerators supporting Shared Virtual Addressing (SVA) via IOMMU PASIDs. It manages accelerator device registration (`uacce_alloc`, `uacce_register`, `uacce_remove`), per-file hardware queue lifecycles (`struct uacce_queue`), IOMMU SVA binding, and user-space memory mappings of MMIO and device-user-share (DUS) queue regions.

## Architecture & Data Flow

- User-space address space mappings must be recorded per queue via `q->mapping = filep->f_mapping` in `uacce_fops_open()` and unmapped per queue via `unmap_mapping_range(q->mapping, 0, 0, 1)` in `uacce_remove()`, because different containers may open the same `struct uacce_device` through distinct inodes and VMA mappings of MMIO and DUS regions must be invalidated before parent device removal. Broken by: storing a single `inode` or `address_space` on `struct uacce_device`, or skipping `unmap_mapping_range()` in `uacce_remove()`. [F30, F57]
- `enum uacce_qfrt`, `UACCE_MAX_REGION`, the `switch (type)` in `uacce_fops_mmap()`, and the `uacce->qf_pg_num[]` sysfs attributes and visibility checks in `uacce_dev_is_visible()` must be updated in lockstep when adding or changing queue file region types. [F31]
- `uacce_dev_attrs` must end with a `NULL` sentinel pointer because it is passed as `uacce_dev_group.attrs` to sysfs attribute group registration. [F39]

## Synchronization

- Lock acquisition across UACCE and MM paths must follow `uacce->mutex` -> `mm->mmap_lock` -> `q->mutex`, as `uacce_fops_open()` takes `mmap_lock` inside `iommu_sva_bind_device()` under `uacce->mutex`, `uacce_fops_mmap()` and `uacce_vma_close()` take `q->mutex` under `mmap_lock`, and `uacce_remove()` nests `q->mutex` inside `uacce->mutex`. Broken by: taking `uacce->mutex` in `uacce_fops_mmap()`, `uacce_vma_close()`, or `uacce->ops->mmap`, or taking `mmap_lock` (e.g., via `copy_to_user`/`copy_from_user`) under `q->mutex`. [F58]
- In `uacce_remove()`, `uacce->parent = NULL` and `uacce->ops = NULL` must be cleared under `uacce->mutex` after holding `q->mutex` around `uacce_put_queue(q)` (which sets `q->state = UACCE_Q_ZOMBIE`), and every file operation must check `uacce_queue_is_valid(q)` under `uacce->mutex` or `q->mutex` (or `uacce->parent != NULL` under `uacce->mutex` in `uacce_fops_open()`, which can race with `uacce_remove()` before `xa_erase()`) before dereferencing `uacce->ops`. [F6, F60, F69]
- All insertions into, deletions from, and iterations over `uacce->queues` (in `uacce_fops_open()`, `uacce_fops_release()`, and `uacce_remove()`) must be serialized by `uacce->mutex`, and `q->uacce`, `q->state`, `q->mapping`, and `q->mutex` must be initialized before `list_add(&q->list, &uacce->queues)` and unlocking `uacce->mutex`. [F13]

## Object Lifetimes

- `uacce_remove()` must disable and unbind each `struct uacce_queue` in `uacce->queues` without calling `list_del(&q->list)` or `kfree(q)`, because open file descriptors and VMAs retain `filep->private_data` and `vma->vm_private_data` until `uacce_fops_release()` unlinks `q->list` under `uacce->mutex` and calls `kfree(q)`. [F2]
- `uacce_put_queue()` must check `q->state` (`UACCE_Q_INIT` or `UACCE_Q_STARTED`) before dereferencing `uacce->ops->stop_queue` or `uacce->ops->put_queue` and must set `q->state = UACCE_Q_ZOMBIE`, because `uacce_fops_release()` unconditionally calls `uacce_put_queue(q)` even after `UACCE_CMD_PUT_Q` or after `uacce_remove()` has already put the queue and set `uacce->ops = NULL`. [F3]
- On queue teardown in `uacce_fops_release()` and `uacce_remove()`, `uacce_put_queue(q)` must be called before `uacce_unbind_queue(q)` so `stop_queue` and `put_queue` quiesce hardware access to `q->pasid` before `iommu_sva_unbind_device(q->handle)` unbinds the PASID; `uacce_unbind_queue()` must check `q->handle` for `NULL` and clear `q->handle = NULL` after unbinding because both functions invoke it on the same queue. [F4, F70]
- `uacce_fops_mmap()` transfers ownership of the allocated `struct uacce_qfile_region` to `q->qfrs[type]` only after `uacce->ops->mmap(q, vma, qfr)` succeeds under `q->mutex` (freeing `qfr` on any error), and `uacce_vma_close()` must clear `q->qfrs[vma->vm_pgoff] = NULL` under `q->mutex` when freeing `qfr`. [F7]
- Once `uacce_alloc()` calls `device_initialize(&uacce->dev)` and inserts `uacce` into `uacce_xa`, `uacce` is reference-counted via `uacce->dev` (`uacce_release`) and must be cleaned up via `uacce_remove()`—even if `uacce_register()` was never called. Broken by: calling `kfree(uacce)` or `put_device(&uacce->dev)` directly in a driver error path after `uacce_alloc()`. [F55]
- `uacce_alloc()` copies `interface->flags` and `interface->ops` and formats `interface->name` into `uacce->dev` before returning, so `struct uacce_interface` may be stack-allocated by the caller whereas the pointed-to `struct uacce_ops` must remain valid until `uacce_remove()` clears `uacce->ops`. Broken by: storing a pointer to `struct uacce_interface` in `struct uacce_device`, or passing a stack-allocated `struct uacce_ops`. [F49]

## API Contracts

- `uacce_fops_mmap()` sets `vma->vm_ops = &uacce_vm_ops`, sets `vma->vm_private_data = q`, and repurposes `vma->vm_pgoff` as the `enum uacce_qfrt` region index used by `uacce_vma_close()` to index `q->qfrs[vma->vm_pgoff]`; driver `uacce->ops->mmap` callbacks must not overwrite `vma->vm_ops` or `vma->vm_private_data`, and must restore `vma->vm_pgoff` before returning if they temporarily zero it for helpers like `dma_mmap_coherent()`. [F64, F65]
- In `struct uacce_ops`, `isolate_err_threshold_read` and `isolate_err_threshold_write` must both be implemented if either is non-NULL, because `uacce_dev_is_visible()` exposes the read-write `isolate_strategy` sysfs attribute whenever either callback is set, and `isolate_strategy_show()` and `isolate_strategy_store()` call them without NULL checks. [F32]
- Sysfs handlers (`available_instances_show()`, `isolate_show()`, `isolate_strategy_show()`, and `isolate_strategy_store()`) call `uacce->ops` without holding `uacce->mutex`, so driver implementations of `get_available_instances`, `get_isolate_state`, and `isolate_err_threshold_read`/`write` must synchronize their own state. [F63]
- Parent drivers must populate `uacce->qf_pg_num[]` in `PAGE_SIZE` page counts after `uacce_alloc()` and before `uacce_register()`, because `cdev_device_add()` evaluates `uacce_dev_is_visible()` during registration and `region_mmio_size_show()`/`region_dus_size_show()` shift `qf_pg_num` left by `PAGE_SHIFT`. [F68]
- Callers of `uacce_alloc()` must NUL-terminate `interface->name` within `UACCE_MAX_NAME_SIZE` bytes because `uacce_alloc()` passes `interface->name` to `dev_set_name()` with an unbounded `"%s-%d"` format specifier. [F48]
- Any change to the signatures of `uacce_alloc()`, `uacce_register()`, or `uacce_remove()` must update both the `IS_ENABLED(CONFIG_UACCE)` declarations and the `!IS_ENABLED(CONFIG_UACCE)` inline stubs in `include/linux/uacce.h` in lockstep. [F33]

## Error Handling

- In `uacce_bind_queue()`, `q->handle` must only be assigned after verifying both `!IS_ERR(handle)` from `iommu_sva_bind_device()` and `pasid != IOMMU_PASID_INVALID` from `iommu_sva_get_pasid()`, and `iommu_sva_unbind_device(handle)` must be called on `IOMMU_PASID_INVALID` before returning `-ENODEV`. [F17]

## External Contracts

- Because `uacce_fops_compat_ioctl()` only converts `arg` with `compat_ptr(arg)` and forwards `cmd` unchanged to `uacce_fops_unl_ioctl()` and `uacce->ops->ioctl`, UAPI ioctl structures such as `struct hisi_qp_ctx` and `struct hisi_qp_info` must have identical sizes and field offsets on 32-bit and 64-bit ABIs. Broken by: adding a pointer, `long`, or unaligned `__u64` field with differing 32/64-bit padding without compat ioctl translation. [F37]
- `isolate_strategy_store()` must reject `unsigned long val > UACCE_MAX_ERR_THRESHOLD` (65535) with `-EINVAL` before passing `val` to `uacce->ops->isolate_err_threshold_write()`, whose parameter is `u32`. [F46]
