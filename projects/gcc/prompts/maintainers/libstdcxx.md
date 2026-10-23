+++
title = "C++ Standard Library (libstdc++)"
paths = ["libstdc++-v3/"]
symbols = ["_GLIBCXX_*"]
+++
# C++ Standard Library (libstdc++): maintainer notes

Conventions and review priorities for `libstdc++-v3`.

- Every non-standard identifier in installed `libstdc++-v3/include/` headers (template parameters, local variables, member names, and helper functions) must use reserved uglyfied names (`__foo`, `_Tp`, `_M_bar`, `_S_baz`) so user-defined macros cannot collide with standard headers.
- Preserve `libstdc++.so` ABI stability: do not alter the size, layout, virtual table, or mangled signature of exported symbols in stable ABI namespaces without an explicit ABI version guard.
