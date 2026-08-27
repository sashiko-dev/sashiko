# Declarative Workflows & the AI Review Pipeline

Sashiko's AI review pipeline is built on a **declarative workflow engine**
(`src/workflow/`). The Linux kernel code review is expressed as a graph of
typed, configurable *stages* instead of hand-written orchestration code. This
document explains how the pipeline works from a user's point of view, what it
does, how to control it, and how each configuration knob changes the review.

> This is the user-facing guide. The design that motivated this architecture
> lives in [`designs/DESIGN_DECLARATIVE_AI_WORKFLOWS.md`](../designs/DESIGN_DECLARATIVE_AI_WORKFLOWS.md),
> and the concrete kernel review workflow is defined in
> `src/worker/kernel_workflow.rs`.

## Quick start

```bash
# Review the last commit with the default pipeline
sashiko review

# Review a range of commits
sashiko review HEAD~3..HEAD

# Focus the review on a specific concern (appended to every stage's system prompt)
sashiko review HEAD --custom-prompt "Pay extra attention to RCU usage."

# Restrict the analysis to specific stages 1-7 (hidden debug flag)
sashiko review HEAD --stages 1,5

# Point at a custom prompt bundle (guides and templates) instead of the bundled one
sashiko review HEAD --prompts /path/to/prompts/kernel
```

From a running daemon or via the CLI:

```bash
sashiko-cli local HEAD --force-local --custom-prompt "Focus on locking correctness"
```

## What the pipeline does

A review is not a single LLM prompt: it is a **multi-stage pipeline** that
plans its own work, fans out specialised analyses in parallel, consolidates
the results, verifies each concern against the code, and finally renders an
LKML-style inline review.

Conceptually the kernel review workflow is:

```
 Phase 0        Planning        Stages 1-7 (parallel)      Consolidation
 ───────        ────────        ─────────────────────      ─────────────
 pre-screen ──▶ (dynamic  ──▶ 1. main goal                Stage 8  dedup
 (pick guides)  planner)     2. high-level implementation  Stage 9  conflict
                             3. execution flow           │   resolution
                             4. resource mgmt             ▼
                             5. locking        ────▶  Stage 10 verification
                             6. security                  │
                             7. hardware                  ▼
                                                  Stage 11 LKML inline report
```

Between each phase the workflow checks for an **early exit**: if no concerns
survive an analysis phase, the remaining (expensive) stages are skipped and
the review stops. A patch with no issues never reaches the report generator.

### The stages

| # | Stage | Runs when | Tools | Output |
|---|-------|-----------|-------|--------|
| 0 | `stage_0_prescreen` | always (skipped when `--stages` is set) | none | Subsystem guides relevant to the diff |
| 1 | `stage_planning` | always (skipped when `--stages` is set) | none | Which of stages 4-7 apply (1-3 always run) |
| 2-8 | `stage_1` … `stage_7` | per the plan (or `--stages`) | all | `concerns` + `dismissed_concerns` JSON |
| 9 | `stage_8_deduplication` | only if stages 1-7 raised concerns | all | Deduplicated concerns |
| 10 | `stage_9_conflict_resolution` | only if dedup found concerns | all | Concerns that survive conflict resolution |
| 11 | `stage_10_verification` | only if concerns remain | all | Validated findings with severity |
| 12 | `stage_11_inline_report` | only if findings remain | all | LKML plain-text review |

Notes:

- Stages 1-3 always run. Stages 4-7 are selected per-patch by the dynamic
  planner (with a strong bias toward running more stages).
- Stages 3-6 review the **diff hunks alone**; stages 1, 2, 7 and the
  consolidation stages also receive the **commit message** (via `git show`).
- Stage 11 output is validated against the `inline-template.md` quoting rules
  and rejected (with feedback to the model) if it does not look like a proper
  LKML review.
- The pre-screen and planning stages never get tools; the analysis and
  consolidation stages do, so the model can inspect the source tree.

