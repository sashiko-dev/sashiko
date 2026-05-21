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

## Re-evaluating existing results

If you have already run ingestion and reviews but want to re-score with
updated evaluation logic:

```bash
cargo run --bin benchmark -- --file benchmarks/benchmark_small.json --analyze-only
```

This skips patch submission and review, reading results directly from the
database.
