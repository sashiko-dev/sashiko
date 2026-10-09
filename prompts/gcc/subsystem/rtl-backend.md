# RTL Optimization Passes and Target Backends (`gcc/config/*`)

## 1. Condition Codes and Comparison Reversal (`ifcvt.cc`, `combine.cc`, `simplify-rtx.cc`)
- **`reversed_comparison_code` and `reverse_condition_maybe_unordered` Can Return `UNKNOWN`**:
  - When reversing an RTL comparison `cond` (e.g., in `ifcvt.cc`, `combine.cc`, `cfgcleanup.cc`, or target backends), `reversed_comparison_code (cond, insn)` returns `UNKNOWN` whenever the comparison cannot be safely inverted (for instance, floating-point comparisons when `HONOR_NANS` is true and an unordered comparison code is not valid for the target CC mode).
  - Never pass the result of `reversed_comparison_code` to `gen_rtx_fmt_ee (code, ...)` or `simplify_gen_relational` without first checking `if (code == UNKNOWN) return false;`! Passing `UNKNOWN` triggers an RTL checking assertion failure (`ice-on-valid-code`).

## 2. RTL Modes, `SUBREG`s, `CONST_POLY_INT`, and Target Expanders (`gcc/config/<arch>/*`)
- **Unordered `poly_int` Modes and `SUBREG` Preconditions**:
  - On targets with scalable vectors (AArch64 SVE, RISC-V V), vector modes like `VNx4QImode` and scalar modes like `DImode` have unordered sizes (`!ordered_p (GET_MODE_SIZE (m1), GET_MODE_SIZE (m2))`). Calling `partial_subreg_p`, `subreg_lowpart_offset`, or `simplify_gen_subreg` without checking `ordered_p` or `validate_subreg` triggers an assertion ICE.
  - Unlike `CONST_INT` (which has `VOIDmode`), `CONST_POLY_INT` carries a concrete integer mode while satisfying `CONSTANT_P (op)`. When unwrapping or distributing mode extensions (`zero_extend` / `sign_extend`), do not assume `CONSTANT_P` operands are mode-less `VOIDmode` constants.
  - Check `GET_MODE_BITSIZE (mode)` vs `GET_MODE_SIZE (mode)` (bits vs bytes) whenever comparing bit-field offsets or shift counts against mode sizes.
- **Testing / Probing Context (`d->testing_p` or `cfun` / `stmt` Nullity)**:
  - Target hooks such as `expand_vec_perm_const` are invoked both during real RTL expansion AND during `can_vec_perm_const_p` capability probing where `d->testing_p` is `true` and `gimple_bb (stmt)` may be `NULL`. Always check `d->testing_p` (and verify `stmt != NULL && gimple_bb (stmt) != NULL`) before querying `flow_bb_inside_loop_p` or mutating state.
- **Target Options, `--param` Precedence, and Multi-Target Macros**:
  - When modifying how `--param` or `-m` target options select vector modes or costs, verify that default/unset param values do not clobber the valid mode list for the current subtarget.
  - In shared frontend/middle-end code, guard target-specific feature hooks (such as Function Multi-Versioning `TARGET_HAS_FMV_TARGET_ATTRIBUTE`) so targets using different attribute models do not regress.

## 3. Machine Descriptions (`*.md`), Constraints, and Reload
- **Standard Optab Name Collisions (`addv<mode>3`, `subv<mode>3`, `mulv<mode>3`)**:
  - In `.md` files, pattern names like `addvsi3`, `subvsi3`, `usubvsi3`, `mulvsi3`, `negvsi2` are reserved standard optab names used by the middle-end for `-ftrapv` overflow-trapping arithmetic. Naming a non-trapping backend pattern under a standard `*v<mode>3` optab name silently overrides the trapping optab and breaks `-ftrapv` (`wrong-code`).
- **Instruction Attributes on C-Block Output Templates**:
  - In backends that derive instruction attributes (such as `"mnemonic"` or `"type"`) automatically from string output templates, converting a `define_insn` output template from a plain string `"..."` to a C block `"{ ... }"` requires explicitly setting the attribute in `(set_attr ...)`.
- **`can_create_pseudo_p ()` and `reload_completed`**:
  - Any `define_expand`, `define_split`, or helper that calls `gen_reg_rtx (mode)` or `force_reg` must ensure `can_create_pseudo_p ()` is true (or guard the `define_split` condition with `&& can_create_pseudo_p ()`).
- **Missing Clobbers, Earlyclobber (`=&`), and RTL Sharing**:
  - If a `define_insn` or `define_split` emits instructions that modify `CC_REG` / `FLAGS_REG` without `(clobber (reg:CC ...))`, `combine` or `sched` can move the insn across a live CC range (`wrong-code`).
  - If a multi-instruction output template writes to output operand `%0` before reading an input operand `%1` or `%2`, the output constraint MUST include `&` (`=&r`).
  - If an operand `operands[i]` can be a `MEM` or complex `rtx` and is referenced more than once in an emitted RTL sequence, wrap subsequent uses in `copy_rtx (operands[i])`.
