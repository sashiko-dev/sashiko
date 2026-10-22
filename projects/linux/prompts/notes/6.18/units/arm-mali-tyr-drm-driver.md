# ARM MALI TYR DRM DRIVER

The Arm Mali Tyr DRM driver is a Rust-based Direct Rendering Manager (DRM) kernel driver for Arm Mali Valhall CSF GPUs (such as the Mali-G610 on Rockchip RK3588). It manages platform probing, clocks, regulators, MMIO register access, soft reset, L2 cache power-on, and GPU feature discovery, exposing the `panthor` DRM UAPI (such as `DRM_IOCTL_PANTHOR_DEV_QUERY`) for drop-in compatibility with existing userspace stacks like Mesa.

## Architecture & Data Flow

- `GpuInfo` in `drivers/gpu/drm/tyr/gpu.rs` must keep a `#[repr(C)]` layout matching C `struct drm_panthor_gpu_info` with no implicit padding, kernel pointers, or interior mutability, and `GpuInfo::new` must initialize explicit padding (`pad0`, `pad`) and unpopulated feature array elements (`texture_features[1..4]`) to `0`, because `File::dev_query` copies `tdev.gpu_info` directly to userspace via `unsafe impl AsBytes for GpuInfo`. [F13, F46]
- `Register<const OFFSET: usize>` constants in `drivers/gpu/drm/tyr/regs.rs` must specify `OFFSET` as a 4-byte-aligned byte offset (with `_HI` 32-bit halves at `_LO + 4`) within the `SZ_2M` `IoMem` mapping rather than a 32-bit word index, because `Register::read` and `Register::write` pass `OFFSET` directly to `Io::read32` and `Io::write32` (and `#![allow(dead_code)]` masks unused definitions until first use). [F28]
- In `TyrDriver::probe`, the GPU clocks (`core_clk`, `stacks_clk`, `coregroup_clk`) must be prepared and enabled and the `mali` and `sram` regulators acquired as `Regulator<regulator::Enabled>` before `issue_soft_reset`, `gpu::l2_power_on`, and `GpuInfo::new` access GPU MMIO registers through `iomem`. [F2]

## Synchronization

- Because `TyrData` has manual `unsafe impl Send` and `unsafe impl Sync` implementations that bypass compiler auto-trait checks, any field added to `TyrData` must be safe for concurrent access across `&TyrDevice` (`File::open`, `File::dev_query`, `TyrObject::new`) or wrapped in synchronization like `Mutex` (`clks`, `regulators`). [F5]
- `<TyrData as PinnedDrop>::drop` acquires the sleeping `self.clks` `Mutex` and calls `disable_unprepare()` on `core`, `stacks`, and `coregroup` clocks, so the final `ARef<TyrDevice>` reference must never be dropped in atomic context (such as an interrupt handler, timer callback, or RCU callback). [F41]
- `issue_soft_reset` and `gpu::l2_power_on` call `kernel::time::delay::fsleep` for 100ms and must only be invoked from sleepable process context without holding a `SpinLock` or inside a `Devres::try_access` RCU read-side critical section. [F40]

## Object Lifetimes

- Teardown of resources in `TyrData` that back `TyrDevice` (disabling `clks` in `TyrData::drop` and dropping `regulators`) must remain tied to `TyrData` rather than `TyrDriver::drop` or `TyrDriver::unbind`, because `TyrDriver` is dropped on platform unbind before `devres` unregisters the DRM device and before open `DrmFile` references release `TyrDevice`. [F7]
- Every `Clk` or `OptionalClk` stored in `Clocks` and enabled via `prepare_enable()` in `TyrDriver::probe` must have `disable_unprepare()` called on it in `<TyrData as PinnedDrop>::drop`, because `Clk`'s `Drop` implementation only calls `clk_put` without disabling or unpreparing the clock. [F1]
- Regulators acquired as `Regulator<regulator::Enabled>` in `TyrDriver::probe` (`mali_regulator` and `sram_regulator`) must be moved into `TyrData::regulators` (`Regulators`), because dropping a `Regulator<regulator::Enabled>` at the end of `probe` immediately disables the regulator supply. [F33]

