# MIPS Subsystem Details

## TLB Duplicate Entry Hazards

Using content-addressed TLB operations during early initialization can trigger
TLB shutdown (machine check exception) on multiple MIPS CPU families. TLB
shutdown sets the `ST0_TS` bit in `CP0_Status` and is fatal — the processor
halts and the kernel's `do_mcheck()` handler in `arch/mips/kernel/traps.c`
cannot recover.

**Affected CPU families:**

TLB shutdown on duplicate entries affects R4x00, microAptiv/M5150, Cavium
OCTEON3, and SB1 cores. The `TLBP`, `TLBWI`, and `TLBWR` instructions can
all trigger shutdown if multiple matching entries exist in the TLB.

**Dangerous initial states:**

Bootloaders and firmware may leave the TLB in pathological states that trigger
shutdown. The SGI IP22 PROM, for example, initializes all TLB entries to the
same virtual address. Other problematic states include duplicate entries from
incomplete previous boot attempts and garbage values that happen to create
collisions.

**Safe vs unsafe operations during TLB initialization:**

| Operation | Instruction | Safety During Init |
|-----------|-------------|-------------------|
| Indexed read | `TLBR` via `tlb_read()` | Safe — reads entry by index |
| Indexed write | `TLBWI` via `tlb_write_indexed()` | Unsafe if it creates a duplicate entry |
| Content probe | `TLBP` via `tlb_probe()` | Unsafe — may shutdown on duplicates |
| Random write | `TLBWR` via `tlb_write_random()` | Unsafe if it creates a duplicate entry |

**Safe initialization pattern:**

The kernel's `r4k_tlb_uniquify()` in `arch/mips/mm/tlb-r4k.c` demonstrates the
correct approach — read all entries by index first, detect duplicates in
software, then overwrite duplicates with unique values using indexed writes
that cannot create new collisions:

```c
// WRONG: Probe before TLB state is validated
for (entry = 0; entry < tlbsize; entry++) {
    write_c0_entryhi(UNIQUE_ENTRYHI(entry));
    tlb_probe();  // DANGER: may shutdown if duplicates exist
    if (read_c0_index() >= 0) {
        /* handle collision */
    }
}

// CORRECT: Read all entries first using indexed operations (r4k_tlb_uniquify pattern)
for (i = 0; i < tlbsize; i++) {
    write_c0_index(i);
    mtc0_tlbr_hazard();
    tlb_read();  // Safe indexed read
    tlb_read_hazard();
    existing_vpns[i] = read_c0_entryhi() & vpn_mask;
}
// Detect duplicates in software, then overwrite with unique values
// using tlb_write_indexed() — safe because each write removes a duplicate
```

TLB instruction wrappers (`tlb_read()`, `tlb_write_indexed()`, `tlb_probe()`,
etc.) and hazard barriers (`mtc0_tlbr_hazard()`, `tlb_read_hazard()`, etc.)
are defined in `arch/mips/include/asm/mipsregs.h`.

## Quick Checks

- **TLBP during initialization**: Any use of `tlb_probe()` in early boot code
  before the TLB has been uniquified is suspect. Check if bootloader state
  could cause duplicate entries.
- **TLBWI/TLBWR creating duplicates**: Indexed and random writes can also
  trigger shutdown if they create a second entry matching an existing one.
  During init, entries must be written with values known to be unique.
- **Bootloader state assumptions**: Code that assumes clean or zeroed TLB
  state at kernel entry is unsafe. Different bootloaders behave differently
  (e.g., SGI IP22 PROM sets all entries to the same VPN).
- **Hazard barriers**: TLB operations require architecture-specific hazard
  barriers between CP0 writes and TLB instructions. See `mtc0_tlbr_hazard()`,
  `tlb_read_hazard()`, `mtc0_tlbw_hazard()`, `tlbw_use_hazard()`.
