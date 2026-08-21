# ARM64 KVM (Host/EL1) Subsystem Details

This guide covers invariants and bug patterns for the ARM64-specific KVM
implementation running at EL1 (Host), derived from historical fixes and the
ARM Architecture Reference Manual (ARM ARM).

## VM & VCPU Lifecycle Initialization

ARM64 KVM requires a strict ordering of initialization before a guest can
execute. Violating this results in uninitialized hardware state or
architectural inconsistencies.

> Improper initialization causes **Hypervisor Aborts**, **Guest Execution
> Failures**, and **-EPERM/-ENOEXEC** errors from the KVM API.

*   **Virtual ID Register Initialization:** Virtual registers such as
    `VMPIDR_EL2` and `VPIDR_EL2` reset to an **UNKNOWN** value on Warm reset.
    They MUST be initialized by software before entering EL1 for the first
    time.
*   **Feature Locking & Finalization:** Architectural features that modify
    guest-visible register layouts or system register behavior (e.g., vector
    or tagging extensions) MUST be finalized and locked (via
    `kvm_arm_vcpu_finalize()`) before the first VCPU entry. The implementation
    MUST reject any attempts to modify feature configurations once the guest
    has entered the `RUNNING` state.
*   **Predicate for "guest has started running":** Use
    `vcpu_has_run_once(vcpu)` (defined in `arch/arm64/include/asm/kvm_host.h`
    — checks `vcpu->pid`, which `kvm_arch_vcpu_run_pid_change()` populates on
    the first guest entry). Do NOT use `kvm_vcpu_initialized(vcpu)` for this
    question: it reflects only whether `KVM_ARM_VCPU_INIT` has been called and
    remains true forever after, including mid-run and post-run. Any
    cap/ioctl/sysreg gate whose intent is "reject after the guest has run"
    must use `vcpu_has_run_once()`; `kvm_vcpu_initialized()` answers a
    different question (was init done) and silently accepts post-run
    reconfiguration.

    ```c
    /* WRONG: passes once KVM_ARM_VCPU_INIT has happened, including after KVM_RUN */
    if (!kvm_vcpu_initialized(vcpu))
            return -EBUSY;

    /* CORRECT: rejects once the guest has actually started executing */
    if (vcpu_has_run_once(vcpu))
            return -EBUSY;
    ```
*   **First-Run Resource Synchronization:** VGIC mapping and trap calculation
    MUST be finalized before the VCPU enters guest mode for the first time.
    The kernel utilizes internal safety nets (e.g.,
    `kvm_arch_vcpu_run_pid_change()`) to enforce this ordering during the
    first VCPU transition.
*   **VGIC Mapping:** Virtual GIC resources MUST be mapped to the guest (via
    `kvm_vgic_map_resources()`) before the first VCPU run.
*   **Trap Calculation:** `kvm_calculate_traps()` must be invoked after all
    feature flags and system registers are finalized, but before the first
    VCPU entry.

**REPORT as bugs:**
*   Attempting to run a VCPU without first calling the relevant finalization
    ioctls.
*   Modifying guest feature registers (e.g., `ID_AA64*`) after the first VCPU
    has entered the `RUNNING` state.
*   Gating "has the guest started running" with `kvm_vcpu_initialized()`
    instead of `vcpu_has_run_once()` — accepts post-run reconfiguration.
*   **UAPI Feature Exposure:** Exposing `ID_AA64*` register fields to
    userspace for hardware features that are not explicitly supported or fully
    implemented. **Reviewer check:** Ensure any exposed bit has a
    corresponding enablement/trap configuration in `HCR_EL2`, `CPTR_EL2`, or
    `MDCR_EL2`.

## Architectural State Management (PSCI & Reset)

The consistency of guest architectural state across resets and power
transitions is critical.

> State management bugs lead to **Incorrect Execution Flow**, **Register
> Corruption**, and **Security Bypasses**.

*   **Warm Reset (PSCI CPU_ON):** Most system registers reset to an
    **UNKNOWN** value on warm reset. Software MUST explicitly initialize the
    execution environment, including disabling EL2 traps (`HCR_EL2`,
    `CPTR_EL2`, `CNTHCTL_EL2`) and ensuring `HCR_EL2.RW == 1` for AArch64
    guests. Note: on hardware without `FEAT_AA32EL1`, `HCR_EL2.RW` is RAO/WI
    and the write is architecturally redundant but harmless.
