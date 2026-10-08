# GIMPLE, SSA, Ranger/VRP, and Vectorizer Invariants

## 1. SSA Form and Flow-Sensitive Metadata (`reset_flow_sensitive_info`)
- **Stale Value Range / Nonzero Bits on Rewritten `SSA_NAME`**:
  - Every `SSA_NAME` carries flow-sensitive metadata (`SSA_NAME_RANGE_INFO`, nonzero bits, pointer alignment/nullness in `SSA_NAME_PTR_INFO`).
  - Whenever a pass (e.g., `tree-ssa-forwprop.cc`, `tree-ssa-phiopt.cc`, `tree-ssa-ccp.cc`, `tree-ssa-reassoc.cc`) modifies the defining statement of an existing `SSA_NAME` in place or reuses an `SSA_NAME` LHS for a rewritten expression whose set of possible values can be *wider* or *different* than before—for example, rewriting `M = X % PHI<2^a, 2^b>` into `M = X & PHI<2^a-1, 2^b-1>` when `X` may be signed negative and `M` is only compared with zero—you **MUST** call `reset_flow_sensitive_info (lhs)`! Otherwise, VRP/ranger will combine the old range (e.g., `[-7, 0]`) with the new bitwise AND (`>= 0`) and fold `M` to `0` (`wrong-code`).
- **Statement Modification & Virtual Operands**:
  - Any in-place change to a `gimple *` statement's operands or code requires `update_stmt (stmt)`.
  - Before removing a statement with `gsi_remove (&gsi, true)` that may have a `gimple_vdef (stmt)`, call `unlink_stmt_vdef (stmt)` and `release_defs (stmt)` (or `release_ssa_name (vdef)`) so virtual SSA form is not corrupted.
- **Checking `SSA_NAME_DEF_STMT` and Statement Kinds**:
  - Never call `SSA_NAME_DEF_STMT (op)` before verifying `TREE_CODE (op) == SSA_NAME`.
  - `SSA_NAME_DEF_STMT (op)` can be `GIMPLE_NOP` (`SSA_NAME_IS_DEFAULT_DEF (op)`), `GIMPLE_PHI`, `GIMPLE_CALL`, `GIMPLE_ASM`, or `GIMPLE_ASSIGN`. Always check `is_gimple_assign (def_stmt)` before calling `gimple_assign_rhs_code (def_stmt)` or `gimple_assign_rhs1 (def_stmt)`.

## 2. Basic Block & CFG Safety
- **`gimple_bb (stmt)` Can Be `NULL`**:
  - Statements not yet inserted into a basic block, or dummy statements created during target hook / cost / permute probing (`testing_p`), have `gimple_bb (stmt) == NULL`. Passing `gimple_bb (stmt)` to `flow_bb_inside_loop_p` or dereferencing `bb->loop_father` without checking `bb != NULL` (or `testing_p` first) causes an immediate segfault ICE.
- **`-fcompare-debug` Invariance**:
  - Loops over basic block statements (`gsi_start_bb`, `gsi_next`) must use `gsi_start_nondebug_bb` / `gsi_next_nondebug` or explicitly skip `is_gimple_debug (stmt)` whenever counting statements, checking single-statement blocks, or computing heuristics.

## 3. Scalable Vectors (`poly_int`) and Vectorizer (`tree-vect-*.cc`)
- Never call `.to_constant ()` on `TYPE_VECTOR_SUBPARTS (type)`, `GET_MODE_SIZE (mode)`, or `GET_MODE_NUNITS (mode)` unless guarded by `.is_constant ()` or restricted to fixed-length vector modes.
- Use `known_eq` / `known_lt` / `known_le` when proving a transformation is valid for *all* runtime vector lengths, and `maybe_eq` / `maybe_lt` when checking whether a hazard/overlap *might* occur.

## 4. Whole-Function Dataflow Sets, Value Numbering (`sccvn`), and Range Queries
- **Function-Wide vs. Dominating Dataflow Sets**:
  - When a pass queries a precomputed set of non-trapping addresses or value equivalences (such as `get_non_trapping ()` in `tree-ssa-phiopt.cc`) to justify hoisting or sinking a memory access out of a conditional basic block, verify whether the set was collected across *all* basic blocks in the function (including the conditional block itself) rather than only dominating blocks. If the conditional block contains a load from the same address, that load populates the function-wide set and cannot justify making an unconditional access outside the condition.
- **Value Numbering (`tree-ssa-sccvn.cc`) Optimistic Equivalence Cycles**:
  - During SCCVN / predicate insertion, `vn_valueize` can map an `SSA_NAME` back to an earlier value or the LHS of the expression being simplified. Any recursive walk over valueized operands must guard against cycles (`nlhs == lhs` or `nrhs == lhs`) across all comparison and binary operators.
- **Type-Agnostic `Value_Range` vs. `int_range_max` (`irange`)**:
  - `int_range_max` (`irange`) only supports integral and pointer types (`irange::supports_p (type)`) and aborts on `SCALAR_FLOAT_TYPE_P`. Use `Value_Range` (which dispatches between `irange`, `frange`, and `prange`) whenever the queried SSA name or expression can have a floating-point or vector type.
