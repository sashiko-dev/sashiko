+++
title = "GDB core"
paths = ["gdb/"]
symbols = ["gdb_*", "lookup_symbol*", "find_symbol*", "cooked_index*", "dwarf2_*"]
+++
# GDB: maintainer notes

- GDB must never let a malformed object file or debug info crash it: parsers in gdb/dwarf2, gdb/objfiles and gdb/symtab run on untrusted input.
- Symbol lookup and cooked-index changes must keep `.gdb_index` and `.debug_names` output deterministic and must not race with the finalization state.
- The C and C++ expression evaluators, the DWARF readers, and gdbserver duplicate parsing and conversion logic; a fix to one path usually needs its twin.
- Changes to MI, the Python API or CLI output are user-visible interfaces: check for consumers in testsuite and documentation.
- Async, event-loop and thread-state changes must be checked against all stop reasons and the remote protocol; a missing state transition shows up as a hang or a wrong stop.
- A behavior change needs a gdb/testsuite test using gdb_test/gdb_test_no_output; name the missing case precisely.
