# Algebraic Folding (`match.pd`, `fold-const.cc`) and `wide_int` Invariants

## 1. `wide_int` vs. `widest_int` Precision Rules (`wide-int.h`)
- Every `wide_int` carries the bit precision of the `tree` or `machine_mode` it was constructed from (`wi::to_wide (t)` has precision `TYPE_PRECISION (TREE_TYPE (t))`).
- **CRITICAL**: Binary operations and comparisons on `wide_int` (`wi::eq_p`, `wi::ne_p`, `wi::lt_p`, `wi::le_p`, `wi::gt_p`, `wi::ge_p`, `wi::lts_p`, `wi::les_p`, `wi::ltu_p`, `wi::leu_p`, `+`, `-`, `&`, `|`, `^`) **assert that both operands have identical precision** (`gcc_checking_assert` in `wide-int.h`).
- In `match.pd` and `fold-const.cc`, shift counts (`C1`, `C3` in `((X >> C1) & C2) << C3`), operands of conversions, or constants from different sub-expressions frequently have **different integer types and different precisions** (e.g., `int` vs `long` or `unsigned char`).
  - Calling `wi::leu_p (wi::to_wide (@1), wi::to_wide (@3))` when `@1` and `@3` are shift counts or constants of potentially different types will **ICE** whenever `TYPE_PRECISION (TREE_TYPE (@1)) != TYPE_PRECISION (TREE_TYPE (@3))`!
  - Always check if the types are guaranteed identical; if not, use `wi::to_widest (@1)` and `wi::to_widest (@3)` (or `wi::to_offset`), or pass the target precision `wi::to_wide (@1, prec)`.

## 2. Overflow and Wrapping Soundness in `match.pd`
- **Eliminating or Moving Sub-Expressions Across `min`/`max`, Comparisons, or Casts**:
  - Consider folding `minmax (a - c, b) + c -> minmax (a, b + c)`. Checking only that `b + c` does not overflow is **WRONG** (`wrong-code` bug)! If `a - c` wraps around (in unsigned arithmetic or `-fwrapv` / `TYPE_OVERFLOW_WRAPS`) or overflows, then `(a - c)` is not mathematically `a - c` in $\mathbb{Z}$: for unsigned `a = 0, b = 0, c = 1`, `MIN (0u - 1u, 0u) + 1u = MIN (UINT_MAX, 0u) + 1u = 1u`, whereas `MIN (0u, 0u + 1u) = 0u`!
  - Whenever an algebraic identity cancels `+ c` and `- c` across a non-linear operator (`MIN_EXPR`, `MAX_EXPR`, `<, <=, >, >=`, `ABS_EXPR`, division, shifts, widening casts), **both** `a - c` AND `b + c` must be proven not to wrap/overflow (e.g., using `range_op_handler (MINUS_EXPR).overflow_free_p (vr_a, vr_c, type)` and `range_op_handler (PLUS_EXPR).overflow_free_p (vr_b, vr_c, type)`).
- **Introducing Undefined Signed Overflow**:
  - If the original expression computed arithmetic in an unsigned type or guarded against overflow, the folded replacement must not introduce signed arithmetic with `TYPE_OVERFLOW_UNDEFINED (type)` unless proven overflow-free or wrapped via `view_convert` / unsigned type (`rewrite_to_defined_overflow`).
- **Shift Count Bounds**:
  - Shifting by a negative value or by `>= TYPE_PRECISION (type)` is undefined in C/C++ GIMPLE. When combining shifts (e.g., `(X >> C1) << C3` or `(X << C1) >> C2`), verify that `C1`, `C3`, and `C1 + C3` or `|C1 - C3|` stay strictly within `[0, precision - 1]` before emitting a shift by their sum/difference.

## 3. Floating-Point and Side-Effect Guards
- Check `HONOR_NANS (type)`, `HONOR_INFINITIES (type)`, `HONOR_SIGNED_ZEROS (type)`, `flag_trapping_math`, and `flag_rounding_math` for any floating-point fold or comparison reversal.
- When dropping an operand in `match.pd` (e.g., `x * 0 -> 0` or `x == x -> true`), ensure `:s` or `tree_side_effects_p` / `generic_expr_could_trap_p` is respected or `omit_one_operand` is used.

## 4. `match.pd` Preprocessor Guards, Modifiers (`:c`, `:s`), Vector/Boolean Types, and GENERIC Oscillation
- **`#if GIMPLE` / `#if GENERIC`, NEVER `#ifdef GIMPLE` / `#ifdef GENERIC`**:
  - `genmatch` defines both `GIMPLE` and `GENERIC` as macros with values `0` or `1` (`#define GIMPLE 1`, `#define GENERIC 0`, and vice versa). Consequently, `#ifdef GIMPLE` and `#ifdef GENERIC` are **always true** in both modes! Using `#ifdef GIMPLE` or `#ifdef GENERIC` in `match.pd` is always a bug (`#if GIMPLE` or `#if GENERIC` must be used instead).
- **Commutativity (`:c`) and Single-Use (`:s`) Modifiers**:
  - When a `match.pd` pattern matches a commutative or comparison operator (`plus`, `mult`, `bit_and`, `bit_ior`, `bit_xor`, `eq`, `ne`, `lt`, `le`, `gt`, `ge`, `min`, `max`) where one operand is asymmetric or cross-referenced in another sub-expression (such as `(a CMP b) ? minmax<a, c> : minmax<b, c>`), check whether `:c` is needed on the operator.
  - When a `match.pd` replacement emits multiple instructions or moves an operation inside a conversion/binary op (e.g., `(T)A + CST -> (T)(A + CST)` or factoring/distributing operations), verify `:s` (`single_use`) is used on intermediate expressions so multi-use SSA values are not recomputed.
- **Vector Types (`element_precision`), `_BitInt`, and Non-1-Bit / Signed `BOOLEAN_TYPE`**:
  - `match.pd` rules on `(convert ...)`, `(abs ...)`, and arithmetic/bitwise operators match both scalar and `VECTOR_TYPE` trees unless explicitly guarded by `INTEGRAL_TYPE_P` or `!VECTOR_TYPE_P`. Calling `TYPE_PRECISION (type)` on a `VECTOR_TYPE` triggers an immediate `tree_check_failed` ICE! Always use `element_precision (type)` when `type` can be a `VECTOR_TYPE`, check `type_has_mode_precision_p` when excluding `_BitInt` / bit-field precisions, and verify target optab / `direct_internal_fn_supported_p` support when generating vector or internal-function (`IFN_*`) operations.
  - In GCC's middle-end, `BOOLEAN_TYPE` can be signed 1-bit (`true == -1`, where negating or casting to signed integer produces `-1` instead of `1`) or multi-bit (Fortran `LOGICAL` and vector masks where `TYPE_PRECISION (type) > 1`, so `BIT_NOT_EXPR` flips upper bits instead of inverting truth value). Never use `BIT_NOT_EXPR` (`~`) or integer negation on arbitrary `BOOLEAN_TYPE`s without checking `TYPE_PRECISION (type) == 1 && TYPE_UNSIGNED (type)`.
- **GENERIC Oscillation with `fold-const.cc`**:
  - When adding a canonicalization in `match.pd` that rewrites conditional expressions into arithmetic/bitwise form (or vice versa), check whether `fold-const.cc` (`fold_binary_op_with_conditional_arg`, `fold_cond_expr_with_comparison`) performs the inverse transformation in GENERIC, which causes infinite mutual recursion unless guarded by `#if GIMPLE`.
