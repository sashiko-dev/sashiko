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

1.  **Series Map Generation:**
    - Analyzes the cover letter and all patch diffs.
    - Produces a strict JSON object (`SeriesMap`) mapping all symbols (structs, functions, macros) introduced across the series.
    - Identifies which patches define a symbol and which patches complete or use it.
2.  **User Summary Generation:**
    - Synthesizes the intent and design choices of the entire series.
    - Persisted to the `patchsets.summary` database column.
    - Displayed at the top of the Patchset view in the web UI.

### Stage 2: Contextual Parallel Review (Workers)
The `SeriesMap` is injected into the `input_payload` of every review worker. The worker prompts are updated with the following logic:

- **Foresight:** Workers are "aware of the future."
- **Suppression:** If a worker reviewing Patch N identifies an incomplete interface or unused code, it must consult the `SeriesMap`. If the map indicates the symbol is completed or used in a later Patch M (where M > N), the worker suppresses the finding.

## Technical Implementation

### Database Changes
- Added `summary` column (TEXT) to the `patchsets` table.
- Added `set_patchset_summary` method to `Database`.

### New Module: `src/summarizer.rs`
Contains the logic for gathering holistic inputs, prompting the AI for maps and summaries, and enforcing the JSON schema.

### Orchestration: `src/reviewer.rs`
The `review_patchset_task` was modified to execute the summarizer sequentially before launching parallel processes via `run_review_tool`.

### Worker Logic: `src/worker/prompts.rs`
The `Worker::run` method extracts the `series_map` and appends it to the `dynamic_context` provided to the LLM agent, along with strict suppression instructions.

## Trade-offs and Considerations
- **Token Usage:** The pre-computation stage adds token overhead, but reduces total tokens by preventing the AI from generating and discussing false-positive findings.
- **Latency:** Summary generation adds a small sequential delay before parallel reviews begin, but provides immediate value to the user while they wait for findings.
- **Strictness:** The `SeriesMap` uses a strict JSON schema to prevent hallucinations from breaking the parallel workers' logic.
