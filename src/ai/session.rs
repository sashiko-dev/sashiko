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

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use super::{
    AiErrorClass, AiMessage, AiProvider, AiRequest, AiResponse, AiResponseFormat, AiRole, AiTool,
    AiUsage, ReasoningEffort, ToolCall, classify_ai_error,
};

/// The unified result of executing an [`LlmSession`].
pub struct SessionResult<T> {
    /// The validated output of the session.
    pub output: T,
    /// The full conversation history.
    pub history: Vec<AiMessage>,
    /// Accumulated token usage statistics.
    pub usage: AiUsage,
}

/// Error returned when an [`LlmSession`] fails, preserving partial conversation
/// history and token usage accumulated before the failure.
#[derive(Debug, Clone)]
pub struct SessionFailure {
    pub message: String,
    pub history: Vec<AiMessage>,
    pub usage: AiUsage,
}

impl std::fmt::Display for SessionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SessionFailure {}

/// Result of validating a session's final response.
#[derive(Debug)]
pub enum ValidationError {
    /// The response was invalid but can be retried.
    /// Contains a feedback message to append to the LLM prompt.
    FormatViolation(String),
    /// A fatal error that cannot be resolved by retrying.
    Fatal(String),
}

/// Action to take upon encountering a provider error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorAction {
    /// Retry the request after appending the feedback message to the prompt history.
    RetryWithFeedback(String),
    /// Abort the session immediately.
    Fail,
}

/// Represents a stateful, task-oriented interaction session with an LLM.
#[async_trait]
pub trait LlmSession: Send {
    /// The final output type returned by the session after validation.
    type Output: Send;

    /// The system prompt guiding the LLM.
    fn system_prompt(&self) -> String;

    /// The initial user prompt.
    fn initial_user_prompt(&self) -> String;

    /// The user prompt to store in history/logs (for space saving).
    /// Defaults to `initial_user_prompt()`.
    fn log_user_prompt(&self) -> String {
        self.initial_user_prompt()
    }

    /// Customizes the validation feedback message.
    fn format_validation_feedback(&self, violation: &str) -> String {
        format!(
            "Previous attempt was rejected: {}. Please correct your output format.",
            violation
        )
    }

    /// Optional list of tools available in this session.
    fn tools(&self) -> Option<Vec<AiTool>> {
        None
    }

    /// Optional temperature override.
    fn temperature(&self) -> Option<f32> {
        None
    }

    /// Optional context tag for logging.
    fn context_tag(&self) -> Option<String> {
        None
    }

    /// Optional expected response format.
    fn response_format(&self) -> Option<AiResponseFormat> {
        None
    }

    /// Optional reasoning effort override for this session.
    fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        None
    }

    /// Executes a tool call requested by the LLM.
    async fn call_tool(&mut self, name: &str, _args: Value) -> Result<Value> {
        anyhow::bail!("Tool execution not implemented for this session: {}", name)
    }

    /// Executes multiple tool calls requested by the LLM.
    /// Default implementation runs them sequentially and formats errors as tool responses.
    async fn call_tools(&mut self, calls: Vec<ToolCall>) -> Result<Vec<(String, Value)>> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let res = match self.call_tool(&call.function_name, call.arguments).await {
                Ok(val) => val,
                Err(e) => serde_json::json!({
                    "error": e.to_string(),
                }),
            };
            results.push((call.id, res));
        }
        Ok(results)
    }

    /// Validates the final response content.
    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError>;

    /// Hook to handle provider errors (e.g. safety blocks, malformed tool calls).
    /// `has_active_tools` reflects whether tool declarations were active on the
    /// failed request (false when tools are disabled or stripped on the final turn).
    fn handle_provider_error(
        &mut self,
        error: &anyhow::Error,
        _attempt: usize,
        has_active_tools: bool,
    ) -> ErrorAction {
        let err_str = error.to_string();
        if is_malformed_tool_call_error(&err_str) {
            ErrorAction::RetryWithFeedback(malformed_tool_call_feedback(has_active_tools))
        } else if err_str.contains("RECITATION") || err_str.contains("blocked") {
            ErrorAction::RetryWithFeedback(
                "IMPORTANT: Your previous response was blocked by a recitation filter. \
                 Please do NOT copy large blocks of code verbatim in your response. \
                 Describe changes in prose, or use highly simplified pseudo-code if you must show code structure."
                    .to_string(),
            )
        } else {
            ErrorAction::Fail
        }
    }
}