## How it works (user perspective)

**The engine, not your review, decides the shape of the run.** Before any
analysis, two cheap stages run with no tools:

1. **Pre-screen (Phase 0)** reads the diff and selects which subsystem guides
   (e.g. `subsystem/locking.md`, `subsystem/network.md`) are relevant. Those
   guides are then injected into every subsequent stage's shared system prompt,
   so the model reviews against the correct subsystem rules.
2. **Dynamic planner** decides which of stages 4-7 are relevant for this
   patch. It always reports the stages that *will* run; Sashiko surfaces this
   as the "plan" in progress output.

The selected stages 1-7 then run **concurrently** against the same patch, each
with a specialised prompt (main-goal, implementation, execution-flow, resource
management, locking, security, hardware). They run under a **FailFast** policy:
if one stage fails hard, the whole batch stops.

As the analysis stages run, the model may call tools to explore the source
tree. The engine guards tool use:

- Tool calls issued together in one turn are run **concurrently**.
- A **duplicate consecutive tool call** is blocked and the model is told why,
  preventing the infinite-loop failures that used to abort reviews.
- A **rejected/failed tool call** is reported back to the model so it can
  correct its parameters, instead of terminating the stage.

After consolidation, verification, and report generation, the workflow emits a
`WorkerResult` containing the JSON findings, the dismissed-concerns list, and
the final LKML review text.

## Configuring the review pipeline

The pipeline is configured through `Settings.toml`, a few CLI flags, and the
prompt files on disk.

### `[ai]` settings

```toml
[ai]
provider = "gemini"
model = "gemini-3.1-pro-preview"
max_interactions = 100     # max LLM turns allowed per stage
temperature = 1.0          # sampling temperature for every stage
```

| Key | Default | Effect on the pipeline |
|-----|---------|------------------------|
| `max_interactions` | `100` | Becomes the per-stage `max_turns` limit. Lower it to bound cost/latency per review; raise it for hard patches that need many tool rounds. |
| `temperature` | `1.0` | Sampling temperature applied to every stage. Lower values make output more deterministic. |
| `provider` / `model` | -- | Which LLM drives all stages. See [LLM Provider Configuration Guide](llm-providers.md). |

### CLI flags

| Flag | Effect on the pipeline |
|------|------------------------|
| `--custom-prompt <TEXT>` | Appends `<TEXT>` as a `<custom_instructions>` block at the end of the shared system prompt used by every stage except the pre-screen. |
| `--stages <1,2,5>` | (Hidden debug flag.) Runs **only** the listed analysis stages. Skips pre-screen and dynamic planning entirely, and bypasses the per-patch plan. |
| `--prompts <DIR>` | Sets the base directory from which `@include` guide files and templates are resolved. Defaults to the bundled kernel prompt set. |
| `--baseline <REF>` | Baseline used for patch application; affects what the diff shows. |
| `--no-ai` | Skips AI review entirely (patch extraction/application validation only). |

For example:

```bash
# Only the locking audit, on a specific range
sashiko review HEAD~2..HEAD --stages 5

# A run with focused guidance
sashiko review HEAD --custom-prompt "Do not suggest API changes outside the patch's subsystem."
```

Turn limits and temperature are set in `Settings.toml` (see the `[ai]` table
above) — there are no `--temperature` / `--max-interactions` flags.

### Prompt files (the `@include` bundle)

Every stage's prompt is a **template** that can pull guidance files in at
render time. The files live under the prompt bundle's `kernel/` directory
(`--prompts` selects which one). Changing these files changes what the model
sees for every future review:

| File | Used by |
|------|---------|
| `subsystem/subsystem.md` | Pre-screen index of subsystem guides |
| `subsystem/<name>.md` | Selected dynamically per-patch (locking, network, vfs, …) |
| `callstack.md`, `technical-patterns.md` | Stage 3 (execution flow) |
| `subsystem/locking.md` | Stage 5 (locking) |
| `false-positive-guide.md`, `severity.md` | Stage 10 (verification) |
| `inline-template.md` | Stage 11 (report format, validated against it) |

