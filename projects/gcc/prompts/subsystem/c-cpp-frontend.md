# C and C++ Frontend Invariants (`gcc/cp/`, `gcc/c/`, `gcc/c-family/`)

## 1. Tree Node Types and Checked Accessors (`TREE_CHECK` / `DECL_CHECK` / `TYPE_CHECK`)
- GCC builds with `--enable-checking`, which aborts (`tree_check_failed`) if a `DECL_*` macro is applied to a `TYPE` or `EXPR`, or vice versa:
  - **`DECL_SOURCE_LOCATION (t)` vs. `location_of (t)`**: `DECL_SOURCE_LOCATION (t)` asserts `DECL_P (t)`. If `t` can be a `TYPE_P` (`RECORD_TYPE`, `UNION_TYPE`, `ENUMERAL_TYPE`) or `IDENTIFIER_NODE`, calling `DECL_SOURCE_LOCATION (t)` immediately ICEs! Use `location_of (const_cast<tree> (t))` or `DECL_SOURCE_LOCATION (TYPE_MAIN_DECL (t))` when `t` can be a type.
  - **`TYPE_MAIN_VARIANT (t)`**: Asserts `TYPE_P (t)`. Never pass a `DECL` or `error_mark_node` to `TYPE_MAIN_VARIANT`.
  - **`DECL_CONTEXT (t)` / `DECL_NAME (t)`**: Assert `DECL_P (t)` (use `TYPE_CONTEXT (t)` / `TYPE_NAME (t)` on types).

## 2. `error_mark_node` Recovery & SFINAE (`tsubst_flags_t complain`)
- **Propagating `error_mark_node`**:
  - Any frontend function receiving a `tree` from parsing, lookup, overload resolution, or template substitution (`tsubst*`, `cp_build_*`, `cxx_eval_*`, `grokdeclarator`) must check `if (t == error_mark_node || error_operand_p (t)) return error_mark_node;` before accessing `TREE_TYPE (t)`, `DECL_*`, or `TYPE_*`.
- **Respecting `complain & tf_error` / `tf_warning`**:
  - In `gcc/cp/`, any function taking `tsubst_flags_t complain` runs during SFINAE when `!(complain & tf_error)`. Calling `error (...)`, `error_at (...)`, or `permerror (...)` without `if (complain & tf_error)` (or `warning` without `if (complain & tf_warning)`) turns a SFINAE substitution failure into a hard error (`rejects-valid`).

## 3. Template Substitution (`pt.cc`) & Specialization Table Re-Entrancy
- **Recursive Specialization Registration**:
  - During `lookup_template_class`, `tsubst_decl`, or `instantiate_decl`, recursive calls (such as `complete_type` or `tsubst` on the enclosing context or default arguments) can instantiate and register the very specialization currently being constructed into `decl_specializations` or `type_specializations`. Always re-check the specialization table after recursive substitution before inserting a duplicate entry.
- **Avoid Premature `ggc_free` of Template Arguments**:
  - Never call `ggc_free` on a `targs` `TREE_VEC` after a substitution or deduction helper (`tsubst`, `unify`, `try_class_unification`) if any created type, specialization, or diagnostic cache could retain a pointer to `targs`.
- **`processing_template_decl` & Dependent Contexts**:
  - Do not unconditionally increment or set `processing_template_decl` during `tsubst` unless the arguments still depend on template parameters (`uses_template_parms`), and always restore `processing_template_decl` via RAII (`processing_template_decl_sentinel`) on early exits.
- **Opaque Alias Templates & Structural Equality**:
  - Alias templates whose template arguments involve `LAMBDA_EXPR` or structural-equality NTTPs (`any_template_arguments_need_structural_equality_p`) must not be eagerly stripped by `strip_typedefs` across template nesting levels.

## 4. `constexpr` Evaluation & `CONSTRUCTOR` Invariants (`constexpr.cc`, `init.cc`, `tree.cc`)
- **`constexpr_ctx::ctor` and Subobject Alignment**:
  - When `cxx_eval_*` descends into a subobject (member, base class, array element, or anonymous union member), `ctx->ctor` and `ctx->object` must match the subobject being initialized (or be cleared for empty bases / updated when activating a union member), otherwise inner stores corrupt the enclosing `CONSTRUCTOR`.
- **Tree Sharing in `CONSTRUCTOR` Nodes**:
  - A `CONSTRUCTOR`'s `constructor_elt` has both `.index` and `.value`. When `.index` is a `RANGE_EXPR` (`[lo ... hi]`), it is a non-constant expression node that must also be unshared by `unshare_constructor` / `unshare_expr` before in-place mutation or gimplification.
- **Zero-Sized Array Domains (`[0, -1]`)**:
  - Zero-sized arrays (`T a[0]` or `{}`) represent their domain with `maxindex = ssize_int (-1)`. Building an index type with `build_index_type (maxindex)` converts `-1` to unsigned `sizetype` (`SIZE_MAX`); use `build_range_type (sizetype, size_zero_node, maxindex)` when `maxindex` can be `-1`.

## 5. Redeclaration Merging & C++20 Modules (`decl.cc`, `module.cc`)
- **`DECL_LANG_SPECIFIC` Allocation (`retrofit_lang_decl`)**:
  - Ordinary `VAR_DECL`s and non-template declarations are created with `DECL_LANG_SPECIFIC (decl) == NULL`. When a redeclaration adds `inline`, `constexpr`, `thread_local`, or module flags, `DECL_LANG_SPECIFIC (newdecl)` is non-NULL while `DECL_LANG_SPECIFIC (olddecl)` may still be `NULL`. Always call `retrofit_lang_decl (olddecl)` before accessing `DECL_LANG_SPECIFIC (olddecl)`.
- **TU-Local Entities and Linkage Scope**:
  - Anonymous namespaces (`!DECL_NAME (ns)`) and internal-linkage declarations are TU-local and must not be exported into C++20 module interfaces, and linkage rules for unnamed types (such as P2115R0 unnamed enums) must check `TYPE_NAMESPACE_SCOPE_P` rather than applying to class-scope members.
