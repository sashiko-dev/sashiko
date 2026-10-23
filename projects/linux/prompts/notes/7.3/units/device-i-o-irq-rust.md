# DEVICE I/O & IRQ [RUST]

This unit provides the core Rust kernel abstractions for device memory-mapped I/O (MMIO), system memory I/O views, typed hardware registers (`register!`), system resource reservations (`Resource`, `Region`), I/O polling (`read_poll_timeout*`), and interrupt request (`irq::Registration`, `ThreadedRegistration`) handling.

## Architecture & Data Flow

- `MmioRaw` and `Mmio` hold provenance-free `__iomem` pointers accessed via `readb`/`writeb`/`memcpy_fromio`/`memcpy_toio`, whereas `SysMem` holds kernel virtual memory pointers accessed via `read_volatile`/`write_volatile`/`memcpy`; never wrap or dereference a pointer from one address space as the other, or dereference `IoBackend::as_ptr` directly in Rust. [F86]
- `Region<SIZE>` requires 4-byte aligned base address and byte size with `size >= SIZE` to uphold `KnownSize` (`MIN_ALIGN = 4`) and padding-free `IntoBytes` invariants; construct `*mut Region<SIZE>` from runtime inputs via `Region::ptr_try_from_raw_parts_mut` or `MmioRaw::new_region`, never `Region::ptr_from_raw_parts_mut` without checking those invariants. [F34]
- Every view returned by `IoBase::as_view` must have a runtime base address aligned to at least `Self::Target::MIN_ALIGN` and byte size of at least `Self::Target::MIN_SIZE`. Because `io_view_assert` passes `base = 0` to `offset_valid` at compile time, it must assert `Alignment::of::<U>().as_usize() <= IO::Target::MIN_ALIGN.as_usize()` in addition to `offset_valid::<U>(0, offset, IO::Target::MIN_SIZE)`. [F35, F59]
- `IoMem<'a, SIZE>` and `ExclusiveIoMem<'a, SIZE>` must remain covariant over `'a` and only hold data valid for the full bound scope of `self.dev`, because their manual `CovariantForLt` implementations and `into_devres` erase `'a` to `'static` inside `DevresLt`. Broken by: adding a field invariant over `'a` or borrowing data shorter than the device's bound scope. [F63]
- `impl_sysmem_io_capable!` and any `IoCapable<T>` implementation for `SysMemBackend` must only be instantiated for types `T` where all bit patterns are valid, because `SysMemBackend::io_read` calls `view.ptr.read_volatile()` on shared hardware memory without a `FromBytes` bound on `T`. Broken by: implementing `IoCapable<T> for SysMemBackend` on `bool` or an enum/struct with invalid bit patterns. [F56]
- The `Io` trait must remain a blanket implementation (`impl<'a, T: IoBase<'a>> Io<'a> for T {}`) with non-overridable methods, because internal unsafe calls to `IoBackend::project_view`, `IoCopyable::copy_to_io`, and `IoCopyable::copy_from_io` rely on `Io`'s bounds, alignment, and slice-length checks. [F60]
- `IoCopyable::copy_read` and `IoCopyable::copy_write` must construct the projected `*mut [u8]` slice with byte length `size_of::<T>()` via `slice_from_raw_parts_mut::<u8>(ptr.cast(), size_of::<T>())` (never element count `1`), because `copy_from_io` and `copy_to_io` read the transfer byte count from the `[u8]` slice metadata. [F52]
- `IoSysMapBackend` must explicitly forward all four `IoCopyable` methods (`copy_from_io`, `copy_to_io`, `copy_read`, and `copy_write`) to `MmioBackend` and `SysMemBackend` rather than falling back to the default `IoCopyable::copy_read` and `IoCopyable::copy_write` trait implementations. [F31]
- In `io::resource::Region::drop`, `flags`, `start`, and `size` must be read from `self` before calling the release function (which frees the C `struct resource`), dispatching to `bindings::release_mem_region` when `Flags::IORESOURCE_MEM` is set or `bindings::release_region` otherwise. [F27]
- In `read_poll_timeout` and `read_poll_timeout_atomic`, the delay call (`fsleep` or `udelay`) must be skipped when `sleep_delta.is_zero()` or `delay_delta.is_zero()`, while `cpu_relax()` must be executed unconditionally at the end of every retry iteration. [F44]

## Synchronization

