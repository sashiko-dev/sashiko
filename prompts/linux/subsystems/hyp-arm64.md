# ARM64 Hyp (EL2) Subsystem Details

This guide covers the ARM64 Hypervisor implementation running at EL2, focusing
on pKVM (Protected KVM) and nVHE isolation, derived from historical fixes and
the ARM Architecture Reference Manual (ARM ARM).

## pKVM Threat Model and Scope

pKVM at EL2 provides a fixed set of guarantees against a hierarchy of
adversaries. Reviews must scope findings against this model: violations inside
the model are bugs; behavior that is undesirable but outside the model (e.g.
host self-DoS) is a hardening improvement, not a bug.

### What pKVM guarantees

*   **Guest confidentiality** vs. Host: Host EL1 cannot read protected-VM
    memory or register state.
*   **Guest integrity** vs. Host: Host EL1 cannot modify protected-VM memory
    or register state out-of-band.
*   **Hypervisor integrity** vs. Host AND Guests: neither side can corrupt EL2
    memory, redirect EL2 control flow, or escape stage-2 isolation.
*   **Host availability** vs. Guests: a guest cannot crash the host kernel or
    take down EL2 (taking EL2 down takes the host down with it).

### What pKVM does NOT guarantee

*   **Host availability vs. itself**: the host kernel can panic itself, or
    trigger a hyp panic via the hypercall ABI from its own privileged code
    paths. pKVM does not defend the host against the host.
*   **Reliability against firmware/hardware faults**: if EL3, the SMMU, the
    GIC, or other firmware/hardware misbehaves, pKVM cannot recover.

### Reachability test for any panic-reachable EL2 path

For any code that can reach a fatal primitive at EL2 (`BUG`, `BUG_ON`,
`WARN_ON`, direct `hyp_panic()`), ask: **who can trigger it?**

| Trigger source | Verdict |
|---|---|
| EL2 internal invariant violation | Bug in EL2 itself, separate from the panic |
| Hardware / firmware error | Out of scope (trust boundary) |
| Host kernel, via privileged code paths | Not a bug. Hardening improvement if cheap |
| Guest, via DMA / hypercall side-effects / shared memory | **Bug** (violates host availability vs. guests) |
| Host userspace, via syscall/ioctl → host kernel → hypercall | **Bug** (violates the Linux kernel security model: userspace must not crash the kernel) |

The host-userspace row matters because many hypercalls are reachable from
userspace through the host's syscall surface; a hyp panic on
userspace-influenced inputs breaks Linux's standard "userspace must not crash
the kernel" property even if the immediate caller at EL1 is the host kernel.

This test composes with the §`WARN_ON` Semantics correctness test, but does
not replace it. Correctness ("is this assertion the right kind of check?") and
severity ("if it triggers, is it a bug?") are independent axes.

## EL2 Execution Context (nVHE/pKVM)

At EL2 in nVHE/pKVM, a hypercall (or other trap) handler runs as a single
**atomic, non-preemptible unit on the trapping CPU**, with physical interrupts
masked, returning to EL1 via `eret` when done. The EL2 hyp is not the kernel:
there is **no scheduler, no preemption, and none of the kernel-context
deferred-work machinery** — no `sleep`/`schedule`, workqueues, softirqs, RCU
callbacks, kthreads, `copy_{from,to}_user`, or `printk`/`pr_*`, and no mutexes
or `irqsave` locks (interrupts are already masked; EL2 locking is
`hyp_spin_lock`). A handler cannot be preempted part-way through and cannot hand
work to a later context: whatever it does, it does synchronously before the
`eret`.

**Worked false-positive.** A finding of the form "this `READ_ONCE()` and the
later `cmpxchg()` can race because the store becomes visible only after a
*delayed* write" presumes the handler can be preempted, or its tail deferred,
between the two. At EL2 nVHE there is no such local gap — the sequence runs to
completion atomically on the trapping CPU.

**Still in scope — cross-CPU concurrency.** This rule removes only the
*local* preemption / deferral assumption. Genuine concurrency between two
physical CPUs each running a handler against shared EL2 state is real and must
still be reasoned about under the LKMM (memory ordering, cmpxchg visibility
across CPUs). Do not let "EL2 is atomic" collapse into "EL2 is single-threaded."

**Do NOT flag:**
- Races or reorderings that require a single EL2 nVHE handler to be preempted,
  or its work deferred to a later context, on the *same* CPU.

## Host De-Privilege Boundary (pKVM Lifecycle)

In nVHE/pKVM, the host kernel boots at EL2 and *de-privileges itself* to EL1
once hyp setup is complete. This boundary is the most important architectural
state for reviewing pKVM patches: the same code can run in a fundamentally
different trust regime depending on which side of it executes.

The boundary is `finalize_pkvm()` (`arch/arm64/kvm/pkvm.c`), registered at
`device_initcall_sync`. It calls `pkvm_drop_host_privileges()`, which switches
the host out of EL2 for good. Before this initcall level, the host kernel is
still executing at EL2 — code is privileged and can directly set up EL2 state,
install hyp text/data, populate per-CPU state, and finalize trap
configuration; memory shared with future-EL2 is writable in place. After it
(`pkvm_drop_host_privileges()` having run), the host is at EL1 and can only
invoke EL2 via hypercalls; EL2-private memory becomes inaccessible (kmemleak
excluded below).

**Markers a reviewer can grep for:**

