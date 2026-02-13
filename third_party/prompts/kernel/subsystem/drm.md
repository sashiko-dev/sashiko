# DRM Subsystem Details

## Atomic Context in Display Hardware Programming

Calling sleeping functions from atomic context causes kernel warnings, system
instability, and potential deadlocks. DRM/KMS display drivers have multiple
code paths that execute in atomic context where sleeping is forbidden.

**Atomic context paths in display drivers:**
- `drm_atomic_helper_commit_tail()` and its variants
- CRTC atomic enable/disable/update callbacks
- Plane atomic update callbacks
- Encoder atomic enable/disable callbacks
- VBLANK handlers and callbacks
- Page flip completion handlers
- Hardware sequencer (`hwseq`) functions called during atomic commits

**Hardware sequencer functions** (`hwseq`, `hw_sequencer` directories in AMD
display drivers, similar patterns in other vendor drivers) implement low-level
display hardware programming. Many of these functions are called from atomic
commit paths and must not sleep.

**Delay functions and atomic context:**

| Function | Can Sleep | Use In Atomic Context |
|----------|-----------|----------------------|
| `udelay()` | No | Safe |
| `ndelay()` | No | Safe |
| `mdelay()` | No | Safe (but discouraged for long delays) |
| `fsleep()` | Yes | Unsafe |
| `msleep()` | Yes | Unsafe |
| `usleep_range()` | Yes | Unsafe |

When reviewing patches that introduce delays in display driver code, verify
the calling context. If the function can be reached from atomic commit paths,
VBLANK handlers, or any spinlock-protected section, only non-sleeping delay
functions (`udelay`, `ndelay`) are safe.

**When replacing wait/poll functions with delays:** If a patch replaces a
hardware polling function (e.g., `wait_for_blank_complete()`) with a fixed
delay, verify whether the original function was designed to work in atomic
context. Hardware polling functions often use busy-wait internally to remain
atomic-safe. A replacement using `fsleep()` or `msleep()` breaks this
invariant.

## Power Management Context Separation (XE Driver)

Using runtime PM flags in system PM paths causes hardware malfunction after
system resume. The device may appear to wake but I2C controllers, display
engines, or other components will not function because reinitialization was
incorrectly skipped.

**System PM vs Runtime PM:**
- **System PM** (`xe_pm_suspend()`, `xe_pm_resume()`): Handles S3/suspend-to-RAM
  and deeper states. The device ALWAYS loses power regardless of configuration.
  Resume must fully reinitialize hardware.
- **Runtime PM** (`xe_pm_runtime_suspend()`, `xe_pm_runtime_resume()`): Handles
  D-state transitions during normal operation. Power loss depends on platform
  and `xe->d3cold.allowed` flag.

**Critical rule**: System PM functions must not use `xe->d3cold.allowed` or
similar runtime PM flags to decide reinitialization depth. System suspend always
loses power, so full reinitialization is always required.

**Pattern to detect:**
```c
// In xe_pm_resume() (system PM) - WRONG
xe_i2c_pm_resume(xe, xe->d3cold.allowed);  // Runtime PM flag in system PM path

// CORRECT
xe_i2c_pm_resume(xe, true);  // System PM always needs full reinit
```

**Fields that indicate runtime PM context** (do not use in system PM paths):
- `xe->d3cold.allowed`
- `xe->d3cold.capable`
- Runtime PM control flags from `struct dev_pm_info`

## XE Driver GT Accessor API Contracts

Dereferencing the return value of `xe_device_get_gt()` without a NULL check
causes a NULL pointer dereference. This function returns NULL for invalid GT
indices or non-existent GTs.

**GT accessor functions:**

| Function | Can Return NULL | Use Case |
|----------|-----------------|----------|
| `xe_device_get_gt(xe, gt_id)` | Yes | General GT lookup by index; caller must check for NULL |
| `xe_root_mmio_gt(xe)` | No | Returns the primary GT of the root tile; used for non-GT MMIO operations |

**When to use each:**
- Use `xe_device_get_gt()` when the GT index comes from user input or
  configuration and NULL is a valid outcome that should be handled
- Use `xe_root_mmio_gt()` when specifically accessing the root tile's primary
  GT and NULL would be a bug (e.g., default values, initialization paths)

**Pattern to detect:**
```c
// WRONG: Direct dereference without NULL check
param->oa_unit = &xe_device_get_gt(oa->xe, 0)->oa.oa_unit[0];

// CORRECT: Use xe_root_mmio_gt() when gt 0 is specifically needed
param->oa_unit = &xe_root_mmio_gt(oa->xe)->oa.oa_unit[0];

// CORRECT: Check for NULL when using xe_device_get_gt()
gt = xe_device_get_gt(xe, id);
if (!gt)
    return -EINVAL;
```

