# DEVICE I/O & IRQ [RUST]

This unit provides safe Rust abstractions over the Linux kernel's memory-mapped I/O (MMIO), system resource management (`struct resource`), I/O polling, and interrupt request (`request_irq` / `request_threaded_irq`) subsystems. It enforces compile-time or runtime bounds and alignment checks on 8/16/32/64-bit MMIO reads and writes (`Io`, `IoMem`, `ExclusiveIoMem`), manages `ioremap`/`__request_region` and `free_irq` lifecycles via `Drop`, `PinnedDrop`, and `Devres`, and bridges pinned Rust handler traits (`Handler`, `ThreadedHandler`) to C interrupt callbacks.

## Architecture & Data Flow

- `RegistrationInner` must be wrapped in a `Devres` bound to `request.dev` so that device unbind revokes `RegistrationInner` and runs `bindings::free_irq` before unbind completes, guaranteeing that `registration.inner.device().as_bound()` is valid inside `handle_irq_callback`, `handle_threaded_irq_callback`, and `thread_fn_callback`. [F10]
- `Registration<T>` and `ThreadedRegistration<T>` must remain `!Unpin` (via `#[pin] _pin: PhantomPinned`), and `RegistrationInner::cookie` and `RegistrationInner::irq` must store the exact `this.as_ptr().cast::<c_void>()` and `request.irq` passed to `bindings::request_irq` / `bindings::request_threaded_irq`. Broken by: removing `PhantomPinned` or passing `&this.handler` as the C cookie while callbacks cast back to the registration struct and `RegistrationInner::drop` frees `self.cookie`. [F11, F21]
- Every `Io` accessor must validate offsets through `io_addr` or `io_addr_assert`: `Io::offset_valid` and `Io::io_addr` must require natural alignment (`offset % core::mem::size_of::<U>() == 0`) and use `checked_add` for both `offset + core::mem::size_of::<U>()` (against `size`) and `self.addr() + offset`, returning `false` or `Err(EINVAL)` on overflow. [F27, F40]
- `Io::io_addr_assert` validates compile-time MMIO offsets against the const generic `SIZE` rather than `self.maxsize()`, which relies on `IoRaw::<SIZE>::new` rejecting `maxsize < SIZE` with `EINVAL` at construction time. [F28]
- `Io<SIZE>` must remain `#[repr(transparent)]` over `IoRaw<SIZE>` and `Resource` must remain `#[repr(transparent)]` over `Opaque<bindings::resource>`, because `Io::from_raw` and `Resource::from_raw` convert raw pointers directly to references via pointer casts. [F30, F31]

## Synchronization

- `Handler::handle` and `ThreadedHandler::handle` execute in hard interrupt context (via `handle_irq_callback` and `handle_threaded_irq_callback`), so any sleeping or blocking work (such as `Mutex::lock`, `read_poll_timeout`, or blocking allocations) must be deferred to `ThreadedHandler::handle_threaded`, which `thread_fn_callback` runs in a dedicated `kthread`. [F59]

## Object Lifetimes

- In `Registration<T>` and `ThreadedRegistration<T>`, `try_pin_init!` in `new` must initialize `handler <- handler` before `inner <- Devres::new(...)` so `handler` is initialized before `request_irq` / `request_threaded_irq` publishes `this.as_ptr()` to concurrent IRQ callbacks; conversely, the struct definitions must declare `inner: Devres<RegistrationInner>` before `handler: T` so `RegistrationInner::drop` completes `bindings::free_irq` before `handler` is dropped. [F8, F9, F22]
- `ExclusiveIoMem::ioremap` must acquire `_region` via `Resource::request_region` with `Flags::IORESOURCE_MEM` (returning `Err(EBUSY)` on `None`) before calling `IoMem::ioremap`, and `struct ExclusiveIoMem` must declare `iomem: IoMem<SIZE>` before `_region: Region` so `IoMem::drop` (`bindings::iounmap`) unmaps the MMIO range before `Region::drop` calls `bindings::release_mem_region`. [F12, F18]
- `Region` must retain ownership of `_name: CString` until `Region::drop` releases the region, because `bindings::__request_region` stores the raw `as_char_ptr()` pointer in `struct resource::name` and `Resource::name` dereferences it via `CStr::from_char_ptr`. [F48]

## API Contracts

