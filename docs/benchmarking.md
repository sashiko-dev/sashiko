# Benchmarking

Sashiko includes a benchmark tool to evaluate AI review performance
against known bugs. It ingests patches, waits for reviews to complete,
then uses an AI judge to compare findings against ground-truth
descriptions.

## Prerequisites

- A running sashiko daemon with a configured LLM provider
- A clean database (move or remove any existing `sashiko.db`)
- A benchmark JSON file (several are provided in `benchmarks/`)

## Quick start

```bash
# Start with a clean database
mv sashiko.db sashiko.db.bak

# Run the benchmark
cargo run --bin benchmark -- --file benchmarks/benchmark_small.json
```

## Benchmark files

| File | Description |
|------|-------------|
| `benchmarks/benchmark_tiny.json` | Minimal set for quick smoke tests. |
| `benchmarks/benchmark_small.json` | Small set for development iteration. |
| `benchmarks/benchmark.json` | Full benchmark suite. |
| `benchmarks/benchmark_preexisting.json` | Tests detection of pre-existing bugs. |
| `benchmarks/benchmark_smoke.json` | CI smoke test set. |

Each file contains entries with a commit hash, a `Fixed-by` reference,
and a `problem_description` that the AI judge uses to evaluate whether
sashiko detected the issue.

## Command-line options

```
cargo run --bin benchmark -- [OPTIONS]
```

| Flag | Description |
|------|-------------|
| `-f, --file <PATH>` | Path to the benchmark JSON file (required). |
| `-p, --port <PORT>` | Override the daemon port (defaults to Settings.toml value). |
| `-r, --repo <URL>` | Override the kernel repository URL. |
| `--analyze-only` | Skip ingestion; only evaluate existing results in the database. |

## Output

The tool prints a summary to the console:

- **Detection rates**: Detected, Missed, Partially Detected
- **Performance metrics**: Average tokens in/out, average turns,
  average time per review
- **Counts**: Total concerns and findings

Detailed results are written to `benchmark_results.json` in the current
directory, including the AI judge's explanation for each finding.

## Local Model Benchmark Profiles

When benchmarking a bounded local OpenAI-compatible model, report these
profiles separately:

| Profile | `enable_static_bug_seeds` | `enable_targeted_bug_pattern_prescan` | Purpose |
|---------|---------------------------|---------------------------------------|---------|
| Neutral | `false` | `false` | Model-quality runs without regression aids. |
| Generic static | `true` | `false` | Measures opt-in deterministic diff-local bug-pattern detectors without the LLM prescan. |
| Regression | `true` | `true` | Tracks regression behavior with all opt-in regression aids enabled. |

Static seeds are opt-in generic bug-pattern detectors. For example, the skb
fragment seed looks for diff-local skb fragment append/growth sites that lack an
apparent `MAX_SKB_FRAGS`-style capacity guard; it is not keyed to a specific
benchmark commit. Keep seeded and neutral results separate in benchmark reports.

## Re-evaluating existing results

If you have already run ingestion and reviews but want to re-score with
updated evaluation logic:

```bash
cargo run --bin benchmark -- --file benchmarks/benchmark_small.json --analyze-only
```

This skips patch submission and review, reading results directly from the
database.