**REPORT as bugs**: Code that directly dereferences `xe_device_get_gt()` return
values (e.g., `xe_device_get_gt(...)->field`) without a preceding NULL check.

## DRM GPU VM (drm_gpuvm) IOMMU Requirement

Returning NULL instead of an error when IOMMU is unavailable allows driver
initialization to proceed in an unsupported configuration. This causes
undefined behavior later when VM operations are attempted on a NULL VM, or
when code assumes a valid VM exists.

**Critical invariant**: `drm_gpuvm` requires IOMMU support. It does not support
physical address fallback modes. Any code path that previously allowed a NULL
VM (for no-IOMMU fallback) must return an error when converted to use
`drm_gpuvm`.

**When reviewing conversions to drm_gpuvm:**

- Look for existing no-IOMMU fallback paths: code that checks `if (!mmu)` or
  `if (!vm)` and continues with `vm = NULL` instead of failing
- These NULL fallback returns must become `ERR_PTR(-ENODEV)` or similar error
  returns
- Check all initialization paths in both GPU and display (KMS/MDP) code, as
  they often share the same VM infrastructure

**Pattern to detect:**
```c
// OLD PATTERN (supports no-IOMMU fallback) - WRONG with drm_gpuvm
mmu = create_mmu(...);
if (!mmu) {
    drm_info(dev, "no IOMMU, fallback to phys contig buffers\n");
    return NULL;  // WRONG: drm_gpuvm requires IOMMU
}

// CORRECT with drm_gpuvm
mmu = create_mmu(...);
if (!mmu) {
    drm_info(dev, "no IOMMU, bailing out\n");
    return ERR_PTR(-ENODEV);  // Fail properly
}
```

**Common locations for no-IOMMU paths:**
- `msm_kms_init_vm()` in `drivers/gpu/drm/msm/msm_kms.c`
- `mdp4_kms_init()` in `drivers/gpu/drm/msm/disp/mdp4/mdp4_kms.c`
- Similar patterns in other display controller initialization functions

**REPORT as bugs**: In drm_gpuvm conversions, any code path that returns NULL
or sets `vm = NULL` when IOMMU/MMU is unavailable. These must return errors.

## DRM Scheduler KUnit Test Flag Semantics

Tests that wait for timeout handler execution may see the handler invoked
multiple times if the wait duration spans multiple timeout periods. Clearing
control flags in the handler creates race conditions where subsequent
invocations behave differently, causing tests to fail spuriously or pass
incorrectly.

**Mock timeout handler re-execution:**
- `mock_sched_timedout_job()` in `drivers/gpu/drm/scheduler/tests/mock_scheduler.c`
  implements the mock timeout behavior for scheduler tests
- Tests that wait for multiples of `MOCK_TIMEOUT` (e.g., `2 * MOCK_TIMEOUT`)
  create windows where the timeout handler fires more than once
- If a control flag governs handler behavior and that flag is cleared on first
  execution, subsequent executions follow a different code path

**Control flags vs status flags:**
- **Control flags** (e.g., `DRM_MOCK_SCHED_JOB_DONT_RESET`): Govern which code
  path the handler takes. Must persist for the job's lifetime so behavior is
  consistent across multiple invocations.
- **Status flags** (e.g., `DRM_MOCK_SCHED_JOB_RESET_SKIPPED`): Record that a
  specific event occurred. Set when the event happens; never cleared.
- Tests should assert on status flags to verify the intended code path executed,
  not on the absence of control flags (which may indicate a race occurred).

```c
// WRONG: Clearing control flag creates race on re-execution
if (job->flags & DRM_MOCK_SCHED_JOB_DONT_RESET) {
    job->flags &= ~DRM_MOCK_SCHED_JOB_DONT_RESET;  // Cleared
    return DRM_GPU_SCHED_STAT_NO_HANG;
}
// Second invocation: flag is clear, takes different path

// CORRECT: Control flag persists, status flag records execution
if (job->flags & DRM_MOCK_SCHED_JOB_DONT_RESET) {
    job->flags |= DRM_MOCK_SCHED_JOB_RESET_SKIPPED;  // Record event
    return DRM_GPU_SCHED_STAT_NO_HANG;
}
// Second invocation: same behavior, status flag already set
```

**REPORT as bugs**: Test code that modifies control flags in the handler those
flags govern, especially in timeout/completion/interrupt handler mocks where
re-execution is possible during extended wait periods.

## XE GuC CT Debug Infrastructure Initialization

Calling `stack_depot_save()` or similar kernel infrastructure APIs without
prior initialization causes NULL pointer dereferences. The stack depot
infrastructure must be explicitly initialized before use.