An inclusion is written as a directive inside a template:

```
<global_review_guidelines>
@include("callstack.md")
@include("technical-patterns.md")
</global_review_guidelines>
```

The engine resolves each `@include("file.md")` relative to the prompt base
directory. When sending the prompt to the LLM it **expands** the file contents
in place; when storing the prompt in logs/history it keeps the compact
`@file.md` token, saving tokens and context space. This single-template
approach replaces the old parallel "full vs clean" prompt pair.

Variables use `{{name}}` placeholders that are substituted from review state
at render time (patch diff, commit SHA, baseline, selected guides, aggregated
concerns, and so on).

> **Tip:** a missing `@include` target is silently consumed — the directive
> never leaks into the prompt as literal text. Keep guide files short and
> specific; they are prepended to every relevant stage.

## Under the hood: the workflow DSL

The engine is a general Rust API in `src/workflow/`. If you want to build or
modify a review workflow, you compose four primitives:

### Prompt templates

```rust
use sashiko::workflow::PromptTemplate;

let template = PromptTemplate::new(
    "Evaluate the backport of {{branch}}.\n\n<backport>\n{{diff}}\n</backport>",
)
.with_var("branch", |s| s.target_branch.clone())
.with_var("diff", |s| s.backport_diff.clone())
.include_file("patterns/backport-rules.md");
```

- `with_var("name", |s| …)` binds a `{{name}}` placeholder to workflow state.
- `include_file("path.md")` adds a static inclusion directive.
- `include_dir("dir/", |name| name.ends_with(".md"))` includes matching files.
- `include_files_from_state(|s| …)` resolves inclusions from state at runtime
  (this is how the pre-screen-selected subsystem guides are injected).

### Output formats

```rust
use sashiko::workflow::OutputFormat;

// Typed JSON, validated before the reducer runs
let json_out = OutputFormat::json::<MyOutput>();              // or json_with_schema(...)

// Plain text with a custom validator and feedback message
let text_out = OutputFormat::text_with_validator(
    |raw, _state| {
        if raw.is_empty() { Err("empty output".into()) } else { Ok(()) }
    },
    |violation| format!("Rejected: {violation}. Please re-emit."),
);
```

An invalid output (JSON that does not parse, or text failing the validator) is
fed back to the model with the formatted message and retried up to
`max_validation_attempts` times instead of failing the review outright.

### Stage policies

```rust
use sashiko::workflow::{StagePolicy, ToolScope, RecitationPolicy};

let policy = StagePolicy {
    max_turns: 15,                        // conversation turns allowed
    max_validation_attempts: 3,           // format-retry budget
    temperature: 0.0,
    tools: ToolScope::None,               // None | All | Selected([...])
    recitation_policy: RecitationPolicy::FallbackToFreeForm {
        reminder: "Do not quote code verbatim.".into(),
    },
};
```

`ToolScope` controls which tools the model sees per stage. `RecitationPolicy`
decides what happens when the provider blocks a response for quoting code
verbatim (abort, retry with a reminder, or fall back to free-form output).

### A complete custom workflow

