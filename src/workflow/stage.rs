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

//! Declarative stage definitions and executable stage implementations.

use anyhow::Result;
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::ai::{
    AiMessage, AiProvider, AiResponse, AiResponseFormat, AiTool, ErrorAction, LlmSession,
    SessionRunner, ToolCall, ValidationError,
};
use crate::toolbox::ToolBox;

use super::events::WorkflowEvent;
use super::output::OutputFormat;
use super::policy::{RecitationPolicy, StagePolicy, ToolScope};
use super::prompt::PromptTemplate;

/// Outcome and metrics from executing a single stage.
#[derive(Debug, Clone, Default)]
pub struct StageOutcome {
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
    pub history: Vec<AiMessage>,
}

/// Execution environment provided to stages during workflow runs.
pub struct WorkflowEnv<'a> {
    pub provider: Arc<dyn AiProvider>,
    pub tools: Arc<ToolBox>,
    pub base_dir: &'a std::path::Path,
    pub context_tag: Option<String>,
}

/// A deferred state mutation function returned after isolated stage execution.
pub type StateMutation<S> = Box<dyn FnOnce(&mut S) + Send>;

/// A type-erased executable stage that operates on workflow state `S`.
#[async_trait]
pub trait ExecutableStage<S: Send + Sync>: Send + Sync {
    /// Returns the human-readable name of the stage.
    fn name(&self) -> &'static str;

    /// Executes the stage in isolation against immutable state `&S`, returning
    /// execution metrics and a deferred reducer mutation to apply to `&mut S`.
    async fn execute_isolated(
        &self,
        env: &WorkflowEnv<'_>,
        state: &S,
        event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
    ) -> Result<(StageOutcome, StateMutation<S>)>;

    /// Executes the stage and immediately applies its mutation to `&mut S`.
    async fn execute(
        &self,
        env: &WorkflowEnv<'_>,
        state: &mut S,
        event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
    ) -> Result<StageOutcome> {
        let (outcome, mutation) = self.execute_isolated(env, state, event_cb).await?;
        mutation(state);
        Ok(outcome)
    }
}

/// Reducer function applying stage output `T` to mutable workflow state `S`.
pub type StageReducer<S, T> = Arc<dyn Fn(&mut S, T) + Send + Sync>;

/// Conditional predicate determining whether a stage should be evaluated.
pub type StageCondition<S> = Arc<dyn Fn(&S) -> bool + Send + Sync>;

/// A declarative workflow stage with typed output `T` and state reduction.
pub struct Stage<S, T> {
    pub name: &'static str,
    pub system_prompt: Option<PromptTemplate<S>>,
    pub user_prompt: PromptTemplate<S>,
    pub output_format: OutputFormat<S, T>,
    pub policy: StagePolicy,
    pub reducer: StageReducer<S, T>,
    pub skip_if: Option<StageCondition<S>>,
}

impl<S: 'static, T: 'static> Stage<S, T> {
    /// Creates a builder for a stage with a given name.
    pub fn builder(name: &'static str) -> StageBuilder<S, T> {
        StageBuilder::new(name)
    }
}

/// Builder for constructing [`Stage`] instances.
pub struct StageBuilder<S, T> {
    name: &'static str,
    system_prompt: Option<PromptTemplate<S>>,
    user_prompt: Option<PromptTemplate<S>>,
    output_format: Option<OutputFormat<S, T>>,
    policy: StagePolicy,
    reducer: Option<StageReducer<S, T>>,
    skip_if: Option<StageCondition<S>>,
}

impl<S: 'static, T: 'static> StageBuilder<S, T> {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            system_prompt: None,
            user_prompt: None,
            output_format: None,
            policy: StagePolicy::default(),
            reducer: None,
            skip_if: None,
        }
    }

    /// Sets the system prompt template.
    pub fn system_prompt(mut self, template: PromptTemplate<S>) -> Self {
        self.system_prompt = Some(template);
        self
    }

    /// Sets the user prompt template.
    pub fn user_prompt(mut self, template: PromptTemplate<S>) -> Self {
        self.user_prompt = Some(template);
        self
    }

    /// Sets the expected output format.
    pub fn output_format(mut self, fmt: OutputFormat<S, T>) -> Self {
        self.output_format = Some(fmt);
        self
    }

    /// Sets the stage execution policy.
    pub fn policy(mut self, policy: StagePolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Configures the tool availability scope for this stage.
    pub fn tools(mut self, scope: ToolScope) -> Self {
        self.policy.tools = scope;
        self
    }

    /// Sets sampling temperature.
    pub fn temperature(mut self, temp: f32) -> Self {
        self.policy.temperature = temp;
        self
    }

    /// Sets max conversational turns.
    pub fn max_turns(mut self, turns: usize) -> Self {
        self.policy.max_turns = turns;
        self
    }

    /// Configures the recitation error policy.
    pub fn on_recitation(mut self, policy: RecitationPolicy) -> Self {
        self.policy.recitation_policy = policy;
        self
    }

    /// Defines how the stage output `T` mutates the workflow state `&mut S`.
    pub fn reduce<F>(mut self, reducer: F) -> Self
    where
        F: Fn(&mut S, T) + Send + Sync + 'static,
    {
        self.reducer = Some(Arc::new(reducer));
        self
    }

    /// Skips execution of this stage if the predicate returns true.
    pub fn skip_if<F>(mut self, predicate: F) -> Self
    where
        F: Fn(&S) -> bool + Send + Sync + 'static,
    {
        self.skip_if = Some(Arc::new(predicate));
        self
    }

    /// Builds the stage.
    pub fn build(self) -> Stage<S, T> {
        let user_prompt = self
            .user_prompt
            .expect("user_prompt must be configured for stage");
        let output_format = self
            .output_format
            .expect("output_format must be configured for stage");
        let reducer = self.reducer.unwrap_or_else(|| Arc::new(|_, _| {}));

        Stage {
            name: self.name,
            system_prompt: self.system_prompt,
            user_prompt,
            output_format,
            policy: self.policy,
            reducer,
            skip_if: self.skip_if,
        }
    }
}