*   `__init` / `__initdata` on hyp-touching code: pre-de-privilege only.
*   **Initcall level** is the canonical pre-vs-post marker. `finalize_pkvm()`
    is registered at `device_initcall_sync`. Functions registered at any level
    strictly before that (`early_initcall`, `pure_initcall`, `core_initcall`,
    `postcore_initcall`, `arch_initcall`, `subsys_initcall`, `fs_initcall`,
    `rootfs_initcall`, `device_initcall`, and the `_sync` variants of those)
    run before `finalize_pkvm` and are pre-de-privilege. `late_initcall` and
    later levels run post-de-privilege. `module_init(fn)` expands to
    `device_initcall(fn)`. Code reachable from `kvm_arm_init`
    (`device_initcall`, including `init_hyp_mode`, `init_subsystems`,
    `finalize_init_hyp_mode`) is therefore pre-de-privilege.
*   `__pkvm_init` and **`__pkvm_init_finalise`**
    (`arch/arm64/kvm/hyp/nvhe/setup.c`) execute *during* de-privilege itself,
    after `kvm_arm_init` and just before the host drops out of EL2. These are
    the last opportunity to set up EL2-private state from privileged context.
*   `is_kvm_arm_initialised()` (`arch/arm64/include/asm/virt.h`) is the
    canonical predicate for "KVM-arm init has completed" (post-de-privilege).
    A guard of the form `if (... || is_kvm_arm_initialised()) return -EINVAL;`
    rejects the call *after* init, so the call is permitted only in the
    privileged window.
*   `is_protected_kvm_enabled()` (`arch/arm64/include/asm/virt.h`) is the
    canonical predicate for "pKVM mode is configured." It becomes true very
    early via cpufeature detection (before any initcall runs) and is
    independent of de-privilege state — so on its own it does *not* tell you
    whether the privileged window is still open. The discriminator the
    hypervisor actually uses is the `kvm_protected_mode_initialized` static key
    (read host-side via `is_pkvm_initialized()`), enabled during pKVM
    finalisation (`arch/arm64/kvm/pkvm.c`): while it is off the early
    "privileged" hypercalls are reachable, and `handle_host_hcall`
    (`arch/arm64/kvm/hyp/nvhe/hyp-main.c`) selects the init-only / always-on /
    finalised-only hcall bands off exactly that branch.
*   **Hypercall ID band** decides a new hypercall's availability phase. A new
    `enum __kvm_host_smccc_func` entry before `__KVM_HOST_SMCCC_FUNC_MIN_PKVM`
    is init-only (gone once pKVM finalises); between `MIN_PKVM` and
    `__KVM_HOST_SMCCC_FUNC_PKVM_ONLY` it is always-on; after `PKVM_ONLY` it is
    pKVM-finalised-only. A new ID in the wrong band is reachable in the wrong
    phase.

**REPORT as bugs:**

*   Code that handles host-shared memory without identifying which side of
    de-privilege it runs on (the §EL2 Security & Trust Boundary rules apply
    post-de-privilege only).
*   Patches that move setup code across the `finalize_pkvm` boundary without
    updating the trust assumptions of the moved code (host-trusted in-place
    writes pre-de-priv become host-untrusted post-de-priv).
*   Runtime hypercall handlers that reference `__init` / `__initdata` symbols
    (those are freed/inaccessible after de-privilege; in particular,
    `kmemleak_free_part` is called on hyp `.bss` / `.data` / `.rodata` in
    `finalize_pkvm`).

## EL2 Security & Trust Boundary (pKVM)

Post-de-privilege (after `finalize_pkvm`), the Host (EL1) is an adversary
against guest confidentiality, guest integrity, and hypervisor integrity (see
§pKVM Threat Model and Scope). EL2 MUST NOT derive security-sensitive values
from any host-controlled source.

> Failure to validate Host-provided data leads to **Hypervisor-level Memory
> Corruption** or **Information Leaks**. Severity classification (bug vs.
> hardening improvement) follows §pKVM Threat Model and Scope: host
> availability against the host itself is not in scope, but compromise of
> confidentiality, integrity, or guest-reachable availability is.

*   **Untrusted Host Data Sources:**
    *   **Host Memory** (`kern_hyp_va` dereferences): Subject to **TOCTOU**
        (Time-of-Check to Time-of-Use) attacks. Data MUST be copied to private
        EL2 memory (e.g., local stack via struct assignment or `memcpy`)
        BEFORE validating or acting on it.
    *   **System Registers** carrying host-written state (e.g., `SCTLR_EL1`,
        `HCR_EL2`): EL2 MUST use **hardcoded architectural constants** or
        known-good EL2-private state instead of reading back what the host
        last wrote.
    *   **Hypercall Arguments**: Every host-supplied value MUST be validated
        and bounds-checked before use. Never act on raw hypercall parameters.
        Scalars passed in registers (e.g., `u64`, handles, indices captured
        via `DECLARE_REG`) become EL2-private once held in a local variable;
        the copy-then-validate rule applies to host-memory pointers
        dereferenced via `kern_hyp_va`, not to register-passed values.
*   **Double-Fetch Risk:** Never dereference a host-provided pointer multiple
    times (double-fetch). Copy the necessary fields to EL2 private memory
    once.
*   **Allocation Sources:** EL2 MUST NOT draw allocations from a free list or
    memcache whose head pointer lives in host memory — the host can redirect
    the allocation to an attacker-chosen physical address (TOCTOU). A
    host-resident cache such as `stage2_teardown_mc` is a sink for host-bound
    reclaim pages, never an allocation source.
