# LLVM False Positive Prevention Guide

Check these conventions before validating a finding, so that real defects are
not rationalized away and non-bugs are not reported.

## 1. Patterns that are NOT bugs

1. **Invariants established immediately before the use**: an `assert()` or a
   `cast<>` that is guaranteed by the preceding construction (a `dyn_cast`
   that just succeeded, a `getType()` that was produced by the matching
   builder call) is fine. Report it only if some input can reach the
   assertion or the wrong cast from valid source.
2. **NFC and pure refactors**: commits that claim NFC do not need new tests
   and do not need release notes. Only report a missing test when behaviour
   changes.
3. **Release notes**: clang asks for a release note for user-visible changes
   (new diagnostic, accepted/rejected code, new flag). Do not ask for one for
   internal refactoring, test-only changes or fixes of a crash that produced
   no output change.
4. **Formatting, naming, include order**: enforced by CI and local style.
   Never report these as findings.
5. **Test inputs**: `clang/test/**`, `llvm/test/**` files intentionally
   contain invalid, weird or undefined constructs. Never report a defect
   inside them; use them as documentation of the intended behaviour.
6. **`llvm_unreachable` in a fully handled switch**: unless a new enum
   value or a new node kind makes it reachable, that is a design decision,
   not a bug.
7. **Language extensions and target-specific behaviour**: clang implements C
   and C++ plus documented extensions (C23, `-ffixed-point`, `_BitInt`,
   HLSL, OpenCL, CUDA). Behaviour that differs from the ISO text only where
   the extension says so is not a bug.
8. **Existing upstream behaviour**: a defect that predates the patch is not a
   regression of this patch, so do not report it as one. Do not drop it either:
   if the patch touches the code or its immediate callers and the defect is
   real, report it with `"preexisting": true` so it is tracked separately from
   the patch's own regressions.

## 2. Real bugs you must NOT dismiss

1. **The symmetric path is left unfixed**: clang usually has more than one
   implementation of the same conversion (Sema vs CodeGen, classic CodeGen vs
   CIR, constant evaluator vs runtime, C vs C++ frontend). If a patch fixes
   one and misses its twin in the same change, that is a finding.
2. **Assertions versus release builds**: an assertion that fires from valid
   code means release builds silently misbehave. Never dismiss it as "just a
   debug assertion".
3. **`isBooleanType()` is not the whole story**: an enumeration with a fixed
   `bool` underlying type converts like `bool` (C23 6.7.3.3p16) but is not
   `isBooleanType()`. Any check that gates on `isBooleanType()` and drives a
   conversion, a normalization or a fast path has to handle such enums too.
4. **Missing normalization after an operation**: a value of bool-like
   representation must be renormalized after arithmetic. Fast paths
   (`atomicrmw`, direct adds) that skip the conversion back through the bool
   representation produce non-canonical objects that later reads or
   `memcpy`/union access can observe.
5. **Sanitizers firing on defined code**: a UBSan/ASan check that fires on a
   conversion the standard defines (for example converting any scalar to
   `_Bool`) is itself the bug.
6. **Diagnostics on one language mode only**: a fix that works in C but not
   in C++, or under `-fms-compatibility`, is incomplete unless the patch says
   why.
7. **Duplicated or leftover code**: a call or state update moved under a
   guard while the original unguarded call remains, or a fix applied to one
   operator/overload and not its sibling, is a real bug.
