// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A general, composable review-pipeline framework.
//!
//! The default patch review (`crate::worker::Worker::run`) is deliberately left
//! untouched. This module lets us express ALTERNATE review workflows (e.g.
//! cherry-pick review) as an ordered list of [`Step`]s over the *same* stage
//! machinery, built on the shared `SessionRunner` / `LlmSession` primitives
//! described in `designs/DESIGN_MODULAR_LLM_SESSIONS.md`.
//!
//! Each stage is executed via [`crate::worker::run_review_stage`], which reuses
//! the exact tool loop, validation, and recitation handling as the patch-review
//! path, so behavior stays equivalent.

use anyhow::Result;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::ai::AiProvider;
use crate::toolbox::ToolBox;
use crate::worker::stage::create_stage;
use crate::worker::{PromptRegistry, WorkerProgressEvent, WorkerResult};

/// Where a stage's instruction prompt comes from.
pub enum StagePrompt {
    /// Use the built-in per-stage prompt from `PromptRegistry::get_stage_prompt`.
    Builtin,
    /// Use an explicit `(content, clean)` override (e.g. cherry-pick stages 1-3).
    Override {
        /// Full instruction text sent to the model.
        content: String,
        /// Compact log form (may reference `@file` guidance instead of bodies).
        clean: String,
    },
}

/// How a stage's output is folded into the accumulated [`PipelineState`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Capture {
    /// Merge `output["concerns"]` / `["dismissed_concerns"]` (analysis stages 1-7).
    MergeConcerns,
    /// Replace the concern set with `output["concerns"]` (dedup/resolution 8-9).
    ReplaceConcerns,
    /// Store `output["findings"]` as the final findings (stage 10).
    Findings,
    /// Store `output.as_str()` as the inline review text (stage 11).
    Inline,
}

/// Builds the `(user_prompt, clean_user_prompt)` for a stage from its
/// instruction text and the state accumulated so far. Used by synthesis stages
/// that inject prior concerns/findings and carry a per-stage output schema.
pub type StagePromptBuilder = Box<dyn Fn(&str, &PipelineState) -> (String, String) + Send + Sync>;

/// A single stage to execute within a pipeline.
pub struct StageSpec {
    /// Stage number; selects validation + trait behavior via `create_stage`.
    pub number: u8,
    /// Where the instruction prompt comes from.
    pub prompt: StagePrompt,
    /// How to fold the stage output into [`PipelineState`].
    pub capture: Capture,
    /// Extra user-prompt guidance appended after the instruction (e.g. the
    /// concerns JSON schema). Empty for stages that carry their own format rules.
    pub format_guidance: String,
    /// Optional dynamic builder for the user prompt from the instruction and the
    /// accumulated state. When set, it takes precedence over `format_guidance`
    /// (synthesis stages 8-12 use this to inject prior concerns/findings).
    pub template: Option<StagePromptBuilder>,
    /// For `StagePrompt::Override` stages, which stage's guide files (severity,
    /// inline template, etc.) to append -- mirroring `get_stage_prompt`, which
    /// `Override` bypasses. `None` appends nothing.
    pub guide_stage: Option<u8>,
}

impl StageSpec {
    /// Create a stage spec with no extra format guidance.
    pub fn new(number: u8, prompt: StagePrompt, capture: Capture) -> Self {
        Self {
            number,
            prompt,
            capture,
            format_guidance: String::new(),
            template: None,
            guide_stage: None,
        }
    }

    /// Append extra format guidance to the user prompt.
    pub fn with_format_guidance(mut self, guidance: impl Into<String>) -> Self {
        self.format_guidance = guidance.into();
        self
    }

    /// Set a dynamic prompt builder (takes precedence over `format_guidance`).
    pub fn with_template(mut self, builder: StagePromptBuilder) -> Self {
        self.template = Some(builder);
        self
    }

    /// Append the guide files for stage `n` (as `get_stage_prompt` would) to an
    /// `Override` prompt.
    pub fn with_guides(mut self, n: u8) -> Self {
        self.guide_stage = Some(n);
        self
    }
}

/// The system context a pipeline supplies to its stages, with and without the
/// review log (mirrors `Worker::run`'s handling of `use_log_in_context`).
#[derive(Default)]
pub struct PipelineContext {
    /// System prompt for stages that include the review log.
    pub system_with_log: String,
    /// System prompt for stages that exclude the review log.
    pub system_without_log: String,
    /// Compact log form of `system_with_log`.
    pub clean_with_log: String,
    /// Compact log form of `system_without_log`.
    pub clean_without_log: String,
}

