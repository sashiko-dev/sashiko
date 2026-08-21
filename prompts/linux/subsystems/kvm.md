# KVM Subsystem Details

This guide covers cross-architecture KVM invariants, locking hierarchies, and
memory management API contracts derived from `Documentation/virt/kvm/` and
historical fixes.

## API and ABI Quick Checks

Before diving into the locking and MMU details below, scan the diff for the
following cross-cutting issues. They come up repeatedly across KVM subsystems
and are easy to miss in a focused review:

- **New guest-visible features default off and are enumerable.** Any new
  behaviour the guest can observe (a new ioctl, a new VM or vCPU capability, a
  new exit reason, a new emulated instruction) MUST be off by default and
  discoverable through the architecture's standard enumeration interface, for
  example `KVM_CHECK_EXTENSION` / `KVM_CAP_*`, `KVM_GET_SUPPORTED_CPUID2` on
  x86, or ID-register feature bits on ARM64. Silently-on features break live
  migration and make capability negotiation impossible.
- **No guest- or host-userspace-reachable `WARN_ON` / `BUG_ON`.** A `WARN_ON`
  or `BUG_ON` whose condition can be driven by a malicious guest or by an
  unprivileged host-userspace process is a host-side denial of service.
  Convert to `pr_warn_once()`, return an error to userspace, or just drop the
  assertion. Asserts are fine for "the kernel itself is buggy" paths, not for
  adversary-reachable inputs.
- **Long loops are a recurring bug class, but the fix is flow-specific.**
  Loops over guest-driven counts (memslots, vCPUs, GFN ranges,
  rmaps/SPTEs, pinned pages) with per-iteration MMU activity (invalidate,
  zap, clflush, unmap) have a long history of soft-lockup and RCU-stall
  fixes. There is no universal mitigation: yielding under `kvm->mmu_lock`
  can hurt fault throughput, and in extreme cases dropping the lock and
  forcing retries can starve the guest. Treat this as background context
  for new long-running paths, not a checklist item demanding
  `cond_resched()` or any other specific yield mechanism.
- **New memslot and vCPU flags default to immutable.** A flag that can be
  cleared or flipped after it is first set creates state-machine transitions
  that almost no caller is prepared for; recent issues around
  `KVM_MEM_GUEST_MEMFD` and post-`KVM_ARM_VCPU_INIT` vCPU-model changes are
  concrete examples. Default new flags to set-once and require an explicit
  justification to make them mutable.

## KVM Locking Hierarchy

KVM uses a complex hierarchy of global and per-VM locks. Violating this order
results in circular deadlocks or use-after-free (UAF) scenarios.

> Improper lock ordering causes **ABBA Deadlocks** and **System Hangs**.

The authoritative lock ordering is `Documentation/virt/kvm/locking.rst`. Read
it whenever a change touches `kvm_lock`, `kvm->lock`, `vcpu->mutex`,
`kvm->slots_lock`, `kvm->srcu`, or `kvm->mmu_lock`. Do not rely on this guide
as a substitute. The bullets below capture the specific failure modes a
reviewer should flag, anchored on that doc.

- **SRCU Constraint:** `synchronize_srcu(&kvm->srcu)` is invoked while holding
  `kvm->lock`, `vcpu->mutex`, or `kvm->slots_lock`. Consequently, **none** of
  these mutexes may be acquired while holding a `srcu_read_lock(&kvm->srcu)`.
- **`slots_arch_lock` Exception:** The architecture-specific memslot lock
  (e.g., in `arch/arm64/kvm/`) is typically NOT involved in SRCU
  synchronization and MAY be acquired inside an SRCU read-side critical
  section.
- **MMU Notifier Sleep Safety:** MMU notifier callbacks
  (`invalidate_range_start` / `_end`) MUST NOT take `kvm->slots_lock` or
  `kvm->slots_arch_lock`, since memslot modification waits on those locks from
  inside the notifier quiescence.

**REPORT as bugs:**
- Acquiring `kvm->lock`, `vcpu->mutex`, or `kvm->slots_lock` inside an SRCU
  read-side critical section.
