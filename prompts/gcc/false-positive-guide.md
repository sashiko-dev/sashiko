# GCC False Positive Prevention Guide

Before validating a finding, check these GCC-specific invariants and conventions to avoid false positives while ensuring real defects are never rationalized away.

## 1. Patterns That Are NOT Bugs in GCC

1. **`gcc_checking_assert` vs. `gcc_assert` for Proven Pass Invariants**:
   - Do not flag a `gcc_assert` or `gcc_checking_assert` as an ICE bug if the exact condition was already established by a dominating check in the same function or is a structural invariant guaranteed by the immediately preceding builder call (e.g., `TREE_CODE (t) == SSA_NAME` immediately after `make_ssa_name`).
   - *However*, if the assertion is reachable with an unguarded `error_mark_node`, `POLY_INT_CST`, different-precision `wide_int`, `UNKNOWN` rtx comparison code, or `NULL` basic block (`gimple_bb (stmt)` in a standalone/testing helper), it IS a real ICE bug.
2. **`GGNEW` / `ggc_alloc` vs. Heap Allocation**:
   - Objects allocated via `ggc_alloc`, `ggc_cleared_alloc`, `build_decl`, `build_int_cst`, `make_node`, `gen_rtx_*`, etc., are managed by GCC's garbage collector (`ggc`). Do NOT report them as C/C++ heap memory leaks for lacking `free()` or `delete`.
   - Conversely, objects allocated with `XNEW`, `XCNEW`, `XNEWVEC`, `xmalloc`, `BITMAP_ALLOC (NULL)`, or `vec::create` (non-`auto_vec` heap vectors) ARE manually managed and DO leak if not freed with `free()`, `BITMAP_FREE`, or `.release()`.
3. **`auto_vec<T>` and `auto_bitmap` Automatic Cleanup**:
   - `auto_vec<T, N>` and `auto_bitmap` are RAII wrappers that free their storage in their C++ destructor when leaving scope. Do NOT flag `auto_vec` or `auto_bitmap` for missing `.release()` or `BITMAP_FREE`.
   - Plain `vec<T>` (`va_heap`, default `vNULL`) is a POD handle without a destructor and DOES require `.release()`.
4. **DejaGnu Testsuite Files (`gcc/testsuite/*`, `libstdc++-v3/testsuite/*`)**:
   - Never report undefined behavior, uninitialized variables, infinite loops, or syntax errors inside `testsuite/` files. Those files are compiler test inputs.

## 2. Invalid Dismissals (Real Bugs You Must NOT Dismiss)

1. **Never Dismiss `wide_int` Precision Mismatches**:
   - `wi::to_wide (t1)` and `wi::to_wide (t2)` have the precision of `TREE_TYPE (t1)` and `TREE_TYPE (t2)`. Passing two `wide_int` values of potentially different precisions (such as two shift counts, bit positions, or operands before/after a cast) to `wi::eq_p`, `wi::lt_p`, `wi::le_p`, `wi::leu_p`, `+`, `-`, `&`, `|` triggers `gcc_checking_assert (precision == ...)` in `wide-int.h`! Unless both trees are proven in code to have identical `TYPE_PRECISION` (or `wi::to_widest` / `wi::to_offset` is used), this is a real `ice-on-valid-code` bug.
2. **Never Dismiss Missing Overflow / Wrapping Checks in `match.pd` or Folding**:
   - Rewriting `minmax (a - c, b) + c` to `minmax (a, b + c)` or similar algebraic identities is unsound if `a - c` wraps around (on unsigned types or `TYPE_OVERFLOW_WRAPS`) even when `b + c` does not overflow. Never assume "constants are usually small" or "wrapping is rare".
3. **Never Dismiss Missing `reset_flow_sensitive_info` When Changing an `SSA_NAME`'s Value or Moving Statements**:
   - If a pass moves a statement out of a conditional basic block or rewrites an existing `SSA_NAME` definition in place so that the `SSA_NAME` can hold a wider or different set of values, any pre-existing value range (`SSA_NAME_RANGE_INFO`) or pointer info (`SSA_NAME_PTR_INFO`) on that `SSA_NAME` becomes stale and will cause downstream VRP/CCP passes to miscompile uses of it.
4. **Never Dismiss `TREE_CHECK` / `DECL_CHECK` / `TYPE_CHECK` / Vector Type Macro Mismatches**:
   - Calling `DECL_SOURCE_LOCATION (t)` or `DECL_CONTEXT (t)` when `t` can be a `TYPE_P` (use `location_of (t)` or `TYPE_CONTEXT (t)`), calling `TYPE_PRECISION (type)` when `type` can be a `VECTOR_TYPE` (use `element_precision (type)`), calling `SSA_NAME_DEF_STMT (t)` without `TREE_CODE (t) == SSA_NAME`, or passing `UNKNOWN` from `reversed_comparison_code` into `gen_rtx_fmt_ee` causes an immediate checking ICE.
5. **Never Dismiss Missing `error_mark_node` / `error_operand_p` or `complain & tf_error` Guards**:
   - In C/C++ frontends, template substitution (`tsubst*`), constexpr evaluation (`cxx_eval_*`), and parser functions regularly encounter `error_mark_node` or run under SFINAE (`!(complain & tf_error)`). Never assume inputs are always valid or non-dependent.
6. **Never Dismiss Duplicate Calls, Forgotten Removals, or Incomplete Symmetric Fixes**:
   - When a patch moves a function call or state update to a new location or places it under a guard and leaves the original unguarded call behind, or when a patch fixes a bug in one function/operator/mode and misses the identical bug in a symmetric sibling function/overload in the same file, it is a real bug.
   - Never dismiss a leftover duplicate call by arguing that the second execution overwrites the first or that GGC garbage-collects orphaned nodes.
7. **Never Dismiss Unallocated Optional Substructures (`DECL_LANG_SPECIFIC`, `SSA_NAME_RANGE_INFO`) or Self-Polluted Dataflow Sets**:
   - In C/C++ redeclaration and module merging, `DECL_LANG_SPECIFIC (olddecl)` can be `NULL` even when `DECL_LANG_SPECIFIC (newdecl)` is non-NULL (`retrofit_lang_decl (olddecl)` is required).
   - When a pass queries a function-wide set of non-trapping addresses or value equivalences to justify hoisting/sinking a memory access out of a conditional block, verify that accesses *inside* the conditional block itself did not populate that set!
8. **Never Dismiss Silent Feature Bypass When a Patch Defeats Its Own Stated Goal**:
   - If a commit's purpose is to enable an optimization, section placement, or vectorization (e.g., marking arrays mergeable or selecting a vector mode for `memset`), and a mode/type/flag check in the callee (`mode != BLKmode`, `MOVE_MAX` vs `STORE_MAX_PIECES`, `#ifdef GIMPLE` vs `#if GIMPLE`) causes the new path to silently bail out or do the wrong thing, do NOT dismiss it as a "safe fallback".
