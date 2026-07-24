# ARM64 Subsystem Details

This guide covers architectural invariants, memory ordering rules, and common
bug patterns for the ARM64 (AArch64) architecture, derived from historical
lore and the ARM Architecture Reference Manual (ARM ARM).

## Grounding: you do not have the ARM ARM

You do not have the ARM ARM. The ARM facts in the loaded arm64/KVM guides are
pre-verified for you. Any ARM fact you cannot find in a loaded guide or show in
the code under review (such as an exact bit position, an ESR exception class or ISS
sub-field, RES0/RES1 polarity, an ordering rule, or what the architecture traps
or routes where or designates as guest-owned versus host-owned) is unverified
recall, and you are reliably wrong on exactly these details. Do not assert
such a detail as fact or build a finding on it, and do not carry a fact from a
different mode or hardware version onto this code. A genuinely suspect patch has
a reason you can quote from a loaded guide or the code: report that, never the
unverified recall.

## ESR_ELx syndrome: exception class, the IL bit, and ISS sub-fields

`ESR_ELx` holds the syndrome of a synchronous exception or an SError, including one that
software synthesizes to inject: the Exception Class `EC` in bits[31:26], the
Instruction Length `IL` in bit[25], and the Instruction Specific Syndrome `ISS`
in bits[24:0] (`ESR_ELx_EC_SHIFT`, `ESR_ELx_IL`, `ESR_ELx_ISS_MASK` in
`arch/arm64/include/asm/esr.h`). The ISS layout is per-EC: the same ISS bit
means different things under different exception classes, so an ISS sub-field
is defined only relative to a stated `EC`. These encodings are the ones the
grounding note warns about, easy to misremember and reliably wrong from recall.
The ones the in-tree code relies on are pre-verified and stated here, so a
finding about them is grounded by quoting this section or the code. A finding
that asserts a different layout from memory is the confabulation to discard.

### The IL bit (bit[25])

`IL` is the instruction-length field. For a trap on a single instruction it is
0 for a 16-bit instruction and 1 for a 32-bit one. The architecture instead
sets `IL` to 1 regardless of the trapping instruction's length for a fixed set
of exceptions: an SError, an Instruction Abort, a PC alignment fault, an SP
alignment fault, a Data Abort with `ISV == 0`, an Illegal Execution State
exception, any debug exception other than a Breakpoint instruction (`BRK` /
`BKPT`) exception, and any exception reported with `EC == 0b000000` (Unknown).
A correct syndrome for any of these has `IL == 1`.

KVM constructs `ESR_ELx` when it injects an exception into a guest or a nested
guest, and the constructed value must carry the architecturally required `IL`.
When the injected exception is one of the kinds above (an injected abort, an
injected undefined or Unknown exception, an injected SError), the syndrome must
set `ESR_ELx_IL`. Omitting it yields a syndrome that does not match what
hardware would have written. This is an architectural requirement, not a
property of any one change, so check the injection paths in the tree under
review and confirm each sets `IL` for the exception kind it raises.

### ISS sub-fields are per-EC

Two exception classes whose ISS the in-tree exception-injection code uses:

- **ERET trap, `EC == 0x1A`** (`ESR_ELx_EC_ERET`, EL2 only: an `ERET`,
  `ERETAA`, or `ERETAB` trapped by `HCR_EL2.NV`):
    - bit[1] `ESR_ELx_ERET_ISS_ERET` (`0x2`): 0 = plain `ERET`, 1 = an
      authenticating `ERETAA` / `ERETAB`.
    - bit[0] `ESR_ELx_ERET_ISS_ERETA` (`0x1`): which key an `ERETAx` used,
      0 = A key (`ERETAA`), 1 = B key (`ERETAB`). RES0 when bit[1] is 0.
    - bits[24:2] RES0.

  In-tree, `esr_iss_is_eretax()` tests bit[1] and `esr_iss_is_eretab()` tests
  bit[0].
