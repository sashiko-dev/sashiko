# LLVM False Positive Elimination Guide

Before reporting an issue in an LLVM patch, check against these standard design paradigms.

## 1. Internal Invariant Assertions
- LLVM makes heavy use of `assert(...)` to enforce internal compiler invariants guaranteed by preceding passes.
- Do NOT flag `assert(V && "Expected non-null value")` as a bug if the preceding logic or compiler pipeline
  strictly guarantees the condition.
- DO flag assertions if malformed source code or unusual IR (e.g. from `-O0` or hand-written bitcode) can trigger them.

## 2. Canonical Forms
- Transforms that convert equivalent expressions into a canonical representation (e.g. placing constants
  on the RHS of commutative binary operators) are intentional design patterns.
- Do not flag canonicalization transforms as redundant code.

## 3. Cost Model Trade-offs
- Heuristics in the Loop Vectorizer, Inliner, or MachineScheduler represent engineering trade-offs.
  Do not flag heuristic threshold adjustments as functional defects unless accompanied by a structural flaw
  (e.g., division by zero in cost computation, negative cost overflow).

## 4. Debug vs Release Separation
- Code guarded by `#ifndef NDEBUG` or `LLVM_DEBUG(...)` is omitted in release builds.
  Ensure side-effects necessary for program correctness are not accidentally placed inside `assert()`.