pub(crate) fn is_malformed_tool_call_error(err_str: &str) -> bool {
    err_str.contains("MALFORMED_FUNCTION_CALL") || err_str.contains("UNEXPECTED_TOOL_CALL")
}

pub(crate) fn malformed_tool_call_feedback(has_tools: bool) -> String {
    if has_tools {
        "IMPORTANT: Your previous tool call was malformed or unexpected and could not be executed. \
         Either respond directly in the required output format without calling tools, or emit a \
         valid tool call matching one of the declared tool schemas."
            .to_string()
    } else {
        "IMPORTANT: No tools are available in this stage. Do NOT attempt to call any tools; \
         respond directly in the required output format using only the provided context."
            .to_string()
    }
}

/// Orchestrates the execution of an [`LlmSession`].
pub struct SessionRunner<'a> {
    provider: &'a dyn AiProvider,
    max_turns: usize,
    max_validation_attempts: usize,
    max_transient_retries: usize,
    max_provider_error_retries: usize,
    on_turn: Option<Box<dyn Fn(usize, usize) + Send + Sync + 'a>>,
}

impl<'a> SessionRunner<'a> {
    /// Creates a new `SessionRunner` with default limits.
    pub fn new(provider: &'a dyn AiProvider) -> Self {
        Self {
            provider,
            max_turns: 15,
            max_validation_attempts: 3,
            max_transient_retries: 5,
            max_provider_error_retries: 3,
            on_turn: None,
        }
    }

    /// Configures the maximum validation retries.
    pub fn with_max_validation_attempts(mut self, attempts: usize) -> Self {
        self.max_validation_attempts = attempts;
        self
    }

    /// Configures the maximum conversational turns.
    pub fn with_max_turns(mut self, turns: usize) -> Self {
        self.max_turns = turns;
        self
    }

    /// Configures the maximum transient and rate-limit retries.
    pub fn with_max_transient_retries(mut self, retries: usize) -> Self {
        self.max_transient_retries = retries;
        self
    }

    /// Configures the maximum provider error retries.
    pub fn with_max_provider_error_retries(mut self, retries: usize) -> Self {
        self.max_provider_error_retries = retries;
        self
    }

    /// Configures a turn callback.
    pub fn with_turn_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(usize, usize) + Send + Sync + 'a,
    {
        self.on_turn = Some(Box::new(cb));
        self
    }

