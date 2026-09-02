# LLVM Target Backends Review Guidelines

## Core Principles
Target backends (AArch64, X86, RISCV, AMDGPU, etc.) translate target-independent machine IR into assembly and object code.

## Critical Invariants to Verify

1. **Calling Conventions and Register Allocator Invariants**:
   - Register clobbers in calling conventions must reflect all registers modified by the call.
   - Zeroing or restoring call-used registers must verify hardware feature presence (e.g. Do not emit vector instructions when vector extensions are disabled).

2. **TableGen and Unwind / DWARF Register Aliases**:
   - In `*RegisterInfo.td`, verify that DWARF register numbers and aliases (`DwarfRegAlias`) correctly match the target ABI specifications.
   - An off-by-one or mismatched alias corrupts stack unwinding and debugger backtraces.

3. **Branch Conditions and Epilogues**:
   - In frame lowering and shadow call stack emission, verify that return instructions (`ret`, `popret`) are placed correctly relative to stack pointer adjustments and shadow stack checks.
