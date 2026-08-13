---
name: gpu-driver-code-scan
description: Run local Linux GPU driver source scans through the bundled gpu-driver-code-scan CLI. Use when an existing local source directory needs a complete inventory, risk-ordered grouping, synthetic-diff review with Sashiko, artifact validation, or a Markdown findings report.
---

# GPU Driver Code Scan

Use this Skill as a thin entry point to the bundled Python CLI. Keep inventory,
grouping, risk sorting, synthetic diff construction, scheduling, Sashiko
invocation, finding normalization, validation, and report rendering inside the
program.

## Input

- Require one existing local GPU driver source directory.
- Require or create one fresh, empty output directory.
- Treat the source tree as read-only.

## Run

Resolve `SKILL_ROOT` as the directory containing this `SKILL.md`, then run:

```bash
PYTHONPATH="$SKILL_ROOT/src" \
python3 -m gpu_driver_code_scan.cli scan "$SOURCE_DIR" \
  --output-dir "$OUTPUT_DIR"
```

Use the CLI defaults unless the user explicitly requests an override. In
particular, do not restate provider, model, concurrency, finding threshold,
group limits, timeouts, or stages in the command merely to duplicate defaults.

Pass supported optional arguments directly to the CLI. Use `--plan-only` only
when the user requests inventory and grouping without model review. Use
`--no-ai` only when the user requests a Sashiko integration smoke test without
an AI call.

Do not call `bin/review` directly. Do not construct synthetic commits, prompts,
or findings in the Agent layer.

## Validate and Return

After a model-backed scan completes, run:

```bash
PYTHONPATH="$SKILL_ROOT/src" \
python3 -m gpu_driver_code_scan.cli validate "$OUTPUT_DIR"
```

Return the scan status and paths to:

```text
<output-dir>/scan-result.json
<output-dir>/findings.json
<output-dir>/report.md
```

Do not add, remove, merge, or rewrite findings outside the scanner. The
pipeline around Sashiko is programmatic; result variability enters only through
Sashiko's configured model review.
