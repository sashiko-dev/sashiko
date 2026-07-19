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
    AiErrorClass, AiProvider, AiRequest, AiResponse, AiRole, AiUsage, ClassifyAiError,
    ProviderCapabilities, ToolCall,
};
use crate::utils::redact_secret;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

// ============================================================================
// Request Types
// ============================================================================

/// Ollama API chat request.
#[derive(Debug, Serialize, Deserialize)]
pub struct OllamaRequest {
    /// Model name (e.g., "llama3", "mistral")
    pub model: String,

    /// Conversation messages
    pub messages: Vec<OllamaMessage>,

    /// Disable streaming (Ollama defaults to streaming)
    #[serde(default)]
    pub stream: bool,

    /// Generation parameters
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<OllamaOptions>,
}

/// A single message in the conversation.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OllamaMessage {
    /// Message role: "system", "user", or "assistant"
    pub role: String,

    /// Message content
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,

    /// Tool calls (for function calling)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OllamaToolCall>>,
}

/// A tool call made by the model.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OllamaToolCall {
    /// The function being called
    pub function: OllamaToolCallFunction,
}

/// Function details within a tool call.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OllamaToolCallFunction {
    /// Function name
    pub name: String,

    /// Function arguments as JSON
    pub arguments: Value,
}

/// Generation options for Ollama.
#[derive(Debug, Serialize, Deserialize)]
pub struct OllamaOptions {
    /// Temperature for sampling (0.0 to 1.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<usize>,

    /// Maximum number of tokens to generate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_predict: Option<i32>,

    /// Enforce JSON response format (Ollama 0.1.24+)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

// ============================================================================
// Response Types
// ============================================================================

/// Ollama API chat response.
#[derive(Debug, Serialize, Deserialize)]
pub struct OllamaResponse {
    /// The assistant's message
    pub message: OllamaMessage,

    /// Total time spent generating the response (nanoseconds)
    pub total_duration: u64,

    /// Time spent loading the model (nanoseconds)
    pub load_duration: u64,

    /// Number of tokens in the prompt
    pub prompt_eval_count: u32,

    /// Time spent evaluating the prompt (nanoseconds)
    pub prompt_eval_duration: u64,

    /// Number of tokens in the response
    pub eval_count: u32,

    /// Time spent generating the response (nanoseconds)
    pub eval_duration: u64,
}

// ============================================================================
// Error Types
// ============================================================================

/// Errors specific to Ollama API communication.
#[derive(Debug, thiserror::Error)]
pub enum OllamaError {
    /// Connection or transport error
    #[error("Transient error: {1}, retry after {0:?}")]
    TransientError(Duration, String),

    /// API returned an error
    #[error("API error: {0}")]
    ApiError(String),

    /// Model not found on the server
    #[error("Model not found: {0}")]
    ModelNotFound(String),
}

impl ClassifyAiError for OllamaError {
    fn ai_error_class(&self) -> AiErrorClass {
        match self {
            OllamaError::TransientError(retry_after, _) => AiErrorClass::Transient {
                retry_after: *retry_after,
            },
            OllamaError::ApiError(_) => AiErrorClass::Fatal,
            OllamaError::ModelNotFound(_) => AiErrorClass::Fatal,
        }
    }
}

// ============================================================================
// Client
// ============================================================================

/// Client for the Ollama API.
///
/// Connects to a local or remote Ollama instance and provides the
/// [`AiProvider`](crate::ai::AiProvider) interface.
pub struct OllamaClient {
    /// Model name to use for requests
    model: String,

    /// Base URL for the Ollama API
    base_url: String,

    /// Context window size for the model
    context_window_size: usize,

    /// Maximum tokens to generate
    max_tokens: u32,

    /// HTTP client
    client: Client,
}

impl OllamaClient {
    /// Create a new Ollama client.
    ///
    /// # Arguments
    ///
    /// * `base_url` - Ollama server address (e.g., "http://localhost:11434")
    /// * `model` - Model name (e.g., "llama3", "mistral:7b")
    /// * `context_window_size` - Maximum context window for the model
    /// * `max_tokens` - Maximum tokens to generate per response
    /// * `api_timeout_secs` - Request timeout in seconds
    pub fn new(
        base_url: String,
        model: String,
        context_window_size: usize,
        max_tokens: u32,
        api_timeout_secs: u64,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(api_timeout_secs))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let base_url = Self::normalize_base_url(&base_url)?;