```rust
use serde::Deserialize;
use sashiko::workflow::{
    OutputFormat, ParallelPolicy, PromptTemplate, Stage, StagePolicy, ToolScope,
    Workflow, WorkflowEngine, WorkflowEnv,
};

#[derive(Default)]
struct CherryPickState {
    target_branch: String,
    upstream_diff: String,
    backport_diff: String,
    concerns: Vec<String>,
    verdict: Option<String>,
}

#[derive(Deserialize)]
struct ConcernsOutput { concerns: Vec<String> }

#[derive(Deserialize)]
struct VerdictOutput { verdict: String }

fn build_cherry_pick_workflow() -> Workflow<CherryPickState> {
    Workflow::builder("cherry_pick_review")
        .stage(
            Stage::builder("intent_and_divergence_audit")
                .user_prompt(
                    PromptTemplate::new(
                        "Compare the upstream commit with the backport for {{branch}}:\n\
                         Upstream:\n{{upstream}}\n\nBackport:\n{{backport}}",
                    )
                    .with_var("branch", |s| s.target_branch.clone())
                    .with_var("upstream", |s| s.upstream_diff.clone())
                    .with_var("backport", |s| s.backport_diff.clone()),
                )
                .output_format(OutputFormat::json())
                .policy(StagePolicy { tools: ToolScope::None, ..Default::default() })
                .reduce(|s: &mut CherryPickState, out: ConcernsOutput| {
                    s.concerns = out.concerns;
                })
                .build(),
        )
        .early_exit_if(|s| s.concerns.is_empty(), "no divergence found")
        .stage(
            Stage::builder("cherry_pick_decision")
                .user_prompt(
                    PromptTemplate::new(
                        "Produce ACCEPT / REJECT given:\n{{concerns}}",
                    )
                    .with_var("concerns", |s| s.concerns.join("\n")),
                )
                .output_format(OutputFormat::json())
                .reduce(|s: &mut CherryPickState, out: VerdictOutput| {
                    s.verdict = Some(out.verdict);
                })
                .build(),
        )
        .build()
}

async fn run(mut state: CherryPickState, env: WorkflowEnv<'_>) -> anyhow::Result<()> {
    let outcome =
        WorkflowEngine::execute(&build_cherry_pick_workflow(), &env, &mut state, None).await?;
    println!(
        "early_exit={} reason={:?} tokens_in={} tokens_out={}",
        outcome.early_exit, outcome.early_exit_reason, outcome.tokens_in, outcome.tokens_out,
    );
    Ok(())
}
```

Supported step combinators:

| Builder method | Behavior |
|----------------|----------|
| `.stage(Stage::builder("name")…build())` | Run one stage, fold its output into state. |
| `.parallel(stages, policy)` | Run a static batch concurrently (`FailFast` or `BestEffort`). |
| `.dynamic_parallel(planner, resolver, policy)` | Run a planning stage, then resolve + run a batch concurrently. This is how stages 1-7 are scheduled. |
| `.branch(condition, then, else)` | Conditionally run one of two sub-workflows. |
| `.early_exit_if(condition, reason)` | Stop the whole workflow early when the condition holds. |
| `.build()` | Finalize the workflow. |

Each `.stage` builder also supports `.system_prompt(…)`, `.temperature(f)`,
`.max_turns(n)`, `.tools(scope)`, `.on_recitation(policy)`, and
`.skip_if(predicate)`.

### Telemetry

`WorkflowEngine::execute` accepts an event callback. The engine emits lifecycle
events automatically — progress bars, "stages that will run" reporting, and
token accounting all come from these events:

```rust
use sashiko::workflow::WorkflowEvent;

let event_cb = |event: WorkflowEvent| match event {
    WorkflowEvent::WorkflowStarted { name } => { /* … */ }
    WorkflowEvent::StageStarted { stage_name } => { /* … */ }
    WorkflowEvent::StageTurn { stage_name, turn, max_turns } => { /* … */ }
    WorkflowEvent::StageFinished { stage_name, tokens_in, tokens_out, tokens_cached } => { /* … */ }
    WorkflowEvent::ParallelResolved { stage_names } => { /* the plan */ }
    WorkflowEvent::EarlyExitTriggered { reason } => { /* … */ }
    WorkflowEvent::WorkflowFinished { name, total_tokens } => { /* … */ }
};
```

## Overriding the default workflow

The default kernel review workflow is **Rust source**, not a runtime data
file. There is no `Settings.toml` key, CLI flag, or on-disk workflow file you
can point Sashiko at — the workflow is compiled into the binary. Overriding it
means editing the definition in the source tree and rebuilding:

