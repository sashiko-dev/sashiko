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

// This code was based on openai.rs
//
// vLLM serves an OpenAI-compatible API, but differs in a few ways that
// warrant a dedicated provider:
//  - `max_tokens` can be omitted entirely; vLLM then generates up to the
//    remaining context (`max_model_len - prompt_tokens`), which matters for
//    servers running with small `--max-model-len` values.
//  - reasoning models (e.g. Qwen3) emit their chain of thought either in a
//    separate `reasoning_content` field (when the server runs with a
//    reasoning parser) or inline as a `<think>...</think>` block in the
//    content, which must be stripped before JSON parsing downstream.
//  - thinking can be disabled per-request via
//    `chat_template_kwargs: {"enable_thinking": false}`.
//  - guided decoding (`response_format: {"type": "json_object"}`) is not
//    supported by every backend (e.g. OpenVINO returns HTTP 500), so it is
//    opt-in; by default the JSON requirement is injected into the system
//    prompt instead.
//  - a server started without `--enable-auto-tool-choice` rejects any
//    request carrying tool definitions with HTTP 400, so forwarding tools
//    is opt-in as well.
//  - a prompt larger than `--max-model-len` is rejected with HTTP 400,
//    where Ollama silently truncates at `num_ctx`. Message contents are
//    truncated client-side to fit the context window so both local
//    backends behave the same.

use crate::ai::token_budget::TokenBudget;
use crate::ai::{
    AiErrorClass, AiProvider, AiRequest, AiResponse, AiResponseFormat, AiRole, AiUsage,
    ClassifyAiError, ProviderCapabilities, ToolCall, classify_status_code,
};
use crate::utils::redact_secret;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Serialize, Deserialize)]
pub struct VllmRequest {
    pub model: String,
    pub messages: Vec<VllmMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<VllmTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VllmMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<VllmToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Chain of thought returned by servers running with a reasoning parser
    /// (`--reasoning-parser`). Depending on the vLLM version the field is
    /// named `reasoning_content` or `reasoning`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VllmToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: VllmToolCallFunction,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VllmToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VllmTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: VllmFunction,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct VllmFunction {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VllmResponse {
    pub choices: Vec<VllmChoice>,
    pub usage: VllmUsage,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VllmChoice {
    pub index: u32,
    pub message: VllmMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VllmUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum VllmError {
    #[error("Rate limit exceeded, retry after {0:?}")]
    RateLimitExceeded(Duration),
    #[error("Transient error: {1}, retry after {0:?}")]
    TransientError(Duration, String),
    #[error("Authentication error: {0}")]
    AuthenticationError(String),
    #[error("API error {0}: {1}")]
    ApiError(reqwest::StatusCode, String),
}

impl ClassifyAiError for VllmError {
    fn ai_error_class(&self) -> AiErrorClass {
        match self {
            VllmError::RateLimitExceeded(retry_after) => AiErrorClass::RateLimit {
                retry_after: *retry_after,
            },
            VllmError::TransientError(retry_after, _) => AiErrorClass::Transient {
                retry_after: *retry_after,
            },
            VllmError::AuthenticationError(_) => AiErrorClass::Fatal,
            VllmError::ApiError(status, _) => {
                classify_status_code(*status).unwrap_or(AiErrorClass::Fatal)
            }
        }
    }
}

pub struct VllmClient {
    model: String,
    base_url: String,
    context_window_size: usize,
    max_tokens: Option<u32>,
    enable_thinking: Option<bool>,
    guided_json: bool,
    enable_tools: bool,
    client: Client,
}

impl VllmClient {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_url: String,
        model: String,
        context_window_size: usize,
        max_tokens: Option<u32>,
        api_timeout_secs: u64,
        enable_thinking: Option<bool>,
        guided_json: bool,
        enable_tools: bool,
    ) -> Result<Self> {
        // vLLM only checks credentials when started with `--api-key`.
        let api_key = std::env::var("VLLM_API_KEY")
            .or_else(|_| std::env::var("LLM_API_KEY"))
            .unwrap_or_default();

        let mut headers = reqwest::header::HeaderMap::new();
        if !api_key.is_empty()
            && let Ok(value) =
                reqwest::header::HeaderValue::from_str(&format!("Bearer {}", api_key))
        {
            headers.insert("Authorization", value);
        }

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(api_timeout_secs))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let base_url = Self::normalize_base_url(&base_url)?;

        Ok(Self {
            model,
            base_url,
            context_window_size,
            max_tokens,
            enable_thinking,
            guided_json,
            enable_tools,
            client,
        })
    }

