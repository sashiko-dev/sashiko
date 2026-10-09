# GCC Technical Bug Patterns Reference

Use this checklist of recurring GCC defect patterns when auditing patches across the compiler and runtime libraries.

## 1. Tree, GIMPLE, and `wide_int` / `poly_int` Patterns

- **`wide_int` Precision Assertion (`wide-int.h`)**:
  - `wide_int` binary operations and comparisons (`wi::eq_p`, `wi::lt_p`, `wi::le_p`, `wi::leu_p`, `wi::add`, `wi::sub`, `wi::bit_and`) assert that both operands have the **exact same bit precision**.
  - Whenever two constants `C1` and `C3` (e.g., two shift counts in `((X >> C1) & C2) << C3`, or a bit-field width and an integer constant) can have different integer types/precisions, converting them via `wi::to_wide (c1)` and `wi::to_wide (c3)` will ICE! Use `wi::to_widest (c1)` / `wi::to_widest (c3)` or `wi::to_offset` when comparing values across potentially different types.
- **Unsigned Wrapping, Overflow, and `:c` / `:s` Modifiers in `match.pd` and `fold-const.cc`**:
  - When folding expressions like `minmax (a - c, b) + c -> minmax (a, b + c)` or `(x + c1) cmp c2`, proving that one sub-expression (`b + c`) does not overflow is NOT sufficient if the other (`a - c`) can wrap around or overflow! Verify that every sub-expression eliminated or commuted across `minmax`, comparisons, or casts is proven overflow/wrap-free.
  - Check `:c` commutativity annotations when matching commutative binary/comparison ops with asymmetric operands, and `:s` (`single_use`) when a replacement emits multiple operations or pushes an operation across a conversion.
  - Check non-1-bit or signed `BOOLEAN_TYPE` (Fortran `LOGICAL`, vector masks): `BIT_NOT_EXPR` or integer negation is only equivalent to logical inversion when `TYPE_PRECISION (type) == 1 && TYPE_UNSIGNED (type)`.
- **Stale Flow-Sensitive Info on Rewritten or Hoisted `SSA_NAME`s**:
  - When an optimization moves a statement out of a guarded basic block or rewrites the definition of an existing `SSA_NAME` in place with an expression that has a wider range or different sign/magnitude, previously recorded `SSA_NAME_RANGE_INFO` / nonzero bits / `SSA_NAME_PTR_INFO` become stale and cause `wrong-code` in VRP/CCP. Must call `reset_flow_sensitive_info (lhs)` (or allocate a fresh `make_ssa_name`).
- **Tree Checking Macro Mismatches (`--enable-checking=yes,rtl`)**:
  - `DECL_SOURCE_LOCATION (t)`, `DECL_CONTEXT (t)`, and `DECL_NAME (t)` require `DECL_P (t)`. Calling them on a `TYPE_P (t)` (such as a `RECORD_TYPE` or `UNION_TYPE`) triggers a `tree_check_failed` ICE; use `location_of (t)`, `TYPE_CONTEXT (t)`, or `TYPE_MAIN_DECL (t)`.
  - `TYPE_PRECISION (t)` requires an integral, real, or fixed-point scalar type (`INTEGRAL_TYPE_P` / `SCALAR_FLOAT_TYPE_P`). Calling `TYPE_PRECISION` on a `VECTOR_TYPE` triggers a `tree_check_failed` ICE; use `element_precision (t)`.
  - `TREE_INT_CST_LOW (t)` and `wi::to_wide (t)` require `TREE_CODE (t) == INTEGER_CST`.
  - `SSA_NAME_DEF_STMT (t)` requires `TREE_CODE (t) == SSA_NAME`.
- **Tree Sharing Violations (`unshare_expr` / `unshare_constructor`)**:
  - GCC requires non-constant tree nodes (`RANGE_EXPR`, `TARGET_EXPR`, `CONSTRUCTOR`, `SAVE_EXPR`, statement lists) not to be shared across multiple places without unsharing. When deep-copying a `CONSTRUCTOR`, both `elt.value` AND `elt.index` (when it is a `RANGE_EXPR`) must be unshared!