**XE GuC CT initialization stages:**
- `xe_guc_ct_init_noalloc()`: Early initialization (no allocations). This is
  where debug infrastructure must be initialized.
- `xe_guc_ct_init()`: Resource allocation phase.

**Debug config guards and initialization:**
When code under `CONFIG_DRM_XE_DEBUG_GUC` or similar debug config options uses
kernel infrastructure APIs (e.g., `stack_depot_save()`), the initialization
call (e.g., `stack_depot_init()`) must:
1. Be placed in `xe_guc_ct_init_noalloc()` (before any code path that uses it)
2. Be guarded by the same config option as the usage site

**Infrastructure requiring explicit initialization:**
- `stack_depot`: requires `stack_depot_init()` before `stack_depot_save()`

**Pattern to detect:**
```c
// In fast_req_track() under CONFIG_DRM_XE_DEBUG_GUC
handle = stack_depot_save(entries, nr, GFP_ATOMIC);  // Uses stack_depot

// WRONG: xe_guc_ct_init_noalloc() does not call stack_depot_init()

// CORRECT: xe_guc_ct_init_noalloc() must include:
#if IS_ENABLED(CONFIG_DRM_XE_DEBUG_GUC)
    stack_depot_init();
#endif
```

**REPORT as bugs**: Code that adds calls to kernel infrastructure APIs (like
`stack_depot_save()`) under debug config options without verifying that the
corresponding initialization function is called in the subsystem init path
under the same config guard.

## MSM VM Lazy Initialization

Accessing `ctx->vm` directly before the VM has been created causes a NULL
pointer dereference or use of uninitialized memory. The msm driver uses lazy
initialization for virtual memory address spaces.

**VM lifecycle:**
- VMs are NOT created when the DRM context is created
- VMs are created on-demand when first needed
- Userspace can opt into features (like VM_BIND) before the VM exists

**Accessor pattern:**
- `msm_context_vm(dev, ctx)`: Ensures the VM is created before returning it.
  Use this accessor in ioctl entry points and early code paths.
- `ctx->vm` direct access: Unsafe before the VM is guaranteed to exist.

```c
// WRONG: ctx->vm may not be initialized yet
if (to_msm_vm(ctx->vm)->unusable)
    return -EPIPE;

// CORRECT: ensures VM exists before access
if (to_msm_vm(msm_context_vm(dev, ctx))->unusable)
    return -EPIPE;
```

**Risky code paths:**
- Early in ioctl handlers before any GPU operations have occurred
- After feature opt-in (VM_BIND) but before first mapping operation
- Code that validates context state before performing real work

**REPORT as bugs**: Direct access to `ctx->vm` or `to_msm_vm(ctx->vm)` in ioctl
entry points or early validation code without a preceding call to
`msm_context_vm()` in the same function.

## struct_size() Overflow Semantics

Checking `if (sz > SIZE_MAX)` after assigning the result of `struct_size()` to
a variable is dead code that provides no protection. The check can never
trigger because `struct_size()` returns `SIZE_MAX` on overflow, not a value
exceeding it.

**How `struct_size()` works:**
- Returns the correctly computed size when no overflow occurs
- Returns `SIZE_MAX` (saturates) when overflow would occur
- The return type is `size_t`, which is the same width as `SIZE_MAX`

**Dead code pattern:**
```c
// WRONG: sz can never be > SIZE_MAX after this assignment
u64 sz = struct_size(job, ops, nr_ops);
if (sz > SIZE_MAX)
    return -ENOMEM;  // Dead code - never executes
```

**Correct patterns:**
```c
// OPTION 1: Check for equality with SIZE_MAX
size_t sz = struct_size(ptr, member, count);
if (sz == SIZE_MAX)
    return -ENOMEM;

// OPTION 2: Let kzalloc fail gracefully (SIZE_MAX allocation fails)
ptr = kzalloc(struct_size(ptr, member, count), GFP_KERNEL | __GFP_NOWARN);
if (!ptr)
    return -ENOMEM;
```

**REPORT as bugs**: Overflow checks of the form `if (sz > SIZE_MAX)` or
`if (size > SIZE_MAX)` after assignment from `struct_size()`, `array_size()`,
or similar size-computing macros that saturate at `SIZE_MAX`.

## Intel GPU Platform and Subplatform Architecture

Incorrect platform modeling causes hardware misidentification, leading to wrong
PHY configurations, PCH misdetection, or failed display initialization. Intel
GPU drivers use a two-level hierarchy where platforms define major architecture
and subplatforms differentiate hardware variants within a platform.

**Platform level:**
- Defined by major hardware architecture (e.g., TGL, DG2, MTL, PTL)
- Platform descriptors (`*_desc`) specify IP version ranges, feature
  capabilities, and workaround sets