/// State accumulated across steps; assembled into the final [`WorkerResult`].
#[derive(Default)]
pub struct PipelineState {
    /// Active concerns.
    pub concerns: Vec<Value>,
    /// Total concerns aggregated from parallel stages (before dedup).
    pub total_concerns: usize,
    /// Dismissed concerns.
    pub dismissed_concerns: Vec<Value>,
    /// Final findings (from a `Capture::Findings` stage), if any.
    pub findings: Option<Value>,
    /// Pre-filter classified findings (preserved for DB output).
    /// V1 parity: the output `findings` field always contains the full
    /// classified list; the Rust-side filter only affects which findings
    /// go to the inline review (Stage 11).
    pub classified_findings: Option<Value>,
    /// Inline review text (from a `Capture::Inline` stage), if any.
    pub review_inline: Option<String>,
    /// Suggested fixes text, if any.
    pub fixes: Option<String>,
    /// Accumulated input tokens.
    pub tokens_in: u32,
    /// Accumulated output tokens.
    pub tokens_out: u32,
    /// Accumulated cached tokens.
    pub tokens_cached: u32,
    /// Accumulated conversation history across stages.
    pub history: Vec<crate::ai::AiMessage>,
}

/// Ingredients required to execute a pipeline. Taken directly (mirroring the
/// private `Worker` fields) so the executor never reaches into `Worker`.
pub struct PipelineEnv<'a> {
    /// The AI provider.
    pub provider: Arc<dyn AiProvider>,
    /// The tool box (worktree-scoped).
    pub tools: Arc<ToolBox>,
    /// The prompt registry (for built-in stage prompts + static context).
    pub prompts: &'a PromptRegistry,
    /// Sampling temperature.
    pub temperature: f32,
    /// Maximum turns per stage.
    pub max_interactions: usize,
    /// Optional context tag for logging.
    pub context_tag: Option<String>,
    /// Optional explicit stage list (V1 parity: WorkerConfig.stages).
    pub stages: Option<Vec<u8>>,
    /// Optional series range for Stage 10 context (V1 parity).
    pub series_range: Option<String>,
}

/// A composable review workflow.
///
/// A pipeline is expressed imperatively: `run` orchestrates its stages through
/// the [`Cx`] handle -- it builds the system context, runs stages, and branches
/// on the accumulated [`PipelineState`] using ordinary control flow.
#[async_trait::async_trait]
pub trait Pipeline: Send + Sync {
    /// Human-readable name (used in logs and the result context).
    fn name(&self) -> &'static str;

    /// Execute the workflow, driving stages through `cx`.
    async fn run(&self, cx: &mut Cx<'_>) -> Result<()>;
}

/// Imperative driver handle passed to [`Pipeline::run`]. Bundles the execution
/// environment, the pipeline-built system context, and the accumulated
/// [`PipelineState`], and exposes the stage-running ops the executor used to
/// apply for a static plan.
pub struct Cx<'a> {
    /// Execution environment (provider, tools, prompts, config).
    pub env: &'a PipelineEnv<'a>,
    /// The patchset under review.
    pub patchset: Value,
    /// State accumulated across stages.
    pub state: PipelineState,
    ctx: PipelineContext,
    progress: Option<&'a (dyn Fn(WorkerProgressEvent) + Send + Sync)>,
}

impl Cx<'_> {
    /// Set the shared system context. Call once from `run`, after building it.
    pub fn set_context(&mut self, ctx: PipelineContext) {
        self.ctx = ctx;
    }

    /// Run a single stage and fold its output into the state.
    pub async fn stage(&mut self, spec: StageSpec) -> Result<()> {
        let outcome = run_stage(&spec, self.env, &self.ctx, &self.state, self.progress).await?;
        fold(&mut self.state, &spec, outcome);
        Ok(())
    }

    /// Run several stages concurrently, folding each output into the state.
    pub async fn parallel(&mut self, specs: Vec<StageSpec>) -> Result<()> {
        let futures = specs
            .iter()
            .map(|spec| run_stage(spec, self.env, &self.ctx, &self.state, self.progress));
        let outcomes = futures::future::try_join_all(futures).await?;
        for (spec, outcome) in specs.iter().zip(outcomes) {
            fold(&mut self.state, spec, outcome);
        }
        Ok(())
    }
}

/// The raw result of running one stage.
struct StageOutcome {
    output: Value,
    tokens_in: u32,
    tokens_out: u32,
    tokens_cached: u32,
    history: Vec<crate::ai::AiMessage>,
}

