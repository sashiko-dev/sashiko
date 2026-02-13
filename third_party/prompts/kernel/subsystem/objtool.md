# Objtool Subsystem Details

## Instruction Classification Semantics

Misclassifying an instruction in `tools/objtool/arch/*/decode.c` corrupts
objtool's control-flow analysis: an `INSN_BUG` misclassified as `INSN_TRAP`
loses dead-end propagation and may cause false "unreachable instruction"
warnings to be suppressed. An `INSN_TRAP` misclassified as `INSN_BUG` makes
objtool treat every subsequent instruction as dead code, hiding real problems.

`INSN_BUG` and `INSN_TRAP` have different effects in
`tools/objtool/check.c`:

- `INSN_BUG` sets `dead_end = true` on the instruction in
  `decode_instructions()`, marking all subsequent code as unreachable. It is
  also used in `validate_retpoline()` to recognize indirect-call protection
  patterns and in `ignore_unreachable_insn()` to suppress warnings for code
  following a known dead end.
- `INSN_TRAP` is treated as ignorable padding in `ignore_unreachable_insn()`
  (like `INSN_NOP`). It is required after `ret` and indirect jumps in
  `validate_sls()` for Straight-Line Speculation mitigation.

Architecture-specific mappings in `tools/objtool/arch/*/decode.c`:

| Architecture | `INSN_BUG` | `INSN_TRAP` |
|---|---|---|
| x86 | `ud2`, `ud1`, `udb` | `int3` |
| LoongArch | `amswap.w $zero, $ra, $zero`; `break 0x1` | `break 0x0` |

## Compiler-Generated Trap Instructions

Reclassifying how objtool handles a specific machine instruction can expose
compiler-generated instances of that same instruction, causing spurious
objtool warnings in otherwise correct builds.

GCC optimization passes that generate trap/break instructions independently
of explicit kernel code:

- `-fisolate-erroneous-paths-dereference` (enabled by `-O2`): GCC inserts
  trap instructions on code paths it proves will dereference null pointers
- `-fsanitize=unreachable`, `-fsanitize=undefined`: sanitizers insert trap
  instructions at provably undefined behavior

When objtool classification changes, the architecture Makefile
(`arch/*/Makefile`) may need corresponding compiler-flag changes to suppress
unwanted instruction generation. For example, `arch/loongarch/Makefile` sets
both `-mno-check-zero-division` and `-fno-isolate-erroneous-paths-dereference`
to prevent GCC from generating `break` instructions that would be
misinterpreted by objtool.

## Quick Checks

- Instruction decoder changes in `tools/objtool/arch/*/decode.c` may require
  corresponding compiler-flag changes in `arch/*/Makefile`
- The instruction classification must match how the architecture's trap
  handler processes the instruction at runtime (e.g., `break 0x1` on
  LoongArch triggers `BUG()` handling, matching its `INSN_BUG` classification)