/// Dynamic adapter bridging [`Stage`] to [`LlmSession`].
struct StageSession<'a, S, T> {
    stage: &'a Stage<S, T>,
    state: &'a S,
    tools: Arc<ToolBox>,
    system_prompt: String,
    user_prompt: String,
    log_user_prompt: String,
    context_tag: Option<String>,
    recitation_fallback_active: bool,
}

#[async_trait]
impl<'a, S: Send + Sync + 'static, T: DeserializeOwned + Send + 'static> LlmSession
    for StageSession<'a, S, T>
{
    type Output = T;

    fn system_prompt(&self) -> String {
        self.system_prompt.clone()
    }

    fn initial_user_prompt(&self) -> String {
        self.user_prompt.clone()
    }

    fn log_user_prompt(&self) -> String {
        self.log_user_prompt.clone()
    }

    fn format_validation_feedback(&self, violation: &str) -> String {
        self.stage.output_format.format_feedback(violation)
    }

    fn tools(&self) -> Option<Vec<AiTool>> {
        match &self.stage.policy.tools {
            ToolScope::None => None,
            ToolScope::All => Some(self.tools.get_declarations_generic()),
            ToolScope::Selected(names) => {
                let all = self.tools.get_declarations_generic();
                Some(
                    all.into_iter()
                        .filter(|t| names.contains(&t.name))
                        .collect(),
                )
            }
        }
    }

    fn temperature(&self) -> Option<f32> {
        Some(self.stage.policy.temperature)
    }

    fn context_tag(&self) -> Option<String> {
        self.context_tag.clone()
    }

    fn response_format(&self) -> Option<AiResponseFormat> {
        if self.recitation_fallback_active {
            return None;
        }
        self.stage
            .output_format
            .schema()
            .map(|s| AiResponseFormat::Json {
                schema: Some(s.clone()),
            })
    }

    async fn call_tool(&mut self, name: &str, args: Value) -> Result<Value> {
        self.tools.call(name, args).await
    }

    async fn call_tools(&mut self, calls: Vec<ToolCall>) -> Result<Vec<(String, Value)>> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            // A rejected call is the model's to read and correct. The trait
            // default propagates the error instead, which ends the stage and
            // with it the review.
            let res = match self.tools.call(&call.function_name, call.arguments).await {
                Ok(v) => v,
                Err(e) => json!({ "error": e.to_string() }),
            };
            results.push((call.id, res));
        }
        Ok(results)
    }

    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
        let text = response.content.as_deref().unwrap_or("");
        match self.stage.output_format.validate(text, self.state) {
            Ok(parsed) => Ok(parsed),
            Err(violation) => Err(ValidationError::FormatViolation(violation)),
        }
    }

    fn handle_provider_error(&mut self, error: &anyhow::Error, _attempt: usize) -> ErrorAction {
        let err_str = error.to_string();
        if err_str.contains("RECITATION") || err_str.contains("blocked") {
            match &self.stage.policy.recitation_policy {
                RecitationPolicy::Fail => ErrorAction::Fail,
                RecitationPolicy::RetryWithReminder(reminder) => {
                    ErrorAction::RetryWithFeedback(reminder.clone())
                }
                RecitationPolicy::FallbackToFreeForm { reminder } => {
                    self.recitation_fallback_active = true;
                    ErrorAction::RetryWithFeedback(reminder.clone())
                }
            }
        } else {
            ErrorAction::Fail
        }
    }
}