- PCI device IDs map to platform descriptors via macros like `INTEL_PTL_IDS`

**Subplatform level:**
- Used when hardware variants share a platform descriptor but require
  differentiation in specific code paths
- Common differentiation points:
  - PHY detection (`intel_encoder_is_c10phy()` and similar)
  - PCH configuration
  - Display port/encoder handling
  - Power management sequences

**When subplatform registration is required:**
- Hardware shares the same platform architecture but has different IP versions
  (e.g., different display IP version within the same platform family)
- Hardware requires different PHY handling, PCH support, or display
  configuration despite sharing general driver flows
- Existing code paths check for subplatform-level distinctions within the
  platform family

**Registration pattern:**
- PCI IDs for subplatforms should be separated into distinct macros (e.g.,
  `INTEL_WCL_IDS` separate from `INTEL_PTL_IDS`)
- Platform descriptor must include subplatform registration that maps PCI IDs
  to the subplatform identifier
- Both i915 and xe drivers need consistent subplatform handling

**Files involved in platform/subplatform registration:**
- `include/drm/intel/pciids.h`: PCI ID macro definitions
- `drivers/gpu/drm/i915/display/intel_display_device.c`: i915 device table and
  platform descriptors with subplatform arrays
- `drivers/gpu/drm/xe/xe_pci.c`: xe device table

**REPORT as bugs**: Commits that add device IDs to an existing platform macro
where the commit message indicates hardware differences (different IP versions,
architectural variants) but no subplatform registration is included.

### Platform Flag Overloading and Hardware Assumptions

When a single platform flag (e.g., `display->platform.pantherlake`) covers
multiple hardware variants with different configurations, code making
hardware-specific decisions can silently break for some variants.

**Overloaded platform flags:**
- A platform flag like `display->platform.pantherlake` may encompass multiple
  hardware variants (e.g., PTL and WCL) that share driver infrastructure but
  have different PHY configurations, port capabilities, or display features
- Code that extends conditions like `phy == PHY_A` to `phy < PHY_C` may be
  correct for the new variant but incorrect for existing variants covered by
  the same flag

**Hardware assumption validation:**

When commit messages or comments state hardware facts like:
- "Platform X doesn't have feature Y"
- "There will never be a case where..."
- "Extending this should not cause issues for platform Z"

These are hypotheses that must be validated, not facts to accept:
- VBT enumeration can enumerate ports even when not connected to expected PHY
  types (e.g., PORT B enumerated for Type-C on PTL even without C10 PHY)
- Type-C configurations may use different PHY types (C20 instead of C10)
- Edge cases that "will never happen" on one variant may occur on another

**PHY type identification functions:**

Functions like `intel_encoder_is_c10phy()` make critical decisions about PHY
type. When reviewing changes:
- Verify the condition is correct for ALL variants covered by the platform flag
- Trace downstream effects when the function returns an incorrect value (wrong
  PHY initialization, incorrect power sequences, lane configuration failures)
- Check whether ports can be enumerated via VBT in configurations the code
  assumes do not exist

**REPORT as bugs**: Code that extends platform-specific conditions (like PHY
type checks) to cover additional ports/PHYs when the platform flag covers
multiple variants with different hardware configurations, unless the commit
explicitly handles each variant's requirements or adds subplatform
differentiation.

## AMDGPU Buffer Object Allocation Contracts

Passing uninitialized pointers to `amdgpu_bo_create_kernel()` causes silent
misuse of random memory addresses as buffer object pointers. Instead of
allocating a new BO, the function attempts to pin garbage memory, leading to
undefined behavior, crashes, or data corruption.

**Dual behavior of `amdgpu_bo_create_kernel()`:**
- If `*bo_ptr == NULL`: Creates and pins a new buffer object
- If `*bo_ptr != NULL`: Attempts to pin the existing BO at that address

This contract is documented in the function's kernel-doc: "Note: For bo_ptr
new BO is only created if bo_ptr points to NULL."

**Wrapper functions for external callers:**

When wrapping `amdgpu_bo_create_kernel()` or `amdgpu_bo_create_reserved()` for
external drivers (V4L2 ISP, display, etc.), the wrapper must explicitly
initialize the pointer parameter to guarantee new BO creation. External
callers may pass uninitialized stack memory through opaque `void **`
parameters.

```c
// WRONG: relies on external caller to pass initialized memory
int wrapper_alloc(void **buf_obj) {
    struct amdgpu_bo **bo = (struct amdgpu_bo **)buf_obj;
    return amdgpu_bo_create_kernel(adev, size, align, domain, bo, ...);
}

// CORRECT: explicitly initialize to guarantee new BO creation
int wrapper_alloc(void **buf_obj) {
    struct amdgpu_bo **bo = (struct amdgpu_bo **)buf_obj;
    *bo = NULL;  // Force creation of new BO
    return amdgpu_bo_create_kernel(adev, size, align, domain, bo, ...);
}
```

