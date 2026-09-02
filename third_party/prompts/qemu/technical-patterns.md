# QEMU Technical Patterns and Anti-Patterns

## 1. QEMU Object Model (QOM) and Device Lifecycle

### 1.1 TypeInfo Definitions
- **Class Size Invariant**: When defining a `TypeInfo` for a class that extends another class
  and defines its own class struct (`MyDeviceClass`), you MUST specify `.class_size = sizeof(MyDeviceClass)`.
  Omitting `.class_size` causes QOM to allocate only the parent class's size, leading to heap corruption
  when `class_init` writes to subclass-specific function pointers or fields.
- **Instance Size Invariant**: Always ensure `.instance_size = sizeof(MyDeviceState)`.
- **Abstract Classes**: Abstract base types must specify `.abstract = true`.

### 1.2 Initialization vs Realization vs Unrealization
- `instance_init`:
  - Must NOT fail and cannot return an `Error *`.
  - Used only for setting up QOM properties, initializing mutexes/queues, and establishing child links.
  - Must NOT allocate hardware state, register MMIO regions, or communicate with the host OS.
- `DeviceRealize` (`device_class->realize`):
  - Performs resource allocation (memory regions, timers, interrupts, host file descriptors).
  - Any failure path must cleanly unwind all partial allocations and set `errp` via `error_setg()`.
  - Must check for errors after sub-device realization: if `qdev_realize()` fails, propagate and return immediately.
- `DeviceUnrealize` (`device_class->unrealize`):
  - Must be the exact symmetric inverse of `realize`.
  - Timers must be deleted (`timer_del`, `timer_free`).
  - Bottom halves (BHs) must be deleted (`qemu_bh_delete`).
  - Memory regions must be removed from parent address spaces.
  - In-flight requests and background workers must be drained/canceled before freeing state.

## 2. Memory Regions and Guest MMIO Handling

### 2.1 Bounds Checking in MemoryRegionOps
- Guest MMIO read/write callbacks (`read`, `write`) receive `hwaddr addr` and `unsigned size`.
- Never access state arrays using `addr` without validating bounds:
  - Check `addr + size <= region_size` to avoid arithmetic overflow.
  - Check array indices: `unsigned index = addr / sizeof(uint32_t); if (index >= NUM_REGISTERS) return;`.
  - Handle partial or unaligned accesses: verify that `size` matches expected register sizes (e.g. 1, 2, 4, or 8 bytes)
    or specify `.valid.min_access_size` and `.valid.max_access_size` in `MemoryRegionOps`.

### 2.2 Reentrancy Protection
- An MMIO write callback that triggers a bus transaction or notifies a guest interrupt can cause
  the guest vCPU to recursively execute another MMIO write to the same device.
- Devices must guard against reentrancy loops or use `MemoryRegionOps::impl.unassigned` / `dma_memory_read` safely.

## 3. DMA and AddressSpace Operations

### 3.1 Length and Buffer Bounds
- Any DMA transfer length provided by the guest (e.g. In descriptor headers or MMIO registers)
  must be verified against the maximum capacity of internal host buffers (`buflen`, `BUFSZ_MAX`).
- In network devices: ensure `tx_len <= sizeof(s->tx_buffer)` before copying guest packets into the buffer.
- In storage devices: ensure `sector_count * sector_size <= allocated_request_size`.

### 3.2 Mapping Symmetry
- Calls to `dma_memory_map()` or `address_space_map()` must check for a valid returned pointer and `plen != 0`.
- Every successful map MUST be paired with `dma_memory_unmap()` / `address_space_unmap()` on all paths,
  including error recovery and reset paths.

## 4. Concurrency, Locking, and Coroutines

### 4.1 Big QEMU Lock (BQL)
- Main-loop handlers and MMIO callbacks execute under the BQL (`bql_lock` / `qemu_mutex_lock_iothread`).
- Do not perform blocking I/O (disk I/O, synchronous network connect, sleep) while holding the BQL.
- When spawning worker threads or asynchronous callbacks that invoke QOM/device APIs, ensure the BQL is acquired.

### 4.2 Coroutines and AioContext
- Never hold a standard pthread `QemuMutex` across a coroutine yield (`qemu_coroutine_yield()`).
  Yielding holds the mutex while another thread or coroutine may need it, leading to deadlock.
  Use `QemuCoMutex` for coroutine-safe mutual exclusion.
- When changing AioContext (e.g., in block jobs), properly acquire the new context and release the old one.

## 5. Migration and VMState

### 5.1 Post-Load Validation
- In `VMStateDescription`, the `post_load` callback is executed when deserializing incoming state from the migration stream.
- The migration stream is potentially untrusted or from an older version. You MUST re-validate all indices, pointers,
  and state boundaries:
  - If a restored index points into an array, verify `restored_idx < ARRAY_SIZE(s->array)`.
  - If a restored length dictates dynamic allocation, verify it does not exceed maximum allowable bounds.
  - If validation fails, return `-EINVAL` to safely fail migration rather than crashing during execution.

### 5.2 Subsections and Backward Compatibility
- Optional or newly added device fields must be placed in a `VMStateDescription` subsection with a `.needed` predicate.
- The `.needed` callback must return `false` if the device is in its default/disabled state, ensuring backwards
  compatibility when migrating to older QEMU releases.

## 6. Error Reporting Conventions

### 6.1 `Error **errp` Handling
- When a function accepts `Error **errp`:
  - Never check `*errp` directly to test for error unless `ERRP_GUARD()` is placed at the top of the function.
    Callers may pass `NULL` or `&error_fatal`.
  - If a function returns a boolean success indicator (`bool foo(..., Error **errp)`), return `false` on error
    and `true` on success.
  - Never leak an error object: if an error is caught and handled internally, free it with `error_free(err)`.
