+++
title = "BFD, opcodes, and shared format code"
paths = ["bfd/", "elfcpp/", "opcodes/", "include/", "libctf/", "libsframe/"]
symbols = ["bfd_*", "_bfd_*", "bfd_elf*", "print_insn_*"]
+++
# BFD and opcodes: maintainer notes

- BFD runs on untrusted object files: every size, offset, index and count read from the file must be validated before it drives an allocation, a copy or a pointer computation.
- Use the bfd_getl32/bfd_getb32 family according to the target endianness; a direct integer access through BFD structures is wrong for swapped or 64-bit targets.
- Reader and writer paths are symmetric: a change to a section or symbol representation must update the writer, the reader and any canonicalization in between.
- Relocations are applied with the target's howto table; do not assume a relocation size or field layout from the generic code.
- Output must not depend on hash-table iteration order or on whether a symbol was canonicalized; BFD output is expected to be reproducible.
- A new target or file format needs an entry in the format's backend and a test in bfd/testsuite, not only a local workaround.
- Do not move parsing diagnostics into library code silently; malformed input must fail with a diagnostic the tool can report.
