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
    AiProvider, AiRequest, AiResponse, AiRole, AiUsage, ProviderCapabilities, ToolCall,
};
use crate::utils::redact_secret;
use anyhow::{Context, Result};
use async_trait::async_trait;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Content {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub parts: Vec<Part>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum Part {
    Text {
        text: String,
        #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
        #[serde(default)]
        thought: bool,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: FunctionCall,
        #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: FunctionResponse,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FunctionCall {
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FunctionResponse {
    pub name: String,
    pub response: Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub function_declarations: Vec<FunctionDeclaration>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FunctionDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: Value, // JSON Schema
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentRequest {
    pub contents: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GenerationConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingConfig {
    pub include_thoughts: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<ThinkingConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentResponse {
    pub candidates: Option<Vec<Candidate>>,
    pub usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    #[serde(default)]
    pub content: Content,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageMetadata {
    pub prompt_token_count: u32,
    pub candidates_token_count: Option<u32>,
    pub total_token_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_content_token_count: Option<u32>,
    #[serde(flatten)]
    pub extra: Option<std::collections::HashMap<String, Value>>,
}

#[async_trait]
pub trait GenAiClient: Send + Sync {
    async fn generate_content(
        &self,
        request: GenerateContentRequest,
    ) -> Result<GenerateContentResponse>;
}

#[derive(Debug)]
pub enum GeminiError {
    QuotaExceeded(Duration),
    TransientError(Duration, String),
    PermissionDenied(String),
    ApiError(reqwest::StatusCode, String),
    Other(String),
}

impl std::fmt::Display for GeminiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GeminiError::QuotaExceeded(d) => write!(f, "Quota exceeded, retry in {:?}", d),
            GeminiError::TransientError(d, msg) => {
                write!(f, "Transient error: {}, retry in {:?}", msg, d)
            }
            GeminiError::PermissionDenied(msg) => write!(f, "Permission denied: {}", msg),
            GeminiError::ApiError(s, msg) => write!(f, "Gemini API error ({}): {}", s, msg),
            GeminiError::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for GeminiError {}

pub struct GeminiClient {
    model: String,
    base_url: String,
    client: Client,
}

impl GeminiClient {
    pub fn new(model: String) -> Self {
        let api_key = std::env::var("GEMINI_API_KEY")
            .or_else(|_| std::env::var("LLM_API_KEY"))
            .unwrap_or_default();
        let base_url = std::env::var("GOOGLE_GEMINI_BASE_URL")
            .or_else(|_| std::env::var("GEMINI_BASE_URL"))
            .unwrap_or_else(|_| "https://generativelanguage.googleapis.com".to_string());

        let mut headers = reqwest::header::HeaderMap::new();
        if !api_key.is_empty()
            && let Ok(value) = reqwest::header::HeaderValue::from_str(&api_key)
        {
            headers.insert("x-goog-api-key", value);
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            model,
            base_url,
            client,
        }
    }

    pub async fn generate_content_single(
        &self,
        request: &GenerateContentRequest,
    ) -> Result<GenerateContentResponse> {
        tracing::info!("Sending Gemini request...");

        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url, self.model
        );
        self.post_request(&url, request).await
    }

    async fn post_request<T: Serialize>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<GenerateContentResponse> {
        let re = Regex::new(r"Please retry in ([0-9.]+)s").unwrap();
        let res = match self.client.post(url).json(body).send().await {
            Ok(res) => res,
            Err(e) => {
                let err_str = redact_secret(&e.to_string());
                tracing::error!("Gemini request failed (transport): {}", err_str);
                return Err(GeminiError::TransientError(Duration::from_secs(30), err_str).into());
            }
        };

        if res.status().is_success() {
            let body_text = res.text().await?;
            match serde_json::from_str::<GenerateContentResponse>(&body_text) {
                Ok(response) => {
                    if let Some(usage) = &response.usage_metadata {
                        let cached = usage.cached_content_token_count.unwrap_or(0);
                        tracing::info!(
                            "{}Gemini response received. Tokens: in={}, cached={}, out={}",
                            crate::ai::get_log_prefix(),
                            usage.prompt_token_count.saturating_sub(cached),
                            cached,
                            usage.candidates_token_count.unwrap_or(0)
                        );
                    } else {
                        tracing::info!(
                            "{}Gemini response received. No usage metadata.",
                            crate::ai::get_log_prefix()
                        );
                    }
                    return Ok(response);
                }
                Err(e) => {
                    tracing::error!("Failed to decode Gemini response: {}", e);
                    anyhow::bail!("Failed to decode response: {}. Body: {}", e, body_text);
                }
            }
        }

        if res.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after_header = res
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse::<f64>().ok());

            let error_text = res.text().await?;
            let retry_seconds = if let Some(secs) = retry_after_header {
                secs
            } else if let Some(caps) = re.captures(&error_text) {
                caps[1].parse::<f64>().unwrap_or(30.0)
            } else {
                30.0
            };
            tracing::warn!(
                "Gemini 429 Quota Exceeded. Retry suggested in {}s. Body: {}",
                retry_seconds,
                error_text
            );
            return Err(
                GeminiError::QuotaExceeded(Duration::from_secs_f64(retry_seconds + 1.0)).into(),
            );
        }

        let status = res.status();
        let error_text = res.text().await?;

        if status == reqwest::StatusCode::FORBIDDEN {
            tracing::error!("Gemini Permission Denied (403): {}", error_text);
            return Err(GeminiError::PermissionDenied(error_text).into());
        }

        let is_transient = status.is_server_error() || status.as_u16() == 499;
        if is_transient {
            tracing::warn!(
                "Gemini API Transient Error: status={}, body={}",
                status,
                error_text
            );
            return Err(GeminiError::TransientError(Duration::from_secs(30), error_text).into());
        }

        tracing::error!("Gemini API Error: status={}, body={}", status, error_text);
        Err(GeminiError::ApiError(status, error_text).into())
    }
}

#[async_trait]
impl GenAiClient for GeminiClient {
    async fn generate_content(
        &self,
        request: GenerateContentRequest,
    ) -> Result<GenerateContentResponse> {
        self.generate_content_single(&request).await
    }
}

pub struct StdioGeminiClient;

#[async_trait]
impl AiProvider for StdioGeminiClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let msg = json!({
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
            model_name: "stdio-gemini".to_string(),
            context_window_size: 1_000_000,
        }
    }
}

impl StdioGeminiClient {
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
                anyhow::bail!("Unexpected EOF from stdin waiting for AI response");
            }

            let resp_msg: Value = serde_json::from_str(&line)?;
            if resp_msg["type"] == "ai_response" {
                let payload = serde_json::from_value(resp_msg["payload"].clone())?;
                Ok(payload)
            } else if resp_msg["type"] == "error" {
                let err_msg = resp_msg["payload"].as_str().unwrap_or("Unknown error");
                anyhow::bail!("Remote AI Error: {}", err_msg)
            } else {
                anyhow::bail!("Unexpected response type: {:?}", resp_msg["type"])
            }
        })
        .await?
    }
}

