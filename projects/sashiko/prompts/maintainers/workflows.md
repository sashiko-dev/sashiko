+++
title = "Workflow Engine, Prompt Templates, and Review Pipeline Policy"
paths = [
    "src/worker/prompts.rs",
    "src/workflow/",
    "src/workflows/",
]
symbols = [
    "LlmSession",
    "OutputFormat",
    "PromptTemplate",
    "SessionRunner",
    "Stage",
    "StageSession",
    "WorkflowEngine",
    "sanitize_prompt_relpath",
]
+++

## Declarative Workflow State Isolation

- Parallel stages (`WorkflowStep::Parallel` and `WorkflowStep::DynamicParallel`)
  must remain side-effect-free during `execute_isolated`: stages may mutate
  shared workflow state only inside their returned reducer closure, applied in
  deterministic declaration order after the parallel batch completes.
- Multi-writer state fields (such as `findings`, `concerns`, and
  `deduplicated_dismissed_concerns` written by both `verification` and
  `post-verification`) must accumulate via `.extend(...)` /
  `record_verified_findings` rather than overwriting or clearing entries written
  by earlier stages.
- Any `early_exit_if` or `skip_if` predicate must match the exact state fields
  populated by the preceding stage's reducer, and progress accounting must use
  `WorkflowEvent::ParallelResolved` or state rather than assuming optional
  stages always emit `StageStarted`.

## PromptTemplate Trust Boundaries

- Maintain the strict separation between `Segment::Template` and
  `Segment::Included` in `PromptTemplate`:
  - File inclusion (`@include("...")` and `@includes`) must run *before*
    variable substitution and scan only `Segment::Template` segments so
    untrusted variable values (such as a patch diff containing `@include(...)`)
    can never place a file.
  - Variable substitution (`substitute_vars`) must scan `Segment::Template`
    segments in a single left-to-right pass without re-scanning inserted
    variable values, and must never scan `Segment::Included` segments.
- Every `@include("...")` directive in a prompt string must byte-match a
  `.include_file("...")` call on the same `PromptTemplate` (and vice versa).
- Any file path reaching `include_files_from_state` must be validated with
  `sanitize_prompt_relpath` (or plain-filename filtering) so untrusted model
  outputs cannot traverse outside the project prompt directory.
- Keep `render_for_log` lightweight: expand variables for auditability, but keep
  file inclusions as `@path` tokens rather than duplicating full prompt guides
  into every stored interaction log.

## SessionRunner, Tool Calls, and Output Validation

- Every `LlmSession` implementation must override `call_tools`, convert
  `ToolBox` errors into `{"error": ...}` JSON values instead of aborting the
  stage with `?`, preserve tool result ordering relative to calls, and enforce
  the consecutive duplicate tool-call guard in both `call_tool` and
  `call_tools`.
- Stage output structs deserialized via `OutputFormat::json` must not mark all
  top-level fields `#[serde(default)]` without a custom validator
  (`.with_validator(...)`) that rejects empty or malformed inner objects.
- Never attach `.with_validator` or `.with_feedback_formatter` to an
  `OutputFormat::Text` stage (which is a silent no-op); use
  `OutputFormat::text_with_validator` instead.
- In `SessionRunner::run`, every message appended after turn 1 must be pushed to
  both `history` and `log_history` so stored interaction transcripts remain
  complete.
