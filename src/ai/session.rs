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

use super::token_budget::TokenBudget;
use super::{
    AiErrorClass, AiMessage, AiProvider, AiRequest, AiResponse, AiResponseFormat, AiRole, AiTool,
    AiUsage, ToolCall, classify_ai_error,
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

    /// Executes a tool call requested by the LLM.
    async fn call_tool(&mut self, name: &str, _args: Value) -> Result<Value> {
        anyhow::bail!("Tool execution not implemented for this session: {}", name)
    }

    /// Executes multiple tool calls requested by the LLM.
    /// Default implementation runs them sequentially, and propagates a tool
    /// error rather than reporting it to the model, which ends the session.
    /// A session that wants the model to see and correct its own bad calls
    /// must override this.
    async fn call_tools(&mut self, calls: Vec<ToolCall>) -> Result<Vec<(String, Value)>> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let res = self.call_tool(&call.function_name, call.arguments).await?;
            results.push((call.id, res));
        }
        Ok(results)
    }

    /// Validates the final response content.
    fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError>;

    /// Hook to handle provider errors (e.g. safety blocks, rate limits).
    fn handle_provider_error(&mut self, error: &anyhow::Error, _attempt: usize) -> ErrorAction {
        let err_str = error.to_string();
        if err_str.contains("RECITATION") || err_str.contains("blocked") {
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

/// Orchestrates the execution of an [`LlmSession`].
pub struct SessionRunner<'a> {
    provider: &'a dyn AiProvider,
    max_turns: usize,
    max_input_tokens: Option<usize>,
    max_validation_attempts: usize,
    max_transient_retries: usize,
    max_provider_error_retries: usize,
    on_turn: Option<Box<dyn Fn(usize, usize) + Send + Sync + 'a>>,
}

fn truncate_to_token_budget(content: &str, max_tokens: usize) -> String {
    if TokenBudget::estimate_tokens(content) <= max_tokens {
        return content.to_string();
    }
    let suffix = "\n[... tool result truncated to fit max_input_tokens ...]";
    if TokenBudget::estimate_tokens(suffix) > max_tokens {
        return String::new();
    }

    let mut low = 0;
    let mut high = content.len();
    let mut best = String::new();
    while low <= high {
        let middle = (low + high) / 2;
        let prefix = crate::utils::utf8_prefix(content, middle);
        let candidate = format!("{}{}", prefix, suffix);
        if TokenBudget::estimate_tokens(&candidate) <= max_tokens {
            best = candidate;
            low = middle + 1;
        } else {
            high = middle.saturating_sub(1);
        }
    }
    best
}

impl<'a> SessionRunner<'a> {
    /// Creates a new `SessionRunner` with default limits.
    pub fn new(provider: &'a dyn AiProvider) -> Self {
        Self {
            provider,
            max_turns: 15,
            max_input_tokens: None,
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

    /// Configures the maximum estimated input tokens per request.
    pub fn with_max_input_tokens(mut self, tokens: usize) -> Self {
        self.max_input_tokens = Some(tokens);
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

    fn fit_request_to_input_budget(&self, request: &mut AiRequest) -> Result<()> {
        let Some(limit) = self.max_input_tokens else {
            return Ok(());
        };
        let estimated = self.provider.estimate_tokens(request);
        if estimated <= limit {
            return Ok(());
        }

        let tool_messages: Vec<(usize, usize)> = request
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.role == AiRole::Tool)
            .filter_map(|(index, message)| {
                message
                    .content
                    .as_deref()
                    .map(TokenBudget::estimate_tokens)
                    .map(|tokens| (index, tokens))
            })
            .collect();
        let tool_tokens: usize = tool_messages.iter().map(|(_, tokens)| tokens).sum();
        let fixed_tokens = estimated.saturating_sub(tool_tokens);
        if tool_messages.is_empty() || fixed_tokens >= limit {
            anyhow::bail!(
                "LLM request input (~{} tokens) exceeds max_input_tokens ({}) before tool results can be reduced",
                estimated,
                limit
            );
        }

        let mut order: Vec<usize> = (0..tool_messages.len()).collect();
        order.sort_by_key(|&index| tool_messages[index].1);
        let mut allowed = vec![0; tool_messages.len()];
        let mut remaining = limit - fixed_tokens;
        for (position, &index) in order.iter().enumerate() {
            let fair_share = remaining / (order.len() - position);
            allowed[index] = tool_messages[index].1.min(fair_share);
            remaining -= allowed[index];
        }

        for ((message_index, original_tokens), allowed_tokens) in
            tool_messages.into_iter().zip(allowed)
        {
            if original_tokens <= allowed_tokens {
                continue;
            }
            let content = request.messages[message_index]
                .content
                .as_deref()
                .unwrap_or_default();
            request.messages[message_index].content =
                Some(truncate_to_token_budget(content, allowed_tokens));
        }

        let reduced = self.provider.estimate_tokens(request);
        if reduced > limit {
            anyhow::bail!(
                "LLM request input remains above max_input_tokens after reducing tool results (~{} > {})",
                reduced,
                limit
            );
        }
        tracing::warn!(
            "LLM request input reduced from ~{} to ~{} tokens to honor max_input_tokens ({})",
            estimated,
            reduced,
            limit
        );
        Ok(())
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

        let mut turns = 0;
        let mut validation_attempts = 0;
        let mut transient_retries = 0;
        let mut provider_error_retries = 0;
        let mut total_prompt_tokens = 0;
        let mut total_completion_tokens = 0;
        let mut total_cached_tokens = 0;

        loop {
            turns += 1;
            if turns > self.max_turns {
                anyhow::bail!("Session exceeded max turns limit ({})", self.max_turns);
            }
            if let Some(ref cb) = self.on_turn {
                cb(turns, self.max_turns);
            }

            let mut request = AiRequest {
                system: Some(session.system_prompt()),
                messages: history.clone(),
                tools: session.tools(),
                temperature: session.temperature(),
                response_format: session.response_format(),
                context_tag: session.context_tag(),
            };
            self.fit_request_to_input_budget(&mut request)?;

            let resp = match self.provider.generate_content(request).await {
                Ok(r) => r,
                Err(e) => match classify_ai_error(&e) {
                    AiErrorClass::RateLimit { retry_after }
                    | AiErrorClass::Transient { retry_after } => {
                        transient_retries += 1;
                        if transient_retries > self.max_transient_retries {
                            anyhow::bail!(
                                "Session failed after {} transient/rate-limit errors. Last error: {}",
                                self.max_transient_retries,
                                e
                            );
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
                        match session.handle_provider_error(&e, provider_error_retries) {
                            ErrorAction::RetryWithFeedback(feedback) => {
                                provider_error_retries += 1;
                                if provider_error_retries > self.max_provider_error_retries {
                                    anyhow::bail!(
                                        "Session failed after {} provider error retries. Last error: {}",
                                        self.max_provider_error_retries,
                                        e
                                    );
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
                            ErrorAction::Fail => return Err(e),
                        }
                    }
                },
            };

            if resp.truncated {
                anyhow::bail!("LLM output was truncated by provider (e.g. hit max tokens)");
            }

            if let Some(usage) = &resp.usage {
                total_prompt_tokens += usage.prompt_tokens;
                total_completion_tokens += usage.completion_tokens;
                total_cached_tokens += usage.cached_tokens.unwrap_or(0);
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
            if let Some(tool_calls) = &resp.tool_calls {
                let results = session.call_tools(tool_calls.clone()).await?;
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

            // No tool calls: validate response
            match session.validate(&resp) {
                Result::Ok(output) => {
                    let usage = AiUsage {
                        prompt_tokens: total_prompt_tokens,
                        completion_tokens: total_completion_tokens,
                        total_tokens: total_prompt_tokens + total_completion_tokens,
                        cached_tokens: Some(total_cached_tokens),
                    };
                    return Ok(SessionResult {
                        output,
                        history: log_history,
                        usage,
                    });
                }
                Result::Err(ValidationError::FormatViolation(violation)) => {
                    validation_attempts += 1;
                    if validation_attempts >= self.max_validation_attempts {
                        anyhow::bail!(
                            "Failed to generate valid response after {} validation attempts. Last violation: {}",
                            self.max_validation_attempts,
                            violation
                        );
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
                    anyhow::bail!("Fatal validation error: {}", err);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ProviderCapabilities;
    use serde_json::json;
    use std::sync::Mutex;

    struct LargeToolSession;

    #[async_trait]
    impl LlmSession for LargeToolSession {
        type Output = String;

        fn system_prompt(&self) -> String {
            "system".to_string()
        }

        fn initial_user_prompt(&self) -> String {
            "user".to_string()
        }

        fn tools(&self) -> Option<Vec<AiTool>> {
            Some(vec![AiTool {
                name: "tool".to_string(),
                description: "test tool".to_string(),
                parameters: json!({"type": "object"}),
            }])
        }

        async fn call_tool(&mut self, _name: &str, _args: Value) -> Result<Value> {
            Ok(json!({"output": "x".repeat(100_000)}))
        }

        fn validate(&mut self, response: &AiResponse) -> Result<Self::Output, ValidationError> {
            response
                .content
                .clone()
                .ok_or_else(|| ValidationError::Fatal("missing final response".to_string()))
        }
    }

    struct RecordingProvider {
        requests: Mutex<Vec<AiRequest>>,
    }

    #[async_trait]
    impl AiProvider for RecordingProvider {
        async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            if requests.len() == 1 {
                return Ok(AiResponse {
                    content: None,
                    thought: None,
                    thought_signature: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".to_string(),
                        function_name: "tool".to_string(),
                        arguments: json!({}),
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

        fn estimate_tokens(&self, request: &AiRequest) -> usize {
            request
                .system
                .iter()
                .chain(
                    request
                        .messages
                        .iter()
                        .filter_map(|message| message.content.as_ref()),
                )
                .map(|content| TokenBudget::estimate_tokens(content))
                .sum()
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "test".to_string(),
                context_window_size: 4_000,
            }
        }
    }

    #[tokio::test]
    async fn caps_requests_to_the_configured_input_budget() -> Result<()> {
        let provider = RecordingProvider {
            requests: Mutex::new(Vec::new()),
        };
        SessionRunner::new(&provider)
            .with_max_turns(2)
            .with_max_input_tokens(4_000)
            .run(&mut LargeToolSession)
            .await?;

        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| provider.estimate_tokens(request) <= 4_000)
        );
        let tool_content = requests[1]
            .messages
            .iter()
            .find(|message| message.role == AiRole::Tool)
            .and_then(|message| message.content.as_deref())
            .unwrap();
        assert!(tool_content.contains("tool result truncated"));
        Ok(())
    }
}
