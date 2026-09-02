# LLVM CodeGen and Instruction Selection Guidelines

## Core Principles
The CodeGen layer lowers LLVM IR to target-specific MachineInstructions via SelectionDAG or GlobalISel.

## Critical Invariants to Verify

1. **Unchecked Null Pointers in LiveIntervals and Rematerialization**:
   - Queries like `LI.getVNInfoAt(Slot)` or `LIS->getInstructionFromIndex(Idx)` can return `nullptr` if a register has no live value definition at that slot.
   - Unconditional dereferencing of `VNInfo *` results in segmentation faults. Always check for null.

2. **MachineBasicBlock Iterator Bounds**:
   - Iterators walking `MachineBasicBlock` can reach `MBB.end()`.
   - Never dereference `MI->getOpcode()` or query operands without verifying `MI != MBB.end()`.

3. **Value Type Mismatches in DAGCombiner**:
   - When lowering or combining DAG nodes (e.g. `ANY_EXTEND`, `SIGN_EXTEND`), ensure the destination value type (`VT`) is strictly wider than the source value type (`SrcVT`).
