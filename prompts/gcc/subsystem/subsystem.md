# GCC Subsystem Guide Index

Load guides from the prompt directory based on what the patch touches. Each
guide holds the invariants, IR contracts, and historical bug patterns for one
area of GCC.

A change can match several rows. Load **every** matching guide, not only the
most specific one.

## Subsystem Guides

| Component | Triggers | File |
|-----------|----------|------|
| GIMPLE, SSA, VRP, Ranger, and Vectorizer | `gcc/tree-ssa-*.cc`, `gcc/tree-vect-*.cc`, `gcc/gimple*.cc`, `gcc/value-range*`, `gcc/range-op*`, `gcc/vr-values*`, `gcc/tree-cfg.cc`, `gcc/cfg*.cc`, `SSA_NAME`, `gimple`, `reset_flow_sensitive_info`, `update_stmt`, `unlink_stmt_vdef`, `poly_int` | gimple-ssa.md |
| Algebraic Folding, match.pd, and wide_int | `gcc/match.pd`, `gcc/fold-const.cc`, `gcc/wide-int*`, `gcc/generic-match*`, `gcc/gimple-match*`, `wi::`, `wide_int`, `widest_int`, `TYPE_OVERFLOW_UNDEFINED`, `TYPE_OVERFLOW_WRAPS`, `HONOR_NANS`, `HONOR_SIGNED_ZEROS` | match-fold.md |
| RTL Passes and Target Backends | `gcc/config/`, `*.md`, `gcc/combine.cc`, `gcc/lra*.cc`, `gcc/ira*.cc`, `gcc/cse.cc`, `gcc/ifcvt.cc`, `gcc/simplify-rtx.cc`, `gcc/explow.cc`, `gcc/expmed.cc`, `gcc/expr.cc`, `gcc/optabs.cc`, `rtx`, `RTX_CODE`, `reversed_comparison_code`, `SUBREG`, `can_create_pseudo_p` | rtl-backend.md |
| C and C++ Frontends | `gcc/cp/`, `gcc/c/`, `gcc/c-family/`, `libcpp/`, `tsubst`, `complain`, `tf_warning_or_error`, `error_mark_node`, `error_operand_p`, `constexpr`, `CONSTRUCTOR`, `unshare_constructor`, `DECL_SOURCE_LOCATION`, `location_of` | c-cpp-frontend.md |
| Fortran Frontend and libgfortran | `gcc/fortran/`, `libgfortran/`, `gfc_`, `trans-decl.cc`, `resolve.cc`, `BIND(C)`, `is_bind_c`, `binding_label`, `VISIBILITY_HIDDEN` | fortran-frontend.md |
| IPA, Callgraph, LTO, and OpenMP Offloading | `gcc/ipa-*.cc`, `gcc/cgraph*.cc`, `gcc/varpool.cc`, `gcc/lto/`, `gcc/omp-*.cc`, `lto.cc`, `cgraph_node`, `varpool_node`, `offload_handle_link_vars`, `symtab_node` | ipa-lto.md |
| GGC, Precompiled Headers, Containers, and Host Safety | `GTY`, `ggc`, `gcc/vec.h`, `gcc/hash-table.h`, `gcc/hash-map.h`, `gcc/bitmap.h`, `auto_vec`, `safe_push`, `BITMAP_ALLOC`, `HOST_WIDE_INT` | ggc-memory.md |
| C++ Standard Library (libstdc++) | `libstdc++-v3/`, `std::ranges`, `std::views`, `std::format`, `_GLIBCXX_` | libstdcxx.md |