    /// Normalize a base URL so it always ends with `/chat/completions`.
    ///
    /// vLLM documents the base URL as `http://localhost:8000/v1`, expecting
    /// the client to append the endpoint path. Our `post_request` POSTs
    /// directly to `self.base_url`, so we ensure the full path is present.
    fn normalize_base_url(url: &str) -> Result<String> {
        let trimmed = url.trim_end_matches('/');

        let (base, path) = match trimmed.split_once("://") {
            Some((scheme, rest)) => match rest.split_once('/') {
                Some((host, path)) => (format!("{scheme}://{host}"), format!("/{}", path)),
                None => (trimmed.to_string(), String::new()),
            },
            None => return Err(anyhow::anyhow!("Invalid url scheme in vLLM url {}", url)),
        };

        if path.ends_with("/chat/completions") {
            return Ok(format!("{base}{path}"));
        }

        let path = match path.as_str() {
            "" | "/v1" | "/v1/chat/completions" => "/v1/chat/completions",
            _ => return Err(anyhow::anyhow!("Invalid vLLM url {}", url)),
        };

        Ok(format!("{base}{path}"))
    }

    /// Default base URL for vLLM (local instance).
    pub fn default_base_url() -> String {
        "http://localhost:8000".to_string()
    }

    /// Fallback context window when none is configured. The real limit is the
    /// server-side `--max-model-len`; set `context_window_size` to match it.
    pub fn default_context_window_for_model(_model: &str) -> usize {
        32_768
    }

    async fn post_request(&self, body: &Value) -> Result<VllmResponse, VllmError> {
        let res = match self.client.post(&self.base_url).json(body).send().await {
            Ok(res) => res,
            Err(e) => {
                let err_str = redact_secret(&e.to_string());
                tracing::error!("vLLM request failed (transport): {}", err_str);
                return Err(VllmError::TransientError(Duration::from_secs(30), err_str));
            }
        };

        if res.status().is_success() {
            let status = res.status();
            let body_text = res.text().await.map_err(|e| {
                let err_str = redact_secret(&e.to_string());
                tracing::error!("Failed to read vLLM response body: {}", err_str);
                VllmError::TransientError(Duration::from_secs(30), err_str)
            })?;
            match serde_json::from_str::<VllmResponse>(&body_text) {
                Ok(response) => {
                    tracing::info!(
                        "vLLM response received. Tokens: in={}, out={}",
                        response.usage.prompt_tokens,
                        response.usage.completion_tokens
                    );
                    return Ok(response);
                }
                Err(e) => {
                    tracing::error!("Failed to decode vLLM response: {}", e);
                    return Err(VllmError::ApiError(status, format!("Parse error: {}", e)));
                }
            }
        }

        let status = res.status();
        let status_code = status.as_u16();

        let retry_after_duration = res
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs);

        let error_text = redact_secret(&res.text().await.unwrap_or_default());

        match status_code {
            429 => {
                let retry_after = retry_after_duration.unwrap_or(Duration::from_secs(60));
                tracing::warn!("vLLM 429 Rate Limit. Retry in {:?}", retry_after);
                Err(VllmError::RateLimitExceeded(retry_after))?
            }
            401 | 403 => Err(VllmError::AuthenticationError(error_text))?,
            500..=599 => {
                tracing::warn!("vLLM Server Error {}: {}", status, error_text);
                Err(VllmError::TransientError(
                    retry_after_duration.unwrap_or(Duration::from_secs(0)),
                    error_text,
                ))?
            }
            _ => Err(VllmError::ApiError(status, error_text))?,
        }
    }
}

