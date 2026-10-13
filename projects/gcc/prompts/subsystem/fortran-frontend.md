# Fortran Frontend Invariants (`gcc/fortran/`, `libgfortran/`)

## 1. Symbol Linkage, `BIND(C)`, and ELF Visibility (`trans-decl.cc`, `resolve.cc`)
- **`PRIVATE` Module Entities with `BIND(C)` Binding Labels**:
  - In Fortran, the `PRIVATE` attribute on a module variable or module procedure restricts visibility of the **Fortran identifier** when the module is `USE`d.
  - However, if a module variable or procedure also has the `BIND(C)` attribute with a binding label (`sym->attr.is_bind_c && sym->binding_label`), the Fortran standard specifies that it has external C linkage under its binding label so C code or shared-library users can link against it.
  - Setting `DECL_VISIBILITY (decl) = VISIBILITY_HIDDEN` and `DECL_VISIBILITY_SPECIFIED (decl) = 1` on `PRIVATE` module variables (`gfc_finish_var_decl`) or module procedures (`build_function_decl`, `create_function_arglist`) **without exempting `BIND(C)` entities that have a binding label** hides the C symbol from shared libraries (`wrong-code` / link failure).
  - Always check all sites in `trans-decl.cc` that apply hidden visibility to `attr.access == ACCESS_PRIVATE` symbols and verify they check `!sym->attr.is_bind_c || !sym->binding_label`.

## 2. Frontend Expression Resolution & Null Safety (`resolve.cc`, `simplify.cc`, `expr.cc`)
- **Null `gfc_expr *`, `gfc_symbol *`, and `gfc_charlen *`**:
  - Deferred-length character types (`ts.u.cl->length == NULL`) and assumed-rank/assumed-shape arrays (`as->lower[i] == NULL` or `as->upper[i] == NULL`) frequently have `NULL` bound expressions. Always check `cl && cl->length && cl->length->expr_type == EXPR_CONSTANT` before reading `cl->length->value.integer`.
- **Memory Management of `gfc_expr` and `mpz_t`**:
  - Replacing a `gfc_expr *` pointer without calling `gfc_free_expr (old)` leaks `mpz_t` / `mpfr_t` allocations. Conversely, assigning an existing `gfc_expr *` into two places without `gfc_copy_expr` causes double-free corruption.

## 3. Array Descriptors, Outlined Regions, and Actual/Formal Association (`trans-array.cc`, `trans-decl.cc`, `interface.cc`)
- **Descriptor Fields in Outlined / OpenMP Regions (`trans-array.cc`, `trans-decl.cc`)**:
  - For assumed-shape, assumed-rank, or pointer/target dummy arrays, descriptor metadata (bounds, stride, offset, `span`, data pointer) is extracted into local `DECL` variables on procedure entry (`gfc_trans_dummy_array_bias`). When an element reference occurs inside an outlined region (such as an OpenMP `target`, `teams`, or `parallel` region), only the local `DECL` variables—not the outer dummy descriptor pointer itself—are mapped or captured by default. Reloading a field directly from the dummy descriptor at each element access instead of reading the local `DECL` variable causes unmapped memory accesses or ICEs inside outlined regions.
- **Scalar vs. Array Actual-Formal Storage Association (`interface.cc`)**:
  - In Fortran argument checking (`gfc_compare_actual_formal`), passing an array element `arr(i)` to an array dummy argument associates the remaining storage sequence starting at `arr(i)` (`get_expr_storage_size`). However, when the formal dummy argument is a **scalar** (`!f->sym->as`, such as a scalar `CHARACTER(len=N)`), only the single element `arr(i)` is passed—never use sequence-association storage size on a scalar dummy, or character length checks will use the entire remaining array size instead of the element length.
