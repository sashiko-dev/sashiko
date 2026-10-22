# DMA MAPPING & SCATTERLIST API [RUST]

This unit provides safe Rust abstractions over the Linux kernel's Direct Memory Access (DMA) mapping (`include/linux/dma-mapping.h`) and scatter-gather list (`include/linux/scatterlist.h`) C APIs, along with thin C helper wrappers for inline kernel functions. It enables Rust device drivers to configure DMA masks, allocate and access coherent DMA buffers (`CoherentAllocation`), and construct or iterate over borrowed and owned DMA-mapped scatter-gather tables (`SGTable`).

## Architecture & Data Flow

- A `struct sg_table` managed by `DmaMappedSgt` must never be manually unmapped with `dma_unmap_sgtable` outside `DmaMappedSgt::drop`, which unconditionally calls `bindings::dma_unmap_sgtable` when `Devres<DmaMappedSgt>` is dropped or revoked on device unbind. Broken by: calling `dma_unmap_sgtable` on `SGTable<Owned<P>>` or `RawSGTable` without disarming `DmaMappedSgt`. [F3]
- `CoherentAllocation<T>` (`dev`, `count`, `cpu_addr`, `dma_handle`, `dma_attrs`) and `DmaMappedSgt` (`dev`, `sgt`, `dir`, `attrs`) must retain their `ARef<device::Device>` and immutable parameters for their entire lifetimes so `CoherentAllocation::drop` (`dma_free_attrs`) and `DmaMappedSgt::drop` (`dma_unmap_sgtable`) pass the exact matching arguments and byte size (`self.count * size_of::<T>()`). Broken by: mutating `count`, `cpu_addr`, `dma_handle`, `dma_attrs`, or `dir` in place (e.g., for sub-slicing) or dropping `ARef<Device>` early. [F6, F25, F27]
- `CoherentAllocation<T>` must bound `T: AsBytes + FromBytes`, and `CoherentAllocation::alloc_attrs` must enforce `build_assert!(core::mem::size_of::<T>() > 0)` and validate `count.checked_mul(core::mem::size_of::<T>())`, ensuring non-ZST types with valid bit patterns/no uninitialized padding and preventing `size_of::<T>() * self.count` overflow in `size`, `dma_handle_with_offset`, `validate_range`, and `drop`. [F56, F60]
- `RawSGTable`, `SGTable<Borrowed>`, `Borrowed`, and `SGEntry` must remain `#[repr(transparent)]` wrappers around `Opaque<bindings::sg_table>` or `Opaque<bindings::scatterlist>` because `Owned::new` casts `&raw mut (*this.as_ptr()).sgt` directly to `*mut bindings::sg_table`, and `SGTable::from_raw`/`SGEntry::from_raw` cast raw C pointers directly to `&SGTable`/`&SGEntry`. [F30, F31]
- Every `DataDirection` (`#[repr(u32)]`) variant must assign its discriminant via `DataDirection::const_cast` from the corresponding `bindings::dma_data_direction_*` constant so `From<DataDirection> for bindings::dma_data_direction` (`direction as u32 as Self`) yields the exact C enum value. [F33]

## Synchronization

- `CoherentAllocation::field_read` and `CoherentAllocation::field_write` (used by `dma_read!`/`dma_write!`) perform `read_volatile`/`write_volatile` via `&self` only to avoid UB when racing with hardware or non-kernel code; they do not provide atomicity or prevent data-race UB between concurrent kernel threads. Broken by: implementing `Sync` for `CoherentAllocation<T>` or concurrently calling `dma_write!`/`field_write` and `dma_read!` on overlapping fields from multiple kernel threads without synchronization. [F13]
- `dma_read!` and `dma_write!` must obtain a raw element pointer via `CoherentAllocation::item_from_index`, project fields with `addr_of!` or `addr_of_mut!` without creating intermediate `&T` or `&mut T` references, and access fields via `field_read` (`F: FromBytes`, `read_volatile`) or `field_write` (`F: AsBytes`, `write_volatile`). [F61, F71]

## Object Lifetimes

- In `struct scatterlist::Owned<P>`, fields must be declared in drop order (`dma: Devres<DmaMappedSgt>`, `sgt: RawSGTable`, `_pages: P`) so dropping `Owned<P>` unmaps the table (and waits for any concurrent device-unbind `Devres` callback) before freeing `sgt` and `_pages`, whereas `Owned::new`'s `try_pin_init!` must initialize `sgt` before `dma` so `DmaMappedSgt::new` and `Devres::new` receive an allocated pinned `RawSGTable`. Broken by: reordering `Owned<P>` fields or matching `try_pin_init!` order to the struct. [F8, F9, F66]

## API Contracts

