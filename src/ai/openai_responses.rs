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

use crate::ai::{
    AiErrorClass, AiProvider, AiProviderMetadata, AiRequest, AiResponse, AiResponseFormat, AiRole,
    AiUsage, ClassifyAiError, ProviderCapabilities, ToolCall, classify_status_code,
};
use crate::utils::redact_secret;
use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::time::Duration;

// ── Request types ──────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub input: Vec<ResponsesInputItem>,
    /// Ask the API to return opaque encrypted reasoning state so it can be
    /// replayed on later manually-managed turns, including with ZDR.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponsesTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextConfig>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum ResponsesInputItem {
    Message {
        role: String,
        content: String,
    },
    FunctionCall {
        #[serde(rename = "type")]
        item_type: String, // "function_call"
        id: String,
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        #[serde(rename = "type")]
        item_type: String, // "function_call_output"
        call_id: String,
        output: String,
    },
    /// An opaque output item from a previous Responses API turn. The API
    /// requires these items to be replayed unchanged when state is managed
    /// client-side, including reasoning and function-call item metadata.
    Raw(Value),
}

#[derive(Debug, Serialize)]
pub struct ResponsesTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Serialize)]
pub struct ReasoningConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TextConfig {
    pub format: TextFormatConfig,
}

#[derive(Debug, Serialize)]
pub struct TextFormatConfig {
    #[serde(rename = "type")]
    pub format_type: String,
}

// ── Response types ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ResponsesResponse {
    #[allow(dead_code)]
    pub id: String,
    pub status: String,
    /// Preserve the API's output items verbatim. Their shape evolves as new
    /// built-in tools and reasoning features are added, and callers managing
    /// state manually must replay every item without reconstruction.
    pub output: Vec<Value>,
    #[serde(default)]
    pub usage: ResponsesUsage,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResponsesUsage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    /// Responses APIs report cached input as a breakdown of input_tokens.
    /// Compatible endpoints sometimes omit or change this object, so a bad
    /// accounting shape must not discard an otherwise valid response.
    #[serde(default, deserialize_with = "lenient_input_tokens_details")]
    pub input_tokens_details: Option<ResponsesInputTokensDetails>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResponsesInputTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

fn lenient_input_tokens_details<'de, D>(
    deserializer: D,
) -> Result<Option<ResponsesInputTokensDetails>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

// ── Errors ──────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum OpenAiError {
    #[error("Rate limit exceeded, retry after {0:?}")]
    RateLimitExceeded(Duration),
    #[error("Transient error: {1}, retry after {0:?}")]
    TransientError(Duration, String),
    #[error("Authentication error: {0}")]
    AuthenticationError(String),
    #[error("API error {0}: {1}")]
    ApiError(reqwest::StatusCode, String),
}

impl ClassifyAiError for OpenAiError {
    fn ai_error_class(&self) -> AiErrorClass {
        match self {
            OpenAiError::RateLimitExceeded(retry_after) => AiErrorClass::RateLimit {
                retry_after: *retry_after,
            },
            OpenAiError::TransientError(retry_after, _) => AiErrorClass::Transient {
                retry_after: *retry_after,
            },
            OpenAiError::AuthenticationError(_) => AiErrorClass::Fatal,
            OpenAiError::ApiError(status, _) => {
                classify_status_code(*status).unwrap_or(AiErrorClass::Fatal)
            }
        }
    }
}

// ── Client ──────────────────────────────────────────────────────

pub struct OpenAiClient {
    model: String,
    base_url: String,
    context_window_size: usize,
    max_tokens: u32,
    reasoning_effort: Option<String>,
    client: Client,
}

impl OpenAiClient {
    pub fn new(
        base_url: String,
        model: String,
        context_window_size: usize,
        max_tokens: u32,
        reasoning_effort: Option<String>,
        api_timeout_secs: u64,
    ) -> Result<Self> {
        let api_key = std::env::var("OPENAI_API_KEY")
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

        let base_url = base_url.trim_end_matches('/').to_string();

        Ok(Self {
            model,
            base_url,
            context_window_size,
            max_tokens,
            reasoning_effort,
            client,
        })
    }

