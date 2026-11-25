+++
title = "Clang Frontend (AST and Sema)"
paths = ["clang/lib/Sema/", "clang/lib/AST/", "clang/lib/Parse/", "clang/include/clang/AST/", "clang/include/clang/Sema/"]
symbols = ["Sema", "QualType", "CastKind"]
+++
# Clang Frontend: maintainer notes

Review guidance for patches touching clang parsing, the AST, and semantic
analysis.

- A conversion must pick the `CastKind` that matches the standard's wording for that source and destination type; `CK_IntegralCast` where a boolean normalization is required silently changes behaviour.
- An enumeration with a fixed underlying type (C23 6.7.3.3, C++11 `enum class` with a `bool` underlying type) converts like its underlying type even when `isBooleanType()` is false; any check gating a conversion, fast path, or normalization on `isBooleanType()` must account for it.
- After diagnosing, Sema recovers with an invalid type or expression rather than by stopping. Code that consumes a recovered expression must check `isInvalidType()` or `containsErrors()` before asserting on the type.
- A diagnostic emitted while forming a template argument or during deduction must go through the SFINAE-aware path: `Sema::Diag` inside an `SFINAETrap`, suppressed when `isSFINAEContext()` is true, or deferred as a `DelayedDiagnostic`. Reporting unconditionally turns a substitution failure into a hard error.
- CodeGen and CIR re-implement parts of the frontend's decisions. When a patch changes `ScalarExprEmitter`, `CGExprScalar`, `CIRGen`, or the constant evaluator, check the counterpart implementation for the same input.
- The `-verify` diagnostics tests assert the exact message text, location, and `expected-` annotation; a diagnostic change that does not update them fails the suite, and a test that only updates the text without adding the triggering case does not cover the new behaviour.
