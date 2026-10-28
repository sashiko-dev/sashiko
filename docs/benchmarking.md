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
| `projects/linux/benchmarks/benchmark_versioned.json` | Multi-version LKML patchset stories (`v1..vN`) testing prior-feedback verification, reviewer attribution, and false-positive suppression. |

Each file contains entries with a `Commit` hash (or target patch `Message-ID` for
versioned mbox entries), an optional `Fixed-by` reference, an optional
`Subsystem`, and a `problem_description` that the AI judge uses to evaluate
whether Sashiko detected (or properly suppressed) the issue.

Versioned benchmark entries may additionally specify `mbox_archive` (a
`.jsonl.gz` fixture bundle), `ancestor_mboxes` (`v1..v(N-1)` mbox threads with
optional `patchset_status` and seeded `prior_findings`), `target_mbox` (`vN`),
`baseline_commit`, `expected_behavior` (`detect_bug`, `detect_with_attribution`,
`detect_regression`, or `suppress_false_positive`), `expected_reviewer`,
`expected_prior_version`, `expected_prior_message_ids`,
`expected_co_reviewers`, and `acceptable_attributions`.

### Attribution ground truth

Attribution labels describe who first raised an issue that is still present
in the target version. They are checked when the suite is loaded, and the
load fails if they are inconsistent:

- `detect_with_attribution` requires `expected_reviewer` (`"Name <email>"`
  exactly as in the `From:` header, or `"Sashiko"` for a seeded prior
  finding), `expected_prior_version` (the **earliest** ancestor version in
  which that reviewer raised the issue), and `expected_prior_message_ids`.
  The Message-IDs (without angle brackets) list every message in that version
  in which the reviewer raised the issue. For a seeded Sashiko finding, the
  ID is the `patch_message_id` of the ancestor patch that the seeded review is
  attached to. `expected_co_reviewers` lists other reviewers who raised the
  same issue independently in the same version. Their messages are added to
  `expected_prior_message_ids`.
- `detect_bug` means that no earlier reviewer raised the issue. It must not
  set `expected_reviewer`, `expected_prior_version`, or
  `expected_prior_message_ids`.
- `acceptable_attributions` (only on `detect_bug` and
  `detect_with_attribution`) lists optional credit: `{reviewer, version,
  message_ids}` items for earlier messages that arguably raised the issue,
  where crediting them is acceptable but not required. Each item's reviewer,
  version, and Message-IDs follow the same rules as the required credit.
- For `detect_regression` and `suppress_false_positive`, `expected_reviewer`
  and `expected_prior_version` are informational only. They name the reviewer
  whose advice or claim the entry is about, and the scorer never uses them for
  credit.

### Scoring rules

The judge only decides whether a reported finding matches the description
(`DETECTED`, `PARTIALLY_DETECTED`, or `MISSED`). It does not decide
attribution. For suppression entries, the judge is told that the description
is a refuted claim which must not be reported. A match on a suppression entry
is scored as `FALSE_POSITIVE`.

Attribution is scored deterministically from the structured
`prior_attribution` object of the matched finding. No free text, name tokens,
or judge opinion is used:

- **Correct**: `raised_by_identities` contains the expected reviewer's email
  (or `sashiko`, or a co-reviewer's email), `originally_raised_in_version`
  equals `expected_prior_version`, and at least one `evidence[].message_id` is
  in `expected_prior_message_ids`.
- **Acceptable**: the attribution matches the entry's
  `acceptable_attributions`. Every identity in `raised_by_identities` must be
  covered by an item whose `version` equals `originally_raised_in_version`.
  If the attribution has evidence, at least one `evidence[].message_id` must
  be among those items' `message_ids`. An acceptable attribution is neither
  correct nor wrong. It is reported separately, and it is left out of the
  attribution rate and precision denominators. On a `detect_bug` entry, an
  exact detection with acceptable credit still passes.
- **Wrong**: an attribution is present but is neither correct nor acceptable.
  This includes older outputs that have no identities or evidence. It also
  includes any other attribution on the matched finding of a `detect_bug` or
  `detect_regression` entry, because no earlier reviewer raised those issues.
  The summary reports these separately as "unexpected attributions".
- **Missing**: an attribution was expected, but the matched finding has none,
  or no finding matched.

The summary reports correct, wrong, missing, and acceptable attributions
separately. The attribution rate is correct attributions divided by the
attribution-expected entries that did not score acceptable. Attribution
precision is correct attributions divided by all correct, wrong, and
unexpected attributions emitted on matched findings.

Versioned entries (those with `target_mbox` or `ancestor_mboxes`) use strict
denominators. Classic commit suites (`benchmark.json`, `benchmark_small.json`,
`benchmark_tiny.json`, `benchmark_missed.json`, `benchmark_preexisting.json`)
keep their original metric definitions. Those definitions score only judged
outcomes, and count `PARTIALLY_DETECTED` toward the total detection rate. For
versioned entries:

- Entries without a completed review (`NOT_REVIEWED`, `NOT_FOUND_IN_DB`) and
  failed judge evaluations (`UNKNOWN`) count as failures in the detection and
  suppression denominators. Only entries without a description (`SKIPPED`) are
  excluded.
- `PARTIALLY_DETECTED` is reported separately. It counts toward the total
  detection rate but never toward the exact rate or a category pass.
- A category pass is strict. A detection entry passes only on exact
  `DETECTED` with the expected attribution outcome: correct when credit is
  expected, and none when it is not. A suppression entry passes only on
  `SUPPRESSED`.
- A review that reports no findings at all passes every suppression entry
  vacuously. Such passes are counted as "suppressed with zero findings", so
  they are visible next to the suppression rate.

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