    pub fn default_base_url() -> String {
        "https://api.openai.com/v1/responses".to_string()
    }

    pub fn default_context_window_for_model(model: &str) -> usize {
        if model.starts_with("gpt-5.6") {
            1_050_000
        } else if model.starts_with("gpt-4o") || model.starts_with("gpt-4-turbo") {
            128_000
        } else if model.starts_with("gpt-3.5") {
            16_385
        } else {
            128_000
        }
    }
}

impl OpenAiClient {
    async fn post_request(&self, body: &Value) -> Result<ResponsesResponse, OpenAiError> {
        let re = Regex::new(r"Please retry in ([0-9.]+)s").unwrap();

        let res = match self.client.post(&self.base_url).json(body).send().await {
            Ok(res) => res,
            Err(e) => {
                let err_str = redact_secret(&e.to_string());
                tracing::error!("OpenAI Responses request failed (transport): {}", err_str);
                return Err(OpenAiError::TransientError(
                    Duration::from_secs(30),
                    err_str,
                ));
            }
        };

        if res.status().is_success() {
            let status = res.status();
            let body_text = res.text().await.map_err(|e| {
                let err_str = redact_secret(&e.to_string());
                OpenAiError::TransientError(Duration::from_secs(30), err_str)
            })?;
            return serde_json::from_str::<ResponsesResponse>(&body_text).map_err(|e| {
                tracing::error!("Failed to decode OpenAI Responses response: {}", e);
                OpenAiError::ApiError(status, format!("Decode error: {e}"))
            });
        }

        // Error handling — same pattern as openai.rs: parse status, extract
        // retry-after for rate limits, classify 401/403 as auth errors, etc.
        let status = res.status();
        let body_text = res.text().await.unwrap_or_default();
        let retry_secs = re
            .captures(&body_text)
            .and_then(|c| c.get(1).and_then(|m| m.as_str().parse::<f64>().ok()));
        let retry_after = Duration::from_secs_f64(retry_secs.unwrap_or(30.0));

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(OpenAiError::RateLimitExceeded(retry_after));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(OpenAiError::AuthenticationError(redact_secret(&body_text)));
        }
        if status.is_server_error() {
            return Err(OpenAiError::TransientError(
                retry_after,
                redact_secret(&body_text),
            ));
        }
        Err(OpenAiError::ApiError(status, redact_secret(&body_text)))
    }
}

// ── Translation ─────────────────────────────────────────────────