/// Execute a pipeline end to end, returning a [`WorkerResult`] in the same shape
/// as `Worker::run` so the review binary can consume it identically.
pub async fn execute_pipeline(
    pipeline: &dyn Pipeline,
    env: &PipelineEnv<'_>,
    patchset: Value,
    progress: Option<&(dyn Fn(WorkerProgressEvent) + Send + Sync)>,
) -> Result<WorkerResult> {
    let mut cx = Cx {
        env,
        patchset,
        state: PipelineState::default(),
        ctx: PipelineContext::default(),
        progress,
    };
    pipeline.run(&mut cx).await?;
    let Cx { state, ctx, .. } = cx;

    let final_output = json!({
        "findings": state
            .classified_findings
            .clone()
            .or_else(|| state.findings.clone())
            .unwrap_or_else(|| Value::Array(state.concerns.clone())),
        "dismissed_concerns": Value::Array(state.dismissed_concerns.clone()),
        "review_inline": state.review_inline.clone().unwrap_or_else(|| {
            // V1 compat: when the pipeline exits early (no findings),
            // the inline review text defaults to 'No issues found.'
            if state.findings.as_ref().and_then(|v| v.as_array()).is_none_or(|a| a.is_empty()) {
                "No issues found.".to_string()
            } else {
                String::new()
            }
        }),
        "fixes": state.fixes.clone().unwrap_or_default(),
        "concerns_count": state.total_concerns,
        "dismissed_concerns_count": state.dismissed_concerns.len(),
    });

    // Prepend the system prompt as a leading system message in the logged
    // history so it is persisted to reviews[n].logs. The system prompt is sent
    // to the model but was previously omitted from the review log (V1 parity).
    let mut logged_history = state.history.clone();
    if !logged_history.is_empty() {
        logged_history.insert(
            0,
            crate::ai::AiMessage {
                role: crate::ai::AiRole::System,
                content: Some(ctx.clean_with_log.clone()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            },
        );
    }

    Ok(WorkerResult {
        output: Some(final_output),
        error: None,
        input_context: format!("Pipeline '{}' completed", pipeline.name()),
        history: logged_history.clone(),
        history_before_pruning: logged_history.clone(),
        history_after_pruning: logged_history,
        tokens_in: state.tokens_in,
        tokens_out: state.tokens_out,
        tokens_cached: state.tokens_cached,
    })
}

async fn run_stage(
    spec: &StageSpec,
    env: &PipelineEnv<'_>,
    ctx: &PipelineContext,
    state: &PipelineState,
    progress: Option<&(dyn Fn(WorkerProgressEvent) + Send + Sync)>,
) -> Result<StageOutcome> {
    let stage = create_stage(spec.number);

    if let Some(progress_cb) = progress {
        progress_cb(WorkerProgressEvent::StageStarted { stage: spec.number });
    }

    let system = if stage.use_log_in_context() {
        ctx.system_with_log.clone()
    } else {
        ctx.system_without_log.clone()
    };

    let (content, clean) = match &spec.prompt {
        StagePrompt::Builtin => env.prompts.get_stage_prompt(spec.number).await?,
        StagePrompt::Override { content, clean } => {
            let mut content = content.clone();
            let mut clean = clean.clone();
            if let Some(guide_stage) = spec.guide_stage {
                env.prompts
                    .append_stage_guides(guide_stage, &mut content, &mut clean)
                    .await?;
            }
            (content, clean)
        }
    };
    let (user_prompt, clean_user_prompt) = if let Some(build) = &spec.template {
        build(&content, state)
    } else if spec.format_guidance.is_empty() {
        (content, clean)
    } else {
        (
            format!("{}\n\n{}", content, spec.format_guidance),
            format!("{}\n\n{}", clean, spec.format_guidance),
        )
    };

    let result = crate::worker::run_review_stage(
        env.provider.as_ref(),
        env.tools.clone(),
        env.temperature,
        env.max_interactions,
        env.context_tag.as_deref(),
        stage,
        system,
        user_prompt,
        clean_user_prompt,
        progress,
    )
    .await?;

    if let Some(progress_cb) = progress {
        progress_cb(WorkerProgressEvent::StageFinished { stage: spec.number });
    }

    Ok(StageOutcome {
        output: result.output,
        tokens_in: result.usage.prompt_tokens as u32,
        tokens_out: result.usage.completion_tokens as u32,
        tokens_cached: result.usage.cached_tokens.unwrap_or(0) as u32,
        history: result.history,
    })
}

fn fold(state: &mut PipelineState, spec: &StageSpec, outcome: StageOutcome) {
    state.tokens_in += outcome.tokens_in;
    state.tokens_out += outcome.tokens_out;
    state.tokens_cached += outcome.tokens_cached;
    state.history.extend(outcome.history);

    match spec.capture {
        Capture::MergeConcerns => {
            if let Some(arr) = outcome.output.get("concerns").and_then(|v| v.as_array()) {
                for item in arr {
                    if let Some(tagged) = tag_source(item.clone(), spec.number) {
                        state.total_concerns += 1;
                        state.concerns.push(tagged);
                    }
                }
            }
            if let Some(arr) = outcome
                .output
                .get("dismissed_concerns")
                .and_then(|v| v.as_array())
            {
                for item in arr {
                    if let Some(tagged) = tag_source(item.clone(), spec.number) {
                        state.dismissed_concerns.push(tagged);
                    }
                }
            }
        }
        Capture::ReplaceConcerns => {
            if let Some(arr) = outcome.output.get("concerns").and_then(|v| v.as_array()) {
                state.concerns = arr.clone();
            }
            if let Some(arr) = outcome
                .output
                .get("dismissed_concerns")
                .and_then(|v| v.as_array())
            {
                state.dismissed_concerns = arr.clone();
            }
        }
        Capture::Findings => {
            state.findings = Some(
                outcome
                    .output
                    .get("findings")
                    .cloned()
                    .unwrap_or(outcome.output),
            );
        }
        Capture::Inline => {
            if let Some(text) = outcome.output.as_str() {
                state.review_inline = Some(text.to_string());
            }
        }
    }
}

/// Inject `source_stage` into a concern (mirrors V1's
/// `normalize_stage_item`).
///
/// Returns `None` for non-object, non-string items (numbers, nulls,
/// booleans) — V1 parity: these are silently dropped.
fn tag_source(item: Value, stage: u8) -> Option<Value> {
    match item {
        Value::Object(mut map) => {
            map.insert("source_stage".to_string(), json!(stage));
            Some(Value::Object(map))
        }
        Value::String(text) => Some(json!({
            "source_stage": stage,
            "type": "General",
            "description": text,
        })),
        _ => None, // Drop non-object, non-string items (V1 parity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NeverProvider;

    #[async_trait::async_trait]
    impl AiProvider for NeverProvider {
        async fn generate_content(
            &self,
            _request: crate::ai::AiRequest,
        ) -> anyhow::Result<crate::ai::AiResponse> {
            panic!("provider must not be called for a step-only pipeline");
        }
        fn estimate_tokens(&self, _request: &crate::ai::AiRequest) -> usize {
            0
        }
        fn get_capabilities(&self) -> crate::ai::ProviderCapabilities {
            crate::ai::ProviderCapabilities {
                model_name: "never".to_string(),
                context_window_size: 1000,
            }
        }
    }

    fn test_env<'a>(prompts: &'a PromptRegistry, dir: &'a std::path::Path) -> PipelineEnv<'a> {
        PipelineEnv {
            provider: Arc::new(NeverProvider),
            tools: Arc::new(ToolBox::new(dir.to_path_buf(), None)),
            prompts,
            temperature: 0.0,
            max_interactions: 3,
            context_tag: None,
            stages: None,
            series_range: None,
        }
    }

    struct MutatePipeline;

    #[async_trait::async_trait]
    impl Pipeline for MutatePipeline {
        fn name(&self) -> &'static str {
            "test"
        }
        async fn run(&self, cx: &mut Cx<'_>) -> Result<()> {
            cx.state
                .concerns
                .push(json!({"type": "X", "description": "d"}));
            cx.state.total_concerns += 1;
            cx.state.review_inline = Some("hello".to_string());
            Ok(())
        }
    }

    struct EarlyReturnPipeline;

    #[async_trait::async_trait]
    impl Pipeline for EarlyReturnPipeline {
        fn name(&self) -> &'static str {
            "test"
        }
        async fn run(&self, cx: &mut Cx<'_>) -> Result<()> {
            // Models an early exit: return before touching review_inline.
            if cx.state.concerns.is_empty() {
                return Ok(());
            }
            cx.state.review_inline = Some("should not run".to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn assembles_worker_result_without_calling_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let prompts = PromptRegistry::new(tmp.path().to_path_buf());
        let env = test_env(&prompts, tmp.path());

        let result = execute_pipeline(&MutatePipeline, &env, json!({}), None)
            .await
            .unwrap();
        let out = result.output.unwrap();
        assert_eq!(out["concerns_count"], 1);
        assert_eq!(out["review_inline"], "hello");
        // findings falls back to the concern list when no Findings stage ran.
        assert_eq!(out["findings"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn early_return_stops_the_pipeline() {
        let tmp = tempfile::tempdir().unwrap();
        let prompts = PromptRegistry::new(tmp.path().to_path_buf());
        let env = test_env(&prompts, tmp.path());

        let result = execute_pipeline(&EarlyReturnPipeline, &env, json!({}), None)
            .await
            .unwrap();
        assert_eq!(result.output.unwrap()["review_inline"], "No issues found.");
    }
}