- **PAC Fail (FPAC), `EC == 0x1C`** (`ESR_ELx_EC_FPAC`: a pointer-authentication
  failure, raised when `FEAT_FPAC` is implemented). The ARM ARM names its two
  ISS bits:
    - bit[1] `DnI`: which key class failed, 0 = Instruction key, 1 = Data key.
    - bit[0] `BnA`: which key failed, 0 = A key, 1 = B key.
    - bits[24:2] RES0.

### Worked example: an FPAC syndrome built from a failed ERETAx (do NOT flag)

When an emulated `ERETAA` / `ERETAB` fails to authenticate and an FPAC exception
is delivered, KVM (`kvm_emulate_nested_eret()`,
`arch/arm64/kvm/emulate-nested.c`) reuses the ERET-trap syndrome it already
holds, masking the ISS down to the bits that carry over and re-stamping the
`EC`. The correct construction is:

```c
esr &= (ESR_ELx_ERET_ISS_ERETA | ESR_ELx_IL);
esr |= FIELD_PREP(ESR_ELx_EC_MASK, ESR_ELx_EC_FPAC);
```

A finding that this builds the FPAC ISS incorrectly is the confabulation to
discard. Read it against the two ISS layouts above:

- ISS bit[0] lines up. Under `EC == 0x1A` it is `ERETA` (A vs B key), under
  `EC == 0x1C` it is `BnA` (A vs B key): same position, same polarity (0 = A,
  1 = B). Keeping bit[0] carries the failing key's A/B sense into the FPAC
  syndrome unchanged.
- ISS bit[1] is cleared, so FPAC `DnI == 0` (Instruction key). That is correct:
  `ERETAA` / `ERETAB` authenticate the return address with the instruction keys
  `APIAKey_EL1` / `APIBKey_EL1`, so a PAC Fail from an `ERETAx` is always an
  instruction-key failure.
- `IL` is kept. An `ERET` / `ERETAx` is a 32-bit A64 instruction, so its trap
  syndrome has `IL == 1`, which is also the `IL` the resulting FPAC exception
  requires.

The general check: when code turns one EC's syndrome into another's by masking
and re-stamping the `EC`, a carried-over ISS bit is correct exactly when its
position and polarity match between the two layouts. That is verifiable from
the encodings, with no recall.

**REPORT as bugs:**
- A synthesized `ESR_ELx` for an injected exception in the `IL == 1` list above
  (an abort, an Unknown or undefined exception, an SError) that does not set
  `ESR_ELx_IL`.
- An ISS bit carried unchanged across exception classes when, in the destination
  class, that bit holds a different field, has opposite polarity, or is RES0.

**Do NOT flag:**
- The FPAC-from-`ERETAx` construction above. Bit[0] (A/B key) and `IL` carry
  over correctly, and `DnI` is correctly 0.
- A claim that an ISS sub-field sits at a different bit, or has opposite
  polarity, than stated here when that claim rests only on recall. That is the
  unverifiable-spec confabulation the grounding note rules out.

## Memory Tagging Extension (MTE) and Tagged Addresses

Mishandling MTE tags or tagged addresses causes `KASAN invalid-access` panics,
spurious tag check faults, and memory permission corruption.

- **Pointer Arithmetic:** The Top Byte Ignore (TBI) hardware feature allows
  the MMU to ignore bits [63:56] during address translation. However,
  **software-side arithmetic** (e.g., bit-shifts to compute page indices) MUST
  explicitly untag the address (e.g., via `untagged_addr()`) to prevent tag
  bits from corrupting calculation results.
    - **Note:** If `FEAT_PAuth` is implemented, `TCR_ELx.TBIDn` selects
      whether TBI applies to instruction fetches as well as data; otherwise
      TBI applies to both unconditionally.
