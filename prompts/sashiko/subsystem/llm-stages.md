# LLM Stage Design and Schema Invariants

This guide covers `src/workflows/` and any stage definitions, prompt templates,
JSON schemas, validators, and reducers built on `src/workflow/`.

Sashiko's accuracy and token efficiency depend on strict structural invariants
between how stages are prompted, what data they receive, and how their outputs
are validated and consumed.

## 1. Stage Design & Data Flow Invariants

### 1.1 Single Responsibility and Overlap
Each analysis stage must focus on solving a single, well-defined analytical
problem (e.g., `concurrency`, `persistence`, `security`).
- **Rule**: Only combine multiple analytical tasks into a single stage if
  answering them requires reasoning over a highly overlapping set of context
  and steps, purely to optimize token usage and latency.
- **Violation**: Adding unrelated checklist items (e.g., checking SQLite
  migration idempotency inside the `security` stage instead of `persistence`).

### 1.2 Minimal But Sufficient Context (The Solvability Test)
Stages must receive *only* the information required to complete their task,
and never less.
- **The Solvability Test**: Verify that the task is actually solvable using only
  the data and tools (`ToolScope`) provided to the LLM. If a human engineer
  could not confidently verify a claim with just that prompt context and toolset,
  the LLM will hallucinate.
- **Example**: In `linux_patch_review.rs`, stages that judge whether code
  implements the author's stated intent (`goal`, `implementation`) set
  `uses_commit_log: true` (`{{target_commit_diff}}`). Stages that trace pure
  mechanics (`execution-flow`, `resources`, `locking`) set
  `uses_commit_log: false` (`{{target_commit_diff_only}}`) so commit message
  claims cannot bias static analysis.
- **Anti-Pattern (The Kitchen Sink)**: Passing the entire workflow state or
  unfiltered history into a stage "just in case".

### 1.3 Diverge & Converge (Map-Reduce)
Broad, complex analyses must split into parallel specialized stages (Map)
followed by consolidation stages (`deduplication` -> `conflict-resolution` ->
`verification`) (Reduce).
- **Invariant**: Parallel map stages must never mutate shared state directly.
  They return a deferred `StateMutation` closure (`stage.rs:execute_isolated`)
  that appends items tagged with the stage name (`append_stage_items`).

### 1.4 Provenance (`stage` -> `stages`)
`append_stage_items` tags each item with the stage that raised it, and that tag
has to survive consolidation so a finding can say where it came from. Each
consolidation prompt asks for it back as a `stages` array — `deduplication`
merges the arrays of the items it groups, `conflict-resolution` and
`verification` carry them through unchanged — and each of those reducers then
calls `keep_stages_that_raised`, which drops any name no analysis stage tagged.
- **Why this matters**: the model transcribes provenance, it does not decide
  it. A stage name the fan-out never produced is a transcription error, and
  reporting it would credit a stage that did not run.
- **Violation**: a consolidation stage that rewrites concerns without carrying
  `stages`, or a reducer that trusts the returned names without filtering them
  against the tags. Adding a stage that reads `stages` for anything but
  reporting is also wrong: it is a record, not an input to analysis.

### 1.5 Negative Data Tracking (`dismissed_concerns`)
When an analysis stage investigates a plausible defect and proves it is safe,
it must output that item in `dismissed_concerns` with concrete evidence in
`reasoning`.
- **Why this matters**: `conflict-resolution` compares `concerns` against
  `dismissed_concerns`. If one stage flags a candidate bug and another stage
  traced the caller and proved the precondition is impossible, the dismissed
  concern prevents a false positive.
- **Violation**: Any reducer or consolidation stage that drops
  `dismissed_concerns` before `conflict-resolution` breaks negative data
  tracking.

### 1.6 Early Exits (Short-Circuiting)
Workflows must defensively bail out as soon as further processing is unnecessary
using `WorkflowBuilder::early_exit_if`.
- **Required Checkpoints**:
  1. After parallel analysis stages: exit if `all_concerns.is_empty()`.
  2. After deduplication: exit if `deduplicated_concerns.is_empty()`.
  3. After conflict resolution: exit if `patch_concerns.is_empty()`.
  4. After verification: exit if `findings.is_empty()`.
- **Why**: Running consolidation or report generation on empty arrays wastes
  tokens and tempts the LLM to hallucinate findings to satisfy its prompt.

### 1.7 No Dead Outputs
Every field produced by a stage's JSON output schema must be stored by its
reducer and consumed by a subsequent stage or persisted in the final output.
- **Violation**: Adding a field to `StageConcernsOutput` or a custom stage
  struct that is deserialized and immediately discarded or never read downstream.

---

## 2. Prompt Engineering & Schema Invariants

### 2.1 Shared Vocabulary and Exact Field Naming
Prompts and JSON schemas must use identical field names and terminology.
- **Rule**: If the schema defines `"function_or_symbol"`, the prompt instruction
  must never refer to it as `"function_name"` or `"symbol"`.
- **Rule**: Every field name must have a single, unambiguous interpretation.

### 2.2 The Escape Hatch (Avoid Rigid Classification)
Never force the LLM to choose between a fixed set of enum options unless those
options mathematically partition all possible real-world scenarios.
- **Rule**: Classifiers and severity/category enums must include an escape path
  (e.g., `"Other"`, `"Unknown"`, or allowing `line: null` when an exact line
  number is unknown).
- **Why**: When boxed into a rigid schema without an escape hatch, an LLM will
  hallucinate an incorrect classification or invent line numbers rather than
  fail schema validation.

### 2.3 Anti-Charity Directives
Prompts that analyze code for defects or reconcile conflicts must explicitly
instruct the model *not* to give code the benefit of the doubt.
- **Local Boundary Rule**: A stage may not dismiss a defect within modified code
  by assuming surrounding callers or external layers validate inputs or mask the
  error, unless it cites concrete code proving the failure mode is structurally
  impossible.

---

## 3. Validators and LLM Feedback

### 3.1 Actionable Validation Feedback
When `OutputFormat::with_validator` rejects an LLM response, the paired
`with_feedback_formatter` is fed back into the conversation turn for retry (up
to `max_validation_attempts`).
- **Invariant**: The feedback string must state *exactly* which formatting or
  structural rule was violated and show the required shape.
- **Example**: `validate_inline_format` in `linux_patch_review.rs` checks for
  prohibited Markdown fences (` ``` `), missing `>` quote prefixes, and missing
  `commit <hash>` headers, returning the exact rule violated so the retry turn
  succeeds.

## Checklist for Diffs Touching Workflows or Stages

1. **Context Match**: Does the stage receive the commit message (`uses_commit_log`)
   only if its task evaluates intent? Does it have the right `ToolScope`?
2. **Schema-Prompt Parity**: Do every JSON key and enum variant in the prompt
    prose match the `OutputFormat::json_with_schema` or Rust struct field names
   character-for-character?
3. **Escape Hatches**: Are nullable fields (`line: null`) or catch-all variants
   provided where exact data might not exist?
4. **No Dead Fields**: Is every field in the stage output struct written into
   state by `.reduce(...)` and read downstream?
5. **Security Sanitization**: If model output selects files or stages (like
   `prescreen` or `planning`), does the reducer filter against path traversal
   (`/`, `\`, `..`) and unknown stage names?
6. **Provenance**: Does every consolidation prompt that rewrites concerns ask
   for `stages` back, and does its reducer pass the result through
   `keep_stages_that_raised` rather than trusting the names returned?
