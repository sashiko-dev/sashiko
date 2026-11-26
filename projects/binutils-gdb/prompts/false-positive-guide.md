# binutils-gdb False Positive Prevention Guide

Check these conventions before validating a finding, so that real defects are
not rationalized away and non-bugs are not reported.

## 1. Patterns that are NOT bugs

1. **Invariants established immediately before the use**: an assertion or a
   cast guaranteed by the preceding validation (a checked `bfd_getl32` size,
   a section already marked present, a `dyn_cast` that just succeeded) is
   fine. Report it only if malformed or valid input can reach it.
2. **Test inputs**: files under `*/testsuite/` intentionally contain
   malformed, invalid or undefined input. Never report a defect inside them;
   use them as documentation of the intended behavior.
3. **Formatting and style**: indentation, line wrapping, ChangeLog entries and
   comment wording are not findings unless they hide a behavior change.
4. **Target-specific behavior**: a patch that only changes one architecture
   or target is not wrong merely because it does not touch other targets.
   Ask instead whether the source-level semantics are preserved per target.
5. **Existing upstream behavior**: a defect that predates the patch is not a
   regression of this patch, so do not report it as one. Do not drop it
   either: if the patch touches the code or its immediate callers and the
   defect is real, report it with `"preexisting": true` so it is tracked
   separately.
6. **Release vs assertion builds**: an assertion that fires from valid input
   means release builds silently misbehave. Never dismiss it as "just a
   debug assertion".
7. **Diagnostics text**: a changed error or warning message is not a bug
   unless a test, script or documented interface depends on the exact text.

## 2. Real bugs you must NOT dismiss

1. **The symmetric path is left unfixed**: reader vs writer, gas target vs
   generic, C vs C++ expression evaluation, gdb vs gdbserver, x86 vs AArch64
   lowering. A fix applied to one and not its twin in the same change is a
   finding.
2. **Malformed-input hardening is security work**: an unchecked size, a
   missing bounds check, or an overflow in a BFD/DWARF parser is a real
   defect even if the input is "unusual"; these parsers run on untrusted
   files.
3. **Nondeterministic output**: output that depends on hash-table or worker
   iteration order breaks reproducible builds and the testsuite; report it.
4. **Duplicated or leftover code**: a call or state update moved under a guard
   while the original unguarded call remains, or a fix applied to one
   operator/reader and not its sibling, is a real bug.
5. **Wrong-object output**: an object or debug file that is structurally
   valid but describes different code than the source is Critical, even if
   every tool still loads it.