- Holding `vcpu->mutex` and then attempting to acquire the parent `kvm->lock`.
- Performing any sleepable operation (`kzalloc` without `GFP_ATOMIC`,
  `mutex_lock`, `copy_from_user`) while holding `kvm->mmu_lock`.

## Memory Management and Memslots

KVM manages guest memory through "memslots." Accessing these without proper
SRCU protection or synchronization leads to **Use-After-Free (UAF)** or stale
translation usage.

> Failure to protect memslot iteration results in **Kernel Panics** (UAF) or
> **Silent Data Corruption** during VM migration.

- **Memslot Consistency (Read-Side):** Accessing guest memory mapping metadata
  (memslots) from the fast path (e.g., page faults or instruction emulation)
  requires a non-preemptible or RCU-protected environment. This ensures that
  the reader never observes a partially updated or freed mapping structure
  during a concurrent memslot update. In KVM, this is achieved by holding
  `kvm->srcu`.
- **Writer Context Exception:** Access without SRCU is permitted only if
  **`kvm->slots_lock`** is held (e.g., during memslot updates).
- **Update API:** All changes to memslots (flags, address ranges) MUST go
  through `kvm_set_memory_region()`. Manual modification of memslot structures
  is forbidden.

**REPORT as bugs:**
- Accessing memslots or calling address translation helpers (e.g.,
  `gfn_to_hva()`) without holding `kvm->srcu` (unless in a writer context with
  `slots_lock`).
- Manual bit-flipping in memslot flags (e.g., `KVM_MEM_LOG_DIRTY_PAGES`)
  outside the official update path.

```c
// WRONG: Accessing memslots without SRCU protection
struct kvm_memslots *slots = kvm_memslots(vcpu->kvm);
hva = __gfn_to_hva_memslots(slots, gfn); // Potential UAF

// CORRECT: Protecting with SRCU
int idx = srcu_read_lock(&kvm->srcu);
struct kvm_memslots *slots = kvm_vcpu_memslots(vcpu);
hva = __gfn_to_hva_memslots(slots, gfn);
srcu_read_unlock(&kvm->srcu, idx);
```

## Invalidation Retry Protocol (MMU Notifiers)

When KVM handles a page fault, it must synchronize with concurrent host MMU
invalidations to ensure it does not install a stale mapping.

> Missing the retry check allows KVM to install **Stale Translations**,
> leading to memory corruption or guest-visible data leakage.

- **The Mandatory Sequence:**
    1. **Capture:** Store the current `mmu_invalidate_seq`.
    2. **Resolve:** Translate the guest address (HVA/GPA) to a physical frame
       (PFN).
    3. **Lock:** Acquire the `kvm->mmu_lock`.
    4. **Check:** Verify `!mmu_invalidate_retry(kvm, captured_seq)`.
    5. **Install:** Commit the mapping to KVM's page tables.
    6. **Unlock:** Release `kvm->mmu_lock`.
- **Generation Count Invariant:** The retry check combines the global
  `mmu_invalidate_seq` with the in-progress gfn range; neither alone is
  sufficient. The sequence counter is the primary generation-safety check; the
  gfn range reduces false-positives.
- **Locking Invariant (gating check):** The retry check that gates
  installation MUST be performed while `kvm->mmu_lock` is held, and the lock
  MUST remain held until the translation is fully installed. Pre-acquisition
  "unsafe" retry checks via `mmu_invalidate_retry_gfn_unsafe()` are a
  legitimate fast-path optimization to avoid lock contention; they do NOT
  replace the gating check, and reviewers should not flag them as bugs.

**REPORT as bugs:**
- Installing a mapping based only on the result of an unsafe retry check,
  without re-checking under `kvm->mmu_lock`.
- Resolving the PFN *before* capturing the sequence or *after* acquiring the
  lock (violates sequence-to-resolution-to-installation ordering).
- Dropping `kvm->mmu_lock` between the retry check and the installation of the
  page table entry.

## VCPU Lifecycle and Preemption

`vcpu_load()` and `vcpu_put()` manage the attachment of a virtual CPU to a
physical CPU.

> Failing to manage VCPU attachment leads to **Leaked Preempt Notifiers**,
> causing NULL dereferences in the scheduler, and **Hardware State
> Corruption**.