- In `irq::Registration::new` and `irq::ThreadedRegistration::new`, `handler`, `request`, and `_pin` must be initialized inside `try_pin_init!` before `bindings::request_irq` or `bindings::request_threaded_irq` is called with `this.as_ptr()`, because the IRQ callback can fire immediately and dereferences `&(*ptr).handler`. [F10]
- `Registration::synchronize`, `ThreadedRegistration::synchronize`, and `PinnedDrop::drop` for both types synchronously wait (`synchronize_irq` / `free_irq`) for in-flight IRQ handlers to finish, so they must not be called from atomic context or while holding any lock acquired by `Handler::handle`, `ThreadedHandler::handle`, or `ThreadedHandler::handle_threaded`. [F85]
- `irq::Handler::handle` and `irq::ThreadedHandler::handle` execute in hard interrupt context where sleeping is forbidden (no `Mutex::lock`, `read_poll_timeout`, or `GFP_KERNEL` allocations); sleeping work must be deferred to `irq::ThreadedHandler::handle_threaded`, which runs in process context on a `kthread` when `handle` returns `ThreadedIrqReturn::WakeThread`. [F82]
- `Io::update` and `Io::try_update` perform an unsynchronized read-modify-write (`io_read`, closure evaluation, then `io_write`) even though `Mmio` and `SysMem` are `Copy + Send + Sync`; callers must externally synchronize concurrent updates to the same register across CPUs or IRQ contexts. [F84]
- `RelaxedMmio` (`Mmio::relaxed` / `RelaxedMmioBackend`) uses `read*_relaxed` and `write*_relaxed` and provides no ordering guarantees with respect to DMA or normal memory accesses; callers requiring DMA/memory ordering must use `Mmio` (`MmioBackend`) or explicit barriers. [F13]
- `SysMemBackend` must perform all `IoCapable` and `IoCopyable` accesses via `read_volatile`, `write_volatile`, or `bindings::memcpy` (never `core::ptr::copy_nonoverlapping` or non-volatile dereferences) so concurrent accesses racing with external hardware remain well-defined. [F14]

## Object Lifetimes

- In `ExclusiveIoMem`, `ExclusiveIoMem::ioremap` must acquire the reservation via `Resource::request_region` before mapping via `IoMem::ioremap`, and `ExclusiveIoMem` must declare `iomem: IoMem` before `_region: Region` so Rust's drop order unmaps via `iounmap` before releasing the region. [F1]
- `PinnedDrop::drop` for `irq::Registration` and `irq::ThreadedRegistration` must call `bindings::free_irq` with `self.request.irq` and the pinned address of `self` (`core::ptr::from_mut::<Self>(self.get_unchecked_mut()).cast::<c_void>()`), matching the `this.as_ptr().cast::<c_void>()` cookie passed at registration, and callers must never `mem::forget` a registration so in-flight IRQ callbacks drain before `handler` and `Self` are deallocated. [F11, F23]
- `io::resource::Region` must retain ownership of `_name: CString` until after `bindings::release_mem_region` or `bindings::release_region` completes in `Region::drop`, because `bindings::__request_region` stores the raw pointer `name.as_char_ptr()` in the C `struct resource` without copying it. [F26]
- Unlike `MmioRaw::new` (which carries no mapping guarantees), `Mmio::from_raw` and `SysMem::new` require the underlying MMIO mapping or kernel virtual memory range `[ptr.addr()..ptr.addr() + KnownSize::size(ptr))` to be valid and aligned for the entire lifetime `'a`. [F62]
- Callers of `IoBackend::project_view` and `ProjectHelper::project_view` must ensure `ptr` is an in-bounds, properly aligned projection of `IoBackend::as_ptr(view)` (validated via `offset_valid`, `try_cast`, or `kernel::ptr::project!`), because backends wrap `ptr` directly into `Mmio<'a, U>` or `SysMem<'a, U>` without validation. [F61]

## API Contracts

