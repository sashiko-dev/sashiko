+++
title = "Libbpf API"
paths = ["tools/lib/bpf/"]
symbols = ["LIBBPF_API", "libbpf_err", "libbpf_err_ptr", "libbpf_ptr"]
+++
# Libbpf API: maintainer notes

Coding conventions libbpf expects for error returns at the public API boundary.

- Public API functions (LIBBPF_API or listed in libbpf.map) report errors through the libbpf_err(), libbpf_err_ptr() or libbpf_ptr() wrappers rather than hand-rolled errno handling.
- Apply the wrapper on the return statement itself, not earlier in the function.
- Internal/static functions do not use these wrappers; they keep kernel-style negative error codes or ERR_PTR, and only the public API boundary converts.
- A public API error path that bypasses the wrappers is reported as a bug, not as a Low-severity style issue.
