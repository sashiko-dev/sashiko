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
- BPF maintainers treat compiler- and linker-emitted ELF object input to `libbpf` as trusted; do not report input-validation gaps reachable solely through hand-crafted or compiler-impossible ELF sections (for example duplicate `SHT_REL` sections for one target section, which the `libbpf` static linker merges, empty relocation sections, or out-of-bounds relocation offsets never emitted by `clang`) unless the patch itself is specifically hardening `libbpf` against malformed or fuzzed ELF objects. Report gaps reachable from well-formed objects, or from data `libbpf` reads from the kernel, under `severity.md` as usual.