fn translate_ai_request(
    request: AiRequest,
    model: &str,
    max_tokens: u32,
    reasoning_effort: Option<&str>,
) -> Result<ResponsesRequest> {
    let mut input = Vec::new();

    // System prompt → "system" role message (Responses API accepts this directly)
    if let Some(system_text) = request.system {
        input.push(ResponsesInputItem::Message {
            role: "system".to_string(),
            content: system_text,
        });
    }

    for msg in request.messages {
        match msg.role {
            AiRole::System => {
                if let Some(content) = msg.content {
                    input.push(ResponsesInputItem::Message {
                        role: "system".to_string(),
                        content,
                    });
                }
            }
            AiRole::User => {
                if let Some(content) = msg.content {
                    input.push(ResponsesInputItem::Message {
                        role: "user".to_string(),
                        content,
                    });
                }
            }
            AiRole::Assistant => {
                if let Some(metadata) = msg.provider_metadata.as_ref()
                    && metadata.provider == OPENAI_RESPONSES_PROVIDER_METADATA
                {
                    if metadata.version != OPENAI_RESPONSES_PROVIDER_METADATA_VERSION {
                        anyhow::bail!(
                            "unsupported OpenAI Responses provider metadata version {}",
                            metadata.version
                        );
                    }
                    let output_items = metadata.data.as_array().ok_or_else(|| {
                        anyhow::anyhow!(
                            "OpenAI Responses provider metadata must contain an array of output items"
                        )
                    })?;
                    input.extend(output_items.iter().cloned().map(ResponsesInputItem::Raw));
                    continue;
                }
                // Assistant text content → "assistant" message
                if let Some(content) = msg.content {
                    input.push(ResponsesInputItem::Message {
                        role: "assistant".to_string(),
                        content,
                    });
                }
                // This fallback supports histories created by providers that
                // do not retain Responses output items. Responses-generated
                // calls always take the opaque replay path above so their
                // API-issued item IDs and status are never reconstructed.
                if let Some(tool_calls) = msg.tool_calls {
                    for tc in tool_calls {
                        let fc_id = if tc.id.starts_with("fc_") {
                            tc.id.clone()
                        } else {
                            format!("fc_{}", tc.id.trim_start_matches("call_"))
                        };
                        input.push(ResponsesInputItem::FunctionCall {
                            item_type: "function_call".to_string(),
                            id: fc_id,
                            call_id: tc.id,
                            name: tc.function_name,
                            arguments: serde_json::to_string(&tc.arguments).unwrap_or_default(),
                        });
                    }
                }
            }
            AiRole::Tool => {
                // Tool result → function_call_output
                if let (Some(call_id), Some(content)) = (msg.tool_call_id, msg.content) {
                    input.push(ResponsesInputItem::FunctionCallOutput {
                        item_type: "function_call_output".to_string(),
                        call_id,
                        output: content,
                    });
                }
            }
        }
    }

    let tools = request.tools.and_then(|t| {
        if t.is_empty() {
            None
        } else {
            Some(
                t.into_iter()
                    .map(|tool| ResponsesTool {
                        tool_type: "function".to_string(),
                        name: tool.name,
                        description: tool.description,
                        parameters: tool.parameters,
                    })
                    .collect(),
            )
        }
    });

    let reasoning = reasoning_effort.map(|effort| ReasoningConfig {
        effort: Some(effort.to_string()),
    });

    let text = request.response_format.map(|rf| match rf {
        AiResponseFormat::Json { .. } => TextConfig {
            format: TextFormatConfig {
                format_type: "json_object".to_string(),
            },
        },
        AiResponseFormat::Text => TextConfig {
            format: TextFormatConfig {
                format_type: "text".to_string(),
            },
        },
    });

    // json_object requests must mention JSON somewhere in the input. Add a
    // system instruction only when the caller did not already do so.
    if text
        .as_ref()
        .is_some_and(|config| config.format.format_type == "json_object")
        && !input.iter().any(|item| match item {
            ResponsesInputItem::Message { content, .. } => content.to_lowercase().contains("json"),
            _ => false,
        })
    {
        input.insert(
            0,
            ResponsesInputItem::Message {
                role: "system".to_string(),
                content: "Respond in JSON format.".to_string(),
            },
        );
    }

    Ok(ResponsesRequest {
        model: model.to_string(),
        input,
        include: Some(vec!["reasoning.encrypted_content".to_string()]),
        tools,
        temperature: None,
        max_output_tokens: Some(max_tokens),
        reasoning,
        text,
    })
}

const OPENAI_RESPONSES_PROVIDER_METADATA: &str = "openai.responses";
const OPENAI_RESPONSES_PROVIDER_METADATA_VERSION: u32 = 1;