*   **Guest Identification & Affinity Sanity:** Identifiers presented to the
    guest for routing or identification (e.g., `MPIDR_EL1` affinity values,
    GIC target IDs) MUST be unique and architecturally consistent across all
    VCPUs. KVM MUST NOT implement workarounds to accommodate guest
    configurations that violate the ARM ARM.
*   **Exception Injection Ordering:** Exception injection MUST preserve
    `ELR_EL1`/`SPSR_EL1` across exit/entry. Miscoordination between exception
    injection state and the synchronous abort/SError handling path leads to
    ELR clobber (`KVM: arm64: Fix clobbered ELR in sync abort/SError`).
*   **Stage 2 Teardown:** Stage 2 teardown must serialize against concurrent
    `mmu_notifier` callbacks; freeing a page table while a notifier walker can
    still observe it causes UAF or double-free. Fixes chain: `KVM: arm64: Only
    drop references on empty tables in stage2_free_walker`, `Fix double-free
    following kvm_pgtable_stage2_free_unlinked()`, `Fix memory leak on stage2
    update of a valid PTE`.

**REPORT as bugs:**
*   Patches that introduce logic to handle non-unique guest CPU affinities or
    relax identification register validation.
*   Exception injection paths that modify `ELR_EL1`/`SPSR_EL1` without
    atomically resolving the pending exception state before the next guest
    entry.
*   Teardown sequences that destroy page tables while the `mmu_notifier` could
    still be active.

## VGIC CPU Interface Access

The Virtual GIC CPU interface registers are highly sensitive to software
access patterns.

> Violating GIC interface access rules causes **Interrupt Hangs**, **Spurious
> Exceptions**, or **UNPREDICTABLE CPU Behavior**.

*   **Priority Register Writes:** Writing `ICV_AP<0|1>R<n>_EL1` to any value
    other than the last read value or all-zeros (when there are no Group-N
    active priorities), or out of order (AP0 must precede AP1), **may result
    in UNPREDICTABLE behavior** of the virtual interrupt prioritization. These
    registers should only be touched for context switching or power
    management.
*   **Self-Synchronization:** Reads of `ICV_IAR0_EL1` and `ICV_IAR1_EL1` are
    self-synchronizing when interrupts are masked by the PE (`PSTATE.{I,F} ==
    {0,0}`). Writes to `ICV_PMR_EL1` are strictly self-synchronizing (no ISB
    required).
*   **System Register Interface Gating:** Access to the memory-mapped GIC
    virtual CPU interface (`GICV_*`) is gated by `ICC_SRE_EL1_NS.SRE` (not by
    `GICD_CTLR.ARE`). When `SRE == 1`, `GICV_*` registers may be RAZ/WI;
    software should use the system-register interface instead.

## Quick Checks

*   **HCR_EL2 Sync:** `HCR_EL2` writes that affect TLB-cached fields (`RW`,
    `NV1`, `NV`, `E2H`, `FWB`, `DCT`) require TLB invalidation before
    translation changes take effect — `ERET` alone does not flush these cached
    fields. For non-TLB-cached fields, an `ERET` to EL1/0 acts as a CSE
    provided `SCTLR_ELx.EOS == 1` (always check `FEAT_ExS` semantics in
    nVHE/VHE paths). World-switch barrier placement differs between nVHE and
    VHE paths; both must be verified independently.
*   **MTE Feature Filtering:** Verify that MTE features are correctly filtered
    in guest ID registers based on hardware support AND VM type (Protected vs.
    Non-Protected).
*   **Feature ID / RESx Writeable Mask:** `ID_AA64*` register fields exposed
    to userspace must have correct writeable masks and runtime sanitization. A
    field that is RES0 in the hardware but exposed as writable causes silent
    guest misconfiguration. Every new field exposure must be paired with the
    correct `kvm_id_reg_rw_mask` or RESx-handling entry and a corresponding
    trap/enablement in `HCR_EL2`, `CPTR_EL2`, or `MDCR_EL2`.