// --- Translation Helpers ---

fn translate_ai_request(request: AiRequest) -> Result<GenerateContentRequest> {
    let mut contents: Vec<Content> = Vec::new();
    let mut system_instruction = None;

    if let Some(sys_content) = request.system {
        system_instruction = Some(Content {
            role: "user".to_string(),
            parts: vec![Part::Text {
                text: sys_content,
                thought_signature: None,
                thought: false,
            }],
        });
    }

    for msg in request.messages {
        match msg.role {
            AiRole::System => {
                if let Some(content) = msg.content {
                    // Only set if not already set by request.system
                    if system_instruction.is_none() {
                        system_instruction = Some(Content {
                            role: "user".to_string(), // role is ignored for system_instruction but required by struct
                            parts: vec![Part::Text {
                                text: content,
                                thought_signature: None,
                                thought: false,
                            }],
                        });
                    }
                }
            }
            AiRole::User => {
                let part = Part::Text {
                    text: msg.content.unwrap_or_default(),
                    thought_signature: None,
                    thought: false,
                };
                if let Some(last) = contents.last_mut()
                    && last.role == "user"
                {
                    last.parts.push(part);
                    continue;
                }
                contents.push(Content {
                    role: "user".to_string(),
                    parts: vec![part],
                });
            }
            AiRole::Assistant => {
                let mut parts = Vec::new();
                if let Some(thought) = msg.thought {
                    parts.push(Part::Text {
                        text: thought,
                        thought_signature: None,
                        thought: true,
                    });
                }
                if let Some(text) = msg.content {
                    parts.push(Part::Text {
                        text,
                        thought_signature: None,
                        thought: false,
                    });
                }
                if let Some(tool_calls) = msg.tool_calls {
                    for call in tool_calls {
                        parts.push(Part::FunctionCall {
                            function_call: FunctionCall {
                                name: call.function_name,
                                args: call.arguments,
                            },
                            thought_signature: call.thought_signature,
                        });
                    }
                }
                contents.push(Content {
                    role: "model".to_string(),
                    parts,
                });
            }
            AiRole::Tool => {
                // Gemini expects a 'function' role for tool responses
                contents.push(Content {
                    role: "function".to_string(),
                    parts: vec![Part::FunctionResponse {
                        function_response: FunctionResponse {
                            name: msg
                                .tool_call_id
                                .context("Tool message missing tool_call_id")?,
                            response: serde_json::from_str(
                                &msg.content.unwrap_or_else(|| "{}".to_string()),
                            )
                            .unwrap_or(json!({})),
                        },
                    }],
                });
            }
        }
    }

    let tools = request.tools.map(|t| {
        vec![Tool {
            function_declarations: t
                .into_iter()
                .map(|tool| FunctionDeclaration {
                    name: tool.name,
                    description: tool.description,
                    parameters: tool.parameters,
                })
                .collect(),
        }]
    });

    let mut response_mime_type = None;
    let mut response_schema = None;

    if let Some(format) = request.response_format {
        match format {
            crate::ai::AiResponseFormat::Text => {
                response_mime_type = Some("text/plain".to_string());
            }
            crate::ai::AiResponseFormat::Json { schema } => {
                response_mime_type = Some("application/json".to_string());
                response_schema = schema;
            }
        }
    }

    Ok(GenerateContentRequest {
        contents,
        tools,
        system_instruction,
        generation_config: Some(GenerationConfig {
            response_mime_type,
            response_schema,
            temperature: request.temperature,
            thinking_config: Some(ThinkingConfig {
                include_thoughts: true,
            }),
        }),
    })
}