**When reviewing AMDGPU wrapper functions, check for:**
- Functions that cast `void **` to `struct amdgpu_bo **` and pass to BO
  allocation functions
- Missing explicit `*bo = NULL` initialization before `amdgpu_bo_create_kernel()`
- API documentation claiming "allocates a new BO" without guaranteeing the
  internal pointer is NULL

**REPORT as bugs**: Wrapper functions exported to external drivers that pass
opaque pointer parameters to `amdgpu_bo_create_kernel()` without first
initializing `*bo = NULL`.

## AMDGPU Address Format APIs

Using the wrong address API in debugfs or diagnostic interfaces causes userspace
tools to receive incorrectly formatted data, leading to silent failures or
wrong analysis results. The AMDGPU driver has multiple functions that return
addresses in different formats.

**Address format functions:**

| Function | Returns | Use Case |
|----------|---------|----------|
| `amdgpu_bo_gpu_offset()` | GPU virtual address | Addresses within GPU's virtual address space for general buffer access |
| `amdgpu_gmc_pd_addr()` | Page Directory (PD) address | Hardware register format for pagetable debugging, VM root addresses |

**When exporting VM/pagetable information:**
- For pagetable dump interfaces consumed by UMR or similar debugging tools,
  use `amdgpu_gmc_pd_addr()` to get the PD format that matches hardware
  register values
- `amdgpu_bo_gpu_offset()` returns a different format that does not match
  what debugging tools expect for pagetable analysis

```c
// WRONG: GPU virtual address format for pagetable debugging
seq_printf(m, "address: 0x%llx\n", amdgpu_bo_gpu_offset(vm.root.bo));

// CORRECT: PD address format for pagetable debugging
seq_printf(m, "pd_address: 0x%llx\n", amdgpu_gmc_pd_addr(vm.root.bo));
```

**REPORT as bugs**: Debugfs files that export VM root or pagetable addresses
using `amdgpu_bo_gpu_offset()` instead of `amdgpu_gmc_pd_addr()`.

## XE Pcode Mailbox Register Updates

Directly assigning new values to pcode mailbox registers without preserving
existing fields corrupts hardware configuration. Power limit time windows,
enable bits, and other settings are lost, causing hardware to operate with
incorrect parameters or to behave erratically after power limit updates.

**Pcode mailbox register layout:**
- Pcode mailbox communication passes register values that contain multiple
  packed fields (enable bits, value fields, time window configurations)
- Power limit registers combine: `PWR_LIM_EN` (enable), `PWR_LIM_VAL` (value),
  `PWR_LIM_TIME` (time window)
- When updating one field (e.g., the power limit value), other fields must be
  preserved using read-modify-write (RMW)

**Pattern requiring RMW:**
```c
// WRONG: loses PWR_LIM_TIME and other fields
ret = xe_pcode_read(tile, mbox, &val0, &val1);
val0 = uval;  // Direct assignment
ret = xe_pcode_write64_timeout(tile, mbox, val0, val1, timeout);

// CORRECT: preserves other fields
ret = xe_pcode_read(tile, mbox, &val0, &val1);
val0 = (val0 & ~clear_mask) | set_value;  // RMW pattern
ret = xe_pcode_write64_timeout(tile, mbox, val0, val1, timeout);
```

**When to suspect missing RMW:**
- Functions named `*_write_*` that first call `*_read_*` then directly assign
  to the read value (not modify it)
- Multiple field definitions exist for the same register (multiple
  `REG_GENMASK` or `REG_BIT` macros covering different bit ranges)
- Hwmon or power management code updating configuration values

**REPORT as bugs**: Pcode write sequences where the value read is overwritten
with direct assignment (`val = new_value`) rather than masked modification
(`val = (val & ~mask) | new_value`).

## AMDGPU Ring Buffer Write Pointer Types

Using mismatched types for write pointers (wptr) causes integer truncation when
ring buffers are large or write pointers exceed 32-bit range. This leads to
incorrect ring buffer indexing, lost commands, or memory corruption.

**Type requirement**: All write pointer variables, parameters, and struct fields
in AMDGPU ring buffer code must be `u64`, not `u32`.

- `struct amdgpu_ring` stores `wptr` as `u64`
- `struct amdgpu_fence` stores `wptr` as `u64`
- Functions operating on write pointers must use `u64` for all wptr parameters

**Pattern to detect:**
```c
// WRONG: mismatched wptr types cause truncation
static void func(struct amdgpu_ring *ring, u64 start_wptr, u32 end_wptr)

// CORRECT: both wptr parameters are u64
static void func(struct amdgpu_ring *ring, u64 start_wptr, u64 end_wptr)
```

