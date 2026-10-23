# RUST [PIN-INIT]

Provides the `pin-init` crate, its proc-macro helper crate (`pin-init-internal`), and kernel wrapper extensions (`rust/kernel/init.rs`) for safe, fallible, in-place and pinned initialization of Rust structs. It solves the safe pinned initialization problem for self-referential C types (like kernel locks and list heads) and large structs that cannot be constructed on the stack before moving to heap or pinned locations.

## Architecture & Data Flow
- `Zeroable` must never be implemented for uninhabited types (such as `!` or `core::convert::Infallible`) or for raw pointers `*mut T` and `*const T` with `T: ?Sized` whose fat-pointer vtable metadata cannot be null. [F21]
- `ZeroableOption` implementations for reference and smart-pointer types (`&T`, `&mut T`, `NonNull<T>`, and `Box<T>`) must require `T: Sized` rather than `T: ?Sized`, because Rust's `Option` layout optimization only guarantees all-zero bytes represent `None` when the pointee is `Sized`. [F52]
- `init_array_from_fn` and `pin_init_array_from_fn` must cast `slot: *mut [T; N]` to `*mut T` before indexing with `slot.add(i)` (to scale by element stride rather than array stride) and, on failure at element `i`, drop only the initialized prefix `0..i` via `ptr::slice_from_raw_parts_mut(slot, i)`. [F47]
- In `InPlaceInit<T> for Arc<T>`, `try_pin_init` and `try_init` must obtain the raw pointer via `Arc::get_mut(&mut this)` while the newly allocated `Arc<MaybeUninit<T>>` is still unique, and only call `this.assume_init()` after `init.__pinned_init(slot)?` or `init.__init(slot)?` returns `Ok(())`. [F13]
- In `StaticInit::deref`, `(*self.cell.get()).assume_init_ref()` must only be called when `self.present.get()` is `true` (checked on entry or rechecked after `self.lock.acquire()`), or after `self.init.take()` completes `f.__pinned_init(ptr).unwrap()` and sets `self.present.set(true)`. [F23]
- `pin_data` must recursively replace every `Self` token in `impl_generics` and `rest` with `struct_name` via `replace_self_and_deny_type_defs` before invoking `__pin_data!`, because `__pin_data!` expands those tokens inside helper types where `Self` refers to the helper type. [F43]
- In `__pin_data!(find_pinned_fields: ...)`, the unpinned `PhantomPinned` arm must precede `@fields_munch($field:ident : $type:ty, ...)` (so unpinned `PhantomPinned` triggers `compile_error!`), and `#[pin]` must precede `#[$($attr:tt)*]` (so `#[pin]` sets `@is_pinned(yes)` rather than entering `@accum`). [F35]
- Every field-completing arm in `__pin_data!(find_pinned_fields: ...)` must reset `@accum()` and `@is_pinned()` to empty when recursing on `$($rest)*`, so per-field state never leaks into the next field or blocks the `@fields_munch($(,)?)` termination arm. [F37]
- In `parse_generics`, `at_start` and `skip_until_comma` must only be reset on a top-level comma (`nesting == 1`), and the `'` arm (when `at_start`) must leave `at_start` true so the following lifetime identifier token is also pushed to `ty_generics`. [F40]
- In `parse_zeroable_derive_input`, `in_generic` and `inserted` must only be updated at `nested == 0`—setting `in_generic = false` on `'`, setting `inserted = true` when inserting `Zeroable` after `:`, and resetting `in_generic = true` and `inserted = false` on `,`—with a post-loop `if in_generic && !inserted` bound insertion for the final parameter. [F41]
- When a `ListHead` that is not self-linked (`!ptr::eq(self.next.as_ptr(), &*self)`) is dropped in `<ListHead as PinnedDrop>::drop`, both `next.prev.set(&self.prev)` and `prev.next.set(&self.next)` must be updated to splice `self` out of the circular list. [F26]
- In `rust/pin-init/examples/mutex.rs`, `CMutexGuard::drop` casts `NonNull<ListHead>` from `self.mtx.wait_list.next()` directly to `*const WaitEntry`, requiring `WaitEntry` to be `#[repr(C)]` with `wait_list: ListHead` as its first field and every node in `CMutex::wait_list` to be a `WaitEntry`. [F49]

## Synchronization
- `__internal::Invariant<T>` must use `PhantomData<fn(*mut T) -> *mut T>` rather than `PhantomData<*mut T>`, because `fn(*mut T) -> *mut T` unconditionally implements `Send` and `Sync` whereas `*mut T` strips `Send` and `Sync` from `InitClosure` and `AllData`. [F64]
- `SpinLock::acquire` must acquire `self.inner` with `compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)` paired with `self.0.inner.store(false, Ordering::Release)` in `SpinLockGuard::drop`. [F11]
- In `CMutex::lock` and `CMutexGuard::drop`, `self.spin_lock` must be held whenever reading or writing `self.locked` or inserting, unlinking (`drop(wait_entry)`), or traversing `self.wait_list`, and `CMutex::lock` must drop `sguard` before `thread::park()` and reacquire it before rechecking `self.locked`. [F65]