- **Tag Initialization Barriers:** A DSB between tag stores (`STG`/`STZG`) and
  the PTE update is required so the explicit tag stores complete before the
  table walker's implicit TTD reads observe the mapping.
- **Shared Page Tag Initialization:** Marking a shared or special page (e.g.,
  the **Linux-specific** huge zero folio) as MTE-tagged too early during
  initialization causes userspace to map the page before its tags are
  definitively cleared. Tag clearing MUST be synchronized and visible before
  the page is made accessible to other observers.

**REPORT as bugs:**
- Using a virtual address directly in a bit-shift or mask operation to find a
  page-table entry without calling `untagged_addr()`.
- Updating a PTE for an MTE-enabled page without a preceding `DSB` after the
  tags were written to ensure visibility to the walker.

## System Register and Context Synchronization

Updates to architectural configuration system registers (e.g., `GCR_EL1`,
`SCTLR_EL1`, `TCR_EL1`) are not guaranteed to be visible to subsequent
instructions without an explicit Context Synchronization Event (CSE).

Missing synchronization results in unpredictable behavior where the CPU
operates under a stale configuration for several cycles, breaking memory
safety semantics or causing unexpected traps.

- **Context Synchronization Events (CSE):** A CSE ensures preceding state
  changes are resolved. Events that constitute a CSE include:
    - Executing an `isb` instruction.
    - Exception entry or return (subject to `SCTLR_ELx.{EIS, EOS}` bits and
      `FEAT_ExS`).
- **Synchronize before an *indirect* read relies on the effect; a plain
  direct read-back does not need one.** The architecture does not require
  an `isb()` after every control-register write. Whether a Context
  Synchronization Event (CSE) is needed is keyed to *how* the new value is next
  used (Arm ARM DDI 0487, D24.1.2.2, Table D24-1, for two accesses to the same
  register):
    - **Direct read-back — an `MRS` of the register just written — needs no
      CSE.** A direct write is ordered before a later direct read of the same
      register with no synchronization ("Direct write -> Direct read: None"),
      so the read-back returns the written value. (Exception: a set/clear
      register (`*_SET`/`*_CLR`, e.g. `PMOVSSET_EL0`); the spec defines its
      write *effect* as an indirect write, so a read-back needs a CSE
      — "Indirect write -> Direct read: Required". Ordinary RW trap/control
      registers are not set/clear.)
    - **Indirect read — the register's *effect* governing a later instruction
      (a trap taking effect, a translation using a new `TTBR`/`TCR`, an FP
      instruction gated by `CPTR`) — requires a CSE** between the write and
      that instruction ("Direct write -> Indirect read: Required"), unless the
      register/field is self-synchronizing (see below).
  A read-back confirms the *stored value*; it does not confirm the *effect* is
  active. Different questions, different answers.
- **The CSE need not be an explicit `isb()` you add; it can be one already on
  the path.** An exception return (`ERET`) or exception entry is a CSE, unless
  `FEAT_ExS` is implemented and the relevant enable is cleared (`SCTLR_ELx.EOS
  == 0` for the return, `SCTLR_ELx.EIS == 0` for the entry), in which case that
  boundary is not a CSE and the effect must be synchronized explicitly. In the
  common case, when the instruction that relies on the effect runs at a
  *lower/other* Exception level, the `ERET`/exception-entry that transfers there
  supplies the CSE, and no separate `isb()` after the write is needed.
- **Ask who relies on the value and how, not which register it is.** The same
  register can need an `isb()` on one path and not another. `CPTR_EL2` traps
  FP/SVE at EL2, EL1 and EL0 alike, yet it is written with no `isb()` when
  *activating* guest traps: EL2 executes no FP/SVE on that path before the
  `ERET`, so nothing in-context relies on the new value and guest entry is the
  CSE. Writing it to *deactivate* those traps so **EL2 itself** may touch
  FP/SVE, by contrast, requires an `isb()` after that write and before EL2's
  FP/SVE access — an in-context indirect read (upstream `257d0aa8e250` added
  exactly that barrier to fix a host SVE-trap crash). Identical register,
  opposite requirement, decided by the consumer.