fn translate_ai_response(resp: ResponsesResponse) -> Result<AiResponse> {
    let mut content_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    let truncated = resp.status == "incomplete";

    if truncated {
        tracing::warn!(
            "{}OpenAI Responses response truncated (status=incomplete).",
            crate::ai::get_log_prefix()
        );
    }

    for item in &resp.output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(parts) = item.get("content").and_then(Value::as_array) {
                    for part in parts {
                        match part.get("type").and_then(Value::as_str) {
                            Some("output_text") => {
                                if let Some(text) = part.get("text").and_then(Value::as_str) {
                                    content_parts.push(text.to_string());
                                }
                            }
                            Some("refusal") => {
                                if let Some(refusal) = part.get("refusal").and_then(Value::as_str) {
                                    tracing::warn!("OpenAI model refused: {}", refusal);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some("function_call") => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("OpenAI function_call is missing call_id"))?;
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("OpenAI function_call is missing name"))?;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("OpenAI function_call is missing arguments"))?;
                let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
                tool_calls.push(ToolCall {
                    id: call_id.to_string(),
                    function_name: name.to_string(),
                    arguments: args,
                    thought_signature: None,
                });
            }
            _ => {}
        }
    }

    let content = if content_parts.is_empty() {
        None
    } else {
        Some(content_parts.join(""))
    };

    let cached_tokens = resp
        .usage
        .input_tokens_details
        .and_then(|details| details.cached_tokens)
        .filter(|&cached| cached > 0 && cached <= resp.usage.input_tokens)
        .map(|cached| cached as usize);

    Ok(AiResponse {
        content,
        thought: None,
        thought_signature: None,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        usage: Some(AiUsage {
            prompt_tokens: resp.usage.input_tokens as usize,
            completion_tokens: resp.usage.output_tokens as usize,
            total_tokens: resp.usage.total_tokens as usize,
            cached_tokens,
        }),
        truncated,
        provider_metadata: Some(AiProviderMetadata {
            provider: OPENAI_RESPONSES_PROVIDER_METADATA.to_string(),
            version: OPENAI_RESPONSES_PROVIDER_METADATA_VERSION,
            data: Value::Array(resp.output),
        }),
    })
}