fn translate_vllm_request(
    request: AiRequest,
    max_tokens: Option<u32>,
    enable_thinking: Option<bool>,
    guided_json: bool,
    enable_tools: bool,
) -> Result<VllmRequest> {
    let mut messages = Vec::new();

    let mut system_text = request.system.unwrap_or_default();

    // Without guided decoding the JSON requirement has to be enforced through
    // the prompt instead of `response_format`.
    if !guided_json
        && let Some(instruction) = request
            .response_format
            .as_ref()
            .and_then(|f| f.format_json_schema_instruction())
    {
        if !system_text.is_empty() {
            system_text.push_str("\n\n");
        }
        system_text.push_str(&instruction);
    }

    if !system_text.is_empty() {
        messages.push(VllmMessage {
            role: "system".to_string(),
            content: Some(system_text),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            reasoning: None,
        });
    }

    for msg in request.messages {
        match msg.role {
            AiRole::System => {
                messages.push(VllmMessage {
                    role: "system".to_string(),
                    content: msg.content,
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    reasoning: None,
                });
            }
            AiRole::User => {
                messages.push(VllmMessage {
                    role: "user".to_string(),
                    content: msg.content,
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    reasoning: None,
                });
            }
            AiRole::Assistant => {
                messages.push(VllmMessage {
                    role: "assistant".to_string(),
                    content: msg.content,
                    tool_calls: msg.tool_calls.map(|tc| {
                        tc.into_iter()
                            .map(|t| VllmToolCall {
                                id: t.id,
                                tool_type: "function".to_string(),
                                function: VllmToolCallFunction {
                                    name: t.function_name,
                                    arguments: serde_json::to_string(&t.arguments).unwrap(),
                                },
                            })
                            .collect()
                    }),
                    tool_call_id: None,
                    reasoning_content: None,
                    reasoning: None,
                });
            }
            AiRole::Tool => {
                messages.push(VllmMessage {
                    role: "tool".to_string(),
                    content: msg.content,
                    tool_calls: None,
                    tool_call_id: msg.tool_call_id,
                    reasoning_content: None,
                    reasoning: None,
                });
            }
        }
    }

    let tools = request.tools.filter(|_| enable_tools).and_then(|t| {
        if t.is_empty() {
            None
        } else {
            Some(
                t.into_iter()
                    .map(|tool| VllmTool {
                        tool_type: "function".to_string(),
                        function: VllmFunction {
                            name: tool.name,
                            description: tool.description,
                            parameters: tool.parameters,
                        },
                    })
                    .collect(),
            )
        }
    });

    let response_format = if guided_json {
        request.response_format.map(|rf| match rf {
            AiResponseFormat::Json { .. } => serde_json::json!({"type": "json_object"}),
            AiResponseFormat::Text => serde_json::json!({"type": "text"}),
        })
    } else {
        None
    };

    let chat_template_kwargs =
        enable_thinking.map(|enabled| serde_json::json!({"enable_thinking": enabled}));

    Ok(VllmRequest {
        model: String::new(),
        messages,
        tools,
        temperature: request.temperature,
        max_tokens,
        response_format,
        chat_template_kwargs,
    })
}

/// Truncate message contents so the request fits within `input_budget`
/// tokens.
///
/// vLLM rejects prompts larger than the server's `--max-model-len` with
/// HTTP 400 (Ollama silently truncates at `num_ctx` instead), which would
/// fail any review whose prompt outgrows a small context window. The budget
/// is handed out smallest-message-first so short messages (system
/// instructions, tool results) survive intact and only the oversized ones
/// are cut.
fn fit_messages_to_budget(messages: &mut [VllmMessage], input_budget: usize) {
    let counts: Vec<usize> = messages
        .iter()
        .map(|m| m.content.as_deref().map_or(0, TokenBudget::estimate_tokens))
        .collect();
    let total: usize = counts.iter().sum();
    if total <= input_budget {
        return;
    }
    tracing::warn!(
        "{}vLLM prompt (~{} tokens) exceeds the input budget ({} tokens); truncating to fit the context window",
        crate::ai::get_log_prefix(),
        total,
        input_budget
    );

    let mut order: Vec<usize> = (0..messages.len()).collect();
    order.sort_by_key(|&i| counts[i]);
    let mut allowed = vec![0usize; messages.len()];
    let mut budget = input_budget;
    for (pos, &i) in order.iter().enumerate() {
        let fair = budget / (order.len() - pos);
        allowed[i] = counts[i].min(fair);
        budget -= allowed[i];
    }

    for (i, msg) in messages.iter_mut().enumerate() {
        if counts[i] == 0 || allowed[i] >= counts[i] {
            continue;
        }
        let content = msg.content.as_deref().unwrap_or_default();
        let keep_bytes = content.len() * allowed[i] / counts[i];
        let kept = crate::utils::utf8_prefix(content, keep_bytes);
        msg.content = Some(format!(
            "{}\n[... truncated to fit the model context window ...]",
            kept
        ));
    }
}

