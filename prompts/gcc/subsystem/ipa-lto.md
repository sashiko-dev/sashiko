# IPA, Callgraph, LTO, and OpenMP Offloading Invariants (`gcc/ipa-*.cc`, `gcc/lto/`, `gcc/omp-*.cc`)

## 1. Pass Ordering and Duplicate Invocation Hazards (`lto.cc`, `passes.def`, `cgraphunit.cc`)
- **Moving Helper Calls Across Pipeline Phases**:
  - When a patch moves a pass step or symbol-processing helper to a different point in the pipeline or behind a conditional guard (e.g., so that it runs after static variable renaming or LTO symbol resolution), carefully check the entire function (using `git_read_files` if the full function is not in the diff hunk) to verify that the **original call site was actually removed**!
  - Leaving the original call behind causes the helper to run twice on every symbol/node (which can duplicate aliases, corrupt `DECL_VALUE_EXPR`, or trigger assertions).
- **Mutating `varpool` / `symtab` While Iterating (`FOR_EACH_VARIABLE`, `FOR_EACH_DEFINED_VARIABLE`)**:
  - During symbol-table walks, creating new variables or aliases (e.g., `varpool_node::create_extra_name_alias`, `varpool_node::add`, `cgraph_node::create`) or mutating linkage/attributes while iterating over the varpool/cgraph can mutate the underlying list during traversal or interact badly with re-invocations. Prefer collecting matching `varpool_node *` / `cgraph_node *` entries into an `auto_vec` during the loop and mutating/creating aliases after the loop finishes.

## 2. IPA Summary and Clone Materialization (`ipa-prop.cc`, `ipa-cp.cc`, `ipa-fnsummary.cc`)
- When a `cgraph_node` or `cgraph_edge` is cloned, redirected, or removed, IPA summaries (`ipa_node_params_sum`, `ipa_edge_args_sum`, `ipcp_transformation`) must be updated or duplicated consistently, and edge indices (`edge->lto_stmt_uid`) must remain synchronized with call statements.
- Always guard `node->get_body ()` or `DECL_STRUCT_FUNCTION (node->decl)` checks in IPA passes that run before LTO body materialization (`node->has_gimple_body_p ()`).
