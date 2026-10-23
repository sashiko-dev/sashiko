+++
title = "C and C++ Frontends"
paths = ["gcc/cp/", "gcc/c/", "gcc/c-family/", "libcpp/"]
symbols = ["tsubst_*", "cp_build_*", "cxx_eval_*"]
+++
# C and C++ Frontends: maintainer notes

Conventions and review priorities for the GCC C and C++ frontends (`gcc/cp/`, `gcc/c/`, `gcc/c-family/`, `libcpp/`).

- Prefer RAII sentinels (`processing_template_decl_sentinel`, `iloc_sentinel`, `warning_sentinel`, `temp_override`) over manual save/restore of global frontend state across functions with early returns.
- In C++ frontend code parameterized by `tsubst_flags_t complain`, gate all diagnostic emissions (`error`, `error_at`, `permerror`, `pedwarn`, `warning`, `warning_at`) on `complain & tf_error` or `complain & tf_warning` so SFINAE substitution failures never become hard errors.
- Prefer explicit AST location helpers (`location_of`, `EXPR_LOCATION`, `DECL_SOURCE_LOCATION`) and `auto_diagnostic_group` over relying on global `input_location` when adding or updating diagnostics.