- On `Io`, the fixed-width write helpers (`write8`, `write16`, `write32`, `write64` and `try_write8`, `try_write16`, `try_write32`, `try_write64`) take `(value, offset)`, whereas generic `Io::write` and `Io::try_write` take `(location, value)` with the offset or register location first. [F16]
- `IoCopyable::copy_from_io(view, buffer)` takes source `view` first and destination `buffer` second, so implementations calling `bindings::memcpy_fromio` or `bindings::memcpy` must reverse the arguments to pass destination `buffer` first and source `view.ptr` second (unlike `copy_to_io`, where `view` is the destination). [F17]
- Every `IoCopyable::copy_from_io` implementation must fully initialize `view.size()` bytes at `buffer`, because default `IoCopyable::copy_read` passes an uninitialized `MaybeUninit<T>` buffer of length `size_of::<T>()` to `copy_from_io` and immediately calls `buf.assume_init()`. [F57]
- `IoCopyable::copy_write` (and `Io::copy_write`) suppresses `value`'s destructor via `core::mem::forget(value)` after copying its bytes to I/O, so backend overrides must match this no-drop contract and callers must not pass values whose cleanup depends on `Drop`. [F8]
- Runtime I/O offset validation in `io_view` (via `offset_valid`) must use `offset.checked_add(size_of::<U>())` to verify `end <= KnownSize::size(ptr)` and check `ptr.addr().wrapping_add(offset) % align_of::<U>() == 0`, returning `Err(EINVAL)` on overflow, out-of-bounds, or misaligned offsets before calling `IoBackend::project_view`. [F36]
- In `RegisterArray`, `SIZE` is the element count and `STRIDE` is the byte distance between elements, so `RegisterArrayLoc::offset`, `RelativeRegisterArrayLoc::offset`, and `register!` array aliases must compute byte offsets as `OFFSET + idx * STRIDE` (not `size_of::<T::Storage>()`) and check `idx < SIZE`. [F51]
- Infallible I/O methods (`Io::read`, `Io::write`, `Io::update`, `read8/16/32/64`, `write8/16/32/64`) and `Array::at` / `RelativeRegisterLoc::at` use `build_assert!`, so callers with runtime-variable offsets/indices or `IoMem<'a, 0>` (from `IoRequest::iomap`) must use `try_read*`, `try_write*`, `try_update`, and `try_at`. [F78]
- `Io::copy_from_slice` and `Io::copy_to_slice` panic via `assert_eq!(self.len(), data.len())` if the `[u8]` I/O view length does not equal `data.len()`; callers copying a subrange must first project the `[u8]` view to the exact slice length (e.g., with `io_project!`). [F38]
- `io::poll::read_poll_timeout` unconditionally invokes `might_sleep()` before polling and sleeps via `fsleep`, so it must only be called from non-atomic contexts; atomic contexts must use `io::poll::read_poll_timeout_atomic`. [F83]
- `io::resource::Flags::contains(self, flags)` must check `(self & flags) == flags` so every bit in `flags` is required to be present in `self`, rather than checking `(self & flags).0 != 0`. [F45]

## Error Handling

- `Resource::request_region` must check the `*mut bindings::resource` returned by `bindings::__request_region` for null via `NonNull::new(region)?` (not `ERR_PTR`), returning `None` on conflict which `ExclusiveIoMem::ioremap` maps to `Err(EBUSY)`. [F19]
- `Resource::name` must check `(*inner).name.is_null()` and return `None` before calling `CStr::from_char_ptr`, because `struct resource::name` in C can be null. [F20]
- `read_poll_timeout` and `read_poll_timeout_atomic` must propagate any `Err` returned by `op()` immediately without overwriting it with `ETIMEDOUT`, returning `Err(ETIMEDOUT)` only when `cond(&val)` is `false` and `start.elapsed() > timeout_delta` or `retry` attempts are exhausted. [F25]

## External Contracts

- `IoMem::ioremap` must convert `resource.size()` from `ResourceSize` (`u64` on 32-bit with `CONFIG_PHYS_ADDR_T_64BIT`) to `usize` via `try_into()?`, reject `size == 0` with `Err(EINVAL)` before calling C `ioremap_np` or `ioremap`, dispatch to `bindings::ioremap_np` when `resource.flags().contains(Flags::IORESOURCE_MEM_NONPOSTED)` and `bindings::ioremap` otherwise, and validate the region via `MmioRaw::new_region`. [F39, F46]
- In `rust/kernel/io.rs` and `rust/kernel/io/mem.rs`, any module or function invoking `bindings::ioremap`, `bindings::ioremap_np`, or `bindings::iounmap` must be gated with `#[cfg(CONFIG_HAS_IOMEM)]` for architectures like s390 that omit those C primitives when `CONFIG_HAS_IOMEM` is disabled. [F90]

## Known Pitfalls

- On `Mmio` views, `Io::copy_read` and `Io::copy_write` copy raw bytes via `memcpy_fromio` and `memcpy_toio` without endianness conversion or single-transaction atomicity; use `Io::read_val` / `Io::write_val` (or `read*`/`write*`) for multi-byte MMIO registers requiring width-matched atomic transactions (`readb`/`readw`/`readl`/`readq`, `writeb`/`writew`/`writel`/`writeq`) and bus-to-CPU byte-swapping. [F37]
- In `io_view_assert` and `io_view`, `ptr` is cast to `*mut U` before applying the offset, so `wrapping_byte_add(offset)` must be used rather than `wrapping_add(offset)` to advance by `offset` bytes instead of scaling by `size_of::<U>()`. [F50]
- Unlike `read_poll_timeout` (which takes `timeout_delta: Delta` and always executes `op` at least once), `read_poll_timeout_atomic` bounds polling by an attempt count (`retry: usize` in `0..retry`) and returns `Err(ETIMEDOUT)` without calling `op` if `retry == 0`. [F80]

## Other

- `ProjectHelper::as_ptr`, `ProjectHelper::project_view`, and `build_assert!` callers (`io_view_assert`, `Io::read`/`write`/`update`, `RegisterArrayLoc::new`, `Flags::new`) must remain `#[inline(always)]` so helper symbols are not emitted and `build_assert!` error paths are optimized out. [F89]
