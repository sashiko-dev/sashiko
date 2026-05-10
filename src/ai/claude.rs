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
    AiErrorClass, AiProvider, AiRequest, AiResponse, AiRole, AiUsage, ClassifyAiError,
    ProviderCapabilities, ToolCall, classify_status_code, decode_stdio_ai_response,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tracing::info;

// --- Claude API Request/Response Types ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClaudeMessage {
    pub role: String, // "user" or "assistant"
    pub content: Vec<ClaudeContent>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClaudeContent {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    Thinking {
        thinking: String,
        signature: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub cache_type: String, // "ephemeral"
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub block_type: String, // "text"
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClaudeTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClaudeRequest {
    pub model: String,
    pub messages: Vec<ClaudeMessage>,
    pub max_tokens: u32, // Required by Claude API
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<SystemBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ClaudeTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ThinkingConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClaudeResponse {
    pub id: String,
    pub content: Vec<ClaudeContent>,
    pub stop_reason: Option<String>,
    pub usage: ClaudeUsage,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClaudeUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClaudeErrorResponse {
    #[serde(rename = "type")]
    pub error_type: String,
    pub error: ClaudeErrorDetails,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClaudeErrorDetails {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

// --- Error Types ---

#[derive(Debug, thiserror::Error)]
pub enum ClaudeError {
    #[error("Rate limit exceeded, retry after {0:?}")]
    RateLimitExceeded(Duration),
    #[error("API overloaded, retry after {0:?}")]
    OverloadedError(Duration),
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
    #[error("Authentication error: {0}")]
    AuthenticationError(String),
    #[error("API error {0}: {1}")]
    ApiError(reqwest::StatusCode, String),
}

impl ClassifyAiError for ClaudeError {
    fn ai_error_class(&self) -> AiErrorClass {
        match self {
            ClaudeError::RateLimitExceeded(retry_after) => AiErrorClass::RateLimit {
                retry_after: *retry_after,
            },
            ClaudeError::OverloadedError(retry_after) => AiErrorClass::Transient {
                retry_after: *retry_after,
            },
            ClaudeError::InvalidRequest(_) => AiErrorClass::Fatal,
            ClaudeError::AuthenticationError(_) => AiErrorClass::Fatal,
            ClaudeError::ApiError(status, _) => {
                classify_status_code(*status).unwrap_or(AiErrorClass::Fatal)
            }
        }
    }
}

// --- ClaudeClient ---

pub struct ClaudeClient {
    api_key: String,
    model: String,
    client: Client,
    enable_caching: bool,
    max_tokens: u32,
    base_url: String,
    thinking: Option<String>,
    effort: Option<String>,
}

impl ClaudeClient {
    pub fn new(
        model: String,
        enable_caching: bool,
        max_tokens: u32,
        base_url: String,
        thinking: Option<String>,
        effort: Option<String>,
    ) -> Self {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .or_else(|_| std::env::var("LLM_API_KEY"))
            .unwrap_or_default();

        Self {
            api_key,
            model,
            client: Client::new(),
            enable_caching,
            max_tokens,
            base_url,
            thinking,
            effort,
        }
    }

    pub fn default_base_url() -> String {
        "https://api.anthropic.com/v1/messages".to_string()
    }

    async fn post_request(&self, body: &ClaudeRequest) -> Result<ClaudeResponse> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-api-key",
            self.api_key.parse().context("Invalid API key format")?,
        );
        headers.insert(
            "anthropic-version",
            "2023-06-01"
                .parse()
                .context("Invalid anthropic-version header")?,
        );
        headers.insert(
            "content-type",
            "application/json"
                .parse()
                .context("Invalid content-type header")?,
        );

        let res = self
            .client
            .post(&self.base_url)
            .headers(headers)
            .json(body)
            .send()
            .await
            .context("Failed to send request to Claude API")?;

        let status = res.status();

        if status.is_success() {
            let body_text = res.text().await?;
            let response: ClaudeResponse =
                serde_json::from_str(&body_text).context("Failed to parse Claude API response")?;

            info!(
                "Claude response received. Tokens: in={}, out={}, cache_read={} cache_write={}",
                response.usage.input_tokens,
                response.usage.output_tokens,
                response.usage.cache_read_input_tokens.unwrap_or(0),
                response.usage.cache_creation_input_tokens.unwrap_or(0),
            );

            Ok(response)
        } else {
            // Parse retry-after header for 429 responses
            let retry_after_duration = if status.as_u16() == 429 {
                res.headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(Duration::from_secs)
            } else {
                None
            };

            let error_body = res
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());

            match status.as_u16() {
                429 => {
                    // Rate limit - use parsed retry-after or default to 60s
                    let duration = retry_after_duration.unwrap_or(Duration::from_secs(60));
                    Err(ClaudeError::RateLimitExceeded(duration))?
                }
                529 => {
                    // Overloaded - use exponential backoff
                    let duration = Duration::from_secs(5); // Start with 5s
                    Err(ClaudeError::OverloadedError(duration))?
                }
                400 => Err(ClaudeError::InvalidRequest(error_body))?,
                401 | 403 => Err(ClaudeError::AuthenticationError(error_body))?,
                _ => Err(ClaudeError::ApiError(status, error_body))?,
            }
        }
    }
}

// --- Translation Functions ---

pub fn translate_ai_request(
    request: &AiRequest,
    enable_caching: bool,
    max_tokens: u32,
    thinking: Option<String>,
    effort: Option<String>,
) -> Result<ClaudeRequest> {
    let mut messages = Vec::new();
    let mut system_blocks = Vec::new();

    // Extract system prompt from the explicit system field
    if let Some(system_text) = &request.system {
        system_blocks.push(SystemBlock {
            block_type: "text".to_string(),
            text: system_text.clone(),
            cache_control: None, // Will be set later if caching is enabled
        });
    }

    // Translate messages
    for msg in &request.messages {
        match msg.role {
            AiRole::System => {
                // System messages in messages array (for backward compatibility)
                // Add to system blocks
                if let Some(content) = &msg.content {
                    system_blocks.push(SystemBlock {
                        block_type: "text".to_string(),
                        text: content.clone(),
                        cache_control: None,
                    });
                }
            }
            AiRole::User => {
                let content = vec![ClaudeContent::Text {
                    text: msg.content.clone().unwrap_or_default(),
                    cache_control: None,
                }];
                messages.push(ClaudeMessage {
                    role: "user".to_string(),
                    content,
                });
            }
            AiRole::Assistant => {
                let mut content = Vec::new();

                // Add text content if present
                if let Some(text) = &msg.content {
                    content.push(ClaudeContent::Text {
                        text: text.clone(),
                        cache_control: None,
                    });
                }

                // Add thinking content if present
                if let (Some(thinking), Some(signature)) = (&msg.thought, &msg.thought_signature) {
                    content.push(ClaudeContent::Thinking {
                        thinking: thinking.clone(),
                        signature: signature.clone(),
                        cache_control: None,
                    });
                }

                // Add tool calls as tool_use blocks
                if let Some(tool_calls) = &msg.tool_calls {
                    for call in tool_calls {
                        content.push(ClaudeContent::ToolUse {
                            id: call.id.clone(),
                            name: call.function_name.clone(),
                            input: call.arguments.clone(),
                        });
                    }
                }

                messages.push(ClaudeMessage {
                    role: "assistant".to_string(),
                    content,
                });
            }
            AiRole::Tool => {
                // Tool results become user messages with tool_result content blocks
                let tool_call_id = msg
                    .tool_call_id
                    .as_ref()
                    .context("Tool message missing tool_call_id")?;

                let content = vec![ClaudeContent::ToolResult {
                    tool_use_id: tool_call_id.clone(),
                    content: msg.content.clone().unwrap_or_else(|| "{}".to_string()),
                    is_error: None,
                    cache_control: None,
                }];

                messages.push(ClaudeMessage {
                    role: "user".to_string(),
                    content,
                });
            }
        }
    }

    // Translate tools
    let tools = request.tools.as_ref().map(|t| {
        t.iter()
            .map(|tool| ClaudeTool {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.parameters.clone(),
                cache_control: None, // Will be set later if caching is enabled
            })
            .collect()
    });

    // Build the request
    let mut claude_request = ClaudeRequest {
        model: String::new(), // Will be set by the client
        messages,
        max_tokens,
        system: if system_blocks.is_empty() {
            None
        } else {
            Some(system_blocks)
        },
        tools,
        thinking: if thinking.is_some() || effort.is_some() {
            Some(ThinkingConfig { thinking, effort })
        } else {
            None
        },
    };

    // Apply cache control if enabled
    if enable_caching {
        apply_cache_control(&mut claude_request);
    }

    Ok(claude_request)
}

pub fn apply_cache_control(request: &mut ClaudeRequest) {
    // Mark last system block for caching
    if let Some(system) = &mut request.system
        && let Some(last_block) = system.last_mut()
    {
        last_block.cache_control = Some(CacheControl {
            cache_type: "ephemeral".to_string(),
        });
    }

    // Mark last tool for caching (if tools exist)
    if let Some(tools) = &mut request.tools
        && let Some(last_tool) = tools.last_mut()
    {
        last_tool.cache_control = Some(CacheControl {
            cache_type: "ephemeral".to_string(),
        });
    }

    // Mark last content for caching
    if let Some(message) = request.messages.last_mut()
        && let Some(content) = message.content.last_mut()
        && let ClaudeContent::Text { cache_control, .. }
        | ClaudeContent::Thinking { cache_control, .. }
        | ClaudeContent::ToolResult { cache_control, .. } = content
    {
        *cache_control = Some(CacheControl {
            cache_type: "ephemeral".to_string(),
        });
    }
}

pub fn translate_ai_response(resp: &ClaudeResponse) -> Result<AiResponse> {
    let mut thought_signature = String::new();
    let mut content = String::new();
    let mut thought = String::new();
    let mut tool_calls = Vec::new();

    for block in &resp.content {
        match block {
            ClaudeContent::Text { text, .. } => {
                content.push_str(text);
            }
            ClaudeContent::Thinking {
                thinking,
                signature,
                ..
            } => {
                thought.push_str(thinking);
                thought_signature.push_str(signature);
            }
            ClaudeContent::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id: id.clone(),
                    function_name: name.clone(),
                    arguments: input.clone(),
                    thought_signature: None,
                });
            }
            ClaudeContent::ToolResult { .. } => {
                // Tool results shouldn't appear in responses, but skip if they do
            }
        }
    }

    let cache_read = resp.usage.cache_read_input_tokens.unwrap_or(0);
    let cache_write = resp.usage.cache_creation_input_tokens.unwrap_or(0);
    let total_input = resp.usage.input_tokens + cache_read + cache_write;
    let usage = AiUsage {
        prompt_tokens: total_input as usize,
        completion_tokens: resp.usage.output_tokens as usize,
        total_tokens: (total_input + resp.usage.output_tokens) as usize,
        cached_tokens: if cache_read > 0 {
            Some(cache_read as usize)
        } else {
            None
        },
    };

    Ok(AiResponse {
        content: if content.is_empty() {
            None
        } else {
            Some(content)
        },
        thought: if thought.is_empty() {
            None
        } else {
            Some(thought)
        },
        thought_signature: if thought_signature.is_empty() {
            None
        } else {
            Some(thought_signature)
        },
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        usage: Some(usage),
    })
}