/// Split an inline `<think>...</think>` block off the content.
///
/// Reasoning models emit their chain of thought inline when the server runs
/// without a reasoning parser. Returns `(thought, content)`. An unclosed
/// `<think>` block (generation cut off mid-reasoning) yields no content.
fn split_think_block(content: Option<String>) -> (Option<String>, Option<String>) {
    let Some(text) = content else {
        return (None, None);
    };
    let Some(rest) = text.trim_start().strip_prefix("<think>") else {
        return (None, Some(text));
    };
    match rest.split_once("</think>") {
        Some((thought, answer)) => {
            let thought = thought.trim();
            let answer = answer.trim();
            (
                (!thought.is_empty()).then(|| thought.to_string()),
                (!answer.is_empty()).then(|| answer.to_string()),
            )
        }
        None => {
            let thought = rest.trim();
            ((!thought.is_empty()).then(|| thought.to_string()), None)
        }
    }
}

fn translate_vllm_response(resp: VllmResponse) -> Result<AiResponse> {
    let choice = resp
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No choices in response"))?;

    let reasoning = choice
        .message
        .reasoning_content
        .or(choice.message.reasoning);
    let (thought, content) = if reasoning.is_some() {
        (reasoning, choice.message.content)
    } else {
        split_think_block(choice.message.content)
    };

    let tool_calls = choice.message.tool_calls.map(|tc| {
        tc.into_iter()
            .map(|t| {
                let arguments: Value =
                    serde_json::from_str(&t.function.arguments).unwrap_or(serde_json::Value::Null);
                ToolCall {
                    id: t.id,
                    function_name: t.function.name,
                    arguments,
                    thought_signature: None,
                }
            })
            .collect()
    });

    let truncated = choice.finish_reason.as_deref() == Some("length");

    if truncated {
        tracing::warn!(
            "{}vLLM response truncated due to finish_reason = length.",
            crate::ai::get_log_prefix()
        );
    }

    let usage = Some(AiUsage {
        prompt_tokens: resp.usage.prompt_tokens as usize,
        completion_tokens: resp.usage.completion_tokens as usize,
        total_tokens: resp.usage.total_tokens as usize,
        cached_tokens: None,
    });

    Ok(AiResponse {
        content,
        thought,
        thought_signature: None,
        tool_calls,
        usage,
        truncated,
    })
}

fn estimate_tokens_generic(request: &AiRequest) -> usize {
    let mut total = 0;
    if let Some(system) = &request.system {
        total += TokenBudget::estimate_tokens(system);
    }
    for msg in &request.messages {
        if let Some(content) = &msg.content {
            total += TokenBudget::estimate_tokens(content);
        }
        if let Some(tool_calls) = &msg.tool_calls {
            for call in tool_calls {
                total += TokenBudget::estimate_tokens(&call.function_name);
                total += TokenBudget::estimate_tokens(&call.arguments.to_string());
            }
        }
    }
    if let Some(tools) = &request.tools {
        for tool in tools {
            total += TokenBudget::estimate_tokens(&tool.name);
            total += TokenBudget::estimate_tokens(&tool.description);
            total += TokenBudget::estimate_tokens(&tool.parameters.to_string());
        }
    }
    total
}