## API Contracts

- `Register::read` and `Register::write` must be passed the same bound `Device<Bound>` (`pdev.as_ref()`) that `iomem` (`Devres<IoMem>`) was created for, because `(*iomem).access(dev)` rejects a mismatched device pointer with `EINVAL`. [F3]
- In `TyrDriver::probe`, `drm::driver::Registration::new_foreign_owned` must be passed the exact same `pdev.as_ref()` device passed to `drm::Device::new`, and only after `issue_soft_reset`, `gpu::l2_power_on`, and `GpuInfo::new` have initialized `TyrData`. [F49]
- Because `genmask_u32(start..=end)` returns a bitmask already shifted to bit `start`, extracting a field at `start > 0` requires either shifting the masked result right by `start` (as in `GpuId::from`) or shifting the value right by `start` before masking with `genmask_u32(0..=(end - start))` (as in `GpuInfo::pa_bits`). [F12]
- In `File::dev_query`, when `devquery.pointer == 0`, the handler must validate `devquery.type_` (returning `Err(EINVAL)` for unknown query types) and write the target struct's byte size into `devquery.size` without writing to `devquery.pointer`; when `devquery.pointer != 0`, it must access userspace only via `UserSlice::new(UserPtr::from_addr(devquery.pointer as usize), devquery.size as usize).writer()` (never casting `devquery.pointer` to a raw pointer or sizing the slice with `size_of_val`) so `UserSliceWriter::write` rejects undersized user buffers with `EFAULT`. [F18, F19, F31]
- In `TyrDriver::probe`, `"stacks"` and `"coregroup"` clocks must be acquired with `OptionalClk::get` whereas `"core"` uses `Clk::get`, so Device Trees omitting optional GPU clocks can still bind. [F38]
- Render-node ioctls declared via `kernel::declare_drm_ioctls!` on `TyrDriver` (such as `PANTHOR_DEV_QUERY`) must include `ioctl::RENDER_ALLOW` in their flags so unprivileged userspace clients can invoke them on render node file descriptors. [F48]

## Error Handling

- `issue_soft_reset` and `gpu::l2_power_on` must verify hardware completion after sleeping by checking `regs::GPU_IRQ_RAWSTAT` against `GPU_IRQ_RAWSTAT_RESET_COMPLETED` and `regs::L2_READY_LO` against `1`, returning `Err(EIO)` if the bit is not set. [F11]
- User-induced ioctl errors in `File::dev_query` (such as unknown `devquery.type_` or invalid `devquery.pointer`/`devquery.size` failing `UserSliceWriter::write`) must return `Err(EINVAL)` or `Err(EFAULT)` quietly without logging via `dev_err!` or panicking via `.unwrap()`. [F50]

## External Contracts

- `INFO.name` in `drivers/gpu/drm/tyr/driver.rs` must remain `c_str!("panthor")` so that userspace stacks (such as Mesa) recognize the DRM device as drop-in compatible with the C `panthor` driver. [F36]

## Known Pitfalls

- In `File::dev_query`, the `devquery.pointer == 0` size-query branch and the `devquery.pointer != 0` data-copy branch dispatch on `devquery.type_` in separate `match` blocks that must be updated in lockstep with matching source objects whenever a new `drm_panthor_dev_query_type` variant is added. [F14]
- `issue_soft_reset` must test reset completion by masking `GPU_IRQ_RAWSTAT` with `GPU_IRQ_RAWSTAT_RESET_COMPLETED` (`regs::GPU_IRQ_RAWSTAT.read(dev, iomem)? & regs::GPU_IRQ_RAWSTAT_RESET_COMPLETED == 0`) rather than comparing the full register value with `!=`. [F23]
- Because `Register::read` performs a 32-bit `read32` and returns `Result<u32>`, constructing 64-bit register fields in `GpuInfo::new` (`shader_present`, `tiler_present`, `l2_present`) requires reading both `*_LO` and `*_HI` registers, widening each with `u64::from`, and shifting `*_HI` left by 32 bits before ORing with `*_LO`. [F24]