        Ok(Self {
            model,
            base_url,
            context_window_size,
            max_tokens,
            client,
        })
    }

    /// Normalize the base URL to ensure it points to `/api/chat`.
    fn normalize_base_url(url: &str) -> Result<String> {
        let trimmed = url.trim_end_matches('/');

        if trimmed.ends_with("/api/chat") {
            Ok(trimmed.to_string())
        } else if trimmed.ends_with("/api") {
            Ok(format!("{}/chat", trimmed))
        } else {
            Ok(format!("{}/api/chat", trimmed))
        }
    }

    /// Default base URL for Ollama (local instance).
    pub fn default_base_url() -> String {
        "http://localhost:11434".to_string()
    }

    /// Default context window for Ollama models.
    ///
    /// Ollama doesn't expose context window information, so we use a
    /// reasonable default that works for most models.
    pub fn default_context_window_for_model(_model: &str) -> usize {
        128_000
    }

    /// Send a request to the Ollama API.
    async fn post_request(&self, body: &Value) -> Result<OllamaResponse, OllamaError> {
        let res = match self.client.post(&self.base_url).json(body).send().await {
            Ok(res) => res,
            Err(e) => {
                let err_str = redact_secret(&e.to_string());
                tracing::error!("Ollama request failed (transport): {}", err_str);
                return Err(OllamaError::TransientError(
                    Duration::from_secs(30), err_str,
                ));
            }
        };

        if res.status().is_success() {
            let body_text = res.text().await.map_err(|e| {
                let err_str = redact_secret(&e.to_string());
                tracing::error!("Failed to read Ollama response body: {}", err_str);
                OllamaError::TransientError(Duration::from_secs(0), err_str)
            })?;

            match serde_json::from_str::<OllamaResponse>(&body_text) {
                Ok(response) => {
                    tracing::info!(
                        "Ollama response received. Tokens: in={}, out={}",
                        response.prompt_eval_count,
                        response.eval_count
                    );
                    return Ok(response);
                }
                Err(e) => {
                    tracing::error!("Failed to decode Ollama response: {}", e);
                    return Err(OllamaError::ApiError(format!("Parse error: {}", e)));
                }
            }
        }

        let status = res.status();
        let error_text = redact_secret(&res.text().await.unwrap_or_default());
        let retry_after = Duration::from_secs(11);

        if status.as_u16() == 404 {
            Err(OllamaError::ModelNotFound(error_text))?
        } else if status.as_u16() >= 500 {
            Err(OllamaError::TransientError(retry_after, error_text))?
        } else {
            Err(OllamaError::ApiError(error_text))?
        }
    }
}

// ============================================================================
// Translation Functions
// ============================================================================

/// Translate an internal AiRequest to Ollama format.
fn translate_ollama_request(request: AiRequest, context_window_size: usize, max_tokens: u32) -> Result<OllamaRequest> {
    let mut messages = Vec::new();

    // Add system message if present
    if let Some(system_text) = request.system {
        messages.push(OllamaMessage {
            role: "system".to_string(),
            content: Some(system_text),
            tool_calls: None,
        });
    }

    // Convert conversation messages
    for msg in request.messages {
        match msg.role {
            AiRole::System => {
                messages.push(OllamaMessage {
                    role: "system".to_string(),
                    content: msg.content,
                    tool_calls: None,
                });
            }
            AiRole::User => {
                messages.push(OllamaMessage {
                    role: "user".to_string(),
                    content: msg.content,
                    tool_calls: None,
                });
            }
            AiRole::Assistant => {
                messages.push(OllamaMessage {
                    role: "assistant".to_string(),
                    content: msg.content,
                    tool_calls: msg.tool_calls.map(|tc| {
                        tc.into_iter()
                            .map(|t| OllamaToolCall {
                                function: OllamaToolCallFunction {
                                    name: t.function_name,
                                    arguments: t.arguments,
                                },
                            })
                            .collect()
                    }),
                });
            }
            AiRole::Tool => {
                // Ollama doesn't support tool role directly, map to assistant
                messages.push(OllamaMessage {
                    role: "assistant".to_string(),
                    content: msg.content,
                    tool_calls: None,
                });
            }
        }
    }

    // Build options with temperature and token limit
    let options = OllamaOptions {
        temperature: request.temperature,
        num_ctx: Some(context_window_size as usize),
        num_predict: Some(max_tokens as i32),
        format: Some("json".to_string()),  // Always enforce JSON for Ollama
    };

    Ok(OllamaRequest {
        model: String::new(),
        messages,
        stream: false,
        options: Some(options),
    })
}

