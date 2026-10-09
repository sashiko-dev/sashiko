# Severity Levels

Calibrate every validated GCC finding to one of four severity levels (`Critical`, `High`, `Medium`, `Low`) based on its impact on compiled programs and the compiler itself.

> Note on GCC's Security Model (`SECURITY.txt`): GCC runs on trusted source code by design. Compiler crashes (ICEs) or resource exhaustion on malformed source code are stability/quality bugs (`High` or `Medium`), whereas silently miscompiling valid code (`wrong-code`) or corrupting target runtime libraries (`libstdc++`, `libgcc`, `libgomp`, etc.) can introduce severe vulnerabilities into compiled user binaries (`Critical`).

## Critical

Bugs that cause **silent miscompilation (`wrong-code`)**, **ABI breakage**, or **memory corruption in target runtime libraries**:
- **`wrong-code` in Optimization or Code Generation**: Any GIMPLE, SSA, `match.pd`, `fold-const.cc`, vectorizer, IPA, RTL, or target backend (`gcc/config/*`) bug that changes the observable behavior of a valid program:
  - Unsound algebraic/bitwise rewrites (ignoring `TYPE_OVERFLOW_WRAPS` vs. `TYPE_OVERFLOW_UNDEFINED`, signed `INT_MIN` overflow, unsigned wrap-around, or negative/oversized shift counts).
  - Violating IEEE-754 floating-point semantics (`HONOR_NANS`, `HONOR_INFINITIES`, `HONOR_SIGNED_ZEROS`, `flag_trapping_math`, `flag_rounding_math`).
  - Stale SSA value-range or nonzero-bits metadata (`reset_flow_sensitive_info` omitted when rewriting an `SSA_NAME` to an expression with a different range/value), missing `update_stmt`, or broken virtual memory SSA (`unlink_stmt_vdef`).
  - Incorrect `poly_int` scalable vector reasoning (using `maybe_eq` where `known_eq` is required, or vice versa).
  - Target backend / RTL miscompilations: missing condition-code (`CC_REG`) or scratch register clobbers in `define_insn`/`define_split`, missing earlyclobber (`=&`), wrong `SUBREG` big-endian byte/lane offsets, or ABI/calling-convention mismatches.
- **Target Runtime Library Bugs (`libstdc++`, `libgcc`, `libgomp`, `libatomic`, `libgfortran`)**: Memory corruption, use-after-free, data races, buffer overflows, or wrong results in runtime code linked into user binaries.

## High

Bugs that **crash the compiler on valid code (`ice-on-valid-code`)**, **reject valid programs (`rejects-valid`)**, **corrupt compiler host memory**, or **break `-fcompare-debug` / bootstrap**:
- **`ice-on-valid-code`**: Any deterministic `gcc_assert`, `gcc_unreachable`, `internal_error`, `TREE_CHECK` / `RTX_CHECK` failure, null pointer dereference (`NULL_TREE`, `NULL_RTX`, `gimple_bb(stmt) == NULL`), `wide_int` precision mismatch assertion (`wi::lt_p` / `wi::leu_p` on operands of different bit precision), `poly_int::to_constant()` assertion on scalable vectors, or IR verification failure (`verify_gimple`, `verify_ssa`, `verify_rtl_sharing`, `verify_flow_info`) reachable from valid source code.
- **`rejects-valid`**: Frontend or SFINAE bugs that cause GCC to reject valid C, C++, Fortran, or Ada code—including emitting an unguarded `error()` or `permerror()` in `gcc/cp/` when `!(complain & tf_error)` during SFINAE substitution or tentative parsing.
- **`-fcompare-debug` Failures**: Allowing `is_gimple_debug(stmt)` or `DEBUG_INSN_P(insn)` to alter code generation, pass decisions, or UIDs.
- **Compiler Memory Corruption & Host UB**:
  - GGC (`GTY(())`) / Precompiled Header (PCH) use-after-free or corruption.
  - Holding a pointer or reference into a `vec<>`, `auto_vec<>`, `hash_table<>`, or `hash_map<>` across an operation (`safe_push`, `put`, recursive call) that can reallocate the container.
  - Host undefined behavior in compiler code (signed integer overflow, shift by `>= width` or `1 << bit` instead of `HOST_WIDE_INT_1U << bit`, uninitialized variable read) that breaks bootstrap or `--enable-checking`.

## Medium

Bugs that **crash on invalid code (`ice-on-invalid-code`)**, **accept invalid code (`accepts-invalid`)**, **hang/explode compile time (`compile-time-hog`)**, **leak host memory**, or **break non-default target builds**:
- **`ice-on-invalid-code`**: Missing `error_mark_node` or `error_operand_p(t)` checks in language frontends or middle-end lowering that cause a `TREE_CHECK` failure, null dereference, or assertion crash after a prior user syntax/type error.
- **`accepts-invalid` / Bogus Warnings**: Failing to diagnose ill-formed code or emitting false-positive warnings on valid constructs.
- **`compile-time-hog`**: Non-terminating `match.pd` / RTL `combine` oscillation or unbounded quadratic/exponential walks without a visited `bitmap`/`hash_set` or `--param` limit.
- **Host Memory / Resource Leaks**: Missing `BITMAP_FREE`, `vec::release()`, `free()`, or `fclose()` on early return or error paths.
- **Cross-Target / Configuration Build Breakage**: Unused variables/functions or missing declarations under `#ifdef TARGET_*` that fail `-Werror` bootstrap on other targets.

## Low

Minor defects with no crash, miscompilation, or build failure:
- Inaccurate `location_t` diagnostic source locations.
- GCC internal diagnostic format string violations (e.g., using raw `'...'` or backticks instead of `%qs`, `%<...%>`, `%qE`, `%qD`, `%qT`).
- Redundant dead code or minor logic inconsistencies that do not alter external behavior.
