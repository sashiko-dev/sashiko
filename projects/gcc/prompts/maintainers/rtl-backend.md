+++
title = "RTL Passes and Target Backends"
paths = ["gcc/config/", "gcc/combine.cc", "gcc/ifcvt.cc", "gcc/simplify-rtx.cc", "gcc/lra*.cc", "gcc/ira*.cc", "gcc/expmed.cc", "gcc/expr.cc", "gcc/optabs.cc"]
symbols = ["reversed_comparison_code", "can_create_pseudo_p"]
+++
# RTL Passes and Target Backends: maintainer notes

Conventions and review priorities for GCC RTL optimization passes and target backends (`gcc/config/`).

- Reserve standard overflow-trapping optab names (`addv<mode>3`, `subv<mode>3`, `usubv<mode>3`, `mulv<mode>3`, `negv<mode>2`) exclusively for instruction patterns that trap on overflow as required by `-ftrapv`; non-trapping backend patterns must use target-prefixed or non-optab pattern names.
- Prefer `poly_int`-aware predicates (`known_eq`, `known_lt`, `known_le`, `maybe_eq`, `ordered_p`) over `.to_constant ()` in target-independent RTL and middle-end code so targets with runtime-sized vector modes remain supported.