- **Mandatory Usage:** These MUST be paired and are mandatory for operations
  interacting with physical-CPU-dependent state (hardware registers, timers,
  preempt notifiers).
- **Side Effects:** `vcpu_load()` registers preempt notifiers. Failing to call
  `vcpu_put()` leads to leaked notifiers and `NULL` pointer dereferences in
  `kvm_sched_out()`.

**REPORT as bugs:**
- Missing `vcpu_load()` or `vcpu_put()` around KVM IOCTLs that modify
  architectural or hardware-switched state.

## Quick Checks

- **VCPU Requests:** `kvm_make_request()` internally issues `smp_wmb()` before
  setting the request bit, paired with `smp_mb__after_atomic()` in
  `kvm_check_request()`. Callers MUST NOT add manual barriers around
  `kvm_make_request()`. The IPI-kick path (`kvm_vcpu_kick`) pairs with a full
  `smp_mb()` and the vcpu's `IN_GUEST_MODE` flag — see
  `Documentation/virt/kvm/vcpu-requests.rst` "Ensuring Requests Are Seen".
- **Updates to hardware-shared SPTE bits preserve concurrent hardware
  writes.** When KVM updates SPTE bits that the hardware page-table walker
  also writes (Dirty/Access on leaf entries), the update must not clobber a
  concurrent hardware update. Atomic single-instruction operations (e.g., x86
  XCHG, atomic AND, `cmpxchg`) are all acceptable; a `cmpxchg` loop is not
  required. Non-leaf SPTEs for bits the architecture doesn't currently track
  (e.g., Access on non-leaf SPTEs in KVM x86) can use plain writes. GICv4
  virtual pending state is ARM64-specific; see kvm-arm64.md.
- **cpus_read_lock() placement:** `cpus_read_lock()` is the outermost KVM
  lock. Taking it outside `kvm_lock` is the official ordering, but it is
  easily triggered inadvertently while holding `kvm_lock`. Reviewers should
  flag any new path that calls `cpus_read_lock()` with `kvm_lock` already
  held.
- **MMU Notifier Pairing Invariant:** Each `invalidate_range_start()` is
  paired with exactly one `invalidate_range_end()` using the same memslots
  array. The retry-protocol sequence counter depends on this pairing; broken
  pairing produces false "retry not needed" results.

## Dirty Ring and Dirty Bitmap

KVM supports two dirty-tracking mechanisms (ring and bitmap) that can run
concurrently. Incorrect synchronization between them leads to lost dirty
information or NULL pointer dereferences.

- **Ring-to-Bitmap Flush:** When the dirty ring is full, dirty information
  MUST be flushed unconditionally to the backup bitmap before the ring is
  cleared. Conditional flushes can silently lose pages whose dirty entries
  were overwritten.
- **Bitmap Range Alignment:** `KVM_CLEAR_DIRTY_LOG` operates on a bitmap range
  that must be aligned to 64 bits; unaligned ranges corrupt adjacent dirty
  state.

**REPORT as bugs:**
- Dirty-ring flush paths that conditionally skip the bitmap backup.
- `KVM_CLEAR_DIRTY_LOG` handlers that do not check for bitmap-size alignment.

## pfn Cache and Private Memslots

- **gfn→pfn Cache Refresh Protocol:** The `gfn_to_pfn_cache` refresh uses the
  same sequence-counter discipline as the MMU notifier retry protocol. Cache
  refresh MUST re-check `mmu_invalidate_seq` after pinning the page to close
  the TOCTOU window. Multiple concurrent invalidations can overlap during the
  pin; the sequence counter catches all of them.
- **Private Memslot Overlap:** Public and private (guest_memfd) memslots MUST
  NOT overlap in GPA space. Overlap detection MUST be enforced during
  `KVM_SET_USER_MEMORY_REGION2` handling; overlapping private/public memslots
  produce UAF or silent data corruption during fault injection.

**REPORT as bugs:**
- `gfn_to_pfn_cache` refresh paths that do not re-check the invalidation
  sequence after pinning.
- `KVM_SET_USER_MEMORY_REGION2` handlers that permit GPA overlap between
  private and public memslots.
