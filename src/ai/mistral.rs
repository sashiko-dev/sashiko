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

//! Mistral AI chat completions provider (`provider = "mistral"`).
//!
//! Mistral's API is close to the OpenAI one, but not close enough for the
//! generic compatible client: replies carry the assistant text as an array of
//! typed content blocks, where the chain of thought arrives as `thinking`
//! blocks next to `text` blocks instead of a `reasoning_content` string. This
//! provider speaks that shape natively, mapping `thinking` blocks to the
//! response `thought` and joining `text` blocks into the content. A chain of
//! thought kept in the conversation travels back on the next assistant turn
//! as a `thinking` block, so multi-round tool use stays stateful for
//! reasoning models.

use crate::ai::{
    AiErrorClass, AiProvider, AiRequest, AiResponse, AiResponseFormat, AiRole, AiUsage,
    ClassifyAiError, ProviderCapabilities, ToolCall, classify_status_code,
};
use crate::settings::MistralSettings;
use crate::utils::redact_secret;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

/// Completion token cap when `[ai.mistral] max_tokens` is unset. Reasoning
/// tokens are billed against it, so it leaves room for both the thinking and
/// the answer.
pub(crate) const DEFAULT_MAX_TOKENS: u32 = 32768;

#[derive(Debug, Serialize)]
pub struct MistralRequest {
    pub model: String,
    pub messages: Vec<MistralMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<MistralTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
}

/// One message of a request. Content is either a plain string — every role
/// accepts it, including an assistant turn that also names tool calls, and
/// the absent form fits an assistant turn that only calls tools — or typed
/// content blocks, which is how an assistant turn re-emits a chain of
/// thought the conversation kept.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MistralMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<MistralRequestContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<MistralToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl MistralMessage {
    /// The message's string content, creating an empty one when absent.
    /// Only the roles that always carry plain text are appended to, so a
    /// block-form message reports no text rather than being overwritten.
    fn text_or_default_mut(&mut self) -> Option<&mut String> {
        match self
            .content
            .get_or_insert_with(|| MistralRequestContent::Text(String::new()))
        {
            MistralRequestContent::Text(text) => Some(text),
            MistralRequestContent::Blocks(_) => None,
        }
    }
}

/// A request message's content: the plain string every role accepts, or the
/// typed blocks an assistant turn uses to carry a chain of thought.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(untagged)]
pub enum MistralRequestContent {
    Text(String),
    Blocks(Vec<MistralRequestBlock>),
}

impl From<String> for MistralRequestContent {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

/// A request-side content block. Only the two shapes the chat completions
/// schema validates for assistant messages are produced: `text` and
/// `thinking` (whose nested chunks carry the chain of thought).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct MistralRequestBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Vec<MistralRequestThinkingPart>>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct MistralRequestThinkingPart {
    #[serde(rename = "type")]
    part_type: String,
    text: String,
}

