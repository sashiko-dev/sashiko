# Design: Declarative AI Workflows in Rust

## 1. Overview & Motivation

Sashiko uses Large Language Models (LLMs) to perform complex multi-stage software engineering tasks, primarily Linux kernel patch review. 

Currently, review stages and orchestration logic are implemented imperatively across multiple modules ([`src/worker/prompts.rs`](../src/worker/prompts.rs), [`src/worker/stage.rs`](../src/worker/stage.rs), and [`src/pipelines/mod.rs`](../src/pipelines/mod.rs)). This results in several architectural pain points:

1. **Scattered Stage Logic**: Prompt text, file inclusion rules, validation logic, error feedback, and output reduction are decoupled across disparate files.
2. **Duplicated Dual-Prompt Tracking**: To conserve token space in review logs and context caches, the codebase manually constructs and threads parallel `(content, clean)` prompt tuples through all layers.
3. **Ad-Hoc Control Flow**: Dynamic stage planning, parallel fan-out, early exit conditions, and conflict resolution loops are written as imperative code blocks within large functions.
4. **High Barrier for New Workflows**: Defining a new workflow (e.g., cherry-pick review, security audit, commit message linting) requires hundreds of lines of boilerplate and error-prone glue code.

### 1.1 Design Goals
- **Declarative & Self-Documenting**: Define stages, prompts, expected schemas, retry conditions, and transitions using an expressive, type-safe fluent Rust API.
- **Unified Prompt & Logging Engine**: Write a single prompt template containing `@include("path/to/file.md")` directives and `{{variable}}` placeholders. When sending to the LLM, directives are expanded inline; when logging to history/database, directives remain concise tokens (e.g. `@subsystem/locking.md`), eliminating manual dual-prompt maintenance.
- **Dynamic File Inclusions from State**: Support dynamically injecting guidance files selected at runtime (e.g. Phase 0 pre-screening).
- **Strongly-Typed Output Validation**: Automate JSON Schema generation for LLMs, type deserialization, custom validation predicates, and automatic feedback generation on format rejections.
- **Fine-Grained Tool Scoping**: Explicitly declare tool availability per stage (`None`, `All`, `Selected(...)`) to conserve prompt tokens and prevent hallucinated tool calls in synthesis stages.
- **Resilient Error & Fallback Policies**: Declaratively specify retry limits, transient error handling, parallel execution policies (`FailFast` vs `BestEffort`), and provider error policies (e.g., recitation fallback to free-form mode).
- **Expressive Workflow Graph (DAG)**: Support sequential steps, static/dynamic fan-out (parallel stage execution), conditional branching, early exits, and typed state reducers.
- **Built-in Lifecycle Telemetry**: Automatically emit standardized progress and turn events without manual callback bookkeeping in stage implementations.
- **Unit Testability**: Enable unit testing of individual stages, prompt rendering, validation logic, and DAG routing in isolation without external infrastructure.

---

## 2. Architecture & Core Primitives

```mermaid
graph TD
    subgraph Declarative Workflow DAG
        WF[Workflow Definition]
        S1[Stage: Phase 0 Pre-Screen]
        S2[Stage: Dynamic Planner]
        SP[Parallel Fan-Out: Stages 1-7]
        EE1{Early Exit?}
        S8[Stage: Deduplication]
        S9[Stage: Conflict Resolution]
        S10[Stage: Verification]
        S11[Stage: LKML Report]
    end

    subgraph Prompt Engine
        PT[PromptTemplate]
        INC[Inclusion Engine: @include]
        VAR[Variable Substitutor: {{var}}]
        DYN[Dynamic State Inclusions]
        EXP[Model Renderer: Inline File Expansion]
        LOG[Log Renderer: Preserves @directives]
    end

    subgraph Output & Validation
        OF[OutputFormat: Json&lt;T&gt; / Text]
        SCH[Auto JSON Schema Generator]
        VAL[Validator & Correction Formatter]
    end

    subgraph Execution Runtime
        WE[WorkflowEngine]
        SR[SessionRunner / LlmSession]
        ST[WorkflowState & Reducers]
        EV[WorkflowEvent Telemetry]
    end

    WF --> S1 --> S2 --> SP --> EE1
    EE1 -- No Concerns --> EXIT[Terminal Result]
    EE1 -- Has Concerns --> S8 --> S9 --> S10 --> S11
    S1 & S2 & SP & S8 & S9 & S10 & S11 --> PT
    PT --> INC & VAR & DYN
    INC & DYN --> EXP & LOG
    S1 & S2 & SP & S8 & S9 & S10 & S11 --> OF --> SCH & VAL
    WE --> ST & EV
    WE --> SR
```