## Object Lifetimes
- `StackInit::init` must set `this.is_init = false` and drop the existing value via `this.value.assume_init_drop()` before calling `init.__pinned_init(this.value.as_mut_ptr())`, and set `this.is_init = true` only after `__pinned_init` succeeds, preventing unpinned overwrites and double-drops on failed re-initialization. [F57]

## API Contracts
- Implementations of `PinInit::__pinned_init` and `Init::__init` (including `pin_init_from_closure` and `init_from_closure` closures) must initialize every field of `slot` before returning `Ok(())`, and on `Err` must clean up any partially initialized fields or C resources in `slot` so callers can deallocate `slot` without dropping it. [F14]
- Every implementation of `Init<T, E>` must implement its supertrait method `PinInit::__pinned_init` to perform the exact same initialization as `Init::__init`, because `ChainInit::__init` and `kernel::init::InPlaceInit::init` invoke `__pinned_init` when initializing unpinned values. [F55]
- `cast_pin_init<T, U, E>` and `cast_init<T, U, E>` cast the destination `*mut U` slot directly via `ptr.cast::<T>()`, requiring that `*mut U` is castable to `*mut T` and that any `T` written at offset 0 satisfies `U`'s layout, alignment, and bit-validity invariants. [F46]
- `OnlyCallFromDrop::new` must only be called from a type's `Drop::drop` implementation to delegate destruction to that same type's `PinnedDrop::drop`; constructing it elsewhere allows safe code to invoke a destructor on a live pinned value. [F58]
- In `rust/kernel/init.rs`, `InPlaceInit::pin_init` and `InPlaceInit::init` must forward the caller-supplied `flags: Flags` argument unchanged to `Self::try_pin_init` and `Self::try_init`. [F63]

## Error Handling
- `ChainPinInit::__pinned_init` and `ChainInit::__init` must call `core::ptr::drop_in_place(slot)` when their chained post-initialization closure returns `Err`, because `self.0` has already completed and transferred ownership of `T` into `slot`. [F3]
- In `PThreadMutex::new` (`init_raw`), once `libc::pthread_mutexattr_init(attr)` returns `0`, `libc::pthread_mutexattr_destroy(attr)` must be called before returning on both the `pthread_mutexattr_settype` failure path and after `pthread_mutex_init` before checking its return code `ret`. [F22]

## External Contracts
- `impl_fn_zeroable_option!` implements `ZeroableOption` (and thus `Zeroable` for `Option<F>`) only for safe and `unsafe` function pointers with `"Rust"` or `"C"` ABI and at most 20 arguments. [F66]

## Known Pitfalls
- Inside `pin_init!`, `try_pin_init!`, `init!`, `try_init!`, and `pin_init_from_closure`, `unsafe` code accessing `slot` or the `&this in` `NonNull<Self>` pointer must use raw pointer projections (such as `core::ptr::addr_of_mut!`) and never create `&Self` or `&mut Self` while any field remains uninitialized. [F62]
- In `__init_internal!`, when `..Zeroable::init_zeroed()` is present (`@init_zeroed(())`), `::core::ptr::write_bytes(slot, 0, 1)` must execute before `init_slot` runs any field initializers or user code blocks so it does not clobber explicitly initialized fields. [F18]
- In `__init_internal!`, the closure passed to `make_closure` must declare an outer `struct __InitOk;` as its `Ok` return type and shadow it with an inner `struct __InitOk;` inside the block where field expressions and `_: { ... }` blocks expand, returning `Ok(__InitOk)` only after that inner block exits so user code cannot `return Ok(...)` early. [F20]
- In `__pin_data!(find_pinned_fields: ...)`, the visibility-munching arm `@fields_munch($fvis:vis $field:ident $($rest:tt)*)` must remain ordered after all `@fields_munch($field:ident : ...)` field-declaration arms because `:vis` matches an empty token sequence and would otherwise infinitely re-match `$field:ident`. [F36]
- In `macro_rules!` definitions (`try_init!`, `try_pin_init!`, `init!`, `pin_init!`), optional matcher repetitions that bind no metavariables (such as trailing-comma `$(,)?` inside `$(::<$($generics:ty),* $(,)?>)?`) must be omitted from the transcriber block (`$(::<$($generics),*>)?`). [F67]
- In `parse_generics`, when encountering `>` while `nesting == 0`, the parser must break out of the token loop before decrementing `nesting` so `rest.extend(toks)` preserves remaining tokens for downstream macro diagnostics. [F34]
- In `pinned_drop`, the `'>'` match arm must `continue` after decrementing `nesting` so the closing `'>'` of `impl<...>` is not checked by `if i >= 1 && nesting == 0`, and the matched `PinnedDrop` token at `idx` must be spliced with `::pin_init::` to prevent trait shadowing. [F42]
- In `ListHead::insert_next`, `prev` (`list.next.prev().replace(...)`) must be initialized before `next` (`list.next.replace(...)`), and in `ListHead::insert_prev`, `next` (`list.prev.next().replace(...)`) must be initialized before `prev` (`list.prev.replace(...)`). [F25]
- Because `ListHead` forms a circular doubly-linked list where `ListHead::next` only returns `None` when a node points to itself (`ptr::eq(self.next.as_ptr(), self)`), any multi-node traversal using `next()` must explicitly test pointer equality against the starting `ListHead` to terminate. [F44]