*   **VA Translation (`AT`):** To perform stage 1+2 translation of a Host VA,
    EL2 must use `AT S12E1R`. If translation fails, hardware reports the error
    in `PAR_EL1` (bit `.F = 1`).
    - **Note:** The targeted translation regime (EL1&0 vs EL2&0) depends on
      `HCR_EL2.{E2H, TGE}`.

**REPORT as bugs:**
*   Dereferencing a Host pointer multiple times without an intervening copy to
    private EL2 memory.
*   Logic that assumes a value in a Host-shared structure (like `struct
    kvm_vcpu`) remains constant between a check and a subsequent use.
*   Code that reads security-sensitive state (like trap configurations) from
    registers known to be modifiable by the Host.

```c
// WRONG: Double-fetching host-shared memory OR stack overflow risk
void handle_hcall(struct kvm_vcpu *host_vcpu) {
    // 1. Double fetch from host memory!
    if (vcpu_has_sve(kern_hyp_va(host_vcpu))) {
        do_sve(kern_hyp_va(host_vcpu));
    }
    // 2. Unsafe stack allocation (~4KB overflow)
    struct kvm_vcpu local_vcpu = *kern_hyp_va(host_vcpu); 
}

// CORRECT: Atomic Copy-then-Validate (Small Struct)
void handle_hcall(struct vcpu_reset_args *host_args) {
    struct vcpu_reset_args local_args;
    
    // Copy once to private memory to prevent TOCTOU
    memcpy(&local_args, kern_hyp_va(host_args), sizeof(local_args));
    
    // Validate and act on the PRIVATE copy only
    if (local_args.flags & VALID_FLAG) {
        update_hyp_state(&local_args);
    }
}
```

## pKVM/nVHE Invariants (EL2)

Violating hypervisor invariants results in **Hypervisor Panics**, **Silent
Isolation Breaks**, or **Memory Protection Bypass**.

### Security Metadata Initialization (pKVM)
Security-critical metadata tracking physical resources (e.g., page ownership
tables like `hyp_vmemmap` in `arch/arm64/kvm/hyp/include/nvhe/memory.h`) MUST
use initialization patterns that evaluate to the least-privileged or "unowned"
state. This ensures that accidental access to zero-filled or uninitialized
memory does not result in unauthorized ownership or permission grants. In
pKVM, this is achieved via a complement-based state where zero-initialization
evaluates to `PKVM_NOPAGE`.
> **REPORT as bugs:** Code that checks page state by direct comparison with
> zero or assumes zero-filled metadata means "owned by hypervisor."

### Stage-2 VMID & Consistency
The architecture does not require `VTTBR_EL2` and `VTCR_EL2` (see
`arch/arm64/include/asm/kvm_mmu.h`) to be identical across all PEs for a VMID,
**provided** `VTTBR_EL2.CnP == 0`.
> **REPORT as bugs:** Setting `VTTBR_EL2.CnP = 1` (Common not Private) on
> multiple PEs if their translation table pointers differ for the same VMID.
> This results in **CONSTRAINED UNPREDICTABLE** translations.

### Fine-Grained Traps (FEAT_FGT)
Accesses to FGT registers (e.g., `HFGRTR_EL2`) may be reordered. To guarantee
a newly enabled trap is active for subsequent instructions, a **Context
Synchronization Event (CSE)** (e.g., `isb` or exception return) MUST occur.

### SMC Trapping
For AArch64 guests, `SMC` instructions trapped from EL1 via `HCR_EL2.TSC = 1`
result in `ESR_EL2.EC = 0x17`; for AArch32 guests, `SMC32` uses `EC = 0x13`.
`SPSR_EL2.SS` captures the `PSTATE.SS` (Software Step) bit of the trapped EL1
context.

## State Divergence & Initialization Boundaries

pKVM state is physically isolated from the Host. Assuming Host state changes
are visible to EL2 leads to **State Desynchronization**.

| Host State | EL2 Hyp State | Sync Mechanism | Source Reference |
| :--- | :--- | :--- | :--- |
| `struct kvm` | `struct pkvm_hyp_vm` | `pkvm_create_hyp_vm()` | `arch/arm64/kvm/hyp/nvhe/pkvm.c` |
| `struct kvm_vcpu` | `struct pkvm_hyp_vcpu` | Hypercall Parameters | `arch/arm64/kvm/hyp/nvhe/hyp-main.c` |
| ID Registers | Hyp-Private ID Regs | Sanitisation in `pkvm_hyp_vm` | `arch/arm64/kvm/hyp/nvhe/sys_regs.c` |

*   **Hyp vs. Host Back-Pointer:** `struct pkvm_hyp_vm` *embeds* a `struct kvm
    kvm` — the EL2-private hyp copy — and separately holds `struct kvm
    *host_kvm`, a back-pointer to the host's (untrusted) instance. Fields
    accessed via `hyp_vm->kvm.X` are EL2-private and safe to treat as trusted
    after initialisation; fields reached via `hyp_vm->host_kvm->X` are
    host-writable and subject to TOCTOU. The same distinction applies to
    `pkvm_hyp_vcpu` (`hyp_vcpu->vcpu` is EL2-private; `hyp_vcpu->host_vcpu` is
    the untrusted back-pointer).
