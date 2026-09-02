# LLVM Core IR and Data Structures Guidelines

## Core Principles
The `llvm/IR` library implements the core Intermediate Representation, Instruction hierarchy, BasicBlocks, and Modules.

## Critical Invariants to Verify

1. **SSA Dominance**:
   - An operand must dominate all its uses unless used within a `PHINode`.
   - When moving instructions or inserting helper code with `IRBuilder`, verify that operand definitions precede the insertion point.

2. **PHINode Consistency**:
   - Every `PHINode` must have exactly one incoming value for each predecessor basic block in the CFG.
   - Removing a predecessor branch without removing corresponding incoming values creates malformed IR.

3. **Iterator Invalidation**:
   - Deleting an instruction using `I->eraseFromParent()` invalidates any iterator currently pointing to `I`.
   - Always use `llvm::make_early_inc_range()` when erasing elements during iteration.
