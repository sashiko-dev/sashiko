# DESIGN: Holistic Patchset Review

## Status
Approved / Implemented

## Context
Sashiko originally reviewed multi-patch series in parallel, isolated processes. This architecture led to two significant issues:
1. **Lack of Holistic Context:** Individual reviews lacked an understanding of the overall goal of the series.
2. **Cross-Patch False Positives:** Reviews for early patches would flag incomplete interfaces or unused structures that were addressed in subsequent patches of the same series.

## Goals
- Provide a high-level technical summary of the entire patch series for human maintainers.
- Eliminate redundant or incorrect findings caused by the sequential nature of kernel patches being reviewed in parallel.
- Maintain the performance benefits of parallel worker execution.

## Proposed Architecture: Two-Stage Pipeline

To solve the context problem without sacrificing speed or introducing complex IPC (Inter-Process Communication), we implement a sequential pre-computation stage before the parallel execution stage.

### Stage 1: Holistic Pre-computation (Daemon)
Before spawning individual worker processes, the `Reviewer` service performs two non-agentic AI calls:

1.  **Series Map Generation** (multi-patch series only; skipped for single-patch reviews):
    - Analyzes the cover letter and all patch diffs.
    - Produces a strict JSON object (`SeriesMap`) mapping all symbols (structs, functions, macros) introduced across the series.
    - Identifies which patches define a symbol and which patches complete or use it.
    - Tracks **cross-patch fixes**: cases where one patch introduces an issue (bug, errant deletion, intermediate breakage) that a later patch in the series corrects.
2.  **User Summary Generation:**
    - Synthesizes the intent and design choices of the entire series.
    - Persisted to the `patchsets.summary` database column.
    - Displayed at the top of the Patchset view in the web UI.

### Stage 2: Contextual Parallel Review (Workers)
The `SeriesMap` and the full list of sibling patch subjects (`all_patches`) are injected into the `input_payload` of every review worker. The worker prompts are updated with the following logic:

- **Foresight:** Workers are "aware of the future."
- **Suppression via SeriesMap:** If a worker reviewing Patch N identifies an incomplete interface, unused code, or cross-patch bug, it must consult the `SeriesMap`. If the map indicates the symbol is completed in a later Patch M (where M > N), or the issue is fixed in a later patch, the worker suppresses the finding.
- **Fallback via `all_patches` subject list:** When SeriesMap generation fails or is skipped, workers still receive a lightweight `<series_context>` block listing all patch indices and subjects. This enables workers to check whether prerequisite patches flagged as "missing" actually correspond to other patches in the same series, and to consider whether subsequent patches may address concerns before flagging them.

## Technical Implementation

### Data Model Changes
- Added `series_map: Option<SeriesMap>` field to `ReviewInput` in `src/worker/prompts.rs`.
- Added `all_patches` (list of `{index, subject}`) to the worker payload sent via `input_payload` in `src/reviewer.rs`.
- `SeriesMap` uses `#[serde(default)]` on all collection fields — a missing key deserializes to an empty vector rather than a parse failure.

### Database Changes
- Added `summary` column (TEXT) to the `patchsets` table (`schema.sql` + migration in `db.rs`).
- Added `set_patchset_summary` method to `Database`.

### New Module: `src/summarizer.rs`
Contains the logic for gathering holistic inputs, prompting the AI for maps and summaries, and enforcing the JSON schema.

**Token budget:** Each patch diff is truncated to `MAX_DIFF_CHARS_PER_PATCH` (4000 chars) using `safe_truncate()` which respects UTF-8 character boundaries. The total merged context is capped at `MAX_TOTAL_CHARS` (50,000 chars); patches beyond this threshold are omitted with a truncation notice. This prevents oversized AI prompts from silently failing for large series.

**`safe_truncate(s, max_bytes)`:** Returns the longest valid UTF-8 prefix of `s` not exceeding `max_bytes` bytes. Backs off from the cut point until it lands on a character boundary, avoiding split multi-byte codepoints.

### Orchestration: `src/reviewer.rs`
The `review_patchset_task` was modified to execute the summarizer sequentially before launching parallel processes via `run_review_tool`. Single-patch series skip SeriesMap generation (no cross-patch dependencies to track).

### Worker Logic: `src/worker/prompts.rs`
- **`ReviewInput` struct** gained `series_map: Option<SeriesMap>` and the `all_patches` list is injected into the JSON payload by the orchestrator.
- **`build_series_context()`** constructs the combined series context from the `SeriesMap` (if present) and the `all_patches` subject list (always present for multi-patch series). This context is appended to the `dynamic_context` provided to the LLM agent, along with strict suppression instructions covering incomplete symbols, cross-patch fixes, and series completeness. Serialization failures are logged as warnings rather than silently swallowed.
- **`calculate_series_range()`** computes the git range (`baseline..last_patch_sha`) used by Stage 10's series validation to verify whether concerns flagged in early patches are resolved by later patches in the same series. Returns `None` when reviewing the last patch (nothing after it to check).

### Binary Rewrite: `src/bin/review.rs`
The review binary was rewritten into a self-contained async main that deserializes `ReviewInput` from stdin, applies patches via `GitWorktree`, and runs the multi-stage worker loop. Uses `anyhow::Context` for error propagation rather than `.expect()` panics.

## Post-Implementation Hardening
- **Error propagation:** `.expect()` calls replaced with `.context()?` throughout `review.rs` to produce structured error JSON instead of panics.
- **Output schema:** `--no-ai` output now includes `inline_review: null` and `history: []` for schema consistency with normal review output.
- **Logging:** `build_series_context()` logs a warning rather than silently discarding the SeriesMap when JSON serialization fails.
- **Code reuse:** `get_patchset_details` and `get_patchset_summary` share a `load_patchset_base()` helper returning a `PatchsetBaseData` struct; the two functions differ only in their reviews subquery (the summary endpoint omits heavy `input_context` and `logs` fields for web UI performance).

## Trade-offs and Considerations
- **Token Usage:** The pre-computation stage adds token overhead, but reduces total tokens by preventing the AI from generating and discussing false-positive findings. Single-patch series skip SeriesMap generation entirely to avoid wasting tokens. Diff context is truncated per-patch at 4000 chars and capped globally at 50,000 chars to prevent oversized prompts — large series may have tail patches omitted from the pre-computation context, but the `all_patches` subject fallback still provides awareness of their existence.
- **Latency:** Summary generation adds a small sequential delay before parallel reviews begin, but provides immediate value to the user while they wait for findings.
- **Strictness:** The `SeriesMap` uses a strict JSON schema with `#[serde(default)]` on all fields to prevent hallucinations from breaking the parallel workers' logic. Missing fields deserialize to empty vectors rather than causing failures.
- **Graceful Degradation:** If SeriesMap generation fails, workers still receive the `all_patches` subject list as a lightweight fallback, ensuring some cross-patch awareness is always available for multi-patch series. If JSON serialization of the SeriesMap fails (e.g., NaN values), the map is omitted from the prompt with a warning log rather than silently producing an empty context.
- **API Payload Size:** The `/api/patchset` endpoint intentionally excludes `input_context` and `logs` fields from review objects — these fields can be tens of kilobytes of raw LLM prompt data. Consumers needing full detail should use `/api/review_log`.
