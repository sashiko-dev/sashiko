# LLVM Technical Patterns and Invariants

## 1. Type Casting and Null Pointer Safety

### 1.1 `cast<T>` vs `dyn_cast<T>` vs `isa<T>`
- **`cast<T>(V)`**: Performs an assertion-checked cast. Use ONLY when the type of `V` has already been
  proven with `isa<T>(V)` or is guaranteed by construction. Never use `cast<T>` on arbitrary input.
- **`dyn_cast<T>(V)`**: Returns `nullptr` if `V` is not an instance of `T`.
  - **Critical Invariant**: Every `dyn_cast<T>` MUST be checked for null before dereferencing:
    ```cpp
    auto *CI = dyn_cast<ConstantInt>(Val);
    if (!CI)
        return false;
    ```
  - Unchecked dereferencing of `dyn_cast` results causes segmentation fault crashes in release builds.
- **`dyn_cast_or_null<T>(V)`**: Use when `V` itself might be `nullptr`.

## 2. SSA Dominance and Instruction Insertion

### 2.1 Dominance Invariant
- Any operand passed to an instruction must dominate that instruction (except inside `PHINode` incoming values).
- When creating helper instructions using `IRBuilder<>`:
  - Verify insertion point: if building an instruction that uses values `A` and `B`, the insertion point
    must be located where both `A` and `B` are in scope and dominate the insertion point.
  - If hoisting or sinking instructions, verify `DT.dominates(Def, Use)`.

### 2.2 Type Agreement in Builders
- Functions such as `IRBuilder::CreateICmp(Pred, LHS, RHS)` require that `LHS->getType() == RHS->getType()`.
- If one operand is a pointer and another is an integer (or null pointer with a different address space),
  direct comparison violates IR type rules and triggers an assertion failure.
- When creating `SelectInst`, the condition must be `i1` (or vector of `i1`), and true/false values must have
  identical types.

## 3. Iterator Invalidation During IR Mutations

### 3.1 BasicBlock and Instruction Iteration
- Deleting an instruction while iterating through a `BasicBlock`:
  ```cpp
  // BUG: Invalidates iterator upon eraseFromParent()
  for (Instruction &I : BB) {
      if (canFold(&I))
          I.eraseFromParent();
  }
  ```
- **Correct Idiom**: Use `llvm::make_early_inc_range(BB)`:
  ```cpp
  for (Instruction &I : llvm::make_early_inc_range(BB)) {
      if (canFold(&I))
          I.eraseFromParent();
  }
  ```
- In MachineBasicBlock transformations: verify iterators do not equal `MBB.end()` before dereferencing:
  ```cpp
  if (MI == MBB.end())
      return;
  unsigned Opcode = MI->getOpcode();
  ```

## 4. Algebraic Simplification and Poison/Undef Soundness

### 4.1 Overflow and Exact Flags
- Instructions may carry flags: `nsw` (no signed wrap), `nuw` (no unsigned wrap), `exact` (exact division).
- When rewriting an expression (e.g. `(A + B) - C` into `A + (B - C)`), the original flags do NOT
  necessarily hold for the newly constructed operations.
- If flags cannot be proven for the new operation, you MUST NOT copy the flags. Drop them to avoid
  introducing false poison.

### 4.2 Speculative Execution and UB
- Never speculatively hoist or create instructions that could trigger immediate undefined behavior
  (e.g., division by zero, null pointer dereference, out-of-bounds GEP with `inbounds`) unless guaranteed
  to execute or guarded by `isSafeToSpeculativelyExecute()`.

## 5. Termination and Fixed-Point Iteration in InstCombine

### 5.1 Loop Prevention
- InstCombine relies on fixed-point iteration. Every transformation must simplify or canonicalize the IR.
- Never write a rule that reverses another canonical rule:
  - If Pass A transforms `icmp slt X, C+1` into `icmp sle X, C`, Pass B must NOT transform `icmp sle X, C` back into `icmp slt X, C+1`.
  - Re-inserting instructions repeatedly without modifying state causes infinite compilation loops.
