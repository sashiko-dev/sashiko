# DESIGN: Codebase Scanner

## Context

Sashiko reviews Git patch series. Repository audits often start from an
existing source tree without a useful patch boundary, but still require every
reviewable file to be considered. The scanner adapts a source snapshot to
Sashiko's patch interface while preserving a versioned artifact contract for
automation.

The scanner lives in `tools/codebase-scan/` and is intentionally separate
from Sashiko's core Rust review engine, server, and email ingestion paths.

## Goals

1. Scan an existing local source repository.
2. Cover every reviewable source region unless a configured finding or time
   limit stops new work.
3. Split large trees into bounded, deterministic review groups.
4. Prioritize implementation and high-risk systems code.
5. Reuse Sashiko's review stages and read-only ToolBox.
6. Produce stable, machine-readable artifacts and a human-readable report.
7. Bound concurrency, process lifetime, and temporary Git state.
8. Require explicit acknowledgement before source snippets are sent to a
   configured AI provider.

## Non-goals

- Do not modify the source tree.
- Do not infer that a snapshot finding is newly introduced.
- Do not replace Sashiko's review engine or prompt selection.
- Do not upload source code or publish reports.
- Do not silently sample a subset of the inventory.

## Data Boundary

Model-backed reviews send each synthetic group diff to the configured AI
provider. The CLI therefore requires `--acknowledge-code-sharing` before any
snapshot or model work starts. This acknowledgement records that the caller
has confirmed the selected source is allowed to be shared with that provider.

`--plan-only` and `--no-ai` do not send source to a model and do not require
the acknowledgement. The acknowledgement state is recorded in the manifest.

## Inventory

The scanner walks the source directory and includes C, header, assembly, Rust,
and linker-script files. It excludes:

- version-control metadata;
- build and output directories;
- generated object or module files;
- symbolic links;
- test and self-test paths.

The selected files are recorded in `inventory.json`. A private snapshot copy
containing only those inventory files is created under the output directory,
so Git operations never touch the input tree and unrelated files cannot enter
the model context.

## Group Planning

Files are first grouped by directory family. Each file is then split into
line regions that satisfy all configured limits:

- maximum distinct files per group;
- maximum lines per group;
- maximum bytes per group.

A single line larger than the byte limit is rejected because it cannot be
represented without violating the configured bound.

### Risk Ordering

Each file receives a deterministic score. Implementation files are preferred
over headers, while generated register headers are deprioritized. The initial
default weights include systems and driver-oriented high-risk areas such as:

- unified memory, HMM, faults, and page migration;
- interconnect, peer memory, RDMA, and GPUDirect;
- mmap and virtual-memory callbacks;
- DMA, scatter-gather, IOMMU, and PCI mapping;
- ioctl and userspace copy paths;
- channels, queues, runlists, and fault handling;
- locking, lifetime, asynchronous work, and timers.

Groups are sorted by descending score and stable group identifier.

### Coverage Gate

The plan records every assigned line range and rejects:

- unassigned files;
- duplicate regions;
- extra assignments;
- gaps in a file's assigned line ranges.

This gate establishes complete planned coverage before any model review
starts.

## Synthetic Patch Construction

The snapshot is committed as target tree `T`. For each group `G`, the scanner
creates a baseline tree `B` by removing only the selected regions from `T`.
The synthetic patch is:

```text
B -> T
```

Restored lines represent the source regions under review. All unchanged files
remain available in the target snapshot for Sashiko's read-only context
tools.

Separate temporary Git indexes are used to build baseline trees. Each index
and lock file is removed immediately after its patch is generated.

Synthetic patches are generated lazily in priority order. This allows a
finding or time limit to stop scheduling without creating every remaining
patch first.

## Review Execution

The scanner invokes Sashiko's `review` binary with:

- explicit baseline and target commits;
- the kernel prompt bundle;
- selected review stages;
- a prompt explaining snapshot semantics;
- `--skip-report-stage`.

Stage 10 remains responsible for validated structured findings. Stage 11 is
skipped because the scanner owns the external report format.

Reviews run in a bounded thread pool. No new work is scheduled after a finding
or time limit is reached, but already-started reviews are allowed to finish so
their findings are not discarded. Signals terminate active review process
groups.

## Finding Normalization

The scanner removes claims that depend on synthetic patch history, such as
"newly added" or "introduced by this patch." It preserves all reportable
findings, enriches source locations from the original tree, and supplies a
concrete fallback repair direction when Stage 10 omits `suggested_fix`.

Low-severity cosmetic findings are excluded and recorded separately.

## Artifact Contract

`scan-result.json` uses schema `codebase-scan/result/v1`. The output
directory contains:

- `manifest.json`;
- `inventory.json`;
- `group-plan.json`;
- `patch-map.json`;
- `findings.json`;
- `excluded-findings.json`;
- `metrics.jsonl`;
- `report.md`;
- per-group review inputs, outputs, and logs.

A successful completion reason is one of:

- `full_inventory_reviewed`;
- `finding_limit_reached`;
- `plan_only`;
- `no_ai_smoke_test`.

Failed groups and incomplete coverage cannot produce a successful result.
`no_ai_smoke_test` validates patch handling and artifacts only; it does not
claim that model review covered the inventory.

## Tests

The Python test suite covers:

- deterministic grouping and exact region coverage;
- large-file splitting and generated-header ordering;
- lazy scheduling and in-flight result preservation;
- process-group cancellation and failure cleanup;
- finding normalization and source location correction;
- stable report and result schemas;
- package installation and no-AI end-to-end execution.

The package must also build as a wheel without untracked runtime dependencies.
