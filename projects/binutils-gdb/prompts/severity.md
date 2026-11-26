# Severity Levels

Calibrate every validated finding to one of `Critical`, `High`, `Medium` or
`Low`, based on what the toolchain does to the programs and files it
processes.

> binutils and GDB process untrusted object files, debug info and remote
> packets, so a memory-safety bug in a parser is a security issue. Silent
> wrong output injects a defect into every object or binary produced with the
> tool. Weight findings accordingly.

## Critical

**Silent wrong output**, memory corruption reachable from untrusted input, or
security-relevant state corruption without any diagnostic:
- Wrong object, debug info, disassembly, relocation or linked binary for a
  valid input.
- Out-of-bounds read/write, use-after-free, double free, or integer overflow
  in a parser or writer reachable from a malformed object, debug info,
  archive, CTF, BPF or remote packet.
- Debugger state corruption that makes the inferior run differently from the
  program under debug, or that silently returns a wrong value.
- Any case where an assertion would fire on valid input: release builds omit
  the assertion and silently misbehave.

## High

**Crashes on valid input**, build/link failures, or broken host-side
invariants:
- `bfd_assert`/`gdb_assert` failure, `internal_error`, null dereference,
  `alloca`/stack overflow, or infinite loop reachable from a valid input.
- Linker or assembler rejecting valid input, or producing an unusable
  object.
- GDB failing to start, load symbols, or set a breakpoint on valid debug
  info.

## Medium

**Diagnostics, crashes on already-invalid input, and compile-time issues**:
- Wrong or missing warning/error, or a diagnostic emitted at the wrong
  location.
- Crash on malformed input after an earlier diagnostic.
- Significant slowdown in a reader, linker pass, or symbol lookup.
- Host-side resource leaks.

## Low

Minor defects with no wrong output and no crash:
- Imprecise diagnostic wording.
- Dead code or redundant checks that do not hide a bug.
- Missing test coverage for behavior that is already correct.