*   **`vcpu_sysreg` numbering is sparse — never range-check it:** `enum
    vcpu_sysreg` (`arch/arm64/include/asm/kvm_host.h`) indexes
    `vcpu->arch.ctxt.sys_regs[]`, but VNCR-mapped entries are numbered by their
    VNCR-page byte offset (`VNCR(r)` = `__VNCR_START__ + VNCR_r / 8`), not in
    declaration order. Selecting or skipping registers by a numeric value range
    over the enum covers unintended registers: a loop meaning "skip the timer
    block" written as `if (i >= CNTVOFF_EL2 && i <= CNTP_CTL_EL0) continue;` also
    skips `SCTLR_EL1` / `TCR_EL1` / `MAIR_EL1` / … whose offsets fall in that byte
    span, silently dropping guest state (a guest that reprograms its own EL1 regs
    still boots, but the host's `GET/SET_ONE_REG` view desyncs). Flag any numeric
    range or `<` / `>` comparison over `enum vcpu_sysreg` values; require an
    explicit per-register allowlist.

## VGIC LPI and vLPI Invariants

VGIC LPI (Locality-specific Peripheral Interrupt) management involves a
layered locking protocol and GICv4 VPE ownership rules that are a recurring
source of bugs.

- **vgic_irq / LPI xarray lock ordering:** Operations on `vgic_irq` structs
  protected by `irq_lock` must not hold the LPI xarray lock simultaneously
  unless the ordering is explicitly documented. Releasing and re-acquiring
  `irq_lock` while the xarray lock is held violates the ordering and causes
  deadlock.
- **LPI xarray access in atomic context:** The LPI xarray lock must not be
  held while calling `vgic_put_irq()` from a raw spinlock context; any path
  that may call `vgic_put_irq()` must document that the LPI xarray lock may be
  taken.
- **GICv4 vLPI unmapping:** Unmapping a vLPI from a VPE MUST succeed on all
  call paths; a failed unmap leaves a dangling forwarding entry. `WARN` on
  unmap failures and treat them as bugs.
- **vPE allocation gating:** vLPI mappings MUST NOT be attempted when vPE
  allocation is disabled. Gate all forwarding setup on the vPE availability
  check.

**REPORT as bugs:**
- New VGIC code that takes the LPI xarray lock while holding `irq_lock`
  (unless the ordering is explicitly established).
- vLPI forwarding setup that proceeds without verifying vPE allocation is
  enabled.
- Unmap paths that silently ignore or swallow vLPI unmap errors.

## Stage-2 Page-Fault Handler Races

The `user_mem_abort()` path and its pKVM equivalents have a recurring pattern
of resource leaks and stale-state bugs.

- **Page Leak on Error Paths:** Every early-exit error path in the fault
  handler that has already resolved a PFN must drop the PFN reference before
  returning. Missing PFN release on error causes page leaks.
- **Memcache Initialization:** The `kvm_mmu_memory_cache` pointer passed into
  the fault handler must be initialized before use; uninitialized pointer
  dereferences produce NULL-deref crashes that are hard to trace back to the
  fault handler.
- **vma_shift Staleness:** The `vma_shift` value computed at fault-entry time
  may be stale if the VMA is modified concurrently (e.g., during nested
  hwpoison injection). Re-check VMA attributes after taking the mmu_lock.

**REPORT as bugs:**
- Error paths in `user_mem_abort()` or stage-2 fault handlers that return
  without releasing a previously resolved PFN.
- Fault handlers that dereference an uninitialized `kvm_mmu_memory_cache`
  pointer.

## GICv3 Trap Ordering (ICH_HCR_EL2)

- **ICH_HCR_EL2.En synchronization:** Changes to `ICH_HCR_EL2.En` are not
  guaranteed to be seen by subsequent guest execution without an explicit
  exit/re-entry cycle. When enabling or disabling GICv3 virtual CPU interface
  traps, a forced guest exit must follow to flush the stale `ICH_HCR_EL2`
  state in the CPU pipeline.
- **Trap configuration ordering across entry/exit:** GICv3 trap bits in
  `ICH_HCR_EL2` must be resynchronized early on guest entry (before the
  LR/VMCR are loaded) to avoid a window where interrupts are delivered without
  correct trap configuration.
- **Protected vs. non-protected trap divergence:** In pKVM, GICv3 trap
  initialization for protected VMs differs from non-protected guests; the
  wrong path silently leaves traps inactive.

**REPORT as bugs:**
- Code that modifies `ICH_HCR_EL2.En` without a subsequent guest exit to flush
  the change.
- Guest-entry paths that load LRs or VMCR before re-synchronizing
  `ICH_HCR_EL2` trap bits.
