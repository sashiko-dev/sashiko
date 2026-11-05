+++
title = "Libbpf API"
paths = ["tools/lib/bpf/"]
symbols = ["LIBBPF_API", "libbpf_err", "libbpf_err_ptr", "libbpf_ptr"]
+++
# Libbpf API: maintainer notes

Coding conventions libbpf expects for error returns at the public API boundary, and how to rate malformed-object input.

- Public API functions (LIBBPF_API or listed in libbpf.map) report errors through the libbpf_err(), libbpf_err_ptr() or libbpf_ptr() wrappers rather than hand-rolled errno handling.
- Apply the wrapper on the return statement itself, not earlier in the function.
- Internal/static functions do not use these wrappers; they keep kernel-style negative error codes or ERR_PTR, and only the public API boundary converts.
- A public API error path that bypasses the wrappers is reported as a bug, not as a Low-severity style issue.
- Input-validation gaps (out-of-bounds reads, leaks, NULL dereferences, failed allocations) reachable only through a malformed or hand-crafted ELF object (for example duplicate SHT_REL sections for one section, which the libbpf static linker merges, or empty relocation sections) are hardening issues: report them as Low at most, never as security issues. BPF maintainers treat libbpf object input as trusted, while the libbpf maintainers still take fixes for malformed or fuzzed objects, so do not drop such findings. Rate a gap reachable from well-formed objects, or from data libbpf reads from the kernel, under severity.md as usual.