#[async_trait]
impl<S: Send + Sync + 'static, T: DeserializeOwned + Send + 'static> ExecutableStage<S>
    for Stage<S, T>
{
    fn name(&self) -> &'static str {
        self.name
    }

    async fn execute_isolated(
        &self,
        env: &WorkflowEnv<'_>,
        state: &S,
        event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
    ) -> Result<(StageOutcome, StateMutation<S>)> {
        if let Some(ref predicate) = self.skip_if
            && predicate(state)
        {
            return Ok((
                StageOutcome {
                    history: Vec::new(),
                    tokens_in: 0,
                    tokens_out: 0,
                    tokens_cached: 0,
                },
                Box::new(|_| {}),
            ));
        }

        if let Some(cb) = event_cb {
            cb(WorkflowEvent::StageStarted {
                stage_name: self.name,
            });
        }

        let system_prompt = if let Some(sys) = &self.system_prompt {
            sys.render_for_model(state, env.base_dir).await?
        } else {
            String::new()
        };

        let user_prompt = self
            .user_prompt
            .render_for_model(state, env.base_dir)
            .await?;
        let log_user_prompt = self.user_prompt.render_for_log(state);

        let stage_name = self.name;
        let result = {
            let mut session = StageSession {
                stage: self,
                state,
                tools: env.tools.clone(),
                system_prompt,
                user_prompt,
                log_user_prompt,
                context_tag: env.context_tag.clone(),
                recitation_fallback_active: false,
            };

            let runner = SessionRunner::new(env.provider.as_ref())
                .with_max_turns(self.policy.max_turns)
                .with_max_validation_attempts(self.policy.max_validation_attempts)
                .with_turn_callback(move |turn, max_turns| {
                    if let Some(cb) = event_cb {
                        cb(WorkflowEvent::StageTurn {
                            stage_name,
                            turn,
                            max_turns,
                        });
                    }
                });

            runner.run(&mut session).await?
        };

        let tokens_in = result.usage.prompt_tokens as u32;
        let tokens_out = result.usage.completion_tokens as u32;
        let tokens_cached = result.usage.cached_tokens.unwrap_or(0) as u32;

        if let Some(cb) = event_cb {
            cb(WorkflowEvent::StageFinished {
                stage_name: self.name,
                tokens_in,
                tokens_out,
                tokens_cached,
            });
        }

        let reducer = self.reducer.clone();
        let mutation: StateMutation<S> = Box::new(move |s: &mut S| {
            reducer(s, result.output);
        });

        let outcome = StageOutcome {
            tokens_in,
            tokens_out,
            tokens_cached,
            history: result.history,
        };

        Ok((outcome, mutation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiRequest, AiRole, ProviderCapabilities};
    use crate::workflow::output::OutputFormat;
    use crate::workflow::prompt::PromptTemplate;
    use std::sync::Mutex;

    #[derive(Default, Clone)]
    struct EmptyState;

    /// Calls a tool on its first turn, then answers. Records every request so a
    /// test can check what the model was told about the tool call.
    struct ToolCallingProvider {
        turn: Mutex<usize>,
        seen: Mutex<Vec<AiRequest>>,
    }

    #[async_trait]
    impl AiProvider for ToolCallingProvider {
        async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
            self.seen.lock().unwrap().push(request);
            let mut turn = self.turn.lock().unwrap();
            *turn += 1;
            if *turn == 1 {
                // git_read_files without the required "revision" argument.
                return Ok(AiResponse {
                    content: None,
                    thought: None,
                    thought_signature: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call_0".to_string(),
                        function_name: "git_read_files".to_string(),
                        arguments: json!({ "files": [{ "path": "x" }] }),
                        thought_signature: None,
                    }]),
                    usage: None,
                    truncated: false,
                });
            }
            Ok(AiResponse {
                content: Some("done".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            0
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 100_000,
            }
        }
    }

    #[tokio::test]
    async fn test_rejected_tool_call_is_reported_to_the_model_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let provider = Arc::new(ToolCallingProvider {
            turn: Mutex::new(0),
            seen: Mutex::new(Vec::new()),
        });
        let env = WorkflowEnv {
            provider: provider.clone(),
            tools: Arc::new(ToolBox::new(tmp.path().to_path_buf(), None)),
            base_dir: tmp.path(),
            context_tag: None,
        };

        let stage: Stage<EmptyState, String> = Stage::builder("tool_error")
            .user_prompt(PromptTemplate::new("go"))
            .output_format(OutputFormat::text())
            .reduce(|_: &mut EmptyState, _: String| {})
            .build();

        let (_outcome, _mutation) = stage
            .execute_isolated(&env, &EmptyState, None)
            .await
            .expect("a rejected tool call must not end the stage");

        // The model must have been handed the error and given another turn.
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "the model should get a second turn");
        let tool_reply = seen[1]
            .messages
            .iter()
            .find(|m| m.role == AiRole::Tool)
            .expect("the second request should carry the tool result");
        assert!(
            tool_reply
                .content
                .as_deref()
                .unwrap_or("")
                .contains("error"),
            "the model should see the tool's error: {:?}",
            tool_reply.content
        );
    }
}