---

## 3. Detailed Component Specifications

### 3.1 Unified Prompt Template Engine (`PromptTemplate`)

A prompt template is defined as a single string containing:
- **Variable Placeholders**: `{{variable_name}}` extracted dynamically from the workflow state.
- **Static File Inclusion Directives**: `@include("path/to/file.md")` or `@include_dir("dir/", filter_fn)`.
- **Dynamic File Inclusions**: Inclusions determined at runtime from state fields (e.g. `state.selected_guides` chosen by Phase 0).

#### The Single-Template Logging Simplification
Instead of creating and maintaining separate `content` and `clean` strings:
- **`render_for_model`**: Substitutes `{{vars}}` and expands all static and dynamic `@include("...")` directives by reading the underlying file contents into the prompt.
- **`render_for_log`**: Substitutes `{{vars}}` but leaves `@include("path/to/file.md")` as a compact reference.

```rust
pub struct PromptTemplate<S> {
    raw_template: String,
    vars: HashMap<String, Box<dyn Fn(&S) -> String + Send + Sync>>,
    static_inclusions: Vec<InclusionDirective>,
    dynamic_inclusions: Vec<Box<dyn Fn(&S) -> Vec<PathBuf> + Send + Sync>>,
}

pub enum InclusionDirective {
    File(PathBuf),
    Directory {
        path: PathBuf,
        filter: Box<dyn Fn(&str) -> bool + Send + Sync>,
    },
}

impl<S> PromptTemplate<S> {
    pub fn new(template: impl Into<String>) -> Self {
        Self {
            raw_template: template.into(),
            vars: HashMap::new(),
            static_inclusions: Vec::new(),
            dynamic_inclusions: Vec::new(),
        }
    }

    /// Bind a state variable placeholder `{{key}}`.
    pub fn with_var<F>(mut self, key: &str, extractor: F) -> Self
    where
        F: Fn(&S) -> String + Send + Sync + 'static,
    {
        self.vars.insert(key.to_string(), Box::new(extractor));
        self
    }

    /// Add a static file inclusion directive.
    pub fn include_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.static_inclusions.push(InclusionDirective::File(path.into()));
        self
    }

    /// Add a directory inclusion directive with filtering.
    pub fn include_dir<F>(mut self, dir: impl Into<PathBuf>, filter: F) -> Self
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        self.static_inclusions.push(InclusionDirective::Directory {
            path: dir.into(),
            filter: Box::new(filter),
        });
        self
    }

    /// Add dynamic file inclusions resolved from workflow state (e.g. Phase 0 guides).
    pub fn include_files_from_state<F>(mut self, resolver: F) -> Self
    where
        F: Fn(&S) -> Vec<PathBuf> + Send + Sync + 'static,
    {
        self.dynamic_inclusions.push(Box::new(resolver));
        self
    }

    /// Renders the expanded prompt for sending to the LLM.
    pub async fn render_for_model(&self, state: &S, registry: &PromptRegistry) -> Result<String> {
        let mut buffer = self.substitute_vars(state);
        // Static inclusions
        for inclusion in &self.static_inclusions {
            match inclusion {
                InclusionDirective::File(path) => {
                    let content = registry.read_file(path).await?;
                    buffer.push_str(&format!("\n\n# {}\n{}\n", path.display(), content));
                }
                InclusionDirective::Directory { path, filter } => {
                    let files = registry.read_directory(path, filter).await?;
                    for (file_name, file_content) in files {
                        buffer.push_str(&format!("\n\n## {}\n{}\n", file_name, file_content));
                    }
                }
            }
        }
        // Dynamic inclusions from state
        for dyn_inc in &self.dynamic_inclusions {
            for path in dyn_inc(state) {
                if let Ok(content) = registry.read_file(&path).await {
                    buffer.push_str(&format!("\n\n# {}\n{}\n", path.display(), content));
                }
            }
        }
        Ok(buffer)
    }

    /// Renders the compact prompt for storage in logs/database.
    pub fn render_for_log(&self, state: &S) -> String {
        let mut buffer = self.substitute_vars(state);
        for inclusion in &self.static_inclusions {
            match inclusion {
                InclusionDirective::File(path) => {
                    buffer.push_str(&format!("\n\n@{}\n", path.display()));
                }
                InclusionDirective::Directory { path, .. } => {
                    buffer.push_str(&format!("\n\n@{}/\n", path.display()));
                }
            }
        }
        for dyn_inc in &self.dynamic_inclusions {
            let files = dyn_inc(state);
            if !files.is_empty() {
                let tags: Vec<String> = files.iter().map(|p| format!("@{}", p.display())).collect();
                buffer.push_str(&format!("\n\n{}\n", tags.join(", ")));
            }
        }
        buffer
    }

    fn substitute_vars(&self, state: &S) -> String {
        let mut text = self.raw_template.clone();
        for (key, extractor) in &self.vars {
            let pattern = format!("{{{{{}}}}}", key);
            text = text.replace(&pattern, &extractor(state));
        }
        text
    }
}
```