- **Counter-exception — a value the `ERET` itself indirectly consumes.** An
  `ERET` does not cover every preceding write. Some `HCR_EL2` fields are read
  by the `ERET` while it constructs the target context, so their new value is
  relied upon *before* the `ERET`'s own synchronization point; those need an
  `isb()` before the `ERET`. The Arm ARM names this: "changes to some fields of
  `HCR_EL2` at EL2 need an explicit ISB in program order before an ERET
  instruction" (D24.1.2.2 Note).
- **GIC Synchronization:**
    - Writes to most `ICC_*_EL1` registers require a CSE to be visible to
      subsequent instructions. Notable exceptions: writes to `ICC_PMR_EL1` and
      reads of `ICC_IAR{0,1}_EL1` / `ICC_NMIAR1_EL1` (when `PSTATE.{I,F} ==
      {0,0}`) are self-synchronizing.
    - Writes to specific memory-mapped GIC registers require polling the RWP
      bit. `GICD_CTLR.RWP` tracks group-enable disables (1→0), `GICD_CTLR`
      ARE/DS field writes, and `GICD_ICENABLER<n>`. `GICR_CTLR.RWP` tracks
      `GICR_ICENABLER0`, `GICR_CTLR.EnableLPIs` (1→0), and DPG writes.
      Priority, routing, and enable-set writes are NOT tracked by either RWP
      bit.
- **Self-Synchronizing Registers and Fields (no ISB needed):** The
  indirect-read rule above (an effect relied upon by a later instruction needs a
  CSE) has architecturally-defined exceptions: some System register
  effects are guaranteed visible to subsequent instructions in program order
  *without* a CSE. The architecture grants this in more than one way, so
  recognize both forms:
    - **SVE/SME effective vector-length fields** (`ZCR_ELx.LEN`, and likewise
      `SMCR_ELx.LEN`): a write takes effect for the following SVE/SME
      instructions in program order with no intervening `isb()`. The ARM ARM
      flags this in the field description with wording of the form "an indirect
      read of `<REG>.<FIELD>` appears to occur in program order relative to a
      direct write of the same register, without the need for explicit
      synchronization." Recognize this class by that wording — do not assume
      every sysreg either always needs an `isb()` or never does.
    - **`FPMR`** (FP8 mode register, `SYS_FPMR`): self-synchronizing at *register*
      granularity — "a direct or indirect read of this register occurs in program
      order relative to a direct write of this register without explicit
      synchronization" (Arm ARM C5.2.9, Configuration). No `isb()` is needed
      between an `FPMR` write and a following FP8 instruction. Its wording omits
      the "appears"/"need for" phrasing of `ZCR/SMCR.LEN`, so match `FPMR` by name,
      not by that exact pattern.
    - **`ICC_PMR_EL1`** (GIC priority mask, noted above): self-synchronizing by
      a separate guarantee in the GIC architecture, *not* by the sysreg wording
      above. After the write is architecturally executed, no interrupt below the
      new priority is taken, without requiring an `isb()` or exception boundary. Same
      no-`isb()` outcome, different mechanism — so do not expect to find the
      "indirect read ... in program order" wording on it. This no-`isb()`
      guarantee is the *masking* direction (writing `ICC_PMR_EL1` to block
      lower-priority interrupts, as on `local_irq_disable()`). The *unmasking*
      direction (relaxing the mask to admit them again, as on
      `local_irq_enable()`) does need a barrier — `pmr_sync()`, a `dsb` gated on
      `ICC_CTLR_EL1.PMHE` (boot-time-patched to a nop when `PMHE == 0`, and
      compiled out entirely without `CONFIG_ARM64_PSEUDO_NMI`), never an `isb()`.
      So do not read this as "`ICC_PMR_EL1` writes never need synchronization":
      flag a missing `pmr_sync()` on an unmask path, though still never a missing
      `isb()`.
  Flagging a missing `isb()` after a write to a self-synchronizing field (e.g.
  `ZCR_ELx.LEN`) or register (e.g. `FPMR`) is a false positive.