| What | Where |
|------|-------|
| Workflow engine and DSL primitives | `src/workflow/` |
| Default kernel review workflow definition | `src/worker/kernel_workflow.rs` — `build_kernel_review_workflow_with_options()` |
| The single call site that runs the review | `src/worker/prompts.rs` — `Worker::run`, the `WorkflowEngine::execute(...)` call |

Every review path (the `sashiko review` command, `sashiko-cli local`, and the
daemon worker) goes through `Worker::run`, which builds the workflow with
`build_kernel_review_workflow_with_options(max_interactions, temperature)` and
executes it. There are two ways to override:

1. **Modify the default graph.** Edit `build_kernel_review_workflow_with_options()`
   in `src/worker/kernel_workflow.rs`: add or remove `.stage(...)` steps, change
   the `early_exit_if` conditions, tweak a stage's `StagePolicy`, or swap the
   prompt templates. Because every review path calls this one function, your
   modified graph is picked up automatically. This is the lowest-friction path —
   the state type stays `KernelReviewState`, so the result handling in
   `Worker::run` keeps working unchanged.

   ```rust
   // src/worker/kernel_workflow.rs
   pub fn build_kernel_review_workflow_with_options(
       max_turns: usize,
       temperature: f32,
   ) -> Workflow<KernelReviewState> {
       Workflow::builder("linux_kernel_code_review")
           .stage(prescreen_stage())
           // ...existing steps...
           // add your own step, e.g. an extra verification stage
           .stage(stage_11_inline_report(max_turns, temperature))
           .build()
   }
   ```

2. **Swap in your own workflow.** Write your own builder (parameterized over
   your own state type, as in the cherry-pick example above) and replace the
   workflow construction in `Worker::run` (`src/worker/prompts.rs`). Your
   workflow receives the same `WorkflowEnv` (provider, tools, prompt base dir)
   and runs through the same `WorkflowEngine::execute` path, so progress
   reporting and token accounting keep working. Note that `Worker::run` reads
   the result out of `KernelReviewState` fields (`findings`, `review_inline`,
   …) — a workflow over a different state type must also adapt that extraction
   to still produce a `WorkerResult`.

After editing, rebuild:

```bash
cargo build --release
```

## How changes affect the review pipeline

| You change… | Pipeline effect |
|-------------|-----------------|
| `[ai].max_interactions` | Per-stage `max_turns` for every stage; bounds tool-round budget. |
| `[ai].temperature` | Sampling temperature for every stage. |
| `--custom-prompt` | Extra `<custom_instructions>` block appended to the shared system prompt of all stages except the pre-screen. |
| `--stages 1,2,5` | Pre-screen and dynamic planning are skipped; only the listed stages run. |
| `--prompts <DIR>` | A different bundle of guide/template files (new subsystem guides, different report template). |
| Any `@include`ed `.md` file | Changes the guidance the model receives in every stage that includes it. |
| A stage's `StagePolicy` in `kernel_workflow.rs` | Changes tool availability, turn limits, temperature, or recitation handling for that stage. |
| The workflow graph in `kernel_workflow.rs` | Adds/removes/reorders stages, early exits, or parallel fan-outs for the whole review. |
| The workflow construction in `Worker::run` (`src/worker/prompts.rs`) | Swaps the default kernel workflow for a custom one (see "Overriding the default workflow"). |

Because stages are declarative and typed, a new review workflow (a security
audit, a cherry-pick check, a commit-message linter) is a new `Workflow`
definition rather than hundreds of lines of imperative glue.

## See also

- [Configuration Reference](configuration.md) — `Settings.toml` and env vars.
- [CLI Reference](sashiko-cli.md) — `sashiko-cli` commands, including `local`.
- [LLM Provider Configuration Guide](llm-providers.md) — provider setup.
- [`designs/DESIGN_DECLARATIVE_AI_WORKFLOWS.md`](../designs/DESIGN_DECLARATIVE_AI_WORKFLOWS.md)
  — the architecture and motivation behind the workflow engine.
