# Design: Linux Performance Review Stage & Neutral-Set Evaluation

## 1. Motivation & Problem Statement

Sashiko's Linux patch review pipeline (`src/workflows/linux_patch_review.rs`) runs three mandatory stages (`goal`, `implementation`, `execution-flow`) and four planner-selected specialist stages (`resources`, `locking`, `security`, `hardware`). Audits of historical misses in `projects/linux/benchmarks/benchmark_missed.json` and `benchmark.json` show a recurring gap around **performance, scalability, and stall regressions**:

- **Struct layout & cacheline false sharing:** Shrinking `struct vmpressure` shifted `vmstats_percpu` onto the same cacheline as hot event counters inside embedding `struct mem_cgroup`; breaking `____cacheline_aligned_in_smp`, `struct_group`, or `CACHELINE_ASSERT`.
- **Hot-path synchronization & contention regressions:** Adding a `spin_lock` on every MIDI output event where RCU is required, or taking `rtnl_lock()` unnecessarily during `vport` destruction.
- **Unbatched flushes, IPIs, and grace periods:** Flushing unmodified memory ranges on unmap, broadcasting per-PMD IPIs during `hugetlb` unshare, or calling `synchronize_rcu_expedited()` during `virtio-scsi` disk probe (causing a 15-second boot stall across many LUNs).
- **Silent fast-path, readahead, and priority regressions:** Clamping `filemap_fault` readahead to VMA boundaries, or overriding architecture-accelerated SIMD crypto implementations via template `cra_priority`.

### Key Insight: Performance Regressions Are Unexpected Side Effects

Almost none of the regressions above occurred in patches that aimed to modify performance. They were unintended second-order side effects of ordinary bug fixes, cleanups, struct field removals, or refactorings. Two architectural rules follow directly from this:

1. **Broad planner gating:** If the `planning` stage only schedules `performance` on diffs that *look* performance-related, the planner will skip the exact bug-fix and cleanup commits where stealth regressions occur. `performance` must use the same broad inclusion rule as `resources` and `locking` (run on any non-trivial C, Rust, or struct/header change; skip only unmistakably non-runtime changes).
2. **Second-order tool tracing (`1-2` hops outward):** The defect is rarely self-contained in the diff hunk. The stage must use `git_grep` and `git_read_files` to inspect outer structs that embed a resized struct, callers that execute on hot paths *or* inside $O(N)$ fan-out loops on "cold" paths (boot probe across LUNs, netdev/vport teardown, `exit`/`munmap`), and fast-path/offload selection conditions.

### Controlling Noise on "Missed Improvements"

Open-ended micro-optimization advice is the primary source of noise in automated performance reviews. To keep mailing-list signal high:
- **Regressions** introduced by the patch are reported whenever backed by a concrete mechanism and execution scale/hotness proof.
- **Missed improvements** are restricted strictly to:
  1. **Defeated or incomplete optimization claims:** The commit explicitly aims to optimize, batch, or reduce contention, but a condition, priority, fallback, or unbatched call defeats the stated gain on a supported path.
  2. **Same-path batching asymmetry:** The patch converts one operation in a hot path to batching/RCU/per-CPU/folios while leaving an adjacent sibling operation in the same path unbatched so the bottleneck remains.
- Speculative micro-optimizations (`likely`/`unlikely`, manual inlining, single one-shot cold allocations without fan-out, small fixed-bound loops) and intentional tradeoffs documented in the commit message (`false-positive-guide.md` §9) are strictly excluded.

## 2. Pipeline Architecture

### 2.1 Stage Definition (`src/workflows/linux_patch_review.rs`)

Add `performance` to `ANALYSIS_STAGES`:

| Field | Value | Rationale |
|---|---|---|
| `name` | `"performance"` | Stable stage key for `--stages`, `planning`, `--cheap`, and `detections_by_stage`. |
| `short` | `"Performance"` | CLI and UI progress label. |
| `guides` | `&["performance.md"]` | Stage-exclusive via `is_stage_exclusive_guide()` so non-`performance` stages incur zero prompt growth. |
| `uses_commit_log` | `true` | Needed to evaluate documented tradeoffs and verify optimization claims in the commit message. |
| `optional` | `true` | Selected by `planning` on all non-trivial executable/struct changes; skipped on docs/bindings/ID-tables/selftests. |
| `wants_series_context` | `false` | Matches the analysis-stage table invariant; `verification` and `post-verification` carry `wants_series_context: true` to filter series interactions. |

### 2.2 Dedicated Guide (`projects/linux/prompts/performance.md`)

`performance.md` defines six structured audit vectors and mandatory proof requirements:
1. **Caller hotness & $O(N)$ fan-out on seemingly cold paths** (`synchronize_rcu*`, `flush_workqueue`, global locks, IPIs, or sleeps invoked per-LUN, per-CPU, per-VMA/PMD, or per-netdev/vport).
2. **Struct layout & cacheline false sharing** (checking embedding structs via `git_grep`, `____cacheline_aligned_in_smp`, `CACHELINE_ASSERT`, `struct_group`, and dirty vs. read-mostly field adjacency).
3. **Silent fast-path, offload, and priority demotion** (crypto `cra_priority`, `NETIF_F_*` offloads, readahead windows, folio batching, `static_branch` polarity).
4. **Asymptotic & range-granularity regressions** ($O(N^2)$ loops over unbounded kernel collections; full-range cache/TLB flushes or walks when only a sub-range or empty range changed).
5. **Memory allocation stalls & order regressions** (`order > PAGE_ALLOC_COSTLY_ORDER` `kmalloc` vs. `kvmalloc`, `GFP_KERNEL` direct-reclaim stalls on latency-sensitive paths).
6. **Stated optimization verification & same-path asymmetry** (auditing commits that claim a performance win).