fn translate_ai_response(resp: GenerateContentResponse) -> Result<AiResponse> {
    let candidate = resp
        .candidates
        .as_ref()
        .and_then(|c| c.first())
        .ok_or_else(|| anyhow::anyhow!("No candidates returned from Gemini"))?;

    let mut content = String::new();
    let mut thought = String::new();
    let mut tool_calls = Vec::new();

    for part in &candidate.content.parts {
        match part {
            Part::Text {
                text,
                thought: is_thought,
                ..
            } => {
                if *is_thought {
                    thought.push_str(text);
                } else {
                    content.push_str(text);
                }
            }
            Part::FunctionCall {
                function_call,
                thought_signature,
            } => {
                tool_calls.push(ToolCall {
                    id: function_call.name.clone(), // Gemini doesn't have explicit call IDs in v1beta
                    function_name: function_call.name.clone(),
                    arguments: function_call.args.clone(),
                    thought_signature: thought_signature.clone(),
                });
            }
            _ => {}
        }
    }

    let usage = resp.usage_metadata.map(|m| AiUsage {
        prompt_tokens: m.prompt_token_count as usize,
        completion_tokens: m.candidates_token_count.unwrap_or(0) as usize,
        total_tokens: m.total_token_count as usize,
        cached_tokens: m.cached_content_token_count.map(|c| c as usize),
    });

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
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        usage,
    })
}

