+++
title = "LLVM Backend and Targets"
paths = ["llvm/lib/CodeGen/", "llvm/lib/Target/", "llvm/include/llvm/CodeGen/"]
symbols = ["SelectionDAG", "MachineInstr", "TargetLowering"]
+++
# LLVM Backend: maintainer notes

Review guidance for patches touching instruction selection, machine passes,
and target backends.

- A calling-convention, register-class, or ABI decision is mirrored in the
  frontend, the target lowering, and the assembler/disassembler surfaces.
  When a patch changes one, check whether the counterparts need the same
  change; an ABI mismatch is user-visible and cannot be repaired by a later
  pass.
- `TargetLowering` and TableGen lowering code may only assume a type is
  legal, a value is in a register, or a node has the expected operand count
  when a target hook (`isTypeLegal`, `hasOperation`, `allowsMemoryAccess`,
  `shouldExpand*`) has established it for every subtarget the code can run
  under.
- If a flag, target feature, or subtarget predicate gates an instruction
  variant, the same predicate must gate every place that selection, assembly
  printing, code emission, and optimization identity assume the variant.
- `MachineInstr` bundles, implicit operands, and physical-register liveness
  are invariants of the machine scheduler and register allocator; a patch
  that removes or rewrites an instruction must update `MachineRegisterInfo`
  uses and defs and the `LiveIntervals`/`LivePhysRegs` state consistently.
- Do not flag a target-specific change because it does not apply to other
  targets when the feature or instruction does not exist there; ask whether
  the same source-level semantic must be preserved on every target instead.
