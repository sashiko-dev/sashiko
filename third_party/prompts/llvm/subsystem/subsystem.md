# LLVM Subsystem Guide Index

| Trigger Pattern | Target Guide | Description |
|---|---|---|
| `llvm/lib/Transforms/`, `InstCombine`, `SimplifyCFG`, `Vectorize` | `transforms.md` | Mid-end optimization passes, algebraic simplifications, vectorization |
| `llvm/lib/Analysis/`, `ValueTracking`, `ConstantRange`, `KnownBits` | `analysis.md` | Static analyses, value bounds tracking, pointer alias analysis |
| `llvm/lib/IR/`, `llvm/include/llvm/IR/`, `Instruction`, `BasicBlock` | `ir-core.md` | Core IR data structures, types, SSA dominance, PHI nodes |
| `llvm/lib/CodeGen/`, `SelectionDAG`, `GlobalISel`, `MachineInstr` | `codegen.md` | Target-independent code generation, instruction selection, regalloc |
| `llvm/lib/Target/`, `.td`, `TargetLowering`, `RISCV`, `AArch64`, `X86` | `targets.md` | Target backends, calling conventions, assembly parsing, tablegen |
| `clang/`, `clang/lib/Sema/`, `clang/lib/AST/`, `clang/lib/Parse/` | `clang-frontend.md` | Clang C/C++ frontend, type checking, AST nodes, overload resolution |
| `lld/`, `ELF`, `COFF`, `MachO`, `Relocations.cpp` | `lld.md` | Linker symbol resolution, section synthesis, relocation handling |