**REPORT as bugs:**
- An *in-context indirect read* with no CSE before it: the writing EL's own
  next instructions rely on the register's *effect* with no `isb()` between.
  Examples: disabling a trap and then the current EL executing the
  now-untrapped instruction (the `CPTR_EL2` deactivation case, upstream
  `257d0aa8e250`); writing `TCR_EL1`/`TTBR_ELx` then a subsequent memory access
  that uses the new translation. Note the reliance is on the *effect*, not on a
  read-back.
- A change to a register field that the `ERET` itself indirectly consumes
  (certain `HCR_EL2` fields) with no explicit `isb()` between the write and the
  `ERET`. The `ERET` is not sufficient here: it reads these fields while
  constructing the target context, before its own synchronization point, so the
  Arm ARM requires "an explicit ISB in program order before an ERET
  instruction" (D24.1.2.2 Note).
- Writing to `ICC_*_EL1` registers (excluding `ICC_PMR_EL1`) without an
  `isb()`.
- Writing to tracked memory-mapped GIC registers without polling the
  appropriate `GICD_CTLR.RWP` or `GICR_CTLR.RWP`.

**Do NOT report (false positives):**
- A missing `isb()` between a direct write and a direct `MRS` read-back of the
  *same* register. Table D24-1 requires none; the read-back returns the written
  value. The read-back below is correct with no CSE before it:
    ```c
    write_sysreg_s(val, SYS_HFGRTR_EL2);
    if (read_sysreg_s(SYS_HFGRTR_EL2) != val)   /* direct read-back: returns val, no CSE */
        return -EIO;
    ```
  `HFGRTR_EL2` controls fine-grained traps taken at EL1/EL0, so its *effect* is
  relied upon only after guest entry; the guest-entry `ERET` (a CSE) provides
  the synchronization, and no `isb()` at EL2 is required for the traps either.
- A missing `isb()` after a control-register write whose effect only governs a
  lower/other Exception level or a different execution state, where an
  `ERET`/exception boundary that is a CSE intervenes before the effect is relied
  upon: guest trap/enable bits set on `vcpu_load` or the world-switch path (e.g.
  the `CNTHCTL_EL2` `EL1*` guest bits written in `timer_set_traps()`, `CPTR_EL2`
  when *activating* guest traps), or `FPEXC32_EL2` (relevant only to an AArch32
  guest; the guest-entry `ERET` synchronizes it — upstream `b1a9a9b96169`
  removed the redundant `isb()`). Judge by the consumer, not the register name.
  (This assumes the boundary is a CSE, i.e. not the `FEAT_ExS` +
  `SCTLR_ELx.EOS`/`EIS == 0` case above.) Beware the same register on a
  different path: under VHE the `CNTHCTL_EL2` `EL0*` bits (`EL0PCTEN` etc.,
  gated by `HCR_EL2.TGE == 1`) govern the *host's own* EL0, an in-context
  consumer — a write the host relies on without an intervening `ERET` does need
  synchronization. Same register, opposite answer, per the consumer.
- A missing `isb()` after a write to a self-synchronizing register/field
  (`ZCR_ELx.LEN`, `SMCR_ELx.LEN`, `FPMR`, or `ICC_PMR_EL1` on the mask path);
  covered by the self-synchronizing-registers rule above.

## Auto-Generated Definitions and Semantic Drift