*   **Persistent State Rule:** Copies from Host memory to the stack are for
    **validation only**. Persistent guest state changes MUST be synchronized
    to EL2-private hyp structures (e.g., `pkvm_hyp_vcpu`) to remain visible
    across guest entries.

## `kern_hyp_va` Idempotence and Pointer Provenance

Misjudging when `kern_hyp_va()` transforms a pointer drives findings in both
directions: a false "double `kern_hyp_va()` corrupts the pointer / faults" on a
pointer it actually leaves unchanged, and a missed "defensive `kern_hyp_va()` on
an already-hyp pointer" that genuinely mangles it. The discriminator is the
pointer's provenance, and it is **not** whether the address is below
`PAGE_OFFSET`.

`kern_hyp_va()` converts a host kernel-linear (TTBR1) pointer to the EL2
hyp-linear VA used to dereference host memory at EL2. It is a **mask-and-tag**
operation (`__kern_hyp_va` in `arch/arm64/include/asm/kvm_mmu.h`; mask and tag
computed in `arch/arm64/kvm/va_layout.c`): clear the high VA bits, then insert a
single constant hyp tag. It has **no offset accumulation**, so it is idempotent
on any pointer that *already carries the hyp tag*. Two classes do, and both are
**below `PAGE_OFFSET`** yet still idempotent:

- The hyp-linear image `kern_hyp_va()` itself produces — so a second application
  is a no-op.