#[async_trait]
impl AiProvider for VllmClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        tracing::info!("Sending vLLM request to model: {}", self.model);

        let mut vllm_req = translate_vllm_request(
            request,
            self.max_tokens,
            self.enable_thinking,
            self.guided_json,
            self.enable_tools,
        )?;
        vllm_req.model = self.model.clone();

        // Leave room for generation, with a margin for the mismatch between
        // our token estimator and the served model's tokenizer plus chat
        // template overhead.
        let generation_reserve = self
            .max_tokens
            .map(|m| m as usize)
            .unwrap_or(self.context_window_size / 4);
        let input_budget = self.context_window_size.saturating_sub(generation_reserve) * 9 / 10;
        fit_messages_to_budget(&mut vllm_req.messages, input_budget);

        let resp_body = serde_json::to_value(&vllm_req)?;
        let resp = self.post_request(&resp_body).await?;
        translate_vllm_response(resp)
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        estimate_tokens_generic(request)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: self.context_window_size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiErrorClass, AiMessage, ClassifyAiError, DEFAULT_RETRY_AFTER};
    use serde_json::json;

    fn user_request(content: &str) -> AiRequest {
        AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some(content.to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        }
    }

    fn response_with_message(message: VllmMessage, finish_reason: &str) -> VllmResponse {
        VllmResponse {
            choices: vec![VllmChoice {
                index: 0,
                message,
                finish_reason: Some(finish_reason.to_string()),
            }],
            usage: VllmUsage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
            },
        }
    }

    fn assistant_message(content: Option<&str>) -> VllmMessage {
        VllmMessage {
            role: "assistant".to_string(),
            content: content.map(|c| c.to_string()),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            reasoning: None,
        }
    }

    #[test]
    fn test_normalize_base_url() {
        assert_eq!(
            VllmClient::normalize_base_url("http://localhost:8000").unwrap(),
            "http://localhost:8000/v1/chat/completions"
        );
        assert_eq!(
            VllmClient::normalize_base_url("http://localhost:8000/").unwrap(),
            "http://localhost:8000/v1/chat/completions"
        );
        assert_eq!(
            VllmClient::normalize_base_url("http://localhost:8000/v1").unwrap(),
            "http://localhost:8000/v1/chat/completions"
        );
        assert_eq!(
            VllmClient::normalize_base_url("http://localhost:8000/v1/chat/completions").unwrap(),
            "http://localhost:8000/v1/chat/completions"
        );
        // A full chat completions URL with an unusual prefix is accepted
        // verbatim (e.g. behind a reverse proxy).
        assert_eq!(
            VllmClient::normalize_base_url("https://gw.example.com/vllm/v1/chat/completions")
                .unwrap(),
            "https://gw.example.com/vllm/v1/chat/completions"
        );
        assert!(VllmClient::normalize_base_url("http://localhost:8000/v2").is_err());
        assert!(VllmClient::normalize_base_url("not-a-url").is_err());
    }

    #[test]
    fn test_default_base_url() {
        assert_eq!(VllmClient::default_base_url(), "http://localhost:8000");
    }

    #[test]
    fn test_translate_request_system_and_user() -> Result<()> {
        let mut request = user_request("Hello!");
        request.system = Some("You are helpful.".to_string());
        request.temperature = Some(0.7);

        let vllm_req = translate_vllm_request(request, Some(4096), None, false, false)?;

        assert_eq!(vllm_req.messages.len(), 2);
        assert_eq!(vllm_req.messages[0].role, "system");
        assert_eq!(
            vllm_req.messages[0].content,
            Some("You are helpful.".to_string())
        );
        assert_eq!(vllm_req.messages[1].role, "user");
        assert_eq!(vllm_req.messages[1].content, Some("Hello!".to_string()));
        assert_eq!(vllm_req.temperature, Some(0.7));
        assert_eq!(vllm_req.max_tokens, Some(4096));
        assert!(vllm_req.chat_template_kwargs.is_none());

        Ok(())
    }

    #[test]
    fn test_translate_request_omits_max_tokens_when_unset() -> Result<()> {
        let vllm_req = translate_vllm_request(user_request("Test"), None, None, false, false)?;

        assert_eq!(vllm_req.max_tokens, None);
        let json = serde_json::to_value(&vllm_req)?;
        assert!(json.get("max_tokens").is_none());

        Ok(())
    }

    #[test]
    fn test_translate_request_enable_thinking_kwargs() -> Result<()> {
        let vllm_req =
            translate_vllm_request(user_request("Test"), None, Some(false), false, false)?;

        assert_eq!(
            vllm_req.chat_template_kwargs,
            Some(json!({"enable_thinking": false}))
        );

        Ok(())
    }

    #[test]
    fn test_translate_request_json_instruction_injected_without_guided_json() -> Result<()> {
        let mut request = user_request("Score this.");
        request.system = Some("You are helpful.".to_string());
        request.response_format = Some(AiResponseFormat::Json { schema: None });

        let vllm_req = translate_vllm_request(request, None, None, false, false)?;

        assert!(vllm_req.response_format.is_none());
        assert_eq!(vllm_req.messages[0].role, "system");
        let system = vllm_req.messages[0].content.as_ref().unwrap();
        assert!(system.starts_with("You are helpful."));
        assert!(system.contains("valid JSON object"));

        Ok(())
    }

    #[test]
    fn test_translate_request_json_instruction_creates_system_message() -> Result<()> {
        let mut request = user_request("Score this.");
        request.response_format = Some(AiResponseFormat::Json { schema: None });

        let vllm_req = translate_vllm_request(request, None, None, false, false)?;

        assert_eq!(vllm_req.messages.len(), 2);
        assert_eq!(vllm_req.messages[0].role, "system");
        assert!(
            vllm_req.messages[0]
                .content
                .as_ref()
                .unwrap()
                .contains("valid JSON object")
        );

        Ok(())
    }

    #[test]
    fn test_translate_request_guided_json() -> Result<()> {
        let mut request = user_request("Score this.");
        request.system = Some("You are helpful.".to_string());
        request.response_format = Some(AiResponseFormat::Json { schema: None });

        let vllm_req = translate_vllm_request(request, None, None, true, false)?;

        assert_eq!(
            vllm_req.response_format,
            Some(json!({"type": "json_object"}))
        );
        // The prompt is left untouched when guided decoding enforces JSON.
        assert_eq!(
            vllm_req.messages[0].content,
            Some("You are helpful.".to_string())
        );

        Ok(())
    }

    #[test]
    fn test_translate_request_assistant_tool_call() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::Assistant,
                content: Some("I'll use a tool.".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_123".to_string(),
                    function_name: "test_tool".to_string(),
                    arguments: json!({"arg1": "val1"}),
                    thought_signature: None,
                }]),
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let vllm_req = translate_vllm_request(request, None, None, false, false)?;

        assert_eq!(vllm_req.messages.len(), 1);
        assert_eq!(vllm_req.messages[0].role, "assistant");
        let tool_calls = vllm_req.messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].function.name, "test_tool");
        assert_eq!(tool_calls[0].function.arguments, r#"{"arg1":"val1"}"#);
        assert_eq!(tool_calls[0].tool_type, "function");

        Ok(())
    }

    #[test]
    fn test_translate_request_empty_tools() -> Result<()> {
        let mut request = user_request("Test");
        request.tools = Some(vec![]);

        let vllm_req = translate_vllm_request(request, None, None, false, true)?;

        assert!(vllm_req.tools.is_none());

        Ok(())
    }

    fn request_with_tool() -> AiRequest {
        let mut request = user_request("Test");
        request.tools = Some(vec![crate::ai::AiTool {
            name: "my_tool".to_string(),
            description: "Does something.".to_string(),
            parameters: json!({"type": "object"}),
        }]);
        request
    }

    #[test]
    fn test_translate_request_drops_tools_when_disabled() -> Result<()> {
        let vllm_req = translate_vllm_request(request_with_tool(), None, None, false, false)?;

        assert!(vllm_req.tools.is_none());
        let json = serde_json::to_value(&vllm_req)?;
        assert!(json.get("tools").is_none());

        Ok(())
    }

    #[test]
    fn test_translate_request_forwards_tools_when_enabled() -> Result<()> {
        let vllm_req = translate_vllm_request(request_with_tool(), None, None, false, true)?;

        let tools = vllm_req.tools.as_ref().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_type, "function");
        assert_eq!(tools[0].function.name, "my_tool");
        assert_eq!(tools[0].function.parameters, json!({"type": "object"}));

        Ok(())
    }

    #[test]
    fn test_fit_messages_to_budget_noop_when_under_budget() {
        let mut messages = vec![
            assistant_message(Some("short system prompt")),
            assistant_message(Some("short user prompt")),
        ];
        let original = messages.clone();

        fit_messages_to_budget(&mut messages, 10_000);

        assert_eq!(messages[0].content, original[0].content);
        assert_eq!(messages[1].content, original[1].content);
    }

    #[test]
    fn test_fit_messages_to_budget_keeps_small_messages_intact() {
        let small = "Respond with a JSON object.";
        let large = "diff line\n".repeat(10_000);
        let mut messages = vec![
            assistant_message(Some(small)),
            assistant_message(Some(&large)),
        ];

        fit_messages_to_budget(&mut messages, 1000);

        // The small message must survive untouched; only the large one is cut.
        assert_eq!(messages[0].content.as_deref(), Some(small));
        let truncated = messages[1].content.as_ref().unwrap();
        assert!(truncated.len() < large.len());
        assert!(truncated.ends_with("[... truncated to fit the model context window ...]"));

        let total: usize = messages
            .iter()
            .map(|m| TokenBudget::estimate_tokens(m.content.as_deref().unwrap()))
            .sum();
        // Allow some slack for the truncation marker.
        assert!(total < 1100, "total {} tokens exceeds budget", total);
    }

    #[test]
    fn test_fit_messages_to_budget_handles_multibyte_content() {
        let large = "patch 🙂 review data ".repeat(5_000);
        let mut messages = vec![assistant_message(Some(&large))];

        // Must not panic on a UTF-8 boundary.
        fit_messages_to_budget(&mut messages, 100);

        assert!(messages[0].content.as_ref().unwrap().len() < large.len());
    }

    #[test]
    fn test_split_think_block_closed() {
        let (thought, content) = split_think_block(Some(
            "<think>Let me think.</think>{\"ok\": true}".to_string(),
        ));
        assert_eq!(thought, Some("Let me think.".to_string()));
        assert_eq!(content, Some("{\"ok\": true}".to_string()));
    }

    #[test]
    fn test_split_think_block_unclosed() {
        let (thought, content) = split_think_block(Some("<think>Still thinking...".to_string()));
        assert_eq!(thought, Some("Still thinking...".to_string()));
        assert_eq!(content, None);
    }

    #[test]
    fn test_split_think_block_absent() {
        let (thought, content) = split_think_block(Some("{\"ok\": true}".to_string()));
        assert_eq!(thought, None);
        assert_eq!(content, Some("{\"ok\": true}".to_string()));
    }

    #[test]
    fn test_translate_response_text() -> Result<()> {
        let resp = response_with_message(assistant_message(Some("Hello!")), "stop");

        let ai_resp = translate_vllm_response(resp)?;

        assert_eq!(ai_resp.content, Some("Hello!".to_string()));
        assert_eq!(ai_resp.thought, None);
        assert_eq!(ai_resp.tool_calls, None);
        assert!(!ai_resp.truncated);
        let usage = ai_resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(usage.total_tokens, 30);

        Ok(())
    }

    #[test]
    fn test_translate_response_strips_think_block() -> Result<()> {
        let resp = response_with_message(
            assistant_message(Some("<think>Hmm.</think>{\"score\": 1}")),
            "stop",
        );

        let ai_resp = translate_vllm_response(resp)?;

        assert_eq!(ai_resp.thought, Some("Hmm.".to_string()));
        assert_eq!(ai_resp.content, Some("{\"score\": 1}".to_string()));

        Ok(())
    }

    #[test]
    fn test_translate_response_reasoning_content_field() -> Result<()> {
        let mut message = assistant_message(Some("{\"score\": 1}"));
        message.reasoning_content = Some("Hmm.".to_string());
        let resp = response_with_message(message, "stop");

        let ai_resp = translate_vllm_response(resp)?;

        assert_eq!(ai_resp.thought, Some("Hmm.".to_string()));
        assert_eq!(ai_resp.content, Some("{\"score\": 1}".to_string()));

        Ok(())
    }

    #[test]
    fn test_translate_response_truncated() -> Result<()> {
        let resp = response_with_message(assistant_message(Some("<think>cut off")), "length");

        let ai_resp = translate_vllm_response(resp)?;

        assert!(ai_resp.truncated);
        assert_eq!(ai_resp.content, None);
        assert_eq!(ai_resp.thought, Some("cut off".to_string()));

        Ok(())
    }

    #[test]
    fn test_translate_response_tool_calls() -> Result<()> {
        let mut message = assistant_message(None);
        message.tool_calls = Some(vec![VllmToolCall {
            id: "call_abc".to_string(),
            tool_type: "function".to_string(),
            function: VllmToolCallFunction {
                name: "my_tool".to_string(),
                arguments: r#"{"arg":"val"}"#.to_string(),
            },
        }]);
        let resp = response_with_message(message, "tool_calls");

        let ai_resp = translate_vllm_response(resp)?;

        assert_eq!(ai_resp.content, None);
        let tool_calls = ai_resp.tool_calls.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_abc");
        assert_eq!(tool_calls[0].function_name, "my_tool");
        assert_eq!(tool_calls[0].arguments["arg"], "val");

        Ok(())
    }

    #[test]
    fn test_translate_response_empty_choices() {
        let resp = VllmResponse {
            choices: vec![],
            usage: VllmUsage {
                prompt_tokens: 10,
                completion_tokens: 0,
                total_tokens: 10,
            },
        };

        assert!(translate_vllm_response(resp).is_err());
    }

    #[test]
    fn test_deserialize_real_vllm_response() -> Result<()> {
        // Trimmed-down version of an actual vLLM 0.26 response, including
        // extra fields that must be ignored.
        let raw = r#"{
            "id": "chatcmpl-8aa73c1fe3ce8b12",
            "object": "chat.completion",
            "model": "OpenVINO/Qwen3-8B-int4-ov",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "<think>\nSome reasoning\n</think>\n\nAnswer",
                    "refusal": null,
                    "reasoning": null
                },
                "logprobs": null,
                "finish_reason": "stop",
                "stop_reason": null
            }],
            "usage": {
                "prompt_tokens": 39,
                "total_tokens": 239,
                "completion_tokens": 200,
                "prompt_tokens_details": null
            }
        }"#;

        let resp: VllmResponse = serde_json::from_str(raw)?;
        let ai_resp = translate_vllm_response(resp)?;

        assert_eq!(ai_resp.thought, Some("Some reasoning".to_string()));
        assert_eq!(ai_resp.content, Some("Answer".to_string()));
        assert_eq!(ai_resp.usage.unwrap().prompt_tokens, 39);

        Ok(())
    }

    #[test]
    fn test_error_classification_rate_limit() {
        let retry_after = Duration::from_secs(7);
        let err = VllmError::RateLimitExceeded(retry_after);
        assert_eq!(
            err.ai_error_class(),
            AiErrorClass::RateLimit { retry_after }
        );
    }

    #[test]
    fn test_error_classification_transient() {
        let retry_after = Duration::from_secs(11);
        let err = VllmError::TransientError(retry_after, "busy".to_string());
        assert_eq!(
            err.ai_error_class(),
            AiErrorClass::Transient { retry_after }
        );
    }

    #[test]
    fn test_error_classification_authentication() {
        let err = VllmError::AuthenticationError("bad key".to_string());
        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_error_classification_api_status() {
        let err = VllmError::ApiError(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "unavailable".to_string(),
        );
        assert_eq!(
            err.ai_error_class(),
            AiErrorClass::Transient {
                retry_after: DEFAULT_RETRY_AFTER,
            }
        );

        let err = VllmError::ApiError(reqwest::StatusCode::BAD_REQUEST, "bad".to_string());
        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_estimate_tokens_basic() {
        let mut request = user_request("Hello world");
        request.system = Some("System prompt".to_string());

        let tokens = estimate_tokens_generic(&request);
        assert!(tokens > 0);
        assert!(tokens < 100);
    }
}