    /// Runs the session to completion. Returns the validated output and conversation history (for logging).
    pub async fn run<S>(&self, session: &mut S) -> Result<SessionResult<S::Output>>
    where
        S: LlmSession,
    {
        let mut history = vec![AiMessage {
            role: AiRole::User,
            content: Some(session.initial_user_prompt()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }];

        let mut log_history = vec![AiMessage {
            role: AiRole::User,
            content: Some(session.log_user_prompt()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }];

        let mut turns: usize = 0;
        let mut validation_attempts = 0;
        // Requests whose answers were rejected, forgotten only if the stage
        // gives up.
        let mut rejected: Vec<AiRequest> = Vec::new();
        let mut transient_retries: usize = 0;
        let mut provider_error_retries = 0;
        let mut pushed_final_turn_prompt = false;
        let mut total_prompt_tokens = 0;
        let mut total_completion_tokens = 0;
        let mut total_cached_tokens = 0;

        let make_failure = |message: String,
                            history: Vec<AiMessage>,
                            prompt_tokens: usize,
                            completion_tokens: usize,
                            cached_tokens: usize|
         -> anyhow::Error {
            anyhow::Error::new(SessionFailure {
                message,
                history,
                usage: AiUsage {
                    prompt_tokens,
                    completion_tokens,
                    total_tokens: prompt_tokens.saturating_add(completion_tokens),
                    cached_tokens: Some(cached_tokens),
                },
            })
        };

        loop {
            turns = turns.saturating_add(1);
            if turns > self.max_turns {
                return Err(make_failure(
                    format!("Session exceeded max turns limit ({})", self.max_turns),
                    log_history,
                    total_prompt_tokens,
                    total_completion_tokens,
                    total_cached_tokens,
                ));
            }
            if let Some(ref cb) = self.on_turn {
                cb(turns, self.max_turns);
            }

            let is_final_turn = turns == self.max_turns;
            if is_final_turn && turns > 1 && !pushed_final_turn_prompt {
                pushed_final_turn_prompt = true;
                let final_prompt = AiMessage {
                    role: AiRole::User,
                    content: Some(
                        "TURN BUDGET EXHAUSTED: You have reached the maximum allowed investigation turns. Do NOT call any tools. Synthesize your final JSON verdict now based on the evidence gathered so far."
                            .to_string(),
                    ),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                };
                history.push(final_prompt.clone());
                log_history.push(final_prompt);
            }

            let tools = if is_final_turn { None } else { session.tools() };

            let request = AiRequest {
                system: Some(session.system_prompt()),
                messages: history.clone(),
                tools,
                temperature: session.temperature(),
                response_format: session.response_format(),
                context_tag: session.context_tag(),
                reasoning_effort: session.reasoning_effort(),
            };

            // Kept so that a rejected answer can be forgotten below.
            let sent = request.clone();
            let resp = match self.provider.generate_content(request).await {
                Ok(r) => r,
                Err(e) => match classify_ai_error(&e) {
                    AiErrorClass::RateLimit { retry_after }
                    | AiErrorClass::Transient { retry_after } => {
                        transient_retries = transient_retries.saturating_add(1);
                        if transient_retries > self.max_transient_retries {
                            return Err(make_failure(
                                format!(
                                    "Session failed after {} transient/rate-limit errors. Last error: {}",
                                    self.max_transient_retries, e
                                ),
                                log_history,
                                total_prompt_tokens,
                                total_completion_tokens,
                                total_cached_tokens,
                            ));
                        }
                        tracing::warn!(
                            "API error ({}), pausing for {:?} before retry (attempt {}/{})...",
                            e,
                            retry_after,
                            transient_retries,
                            self.max_transient_retries
                        );
                        tokio::time::sleep(retry_after).await;
                        turns = turns.saturating_sub(1);
                        continue;
                    }
                    AiErrorClass::Fatal => {
                        let has_active_tools = sent.tools.as_ref().is_some_and(|t| !t.is_empty());
                        match session.handle_provider_error(
                            &e,
                            provider_error_retries,
                            has_active_tools,
                        ) {
                            ErrorAction::RetryWithFeedback(feedback) => {
                                provider_error_retries = provider_error_retries.saturating_add(1);
                                if provider_error_retries > self.max_provider_error_retries {
                                    for request in &rejected {
                                        self.provider.forget(request).await;
                                    }
                                    return Err(make_failure(
                                        format!(
                                            "Session failed after {} provider error retries. Last error: {}",
                                            self.max_provider_error_retries, e
                                        ),
                                        log_history,
                                        total_prompt_tokens,
                                        total_completion_tokens,
                                        total_cached_tokens,
                                    ));
                                }
                                let msg = AiMessage {
                                    role: AiRole::User,
                                    content: Some(feedback.clone()),
                                    thought: None,
                                    thought_signature: None,
                                    tool_calls: None,
                                    tool_call_id: None,
                                };
                                history.push(msg.clone());
                                log_history.push(msg);
                                turns = turns.saturating_sub(1);
                                continue;
                            }
                            ErrorAction::Fail => {
                                for request in &rejected {
                                    self.provider.forget(request).await;
                                }
                                let message = e.to_string();
                                return Err(e.context(SessionFailure {
                                    message,
                                    history: log_history,
                                    usage: AiUsage {
                                        prompt_tokens: total_prompt_tokens,
                                        completion_tokens: total_completion_tokens,
                                        total_tokens: total_prompt_tokens
                                            .saturating_add(total_completion_tokens),
                                        cached_tokens: Some(total_cached_tokens),
                                    },
                                }));
                            }
                        }
                    }
                },
            };

            if resp.truncated {
                // Giving up, as when the answers are rejected below: forget
                // this cut-off answer and any the stage rejected before it, so
                // that a retry asks the model again.
                for request in rejected.iter().chain(std::iter::once(&sent)) {
                    self.provider.forget(request).await;
                }
                return Err(make_failure(
                    "LLM output was truncated by provider (e.g. hit max tokens)".to_string(),
                    log_history,
                    total_prompt_tokens,
                    total_completion_tokens,
                    total_cached_tokens,
                ));
            }

            if let Some(usage) = &resp.usage {
                total_prompt_tokens = total_prompt_tokens.saturating_add(usage.prompt_tokens);
                total_completion_tokens =
                    total_completion_tokens.saturating_add(usage.completion_tokens);
                total_cached_tokens =
                    total_cached_tokens.saturating_add(usage.cached_tokens.unwrap_or(0));
            }

            let assistant_msg = AiMessage {
                role: AiRole::Assistant,
                content: resp.content.clone(),
                thought: resp.thought.clone(),
                thought_signature: resp.thought_signature.clone(),
                tool_calls: resp.tool_calls.clone(),
                tool_call_id: None,
            };
            history.push(assistant_msg.clone());
            log_history.push(assistant_msg);

            // Handle Tool Calls
            if let Some(tool_calls) = &resp.tool_calls
                && !tool_calls.is_empty()
            {
                if is_final_turn {
                    tracing::warn!(
                        "Model emitted tool calls on final turn; ignoring tools to force validation."
                    );
                } else {
                    let results = match session.call_tools(tool_calls.clone()).await {
                        Ok(r) => r,
                        Err(e) => {
                            let message = e.to_string();
                            return Err(e.context(SessionFailure {
                                message,
                                history: log_history,
                                usage: AiUsage {
                                    prompt_tokens: total_prompt_tokens,
                                    completion_tokens: total_completion_tokens,
                                    total_tokens: total_prompt_tokens
                                        .saturating_add(total_completion_tokens),
                                    cached_tokens: Some(total_cached_tokens),
                                },
                            }));
                        }
                    };
                    for (call_id, result) in results {
                        let tool_msg = AiMessage {
                            role: AiRole::Tool,
                            content: Some(result.to_string()),
                            thought: None,
                            thought_signature: None,
                            tool_calls: None,
                            tool_call_id: Some(call_id),
                        };
                        history.push(tool_msg.clone());
                        log_history.push(tool_msg);
                    }
                    continue; // Loop again to feed tool results back to LLM
                }
            }

            // No tool calls: validate response
            match session.validate(&resp) {
                Result::Ok(output) => {
                    let usage = AiUsage {
                        prompt_tokens: total_prompt_tokens,
                        completion_tokens: total_completion_tokens,
                        total_tokens: total_prompt_tokens.saturating_add(total_completion_tokens),
                        cached_tokens: Some(total_cached_tokens),
                    };
                    return Ok(SessionResult {
                        output,
                        history: log_history,
                        usage,
                    });
                }
                Result::Err(ValidationError::FormatViolation(violation)) => {
                    rejected.push(sent);
                    validation_attempts += 1;
                    if validation_attempts >= self.max_validation_attempts {
                        // Giving up: forget the rejected answers, or a
                        // retry would be handed them and fail the same way.
                        for request in &rejected {
                            self.provider.forget(request).await;
                        }
                        return Err(make_failure(
                            format!(
                                "Failed to generate valid response after {} validation attempts. Last violation: {}",
                                self.max_validation_attempts, violation
                            ),
                            log_history,
                            total_prompt_tokens,
                            total_completion_tokens,
                            total_cached_tokens,
                        ));
                    }
                    let feedback = session.format_validation_feedback(&violation);
                    let msg = AiMessage {
                        role: AiRole::User,
                        content: Some(feedback),
                        thought: None,
                        thought_signature: None,
                        tool_calls: None,
                        tool_call_id: None,
                    };
                    history.push(msg.clone());
                    log_history.push(msg);
                    turns = turns.saturating_sub(1);
                }
                Result::Err(ValidationError::Fatal(err)) => {
                    return Err(make_failure(
                        format!("Fatal validation error: {}", err),
                        log_history,
                        total_prompt_tokens,
                        total_completion_tokens,
                        total_cached_tokens,
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ProviderCapabilities;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct MockProvider {
        responses: Mutex<VecDeque<AiResponse>>,
    }

    impl MockProvider {
        fn new(responses: Vec<AiResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }
    }

    #[async_trait]
    impl AiProvider for MockProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            let mut q = self.responses.lock().unwrap();
            q.pop_front()
                .ok_or_else(|| anyhow::anyhow!("No more mock responses"))
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 4096,
            }
        }
    }

    struct DummySession;

    #[async_trait]
    impl LlmSession for DummySession {
        type Output = String;

        fn system_prompt(&self) -> String {
            "system".to_string()
        }

        fn initial_user_prompt(&self) -> String {
            "initial prompt".to_string()
        }

        async fn call_tool(&mut self, name: &str, _args: Value) -> Result<Value> {
            if name == "fail" {
                anyhow::bail!("Tool execution failed");
            }
            Ok(serde_json::json!({"result": "success"}))
        }

        fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
            Ok(response.content.clone().unwrap_or_default())
        }
    }

    #[tokio::test]
    async fn test_call_tools_captures_errors_as_json() {
        let mut session = DummySession;
        let calls = vec![
            ToolCall {
                id: "call_1".to_string(),
                function_name: "ok_tool".to_string(),
                arguments: serde_json::json!({}),
                thought_signature: None,
            },
            ToolCall {
                id: "call_2".to_string(),
                function_name: "fail".to_string(),
                arguments: serde_json::json!({}),
                thought_signature: None,
            },
        ];

        let results = session.call_tools(calls).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "call_1");
        assert_eq!(results[0].1, serde_json::json!({"result": "success"}));
        assert_eq!(results[1].0, "call_2");
        assert_eq!(
            results[1].1,
            serde_json::json!({"error": "Tool execution failed"})
        );
    }

    #[tokio::test]
    async fn test_session_runner_survives_tool_error() {
        let responses = vec![
            AiResponse {
                content: None,
                thought: None,
                thought_signature: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_err".to_string(),
                    function_name: "fail".to_string(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                }]),
                usage: None,
                truncated: false,
            },
            AiResponse {
                content: Some("Recovered after tool error".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            },
        ];

        let provider = MockProvider::new(responses);
        let runner = SessionRunner::new(&provider);
        let mut session = DummySession;

        let res = runner.run(&mut session).await.unwrap();
        assert_eq!(res.output, "Recovered after tool error");

        // Verify history contains the error response for the tool
        let tool_msg = res.history.iter().find(|m| m.role == AiRole::Tool).unwrap();
        assert_eq!(tool_msg.tool_call_id, Some("call_err".to_string()));
        assert!(
            tool_msg
                .content
                .as_ref()
                .unwrap()
                .contains("Tool execution failed")
        );
    }

    #[tokio::test]
    async fn test_session_runner_forces_synthesis_on_max_turns() {
        let responses = vec![
            // Turn 1: tool call
            AiResponse {
                content: None,
                thought: None,
                thought_signature: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    function_name: "ok_tool".to_string(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                }]),
                usage: None,
                truncated: false,
            },
            // Turn 2 (max turns): synthesized output
            AiResponse {
                content: Some("Final synthesized verdict".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            },
        ];

        let provider = MockProvider::new(responses);
        let runner = SessionRunner::new(&provider).with_max_turns(2);
        let mut session = DummySession;

        let res = runner.run(&mut session).await.unwrap();
        assert_eq!(res.output, "Final synthesized verdict");

        // Verify history contains the budget exhausted user message
        let exhausted_msg = res.history.iter().find(|m| {
            m.role == AiRole::User
                && m.content
                    .as_deref()
                    .unwrap_or("")
                    .contains("TURN BUDGET EXHAUSTED")
        });
        assert!(exhausted_msg.is_some());
    }

    #[tokio::test]
    async fn test_session_runner_recovers_from_unexpected_tool_call_on_final_turn_without_duplicate_budget_prompt()
     {
        struct FinalTurnMalformedProvider {
            calls: std::sync::Mutex<usize>,
        }

        #[async_trait]
        impl AiProvider for FinalTurnMalformedProvider {
            async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                match *calls {
                    1 => Ok(AiResponse {
                        content: None,
                        thought: None,
                        thought_signature: None,
                        tool_calls: Some(vec![ToolCall {
                            id: "call_1".to_string(),
                            function_name: "ok_tool".to_string(),
                            arguments: serde_json::json!({}),
                            thought_signature: None,
                        }]),
                        usage: None,
                        truncated: false,
                    }),
                    2 => anyhow::bail!(
                        "Gemini candidate failed tool call (finish reason: UNEXPECTED_TOOL_CALL)"
                    ),
                    _ => Ok(AiResponse {
                        content: Some("Recovered final verdict".to_string()),
                        thought: None,
                        thought_signature: None,
                        tool_calls: None,
                        usage: None,
                        truncated: false,
                    }),
                }
            }

            fn get_capabilities(&self) -> super::super::ProviderCapabilities {
                super::super::ProviderCapabilities {
                    model_name: "mock".to_string(),
                    context_window_size: 100_000,
                }
            }
        }

        let provider = FinalTurnMalformedProvider {
            calls: std::sync::Mutex::new(0),
        };
        let runner = SessionRunner::new(&provider).with_max_turns(2);

        struct ToolEnabledSession;
        #[async_trait]
        impl LlmSession for ToolEnabledSession {
            type Output = String;

            fn system_prompt(&self) -> String {
                "sys".to_string()
            }

            fn initial_user_prompt(&self) -> String {
                "user".to_string()
            }

            fn tools(&self) -> Option<Vec<AiTool>> {
                Some(vec![AiTool {
                    name: "ok_tool".to_string(),
                    description: "ok".to_string(),
                    parameters: serde_json::json!({"type": "object", "properties": {}}),
                }])
            }

            async fn call_tool(&mut self, _name: &str, _args: Value) -> Result<Value> {
                Ok(serde_json::json!({"result": "success"}))
            }

            fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
                Ok(response.content.clone().unwrap_or_default())
            }
        }

        let mut session = ToolEnabledSession;

        let res = runner.run(&mut session).await.unwrap();
        assert_eq!(res.output, "Recovered final verdict");

        let exhausted_count = res
            .history
            .iter()
            .filter(|m| {
                m.role == AiRole::User
                    && m.content
                        .as_deref()
                        .unwrap_or("")
                        .contains("TURN BUDGET EXHAUSTED")
            })
            .count();
        assert_eq!(
            exhausted_count, 1,
            "final-turn retry must not duplicate TURN BUDGET EXHAUSTED prompt"
        );
        let feedback_has_no_tools = res.history.iter().any(|m| {
            m.role == AiRole::User
                && m.content
                    .as_deref()
                    .unwrap_or("")
                    .contains("No tools are available in this stage")
        });
        assert!(
            feedback_has_no_tools,
            "final-turn retry after tools are stripped must instruct model not to call tools"
        );
    }
}