Every raised concern must cite in `reasoning`: (a) the concrete caller or struct-embedding chain verified in the tree, (b) why the path executes frequently or scales with $N$, and (c) the exact behavioral delta between `Baseline SHA` and `Target Commit SHA`.

### 2.3 Verification & Severity Calibration

- `projects/linux/prompts/severity.md`:
  - `High`: Clear hot-path throughput/latency regression, $O(N^2)$ blowup on unbounded kernel state, cacheline false sharing in hot core structs, multi-second boot/probe/unmap stall, or silently defeating hardware offloads / arch-optimized implementations.
  - `Medium`: Contained regression on a warm/secondary path, or an optimization commit whose stated optimization is defeated/incomplete on a supported path.
  - `Low`: Minor same-path batching asymmetry on an optimization commit.
  - Pure micro-optimization nitpicks without proven scale or hotness are dismissed in `verification` / `post-verification`, never emitted at `Low`.
- `projects/linux/prompts/false-positive-guide.md` (§9): Reinforce dismissing unstated micro-optimizations on bounded/one-shot paths while forbidding invented "simplicity tradeoffs" when a patch unintentionally degrades scaling, cacheline layout, or offloads.

## 3. Measuring Effectiveness & False-Positive Rate

### 3.1 `expect_clean` Neutral Entries in `src/benchmark.rs`

To measure false-positive noise directly inside `sashiko benchmark`, extend `BenchmarkExpectedBehavior` with `ExpectClean` (`"expected_behavior": "expect_clean"`):
- Requires no `problem_description` and invokes zero LLM judge calls.
- Scores `SUPPRESSED` when `patch_findings_count == 0` (`!preexisting`), and `FALSE_POSITIVE` when one or more non-preexisting patch findings are reported.
- Populates `false_positives_by_stage: BTreeMap<String, usize>` from the `stages` provenance of every false-positive finding so noise introduced by `performance` is isolated from baseline stages.

### 3.2 Balanced Suite (`projects/linux/benchmarks/benchmark_performance.json`)

Construct a balanced benchmark combining positive regressions and soak-verified neutral commits, tagged by `category`:
- **Positive Set (`expected_behavior = "detect_bug"`):**
  - Known performance misses and hits from `benchmark_missed.json` and `benchmark.json`.
  - Mined 2025–2026 mainline regressions (`Fixes:` commits fixing verified stalls, contention, quadratic walks, cacheline shifts, or offload/readahead regressions), weighted toward stealth side effects on non-performance commits.
- **Neutral Set (`expected_behavior = "expect_clean"`):**
  - Mainline commits with $\ge 6$ months of upstream soak time and **zero** subsequent `Fixes:` or `Revert` commits in git history, stratified across three false-positive traps:
    1. `neutral_lookalike_cold_or_bounded`: Adds locks, `synchronize_rcu`, struct edits, or small bounded loops with no hot caller or $O(N)$ fan-out.
    2. `neutral_hot_path_change`: Cleanly merged `mm/`, `net/`, `kernel/sched/`, `fs/`, and `io_uring/` hot-path changes (including documented simplicity/correctness tradeoffs).
    3. `neutral_routine_driver_subsys`: Everyday driver and subsystem patches.

### 3.3 Empirical Evaluation & Production Integration

Controlled A/B evaluations on `benchmark_performance.json` (40 unique commits: 20 upstream regressions + 20 neutral `expect_clean` commits) with `gemini-4-argon` (`--effort high`, `--concurrency 10`) demonstrated:
- **Positive detection recall:** Improved from **3 / 20 (`15.0%`)** on baseline `main` to **14 / 20 (`70.0%`)** (`+55.0 pp`, `4.67x`), with **9 of the 11 net-new detections** originating solely from `performance` (`18 / 20` raised as stage concerns).
- **Neutral false-positive noise:** **12 / 20 (`60.0%`)** clean vs. **11 / 20 (`55.0%`)** on baseline `main` (**0 net-new false-positive neutral commits**; the single sole-origin `performance` finding on `192cb1f5cb16` matched a real `Cc: stable` upstream kernel bug fixed in `df72e55e754c`).
- **Resource & latency overhead:** `+11.0%` median input tokens (`91.4%` cached), `+8.4%` median output tokens, and `-3.1%` median per-commit duration (parallel execution inside the analysis fan-out).

Based on these results, `performance` is integrated as a standard optional specialist stage in `linux_patch_review::ANALYSIS_STAGES` across both local (`sashiko review`, including `--stages performance` and `--cheap`) and daemon (`sashiko` server / worker) review modes, matching `security`, `resources`, and `locking`.
