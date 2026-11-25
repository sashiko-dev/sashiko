+++
title = "LLVM IR and Middle-End"
paths = ["llvm/lib/IR/", "llvm/lib/Transforms/", "llvm/lib/Analysis/", "llvm/include/llvm/IR/", "llvm/include/llvm/Transforms/", "llvm/include/llvm/Analysis/"]
symbols = ["IRBuilder", "InstCombine", "ValueTracking", "SimplifyQuery"]
+++
# LLVM IR and Middle-End: maintainer notes

Review guidance for patches touching IR construction, analysis, and transform
passes.

- An analysis or assumption built on `poison` or `undef` must be valid for every possible value the instruction may take. Folding that removes or propagates a poison-generating operation can change a defined program into UB.
- `nsw`, `nuw`, `inbounds`, `!range`, `noundef`, and fast-math flags may only be added when every operand value that reaches the instruction satisfies them; widening a flag across a cast, select, phi, or loop iteration requires proof, not convention.
- A transform that modifies IR must keep the analysis manager's preserved sets accurate (`PreservedAnalyses::preserve` only for analyses that are genuinely unchanged), and must update `AssumptionCache`, `DominatorTree` and `MemorySSA` when the change alters domination, assumptions, or memory ordering.
- An `IRBuilder`'s insertion point, current debug location, and block terminator invariants are state; moving builders across blocks without re-anchoring them produces instructions after terminators or in unrelated functions.
- Iteration over `DenseMap`, `DenseSet`, `SmallPtrSet`, or pointer-keyed hash containers must not influence the emitted IR, because the iteration order depends on addresses and breaks reproducibility.
- InstCombine and other canonicalization passes must agree with each other on the canonical form; a new fold that reverses an existing canonicalization can ping-pong forever, so check the inverse in `InstCombine` and `SimplifyCFG` before reporting convergence.

