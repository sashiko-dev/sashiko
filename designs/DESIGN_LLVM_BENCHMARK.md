# Design: LLVM Bug Benchmark Dataset

## Objective
To establish a large-scale ground truth benchmark dataset of 500 historically known defects in the LLVM Project codebase (`llvm-project`). This dataset enables Sashiko to evaluate automated code review accuracy, false positive rates, and defect detection capabilities across compiler middle-end transformations, frontends, linkers, runtime libraries, and target code generation.

## Background
Sashiko utilizes ground truth benchmarks (e.g. Linux kernel and QEMU benchmarks) to measure code review effectiveness against real defects. LLVM is a premier compiler infrastructure project written in modern C++, comprising diverse components with distinct correctness and performance requirements.

In LLVM development, regressions and defects introduced by earlier commits are tracked via inline commit references, regression descriptions (e.g. `Fixes <commit>`, `Regression introduced by <commit>`, `Caused by <commit>`), and GitHub issue resolutions. By mining historical pairs of bug-introducing commits and their subsequent fix commits, we extract genuine defects that were committed to the repository and later corrected.

## Schema Definition
The dataset resides at `benchmarks/llvm/benchmark.json` and adheres to Sashiko's unified `BenchmarkEntry` format:

```json
[
  {
    "Commit": "<40-character SHA of bug-introducing commit>",
    "Fixed-by": "<40-character SHA of bug-fixing commit>",
    "subsystem": "<canonical subsystem, e.g. clang, llvm/InstCombine, lld>",
    "problem_description": "<precise description of defect and failure condition>"
  }
]
```

### Fields
- **Commit**: The 40-character git commit hash in the LLVM repository that introduced the regression or defect. This commit is submitted to Sashiko for automated review.
- **Fixed-by**: The 40-character git commit hash that resolved the issue.
- **subsystem**: The component area within the LLVM monorepo (e.g. `clang`, `llvm/InstCombine`, `llvm/Vectorize`, `llvm/CodeGen`, `llvm/Target/AArch64`, `llvm/Target/RISCV`, `llvm/Target/X86`, `lld`, `lldb`, `mlir`, `compiler-rt`, `flang`, `libcxx`).
- **problem_description**: A declarative summary explaining the defect, affected classes/functions, and failure mode (crash, assertion failure, miscompilation, infinite loop, type mismatch, memory safety).

## Selection and Curation Criteria
To ensure high benchmark fidelity across 500 entities:

1. **Commit Integrity**: Both `Commit` and `Fixed-by` must resolve to valid, distinct commit objects in the LLVM repository. All 500 bug-introducing commits and fix commits are unique.
2. **Compiler Code Focus**: Only commits modifying compiler and runtime source code (`.cpp`, `.c`, `.h`, `.inc`, `.td`) are included. Mechanical Bazel build file synchronizations, documentation updates, and test-only modifications are excluded.
3. **Subsystem Diversity**:
   - Compiler frontends: `clang`, `flang`
   - Optimization passes: `llvm/InstCombine`, `llvm/Vectorize`, `llvm/Scalar`, `llvm/Analysis`
   - Code generation and backends: `llvm/CodeGen`, `llvm/Target/AArch64`, `llvm/Target/RISCV`, `llvm/Target/X86`, `llvm/Target/AMDGPU`, `llvm/Target/PowerPC`
   - Low-level tools and runtimes: `lld`, `lldb`, `mlir`, `compiler-rt`, `libcxx`
4. **Defect Categorization**:
   - Compiler crashes and NULL/uninitialized pointer dereferences (e.g. invalid `dyn_cast` assumptions).
   - Assertion failures in optimization passes or instruction selectors.
   - Miscompilations and unsound algebraic rewrites (e.g. invalid `ConstantRange` deduction, incorrect DAG combinations).
   - Infinite optimization loops and termination failures.
   - Memory safety and lifetime bugs within compiler AST/IR transformations.

## Validation Workflow
Evaluation is executed using Sashiko's benchmark binary:

```bash
cargo run --bin benchmark -- \
  --file benchmarks/llvm/benchmark.json \
  --repo <path-to-llvm-project>
```