Hardware definitions are increasingly moved from hand-rolled C macros to
auto-generated infrastructure (e.g. the `.sysreg` files consumed by
`gen-sysreg.awk`). The generator's contract is to reflect *global architectural
truth*, not whatever software context the old C macros had baked in. A
declarative change can therefore silently mutate the *value* of an
auto-generated aggregate mask while leaving its *name* unchanged: the compiler
stays silent, and standard CI stays silent because the build holds only the new
snapshot, not the old one. Catching this is by design a code-review
responsibility, not a tooling one.

**Worked example.** `<REG>_RES1` is generated *only* from literal `Res1 N`
lines in that register's `.sysreg` block (`gen-sysreg.awk`); it has no concept
of features and defaults to `UL(0)` when the block declares no `Res1` bit. So a
`.sysreg` edit that reclassifies a bit between `Res1 N` and `Field N` (or
`Res0`) — adding a newly architected RES1 bit, or turning an existing RES1 bit
into a writable field — silently changes the *value* of `<REG>_RES1` while its
*name* stays put. `SCTLR_EL2_RES1` is a live consumer: today it expands to
`UL(0)` and is OR'd into the EL2 SCTLR init values `INIT_SCTLR_EL2_MMU_ON` /
`INIT_SCTLR_EL2_MMU_OFF` (`arch/arm64/include/asm/sysreg.h`). If a future
`.sysreg` change added or dropped a `Res1` line in the `SCTLR_EL2` block, those
init values would change with no edit at the consuming macro and no compiler or
CI signal.

Do not look to the generated mask for feature conditionality — it is not there.
"RES1 only when a feature is absent" (e.g. `SCTLR_ELx.{EIS, EOS}` are RES1 when
`FEAT_ExS` is unimplemented) lives in KVM's runtime feature map, tagged
`AS_RES1` ("RES1 when not supported") in `arch/arm64/kvm/config.c`, not in
`<REG>_RES1`. And do not pin the check to an init macro's *current* contents
either: both the value of a `_RES1` aggregate and the explicit field-macro terms
an init ORs in (e.g. `SCTLR_ELx_EIS` / `SCTLR_ELx_EOS`) drift between releases.
`INIT_SCTLR_EL2_MMU_ON` is a live example — it set neither `EIS` nor `EOS` in one
release and forces both, via the field macros, in a later one. Read the consuming
init at the revision under review, not from memory.

- **Trigger.** A patch touches a `.sysreg` file so as to allocate bits or
  change a field's RES0 / RES1 classification, regenerating an aggregate
  mask (`<REG>_RES0`, `<REG>_RES1`, and similar).
- **Check.** Identify the auto-generated aggregate macros whose value the
  change alters, then enumerate the C call-sites that consume those macros and
  inspect each.
- **Action.** Raise an Open Question / Tension asking the author to confirm the
  consuming C does not rely on the *previous* semantic value of the mask — in
  particular, code that ORs `<REG>_RES1` into an initial register value
  expecting specific bits to be set.

This generalizes beyond `.sysreg` to any auto-generated hardware-definition
infrastructure where a declarative update silently changes
the value of a consumed macro.

**REPORT as a tension (Open Question):**
- A `.sysreg` / declarative change that regenerates an aggregate mask consumed
  by C initialization, without confirmation that the consumer is robust to the
  mask's value changing.

## TLB Invalidation and Break-Before-Make (BBM)

Failure to follow the correct invalidation and synchronization sequence leads
to pipeline inconsistencies, stale TLB usage, and TLB Conflict Aborts.

### TLB Maintenance Observer Rules

The completion of a TLB maintenance instruction (`TLBI`) is guaranteed
**only** by the execution of a `DSB` by the **same** Processing Element (PE)
that performed the `TLBI`.

- **Global Visibility:** A broadcast `TLBI` (e.g., `TLBI VAE1IS`) is only
  guaranteed to be finished for all other PEs after the issuing PE executes a
  `DSB ISH`.
- **Remote DSB Inefficacy:** CPU B cannot use its own `DSB` to force CPU A's
  broadcasted `TLBI` to complete.
