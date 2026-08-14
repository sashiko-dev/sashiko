# Codebase Scan

[简体中文](README.zh-CN.md) | English

## What It Does

`codebase-scan` is a local command-line scanner for existing codebases. It
inventories a source tree, reviews risk-ordered groups with
[Sashiko](https://github.com/sashiko-dev/sashiko), and writes versioned JSON
artifacts plus a Markdown report.

It can be installed either as a standard Python CLI or as a thin Agent Skill.
Both interfaces run the same scanner implementation and produce the same
versioned artifact contract.

## Scan Strategy

![Codebase scan strategy](docs/scan-strategy.svg)

1. **Inventory** reviewable source files and exclude generated or test output.
2. **Group** files by directory; split large files into contiguous line regions
   within file, line, and byte budgets.
3. **Sort** groups by implementation and driver-oriented risk signals such as
   memory faults, DMA/IOMMU, userspace APIs, queues, locking, and lifetime
   management.
4. For each group `G` in the complete tree `T`, construct a synthetic pair:

   ```text
   Baseline = T - G
   Target   = T
   ```

   The diff contains only `G`, while the target commit still gives Sashiko the
   full source tree and original line numbers for context.
5. Review groups concurrently, normalize findings, verify complete region
   assignment, and write `scan-result.json`, `findings.json`, and `report.md`.

`--max-findings` stops submission of new groups; it does not truncate results.
Already-started groups finish and all of their findings remain in the report.

## Command Line

Initialize and install once:

```bash
git clone https://github.com/sashiko-dev/sashiko.git
cd sashiko
cargo build --release --bin review --manifest-path Cargo.toml
cd tools/codebase-scan
python3 -m pip install -e .
```

To install the same program as an Agent Skill instead:

```bash
scripts/install-skill --target ~/.codex
```

The installed `$codebase-scan` Skill collects the local source and
output paths, then invokes the same scanner implementation through this CLI.

Run with the public defaults:

```bash
codebase-scan scan /path/to/source \
  --output-dir ./artifacts/run-001 \
  --acknowledge-code-sharing
```

Model-backed scans send the selected source snippets to the configured AI
provider. Use `--acknowledge-code-sharing` only after confirming that the
source may be shared with that provider. `--plan-only` and `--no-ai` never
send source to a model and do not require this acknowledgement.

| Argument | Description | Default |
|---|---|---|
| `source_dir` | Existing local source repository or directory | required |
| `--output-dir` | Empty directory for generated artifacts | required |
| `--project` | Project name in artifacts and report | source directory name |
| `--source-url` | Optional source locator shown in the report | empty |
| `--reference-url` | Optional reference-context locator shown in the report | empty |
| `--provider` | Sashiko AI provider | `codex-cli` |
| `--model` | Provider model | `gpt-5.5-2026-04-24` |
| `--concurrency` | Concurrent scan groups | `3` |
| `--max-findings` | Threshold for stopping new group submission | `10` |
| `--max-files-per-group` | Maximum distinct files in one group | `30` |
| `--max-lines-per-group` | Maximum target lines in one group | `1000` |
| `--max-bytes-per-group` | Maximum target bytes in one group | `100000` |
| `--max-review-seconds` | Whole-scan review budget; `0` disables | `7200` |
| `--review-timeout-seconds` | Timeout for one Sashiko group | `3600` |
| `--stages` | Sashiko review stages | `3,4,5,6,7` |
| `--include` | Limit inventory to matching globs; repeatable | none |
| `--plan-only` | Build inventory, groups, and patch map without AI | `false` |
| `--no-ai` | Exercise Sashiko patch handling without model calls | `false` |
| `--acknowledge-code-sharing` | Confirm source snippets may be sent to the configured AI provider | required for model scans |

Validate a completed output directory with:

```bash
codebase-scan validate ./artifacts/run-001
```