- Callers of `Io::from_raw` must guarantee that `raw.addr()` points to a valid I/O-mapped memory region of at least `raw.maxsize()` bytes, because `IoRaw::new` only checks `maxsize >= SIZE` and `Io` accessors pass `addr + offset` directly to C MMIO primitives. [F49]
- Callers of `Resource::from_raw` must ensure the `bindings::resource` pointer remains valid and is not concurrently mutated by C for the lifetime `'a` of the returned `&'a Resource`, because `Resource::size`, `start`, `name`, and `flags` read the C struct fields without synchronization. [F13]
- `IoMem::ioremap` must convert `resource.size()` (`ResourceSize` / `bindings::phys_addr_t`, which can be `u64` on 32-bit builds with `CONFIG_PHYS_ADDR_T_64BIT`) to `usize` via `try_into()?` and reject `size == 0` with `EINVAL` before calling `bindings::ioremap` or `bindings::ioremap_np`. [F29]
- `read_poll_timeout` may only be called in non-atomic context and must unconditionally invoke `might_sleep()` before entering the poll loop (even if `cond` is immediately true or `sleep_delta` is zero) and `cpu_relax()` on every retry; each iteration must evaluate `op()?` and return `Ok(val)` when `cond(&val)` holds before checking `start.elapsed() > timeout_delta`, and on timeout must return `Err(ETIMEDOUT)` immediately without re-invoking `op`. [F37, F60]
- `Io` accessors (`read*`, `try_read*`, `write*`, `try_write*`) take `offset` in bytes and require both `offset + size_of::<U>() <= maxsize` (or `SIZE`) and `offset % size_of::<U>() == 0`; `write*` and `try_write*` take `(value, offset)` in that order. [F19]
- In `Io`, all 64-bit MMIO accessors (`read64`, `try_read64`, `read64_relaxed`, `try_read64_relaxed`, `write64`, `try_write64`, `write64_relaxed`, `try_write64_relaxed`) must be gated with `#[cfg(CONFIG_64BIT)]` because the C `readq*` and `writeq*` primitives are only provided on 64-bit builds. [F67]
- `ThreadedHandler::handle_threaded` is invoked by the IRQ `kthread` if and only if the hardirq callback `ThreadedHandler::handle` returns `ThreadedIrqReturn::WakeThread` (its default). Broken by: returning `ThreadedIrqReturn::Handled` from a custom `ThreadedHandler::handle` while expecting `handle_threaded` to run. [F75]
- `io::resource::Flags::contains` checks `(self & flags) == flags`, requiring every bit in `flags` to be set in `self` rather than any overlapping bit. [F34]

## Error Handling

- `IoMem::ioremap` must check `resource.flags().contains(io::resource::Flags::IORESOURCE_MEM_NONPOSTED)` to select `bindings::ioremap_np` over `bindings::ioremap`, and must check `addr.is_null()` (not `from_err_ptr`) to return `Err(ENOMEM)` on failure. [F16]
- `Registration::try_synchronize` and `ThreadedRegistration::try_synchronize` must acquire the inner `RegistrationInner` via `self.inner.try_access().ok_or(ENODEV)?` (or `self.inner.access(dev)?` in `synchronize`) before calling `RegistrationInner::synchronize` (`bindings::synchronize_irq`). [F20]
- `Resource::name` must check `name.is_null()` and return `None` before calling `CStr::from_char_ptr`, and callers such as `ExclusiveIoMem::ioremap` must handle `None` via a fallback (`unwrap_or(c_str!(""))`) rather than assuming every `Resource` has a non-null name. [F17]

## External Contracts

- `Region::drop` must read `flags`, `start`, and `size` from `self.resource` before releasing the C resource and must call `bindings::release_mem_region` when `Flags::IORESOURCE_MEM` is set or `bindings::release_region` otherwise. Broken by: calling `release_region` on an `IORESOURCE_MEM` region, or reading `self.resource` after the release call frees the C `struct resource`. [F3]
- `IrqReturn` and `ThreadedIrqReturn` must remain `#[repr(u32)]` enums whose discriminants equal the `bindings::irqreturn_*` constants because `handle_irq_callback`, `handle_threaded_irq_callback`, and `thread_fn_callback` cast their return values directly to `c_uint` for the C IRQ core. [F32]
