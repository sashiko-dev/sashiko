# LLVM Severity Levels

When identifying issues in LLVM compiler patches, you must assign a severity level to each finding.
Don't unnecessarily raise priority; Critical must be critical, High must be very damaging.
Use Medium as default and calibrate using consequence, reachability, and triggering conditions.

## Calibrating the level (reason before you label)

State this reasoning at the start of the `severity_explanation` so the label is auditable.

- Consequence: What actually happens (silent miscompilation/wrong code, compiler crash/assert failure,
  infinite optimization loop/hang, invalid IR/verifier failure, ABI breakage, diagnostic quality).
- Triggering path: Name the IR pattern, optimization pass, target architecture, or source construct.
  If resting on unproven assumptions, report the finding and mark it speculative (capped at Medium).
- Reachability: If reachable on standard, valid input code under common optimization flags (-O2/-O3),
  raise the level.

## Critical
- **Definition**: Silent wrong-code generation (miscompilation) on valid source code without undefined behavior.
- **Question to ask**: Does this change silently alter the semantics of valid user programs, resulting in incorrect runtime execution without any warning? If yes, it's Critical.
- **Examples**:
    - Soundness bugs in InstCombine/SimplifyCFG/EarlyCSE generating incorrect values.
    - Undefined/poison preservation violations (e.g., dropping `nsw`/`nuw` or converting poison to arbitrary value incorrectly).
    - Incorrect type bitwidth or sign handling in constant folding or arithmetic rewrites.
    - SSA dominance violations or invalid phi node placement causing wrong value propagation.
    - Target backend lowering generating wrong machine instructions or clobbering live registers.
    - ABI breakage (calling convention or argument passing mismatch) causing silent corruption.

## High
- **Definition**: Compiler crash, assertion failure on valid code, infinite loop, or invalid IR generation.
- **Question to ask**: Can this bug crash the compiler, trigger an assertion in debug/release builds, hang the compiler indefinitely, or fail the LLVM module verifier on valid input? If yes, it's High.
- **Examples**:
    - Compiler assertion failure (`assert(...)`) triggered by valid IR or valid frontend constructs.
    - Segmentation fault / crash via unchecked `dyn_cast<T>` returning null.
    - Iterator invalidation resulting in use-after-free during pass transformations.
    - Infinite transformation loop (ping-pong between two opposing InstCombine/DAGCombines).
    - Emitting malformed LLVM IR that fails `llvm::verifyModule()` or `llvm::verifyFunction()`.
    - Memory corruption within LLVM data structures during compilation.

## Medium
- **Definition**: Optimization regressions, cold-path leaks, missed canonicalizations, poor diagnostics.
- **Examples**:
    - Optimization regression (generating slower code or blocking subsequent optimizations).
    - Missing commutativity check in pattern matching leading to missed optimization opportunities.
    - Memory leak in analysis or pass data structures.
    - Misleading or imprecise diagnostic / error recovery in Clang frontend.
    - Missing test coverage for modified transformation branches.
    - Speculative findings where the triggering IR pattern is unproven.

## Low
- **Definition**: Code style, naming, comments, minor refactorings.
- **Examples**:
    - Coding standards violations (LLVM Coding Standards: naming conventions, 80-column rule).
    - Unnecessary `auto` or explicit types where idiom prefers the other.
    - Typos in comments or diagnostics.
    - Redundant header includes or unused helper functions.
