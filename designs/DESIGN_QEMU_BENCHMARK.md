# Design: QEMU Bug Benchmark Dataset

## Objective
To extend Sashiko's review evaluation capabilities beyond the Linux kernel by establishing a benchmark dataset of 500 historically known bugs in the QEMU codebase. This dataset serves as ground truth to measure Sashiko's bug detection accuracy, false positive rates, and review quality on QEMU changes.

## Background
Sashiko utilizes ground truth benchmarks (such as `benchmarks/benchmark.json` and `benchmarks/benchmark_small.json`) to validate automated code reviews against real-world defects.

QEMU adheres to patch submission standards similar to the Linux kernel, employing `Fixes:` trailers in commit messages to reference the commit that introduced a defect. By mining these historical pairs of bug-introducing commits and bug-fixing commits, we extract genuine bugs that escaped initial review and were subsequently identified and resolved.

## Schema Definition
The QEMU benchmark dataset resides in `benchmarks/qemu/benchmark.json` and adheres to the standard `BenchmarkEntry` schema defined in `src/bin/benchmark.rs`:

```json
[
  {
    "Commit": "<40-character SHA of bug-introducing commit>",
    "Fixed-by": "<40-character SHA of bug-fixing commit>",
    "subsystem": "<canonical subsystem or device module>",
    "problem_description": "<precise description of defect and failure condition>"
  }
]
```

### Fields
- **Commit**: The full git commit object ID in QEMU that introduced the defect or contains the unpatched vulnerability/bug. When evaluating, Sashiko reviews this commit diff.
- **Fixed-by**: The full git commit object ID that resolved the defect and provided the `Fixes:` reference.
- **subsystem**: The subsystem or component area (e.g. `scsi`, `hw/nvme`, `virtio`, `migration`, `target/arm`, `target/riscv`, `block`, `tcg`).
- **problem_description**: A concise, factual description detailing the root cause, affected function/variable, and triggering condition. This serves as the ground truth prompt during LLM evaluation.

## Selection and Curation Criteria
To ensure high benchmark fidelity and meaningful review evaluation, candidates must satisfy the following constraints:

1. **Existence & Integrity**: Both the bad commit SHA and the fix commit SHA must resolve to valid commit objects in the QEMU repository.
2. **Review Suitability**:
   - The bug must be introduced or observable within the diff of `Commit`.
   - Very large mechanical refactors (e.g. thousands of lines of renaming) are excluded in favor of focused commits typical of reviewable patchsets.
3. **Subsystem Diversity**:
   - Hardware device models (`hw/nvme`, `hw/scsi`, `hw/ide`, `hw/net`, `hw/pci`, `hw/usb`, `hw/display`).
   - Virtualization and transport (`virtio`, `vhost`, `9pfs`).
   - CPU architectures and translation (`target/arm`, `target/riscv`, `target/i386`, `target/s390x`, `target/ppc`, `tcg`).
   - Core infrastructure (`migration`, `block`, `dirty-bitmap`, `monitor`, `io`, `util`).
4. **Bug Class Diversity**:
   - Memory management: Memory leaks, unreferenced heap allocations.
   - Pointer safety: NULL pointer dereferences, missing validation on external/guest inputs.
   - Buffer bounds: Out-of-bounds reads/writes, heap and stack buffer overflows.
   - Lifetime errors: Use-after-free, double-free, premature deallocation before draining async I/O.
   - Initialization: Uninitialized variables, uninitialized guest-visible fields, uninitialized mutexes.
   - Arithmetic & Logic: Integer overflows, sign extension bugs, inverted conditions, truncation.
   - Concurrency & State: Race conditions, missing BQL acquisition, missing cleanup on error unwinding paths.

## Validation Workflow
Evaluation against the QEMU benchmark dataset is executed with Sashiko's unified benchmark tool:

```bash
cargo run --bin benchmark -- \
  --file benchmarks/qemu/benchmark.json \
  --repo <path-to-qemu-or-url>
```

The benchmark runner submits each `Commit` to Sashiko's review worker, waits for review completion, and compares findings against `problem_description` using an LLM evaluator, producing structured accuracy metrics (DETECTED, PARTIALLY_DETECTED, MISSED).