- **Local Synchronization:** `isb` instructions are NOT broadcast. Each
  observing PE MUST independently execute its own `isb()` (or undergo a CSE)
  locally *after* the issuing PE's `DSB` completes to ensure the invalidation
  is visible to the local fetch path.

### Break-Before-Make (BBM) Requirements

When updating a live translation table entry (shared across multiple threads),
you MUST follow the BBM sequence to prevent TLB conflicts.

**BBM is strictly required when:**
- **Changing block or table sizes:** `FEAT_BBML1/2` relax the requirement for
  an explicitly-invalid intermediate descriptor; TLB maintenance is still
  required.
- **Creating a global entry** that overlaps existing non-global entries.
- **Changing the Output Address (OA):** Strictly required by the architecture
  if the contents of memory at the new OA do not match the contents at the
  previous OA.
- **Changing memory attributes** (type or cacheability).

**The BBM Sequence:**
1. Replace the old entry with an invalid entry.
2. Execute a `DSB` (ensure invalid entry is globally visible).
3. Invalidate relevant TLB entries (Broadcast `TLBI`).
4. Execute another `DSB` (ensure invalidation is complete).
5. Write the new, updated translation table entry.
6. Execute a final `DSB`.

**REPORT as bugs:**
- Code performing `TLBI` without both a subsequent `DSB` and `isb()` on the
  issuing CPU (for executable mappings or where local synchronization is
  required).
- Missing `isb()` after TLBI in mode-entry paths (e.g., `enter_vhe()`, nVHE
  `__tlb_switch_to_guest()`, `__primary_switch()`); the TLBI is not
  synchronized to the new execution context without it.
- Updating a live page table entry (changing OA to a non-matching address or
  attributes) without an intervening invalidation (skipping the "Break" step).
- Kernel block/page-mapping changes on systems where secondary CPUs may not
  support `FEAT_BBML2`, without gating on the CPU-feature cap or falling back
  to full BBM.

## Instruction and Data Coherency (PoC vs PoU)

The architecture does not inherently ensure coherency between instruction
caches and memory. Software must manage this manually using the Point of
Unification (PoU) or Point of Coherency (PoC).

- **Self-Modifying Code (PoU):** When writing new instructions as data (e.g.,
  JIT), software MUST:
    1. Clean the data cache to the PoU (`DC CVAU`).
    2. Execute `DSB ISH`.
    3. Invalidate the instruction cache to the PoU (`IC IVAU`).
    4. Execute `DSB ISH`.
    5. Ensure an `isb()` occurs on **all observing CPUs** (e.g., via IPI).
- **Instruction Patching (CMODX):** Concurrent modification and execution of
  instructions is safe only for the architecturally enumerated CMODX set: `B`,
  `B.cond`, `BL`, `BRK`, `CB<cc>`, `CBB<cc>`, `CBH<cc>`, `CBNZ`, `CBZ`, `HVC`,
  `ISB`, `NOP`, `SMC`, `SVC`, `TBNZ`, `TBZ`, `TRCIT`, and `UDF`. For all other
  instructions, an explicit `isb()` or CSE is mandatory on **all observing
  CPUs** before execution.
- **External Agents (PoC):** Communicating with non-coherent DMA controllers
  or managing mismatched memory attributes requires cleaning/invalidating to
  the Point of Coherency (PoC) (e.g., `DC CVAC`).

**REPORT as bugs:**
- Modifying instructions (e.g., jump label patching) without ensuring an
  `isb()` or CSE occurs on all CPUs executing the modified code.

## Lockless Page Table Walks

Lockless walkers (e.g., GUP or fast-path page faults) must carefully manage
compiler ordering to avoid observing inconsistent page table states.

- **Atomicity and Ordering:** Software MUST use `READ_ONCE()` when loading a
  descriptor from a shared page table in a lockless walk to prevent the
  compiler from splitting the load or reordering it against subsequent logic.