When reviewing functions with multiple wptr-related parameters, verify type
consistency. If one parameter is `u64`, all semantically equivalent parameters
should also be `u64`.

**REPORT as bugs**: Function signatures where some wptr parameters are `u64` and
others are `u32`, especially when both represent write positions on the same ring.

## AMDGPU Fence Sequence Number Wrap-Around

Incorrect handling of fence sequence number wrap-around causes fence iteration
loops to terminate early or skip all fences entirely. When `sync_seq` wraps to a
small value while the last processed sequence is near the maximum, direct
comparison (`i <= sync_seq`) fails immediately even though unprocessed fences
exist.

**Sequence number characteristics:**
- Fence sequence numbers in `amdgpu_fence_driver` are `uint32_t` cyclic counters
- The fence array is indexed using a mask: `fences[seq & num_fences_mask]`
- Sequence numbers can wrap around from large values to small values

**Incorrect pattern (direct comparison):**
```c
// WRONG: fails when sync_seq wraps around
for (i = seqno + 1; i <= ring->fence_drv.sync_seq; ++i) {
    ptr = &ring->fence_drv.fences[i & ring->fence_drv.num_fences_mask];
    // ...
}
```

If `seqno = 0xFFFFFFFF` and `sync_seq = 0x00000005` (wrapped), the loop condition
`i <= 0x00000005` is immediately false, skipping all fences.

**Correct pattern (masked do-while):**
```c
// CORRECT: handles wrap-around via masked equality
last_seq = amdgpu_fence_read(ring) & ring->fence_drv.num_fences_mask;
seq = ring->fence_drv.sync_seq & ring->fence_drv.num_fences_mask;
do {
    last_seq = (last_seq + 1) & ring->fence_drv.num_fences_mask;
    ptr = &ring->fence_drv.fences[last_seq];
    // ...
} while (last_seq != seq);
```

**Detection heuristics:**
- Look for loops iterating over sequence numbers with `<=` or `<` comparisons
- Check if the loop index is masked when used for array access but not in the
  loop condition (inconsistent masking)

**REPORT as bugs**: Fence iteration loops that use direct comparison
(`i <= sync_seq`) rather than masked equality (`last_seq != seq`) for
termination.

## Display Scaling Mode Semantics (RMX_*)

Using `RMX_FULL` as a default scaling mode distorts the displayed image by
stretching it to fill the screen without preserving aspect ratio. Users see
distorted content (circles become ovals, squares become rectangles) which is
almost never the intended behavior for automatic scaling.

**Scaling mode values:**

| Mode | Behavior | Aspect Ratio |
|------|----------|--------------|
| `RMX_OFF` | No scaling, native resolution only | N/A |
| `RMX_FULL` | Stretch to fill entire screen | Not preserved (distorts image) |
| `RMX_ASPECT` | Scale to maximum size while preserving ratio | Preserved (black bars added) |
| `RMX_CENTER` | Center image without scaling | Preserved |

**When the driver sets scaling automatically** (e.g., for non-native resolutions
on eDP panels when userspace has not explicitly requested a scaling mode):
- `RMX_ASPECT` preserves user content appearance and is the appropriate default
- `RMX_FULL` distorts content and should only be used when explicitly requested
  by userspace or when hardware limitations require it

**Pattern to detect:**
```c
// WRONG: distorts aspect ratio as an automatic default
if (!scaling_enabled && non_native_resolution)
    dm_new_connector_state->scaling = RMX_FULL;

// CORRECT: preserves aspect ratio for automatic scaling
if (!scaling_enabled && non_native_resolution)
    dm_new_connector_state->scaling = RMX_ASPECT;
```

**REPORT as bugs**: Code that sets `RMX_FULL` as an automatic or default scaling
mode without explicit userspace request or documented hardware requirement.

## i915 CX0 PHY Register Access Protocol

Accessing CX0 PHY registers without the proper transaction wrapper or
prerequisite configuration causes PHY failures, hardware timeouts, and error
messages like "PHY * failed after N retries". The i915 CX0 PHY subsystem
requires specific protocols for register access.

**Mandatory transaction wrapper:**

All functions that access CX0 PHY registers via `intel_cx0_rmw()`,
`intel_cx0_read()`, or `intel_cx0_write()` must be wrapped in a transaction:

```c
intel_wakeref_t wakeref;
wakeref = intel_cx0_phy_transaction_begin(encoder);
// ... PHY register accesses ...
intel_cx0_phy_transaction_end(encoder, wakeref);
```

When reviewing new functions that call these PHY access functions:
- Verify the function has `intel_cx0_phy_transaction_begin/end` wrapper
- If the function lacks the wrapper, verify that ALL callers have an active
  transaction before calling