## 2. RTL and Target Backend (`gcc/config/*`) Patterns

- **`reversed_comparison_code` Returning `UNKNOWN`**:
  - `reversed_comparison_code (cond, insn)` and `reverse_condition_maybe_unordered` can return `UNKNOWN` (especially on floating-point comparisons when `!flag_finite_math_only` / `HONOR_NANS` prevents reversing `LT` to `GE` without `UNGE`, or when target CC modes cannot represent the reversed code). Passing `UNKNOWN` directly to `gen_rtx_fmt_ee (code, ...)` or emitting a conditional trap/move without checking `if (code == UNKNOWN) return false;` triggers an RTL checking ICE or invalid insn!
- **Speculative / Dry-Run Probing and Null Basic Blocks (`testing_p`, `gimple_bb (stmt) == NULL`)**:
  - Target hooks and helpers (such as vector permute expanders or cost hooks) are frequently invoked in dry-run/testing mode (`d->testing_p`) or on synthetic/detached statements where `cfun` or `gimple_bb (stmt)` is `NULL`. Querying `gimple_bb (stmt)` or loop membership before checking `d->testing_p` (or without guarding `bb != NULL`) causes a segmentation fault ICE.
- **Target Option & `--param` Override Precedence**:
  - When refactoring target hooks or ISA/tuning checks, ensure user-specified `--param` or `-m` flags only override or select modes that are valid/enabled in the available mode list, and verify multi-ISA fallback guards (`TARGET_SSE4_1` vs `TARGET_AVX`, `TARGET_HAS_FMV_TARGET_ATTRIBUTE`, etc.) match the actual instruction requirements.
- **Post-Reload Pseudo Creation (`can_create_pseudo_p ()`)**:
  - Calling `gen_reg_rtx (mode)` inside an insn splitter or expander that can run when `!can_create_pseudo_p ()` (`reload_completed`) will ICE.

## 3. Language Frontends (C++, C, Fortran) & LTO/IPA

- **C++ Template Specialization Table Re-Entrancy and `constexpr` Context Alignment (`pt.cc`, `constexpr.cc`)**:
  - In `gcc/cp/pt.cc`, recursive `tsubst` / `complete_type` calls between `spec_hasher::hash` and `register_specialization` can grow the specialization hash table (`decl_specializations` / `type_specializations`) and invalidate held slot pointers, or cache a specialization that still references a `targs` `TREE_VEC` that the caller subsequently frees with `ggc_free (targs)`.
  - In `gcc/cp/constexpr.cc`, when recursively evaluating base classes, subobjects, or delegating constructors, verify `ctx->ctor` and `ctx->object` stay synchronized with the subobject being initialized.
- **Fortran `BIND(C)` and Module Visibility (`gcc/fortran/trans-decl.cc`)**:
  - In Fortran, a module entity with the `PRIVATE` attribute is hidden at the Fortran module level, **unless** it also has a `BIND(C)` binding label (`sym->attr.is_bind_c` and `sym->binding_label`) which gives it external C linkage. Applying `DECL_VISIBILITY (decl) = VISIBILITY_HIDDEN` to `PRIVATE` module variables or procedures without exempting `BIND(C)` entities with a binding label breaks shared library symbol export (`wrong-code` / link failure).
- **LTO / OpenMP / IPA Symbol & Varpool Walks (`gcc/lto/`, `gcc/ipa-*.cc`)**:
  - When moving a pass helper call to a new point in the pipeline or behind a conditional guard, verify the original call site is removed so the helper does not run twice.
  - When modifying `varpool_node` or `cgraph_node` tables while iterating `FOR_EACH_VARIABLE` / `FOR_EACH_FUNCTION`, inserting new nodes or aliases during iteration can invalidate the list or re-visit nodes; collect candidates in an `auto_vec` first and process them after the walk.
