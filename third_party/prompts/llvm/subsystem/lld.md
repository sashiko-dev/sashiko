# LLD Linker Review Guidelines

## Core Principles
The `lld` linker resolves external symbols, processes object file sections, computes relocations, and synthesizes ELF/COFF/MachO executables.

## Critical Invariants to Verify

1. **Relocations and Authentication Invariants**:
   - For signed GOT or Pointer Authentication (PAC) relocations on targets like AArch64:
     - Relocations against non-preemptible IFUNC symbols must emit supported relocation types or report user-facing linker errors.
     - Never fail an internal assertion or silently omit authentication flags on GOT entries when an unsupported relocation combination is encountered.

2. **Section Flag Merging**:
   - When merging relocatable input segments in `-r` (relocatable link mode), segments with different flags (e.g. Executable vs non-executable, writable vs read-only) must not be coalesced together.
