# DMA MAPPING & SCATTERLIST API [RUST]

This unit provides safe Rust abstractions over the Linux kernel's Direct Memory Access (DMA) mapping (`include/linux/dma-mapping.h`) and scatter-gather list (`include/linux/scatterlist.h`) C APIs, along with thin C helper wrappers for inline kernel functions. It enables Rust device drivers to configure DMA masks, allocate and access coherent DMA buffers (`CoherentAllocation`), and construct or iterate over borrowed and owned DMA-mapped scatter-gather tables (`SGTable`).

## Architecture & Data Flow

- `RawSGTable` must remain a `#[repr(transparent)]` wrapper around `Opaque<bindings::sg_table>` because `Owned::new` casts `&raw mut (*this.as_ptr()).sgt` (`*mut RawSGTable`) directly to `*mut bindings::sg_table` when constructing `DmaMappedSgt`. Broken by: adding fields or removing `#[repr(transparent)]` without updating `Owned::new` to use `RawSGTable::as_raw`. [F30]
- `SGTable<Borrowed>`, `Borrowed`, and `SGEntry` must remain `#[repr(transparent)]` wrappers around `Opaque<bindings::sg_table>` and `Opaque<bindings::scatterlist>` respectively, because `SGTable::from_raw` and `SGEntry::from_raw` cast raw C struct pointers directly to `&SGTable` and `&SGEntry`. [F31]
- A `struct sg_table` managed by `DmaMappedSgt` must never be manually unmapped with `dma_unmap_sgtable` outside `DmaMappedSgt::drop`, which unconditionally calls `bindings::dma_unmap_sgtable` when `Devres<DmaMappedSgt>` is dropped or revoked on device unbind. Broken by: calling `dma_unmap_sgtable` on `SGTable<Owned<P>>` or `RawSGTable` without disarming `DmaMappedSgt`. [F3]
- Every variant of `DataDirection` (`#[repr(u32)]`) must assign its discriminant via `DataDirection::const_cast` from the corresponding `bindings::dma_data_direction_*` constant so `From<DataDirection> for bindings::dma_data_direction` (`direction as u32 as Self`) produces the exact C enum value. [F33]

## Object Lifetimes

- In `struct scatterlist::Owned<P>`, fields must be declared in drop order (`dma: Devres<DmaMappedSgt>`, `sgt: RawSGTable`, `_pages: P`) so dropping `Owned<P>` unmaps the table (and waits for any concurrent device-unbind `Devres` callback) before freeing `sgt` and `_pages`, whereas `Owned::new` must initialize `sgt` before `dma` inside `try_pin_init!` so `DmaMappedSgt::new` and `Devres::new` receive an already-initialized `sgt` pointer. [F8, F9, F66]
- `CoherentAllocation<T>` and `DmaMappedSgt` must retain `ARef<device::Device>` (`self.dev`) alongside their allocation or mapping parameters (`count`, `dma_handle`, `dma_attrs`, `dir`) without mutating them, and `DmaMappedSgt::new` must check `bindings::dma_map_sgtable` with `error::to_result`, so `Drop` passes a valid device and identical parameters (`dev`, `sgt`, `dir.into()`, `attrs` `0`) to `dma_free_attrs` or `dma_unmap_sgtable`. [F6, F25]

## API Contracts

- Callers of `SGTable::from_raw` must guarantee the `struct sg_table` behind `ptr` is not modified concurrently for the lifetime `'a` of `&'a SGTable`, and `SGTable<Owned<P>>` upholds this in `Deref::deref` by never mutating `self.inner.sgt` after `Owned::new`. Broken by: mutating the underlying `sg_table` through `&self` on `SGTable<Owned<P>>` or `RawSGTable`. [F14]
- `Owned::new` must forward the caller-supplied `flags: alloc::Flags` to every fallible allocation it performs (`KVec::with_capacity`, `page_vec.push`, and `RawSGTable::new` into `bindings::sg_alloc_table_from_pages_segment`). Broken by: hardcoding `GFP_KERNEL`. [F87]
- `Attrs::contains(self, flags)` must compare `(self & flags) == flags` rather than `(self & flags).0 != 0` so multi-bit `Attrs` masks require all specified bits to be set. [F46]

## Error Handling

- `SGTable::iter` and `SGTableIter::next` must bound iteration by the DMA-mapped entry count `(*self.as_raw()).nents` (not `orig_nents`): `iter` must only read `sgl` and call `SGEntry::from_raw` when `nents > 0` (setting `pos` to `None` when `nents == 0`), and `next` must decrement `self.nents` via `saturating_sub(1)` and only call `SGEntry::from_raw(next)` when both `!next.is_null()` (from `bindings::sg_next`) and `self.nents > 0` hold. [F21, F22, F39]

## External Contracts

- `RawSGTable::new` must reject an empty `pages` slice with `Err(EINVAL)`, zero-initialize `sgt` via `Opaque::zeroed()`, and convert `pages.len()` via `try_into()?` before calling `bindings::sg_alloc_table_from_pages_segment`; in `Owned::new`, the `size_t` from `bindings::dma_max_mapping_size` must map `0` to `u32::MAX` and saturate larger values via `u32::try_from(max_segment).unwrap_or(u32::MAX)` instead of casting with `as u32`. [F23, F38, F50]

## Known Pitfalls

- Because `SGTable<Owned<P>>` manages its `DmaMappedSgt` inside `Devres`, the DMA mapping is automatically unmapped on device unbind even while `SGTable<Owned<P>>` or an `SGTableIter` borrowed from it is still alive. Broken by: assuming holding `SGTable<Owned<P>>` or `SGTableIter` keeps `SGEntry` DMA addresses mapped after device unbind. [F4]
- In `CoherentAllocation::dma_handle_with_offset`, the validated byte offset `offset * core::mem::size_of::<T>()` must be widened to `DmaAddress` before adding to `self.dma_handle`, because `DmaAddress` (`dma_addr_t`) can be `u64` on 32-bit `CONFIG_ARCH_DMA_ADDR_T_64BIT` kernels. Broken by: casting `self.dma_handle as usize` before addition. [F52]
- In `DmaMask::try_new`, `n` must match `0 => 0` separately from `1..=64 => u64::MAX >> (64 - n)` and reject `n > 64` before subtracting, because `n == 0` shifts `u64` by its 64-bit width and `n > 64` underflows `64 - n`. [F48]
