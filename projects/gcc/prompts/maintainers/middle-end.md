+++
title = "GIMPLE, SSA, and Algebraic Folding"
paths = ["gcc/match.pd", "gcc/fold-const.cc", "gcc/tree-ssa-*.cc", "gcc/tree-vect-*.cc", "gcc/gimple*.cc", "gcc/value-range*", "gcc/range-op*"]
symbols = ["reset_flow_sensitive_info*", "unlink_stmt_vdef"]
+++
# GIMPLE, SSA, and Algebraic Folding: maintainer notes

Conventions and review priorities for GCC middle-end GIMPLE/SSA passes and algebraic folding.

- Prefer declarative patterns in `gcc/match.pd` over adding new ad-hoc expression simplifications to `fold-const.cc` or pass-local folders when the identity is general.
- Prefer the `Value_Range` and Ranger query interfaces (`range_of_expr`, `range_of_stmt`) over legacy type-specific or global range queries in new pass logic.
- All GIMPLE and SSA passes must maintain strict `-fcompare-debug` invariance: `GIMPLE_DEBUG` statements (`is_gimple_debug`) and hash-table iteration over raw pointer addresses must never alter optimization decisions, statement counts, basic-block ordering, or `DECL_UID` / `SSA_NAME_VERSION` allocation.