- The transaction ensures PHY power management and register access
  serialization

**C10 VDR register programming sequence:**

For C10 PHY (check with `intel_encoder_is_c10phy()`), accessing PHY internal
registers via MsgBus requires setting `C10_VDR_CTRL_MSGBUS_ACCESS` first. This
is documented in Bspec 68962.

```c
// CORRECT: Set MSGBUS_ACCESS before internal register access
if (intel_encoder_is_c10phy(encoder))
    intel_cx0_rmw(encoder, owned_lane_mask, PHY_C10_VDR_CONTROL(1), 0,
                  C10_VDR_CTRL_MSGBUS_ACCESS, MB_WRITE_COMMITTED);

// ... then access PHY_CMN1_CONTROL or other internal registers ...
```

When reviewing functions that access `PHY_CMN1_CONTROL` or similar internal
PHY registers, verify `C10_VDR_CTRL_MSGBUS_ACCESS` is set before the access
when C10 PHY is in use.

**AUXLess ALPM conditional register access:**

Functions that configure hardware settings specific to AUXLess ALPM (per Bspec
68849) must use early return to skip all register access when the feature is
not active. Accessing PHY registers unconditionally can cause failures even
when the "enable" bit is conditionally set within the write.

```c
// WRONG: Accesses registers even when feature is not active
bool enable = intel_alpm_is_alpm_aux_less(...);
for (i = 0; i < 4; i++) {
    intel_cx0_rmw(..., enable ? BIT : 0, ...);  // Still accesses register
}

// CORRECT: Early return when feature is not active
if (!intel_alpm_is_alpm_aux_less(enc_to_intel_dp(encoder), crtc_state))
    return;
// Register access only occurs when feature is active
for (i = 0; i < 4; i++) {
    intel_cx0_rmw(..., BIT, ...);
}
```

**REPORT as bugs**: New functions that call `intel_cx0_rmw()`,
`intel_cx0_read()`, or `intel_cx0_write()` without either (1) having
`intel_cx0_phy_transaction_begin/end` wrapper, or (2) documentation that all
callers hold an active transaction.

## DisplayPort DPCD Register Access Patterns

Reading certain DPCD registers triggers unintended hardware state changes in
DP/eDP sinks, causing screen flickering, link training failures, or incorrect
voltage swing reporting. This occurs because some registers participate in link
training state machines and are not safe for arbitrary read access.

**DPCD registers with potential side-effects:**

Registers in the link training status range may cause state machine transitions
when read, even though this behavior is technically non-compliant:

| Register | Address | Risk When Read |
|----------|---------|----------------|
| `DP_LANE0_1_STATUS` | 0x202 | Participates in CR/EQ training state machine; may trigger state changes |
| `DP_LANE2_3_STATUS` | 0x203 | Same as above |
| Training status registers | 0x202-0x207 | Part of link training feedback loop |

**Safer alternatives for probing:**

- `DP_TRAINING_PATTERN_SET` (0x102): Configuration register; reading does not
  affect sink state
- `DP_DPCD_REV` (0x000): Capability register with no side-effects
- Simple status/capability registers outside the training status range

**When reviewing DPCD register address changes:**

- Patches changing which register is used for probing, detection, or quirk
  workarounds require careful analysis of the new register's role
- Check if the register is read during normal link training sequences (CR/EQ)
- Registers that participate in state machines may cause different hardware
  responses when read outside their expected sequence
- Panel-specific quirks exist: some sinks have non-compliant behavior that only
  manifests with specific register access patterns

**Pattern to detect:**
```c
// Changing DPCD probe address - verify new register is safe
-	ret = drm_dp_dpcd_probe(aux, DP_DPCD_REV);
+	ret = drm_dp_dpcd_probe(aux, DP_LANE0_1_STATUS);  // Risky - training state machine
```

**REPORT as bugs**: Changes to DPCD probe/quirk register addresses that switch
to registers in the link training status range (0x202-0x207) without
documenting why the register is safe to read in all operational contexts.

## AMDGPU PSP Firmware Version Checking

Calling new PSP GFX commands on firmware that does not support them causes
command failures, hardware timeouts, or undefined behavior. PSP firmware and
hardware are versioned independently: newer hardware may run older firmware
during updates, testing, or due to deployment policies.

**Dual version guard requirement:**

When code calls `psp_cmd_submit_buf()` or `psp_get_fw_reservation_info()` with
a new or recent `GFX_CMD_ID_*` command, both checks are required:
- `amdgpu_ip_version(adev, MP0_HWIP, 0)` -- hardware capability
- `adev->psp.sos.fw_version` -- firmware capability