pub fn estimate_tokens_generic(request: &AiRequest) -> usize {
    use crate::ai::token_budget::TokenBudget;

    let mut total = 0;

    // Count system prompt tokens
    if let Some(system) = &request.system {
        total += TokenBudget::estimate_tokens(system);
    }

    // Count message tokens
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

    // Count tool definition tokens
    if let Some(tools) = &request.tools {
        for tool in tools {
            total += TokenBudget::estimate_tokens(&tool.name);
            total += TokenBudget::estimate_tokens(&tool.description);
            total += TokenBudget::estimate_tokens(&tool.parameters.to_string());
        }
    }

    total
}

// --- AiProvider Implementation ---

#[async_trait]
impl AiProvider for ClaudeClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        // 1. Translate generic request to Claude format
        let mut claude_req = translate_ai_request(
            &request,
            self.enable_caching,
            self.max_tokens,
            self.thinking.clone(),
            self.effort.clone(),
        )?;

        // 2. Set the model
        claude_req.model = self.model.clone();

        // 3. Make API call
        let response = self.post_request(&claude_req).await?;

        // 4. Translate response back to generic format
        translate_ai_response(&response)
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        // Reuse existing cl100k_base tokenizer from token_budget.rs
        estimate_tokens_generic(request)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: 200_000, // Claude 3.5 Sonnet context window
        }
    }

    // Optional caching methods - implement as no-ops for now
}