- **Folded Level Handling:** When some page-table levels are statically folded
  (`PGTABLE_LEVELS ≤ 2`), lockless walks must account for the folded topology.
  The standard multi-level `READ_ONCE()` pattern applied at a folded level
  will observe stale or incorrect state.

**REPORT as bugs:**
- Dereferencing a shared PTE/PMD/PUD/PGD pointer in a lockless walk without
  using `READ_ONCE()`.
- Lockless walk logic that assumes all page-table levels are live without
  checking for folded-level conditions.

## Exception Handling and Stack Management

Manipulating the Stack Pointer (`SP`) is architecturally hazardous due to
potential clobbering of exception return state (`ELR_ELx`, `SPSR_ELx`).

- **DAIF Masking:** Asynchronous exceptions (IRQ, FIQ, SError) MUST be masked
  during stack transitions or pivots (e.g., when switching to a
  **Linux-specific** Shadow Call Stack). An exception hitting while `SP` is
  being moved or state is out of sync will cause fatal recursive faults.
- **Stack Alignment:** `SCTLR_ELx.SA` enforces 16-byte SP alignment for memory
  accesses at ELx; `SCTLR_EL1.SA0` controls the same check at EL0. Check is on
  memory access via SP, not on SP modification.

**REPORT as bugs:**
- Manipulating `SP` or switching stacks without masking `DAIF`.

## SVE, SME, and FPSIMD Register State

Vector-length changes and signal-return paths have a history of leaving stale
register state or performing incorrect state merges.

- **VL-Change State Invalidation:** When the SVE or SME vector length is
  changed for a task, any context derived from the old VL (buffers, register
  views, ptrace payloads) MUST be invalidated or rebuilt before execution
  continues. Incomplete invalidation resurrects stale data in the new
  vector-width context.
- **Streaming-Mode SVE Payload:** When entering streaming SVE mode, the
  ptrace/signal interface requires an explicit SVE payload for streaming-mode
  state; inheriting previous non-streaming state is architecturally incorrect.
- **FPSIMD/SVE State Merge on Signal Return:** When returning from a signal
  handler, FPSIMD and SVE state must be merged in a single coherent step.
  Partial or incorrectly ordered merges silently corrupt the Z-register upper
  halves.

**REPORT as bugs:**
- VL-change paths that do not invalidate or rebuild SVE/SME register context
  for the new length.
- Signal-return paths that merge FPSIMD and SVE register state incorrectly or
  in multiple non-atomic steps.

## Quick Checks

- **System Instructions with Fixed Register Requirements:** Inline assembly
  for system instructions that demand specific registers (e.g., `GIC CDEOI`
  requiring `XZR` / register 31) MUST hardcode the exact register in the
  instruction string. Relying solely on compiler constraints (like `r`) can
  lead to misencoded instructions if the compiler selects a general-purpose
  register, causing `CONSTRAINED UNPREDICTABLE` behavior.
- **CONSTRAINED UNPREDICTABLE:** Triggered by invalid encodings or register
  overlaps. Hardware may execute as a `NOP`, treat as `UNDEFINED`, or generate
  `Alignment/MMU faults`. It results in **UNKNOWN** state and MUST NOT be
  relied upon.
- **TLBI Range Operands:** Range-based invalidation (e.g., `TLBI RVAE1IS`)
  requires correct `SCALE` and `NUM` encoding. If `TG` does not match the
  current granule, the TLBI is CONSTRAINED UNPREDICTABLE — possibly including
  no invalidation.
- **PTE Barrier Batching:** Batching DSB/ISB across multiple kernel-mapping
  PTE updates is only correct in contexts that cannot be interrupted
  mid-batch. In interrupt contexts the batching window is broken and an
  explicit barrier must be issued before the interrupted path observes the
  mappings.