```c
// WRONG: IP version check only - missing firmware version guard
if (amdgpu_ip_version(adev, MP0_HWIP, 0) == IP_VERSION(14, 0, 2)) {
    psp_get_fw_reservation_info(psp, GFX_CMD_ID_NEW_FEATURE, ...);
}

// CORRECT: Both IP and firmware version checks
if (amdgpu_ip_version(adev, MP0_HWIP, 0) == IP_VERSION(14, 0, 2)) {
    if (adev->psp.sos.fw_version >= MINIMUM_FW_VERSION) {
        psp_get_fw_reservation_info(psp, GFX_CMD_ID_NEW_FEATURE, ...);
    }
}
```

**Detection heuristics:**
- New enum values added to `psp_gfx_cmd_id` in `psp_gfx_if.h`
- Functions that check `amdgpu_ip_version(adev, MP0_HWIP, 0)` without a
  corresponding `adev->psp.sos.fw_version` check
- PSP command submission in init/resume paths without version validation

**REPORT as bugs**: Code that calls PSP GFX commands with only an IP version
check and no firmware version guard, especially for commands introduced in
recent IP versions (14.0.2, 14.0.3, or newer).

## XE SR-IOV Virtual Function (VF) Constraints

Attempting to register or access PF-only hardware resources from a Virtual
Function (VF) context causes hardware failures, MMIO timeouts, or incorrect
driver behavior. The xe driver supports SR-IOV virtualization where VFs have
restricted access to certain hardware subsystems.

**VF-restricted resources in xe driver:**
- I2C controllers (PF-only access)
- Direct SOC_BASE MMIO register access
- Some power management features
- GuC firmware loading (handled by PF)

**Required VF check in probe paths:**

When adding new device or subsystem registration in probe functions (especially
for hardware controllers like I2C, PMU, or display components), verify whether
the resource is accessible to VFs. If the resource is PF-only, add an early
return guard:

```c
// CORRECT: VF check before PF-only resource access
int xe_foo_probe(struct xe_device *xe)
{
    if (IS_SRIOV_VF(xe))
        return 0;

    // ... PF-only initialization ...
}
```

**Detection pattern:**

New `*_probe()` or device registration functions that:
- Access SOC_BASE MMIO regions
- Register platform devices for hardware controllers (I2C, PMU)
- Set up interrupts from the root device
- Access PCI config space directly

If these operations appear in new probe paths without an `IS_SRIOV_VF()` guard,
flag as potential bug.

**REPORT as bugs**: New probe functions in xe driver that register platform
devices or access hardware controllers without checking `IS_SRIOV_VF()` when
the resource is not available to VFs.

## fwnode API Error Return Conventions

Checking for NULL when a function returns `ERR_PTR()` causes error conditions
to go undetected, leading to crashes when the error pointer is later
dereferenced.

**fwnode functions that return ERR_PTR (not NULL):**
- `fwnode_create_software_node()` - returns `ERR_PTR()` on failure

Note: `software_node_register()` returns `int` (not `ERR_PTR()`), and
`fwnode_graph_get_*()` functions return NULL on failure. Only functions that
return `struct fwnode_handle *` and are documented to use `ERR_PTR()` require
`IS_ERR()` checks.

**Incorrect pattern:**
```c
// WRONG: fwnode_create_software_node returns ERR_PTR, not NULL
fwnode = fwnode_create_software_node(props, NULL);
if (!fwnode)
    return -ENOMEM;  // Never triggers - ERR_PTR is non-NULL
// Crash: fwnode is an invalid error pointer, dereference fails
```

**Correct pattern:**
```c
// CORRECT: Check for error pointer and extract error code
fwnode = fwnode_create_software_node(props, NULL);
if (IS_ERR(fwnode))
    return PTR_ERR(fwnode);
```

When reviewing code that calls fwnode APIs, verify the error check matches the
return convention. A NULL check (`if (!ptr)`) for an `ERR_PTR`-returning
function is always a bug.

**REPORT as bugs**: Code that checks `if (!fwnode)` or `if (fwnode == NULL)`
after calling `fwnode_create_software_node()` or similar fwnode APIs that
return `ERR_PTR()` on error.

## Quick Checks

- **Sleeping functions in hwseq paths**: `fsleep`, `msleep`, `usleep_range`,
  `mutex_lock`, `GFP_KERNEL` allocations in hardware sequencer functions
  require atomic context analysis
- **Atomic commit callback implementations**: CRTC, plane, and encoder atomic
  callbacks run in atomic context during non-blocking commits
- **VBLANK and page flip handlers**: always atomic context, no sleeping allowed
- **Runtime PM flags in system PM**: If `xe_pm_resume()` or `xe_pm_suspend()`
  uses `d3cold.allowed` or similar flags for conditional behavior, verify
  this is intentional (it usually is not)