// --- StdioClaudeClient for IPC ---

pub struct StdioClaudeClient;

#[async_trait]
trait GenClaudeClient: Send + Sync {
    async fn exec_stdio(&self, msg: Value) -> Result<AiResponse> {
        tokio::task::spawn_blocking(move || -> Result<AiResponse> {
            use std::io::Write;
            let mut stdout = std::io::stdout();
            if let Err(e) = writeln!(stdout, "{}", serde_json::to_string(&msg)?) {
                eprintln!("Fatal error: parent closed stdout. Exiting. ({})", e);
                std::process::exit(1);
            }
            if let Err(e) = stdout.flush() {
                eprintln!("Fatal error: failed flushing stdout. Exiting. ({})", e);
                std::process::exit(1);
            }

            let stdin = std::io::stdin();
            let mut line = String::new();
            if stdin.read_line(&mut line)? == 0 {
                bail!("Unexpected EOF from stdin waiting for AI response");
            }

            decode_stdio_ai_response(&line)
        })
        .await?
    }
}

impl GenClaudeClient for StdioClaudeClient {}

#[async_trait]
impl AiProvider for StdioClaudeClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let msg = serde_json::json!({
            "type": "ai_request",
            "payload": request
        });
        self.exec_stdio(msg).await
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        estimate_tokens_generic(request)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: "stdio-claude".to_string(),
            context_window_size: 200_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{
        AiErrorClass, AiMessage, AiRequest, AiRole, AiTool, ClassifyAiError, DEFAULT_RETRY_AFTER,
        ToolCall,
    };
    use serde_json::json;

    fn make_request(messages: Vec<AiMessage>) -> AiRequest {
        AiRequest {
            system: None,
            messages,
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        }
    }

    #[test]
    fn test_rate_limit_exceeded_classifies_as_rate_limit() {
        let retry_after = Duration::from_secs(7);
        let err = ClaudeError::RateLimitExceeded(retry_after);

        assert_eq!(
            err.ai_error_class(),
            AiErrorClass::RateLimit { retry_after }
        );
    }

    #[test]
    fn test_overloaded_error_classifies_as_transient() {
        let retry_after = Duration::from_secs(11);
        let err = ClaudeError::OverloadedError(retry_after);

        assert_eq!(
            err.ai_error_class(),
            AiErrorClass::Transient { retry_after }
        );
    }

    #[test]
    fn test_invalid_request_classifies_as_fatal() {
        let err = ClaudeError::InvalidRequest("bad request".to_string());

        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_authentication_error_classifies_as_fatal() {
        let err = ClaudeError::AuthenticationError("bad key".to_string());

        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    #[test]
    fn test_api_error_server_status_classifies_as_transient() {
        let err = ClaudeError::ApiError(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "unavailable".to_string(),
        );

        assert_eq!(
            err.ai_error_class(),
            AiErrorClass::Transient {
                retry_after: DEFAULT_RETRY_AFTER,
            }
        );
    }

    #[test]
    fn test_api_error_client_status_classifies_as_fatal() {
        let err =
            ClaudeError::ApiError(reqwest::StatusCode::BAD_REQUEST, "bad request".to_string());

        assert_eq!(err.ai_error_class(), AiErrorClass::Fatal);
    }

    // --- ThinkingConfig tests (Bug 1 regression) ---

    #[test]
    fn test_thinking_config_omitted_when_both_none() {
        let req = make_request(vec![AiMessage {
            role: AiRole::User,
            content: Some("hi".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }]);

        let claude_req = translate_ai_request(&req, false, 4096, None, None).unwrap();
        assert!(claude_req.thinking.is_none());

        let json = serde_json::to_value(&claude_req).unwrap();
        assert!(!json.as_object().unwrap().contains_key("thinking"));
    }

    #[test]
    fn test_thinking_config_present_when_thinking_set() {
        let req = make_request(vec![AiMessage {
            role: AiRole::User,
            content: Some("hi".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }]);

        let claude_req =
            translate_ai_request(&req, false, 4096, Some("enabled".to_string()), None).unwrap();
        assert!(claude_req.thinking.is_some());
        let tc = claude_req.thinking.unwrap();
        assert_eq!(tc.thinking.as_deref(), Some("enabled"));
        assert!(tc.effort.is_none());
    }

    #[test]
    fn test_thinking_config_present_when_effort_set() {
        let req = make_request(vec![AiMessage {
            role: AiRole::User,
            content: Some("hi".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }]);

        let claude_req =
            translate_ai_request(&req, false, 4096, None, Some("high".to_string())).unwrap();
        assert!(claude_req.thinking.is_some());
        let tc = claude_req.thinking.unwrap();
        assert!(tc.thinking.is_none());
        assert_eq!(tc.effort.as_deref(), Some("high"));
    }

    #[test]
    fn test_thinking_config_serialization_populated() {
        let req = make_request(vec![AiMessage {
            role: AiRole::User,
            content: Some("hi".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }]);

        let claude_req = translate_ai_request(
            &req,
            false,
            4096,
            Some("enabled".to_string()),
            Some("high".to_string()),
        )
        .unwrap();
        let json = serde_json::to_value(&claude_req).unwrap();
        let thinking = &json["thinking"];
        assert_eq!(thinking["type"], "enabled");
        assert_eq!(thinking["effort"], "high");
    }

    // --- Request translation tests ---

    #[test]
    fn test_translate_system_and_user() -> Result<()> {
        let mut req = make_request(vec![AiMessage {
            role: AiRole::User,
            content: Some("Hello!".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }]);
        req.system = Some("You are helpful.".to_string());

        let claude_req = translate_ai_request(&req, false, 4096, None, None)?;

        let sys = claude_req.system.unwrap();
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0].text, "You are helpful.");

        assert_eq!(claude_req.messages.len(), 1);
        assert_eq!(claude_req.messages[0].role, "user");
        assert_eq!(claude_req.messages[0].content.len(), 1);
        if let ClaudeContent::Text { text, .. } = &claude_req.messages[0].content[0] {
            assert_eq!(text, "Hello!");
        } else {
            panic!("Expected Text content");
        }

        Ok(())
    }

    #[test]
    fn test_translate_assistant_with_tool_calls() -> Result<()> {
        let req = make_request(vec![AiMessage {
            role: AiRole::Assistant,
            content: Some("Let me check.".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                function_name: "git_log".to_string(),
                arguments: json!({"n": 5}),
                thought_signature: None,
            }]),
            tool_call_id: None,
        }]);

        let claude_req = translate_ai_request(&req, false, 4096, None, None)?;
        let content = &claude_req.messages[0].content;
        assert_eq!(content.len(), 2);

        if let ClaudeContent::Text { text, .. } = &content[0] {
            assert_eq!(text, "Let me check.");
        } else {
            panic!("Expected Text block");
        }

        if let ClaudeContent::ToolUse { id, name, input } = &content[1] {
            assert_eq!(id, "call_1");
            assert_eq!(name, "git_log");
            assert_eq!(input, &json!({"n": 5}));
        } else {
            panic!("Expected ToolUse block");
        }

        Ok(())
    }

    #[test]
    fn test_translate_tool_result() -> Result<()> {
        let req = make_request(vec![AiMessage {
            role: AiRole::Tool,
            content: Some("commit abc123".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: Some("call_1".to_string()),
        }]);

        let claude_req = translate_ai_request(&req, false, 4096, None, None)?;
        assert_eq!(claude_req.messages.len(), 1);
        assert_eq!(claude_req.messages[0].role, "user");

        if let ClaudeContent::ToolResult {
            tool_use_id,
            content,
            ..
        } = &claude_req.messages[0].content[0]
        {
            assert_eq!(tool_use_id, "call_1");
            assert_eq!(content, "commit abc123");
        } else {
            panic!("Expected ToolResult block");
        }

        Ok(())
    }

    #[test]
    fn test_translate_tools() -> Result<()> {
        let mut req = make_request(vec![AiMessage {
            role: AiRole::User,
            content: Some("hi".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }]);
        req.tools = Some(vec![AiTool {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}}
            }),
        }]);

        let claude_req = translate_ai_request(&req, false, 4096, None, None)?;
        let tools = claude_req.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_file");
        assert_eq!(tools[0].description, "Read a file");

        Ok(())
    }

    // --- Response translation tests ---

    #[test]
    fn test_translate_response_text() -> Result<()> {
        let resp = ClaudeResponse {
            id: "msg_1".to_string(),
            content: vec![ClaudeContent::Text {
                text: "Hello!".to_string(),
                cache_control: None,
            }],
            stop_reason: Some("end_turn".to_string()),
            usage: ClaudeUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        };

        let ai_resp = translate_ai_response(&resp)?;
        assert_eq!(ai_resp.content.as_deref(), Some("Hello!"));
        assert!(ai_resp.thought.is_none());
        assert!(ai_resp.tool_calls.is_none());

        let usage = ai_resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);

        Ok(())
    }

    #[test]
    fn test_translate_response_tool_calls() -> Result<()> {
        let resp = ClaudeResponse {
            id: "msg_2".to_string(),
            content: vec![ClaudeContent::ToolUse {
                id: "call_1".to_string(),
                name: "git_log".to_string(),
                input: json!({"n": 5}),
            }],
            stop_reason: Some("tool_use".to_string()),
            usage: ClaudeUsage {
                input_tokens: 20,
                output_tokens: 10,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        };

        let ai_resp = translate_ai_response(&resp)?;
        assert!(ai_resp.content.is_none());
        let calls = ai_resp.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function_name, "git_log");
        assert_eq!(calls[0].arguments, json!({"n": 5}));

        Ok(())
    }

    #[test]
    fn test_translate_response_thinking() -> Result<()> {
        let resp = ClaudeResponse {
            id: "msg_3".to_string(),
            content: vec![
                ClaudeContent::Thinking {
                    thinking: "Let me think...".to_string(),
                    signature: "sig_abc".to_string(),
                    cache_control: None,
                },
                ClaudeContent::Text {
                    text: "Here's my answer.".to_string(),
                    cache_control: None,
                },
            ],
            stop_reason: Some("end_turn".to_string()),
            usage: ClaudeUsage {
                input_tokens: 30,
                output_tokens: 15,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        };

        let ai_resp = translate_ai_response(&resp)?;
        assert_eq!(ai_resp.content.as_deref(), Some("Here's my answer."));
        assert_eq!(ai_resp.thought.as_deref(), Some("Let me think..."));
        assert_eq!(ai_resp.thought_signature.as_deref(), Some("sig_abc"));

        Ok(())
    }

    #[test]
    fn test_translate_response_usage_with_cache() -> Result<()> {
        let resp = ClaudeResponse {
            id: "msg_4".to_string(),
            content: vec![ClaudeContent::Text {
                text: "ok".to_string(),
                cache_control: None,
            }],
            stop_reason: Some("end_turn".to_string()),
            usage: ClaudeUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_input_tokens: Some(200),
                cache_read_input_tokens: Some(500),
            },
        };

        let ai_resp = translate_ai_response(&resp)?;
        let usage = ai_resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 800); // 100 + 500 + 200
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 850);
        assert_eq!(usage.cached_tokens, Some(500));

        Ok(())
    }

    // --- Cache control tests ---

    #[test]
    fn test_cache_control_applied() -> Result<()> {
        let mut req = make_request(vec![AiMessage {
            role: AiRole::User,
            content: Some("Hello!".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }]);
        req.system = Some("System prompt.".to_string());

        let claude_req = translate_ai_request(&req, true, 4096, None, None)?;

        // Last system block should have cache_control
        let sys = claude_req.system.unwrap();
        assert!(sys.last().unwrap().cache_control.is_some());

        // Last message content should have cache_control
        let last_msg = claude_req.messages.last().unwrap();
        if let ClaudeContent::Text { cache_control, .. } = last_msg.content.last().unwrap() {
            assert!(cache_control.is_some());
        } else {
            panic!("Expected Text content with cache_control");
        }

        Ok(())
    }

    #[test]
    fn test_cache_control_not_applied_when_disabled() -> Result<()> {
        let mut req = make_request(vec![AiMessage {
            role: AiRole::User,
            content: Some("Hello!".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }]);
        req.system = Some("System prompt.".to_string());

        let claude_req = translate_ai_request(&req, false, 4096, None, None)?;

        let sys = claude_req.system.unwrap();
        assert!(sys.last().unwrap().cache_control.is_none());

        let last_msg = claude_req.messages.last().unwrap();
        if let ClaudeContent::Text { cache_control, .. } = last_msg.content.last().unwrap() {
            assert!(cache_control.is_none());
        } else {
            panic!("Expected Text content");
        }

        Ok(())
    }
}
