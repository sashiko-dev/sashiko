# Benchmarking

Sashiko includes a built-in `sashiko benchmark` subcommand to evaluate AI
review performance against known bugs. It automatically provisions an isolated
per-run SQLite database under `benchmarks/runs/`, ingests unique benchmark
commits in-process (requiring no separate daemon), runs reviews, and uses a
structured AI judge to compare findings and dismissed concerns against
ground-truth descriptions.

## Prerequisites

- `Settings.toml` (or `~/.config/sashiko.toml`) with a configured LLM provider
  and git repository path
- A benchmark JSON file (Linux suites are provided in `projects/linux/benchmarks/`)

## Quick start

```bash
# Run a benchmark suite (creates an isolated database and run directory automatically)
sashiko benchmark projects/linux/benchmarks/benchmark_tiny.json

# Or via cargo
cargo run --bin sashiko -- benchmark projects/linux/benchmarks/benchmark_tiny.json
```

## Benchmark files

| File | Description |
|------|-------------|
| `projects/linux/benchmarks/benchmark_tiny.json` | Minimal set for quick iteration (9 entries). |
| `projects/linux/benchmarks/benchmark_small.json` | Standard set for development iteration (99 entries). |
| `projects/linux/benchmarks/benchmark.json` | Full benchmark suite (999 entries). |
| `projects/linux/benchmarks/benchmark_missed.json` | Upstream fixes for bugs missed by Sashiko after public launch. |
| `projects/linux/benchmarks/benchmark_preexisting.json` | Tests detection of pre-existing bugs. |

Each file contains entries with a `Commit` hash, an optional `Fixed-by`
reference, an optional `Subsystem`, and a `problem_description` that the AI
judge uses to evaluate whether Sashiko detected the issue.

## Command-line options

```
sashiko benchmark [OPTIONS] <FILE>
```

| Argument / Flag | Description |
|-----------------|-------------|
| `<FILE>` / `-f, --file <FILE>` | Path to the benchmark JSON file (required). |
| `-r, --repo <PATH_OR_URL>` | Override the target git repository path or URL. |
| `--db <PATH>` | Explicit SQLite database path to create or re-analyze. |
| `--output-dir <DIR>` | Base directory for benchmark runs (default: `benchmarks/runs`). |
| `--analyze-only` | Skip ingestion and reviews; re-evaluate an existing benchmark database. |
| `--concurrency <N>` | Override review and judge concurrency. |
| `--ai-provider <PROVIDER>` | Override the AI provider. |
| `--effort <MODE>` | Override reasoning effort mode (`low`, `mid`, `high`, or `dynamic`). |
| `--report-preexisting` | Include pre-existing bugs in verification and report generation (`benchmark_preexisting.json` enables this automatically). |
| `--settings <PATH>` | Settings file override. |
| `--prompts <PATH>` | Prompt directory override. |
| `--format <FORMAT>` | Output format: `text` (default) or `json`. |
| `--color <COLOR>` | Color mode: `auto` (default), `always`, or `never`. |

## Preserved run directory & output

Every full benchmark run creates a self-contained directory:

```
benchmarks/runs/<YYYYMMDD-HHMMSS>_<sashiko-sha>_<suite-name>/
├── sashiko-<YYYYMMDD-HHMMSS>-<sashiko-sha>.db
├── results.json
└── summary.md
```

- **`sashiko-<timestamp>-<sha>.db`**: Preserved SQLite database containing all
  patches, reviews, `ai_interactions`, `review_map` provenance, `findings`,
  `bugs`, and `tool_usages` from the run.
- **`results.json`** (also written to `benchmark_results.json` in the working
  directory): Structured JSON report with `metadata` (Sashiko SHA, prompt
  bundle hash, model, effort, wall clock), `summary` (detection rates, miss
  diagnosis breakdown, precision/noise metrics, stage attribution, and
  deduplicated per-commit `mean`/`p50`/`p90`/`max` resource distributions), and
  per-entry `results` (including `matched_finding`, `miss_reason`,
  `dismissed_by_stage`, `stages`, `findings`, and `dismissed_concerns`).
- **`summary.md`**: Human-readable Markdown report with tables for detection
  accuracy, miss diagnosis, precision/noise, stage attribution, resource
  distributions, and per-entry verdicts.

## Re-evaluating existing results

To re-score a previous benchmark run without re-running ingestion or reviews:

```bash
# Automatically discovers the most recent preserved database for this suite
sashiko benchmark projects/linux/benchmarks/benchmark_small.json --analyze-only

# Or specify an explicit database file
sashiko benchmark projects/linux/benchmarks/benchmark_small.json --analyze-only --db benchmarks/runs/<run>/sashiko-<ts>-<sha>.db
```