---

### 3.2 Output Specification & Validation (`OutputFormat<S, T>`)

A stage specifies its output type `T` and validation rules:

```rust
pub enum OutputFormat<S, T> {
    /// Strongly-typed JSON output with automatic schema derivation.
    Json {
        schema: Option<serde_json::Value>,
        validator: Option<Box<dyn Fn(&T, &S) -> Result<(), String> + Send + Sync>>,
        feedback_formatter: Option<Box<dyn Fn(&str) -> String + Send + Sync>>,
    },
    /// Plaintext output validated by a custom function (e.g. LKML inline comments).
    Text {
        validator: Box<dyn Fn(&str, &S) -> Result<(), String> + Send + Sync>,
        feedback_formatter: Box<dyn Fn(&str) -> String + Send + Sync>,
    },
}

impl<S, T> OutputFormat<S, T>
where
    T: serde::de::DeserializeOwned + schemars::JsonSchema + Send + 'static,
{
    /// Creates a JSON output specification deriving its JSON Schema from `T`.
    pub fn json() -> Self {
        let schema = schemars::schema_for!(T);
        let schema_val = serde_json::to_value(&schema).ok();
        Self::Json {
            schema: schema_val,
            validator: None,
            feedback_formatter: None,
        }
    }

    /// Attaches a custom semantic validation predicate to the parsed JSON.
    pub fn with_validator<F>(mut self, validator_fn: F) -> Self
    where
        F: Fn(&T, &S) -> Result<(), String> + Send + Sync + 'static,
    {
        if let Self::Json { ref mut validator, .. } = self {
            *validator = Some(Box::new(validator_fn));
        }
        self
    }
}
```

---

