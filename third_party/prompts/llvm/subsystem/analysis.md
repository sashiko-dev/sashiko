# LLVM Analysis and Value Tracking Guidelines

## Core Principles
Static analysis passes deduce program invariants (ValueTracking, ScalarEvolution, KnownBits, ConstantRange, AliasAnalysis).

## Critical Invariants to Verify

1. **`KnownBits` and `ConstantRange` Calculations**:
   - Verify signed vs unsigned interpretations.
   - When shifting or negating ranges, ensure the minimum signed value (`INT_MIN`) does not cause arithmetic overflow.
   - For bitwise operations, verify that known zeros and known ones are not transposed.

2. **Pointer Provenance and Alias Analysis**:
   - Analyzing uses of a pointer (`AnalyzeUsesOfPointer`) must account for self-referencing stores or cycles.
   - Constant pointer deductions must respect object bounds and provenance.
