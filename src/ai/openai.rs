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

use crate::ai::token_budget::TokenBudget;
use crate::ai::{
    AiProvider, AiRequest, AiResponse, AiResponseFormat, AiRole, AiUsage, ProviderCapabilities,
    ToolCall,
};
use crate::utils::redact_secret;
use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenAiRequest {
    pub model: String,
    pub messages: Vec<OpenAiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OpenAiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OpenAiMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OpenAiToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: OpenAiToolCallFunction,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OpenAiToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OpenAiTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: OpenAiFunction,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OpenAiFunction {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenAiResponse {
    pub choices: Vec<OpenAiChoice>,
    pub usage: OpenAiUsage,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenAiChoice {
    pub index: u32,
    pub message: OpenAiMessage,
    pub finish_reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenAiUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum OpenAiCompatError {
    #[error("Rate limit exceeded, retry after {0:?}")]
    RateLimitExceeded(Duration),
    #[error("Transient error: {1}, retry after {0:?}")]
    TransientError(Duration, String),
    #[error("Authentication error: {0}")]
    AuthenticationError(String),
    #[error("API error {0}: {1}")]
    ApiError(reqwest::StatusCode, String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiProviderType {
    /// Official OpenAI API — uses `max_completion_tokens`.
    OpenAi,
    /// Third-party OpenAI-compatible APIs — uses `max_tokens`.
    OpenAiCompatible,
}

pub struct OpenAiCompatClient {
    model: String,
    base_url: String,
    context_window_size: usize,
    max_tokens: u32,
    provider_type: OpenAiProviderType,
    client: Client,
}

impl OpenAiCompatClient {
    pub fn new(
        base_url: String,
        provider_type: OpenAiProviderType,
        model: String,
        context_window_size: usize,
        max_tokens: u32,
        api_timeout_secs: u64,
    ) -> Self {
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

        Self {
            model,
            base_url,
            context_window_size,
            max_tokens,
            provider_type,
            client,
        }
    }

    pub fn default_base_url_for_model(model: &str) -> String {
        if model.starts_with("glm-") {
            "https://open.bigmodel.cn/api/paas/v4/chat/completions".to_string()
        } else if model.starts_with("moonshot-") {
            "https://api.moonshot.cn/v1/chat/completions".to_string()
        } else if model.starts_with("abab7-") {
            "https://api.minimax.chat/v1/text/chatcompletion_v2".to_string()
        } else {
            "https://api.openai.com/v1/chat/completions".to_string()
        }
    }

    pub fn default_context_window_for_model(model: &str) -> usize {
        if model.starts_with("glm-") || model.starts_with("moonshot-") {
            128_000
        } else if model.starts_with("abab7-") {
            245_760
        } else if model.starts_with("gpt-4o") || model.starts_with("gpt-4-turbo") {
            128_000
        } else if model.starts_with("gpt-3.5") {
            16_385
        } else {
            128_000
        }
    }

    async fn post_request(&self, body: &Value) -> Result<OpenAiResponse, OpenAiCompatError> {
        let re = Regex::new(r"Please retry in ([0-9.]+)s").unwrap();

        let res = match self.client.post(&self.base_url).json(body).send().await {
            Ok(res) => res,
            Err(e) => {
                let err_str = redact_secret(&e.to_string());
                tracing::error!("OpenAI request failed (transport): {}", err_str);
                return Err(OpenAiCompatError::TransientError(
                    Duration::from_secs(30),
                    err_str,
                ));
            }
        };

        if res.status().is_success() {
            let status = res.status();
            let body_text = res.text().await.map_err(|e| {
                let err_str = redact_secret(&e.to_string());
                tracing::error!("Failed to read OpenAI response body: {}", err_str);
                OpenAiCompatError::TransientError(Duration::from_secs(30), err_str)
            })?;
            match serde_json::from_str::<OpenAiResponse>(&body_text) {
                Ok(response) => {
                    tracing::info!(
                        "OpenAI response received. Tokens: in={}, out={}",
                        response.usage.prompt_tokens,
                        response.usage.completion_tokens
                    );
                    return Ok(response);
                }
                Err(e) => {
                    tracing::error!("Failed to decode OpenAI response: {}", e);
                    return Err(OpenAiCompatError::ApiError(
                        status,
                        format!("Parse error: {}", e),
                    ));
                }
            }
        }

        let status = res.status();
        let status_code = status.as_u16();

        let retry_after_duration = if status_code == 429 {
            res.headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(Duration::from_secs)
        } else {
            None
        };

        let error_text = res.text().await.unwrap_or_default();

        match status_code {
            429 => {
                let mut retry_seconds = retry_after_duration
                    .unwrap_or(Duration::from_secs(60))
                    .as_secs_f64();
                if let Some(caps) = re.captures(&error_text) {
                    retry_seconds = caps[1].parse::<f64>().unwrap_or(retry_seconds);
                }
                tracing::warn!("OpenAI 429 Rate Limit. Retry in {}s", retry_seconds);
                Err(OpenAiCompatError::RateLimitExceeded(
                    Duration::from_secs_f64(retry_seconds),
                ))?
            }
            401 | 403 => Err(OpenAiCompatError::AuthenticationError(error_text))?,
            500 | 502 | 503 | 504 => {
                tracing::warn!("OpenAI Server Error {}: {}", status, error_text);
                Err(OpenAiCompatError::TransientError(
                    Duration::from_secs(30),
                    error_text,
                ))?
            }
            _ => Err(OpenAiCompatError::ApiError(status, error_text))?,
        }
    }
}

fn normalize_schema(mut schema: Value) -> Value {
    if let Some(obj) = schema.as_object_mut() {
        if let Some(ty) = obj.get_mut("type")
            && let Some(s) = ty.as_str()
        {
            *ty = Value::String(s.to_lowercase());
        }
        for (_, val) in obj.iter_mut() {
            if val.is_object() || val.is_array() {
                *val = normalize_schema(val.clone());
            }
        }
    } else if let Some(arr) = schema.as_array_mut() {
        for val in arr.iter_mut() {
            if val.is_object() || val.is_array() {
                *val = normalize_schema(val.clone());
            }
        }
    }
    schema
}

fn translate_ai_request(request: AiRequest, max_tokens: u32, provider_type: OpenAiProviderType) -> Result<OpenAiRequest> {
    let mut messages = Vec::new();

    if let Some(system_text) = request.system {
        messages.push(OpenAiMessage {
            role: "system".to_string(),
            content: Some(system_text),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    for msg in request.messages {
        match msg.role {
            AiRole::System => {
                messages.push(OpenAiMessage {
                    role: "system".to_string(),
                    content: msg.content,
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            AiRole::User => {
                messages.push(OpenAiMessage {
                    role: "user".to_string(),
                    content: msg.content,
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            AiRole::Assistant => {
                messages.push(OpenAiMessage {
                    role: "assistant".to_string(),
                    content: msg.content,
                    tool_calls: msg.tool_calls.map(|tc| {
                        tc.into_iter()
                            .map(|t| OpenAiToolCall {
                                id: t.id,
                                tool_type: "function".to_string(),
                                function: OpenAiToolCallFunction {
                                    name: t.function_name,
                                    arguments: serde_json::to_string(&t.arguments).unwrap(),
                                },
                            })
                            .collect()
                    }),
                    tool_call_id: None,
                });
            }
            AiRole::Tool => {
                messages.push(OpenAiMessage {
                    role: "tool".to_string(),
                    content: msg.content,
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
                    .map(|tool| OpenAiTool {
                        tool_type: "function".to_string(),
                        function: OpenAiFunction {
                            name: tool.name,
                            description: tool.description,
                            parameters: normalize_schema(tool.parameters),
                        },
                    })
                    .collect(),
            )
        }
    });

    let response_format = request.response_format.map(|rf| match rf {
        AiResponseFormat::Json { .. } => serde_json::json!({"type": "json_object"}),
        AiResponseFormat::Text => serde_json::json!({"type": "text"}),
    });

    // OpenAI requires the word "json" to appear in at least one message when
    // using response_format: json_object. Inject it if missing.
    if response_format.as_ref().is_some_and(|rf| rf["type"] == "json_object") {
        let has_json = messages.iter().any(|m| {
            m.content
                .as_ref()
                .is_some_and(|c| c.to_lowercase().contains("json"))
        });
        if !has_json {
            if let Some(system_msg) = messages.iter_mut().find(|m| m.role == "system") {
                let content = system_msg.content.get_or_insert_default();
                content.push_str("\nRespond in JSON format.");
            } else {
                messages.insert(
                    0,
                    OpenAiMessage {
                        role: "system".to_string(),
                        content: Some("Respond in JSON format.".to_string()),
                        tool_calls: None,
                        tool_call_id: None,
                    },
                );
            }
        }
    }

    let (max_tokens_field, max_completion_tokens_field) = match provider_type {
        OpenAiProviderType::OpenAi => (None, Some(max_tokens)),
        OpenAiProviderType::OpenAiCompatible => (Some(max_tokens), None),
    };

    Ok(OpenAiRequest {
        model: String::new(),
        messages,
        tools,
        temperature: request.temperature,
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
        response_format,
    })
}

fn translate_ai_response(resp: OpenAiResponse) -> Result<AiResponse> {
    let choice = resp
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No choices in response"))?;

    let content = choice.message.content;
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

    let usage = Some(AiUsage {
        prompt_tokens: resp.usage.prompt_tokens as usize,
        completion_tokens: resp.usage.completion_tokens as usize,
        total_tokens: resp.usage.total_tokens as usize,
        cached_tokens: None,
    });

    Ok(AiResponse {
        content,
        thought: None,
        tool_calls,
        usage,
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
impl AiProvider for OpenAiCompatClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        tracing::info!("Sending OpenAI request...");

        let mut openai_req = translate_ai_request(request, self.max_tokens, self.provider_type)?;
        openai_req.model = self.model.clone();

        let resp_body = serde_json::to_value(&openai_req)?;
        let resp = self.post_request(&resp_body).await?;
        translate_ai_response(resp)
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
    use crate::ai::{AiMessage, AiTool};
    use serde_json::json;

    #[test]
    fn test_translate_request_system_and_user() -> Result<()> {
        let request = AiRequest {
            system: Some("You are helpful.".to_string()),
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Hello!".to_string()),
                thought: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: Some(0.7),
            response_format: None,
            context_tag: None,
        };

        let openai_req = translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible)?;

        assert_eq!(openai_req.messages.len(), 2);
        assert_eq!(openai_req.messages[0].role, "system");
        assert_eq!(
            openai_req.messages[0].content,
            Some("You are helpful.".to_string())
        );
        assert_eq!(openai_req.messages[1].role, "user");
        assert_eq!(openai_req.messages[1].content, Some("Hello!".to_string()));
        assert_eq!(openai_req.temperature, Some(0.7));
        assert_eq!(openai_req.max_tokens, Some(4096));

        Ok(())
    }

    #[test]
    fn test_translate_request_system_in_messages() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![
                AiMessage {
                    role: AiRole::System,
                    content: Some("Be concise.".to_string()),
                    thought: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::User,
                    content: Some("Say hi.".to_string()),
                    thought: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let openai_req = translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible)?;

        assert_eq!(openai_req.messages.len(), 2);
        assert_eq!(openai_req.messages[0].role, "system");
        assert_eq!(
            openai_req.messages[0].content,
            Some("Be concise.".to_string())
        );
        assert_eq!(openai_req.messages[1].role, "user");

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

        let openai_req = translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible)?;

        assert_eq!(openai_req.messages.len(), 1);
        assert_eq!(openai_req.messages[0].role, "assistant");
        assert_eq!(
            openai_req.messages[0].content,
            Some("I'll use a tool.".to_string())
        );
        let tool_calls = openai_req.messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].function.name, "test_tool");
        assert_eq!(tool_calls[0].function.arguments, r#"{"arg1":"val1"}"#);
        assert_eq!(tool_calls[0].tool_type, "function");

        Ok(())
    }

    #[test]
    fn test_translate_request_tool_response() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::Tool,
                content: Some(json!({"result": "success"}).to_string()),
                thought: None,
                tool_calls: None,
                tool_call_id: Some("call_123".to_string()),
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let openai_req = translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible)?;

        assert_eq!(openai_req.messages.len(), 1);
        assert_eq!(openai_req.messages[0].role, "tool");
        assert_eq!(
            openai_req.messages[0].tool_call_id,
            Some("call_123".to_string())
        );
        assert_eq!(
            openai_req.messages[0].content,
            Some(r#"{"result":"success"}"#.to_string())
        );

        Ok(())
    }

    #[test]
    fn test_translate_request_tools_definition() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![],
            tools: Some(vec![AiTool {
                name: "my_tool".to_string(),
                description: "Does something.".to_string(),
                parameters: json!({"type": "object"}),
            }]),
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let openai_req = translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible)?;

        let tools = openai_req.tools.as_ref().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_type, "function");
        assert_eq!(tools[0].function.name, "my_tool");
        assert_eq!(tools[0].function.description, "Does something.");
        assert_eq!(tools[0].function.parameters, json!({"type": "object"}));

        Ok(())
    }

    #[test]
    fn test_translate_request_empty_tools() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![],
            tools: Some(vec![]),
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let openai_req = translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible)?;

        // An empty tools array should be mapped to None so it gets skipped in serialization
        assert!(openai_req.tools.is_none());

        Ok(())
    }

    #[test]
    fn test_translate_request_conversation_chain() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![
                AiMessage {
                    role: AiRole::User,
                    content: Some("Use tool".to_string()),
                    thought: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::Assistant,
                    content: None,
                    thought: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "c1".to_string(),
                        function_name: "t1".to_string(),
                        arguments: json!({}),
                        thought_signature: None,
                    }]),
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::Tool,
                    content: Some(r#"{"ok":true}"#.to_string()),
                    thought: None,
                    tool_calls: None,
                    tool_call_id: Some("c1".to_string()),
                },
            ],
            tools: Some(vec![AiTool {
                name: "t1".to_string(),
                description: "d1".to_string(),
                parameters: json!({}),
            }]),
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let openai_req = translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible)?;

        assert_eq!(openai_req.messages.len(), 3);
        assert_eq!(openai_req.messages[0].role, "user");
        assert_eq!(openai_req.messages[1].role, "assistant");
        assert_eq!(openai_req.messages[2].role, "tool");
        assert_eq!(openai_req.messages[2].tool_call_id.as_deref(), Some("c1"));
        assert!(openai_req.tools.is_some());

        Ok(())
    }

    #[test]
    fn test_translate_request_json_format() -> Result<()> {
        let schema = json!({
            "type": "object",
            "properties": {"score": {"type": "number"}}
        });
        let request = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Score this.".to_string()),
                thought: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: Some(AiResponseFormat::Json {
                schema: Some(schema.clone()),
            }),
            context_tag: None,
        };

        let openai_req = translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible)?;

        assert_eq!(
            openai_req.response_format,
            Some(json!({"type": "json_object"}))
        );
        // "json" not in any message, so a system message should be prepended
        assert_eq!(openai_req.messages[0].role, "system");
        assert_eq!(
            openai_req.messages[0].content,
            Some("Respond in JSON format.".to_string())
        );
        assert_eq!(openai_req.messages.len(), 2);

        Ok(())
    }

    #[test]
    fn test_translate_request_json_format_no_injection_when_present() -> Result<()> {
        let request = AiRequest {
            system: Some("You are helpful.".to_string()),
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Return the score as JSON.".to_string()),
                thought: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: Some(AiResponseFormat::Json { schema: None }),
            context_tag: None,
        };

        let openai_req = translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible)?;

        assert_eq!(
            openai_req.response_format,
            Some(json!({"type": "json_object"}))
        );
        // "json" already in user message, system prompt should be unchanged
        assert_eq!(openai_req.messages.len(), 2);
        assert_eq!(
            openai_req.messages[0].content,
            Some("You are helpful.".to_string())
        );

        Ok(())
    }

    #[test]
    fn test_translate_request_temperature() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Test".to_string()),
                thought: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: Some(0.5),
            response_format: None,
            context_tag: None,
        };

        let openai_req = translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible)?;

        assert_eq!(openai_req.temperature, Some(0.5));

        Ok(())
    }

    #[test]
    fn test_translate_response_text() -> Result<()> {
        let openai_resp = OpenAiResponse {
            choices: vec![OpenAiChoice {
                index: 0,
                message: OpenAiMessage {
                    role: "assistant".to_string(),
                    content: Some("Hello!".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                },
                finish_reason: "stop".to_string(),
            }],
            usage: OpenAiUsage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
            },
        };

        let ai_resp = translate_ai_response(openai_resp)?;

        assert_eq!(ai_resp.content, Some("Hello!".to_string()));
        assert_eq!(ai_resp.thought, None);
        assert_eq!(ai_resp.tool_calls, None);
        let usage = ai_resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(usage.total_tokens, 30);
        assert_eq!(usage.cached_tokens, None);

        Ok(())
    }

    #[test]
    fn test_translate_response_tool_calls() -> Result<()> {
        let openai_resp = OpenAiResponse {
            choices: vec![OpenAiChoice {
                index: 0,
                message: OpenAiMessage {
                    role: "assistant".to_string(),
                    content: None,
                    tool_calls: Some(vec![OpenAiToolCall {
                        id: "call_abc".to_string(),
                        tool_type: "function".to_string(),
                        function: OpenAiToolCallFunction {
                            name: "my_tool".to_string(),
                            arguments: r#"{"arg":"val"}"#.to_string(),
                        },
                    }]),
                    tool_call_id: None,
                },
                finish_reason: "tool_calls".to_string(),
            }],
            usage: OpenAiUsage {
                prompt_tokens: 15,
                completion_tokens: 25,
                total_tokens: 40,
            },
        };

        let ai_resp = translate_ai_response(openai_resp)?;

        assert_eq!(ai_resp.content, None);
        assert_eq!(ai_resp.thought, None);
        let tool_calls = ai_resp.tool_calls.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_abc");
        assert_eq!(tool_calls[0].function_name, "my_tool");
        assert_eq!(tool_calls[0].arguments["arg"], "val");
        assert_eq!(tool_calls[0].thought_signature, None);

        Ok(())
    }

    #[test]
    fn test_translate_response_empty_choices() {
        let openai_resp = OpenAiResponse {
            choices: vec![],
            usage: OpenAiUsage {
                prompt_tokens: 10,
                completion_tokens: 0,
                total_tokens: 10,
            },
        };

        let result = translate_ai_response(openai_resp);
        assert!(result.is_err());
    }

    #[test]
    fn test_estimate_tokens() {
        let request = AiRequest {
            system: Some("System prompt".to_string()),
            messages: vec![
                AiMessage {
                    role: AiRole::User,
                    content: Some("Short message".to_string()),
                    thought: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::Assistant,
                    content: None,
                    thought: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "c1".to_string(),
                        function_name: "my_function".to_string(),
                        arguments: json!({"key": "value"}),
                        thought_signature: None,
                    }]),
                    tool_call_id: None,
                },
            ],
            tools: Some(vec![AiTool {
                name: "my_function".to_string(),
                description: "Does something".to_string(),
                parameters: json!({"type": "object"}),
            }]),
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let tokens = estimate_tokens_generic(&request);
        assert!(tokens > 10);
        assert!(tokens < 200);
    }

    #[test]
    fn test_max_tokens_for_openai_compatible() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Test".to_string()),
                thought: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let openai_req = translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible)?;

        assert_eq!(openai_req.max_tokens, Some(4096));
        assert_eq!(openai_req.max_completion_tokens, None);

        // Verify serialized JSON has max_tokens and no max_completion_tokens
        let json = serde_json::to_value(&openai_req)?;
        assert_eq!(json["max_tokens"], 4096);
        assert!(json.get("max_completion_tokens").is_none());

        Ok(())
    }

    #[test]
    fn test_max_completion_tokens_for_openai() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Test".to_string()),
                thought: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let openai_req = translate_ai_request(request, 4096, OpenAiProviderType::OpenAi)?;

        assert_eq!(openai_req.max_tokens, None);
        assert_eq!(openai_req.max_completion_tokens, Some(4096));

        // Verify serialized JSON has max_completion_tokens and no max_tokens
        let json = serde_json::to_value(&openai_req)?;
        assert!(json.get("max_tokens").is_none());
        assert_eq!(json["max_completion_tokens"], 4096);

        Ok(())
    }

    #[test]
    fn test_normalize_schema_lowercases_types() {
        let schema = json!({
            "type": "OBJECT",
            "properties": {
                "files": {
                    "type": "ARRAY",
                    "items": {
                        "type": "OBJECT",
                        "properties": {
                            "path": { "type": "STRING" },
                            "start_line": { "type": "INTEGER" }
                        }
                    }
                },
                "mode": { "type": "STRING", "enum": ["raw", "smart"] },
                "flag": { "type": "BOOLEAN" }
            }
        });

        let normalized = normalize_schema(schema);

        assert_eq!(normalized["type"], "object");
        assert_eq!(normalized["properties"]["files"]["type"], "array");
        assert_eq!(normalized["properties"]["files"]["items"]["type"], "object");
        assert_eq!(
            normalized["properties"]["files"]["items"]["properties"]["path"]["type"],
            "string"
        );
        assert_eq!(
            normalized["properties"]["files"]["items"]["properties"]["start_line"]["type"],
            "integer"
        );
        assert_eq!(normalized["properties"]["mode"]["type"], "string");
        assert_eq!(normalized["properties"]["flag"]["type"], "boolean");
    }

    #[test]
    fn test_normalize_schema_already_lowercase() {
        let schema = json!({"type": "object", "properties": {"x": {"type": "string"}}});
        let normalized = normalize_schema(schema.clone());
        assert_eq!(normalized, schema);
    }

    #[test]
    fn test_translate_request_normalizes_tool_schemas() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![],
            tools: Some(vec![AiTool {
                name: "my_tool".to_string(),
                description: "Does something.".to_string(),
                parameters: json!({
                    "type": "OBJECT",
                    "properties": {
                        "mode": { "type": "STRING" }
                    }
                }),
            }]),
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let openai_req = translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible)?;

        let tools = openai_req.tools.as_ref().unwrap();
        assert_eq!(tools[0].function.parameters["type"], "object");
        assert_eq!(
            tools[0].function.parameters["properties"]["mode"]["type"],
            "string"
        );

        Ok(())
    }
}