### 3.3 Stage Policy & Resilient Error Handling (`StagePolicy`)

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolScope {
    /// No tools provided to the LLM (for pure reasoning/synthesis stages).
    None,
    /// All tools in the active ToolBox are enabled.
    All,
    /// Only specific tool names are provided.
    Selected(Vec<String>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParallelPolicy {
    /// If any parallel stage fails, abort the entire batch immediately.
    FailFast,
    /// Continue running remaining stages, logging warnings for failed ones.
    BestEffort,
}

#[derive(Clone)]
pub struct StagePolicy {
    pub max_turns: usize,
    pub max_validation_attempts: usize,
    pub temperature: f32,
    pub tools: ToolScope,
    pub recitation_policy: RecitationPolicy,
}

impl Default for StagePolicy {
    fn default() -> Self {
        Self {
            max_turns: 15,
            max_validation_attempts: 3,
            temperature: 0.0,
            tools: ToolScope::All,
            recitation_policy: RecitationPolicy::Fail,
        }
    }
}

#[derive(Clone)]
pub enum RecitationPolicy {
    /// Abort on recitation error.
    Fail,
    /// Retry once after appending a reminder to avoid verbatim quoting.
    RetryWithReminder(&'static str),
    /// Switch the stage into a free-form summary mode and retry.
    FallbackToFreeForm { reminder: &'static str },
}
```

---

### 3.4 Type-Erased Executable Stage (`ExecutableStage<S>`)

To allow a workflow to contain stages returning different output types `T` within a single homogeneous graph, we introduce the `ExecutableStage<S>` trait:

```rust
#[async_trait::async_trait]
pub trait ExecutableStage<S>: Send + Sync {
    fn name(&self) -> &'static str;
    async fn execute(
        &self,
        env: &WorkflowEnv<'_>,
        state: &mut S,
        event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
    ) -> Result<StageMetrics>;
}

pub struct Stage<S, T> {
    pub name: &'static str,
    pub system_prompt: Option<PromptTemplate<S>>,
    pub user_prompt: PromptTemplate<S>,
    pub output_format: OutputFormat<S, T>,
    pub policy: StagePolicy,
    pub reducer: Box<dyn Fn(&mut S, T) + Send + Sync>,
}

impl<S: 'static, T: 'static> Stage<S, T> {
    pub fn builder(name: &'static str) -> StageBuilder<S, T> {
        StageBuilder::new(name)
    }
}

#[async_trait::async_trait]
impl<S: Send + Sync + 'static, T: serde::de::DeserializeOwned + Send + 'static> ExecutableStage<S> for Stage<S, T> {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn execute(
        &self,
        env: &WorkflowEnv<'_>,
        state: &mut S,
        event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
    ) -> Result<StageMetrics> {
        // 1. Render system and user prompts using self.system_prompt & self.user_prompt
        // 2. Build LlmSession bridging output_format, policy, and tools
        // 3. Execute via SessionRunner
        // 4. Invoke (self.reducer)(state, parsed_output)
        // 5. Return token usage & execution metrics
        todo!()
    }
}
```

---

### 3.5 Workflow Graph & Control Flow (`Workflow<S>`)

```rust
pub enum WorkflowStep<S> {
    /// Run a single stage to completion and fold its output into `S`.
    Stage(Box<dyn ExecutableStage<S>>),

    /// Static Fan-Out: Run multiple stages concurrently, join results, and fold.
    Parallel {
        stages: Vec<Box<dyn ExecutableStage<S>>>,
        policy: ParallelPolicy,
    },

    /// Dynamic Fan-Out: Run a planning stage, then resolve and run stages concurrently.
    DynamicParallel {
        planner: Box<dyn ExecutableStage<S>>,
        resolver: Box<dyn Fn(&S) -> Vec<Box<dyn ExecutableStage<S>>> + Send + Sync>,
        policy: ParallelPolicy,
    },

    /// Conditional Branching.
    Branch {
        condition: Box<dyn Fn(&S) -> bool + Send + Sync>,
        then_flow: Workflow<S>,
        else_flow: Option<Workflow<S>>,
    },

    /// Early Exit condition: stops workflow immediately and produces a final result.
    EarlyExitIf {
        condition: Box<dyn Fn(&S) -> bool + Send + Sync>,
        finalizer: Box<dyn Fn(&S) -> WorkerResult + Send + Sync>,
    },
}

pub struct Workflow<S> {
    pub name: &'static str,
    pub steps: Vec<WorkflowStep<S>>,
}
```

---

### 3.6 Automated Telemetry & Lifecycle Events (`WorkflowEvent`)

The workflow engine automatically emits structured events across all steps:

```rust
#[derive(Debug, Clone)]
pub enum WorkflowEvent {
    WorkflowStarted { name: &'static str },
    StageStarted { stage_name: &'static str },
    StageTurn { stage_name: &'static str, turn: usize, max_turns: usize },
    StageFinished { stage_name: &'static str, tokens_in: u32, tokens_out: u32 },
    EarlyExitTriggered { reason: &'static str },
    WorkflowFinished { name: &'static str, total_tokens: u32 },
}
```

---

## 4. End-to-End Walkthrough: Sashiko Kernel Review Workflow

Here is how the complete 11-stage Sashiko review protocol is expressed with this declarative architecture:

```rust
pub fn build_kernel_review_workflow() -> Workflow<ReviewState> {
    Workflow::builder("kernel_patch_review")
        // Phase 0: Pre-screen relevant subsystem guides based on patch diff
        .stage(
            Stage::builder("phase_0_prescreen")
                .system_prompt(PromptTemplate::new(
                    "You are an AI assistant preparing a Linux kernel patch review.\n\
                     Review the provided patch and select all potentially relevant subsystem guides."
                ))
                .user_prompt(
                    PromptTemplate::new(
                        "<subsystem_guide_index>\n{{subsystem_index}}\n</subsystem_guide_index>\n\n\
                         <patch>\n{{target_diff}}\n</patch>"
                    )
                    .with_var("subsystem_index", |s| s.subsystem_index.clone())
                    .with_var("target_diff", |s| s.target_diff.clone())
                )
                .policy(StagePolicy { tools: ToolScope::None, ..Default::default() })
                .output_format(OutputFormat::json::<PrescreenOutput>())
                .reduce(|state, out| {
                    state.selected_guides = out.selected_prompts;
                })
        )

        // Dynamic Planning Phase & Concurrent Analysis (Stages 1-7)
        .dynamic_parallel(
            // Dynamic Planner
            Stage::builder("planning_phase")
                .system_prompt(system_prompt_with_log())
                .user_prompt(
                    PromptTemplate::new(
                        "Analyze the provided patch and determine which stages are relevant (4-7):\n\
                         {{target_diff}}"
                    )
                    .with_var("target_diff", |s| s.target_diff.clone())
                )
                .policy(StagePolicy { tools: ToolScope::None, ..Default::default() })
                .output_format(OutputFormat::json::<PlanningOutput>())
                .reduce(|state, out| {
                    state.planned_stages = out.relevant_stages;
                }),
            // Resolver: Stages 1-3 always run; Stages 4-7 run if selected
            |state| {
                let mut stages = vec![
                    stage_1_main_goal(),
                    stage_2_high_level_impl(),
                    stage_3_execution_flow(),
                ];
                for &stage_num in &state.planned_stages {
                    match stage_num {
                        4 => stages.push(stage_4_resource_management()),
                        5 => stages.push(stage_5_locking()),
                        6 => stages.push(stage_6_security()),
                        7 => stages.push(stage_7_hardware()),
                        _ => {}
                    }
                }
                stages
            },
            ParallelPolicy::FailFast,
        )

        // Early Exit 1: If no concerns were raised in analysis stages 1-7
        .early_exit_if(
            |state| state.concerns.is_empty(),
            |state| ReviewResult::no_issues(state, "No concerns detected in analysis stages.")
        )

        // Stage 8: Deduplication & Consolidation
        .stage(
            Stage::builder("stage_8_deduplication")
                .system_prompt(system_prompt_with_log())
                .user_prompt(
                    PromptTemplate::new(
                        "# Stage 8. Deduplication and Consolidation\n\n\
                         Aggregated Concerns:\n{{concerns_json}}\n\n\
                         Aggregated Dismissed Concerns:\n{{dismissed_json}}"
                    )
                    .with_var("concerns_json", |s| s.serialize_concerns())
                    .with_var("dismissed_json", |s| s.serialize_dismissed_concerns())
                )
                .policy(StagePolicy { tools: ToolScope::None, ..Default::default() })
                .output_format(OutputFormat::json::<DeduplicationOutput>())
                .reduce(Reducer::replace_concerns())
        )

        // Early Exit 2: If all concerns were deduplicated or dismissed
        .early_exit_if(
            |state| state.concerns.is_empty(),
            |state| ReviewResult::no_issues(state, "All candidate concerns dismissed during deduplication.")
        )

        // Stage 9: Conflict Resolution
        .stage(
            Stage::builder("stage_9_conflict_resolution")
                .system_prompt(system_prompt_with_log())
                .user_prompt(
                    PromptTemplate::new(
                        "# Stage 9. Concern / Dismissed Concern Conflict Resolution\n\n\
                         Concerns:\n{{concerns_json}}\n\n\
                         Dismissed Concerns:\n{{dismissed_json}}"
                    )
                    .with_var("concerns_json", |s| s.serialize_concerns())
                    .with_var("dismissed_json", |s| s.serialize_dismissed_concerns())
                )
                .policy(StagePolicy { tools: ToolScope::None, ..Default::default() })
                .output_format(OutputFormat::json::<ConflictResolutionOutput>())
                .reduce(Reducer::replace_concerns())
        )

        // Stage 10: Verification and Severity Estimation
        .stage(
            Stage::builder("stage_10_verification")
                .system_prompt(system_prompt_with_log())
                .user_prompt(
                    PromptTemplate::new(
                        "# Stage 10. Verification and Severity Estimation\n\n\
                         {{series_context}}\
                         Validate each concern and assign severity:\n{{concerns_json}}"
                    )
                    .include_file("false-positive-guide.md")
                    .include_file("severity.md")
                    .with_var("series_context", |s| s.series_context_string())
                    .with_var("concerns_json", |s| s.serialize_concerns())
                )
                .output_format(OutputFormat::json::<VerificationOutput>())
                .reduce(Reducer::set_findings())
        )

        // Early Exit 3: If no valid findings survived verification
        .early_exit_if(
            |state| state.findings.is_empty(),
            |state| ReviewResult::no_issues(state, "No valid findings remained after verification.")
        )

        // Stage 11: LKML-Friendly Inline Report Generation
        .stage(
            Stage::builder("stage_11_report_generation")
                .system_prompt(system_prompt_with_log())
                .user_prompt(
                    PromptTemplate::new(
                        "# Stage 11. LKML Report Generation\n\n\
                         Convert findings into standard inline review format:\n{{findings_json}}"
                    )
                    .include_file("inline-template.md")
                    .with_var("findings_json", |s| s.serialize_findings())
                )
                .policy(StagePolicy {
                    tools: ToolScope::None,
                    recitation_policy: RecitationPolicy::FallbackToFreeForm {
                        reminder: "CRITICAL: Recitation filter triggered. Do not quote patch code; write a free-form summary.",
                    },
                    ..Default::default()
                })
                .output_format(OutputFormat::text_with_validator(
                    validate_lkml_inline_format,
                    format_lkml_feedback,
                ))
                .reduce(Reducer::set_inline_review())
        )
        .build()
}
```

---

## 5. End-to-End Walkthrough: Adding a Custom Cherry-Pick Workflow

Creating an alternate workflow for reviewing backported cherry-picks requires only ~40 lines:

```rust
pub fn build_cherry_pick_workflow() -> Workflow<CherryPickState> {
    Workflow::builder("cherry_pick_review")
        .stage(
            Stage::builder("intent_and_divergence_audit")
                .include_file("patterns/backport-rules.md")
                .user_prompt(
                    PromptTemplate::new(
                        "Evaluate upstream commit vs backport patch for target branch {{branch}}:\n\
                         Upstream:\n{{upstream_diff}}\n\n\
                         Backport:\n{{backport_diff}}"
                    )
                    .with_var("branch", |s| s.target_branch.clone())
                    .with_var("upstream_diff", |s| s.upstream_diff.clone())
                    .with_var("backport_diff", |s| s.backport_diff.clone())
                )
                .output_format(OutputFormat::json::<AnalysisConcernsOutput>())
                .reduce(Reducer::merge_concerns())
        )
        .stage(
            Stage::builder("cherry_pick_decision")
                .user_prompt(
                    PromptTemplate::new(
                        "Produce final verdict (ACCEPT / REJECT / MODIFY) with justification:\n\
                         Concerns:\n{{concerns_json}}"
                    )
                    .with_var("concerns_json", |s| s.serialize_concerns())
                )
                .policy(StagePolicy { tools: ToolScope::None, ..Default::default() })
                .output_format(OutputFormat::json::<CherryPickVerdictOutput>())
                .reduce(|state, out| {
                    state.verdict = out.verdict;
                    state.findings = out.findings;
                })
        )
        .build()
}
```

---

## 6. Testing & Verification Plan

1. **Prompt Template Unit Tests**:
   - Verify `render_for_model` correctly expands static and dynamic `@include("file.md")` directives into full file content and interpolates `{{vars}}`.
   - Verify `render_for_log` leaves `@include("file.md")` intact as an unexpanded directive token while interpolating `{{vars}}`.
2. **Schema & Validation Unit Tests**:
   - Verify that `OutputFormat::json::<T>()` correctly derives JSON Schema and rejects invalid structures with actionable feedback messages.
3. **Workflow Control Flow Tests**:
   - Test early exit conditions using mock states (e.g. confirming stages 8–11 are skipped when no concerns exist).
   - Test parallel stage fan-out and deterministic output aggregation under `FailFast` and `BestEffort` policies.
4. **Integration & Regression Testing**:
   - Run `make check-pr` and `make integration-test`.
   - Execute benchmark evaluations (`cargo run --bin benchmark -- --file benchmarks/benchmark_small.json`) to guarantee 100% behavioral parity with existing review results.

---

## 7. Implementation Roadmap

- **Step 1**: Implement core data structures in `src/workflow/` (`PromptTemplate`, `OutputFormat`, `StagePolicy`, `Stage`, `WorkflowBuilder`).
- **Step 2**: Implement `WorkflowEngine` runtime bridging declarative stages to `SessionRunner` and `AiProvider`.
- **Step 3**: Re-express the standard Sashiko 11-stage review protocol as a declarative workflow.
- **Step 4**: Verify all unit, integration, and benchmark tests pass cleanly.