/// Translate an Ollama response to internal AiResponse format.
fn translate_ollama_response(resp: OllamaResponse) -> Result<AiResponse> {
    let content = resp.message.content;

    let tool_calls = resp.message.tool_calls.map(|tc| {
        tc.into_iter()
            .map(|t| {
                // Ollama doesn't provide tool call IDs, generate one
                ToolCall {
                    id: format!("ollama_{}", uuid::Uuid::new_v4()),
                    function_name: t.function.name,
                    arguments: t.function.arguments,
                    thought_signature: None,
                }
            })
            .collect()
    });

    let usage = Some(AiUsage {
        prompt_tokens: resp.prompt_eval_count as usize,
        completion_tokens: resp.eval_count as usize,
        total_tokens: (resp.prompt_eval_count + resp.eval_count) as usize,
        cached_tokens: None,
    });

    Ok(AiResponse {
        content,
        thought: None,
        thought_signature: None,
        tool_calls,
        usage,
        truncated: false,
    })
}

/// Estimate token count for a request.
fn estimate_tokens(request: &AiRequest) -> usize {
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

// ============================================================================
// AiProvider Implementation
// ============================================================================

#[async_trait]
impl AiProvider for OllamaClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        tracing::info!("Sending Ollama request to model: {}", self.model);

        let mut ollama_req = translate_ollama_request(request, self.context_window_size, self.max_tokens)?;
        ollama_req.model = self.model.clone();

        let resp_body = serde_json::to_value(&ollama_req)?;
        let resp = self.post_request(&resp_body).await?;

        translate_ollama_response(resp)
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        estimate_tokens(request)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: self.context_window_size,
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiErrorClass, AiMessage, ClassifyAiError};
    use serde_json::json;

    #[test]
    fn test_normalize_base_url_with_chat_endpoint() {
        assert_eq!(
            OllamaClient::normalize_base_url("http://localhost:11434/api/chat").unwrap(),
            "http://localhost:11434/api/chat"
        );
    }

    #[test]
    fn test_normalize_base_url_with_api_only() {
        assert_eq!(
            OllamaClient::normalize_base_url("http://localhost:11434/api").unwrap(),
            "http://localhost:11434/api/chat"
        );
    }

    #[test]
    fn test_normalize_base_url_with_base_only() {
        assert_eq!(
            OllamaClient::normalize_base_url("http://localhost:11434").unwrap(),
            "http://localhost:11434/api/chat"
        );
    }

    #[test]
    fn test_normalize_base_url_with_trailing_slash() {
        assert_eq!(
            OllamaClient::normalize_base_url("http://localhost:11434/").unwrap(),
            "http://localhost:11434/api/chat"
        );
    }

    #[test]
    fn test_translate_ollama_request_with_system() -> Result<()> {
        let request = AiRequest {
            system: Some("You are a helpful assistant.".to_string()),
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Hello!".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: Some(0.7),
            response_format: None,
            context_tag: None,
        };

        let ollama_req = translate_ollama_request(request, 128_000, 4096)?;

        assert_eq!(ollama_req.messages.len(), 2);
        assert_eq!(ollama_req.messages[0].role, "system");
        assert_eq!(ollama_req.messages[1].role, "user");
        assert!(!ollama_req.stream);
        assert!(ollama_req.options.is_some());

        let options = ollama_req.options.unwrap();
        assert_eq!(options.temperature, Some(0.7));
        assert_eq!(options.num_predict, Some(4096));

        Ok(())
    }

    #[test]
    fn test_translate_ollama_request_with_tool_calls() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::Assistant,
                content: Some("Using a tool.".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_123".to_string(),
                    function_name: "search".to_string(),
                    arguments: json!({"query": "test"}),
                    thought_signature: None,
                }]),
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let ollama_req = translate_ollama_request(request, 128_000, 4096)?;

        assert_eq!(ollama_req.messages.len(), 1);
        assert_eq!(ollama_req.messages[0].role, "assistant");

        let tool_calls = ollama_req.messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].function.name, "search");
        assert_eq!(tool_calls[0].function.arguments["query"], "test");

        Ok(())
    }

    #[test]
    fn test_translate_ollama_response_text() -> Result<()> {
        let ollama_resp = OllamaResponse {
            message: OllamaMessage {
                role: "assistant".to_string(),
                content: Some("Hello from Ollama!".to_string()),
                tool_calls: None,
            },
            total_duration: 1_000_000_000,
            load_duration: 100_000_000,
            prompt_eval_count: 10,
            prompt_eval_duration: 500_000_000,
            eval_count: 20,
            eval_duration: 400_000_000,
        };

        let ai_resp = translate_ollama_response(ollama_resp)?;

        assert_eq!(ai_resp.content, Some("Hello from Ollama!".to_string()));
        assert_eq!(ai_resp.tool_calls, None);

        let usage = ai_resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(usage.total_tokens, 30);
        assert_eq!(usage.cached_tokens, None);

        Ok(())
    }

    #[test]
    fn test_translate_ollama_response_with_tool_calls() -> Result<()> {
        let ollama_resp = OllamaResponse {
            message: OllamaMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![OllamaToolCall {
                    function: OllamaToolCallFunction {
                        name: "calculator".to_string(),
                        arguments: json!({"expression": "2+2"}),
                    },
                }]),
            },
            total_duration: 800_000_000,
            load_duration: 50_000_000,
            prompt_eval_count: 15,
            prompt_eval_duration: 400_000_000,
            eval_count: 25,
            eval_duration: 350_000_000,
        };

        let ai_resp = translate_ollama_response(ollama_resp)?;

        assert_eq!(ai_resp.content, None);
        let tool_calls = ai_resp.tool_calls.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert!(tool_calls[0].id.starts_with("ollama_"));
        assert_eq!(tool_calls[0].function_name, "calculator");
        assert_eq!(tool_calls[0].arguments["expression"], "2+2");

        Ok(())
    }

    #[test]
    fn test_error_classification_connection() {
         let retry_after = Duration::from_secs(11);
        let err = OllamaError::TransientError(retry_after, "timeout".to_string());
        assert_eq!(
            err.ai_error_class(),
            AiErrorClass::Transient {
                retry_after: retry_after,
            }
        );
    }

    #[test]
    fn test_error_classification_api() {
        let err = OllamaError::ApiError("invalid request".to_string());
        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_error_classification_model_not_found() {
        let err = OllamaError::ModelNotFound("model xyz not found".to_string());
        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_default_base_url() {
        assert_eq!(
            OllamaClient::default_base_url(),
            "http://localhost:11434"
        );
    }

    #[test]
    fn test_default_context_window() {
        assert_eq!(OllamaClient::default_context_window_for_model("llama3"), 128_000);
        assert_eq!(OllamaClient::default_context_window_for_model("mistral"), 128_000);
    }

    #[test]
    fn test_estimate_tokens_basic() {
        let request = AiRequest {
            system: Some("System prompt".to_string()),
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Hello world".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let tokens = estimate_tokens(&request);
        assert!(tokens > 0);
        assert!(tokens < 100);
    }

    #[test]
    fn test_translate_request_preserves_temperature() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Test".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: Some(0.5),
            response_format: None,
            context_tag: None,
        };

        let ollama_req = translate_ollama_request(request, 128_000, 2048)?;
        let options = ollama_req.options.unwrap();

        assert_eq!(options.temperature, Some(0.5));
        assert_eq!(options.num_predict, Some(2048));

        Ok(())
    }

    #[test]
    fn test_translate_request_with_none_temperature() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Test".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let ollama_req = translate_ollama_request(request,128_000, 2048)?;
        let options = ollama_req.options.unwrap();

        assert_eq!(options.temperature, None);
        assert_eq!(options.num_predict, Some(2048));

        Ok(())
    }
}
