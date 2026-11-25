# Severity Levels

Calibrate every validated finding to one of `Critical`, `High`, `Medium` or
`Low`, based on what the compiler does to the programs it compiles.

> A compiler runs on trusted source code, so a crash on malformed input is a
> quality bug, while silently miscompiling valid code injects a defect into
> every binary built with that compiler. Weight findings accordingly.

## Critical

**Silent miscompilation of valid code**, ABI breakage, or unsoundness that a
program can observe without any diagnostic:
- Wrong value or type produced by an implicit or explicit conversion, constant
  folding, or overload resolution (including enums with a fixed underlying
  type, fixed-point and complex conversions, `_Bool` semantics).
- Emitted IR that is not a valid implementation of the source semantics:
  `poison`/`undef` where the language is well defined, invalid
  `nsw`/`nuw`/`inbounds`/`!range`/`noundef`, out-of-range `fptoui`/`fptosi`.
- Optimization or codegen that changes observable behaviour: aliasing/TBAA
  metadata, `atomicrmw` vs `cmpxchg` selection that skips a required
  normalization, ABI or calling-convention mismatches, target features
  assumed but not checked.
- Any case where an assertion would fire on valid input: release builds do
  not have the assertion and will silently produce the wrong result.

## High

**Crashes on valid code**, verifier failures, or broken host-side invariants:
- `assert()`, `llvm_unreachable`, `cast<>` on the wrong class, null
  dereference, `APInt`/`APSInt` invariant violation (precision mismatch,
  `isIntN`, shift that does not fit), reachable from valid source.
- IR verifier failure or an invalid IR construct accepted by the frontend.
- Compiler hangs or unbounded recursion on valid input.

## Medium

**Diagnostics, crashes on already-invalid input, and compile-time issues**:
- Accepts-invalid or rejects-valid behaviour, wrong note/diagnostic.
- Crash on ill-formed input after an earlier error (missing `error` recovery,
  use of an invalid type/expression).
- Significant compile-time regression: exponential work in template
  instantiation, overload resolution or constant evaluation.
- Host-side memory bugs in the compiler that need unusual input to trigger.

## Low

Minor defects with no miscompilation and no crash:
- Imprecise diagnostic location or wording.
- Dead code, redundant checks, comment/format deviations that do not hide a
  bug.
- Missing test coverage for behaviour that is already correct.