/// Builds an assistant turn's content. A kept chain of thought is re-emitted
/// as a `thinking` block ahead of the answer text, keeping reasoning models
/// stateful across tool rounds; a turn without one stays a plain string, and
/// a turn that only calls tools stays absent.
fn assistant_content(
    content: Option<String>,
    thought: Option<String>,
) -> Option<MistralRequestContent> {
    let thought = thought.filter(|t| !t.trim().is_empty());
    let text = content.filter(|c| !c.is_empty());
    match (thought, text) {
        (None, None) => None,
        (None, text) => text.map(MistralRequestContent::Text),
        (Some(thought), text) => {
            let mut blocks = vec![MistralRequestBlock {
                block_type: "thinking".to_string(),
                text: None,
                thinking: Some(vec![MistralRequestThinkingPart {
                    part_type: "text".to_string(),
                    text: thought,
                }]),
            }];
            if let Some(text) = text {
                blocks.push(MistralRequestBlock {
                    block_type: "text".to_string(),
                    text: Some(text),
                    thinking: None,
                });
            }
            Some(MistralRequestContent::Blocks(blocks))
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MistralToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: MistralToolCallFunction,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MistralToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MistralTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: MistralFunction,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MistralFunction {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Deserialize)]
pub struct MistralResponse {
    pub choices: Vec<MistralChoice>,
    #[serde(default)]
    pub usage: MistralUsage,
}

#[derive(Debug, Deserialize)]
pub struct MistralChoice {
    #[serde(default)]
    pub index: u32,
    pub message: MistralResponseMessage,
    pub finish_reason: String,
}

/// The assistant turn as the reply carries it. The content is either a plain
/// string (a reply produced without thinking), typed content blocks, or null
/// (a turn that only calls tools).
#[derive(Debug, Deserialize)]
pub struct MistralResponseMessage {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub content: Option<MistralContent>,
    #[serde(default)]
    pub tool_calls: Option<Vec<MistralToolCall>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum MistralContent {
    Text(String),
    Blocks(Vec<MistralBlock>),
    /// A shape this provider does not know. Kept so a new block kind costs a
    /// lost fragment rather than a lost completion.
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MistralBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    /// Text of a `text` block.
    #[serde(default)]
    pub text: Option<String>,
    /// Nested text chunks of a `thinking` block.
    #[serde(default)]
    pub thinking: Option<Vec<MistralThinkingPart>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MistralThinkingPart {
    #[serde(default)]
    pub text: Option<String>,
}

/// Every field defaults, and the object itself defaults on the response: the
/// counts are accounting, and losing them costs less than losing a
/// completion that arrived intact.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MistralUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    /// Absent when nothing was served from the prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<MistralPromptTokensDetails>,
    /// Reasoning tokens, for the log line only: they already count toward
    /// `completion_tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<MistralCompletionTokensDetails>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MistralPromptTokensDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MistralCompletionTokensDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum MistralError {
    #[error("Rate limit exceeded, retry after {0:?}")]
    RateLimitExceeded(Duration),
    #[error("Transient error: {1}, retry after {0:?}")]
    TransientError(Duration, String),
    #[error("Permanent transport error: {0}")]
    PermanentError(String),
    #[error("Authentication error: {0}")]
    AuthenticationError(String),
    #[error("API error {0}: {1}")]
    ApiError(reqwest::StatusCode, String),
}

impl ClassifyAiError for MistralError {
    fn ai_error_class(&self) -> AiErrorClass {
        match self {
            MistralError::RateLimitExceeded(retry_after) => AiErrorClass::RateLimit {
                retry_after: *retry_after,
            },
            MistralError::TransientError(retry_after, _) => AiErrorClass::Transient {
                retry_after: *retry_after,
            },
            // Retrying cannot fix a builder or redirect failure.
            MistralError::PermanentError(_) => AiErrorClass::Fatal,
            MistralError::AuthenticationError(_) => AiErrorClass::Fatal,
            MistralError::ApiError(status, _) => {
                classify_status_code(*status).unwrap_or(AiErrorClass::Fatal)
            }
        }
    }
}

pub struct MistralClient {
    model: String,
    base_url: String,
    context_window_size: usize,
    max_tokens: u32,
    /// Sent as `reasoning_effort` when a stage does not specify one.
    effort: Option<String>,
    client: Client,
}

impl MistralClient {
    pub const DEFAULT_BASE_URL: &'static str = "https://api.mistral.ai/v1/chat/completions";

    pub fn new(
        model: String,
        settings: Option<&MistralSettings>,
        api_timeout_secs: u64,
    ) -> Result<Self> {
        let api_key = Self::resolve_api_key(
            std::env::var("MISTRAL_API_KEY").ok(),
            std::env::var("LLM_API_KEY").ok(),
        );
        Self::new_with_key(model, settings, &api_key, api_timeout_secs)
    }

    /// A variable set to the empty string is no configuration: the shared
    /// key only wins when the dedicated one is missing or blank, so a
    /// stray blank MISTRAL_API_KEY cannot shadow a valid LLM_API_KEY.
    fn resolve_api_key(dedicated: Option<String>, shared: Option<String>) -> String {
        dedicated
            .filter(|key| !key.is_empty())
            .or_else(|| shared.filter(|key| !key.is_empty()))
            .unwrap_or_default()
    }

    pub(crate) fn new_with_key(
        model: String,
        settings: Option<&MistralSettings>,
        api_key: &str,
        api_timeout_secs: u64,
    ) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        if api_key.is_empty() {
            // A keyless gateway is a legitimate deployment, but an operator
            // hitting a remote 401 must not have to guess why no credentials
            // were sent.
            tracing::warn!(
                "MISTRAL_API_KEY is not set or is empty; requests to the Mistral endpoint will be unauthenticated"
            );
        } else {
            // A key with control characters (a stray newline from a
            // copy/paste accident, say) can never authenticate, so it fails
            // construction instead of degrading into unauthenticated posts.
            let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {api_key}"))
                .map_err(|_| {
                    anyhow::anyhow!(
                        "MISTRAL_API_KEY is set but contains characters that cannot appear in an HTTP header value"
                    )
                })?;
            headers.insert("Authorization", value);
        }

        let client = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(api_timeout_secs))
            .build()
            .unwrap_or_else(|_| Client::new());

        let base_url = settings
            .and_then(|s| s.base_url.clone())
            .unwrap_or_else(|| Self::DEFAULT_BASE_URL.to_string());

        Ok(Self {
            context_window_size: settings
                .and_then(|s| s.context_window_size)
                .unwrap_or_else(|| Self::default_context_window_for_model(&model)),
            max_tokens: settings
                .and_then(|s| s.max_tokens)
                .unwrap_or(DEFAULT_MAX_TOKENS),
            effort: settings.and_then(|s| s.effort.clone()),
            base_url: Self::normalize_base_url(&base_url)?,
            model,
            client,
        })
    }

    /// Accepts the API host, any `…/v1` root (the Mistral API or a gateway
    /// behind a path prefix), or the full chat completions URL; any other
    /// path is a mistake this provider refuses rather than posts to.
    fn normalize_base_url(url: &str) -> Result<String> {
        let trimmed = url.trim_end_matches('/');

        let (base, path) = match trimmed.split_once("://") {
            Some((scheme, rest)) => match rest.split_once('/') {
                Some((host, path)) => (format!("{scheme}://{host}"), format!("/{}", path)),
                None => (trimmed.to_string(), String::new()),
            },
            None => return Err(anyhow::anyhow!("Invalid url scheme in Mistral url {}", url)),
        };

        let path = if path.ends_with("/chat/completions") {
            path
        } else if path.is_empty() || path == "/v1" {
            "/v1/chat/completions".to_string()
        } else if path.ends_with("/v1") {
            format!("{path}/chat/completions")
        } else {
            return Err(anyhow::anyhow!("Invalid Mistral url {}", url));
        };

        Ok(format!("{base}{path}"))
    }

    /// Hosted zai-glm models take a 1M-token context; everything else
    /// defaults to the 128k Mistral tier. `[ai.mistral] context_window_size`
    /// overrides both.
    pub fn default_context_window_for_model(model: &str) -> usize {
        let lowered = model.to_ascii_lowercase();
        if lowered.starts_with("zai-glm-") || lowered.starts_with("glm-") {
            1_048_576
        } else {
            131_072
        }
    }

    fn translate_request(&self, request: AiRequest) -> MistralRequest {
        let mut messages = Vec::new();

        if let Some(system_text) = request.system {
            messages.push(MistralMessage {
                role: "system".to_string(),
                content: Some(system_text.into()),
                tool_calls: None,
                tool_call_id: None,
            });
        }

        for msg in request.messages {
            match msg.role {
                AiRole::System => {
                    messages.push(MistralMessage {
                        role: "system".to_string(),
                        content: msg.content.map(MistralRequestContent::Text),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
                AiRole::User => {
                    messages.push(MistralMessage {
                        role: "user".to_string(),
                        content: msg.content.map(MistralRequestContent::Text),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
                AiRole::Assistant => {
                    messages.push(MistralMessage {
                        role: "assistant".to_string(),
                        content: assistant_content(msg.content, msg.thought),
                        tool_calls: msg.tool_calls.map(|tc| {
                            tc.into_iter()
                                .map(|t| MistralToolCall {
                                    id: t.id,
                                    tool_type: "function".to_string(),
                                    function: MistralToolCallFunction {
                                        name: t.function_name,
                                        arguments: serde_json::to_string(&t.arguments)
                                            .unwrap_or_else(|_| "null".to_string()),
                                    },
                                })
                                .collect()
                        }),
                        tool_call_id: None,
                    });
                }
                AiRole::Tool => {
                    messages.push(MistralMessage {
                        role: "tool".to_string(),
                        content: msg.content.map(MistralRequestContent::Text),
                        tool_calls: None,
                        tool_call_id: msg.tool_call_id,
                    });
                }
            }
        }

        let tools = request.tools.and_then(|t| {
            if t.is_empty() {
                None
            } else {
                Some(
                    t.into_iter()
                        .map(|tool| MistralTool {
                            tool_type: "function".to_string(),
                            function: MistralFunction {
                                name: tool.name,
                                description: tool.description,
                                parameters: tool.parameters,
                            },
                        })
                        .collect(),
                )
            }
        });

        let response_format = match request.response_format {
            Some(AiResponseFormat::Json { .. }) => Some(serde_json::json!({"type": "json_object"})),
            Some(AiResponseFormat::Text) | None => None,
        };

        // A json_object gate may require the word json to appear in the
        // messages. The system prompt already carries the schema instruction,
        // so this only fires when a stage asks for JSON without one.
        if response_format
            .as_ref()
            .is_some_and(|rf| rf["type"] == "json_object")
        {
            if let Some(user_msg) = messages.iter_mut().find(|m| m.role == "user") {
                if let Some(content) = user_msg.text_or_default_mut()
                    && !content.to_lowercase().contains("json")
                {
                    content.push_str("\nRespond in JSON format.");
                }
            } else {
                messages.push(MistralMessage {
                    role: "user".to_string(),
                    content: Some("Respond in JSON format.".to_string().into()),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
        }

        // A stage's own effort wins; the configured level is the default. A
        // level the model rejects comes back as a 400 naming the accepted
        // set, which classifies fatal.
        let reasoning_effort = request
            .reasoning_effort
            .map(|e| effort_value(e).to_string())
            .or_else(|| self.configured_effort());

        MistralRequest {
            model: self.model.clone(),
            messages,
            tools,
            temperature: request.temperature,
            max_tokens: Some(self.max_tokens),
            reasoning_effort,
            response_format,
        }
    }

    fn configured_effort(&self) -> Option<String> {
        self.effort
            .as_ref()
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
    }

    async fn post_request(&self, body: &MistralRequest) -> Result<MistralResponse, MistralError> {
        let res = match self.client.post(&self.base_url).json(body).send().await {
            Ok(res) => res,
            Err(e) => {
                if crate::ai::is_permanent_transport_error(&e) {
                    // Fail fast like the sibling providers instead of
                    // climbing the backoff ladder for a failure no retry
                    // can fix. The error surfaces through the returned
                    // class, so it is not logged here.
                    return Err(MistralError::PermanentError(redact_secret(&format!(
                        "{:#}",
                        anyhow::Error::from(e)
                    ))));
                }
                let err_str = redact_secret(&format!("{:#}", anyhow::Error::from(e)));
                tracing::error!(
                    "{}Mistral request failed (transport): {}",
                    crate::ai::get_log_prefix(),
                    err_str
                );
                return Err(MistralError::TransientError(
                    Duration::from_secs(30),
                    err_str,
                ));
            }
        };

        if res.status().is_success() {
            let status = res.status();
            let body_text = res.text().await.map_err(|e| {
                let err_str = redact_secret(&format!("{:#}", anyhow::Error::from(e)));
                tracing::error!(
                    "{}Failed to read Mistral response body: {}",
                    crate::ai::get_log_prefix(),
                    err_str
                );
                MistralError::TransientError(Duration::from_secs(30), err_str)
            })?;
            return match serde_json::from_str::<MistralResponse>(&body_text) {
                Ok(response) => {
                    let reasoning = response
                        .usage
                        .completion_tokens_details
                        .as_ref()
                        .and_then(|d| d.reasoning_tokens)
                        .unwrap_or(0);
                    tracing::info!(
                        "{}Mistral response received. Tokens: in={}, out={} (reasoning={})",
                        crate::ai::get_log_prefix(),
                        response.usage.prompt_tokens,
                        response.usage.completion_tokens,
                        reasoning
                    );
                    Ok(response)
                }
                Err(e) => {
                    tracing::error!(
                        "{}Failed to decode Mistral response: {}",
                        crate::ai::get_log_prefix(),
                        e
                    );
                    Err(MistralError::ApiError(
                        status,
                        format!("Parse error: {}", e),
                    ))
                }
            };
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
                let retry_duration = retry_after_duration.unwrap_or(crate::ai::DEFAULT_RETRY_AFTER);
                tracing::warn!(
                    "{}Mistral 429 Rate Limit. Retry in {}s",
                    crate::ai::get_log_prefix(),
                    retry_duration.as_secs_f64()
                );
                Err(MistralError::RateLimitExceeded(retry_duration))
            }
            401 | 403 => Err(MistralError::AuthenticationError(error_text)),
            500..=599 => {
                tracing::warn!(
                    "{}Mistral Server Error {}: {}",
                    crate::ai::get_log_prefix(),
                    status,
                    error_text
                );
                Err(MistralError::TransientError(
                    retry_after_duration.unwrap_or(Duration::from_secs(0)),
                    error_text,
                ))
            }
            _ => Err(MistralError::ApiError(status, error_text)),
        }
    }
}

/// Splits a reply's content into the answer text and the chain of thought.
/// `text` blocks concatenate in order; each `thinking` block's nested chunks
/// concatenate into the thought. Content the provider does not recognize is
/// dropped rather than failing the reply.
fn split_content(content: Option<MistralContent>) -> (Option<String>, Option<String>) {
    let blocks = match content {
        None | Some(MistralContent::Other) => return (None, None),
        Some(MistralContent::Text(text)) => return (non_empty(text), None),
        Some(MistralContent::Blocks(blocks)) => blocks,
    };

    let mut text = String::new();
    let mut thought = String::new();
    for block in blocks {
        match block.block_type.as_str() {
            "text" => {
                if let Some(part) = block.text {
                    text.push_str(&part);
                }
            }
            "thinking" => {
                if let Some(parts) = block.thinking {
                    for part in parts {
                        if let Some(text) = part.text {
                            thought.push_str(&text);
                        }
                    }
                } else if let Some(part) = block.text {
                    thought.push_str(&part);
                }
            }
            _ => {}
        }
    }
    (non_empty(text), non_empty(thought))
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() { None } else { Some(value) }
}

/// The lowercase `reasoning_effort` level a stage's effort maps to. The API
/// also accepts "none", "minimal", "xhigh" and "max", which the stage
/// machinery never asks for.
fn effort_value(effort: crate::ai::ReasoningEffort) -> &'static str {
    match effort {
        crate::ai::ReasoningEffort::Low => "low",
        crate::ai::ReasoningEffort::Medium => "medium",
        crate::ai::ReasoningEffort::High => "high",
    }
}

fn translate_ai_response(resp: MistralResponse) -> Result<AiResponse> {
    let choice = resp
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No choices in response"))?;

    let (content, thought) = split_content(choice.message.content);
    let tool_calls = choice.message.tool_calls.map(|tc| {
        tc.into_iter()
            .map(|t| {
                let arguments: Value =
                    serde_json::from_str(&t.function.arguments).unwrap_or(Value::Null);
                ToolCall {
                    id: t.id,
                    function_name: t.function.name,
                    arguments,
                    thought_signature: None,
                }
            })
            .collect()
    });

    let truncated = choice.finish_reason == "length";

    if truncated {
        tracing::warn!(
            "{}Mistral response truncated due to finish_reason = length.",
            crate::ai::get_log_prefix()
        );
    }

    // prompt_tokens already counts the cached prefix, so a cached_tokens
    // breakdown above prompt_tokens is dropped rather than trusted: a
    // consumer subtracting it for uncached input still gets zero.
    let cached = resp
        .usage
        .prompt_tokens_details
        .and_then(|d| d.cached_tokens)
        .filter(|&c| c <= resp.usage.prompt_tokens)
        .unwrap_or(0);

    let usage = Some(AiUsage {
        prompt_tokens: resp.usage.prompt_tokens as usize,
        completion_tokens: resp.usage.completion_tokens as usize,
        total_tokens: resp.usage.total_tokens as usize,
        cached_tokens: (cached > 0).then_some(cached as usize),
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

#[async_trait]
impl AiProvider for MistralClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        tracing::info!("{}Sending Mistral request...", crate::ai::get_log_prefix());

        let body = self.translate_request(request);
        let resp = self.post_request(&body).await?;
        translate_ai_response(resp)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: self.context_window_size,
        }
    }

    fn cache_identity(&self) -> String {
        // The endpoint and the output cap shape the reply but travel outside
        // the request, so the bare model name cannot distinguish them. A call
        // that hit the default max_tokens cap comes back empty with
        // finish_reason "length"; raising max_tokens has to miss that entry
        // rather than replay it. The configured effort level shapes the reply
        // the same way, so it joins the identity when set.
        let max_tokens = self.max_tokens.to_string();
        crate::ai::cache_identity_with(
            &self.model,
            &[
                ("max_tokens", Some(max_tokens.as_str())),
                ("base_url", Some(self.base_url.as_str())),
                ("effort", self.effort.as_deref()),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiMessage, AiTool, ReasoningEffort};
    use serde_json::json;

    fn settings(base_url: Option<&str>, effort: Option<&str>) -> MistralSettings {
        MistralSettings {
            base_url: base_url.map(str::to_string),
            effort: effort.map(str::to_string),
            ..Default::default()
        }
    }

    fn client(base_url: &str) -> MistralClient {
        MistralClient::new_with_key(
            "zai-glm-5-3-flash".to_string(),
            Some(&settings(Some(base_url), None)),
            "test-key",
            30,
        )
        .unwrap()
    }

    fn empty_request() -> AiRequest {
        AiRequest {
            system: None,
            messages: vec![],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
        }
    }

    fn message(role: AiRole, content: &str) -> AiMessage {
        AiMessage {
            role,
            content: Some(content.to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    #[test]
    fn base_url_forms_land_on_chat_completions() {
        for form in [
            "https://api.mistral.ai",
            "https://api.mistral.ai/",
            "https://api.mistral.ai/v1",
            "https://api.mistral.ai/v1/",
            "https://api.mistral.ai/v1/chat/completions",
        ] {
            let c = MistralClient::new_with_key(
                "m".to_string(),
                Some(&settings(Some(form), None)),
                "k",
                30,
            )
            .unwrap();
            assert_eq!(c.base_url, "https://api.mistral.ai/v1/chat/completions");
        }

        // A gateway behind another path prefix keeps that prefix.
        let gateway = MistralClient::new_with_key(
            "m".to_string(),
            Some(&settings(Some("https://gw.example/api/v1"), None)),
            "k",
            30,
        )
        .unwrap();
        assert_eq!(
            gateway.base_url,
            "https://gw.example/api/v1/chat/completions"
        );

        // Anything else is refused rather than posted to.
        for bad in ["https://api.mistral.ai/v1/other", "api.mistral.ai"] {
            assert!(
                MistralClient::new_with_key(
                    "m".to_string(),
                    Some(&settings(Some(bad), None)),
                    "k",
                    30
                )
                .is_err()
            );
        }
    }

    #[test]
    fn context_window_defaults_by_model_and_stays_overridable() {
        assert_eq!(
            MistralClient::default_context_window_for_model("zai-glm-5-3-flash"),
            1_048_576
        );
        assert_eq!(
            MistralClient::default_context_window_for_model("GLM-5"),
            1_048_576
        );
        assert_eq!(
            MistralClient::default_context_window_for_model("mistral-large-latest"),
            131_072
        );

        let c = MistralClient::new_with_key(
            "m".to_string(),
            Some(&MistralSettings {
                context_window_size: Some(4096),
                ..Default::default()
            }),
            "k",
            30,
        )
        .unwrap();
        assert_eq!(c.get_capabilities().context_window_size, 4096);
        assert_eq!(c.get_capabilities().model_name, "m");
    }

    #[test]
    fn request_translation_maps_roles_tools_and_effort() {
        let c = client("https://api.mistral.ai");
        let request = AiRequest {
            system: Some("be brief".to_string()),
            messages: vec![
                message(AiRole::User, "hi"),
                AiMessage {
                    role: AiRole::Assistant,
                    content: None,
                    thought: None,
                    thought_signature: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".to_string(),
                        function_name: "lookup".to_string(),
                        arguments: json!({"q": "x"}),
                        thought_signature: None,
                    }]),
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::Tool,
                    content: Some("result".to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: Some("call_1".to_string()),
                },
            ],
            tools: Some(vec![AiTool {
                name: "lookup".to_string(),
                description: "finds things".to_string(),
                parameters: json!({"type": "object"}),
            }]),
            temperature: Some(1.0),
            response_format: Some(AiResponseFormat::Text),
            context_tag: None,
            reasoning_effort: Some(ReasoningEffort::Low),
        };

        let body = c.translate_request(request);
        assert_eq!(body.model, "zai-glm-5-3-flash");
        assert_eq!(body.max_tokens, Some(32768));
        assert_eq!(body.temperature, Some(1.0));
        assert_eq!(body.reasoning_effort.as_deref(), Some("low"));

        assert_eq!(body.messages.len(), 4);
        assert_eq!(body.messages[0].role, "system");
        assert_eq!(body.messages[1].role, "user");

        // An assistant tool call travels as a typed call with the arguments
        // serialized back to the JSON string the API expects.
        let calls = body.messages[2].tool_calls.as_ref().unwrap();
        assert_eq!(calls[0].tool_type, "function");
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "lookup");
        assert_eq!(calls[0].function.arguments, r#"{"q":"x"}"#);

        // The tool result names the call it answers.
        assert_eq!(body.messages[3].tool_call_id.as_deref(), Some("call_1"));

        let tools = body.tools.as_ref().unwrap();
        assert_eq!(tools[0].tool_type, "function");
        assert_eq!(tools[0].function.name, "lookup");
        assert_eq!(tools[0].function.parameters, json!({"type": "object"}));

        // Text mode serializes without a response_format field at all, which
        // is what endpoints that do not know the parameter need.
        let text = serde_json::to_string(&body).unwrap();
        assert!(!text.contains("response_format"));
    }

    #[test]
    fn json_mode_appends_instruction_when_the_prompt_omits_the_word() {
        let c = client("https://api.mistral.ai");
        let request = AiRequest {
            messages: vec![message(AiRole::User, "list them")],
            response_format: Some(AiResponseFormat::Json { schema: None }),
            ..empty_request()
        };

        let body = c.translate_request(request);
        assert_eq!(
            body.response_format.as_ref().unwrap()["type"],
            "json_object"
        );
        assert_eq!(
            body.messages[0].content,
            Some(MistralRequestContent::Text(
                "list them\nRespond in JSON format.".to_string()
            ))
        );
    }

    #[test]
    fn json_mode_leaves_a_prompt_that_already_names_json_alone() {
        let c = client("https://api.mistral.ai");
        let request = AiRequest {
            messages: vec![message(AiRole::User, "produce JSON please")],
            response_format: Some(AiResponseFormat::Json { schema: None }),
            ..empty_request()
        };

        let body = c.translate_request(request);
        assert_eq!(
            body.messages[0].content,
            Some(MistralRequestContent::Text(
                "produce JSON please".to_string()
            ))
        );
    }

    #[test]
    fn assistant_thought_travels_back_as_a_thinking_block() {
        let c = client("https://api.mistral.ai");
        let body = c.translate_request(AiRequest {
            messages: vec![
                message(AiRole::User, "hi"),
                AiMessage {
                    role: AiRole::Assistant,
                    content: Some("mid answer".to_string()),
                    thought: Some("hmm".to_string()),
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                message(AiRole::User, "go on"),
            ],
            ..empty_request()
        });

        // The chain of thought the session kept is re-sent as a typed
        // thinking block ahead of the answer text, keeping a reasoning model
        // stateful across tool rounds.
        let wire = serde_json::to_value(&body).unwrap();
        assert_eq!(
            wire["messages"][1]["content"],
            json!([
                {"type": "thinking", "thinking": [{"type": "text", "text": "hmm"}]},
                {"type": "text", "text": "mid answer"}
            ])
        );

        // Without a thought the plain string form is used, and a blank
        // thought is dropped rather than sent as an empty block.
        let messages = c
            .translate_request(AiRequest {
                messages: vec![AiMessage {
                    role: AiRole::Assistant,
                    content: Some("answer".to_string()),
                    thought: Some("   ".to_string()),
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ..empty_request()
            })
            .messages;
        let wire = serde_json::to_value(&messages).unwrap();
        assert_eq!(wire[0]["content"], json!("answer"));

        // The production tool round keeps the thought AND the tool calls on
        // the same assistant turn: block-form content travels next to the
        // calls array, which is the shape verified against the live chat
        // completions endpoint.
        let messages = c
            .translate_request(AiRequest {
                messages: vec![AiMessage {
                    role: AiRole::Assistant,
                    content: Some("checking".to_string()),
                    thought: Some("hmm".to_string()),
                    thought_signature: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".to_string(),
                        function_name: "lookup".to_string(),
                        arguments: json!({"q": "x"}),
                        thought_signature: None,
                    }]),
                    tool_call_id: None,
                }],
                ..empty_request()
            })
            .messages;
        assert_eq!(messages[0].tool_calls.as_ref().unwrap().len(), 1);
        let wire = serde_json::to_value(&messages).unwrap();
        assert_eq!(
            wire[0]["content"],
            json!([
                {"type": "thinking", "thinking": [{"type": "text", "text": "hmm"}]},
                {"type": "text", "text": "checking"}
            ])
        );
    }

    #[test]
    fn missing_api_key_builds_but_malformed_one_is_refused() {
        // A keyless gateway is a legitimate deployment; it still builds.
        let c = MistralClient::new_with_key("m".to_string(), None, "", 30).unwrap();
        assert_eq!(c.get_capabilities().model_name, "m");

        // A key that can never form a valid header value fails construction
        // instead of degrading into unauthenticated requests.
        let err = MistralClient::new_with_key("m".to_string(), None, "bad\nkey", 30)
            .err()
            .expect("a malformed key must fail construction");
        assert!(
            err.to_string()
                .contains("characters that cannot appear in an HTTP header"),
            "{err}"
        );
    }

    #[test]
    fn configured_effort_fills_in_when_a_stage_does_not_ask() {
        let c = MistralClient::new_with_key(
            "m".to_string(),
            Some(&settings(None, Some("medium"))),
            "k",
            30,
        )
        .unwrap();

        let body = c.translate_request(empty_request());
        assert_eq!(body.reasoning_effort.as_deref(), Some("medium"));

        // A stage's own level still wins over the configured one.
        let body = c.translate_request(AiRequest {
            reasoning_effort: Some(ReasoningEffort::High),
            ..empty_request()
        });
        assert_eq!(body.reasoning_effort.as_deref(), Some("high"));
    }

    fn response(body: serde_json::Value) -> AiResponse {
        translate_ai_response(serde_json::from_value(body).unwrap()).unwrap()
    }

    fn choices_message(message: serde_json::Value, finish_reason: &str) -> serde_json::Value {
        json!({
            "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        })
    }

    #[test]
    fn string_content_parses_with_usage() {
        let r = response(choices_message(
            json!({"role": "assistant", "content": "hello"}),
            "stop",
        ));
        assert_eq!(r.content.as_deref(), Some("hello"));
        assert_eq!(r.thought, None);
        assert!(!r.truncated);
        assert_eq!(r.tool_calls, None);

        let usage = r.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
        assert_eq!(usage.cached_tokens, None);
    }

    #[test]
    fn thinking_blocks_split_into_content_and_thought() {
        let r = response(choices_message(
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": [
                    {"type": "text", "text": "step 1 "},
                    {"type": "text", "text": "step 2"},
                ]},
                {"type": "text", "text": "The answer."},
            ]}),
            "stop",
        ));
        assert_eq!(r.thought.as_deref(), Some("step 1 step 2"));
        assert_eq!(r.content.as_deref(), Some("The answer."));
    }

    #[test]
    fn unknown_blocks_and_null_content_degrade_to_nothing() {
        let r = response(choices_message(
            json!({"role": "assistant", "content": [
                {"type": "image_url", "image_url": {"url": "https://x"}},
            ]}),
            "stop",
        ));
        assert_eq!(r.content, None);
        assert_eq!(r.thought, None);

        let r = response(choices_message(
            json!({"role": "assistant", "content": null}),
            "stop",
        ));
        assert_eq!(r.content, None);
        assert_eq!(r.thought, None);
    }

    #[test]
    fn tool_calls_parse_arguments_from_the_wire_string() {
        let r = response(choices_message(
            json!({"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_9", "type": "function",
                 "function": {"name": "lookup", "arguments": "{\"q\":\"x\"}"}},
            ]}),
            "tool_calls",
        ));
        let calls = r.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_9");
        assert_eq!(calls[0].function_name, "lookup");
        assert_eq!(calls[0].arguments, json!({"q": "x"}));
        assert_eq!(calls[0].thought_signature, None);
    }

    #[test]
    fn finish_reason_length_marks_truncation() {
        let r = response(choices_message(
            json!({"role": "assistant", "content": "half a"}),
            "length",
        ));
        assert!(r.truncated);
    }

    #[test]
    fn implausible_cached_tokens_are_dropped() {
        let over = json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 5, "total_tokens": 105,
                      "prompt_tokens_details": {"cached_tokens": 400}}
        });
        assert_eq!(response(over).usage.unwrap().cached_tokens, None);

        let plausible = json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 5, "total_tokens": 105,
                      "prompt_tokens_details": {"cached_tokens": 40}}
        });
        assert_eq!(response(plausible).usage.unwrap().cached_tokens, Some(40));
    }

    #[test]
    fn error_classification_covers_all_variants() {
        use crate::ai::ClassifyAiError as _;

        assert!(matches!(
            MistralError::RateLimitExceeded(Duration::from_secs(5)).ai_error_class(),
            AiErrorClass::RateLimit { retry_after } if retry_after == Duration::from_secs(5)
        ));
        assert!(matches!(
            MistralError::AuthenticationError("no".to_string()).ai_error_class(),
            AiErrorClass::Fatal
        ));
        assert!(matches!(
            MistralError::TransientError(Duration::from_secs(9), "e".to_string()).ai_error_class(),
            AiErrorClass::Transient { retry_after } if retry_after == Duration::from_secs(9)
        ));
        assert!(matches!(
            MistralError::PermanentError("e".to_string()).ai_error_class(),
            AiErrorClass::Fatal
        ));
        assert!(matches!(
            MistralError::ApiError(reqwest::StatusCode::BAD_REQUEST, "e".to_string())
                .ai_error_class(),
            AiErrorClass::Fatal
        ));
        assert!(matches!(
            MistralError::ApiError(reqwest::StatusCode::BAD_GATEWAY, "e".to_string())
                .ai_error_class(),
            AiErrorClass::Transient { .. }
        ));
    }

    #[test]
    fn cache_identity_distinguishes_request_shaping_knobs() {
        let bare = client("https://api.mistral.ai").cache_identity();
        assert_eq!(
            bare,
            "zai-glm-5-3-flash|max_tokens=32768|base_url=https://api.mistral.ai/v1/chat/completions"
        );

        // The configured effort rides along when set.
        let with_effort = MistralClient::new_with_key(
            "zai-glm-5-3-flash".to_string(),
            Some(&settings(Some("https://api.mistral.ai"), Some("low"))),
            "k",
            30,
        )
        .unwrap()
        .cache_identity();
        assert_ne!(bare, with_effort);
        assert!(with_effort.contains("|effort=low"));

        // A different max_tokens must not replay a capped-empty reply.
        let with_cap = MistralClient::new_with_key(
            "zai-glm-5-3-flash".to_string(),
            Some(&MistralSettings {
                base_url: Some("https://api.mistral.ai".to_string()),
                max_tokens: Some(1024),
                ..Default::default()
            }),
            "k",
            30,
        )
        .unwrap()
        .cache_identity();
        assert_ne!(bare, with_cap);
        assert!(with_cap.contains("|max_tokens=1024"));
    }

    /// A stub chat completions server that records every request and answers
    /// with a thinking-blocks reply, so the full round trip is exercised.
    #[cfg(feature = "server")]
    async fn capturing_server() -> anyhow::Result<(
        String,
        std::sync::Arc<tokio::sync::Mutex<Vec<(axum::http::HeaderMap, Value)>>>,
        tokio::task::JoinHandle<()>,
    )> {
        use axum::{Json, Router, http::HeaderMap, routing::post};

        let captured: std::sync::Arc<tokio::sync::Mutex<Vec<(HeaderMap, Value)>>> =
            std::sync::Arc::default();
        let seen = std::sync::Arc::clone(&captured);
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let seen = std::sync::Arc::clone(&seen);
                async move {
                    seen.lock().await.push((headers, body));
                    Json(json!({
                        "choices": [{"index": 0, "message": {"role": "assistant", "content": [
                            {"type": "thinking", "thinking": [{"type": "text", "text": "hmm"}]},
                            {"type": "text", "text": "answer"},
                        ]}, "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok((base_url, captured, server))
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn round_trip_posts_the_mistral_shape_and_parses_thinking_blocks() -> anyhow::Result<()> {
        let (base_url, captured, _server) = capturing_server().await?;

        let client = MistralClient::new_with_key(
            "zai-glm-5-3-flash".to_string(),
            Some(&settings(Some(&base_url), None)),
            "sekret",
            30,
        )?;
        let response = client.generate_content(empty_request()).await?;

        let (headers, body) = captured
            .lock()
            .await
            .first()
            .expect("the request reached the stub")
            .clone();
        assert_eq!(headers["authorization"], "Bearer sekret");
        assert_eq!(body["model"], "zai-glm-5-3-flash");
        assert_eq!(body["max_tokens"], 32768);
        assert!(body.get("reasoning_effort").is_none());

        assert_eq!(response.content.as_deref(), Some("answer"));
        assert_eq!(response.thought.as_deref(), Some("hmm"));
        let usage = response.usage.unwrap();
        assert_eq!((usage.prompt_tokens, usage.total_tokens), (3, 5));

        // A kept chain of thought re-emits on the next assistant turn as
        // content blocks shaped exactly like the blocks the API itself
        // emits in replies, so the wire form stays symmetric end to end.
        let follow_up = AiRequest {
            messages: vec![
                AiMessage {
                    role: AiRole::Assistant,
                    content: response.content.clone(),
                    thought: response.thought.clone(),
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::User,
                    content: Some("go on".to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            ..empty_request()
        };
        client.generate_content(follow_up).await?;

        let (_, second) = captured
            .lock()
            .await
            .last()
            .expect("the follow-up reached the stub")
            .clone();
        assert_eq!(
            second["messages"][0]["content"],
            json!([
                {"type": "thinking", "thinking": [{"type": "text", "text": "hmm"}]},
                {"type": "text", "text": "answer"}
            ])
        );
        Ok(())
    }

    /// A stub chat completions server answering every POST with a fixed
    /// status, optional Retry-After seconds, and body.
    #[cfg(feature = "server")]
    async fn status_server(
        status: u16,
        retry_after: Option<u64>,
        body: &'static str,
    ) -> anyhow::Result<String> {
        use axum::{Router, response::IntoResponse, routing::post};

        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || async move {
                let mut res = (
                    axum::http::StatusCode::from_u16(status).expect("test statuses are valid"),
                    body,
                )
                    .into_response();
                if let Some(secs) = retry_after {
                    res.headers_mut()
                        .insert("retry-after", axum::http::HeaderValue::from(secs));
                }
                res
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(base_url)
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn post_request_maps_http_failures_to_error_variants() -> anyhow::Result<()> {
        async fn probe(status: u16, retry_after: Option<u64>, body: &'static str) -> MistralError {
            let base_url = status_server(status, retry_after, body).await.unwrap();
            let client = MistralClient::new_with_key(
                "m".to_string(),
                Some(&settings(Some(&base_url), None)),
                "k",
                30,
            )
            .unwrap();
            client
                .post_request(&client.translate_request(empty_request()))
                .await
                .unwrap_err()
        }

        // A 429 honors the Retry-After header...
        let err = probe(429, Some(7), "slow down").await;
        assert!(
            matches!(err, MistralError::RateLimitExceeded(d) if d == Duration::from_secs(7)),
            "{err}"
        );

        // ...and falls back to the default when the header is absent.
        let err = probe(429, None, "slow down").await;
        assert!(
            matches!(err, MistralError::RateLimitExceeded(d) if d == crate::ai::DEFAULT_RETRY_AFTER),
            "{err}"
        );

        let err = probe(401, None, "{\"message\":\"bad key\"}").await;
        assert!(
            matches!(err, MistralError::AuthenticationError(ref t) if t.contains("bad key")),
            "{err}"
        );

        // A 5xx is transient with the body as the reason, delayed by a
        // Retry-After when the server sends one.
        let err = probe(500, None, "boom").await;
        assert!(
            matches!(err, MistralError::TransientError(d, ref t) if d.is_zero() && t == "boom"),
            "{err}"
        );
        let err = probe(500, Some(2), "boom").await;
        assert!(
            matches!(err, MistralError::TransientError(d, _) if d == Duration::from_secs(2)),
            "{err}"
        );

        // A 2xx whose body is not a chat completion is an API error, not a
        // silent success.
        let err = probe(200, None, "<html>not json</html>").await;
        assert!(
            matches!(err, MistralError::ApiError(_, ref t) if t.starts_with("Parse error")),
            "{err}"
        );

        Ok(())
    }

    /// An environment variable set to the empty string is no configuration:
    /// the shared key must win over a blank MISTRAL_API_KEY.
    #[test]
    fn empty_mistral_key_falls_through_to_the_shared_key() {
        let resolve = |dedicated: Option<&str>, shared: Option<&str>| {
            MistralClient::resolve_api_key(
                dedicated.map(str::to_string),
                shared.map(str::to_string),
            )
        };
        assert_eq!(resolve(Some("ded"), Some("shared")), "ded");
        assert_eq!(resolve(Some(""), Some("shared")), "shared");
        assert_eq!(resolve(None, Some("shared")), "shared");
        assert_eq!(resolve(Some(""), None), "");
        assert_eq!(resolve(None, None), "");
        assert_eq!(resolve(Some(" "), Some("shared")), " ");
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn redirect_loop_fails_fast_as_permanent() -> anyhow::Result<()> {
        // A self-redirecting stub exhausts reqwest's redirect budget, which
        // surfaces as a redirect transport error: the permanent arm must
        // return it without logging or climbing the backoff ladder.
        async fn bounce() -> axum::response::Redirect {
            axum::response::Redirect::temporary("/v1/chat/completions")
        }
        let app = axum::Router::new()
            .route("/", axum::routing::any(bounce))
            .route("/v1/chat/completions", axum::routing::any(bounce));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}", listener.local_addr()?);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = MistralClient::new_with_key(
            "m".to_string(),
            Some(&settings(Some(&base_url), None)),
            "k",
            30,
        )?;
        let err = client
            .post_request(&client.translate_request(empty_request()))
            .await
            .unwrap_err();
        assert!(matches!(err, MistralError::PermanentError(_)), "{err}");
        Ok(())
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn transport_failure_maps_to_a_transient_error() -> anyhow::Result<()> {
        // Port 1 on the loopback interface has no listener and cannot be
        // bound by an unprivileged test, so the connection is refused
        // deterministically — unlike a bound-then-dropped ephemeral port,
        // which a concurrent bind can claim back mid-test.
        let client = MistralClient::new_with_key(
            "m".to_string(),
            Some(&settings(Some("http://127.0.0.1:1"), None)),
            "k",
            30,
        )?;
        let err = client
            .post_request(&client.translate_request(empty_request()))
            .await
            .unwrap_err();
        assert!(matches!(err, MistralError::TransientError(..)), "{err}");
        Ok(())
    }
}