fn estimate_tokens_generic(request: &AiRequest) -> usize {
    let mut total = 0;
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
impl AiProvider for GeminiClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let gen_req = translate_ai_request(request)?;
        let resp = GenAiClient::generate_content(self, gen_req).await?;
        translate_ai_response(resp)
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        estimate_tokens_generic(request)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: 1_000_000, // Gemini 1.5 Pro default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiMessage, AiResponseFormat, AiRole, AiTool, ToolCall};
    use serde_json::json;

    #[test]
    fn test_translate_ai_request_system_and_user() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![
                AiMessage {
                    role: AiRole::System,
                    content: Some("You are a helpful assistant.".to_string()),
                    thought: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::User,
                    content: Some("Hello!".to_string()),
                    thought: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            tools: None,
            temperature: Some(0.7),
            response_format: None,
            context_tag: None,
        };

        let gemini_req = translate_ai_request(request)?;

        assert!(gemini_req.system_instruction.is_some());
        let sys_part = &gemini_req.system_instruction.unwrap().parts[0];
        if let Part::Text { text, .. } = sys_part {
            assert_eq!(text, "You are a helpful assistant.");
        } else {
            panic!("Expected Text part in system instruction");
        }

        assert_eq!(gemini_req.contents.len(), 1);
        assert_eq!(gemini_req.contents[0].role, "user");
        let user_part = &gemini_req.contents[0].parts[0];
        if let Part::Text { text, .. } = user_part {
            assert_eq!(text, "Hello!");
        } else {
            panic!("Expected Text part in user content");
        }

        assert_eq!(gemini_req.generation_config.unwrap().temperature, Some(0.7));

        Ok(())
    }

    #[test]
    fn test_translate_ai_request_assistant_tool_call() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::Assistant,
                content: Some("I will use a tool.".to_string()),
                thought: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_123".to_string(),
                    function_name: "test_tool".to_string(),
                    arguments: json!({"arg1": "val1"}),
                    thought_signature: Some("thought_sig_abc".to_string()),
                }]),
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let gemini_req = translate_ai_request(request)?;

        assert_eq!(gemini_req.contents.len(), 1);
        assert_eq!(gemini_req.contents[0].role, "model");
        assert_eq!(gemini_req.contents[0].parts.len(), 2);

        if let Part::Text { text, .. } = &gemini_req.contents[0].parts[0] {
            assert_eq!(text, "I will use a tool.");
        } else {
            panic!("Expected Text part first");
        }

        if let Part::FunctionCall {
            function_call,
            thought_signature,
        } = &gemini_req.contents[0].parts[1]
        {
            assert_eq!(function_call.name, "test_tool");
            assert_eq!(function_call.args["arg1"], "val1");
            assert_eq!(thought_signature.as_deref(), Some("thought_sig_abc"));
        } else {
            panic!("Expected FunctionCall part second");
        }

        Ok(())
    }

    #[test]
    fn test_translate_ai_response_with_thought_signature() -> Result<()> {
        let gemini_resp = GenerateContentResponse {
            candidates: Some(vec![Candidate {
                content: Content {
                    role: "model".to_string(),
                    parts: vec![Part::FunctionCall {
                        function_call: FunctionCall {
                            name: "test_tool".to_string(),
                            args: json!({"arg1": "val1"}),
                        },
                        thought_signature: Some("thought_sig_xyz".to_string()),
                    }],
                },
                finish_reason: Some("STOP".to_string()),
            }]),
            usage_metadata: Some(UsageMetadata {
                prompt_token_count: 10,
                candidates_token_count: Some(20),
                total_token_count: 30,
                cached_content_token_count: None,
                extra: None,
            }),
        };

        let ai_resp = translate_ai_response(gemini_resp)?;

        assert!(ai_resp.content.is_none());
        let tool_calls = ai_resp.tool_calls.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].function_name, "test_tool");
        assert_eq!(
            tool_calls[0].thought_signature.as_deref(),
            Some("thought_sig_xyz")
        );

        Ok(())
    }

    #[test]
    fn test_translate_ai_request_tool_response() -> Result<()> {
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

        let gemini_req = translate_ai_request(request)?;

        assert_eq!(gemini_req.contents.len(), 1);
        assert_eq!(gemini_req.contents[0].role, "function");
        if let Part::FunctionResponse { function_response } = &gemini_req.contents[0].parts[0] {
            assert_eq!(function_response.name, "call_123");
            assert_eq!(function_response.response["result"], "success");
        } else {
            panic!("Expected FunctionResponse part");
        }

        Ok(())
    }

    #[test]
    fn test_translate_ai_request_json_format() -> Result<()> {
        let schema = json!({
            "type": "object",
            "properties": {
                "score": {"type": "number"}
            }
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

        let gemini_req = translate_ai_request(request)?;
        let config = gemini_req.generation_config.unwrap();

        assert_eq!(
            config.response_mime_type.as_deref(),
            Some("application/json")
        );
        assert_eq!(config.response_schema.as_ref(), Some(&schema));

        Ok(())
    }

    #[test]
    fn test_translate_ai_request_conversation_chain() -> Result<()> {
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
                        thought_signature: Some("s1".to_string()),
                    }]),
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::Tool,
                    content: Some("{\"ok\":true}".to_string()),
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

        let gemini_req = translate_ai_request(request)?;

        assert_eq!(gemini_req.contents.len(), 3);
        assert_eq!(gemini_req.contents[0].role, "user");
        assert_eq!(gemini_req.contents[1].role, "model");
        assert_eq!(gemini_req.contents[2].role, "function");

        // Verify thought signature in middle of chain
        if let Part::FunctionCall {
            thought_signature, ..
        } = &gemini_req.contents[1].parts[0]
        {
            assert_eq!(thought_signature.as_deref(), Some("s1"));
        } else {
            panic!("Expected FunctionCall in middle of chain");
        }

        assert!(gemini_req.tools.is_some());
        assert_eq!(
            gemini_req.tools.as_ref().unwrap()[0].function_declarations[0].name,
            "t1"
        );

        Ok(())
    }

    #[test]
    fn test_estimate_tokens_logic() {
        let request = AiRequest {
            system: None,
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
        // "Short message" is ~2-3 tokens
        // "my_function" is ~2 tokens
        // "{\"key\": \"value\"}" is ~7 tokens
        // tool metadata...
        // Total should be around 20-40 tokens.
        assert!(tokens > 10);
        assert!(tokens < 200);
    }
}