- Before dropping a `CoherentAllocation<T>`, the caller must ensure the hardware device has stopped performing DMA on that region, because `CoherentAllocation::drop` immediately calls `bindings::dma_free_attrs` without synchronizing with device activity. Broken by: dropping `CoherentAllocation` (including on early-return error paths) while hardware DMA is active. [F5]
- Callers of `dma::Device::dma_set_mask`, `dma_set_coherent_mask`, and `dma_set_mask_and_coherent` must guarantee that no DMA allocation or mapping primitives (such as `CoherentAllocation::alloc_attrs` or `SGTable::new`) run concurrently on the same device. [F11]
- Callers of `CoherentAllocation::as_slice`, `as_slice_mut`, and `write` must ensure no hardware DMA access and no conflicting CPU access races with the call for the duration of the slice or write; moreover, `as_slice_mut`, `write`, and `start_ptr_mut` must take `&mut self` (not `&self`) so the borrow checker enforces exclusive CPU access. [F12, F62]
- Callers of `SGTable::from_raw` must guarantee the `struct sg_table` behind `ptr` is not modified concurrently for the lifetime `'a` of `&'a SGTable`, and `SGTable<Owned<P>>` upholds this in `Deref::deref` by never mutating `self.inner.sgt` after `Owned::new`. Broken by: mutating `struct sg_table` through `&self` on `SGTable<Owned<P>>` or `RawSGTable`. [F14]
- `Owned::new` must forward the caller-supplied `flags: alloc::Flags` to every fallible allocation (`KVec::with_capacity`, `page_vec.push`, and `RawSGTable::new` into `bindings::sg_alloc_table_from_pages_segment`). Broken by: hardcoding `GFP_KERNEL`. [F87]
- `Attrs::contains(self, flags)` must check `(self & flags) == flags` rather than `(self & flags).0 != 0` so multi-bit `Attrs` masks require all specified bits to be set. [F46]

## Error Handling

- `CoherentAllocation::alloc_attrs` must initialize the `dma_handle` out-parameter to `0` before calling `bindings::dma_alloc_attrs` and return `Err(ENOMEM)` when the returned CPU pointer `ret.is_null()`. [F16]
- `CoherentAllocation::alloc_attrs` must check `count.checked_mul(core::mem::size_of::<T>()).ok_or(EOVERFLOW)?`, and range accessors (`as_slice`, `as_slice_mut`, `write`) must validate `offset` and `count` via `validate_range(offset, count)`—checking `offset.checked_add(count).ok_or(EOVERFLOW)?` before returning `Err(EINVAL)` when the end index exceeds `self.count`—before calling `self.cpu_addr.add(offset)`. [F18, F35]
- `SGTable::iter` and `SGTableIter::next` must bound traversal by the DMA-mapped entry count `(*self.as_raw()).nents` (not `orig_nents`): `iter` only reads `sgl` and wraps it with `SGEntry::from_raw` when `nents > 0` (setting `pos` to `None` when `nents == 0`), and `next` decrements `self.nents` via `saturating_sub(1)` and only calls `SGEntry::from_raw(next)` when both `!next.is_null()` (from `bindings::sg_next`) and `self.nents > 0` hold. [F21, F22, F39]

## External Contracts

- `Owned::new` must map a `0` or `> u32::MAX` return from `bindings::dma_max_mapping_size` (`size_t`) to `u32::MAX` (via `u32::try_from(max_segment).unwrap_or(u32::MAX)`) before calling `RawSGTable::new`, and `RawSGTable::new` must reject an empty `pages` slice with `Err(EINVAL)`, convert `pages.len()` via `try_into()?`, and zero-initialize `sgt` via `Opaque::zeroed()` before calling `bindings::sg_alloc_table_from_pages_segment`. [F23, F38, F50]

## Known Pitfalls

- Because `SGTable<Owned<P>>` manages its `DmaMappedSgt` inside `Devres`, the DMA mapping is automatically unmapped upon device unbind even while `SGTable<Owned<P>>` or an `SGTableIter` borrowed from it is still alive. Broken by: assuming a live `SGTable<Owned<P>>` or `SGTableIter` keeps `SGEntry` DMA addresses mapped after `Device` unbind. [F4]
- All `offset` and `count` arguments on `CoherentAllocation<T>` are in units of `T`: `self.cpu_addr` (`*mut T`) scales `.add(offset)` by `size_of::<T>()` automatically, whereas `self.dma_handle` (`DmaAddress`) is byte-addressed so `dma_handle_with_offset` must multiply `offset * core::mem::size_of::<T>()` and widen to `DmaAddress` before adding to `self.dma_handle` (since `dma_addr_t` can be `u64` on 32-bit architectures). Broken by: casting `self.dma_handle as usize` or mixing byte and element offsets. [F52, F55]
- In `DmaMask::try_new`, `n` must match `0 => 0` separately from `1..=64 => u64::MAX >> (64 - n)` and reject `n > 64` before subtracting, because `n == 0` shifts `u64` by 64 and `n > 64` underflows `64 - n`. [F48]
- In the `dma_write!` macro definition, the whole-element arm `($dma:expr, $idx: expr, = $val:expr)` must precede the field-projection arm `($dma:expr, $idx: expr, $(.$field:ident)* = $val:expr)`. [F47]