#[async_trait]
impl AiProvider for OpenAiClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        tracing::info!("Sending OpenAI Responses request...");

        let responses_req = translate_ai_request(
            request,
            &self.model,
            self.max_tokens,
            self.reasoning_effort.as_deref(),
        )?;

        let body = serde_json::to_value(&responses_req)?;
        let resp = self.post_request(&body).await?;
        translate_ai_response(resp)
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        crate::ai::openai::estimate_tokens_generic(request)
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
    use crate::ai::{AiMessage, AiResponseFormat, AiTool, classify_ai_error};
    use serde_json::json;

    fn user_msg(text: &str) -> AiMessage {
        AiMessage {
            role: AiRole::User,
            content: Some(text.to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
            provider_metadata: None,
        }
    }

    #[test]
    fn test_translate_request_basic() -> Result<()> {
        let request = AiRequest {
            system: Some("Be helpful.".to_string()),
            messages: vec![user_msg("Hello")],
            tools: None,
            temperature: Some(0.7),
            response_format: None,
            context_tag: None,
        };

        let req = translate_ai_request(request, "gpt-5.6-sol", 4096, None)?;
        assert_eq!(req.model, "gpt-5.6-sol");
        assert_eq!(req.input.len(), 2); // system + user
        assert!(
            req.temperature.is_none(),
            "Responses API always suppresses temperature"
        );
        assert_eq!(req.max_output_tokens, Some(4096));
        assert!(req.reasoning.is_none());
        assert!(req.tools.is_none());
        assert_eq!(
            req.include,
            Some(vec!["reasoning.encrypted_content".to_string()])
        );
        Ok(())
    }

    #[test]
    fn test_translate_request_with_reasoning_effort() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![user_msg("Test")],
            tools: None,
            temperature: Some(0.7),
            response_format: None,
            context_tag: None,
        };

        let req = translate_ai_request(request, "gpt-5.6-sol", 4096, Some("medium"))?;
        let reasoning = req.reasoning.unwrap();
        assert_eq!(reasoning.effort.as_deref(), Some("medium"));
        assert!(
            req.temperature.is_none(),
            "Responses API always suppresses temperature"
        );
        Ok(())
    }

    #[test]
    fn test_translate_request_with_tools() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![user_msg("Test")],
            tools: Some(vec![AiTool {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            }]),
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let req = translate_ai_request(request, "gpt-5.6-sol", 4096, Some("high"))?;
        let tools = req.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_file");
        // reasoning_effort and tools coexist on the Responses API
        assert!(req.reasoning.is_some());
        Ok(())
    }

    #[test]
    fn test_translate_request_json_format_injects_json_instruction() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![user_msg("Test")],
            tools: None,
            temperature: None,
            response_format: Some(AiResponseFormat::Json { schema: None }),
            context_tag: None,
        };

        let req = translate_ai_request(request, "gpt-5.6-sol", 4096, None)?;
        let text = req.text.unwrap();
        assert_eq!(text.format.format_type, "json_object");
        assert!(matches!(
            req.input.first(),
            Some(ResponsesInputItem::Message { role, content })
                if role == "system" && content == "Respond in JSON format."
        ));
        Ok(())
    }

    #[test]
    fn test_translate_request_json_format_does_not_duplicate_instruction() -> Result<()> {
        let request = AiRequest {
            system: Some("Return JSON only.".to_string()),
            messages: vec![user_msg("Test")],
            tools: None,
            temperature: None,
            response_format: Some(AiResponseFormat::Json { schema: None }),
            context_tag: None,
        };

        let req = translate_ai_request(request, "gpt-5.6-sol", 4096, None)?;
        assert_eq!(req.input.len(), 2);
        Ok(())
    }

    #[test]
    fn test_translate_response_preserves_and_replays_raw_output_items() -> Result<()> {
        let output = vec![
            json!({
                "id": "rsn_1",
                "type": "reasoning",
                "status": "completed",
                "phase": "analysis",
                "summary": [{"type": "summary_text", "text": "Checking the repository."}],
                "encrypted_content": "opaque-reasoning-state"
            }),
            json!({
                "id": "msg_1",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [
                    {"type": "future_content", "payload": {"kept": true}},
                    {"type": "output_text", "text": "I need to inspect a file."}
                ]
            }),
            json!({
                "id": "fc_server_issued_id",
                "type": "function_call",
                "status": "completed",
                "phase": "analysis",
                "call_id": "call_1",
                "name": "read_file",
                "arguments": r#"{"path":"/tmp/test"}"#
            }),
            json!({
                "id": "future_1",
                "type": "future_output_item",
                "provider_field": {"kept": true}
            }),
        ];
        let resp = ResponsesResponse {
            id: "resp_1".to_string(),
            status: "completed".to_string(),
            output: output.clone(),
            usage: ResponsesUsage::default(),
        };

        let ai_resp = translate_ai_response(resp)?;
        assert_eq!(
            ai_resp.content.as_deref(),
            Some("I need to inspect a file.")
        );
        let tc = ai_resp.tool_calls.clone().unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0].id, "call_1");
        assert_eq!(tc[0].function_name, "read_file");
        assert_eq!(tc[0].arguments["path"], "/tmp/test");

        let metadata = ai_resp.provider_metadata.clone().unwrap();
        assert_eq!(metadata.provider, OPENAI_RESPONSES_PROVIDER_METADATA);
        assert_eq!(metadata.version, OPENAI_RESPONSES_PROVIDER_METADATA_VERSION);
        assert_eq!(metadata.data, Value::Array(output.clone()));

        let history = vec![
            user_msg("Read the file."),
            AiMessage {
                role: AiRole::Assistant,
                content: ai_resp.content.clone(),
                thought: ai_resp.thought.clone(),
                thought_signature: ai_resp.thought_signature.clone(),
                tool_calls: ai_resp.tool_calls.clone(),
                tool_call_id: None,
                provider_metadata: Some(metadata),
            },
            AiMessage {
                role: AiRole::Tool,
                content: Some("file contents".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: Some("call_1".to_string()),
                provider_metadata: None,
            },
        ];
        let continuation = AiRequest {
            system: None,
            messages: history,
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };
        let request = translate_ai_request(continuation, "gpt-5.6-sol", 4096, None)?;
        assert_eq!(request.input.len(), output.len() + 2);
        assert!(
            matches!(&request.input[0], ResponsesInputItem::Message { role, .. } if role == "user")
        );
        for (index, item) in output.iter().enumerate() {
            assert_eq!(serde_json::to_value(&request.input[index + 1])?, *item);
        }
        assert!(matches!(
            request.input.last().unwrap(),
            ResponsesInputItem::FunctionCallOutput { call_id, output, .. }
                if call_id == "call_1" && output == "file contents"
        ));

        let serialized = serde_json::to_string(&ai_resp)?;
        let round_trip: AiResponse = serde_json::from_str(&serialized)?;
        assert_eq!(
            round_trip.provider_metadata.unwrap().data,
            Value::Array(output)
        );
        Ok(())
    }

    #[test]
    fn test_translate_response_incomplete_is_truncated() -> Result<()> {
        let resp = ResponsesResponse {
            id: "resp_3".to_string(),
            status: "incomplete".to_string(),
            output: vec![],
            usage: ResponsesUsage::default(),
        };

        let ai_resp = translate_ai_response(resp)?;
        assert!(ai_resp.truncated);
        Ok(())
    }

    #[test]
    fn test_translate_response_usage_mapping() -> Result<()> {
        let resp = ResponsesResponse {
            id: "resp_4".to_string(),
            status: "completed".to_string(),
            output: vec![],
            usage: ResponsesUsage {
                input_tokens: 20,
                output_tokens: 8,
                total_tokens: 28,
                input_tokens_details: Some(ResponsesInputTokensDetails {
                    cached_tokens: Some(16),
                }),
            },
        };

        let ai_resp = translate_ai_response(resp)?;
        let usage = ai_resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 20);
        assert_eq!(usage.completion_tokens, 8);
        assert_eq!(usage.total_tokens, 28);
        assert_eq!(usage.cached_tokens, Some(16));
        Ok(())
    }

    #[test]
    fn test_translate_response_ignores_invalid_cached_tokens() -> Result<()> {
        let resp: ResponsesResponse = serde_json::from_value(json!({
            "id": "resp_5",
            "status": "completed",
            "output": [],
            "usage": {
                "input_tokens": 20,
                "output_tokens": 8,
                "total_tokens": 28,
                "input_tokens_details": {"cached_tokens": 21}
            }
        }))?;

        assert_eq!(
            translate_ai_response(resp)?.usage.unwrap().cached_tokens,
            None
        );

        let malformed: ResponsesResponse = serde_json::from_value(json!({
            "id": "resp_6",
            "status": "completed",
            "output": [],
            "usage": {
                "input_tokens": 20,
                "output_tokens": 8,
                "total_tokens": 28,
                "input_tokens_details": "not-an-object"
            }
        }))?;
        assert_eq!(
            translate_ai_response(malformed)?
                .usage
                .unwrap()
                .cached_tokens,
            None
        );

        let zero: ResponsesResponse = serde_json::from_value(json!({
            "id": "resp_7",
            "status": "completed",
            "output": [],
            "usage": {
                "input_tokens": 20,
                "output_tokens": 8,
                "total_tokens": 28,
                "input_tokens_details": {"cached_tokens": 0}
            }
        }))?;
        assert_eq!(
            translate_ai_response(zero)?.usage.unwrap().cached_tokens,
            None
        );
        Ok(())
    }

    #[test]
    fn test_error_classification() {
        let err = OpenAiError::RateLimitExceeded(Duration::from_secs(5));
        assert!(matches!(
            err.ai_error_class(),
            AiErrorClass::RateLimit { .. }
        ));

        let err = OpenAiError::AuthenticationError("bad key".to_string());
        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);

        let err = anyhow::Error::new(OpenAiError::RateLimitExceeded(Duration::from_secs(5)));
        assert_eq!(
            classify_ai_error(&err),
            AiErrorClass::RateLimit {
                retry_after: Duration::from_secs(5)
            }
        );

        let err = anyhow::Error::new(OpenAiError::ApiError(
            reqwest::StatusCode::BAD_GATEWAY,
            "gateway error".to_string(),
        ));
        assert!(matches!(
            classify_ai_error(&err),
            AiErrorClass::Transient { .. }
        ));
    }

    #[test]
    fn test_default_context_window_for_gpt_5_6() {
        assert_eq!(
            OpenAiClient::default_context_window_for_model("gpt-5.6-sol"),
            1_050_000
        );
    }
}
