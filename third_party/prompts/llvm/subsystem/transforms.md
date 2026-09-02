# LLVM Transforms and Optimizations Review Guidelines

## Core Principles
Mid-end passes (InstCombine, SimplifyCFG, LoopVectorize, GVN, SCCP) rewrite IR into more efficient forms.

## Critical Invariants to Verify

1. **Algebraic Rewrites and Type Agreement**:
   - When folding binary or comparison instructions:
     - Check that both operands have identical types before creating new instructions.
     - For `foldICmpOrConstant`: if replacing two comparisons with a single comparison against null,
       ensure both compared values have the same pointer type or address space.
   - Preserving vector vs scalar semantics: verify whether transforms apply to vector types with the same lane counts.

2. **Poison and Undef Flags**:
   - `nsw` / `nuw` / `exact`: If an optimization re-associates operands `(A + B) + C` into `A + (B + C)`,
     the inner addition `B + C` may overflow even if the original did not. Flags must be dropped.
   - When folding selects or shuffles, verify that undef lanes do not poison defined lanes.

3. **Termination and Infinite Loops**:
   - Every InstCombine visit method must return an `Instruction *` or `nullptr`.
   - Modifying the worklist repeatedly without reducing cost leads to compiler hang / infinite loop.