- Every EL2 **linear-map** pointer: the hyp page allocator's output
  (`hyp_phys_to_virt` / `hyp_page_to_virt`, `nvhe/memory.h`) and anything
  reachable by `hyp_virt_to_phys` / `hyp_virt_to_page`. This covers the large
  EL2-private objects — `pkvm_hyp_vm`, `pkvm_hyp_vcpu`, and the embedded
  `hyp_vm->kvm` (see §State Divergence & Initialization Boundaries, "Hyp vs. Host
  Back-Pointer").

So the real split is **linear-map** (carries the tag → `kern_hyp_va` is a no-op)
vs **private-VA-range** (no tag → mangled), not above/below `PAGE_OFFSET`.

Idempotence does NOT extend to an EL2 **private-VA-range** pointer — the output
of `pkvm_alloc_private_va_range` (`nvhe/mm.c`): `hyp_vmemmap`, the fixmap,
ioremap/MMIO mappings, hyp stacks. These live outside the linear map and do not
carry the hyp tag, so the mask relocates them and the *first* application
corrupts the pointer.

Severity of a genuinely mis-applied `kern_hyp_va()` depends on the target tree's
`__kern_hyp_va()`. Upstream masks unconditionally, so a private-range input is
corrupted. Android adds `if (!is_ttbr1_addr(v)) return v;` (a `>= PAGE_OFFSET`
test), so there every already-hyp pointer short-circuits to a harmless no-op and
only a genuine TTBR1 pointer is transformed. Check which form the tree carries
before assigning severity.

**Worked false-positive.** A pointer initialized as a hyp VA and later passed
through `kern_hyp_va()` again is the recurring trap. `hyp_vcpu->vcpu.arch.sve_state`
is set to a hyp VA in `pkvm_vcpu_init_sve()`, and `unpin_host_sve_state()` used
to call `kern_hyp_va()` on it again; because that conversion is idempotent the
call was redundant, not a bug, and upstream `02471a78a052b` removed it ("Since
`kern_hyp_va()` is idempotent, it's not a bug"). The same holds for `kern_hyp_va(&hyp_vm->kvm)` or
`kern_hyp_va(vcpu->kvm)` when `vcpu` is the hyp vCPU (`vcpu->kvm == &hyp_vm->kvm`,
a linear-map object). Do not escalate any of these to memory corruption or a data
abort; at most note the redundant call.

**Do NOT flag:**
- A redundant or double `kern_hyp_va()` on a linear-map pointer — a host
  kernel-linear pointer, its hyp image, or any EL2 object reachable by
  `hyp_virt_to_phys` / `hyp_virt_to_page` (e.g. `&hyp_vm->kvm`) — as corruption,
  a fault, or any severity bug. It is a no-op; at most a cleanup note, since a
  redundant `kern_hyp_va()` obscures the host→EL2 boundary each call site marks.

## EL2 Buddy Allocator (`hyp_pool`)

EL2 uses a private buddy allocator (`struct hyp_pool` in
`arch/arm64/kvm/hyp/include/nvhe/gfp.h`, implementation in
`nvhe/page_alloc.c`). It is the *only* page allocator available at EL2 — there
is no `kmalloc`, no `alloc_pages`. There is one global pool (`hpool`, set up
in `__pkvm_init_finalise`) plus one per protected VM (`hyp_vm->pool`).

**API** (all symbols are EL2-only; do not confuse with the host page
allocator):

*   `hyp_alloc_pages(pool, order)` — returns a refcount=1 page; NULL on OOM.
    Pages are zeroed.
*   `hyp_get_page(pool, addr)` — increments refcount.
*   `hyp_put_page(pool, addr)` — decrements; on last ref the page is **zeroed
    and reattached to the buddy tree**. (Zeroing happens on free, not on
    alloc.)
*   `hyp_split_page(page)` — break a high-order block into
    individually-refcounted order-0 pages; each must be put separately.
*   `hyp_pool_init(pool, pfn, nr_pages, reserved_pages)` — `reserved_pages`
    are kept at refcount 1 and never enter the free tree.
*   `hyp_page_count(addr)` — returns the current refcount of the page.

**Invariants:**

*   **Lock discipline:** `pool->lock` protects *both* the buddy tree *and*
    per-page metadata (`struct hyp_page::refcount`, `::order`). Refcount
    changes that may trigger a tree update (`hyp_put_page`) MUST happen inside
    the same critical section as the tree update. Do not read or mutate
    `page->refcount` / `page->order` outside `pool->lock` and assume
    consistency.
*   **`HYP_NO_ORDER` convention:** for a high-order block, only the head
    `struct hyp_page` carries its order; the tail `struct hyp_page`s carry
    `HYP_NO_ORDER`. Walkers that inspect `->order` must handle this.
*   **Cross-pool:** every `hyp_put_page` must use the same pool the page was
    allocated from. Mixing `hpool` with a per-VM pool corrupts both.
*   **External pages:** `__hyp_attach_page` accepts pages outside
    `[range_start, range_end)` and inserts them at order 0 without coalescing
    — used for host donations. "Freed page is not in the pool's range" is
    therefore not by itself a bug.

**REPORT as bugs:**

*   Touching `page->refcount` or `page->order` without `pool->lock`.
*   Treating `hyp_alloc_pages()` failure as fatal (`WARN_ON` / `BUG_ON`) —
    `-ENOMEM` is a normal runtime outcome.
*   Allocating from one pool and freeing into another.

## EL2 Transient Mappings (`hyp_fixmap`)

To touch a host page outside the linear hyp map, EL2 uses `hyp_fixmap_map(phys)`
/ `hyp_fixmap_unmap()` (`nvhe/mm.c`). The slot is **per-CPU**, the calls **must
be paired**, and mappings **must not nest**: a path that fails to unmap on every
exit holds the slot and corrupts the next user on that CPU, and a nested map
clobbers the live slot. The unmap is also what performs the slot's TLB
invalidation (the map does not), so a missed unmap leaves a stale valid TLB
entry and the next map's freshly written PTE is bypassed: the new user reads or
writes the previous mapping's physical page.

**REPORT as bugs:**
- A `hyp_fixmap_map()` whose error or early-exit paths do not all reach the
  matching `hyp_fixmap_unmap()`.
- A second `hyp_fixmap_map()` on a CPU before the first is unmapped.

## pKVM Page-Ownership Transitions

pKVM tracks physical page ownership across three actors (host, hypervisor,
guest). Ownership transitions (share, unshare, donate) are the highest-density
pKVM-specific bug pattern and are in the attacker's direct reach from the
host.

- **Argument Validation Before Transition:** Every hypercall that initiates an
  ownership transition MUST validate the supplied range arguments (base
  address, size) against the current ownership state BEFORE modifying any EL2
  metadata. Unvalidated ranges cause out-of-bounds metadata corruption.
- **Cross-Check After Transition:** After completing a transition, EL2 MUST
  verify that the resulting ownership state is consistent with what was
  requested. Silent inconsistency propagates across subsequent transitions.
- **Atomicity of State Updates:** Ownership metadata and page-table entries
  for the affected range MUST be updated atomically from EL2's perspective.
  Partial updates observable to other CPUs create TOCTOU windows.
- **Rollback Before Propagating Errors:** When EL2 mutates ownership metadata
  (`hyp_vmemmap`, page state, refcounts) *before* calling a fallible primitive
  (`kvm_pgtable_stage2_map`, `pkvm_create_mappings_locked`, …), the error path
  MUST undo the mutation before returning. A bare `return err` after a partial
  mutation leaves EL2 metadata inconsistent.
- **Reclaim Path Discipline:** The reclaim path for a dying guest MUST
  enumerate pages by their recorded ownership state, not by the page-table
  walk. Pages already donated but whose metadata was not updated are otherwise
  leaked.

**REPORT as bugs:**
- Ownership transition hypercalls that proceed without fully validating the
  supplied range against current EL2 metadata.
- Reclaim or teardown paths that walk the page table rather than the ownership
  metadata to enumerate pages to free.

## FF-A Interface Validation

The Firmware Framework for Arm (FF-A) interface between EL2 and EL3 is a large
attack surface with a recurring pattern of missing range/offset checks.

- **Offset and Length Validation:** Every FF-A memory-sharing hypercall that
  accepts a buffer descriptor MUST validate the `offset` and `length` fields
  against the actual buffer size before dereferencing. Missing checks allow
  the host to construct a descriptor that causes EL2 to read out-of-bounds
  hypervisor memory.
- **Version Negotiation:** EL2 MUST enforce the agreed FF-A version; downgrade
  responses from EL3 must be rejected correctly and not silently used as a
  higher version. Correct acquire/release ordering is required for
  version-negotiation state visible across CPUs.
- **Unsupported Interface Masking:** Optional FF-A 1.1/1.2 interfaces that
  pKVM does not implement MUST be masked in responses to `FFA_FEATURES`
  queries. Unmasked optional interfaces are advertised as supported, leading
  to incorrect guest behavior.

**REPORT as bugs:**
- FF-A handlers that use a host-supplied offset or length without
  bounds-checking against the declared buffer size.
- `FFA_FEATURES` responses that advertise optional interfaces not implemented
  by pKVM.

## Trap Register Initialization and Protected-vs-Unprotected Divergence

- **Initialization in EL2:** Trap configuration registers (`HFGRTR_EL2`,
  `MDCR_EL2`, etc.) MUST be initialized in EL2-private context, not relied
  upon from values written by the host. On protected VMs, the hyp initializes
  trap state from hardcoded architectural constants; on non-protected VMs, a
  copy of the host-set values is made on VCPU load. Missing initialization
  leaves traps in UNKNOWN state after warm reset.
- **FGT registers are EL2-only:** `HFGRTR_EL2`, `HFGWTR_EL2`, `HFGITR_EL2`,
  `HDFGRTR_EL2`, `HDFGWTR_EL2`, and `HAFGRTR_EL2` UNDEF on EL1/EL0 access
  (absent nested virt). The host cannot write them architecturally, so reading
  them back after an EL2-side write (e.g., to verify a just-applied
  configuration) is safe and is NOT a "reading host-written state" violation.
  The rule below applies to registers the host can write, such as `SCTLR_EL1`
  or `HCR_EL2`.
- **Copy on VCPU Load:** For non-protected pKVM guests, FGT trap registers
  MUST be copied from the host VCPU (`hyp_vcpu->host_vcpu->arch.fgt`) to the
  hyp VCPU (`hyp_vcpu->vcpu.arch.fgt`) on each VCPU load. Stale hyp-side
  copies cause incorrect trap behavior for the next guest entry.
- **MTE / ID Register Initialization:** MTE flag initialization and ID
  register initialization for protected VMs must follow the protected-VM path,
  not the non-protected path. Using the wrong path silently leaves per-VM
  security flags in an incorrect state.

**REPORT as bugs:**
- Hyp code that reads trap configuration from host-written registers rather
  than EL2-private hyp state.
- VCPU-load paths that do not refresh the hyp-side FGT register copy from the
  host VCPU.

## Host-Owned vs Hyp-Owned `HCR_EL2` Bits (Protected vCPUs)

For a protected vCPU, `HCR_EL2` is computed by the hyp at vCPU init from
architectural and feature state (`pkvm_vcpu_reset_hcr()` seeds `HCR_GUEST_FLAGS`
plus feature bits; `pvm_init_traps_hcr()` layers on the guest's trap
configuration), *not* from the host. The host's `hcr_el2` value must never be
allowed to define a protected guest's regime. But a small set of `HCR_EL2` bits
are *host-owned runtime signals*, and those MUST flow from the host vCPU copy
into the hyp vCPU on each load/entry (`handle___pkvm_vcpu_load`,
`flush_hyp_vcpu`) and back on exit (`sync_hyp_vcpu`). The per-entry set is
therefore an **allowlist**, and a reviewer must classify each `HCR_EL2` bit the
code touches into one of two categories:

- **Hyp-owned regime / configuration bits** define the guest's translation,
  execution, and trap environment: e.g. `VM` (stage-2 enable), `RW`
  (AArch64/AArch32 execution state), `TGE`, `IMO`/`FMO`/`AMO` (interrupt
  routing), `TSC` (SMC trapping), `E2H`, and the `TID*`/`TACR` feature-trap
  bits. These are fixed at hyp vCPU init. The host must never be able to set or
  clear them for a protected guest.
- **Host-owned runtime signals** are set/cleared dynamically by the host to
  apply a benign policy or inject an event, and are meaningless unless they
  reach the hyp vCPU that runs: `TWI`/`TWE` (WFI/WFE trapping policy) and `VSE`
  (a pending *virtual SError* to deliver to the guest; under FEAT_RAS the host
  also writes `VSESR_EL2` for the syndrome, then sets `VSE` to make it pending).
  The virtual IRQ/FIQ pending bits (`VI`/`VF`) are architecturally the same
  class of injection signal, but are not part of this allowlist: KVM delivers
  guest interrupts through the vGIC, not via these `HCR_EL2` bits.

Because the per-entry set is an allowlist, **both directions are bugs**:

- **Over-inclusion (security):** sourcing a regime/configuration bit from the
  host into a protected hyp vCPU lets the host reconfigure the guest's
  trap/execution environment.
- **Under-inclusion (function):** dropping a host-owned signal from the
  allowlist means the host can no longer reach the guest with it. Canonical
  case: `flush_hyp_vcpu()` masked the host's `hcr_el2` down to
  `HCR_TWI | HCR_TWE`, which silently dropped `HCR_VSE`, so a host-injected (and
  deferred/masked) virtual SError was never delivered to a pKVM guest. The
  confinement that caused this was introduced by `b56680de9c648` ("KVM: arm64:
  Initialize trap register values in hyp in pKVM"); the fix restores `HCR_VSE`
  to the flowed set. A new per-entry mask that omits a legitimate host-owned
  signal is this bug.

**REPORT as bugs:**
- A protected-vCPU load/entry path that sources any `HCR_EL2` bit *other than* a
  host-owned signal (i.e. a regime/configuration bit) from the host.
- A per-entry `HCR_EL2` allowlist that omits a host-owned injection signal the
  host relies on (e.g. `HCR_VSE` for virtual SError delivery), so the event
  never reaches the guest.

**Do NOT flag:**
- The per-entry path flowing only the allowlisted host-owned bits
  (`HCR_TWI`/`HCR_TWE`/`HCR_VSE`) while leaving every other `HCR_EL2` bit at its
  hyp-init value. That confinement is correct, not a "partial HCR sync."

## FPSIMD / SVE Save Regime (pKVM Mode vs Standard nVHE)

EL2 switches FPSIMD/SVE/SME state lazily: the first guest access to FP/SIMD/SVE
traps to `kvm_hyp_handle_fpsimd()` (`hyp/switch.h`), which deactivates the
relevant CPTR traps, saves the live host context, and restores the guest
context. How the *host's* state is preserved diverges by **KVM mode** — whether
pKVM is enabled (`is_protected_kvm_enabled()`), NOT by whether the individual
guest is a protected pVM — and under pKVM that divergence is a confidentiality
invariant, not a performance tweak:

- **pKVM enabled (`is_protected_kvm_enabled()`):** the trap handler eagerly
  saves the host's FP/SVE state *in the hyp*, gated on
  `is_protected_kvm_enabled() && host_owns_fp_regs()`
  (`kvm_hyp_save_fpsimd_host()`). The gate is the global mode, so this applies
  to **every** guest the hyp runs — protected pVMs and non-protected guests
  alike (all of which take the `is_protected_kvm_enabled()` branch of
  `handle___kvm_vcpu_run`, i.e. `flush_hyp_vcpu` / `sync_hyp_vcpu`). The hyp
  owns host-state save/restore here "as not to reveal that fpsimd was used by a
  guest nor leak upper sve bits": the host must not be able to infer a guest
  touched FP, and stale host SVE register bits must never be visible to the
  guest. The host SVE state is saved at the *host's* max VL
  (`kvm_host_sve_max_vl`), not the guest's.
- **Standard nVHE (pKVM disabled):** the hyp does NOT eagerly save host FP
  state. The host is fully trusted and its vCPU is run directly; its own
  lazy-FPSIMD machinery restores host state after the run, and the hyp only
  marshals vector length around entry/exit via `fpsimd_lazy_switch_to_guest()` /
  `fpsimd_lazy_switch_to_host()` (which move `ZCR_EL1` / `ZCR_EL2`), wrapped
  around `__kvm_vcpu_run()`. These lazy-switch helpers are NOT used under pKVM.

New or modified EL2 FP-trap / FP-switch code must respect this split.

**REPORT as bugs:**
- Under pKVM, an FP path that restores (or exposes) guest FP without first
  ensuring the hyp has saved and scrubbed the host's live FP/SVE state. This
  leaks host register contents, including SVE upper bits, into the guest.
- Saving *host* FP/SVE state at the guest's VL (or any VL narrower than the
  host max), which truncates the host's registers.
- FP-trap code that keys the eager-save decision off the per-guest protected
  flag instead of `is_protected_kvm_enabled()`, or that assumes the
  `fpsimd_lazy_switch_to_*` path runs under pKVM.

**Do NOT flag:**
- The absence of an eager `kvm_hyp_save_fpsimd_host()` on the standard-nVHE
  (pKVM-disabled) path. There the trusted host's own lazy restore handles it; a
  hyp-side eager save is not required and is not a "missing save."
- The eager host-save running for a *non-protected* guest under pKVM. The gate
  is the global mode, so this is correct, not redundant or misapplied work.

## Per-Load vs Per-Entry Synchronization Seams (pKVM Lifecycle)

When a guideline or commit message says some state "MUST be synced," the *seam*
it names is part of the requirement. pKVM has two synchronization points with
very different cadence, and a "missing sync" finding is valid only when the
check is applied at the seam the requirement actually names.

| Seam | Function(s) | Cadence | Handles |
| :--- | :--- | :--- | :--- |
| **Per-load** | `handle___pkvm_vcpu_load` (calls `pkvm_load_hyp_vcpu`) | Once, when the host's `KVM_RUN` loads a vCPU onto a physical CPU | Coarse configuration that persists across many entries (e.g. the `arch.fgt` block copy for non-protected guests) |
| **Per-entry** | `flush_hyp_vcpu` (and the matching `sync_hyp_vcpu` on exit) | Every guest re-entry — potentially thousands of times between loads | Fine-grained per-entry state churn (e.g. `hcr_el2`, `mdcr_el2`, `arch.iflags`) |

Before flagging a "missing sync," quote the guideline's *exact* cadence word —
"on each vCPU **load**" vs "on each **entry**" / "**run**" — and confirm the
check is against the matching function. A copy the requirement places at vCPU
load is satisfied even when it is absent from the per-entry path, and vice
versa.

**Worked false-positive.** FGT trap registers are required to be copied "on
each vCPU load" (see §Trap Register Initialization). That copy lives in
`handle___pkvm_vcpu_load`, whose non-protected `else` branch copies the whole
`arch.fgt` block from `host_vcpu` into the hyp vCPU. Flagging `flush_hyp_vcpu`
(the per-entry path) for "not syncing `arch.fgt`" is a false positive: it is
the wrong seam. The per-entry path correctly syncs only per-entry state; the
per-load FGT copy is not its responsibility.

**Do NOT flag:**
- Per-load state (the FGT block, etc.) "missing" from the per-entry path
  (`flush_hyp_vcpu` / `sync_hyp_vcpu`) when it is correctly handled at vCPU
  load.

## SMC imm16 / SMCCC Pass-Through Rules

EL2 controls which `SMC` encodings the host is permitted to forward to EL3.
This gating is part of the pKVM trust boundary.

- **imm16 == 0 Enforcement:** The host is only permitted to use `SMC`
  instructions with `imm16 == 0`. Any `SMC` with a non-zero `imm16` MUST be
  rejected by EL2 before forwarding to EL3. This prevents the host from
  exploiting firmware interfaces not intended for guest use.

**REPORT as bugs:**
- SMC pass-through paths that forward SMC instructions to EL3 without checking
  that `imm16 == 0`.

## Huge-Page Handling under pKVM

Large-page (block-descriptor) handling in protected-mode stage-2 is a distinct
failure mode from normal page faults.

- **Protected-Mode Size Validation:** When installing a stage-2 mapping for a
  protected VM, the mapping granule MUST be validated against the requested
  fault granule. Installing a larger block mapping for a fault that expected a
  page-size mapping silently breaks isolation.
- **Range Adjustment on Stage-2 Fault:** The range covered by a stage-2 fault
  must be computed from the IPA and the level of the faulting entry, not from
  the host-supplied VMA shift. A stale or host-manipulated VMA shift produces
  an incorrect range adjustment.

**REPORT as bugs:**
- Protected-mode fault handlers that install block descriptors without
  verifying the block granule matches the expected fault size.
- Fault handlers that derive the mapping range from the host VMA shift without
  re-validating against the IPA alignment.

## `WARN_ON` Semantics at EL2

At EL2 in nVHE/pKVM, both `BUG_ON()` and `WARN_ON()` expand to `BRK` (via
`__BUG_FLAGS` in `arch/arm64/include/asm/bug.h`), which the hyp panic handler
treats as fatal. There is no "warn and continue" semantics for these macros at
EL2 — code after a triggered `WARN_ON` is unreachable. The rule below applies
to any EL2 invocation whose expansion ultimately reaches that `BRK`; if a
patch uses a warning primitive that does not, the rule does not apply.

**Test for any `WARN_ON(cond)` at EL2: can `cond` evaluate true through any
contract-permitted input — or only through a violation of EL2's own
invariants?**

*   *Through contracted inputs* (`WARN_ON` wrong): host-supplied input
    post-de-privilege, allocator/lookup outcomes, hardware/firmware return
    values, concurrency races, anything timeout/poll-driven.
*   *Only through invariant violation* (`WARN_ON` correct): values from EL2's
    own just-completed state — a slot EL2 just populated being NULL, a
    refcount EL2 just incremented being zero, an internal data structure EL2
    just initialized being inconsistent.

If a separate bug elsewhere makes an EL2-internal invariant violatable (e.g.,
a slot left NULL by a different lifecycle bug), flag *that* bug — do not
demote the local `WARN_ON`. `WARN_ON` defends the invariant; chaining "the
invariant might be broken elsewhere" into "therefore this assertion is wrong"
defeats its purpose.

A `WARN_ON` flagged "wrong" by this test is a *correctness* finding: the
assertion is the wrong tool, the function should error out instead. Whether it
is also a *bug* is judged separately by the reachability test in §pKVM Threat
Model and Scope. A host-kernel-only reachable wrong `WARN_ON` is a hardening
improvement. A guest- or host-userspace-reachable one is a bug.

**Dead-branch trap:** patterns like `WARN_ON(err); do_fallback();` or `if
(WARN_ON(err)) goto out;` at EL2 do NOT execute the recovery path — the WARN
path panics. The error-handling code is dead. Reviewers familiar with
host-side `WARN_ON` semantics routinely miss this.

Do not invoke `CONFIG_BUG=n` to upgrade a defensive `WARN_ON` into a
"null-deref" claim. `CONFIG_BUG=n` is an EXPERT-only opt-out; the test above
(reachability through contracted inputs) determines correctness, not the
hypothetical no-op behavior of a corrected assertion.

### BUG() and hyp_panic() are equivalent at EL2 nvhe

Plain `BUG()` (not just `BUG_ON()`) at EL2 nvhe expands to `BRK BUG_BRK_IMM`.
That instruction is caught by the EL2 exception vector
(`kvm_unexpected_el2_exception`), which calls
`__guest_exit_restore_elr_and_panic`, which calls `hyp_panic()`. `BUG()` and a
direct `hyp_panic()` call are therefore functionally equivalent at EL2 — both
terminate with a hyp panic, no return.

The prevailing convention in `arch/arm64/kvm/hyp/nvhe/` is `BUG()`: 6 call
sites across the `*.c` files (4 in `hyp-main.c` alone) versus 2 direct
`hyp_panic()` call sites. **Do not flag a `BUG()` ↔ `hyp_panic()` substitution
as a regression** — neither form is more or less correct than the other, and
`BUG()` is the house style.

## Quick Checks

*   **Dead State Invariant:** On any fatal initialization error or
    host-triggered corruption, the VM MUST be marked as definitively "dead"
    (e.g., via `is_dying` in `struct kvm_protected_vm`). This acts as a
    fail-safe to prevent resource reassignment logic from running on corrupted
    metadata.
*   **Asynchronous Engine Synchronization (Speculation):** Clearing
    control/enable bits for asynchronous hardware engines (e.g., profiling,
    tracing, or debug extensions) is **insufficient** to stop speculative page
    table walks or memory accesses. When switching translation regimes (e.g.,
    Guest <-> Host switches at EL2), software MUST execute the architecturally
    mandated synchronization sequence for all active engines (e.g., `PSB
    CSYNC` for profiling) followed by a speculative execution barrier (e.g.,
    `SB`, `DSB` + `ISB`) to definitively halt out-of-context execution.
    - **REPORT as bugs:** Code that disables an asynchronous feature but fails
      to execute a subsequent synchronization barrier before changing the
      translation context (e.g., updating `VTTBR_EL2` or `TTBRn_ELx`).
