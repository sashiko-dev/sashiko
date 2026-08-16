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

//! OpenAI models on Amazon Bedrock through the bedrock-mantle endpoint's
//! OpenAI Responses API.
//!
//! Requests are stateless (`store: false`). The provider's ordered output
//! items, including encrypted reasoning, travel back to the next turn in the
//! assistant message's `thought_signature`, which the session already
//! replays and which the response cache and review logs already scrub.

use crate::ai::bedrock_token::{BedrockToken, generate_token};
use crate::ai::openai_common::OpenAiCompatError;
use crate::ai::{
    AiProvider, AiRequest, AiResponse, AiResponseFormat, AiRole, AiUsage, ProviderCapabilities,
    ToolCall,
};
use crate::utils::redact_secret;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use regex::Regex;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{Duration, SystemTime};
use url::Url;

/// Key under which the Responses output transcript is kept in
/// `thought_signature`; anything else found there is ignored.
const REPLAY_KEY: &str = "bedrock_mantle_output";

/// Environment variable holding a long-term Amazon Bedrock API key, the same
/// one the AWS SDKs read. Unset means short-term keys are derived from the
/// AWS credential chain.
pub const BEARER_TOKEN_ENV: &str = "AWS_BEARER_TOKEN_BEDROCK";

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ResponsesTool>>,
    max_output_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<Value>,
    include: Vec<String>,
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_cache_options: Option<Value>,
}

#[derive(Debug, Serialize)]
struct ResponsesTool {
    #[serde(rename = "type")]
    tool_type: &'static str,
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Debug, Deserialize)]
struct ResponsesResponse {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    output: Vec<Value>,
    #[serde(default)]
    usage: Option<ResponsesUsage>,
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    error: Option<ResponsesApiError>,
}

#[derive(Debug, Deserialize)]
struct ResponsesUsage {
    input_tokens: u32,
    output_tokens: u32,
    total_tokens: u32,
    #[serde(default)]
    input_tokens_details: Option<InputTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
    #[serde(default)]
    cache_write_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct IncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsesApiError {
    #[serde(default)]
    code: Option<String>,
    message: String,
}

/// Settings for [`MantleClient`], taken from `[ai.bedrock]`.
#[derive(Debug, Clone, Default)]
pub struct MantleOptions {
    pub model: String,
    pub region: Option<String>,
    pub base_url: Option<String>,
    pub max_tokens: u32,
    pub effort: Option<String>,
    pub api_timeout_secs: u64,
    /// Long-term Bedrock API key; `None` derives short-term keys instead.
    pub api_key: Option<String>,
    /// Send prompt_cache_options and cache breakpoints.
    pub prompt_caching: bool,
    pub prompt_cache_ttl: Option<String>,
}

struct Endpoint {
    url: String,
    region: String,
}

pub struct MantleClient {
    model: String,
    region: Option<String>,
    base_url: Option<String>,
    max_tokens: u32,
    effort: Option<String>,
    api_key: Option<String>,
    prompt_caching: bool,
    prompt_cache_ttl: Option<String>,
    context_window_size: usize,
    endpoint: tokio::sync::OnceCell<Endpoint>,
    token: tokio::sync::Mutex<Option<BedrockToken>>,
    client: Client,
}

impl MantleClient {
    pub fn new(options: MantleOptions) -> Result<Self> {
        let MantleOptions {
            model,
            region,
            base_url,
            max_tokens,
            effort,
            api_timeout_secs,
            api_key,
            prompt_caching,
            prompt_cache_ttl,
        } = options;
        if let Some(url) = &base_url {
            responses_endpoint(url)?;
        }
        tracing::info!(
            "Bedrock model {model} uses the bedrock-mantle Responses API, authenticating with {}",
            if api_key.is_some() {
                BEARER_TOKEN_ENV
            } else {
                "short-term API keys from the AWS credential chain"
            }
        );

        let client = Client::builder()
            .timeout(Duration::from_secs(api_timeout_secs))
            .build()?;
        Ok(Self {
            model,
            region,
            base_url,
            max_tokens,
            effort,
            api_key,
            prompt_caching,
            prompt_cache_ttl,
            context_window_size: 128_000,
            endpoint: tokio::sync::OnceCell::new(),
            token: tokio::sync::Mutex::new(None),
            client,
        })
    }

    /// The signing region follows the endpoint, so a region in an overridden
    /// URL wins over `[ai.bedrock] region`, which wins over the AWS chain.
    async fn endpoint(&self) -> Result<&Endpoint> {
        self.endpoint
            .get_or_try_init(|| async {
                let mut region = self
                    .base_url
                    .as_deref()
                    .and_then(region_from_base_url)
                    .or_else(|| self.region.clone());
                if region.is_none() {
                    let sdk_config = aws_config::from_env().load().await;
                    region = sdk_config.region().map(|r| r.to_string());
                }
                let region = region.ok_or_else(|| {
                    anyhow!("bedrock-mantle: no AWS region; set [ai.bedrock] region or AWS_REGION")
                })?;
                let url = match &self.base_url {
                    Some(url) => responses_endpoint(url)?,
                    None => format!("https://bedrock-mantle.{region}.api.aws/v1/responses"),
                };
                Ok::<_, anyhow::Error>(Endpoint { url, region })
            })
            .await
    }

    async fn bearer_token(&self, region: &str) -> Result<String> {
        if let Some(key) = &self.api_key {
            return Ok(key.clone());
        }
        let mut cached = self.token.lock().await;
        if let Some(token) = cached.as_ref()
            && SystemTime::now() < token.refresh_at
        {
            return Ok(token.token.clone());
        }
        let token = generate_token(region).await?;
        let value = token.token.clone();
        *cached = Some(token);
        Ok(value)
    }

    async fn post_request(&self, body: &ResponsesRequest) -> Result<ResponsesResponse> {
        let endpoint = self.endpoint().await?;
        let token = self.bearer_token(&endpoint.region).await.map_err(|error| {
            OpenAiCompatError::AuthenticationError(format!(
                "failed to create an Amazon Bedrock API key: {error:#}"
            ))
        })?;
        let response = self
            .client
            .post(&endpoint.url)
            .bearer_auth(token)
            .json(body)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                let message = redact_secret(&error.to_string());
                tracing::error!("bedrock-mantle request failed (transport): {message}");
                return Err(
                    OpenAiCompatError::TransientError(Duration::from_secs(30), message).into(),
                );
            }
        };

        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);
        let text = response.text().await.map_err(|error| {
            OpenAiCompatError::TransientError(
                Duration::from_secs(30),
                redact_secret(&error.to_string()),
            )
        })?;

        if status.is_success() {
            let parsed: ResponsesResponse = serde_json::from_str(&text).map_err(|error| {
                OpenAiCompatError::ApiError(status, format!("Parse error: {error}"))
            })?;
            if let Some(error) = &parsed.error {
                let message = match &error.code {
                    Some(code) => format!("{code}: {}", error.message),
                    None => error.message.clone(),
                };
                return Err(OpenAiCompatError::ApiError(status, message).into());
            }
            if matches!(parsed.status.as_deref(), Some("failed" | "cancelled")) {
                return Err(OpenAiCompatError::ApiError(
                    status,
                    format!("Responses API returned status {:?}", parsed.status),
                )
                .into());
            }
            match &parsed.usage {
                Some(usage) => {
                    let details = usage.input_tokens_details.as_ref();
                    tracing::info!(
                        "{}bedrock-mantle response received. Tokens: in={}, out={}, \
                         cache_read={}, cache_write={}",
                        crate::ai::get_log_prefix(),
                        usage.input_tokens,
                        usage.output_tokens,
                        details.and_then(|d| d.cached_tokens).unwrap_or(0),
                        details.and_then(|d| d.cache_write_tokens).unwrap_or(0),
                    )
                }
                None => tracing::info!(
                    "{}bedrock-mantle response received without usage.",
                    crate::ai::get_log_prefix()
                ),
            }
            return Ok(parsed);
        }

        let error_text = redact_secret(&text);
        let error = match status {
            StatusCode::TOO_MANY_REQUESTS => {
                let retry_re = Regex::new(r"Please retry in ([0-9.]+)s").expect("valid regex");
                let delay = retry_re
                    .captures(&error_text)
                    .and_then(|caps| caps[1].parse::<f64>().ok())
                    .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
                    .or(retry_after)
                    .unwrap_or(Duration::from_secs(60));
                tracing::warn!(
                    "bedrock-mantle 429 rate limit. Retry in {}s",
                    delay.as_secs_f64()
                );
                OpenAiCompatError::RateLimitExceeded(delay)
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                // A rejected short-term key is renewed on the next request.
                self.token.lock().await.take();
                OpenAiCompatError::AuthenticationError(error_text)
            }
            status if status.is_server_error() => {
                tracing::warn!("bedrock-mantle server error {status}: {error_text}");
                OpenAiCompatError::TransientError(
                    retry_after.unwrap_or(Duration::from_secs(0)),
                    error_text,
                )
            }
            _ => OpenAiCompatError::ApiError(status, error_text),
        };
        Err(error.into())
    }
}

/// Mantle names OpenAI models by foundation model ID (`openai.gpt-...`);
/// inference profile IDs (`global.openai.*`) and ARNs only work elsewhere.
pub(crate) fn accepts_model(model: &str) -> bool {
    model.starts_with("openai.")
}

/// Accepts an API root ending in `/v1` (bedrock-mantle serves some models
/// under `/openai/v1`) or a complete `/responses` endpoint.
fn responses_endpoint(base_url: &str) -> Result<String> {
    let mut url = Url::parse(base_url)
        .map_err(|_| anyhow!("[ai.bedrock] invalid mantle_base_url {base_url}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(anyhow!("[ai.bedrock] invalid mantle_base_url {base_url}"));
    }
    let path = url.path().trim_end_matches('/');
    let path = if path.ends_with("/responses") {
        path.to_string()
    } else if path.ends_with("/v1") {
        format!("{path}/responses")
    } else {
        return Err(anyhow!(
            "[ai.bedrock] mantle_base_url {base_url} must end in /v1 or /responses"
        ));
    };
    url.set_path(&path);
    Ok(url.to_string())
}

/// Extracts the region from a bedrock-mantle or bedrock-runtime URL.
fn region_from_base_url(base_url: &str) -> Option<String> {
    let url = Url::parse(base_url).ok()?;
    let host = url.host_str()?;
    let region = host
        .strip_prefix("bedrock-mantle.")
        .and_then(|rest| rest.strip_suffix(".api.aws"))
        .or_else(|| {
            host.strip_prefix("bedrock-runtime.")
                .and_then(|rest| rest.strip_suffix(".amazonaws.com"))
        })?;
    (!region.is_empty() && !region.contains('.')).then(|| region.to_string())
}

fn message_item(role: &str, content: String) -> Value {
    json!({"type": "message", "role": role, "content": content})
}

fn item_type(item: &Value) -> Option<&str> {
    item.get("type").and_then(Value::as_str)
}

/// Keeps only output items accepted as continuation input, dropping a
/// trailing reasoning suffix that has no later message or function call.
fn sanitize_replay_items(items: impl IntoIterator<Item = Value>) -> Vec<Value> {
    let mut candidates: Vec<Value> = items
        .into_iter()
        .filter(|item| {
            matches!(
                item_type(item),
                Some("reasoning" | "message" | "function_call")
            )
        })
        .collect();
    let Some(last) = candidates
        .iter()
        .rposition(|item| matches!(item_type(item), Some("message" | "function_call")))
    else {
        return Vec::new();
    };
    candidates.truncate(last + 1);
    candidates
}

/// Bedrock accepts `prompt_cache_breakpoint` only on `input_text` parts of
/// prompt messages, so a plain string content is expanded into one part.
fn add_cache_breakpoint(item: &mut Value) {
    let is_prompt_message = item_type(item) == Some("message")
        && matches!(
            item.get("role").and_then(Value::as_str),
            Some("system" | "developer" | "user")
        );
    if !is_prompt_message {
        return;
    }
    let marker = json!({"mode": "explicit"});
    match item.get_mut("content") {
        Some(content) if content.is_string() => {
            let text = content.take();
            *content = json!([{
                "type": "input_text",
                "text": text,
                "prompt_cache_breakpoint": marker,
            }]);
        }
        Some(Value::Array(parts)) => {
            if let Some(part) = parts
                .iter_mut()
                .rev()
                .find(|part| item_type(part) == Some("input_text"))
            {
                part["prompt_cache_breakpoint"] = marker;
            }
        }
        _ => {}
    }
}

/// Marks the end of the system prompt and the last user message. Implicit
/// mode also lets Bedrock place an automatic breakpoint on the latest
/// message, covering the growing tool-call history; Bedrock allows four
/// cache writes per request, so two explicit ones leave room for it.
fn apply_prompt_cache(request: &mut ResponsesRequest, ttl: Option<&str>) {
    let mut options = json!({"mode": "implicit"});
    if let Some(ttl) = ttl {
        options["ttl"] = json!(ttl);
    }
    request.prompt_cache_options = Some(options);

    let input = &mut request.input;
    let is_system = |item: &Value| {
        item_type(item) == Some("message")
            && matches!(
                item.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            )
    };
    let system_end = input
        .iter()
        .take_while(|item| is_system(item))
        .count()
        .checked_sub(1);
    let last_user = input.iter().rposition(|item| {
        item_type(item) == Some("message")
            && item.get("role").and_then(Value::as_str) == Some("user")
    });
    for index in [system_end, last_user].into_iter().flatten() {
        add_cache_breakpoint(&mut input[index]);
    }
}

fn encode_replay(items: Vec<Value>) -> String {
    json!({ REPLAY_KEY: items }).to_string()
}

fn decode_replay(signature: &str) -> Option<Vec<Value>> {
    let mut value: Value = serde_json::from_str(signature).ok()?;
    match value.get_mut(REPLAY_KEY)?.take() {
        Value::Array(items) => Some(items),
        _ => None,
    }
}

fn translate_request(
    request: AiRequest,
    model: String,
    max_tokens: u32,
    effort: Option<String>,
) -> Result<ResponsesRequest> {
    // Temperature is omitted: OpenAI reasoning models reject it.
    let AiRequest {
        system,
        messages,
        tools,
        response_format,
        ..
    } = request;

    let mut input = Vec::new();
    if let Some(system) = system {
        input.push(message_item("system", system));
    }

    for message in messages {
        match message.role {
            AiRole::System => {
                if let Some(content) = message.content {
                    input.push(message_item("system", content));
                }
            }
            AiRole::User => {
                if let Some(content) = message.content {
                    input.push(message_item("user", content));
                }
            }
            AiRole::Assistant => {
                let replay = message
                    .thought_signature
                    .as_deref()
                    .and_then(decode_replay)
                    .map(sanitize_replay_items)
                    .filter(|items| !items.is_empty());
                if let Some(items) = replay {
                    input.extend(items);
                    continue;
                }
                if let Some(content) = message.content
                    && !content.is_empty()
                {
                    input.push(message_item("assistant", content));
                }
                for call in message.tool_calls.into_iter().flatten() {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.function_name,
                        "arguments": serde_json::to_string(&call.arguments)?,
                    }));
                }
            }
            AiRole::Tool => {
                let call_id = message
                    .tool_call_id
                    .ok_or_else(|| anyhow!("Responses tool result is missing tool_call_id"))?;
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": message.content.unwrap_or_default(),
                }));
            }
        }
    }

    let tools = tools.filter(|tools| !tools.is_empty()).map(|tools| {
        tools
            .into_iter()
            .map(|tool| ResponsesTool {
                tool_type: "function",
                name: tool.name,
                description: tool.description,
                parameters: tool.parameters,
            })
            .collect()
    });

    let text = match response_format {
        Some(AiResponseFormat::Json { .. }) => {
            // The prompt already carries the schema; json_object mode avoids
            // OpenAI's stricter structured-output schema subset. It requires
            // the word "JSON" somewhere in the input.
            let mentions_json = input.iter().any(|item| {
                item.get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| content.to_ascii_lowercase().contains("json"))
            });
            if !mentions_json {
                input.insert(
                    0,
                    message_item("system", "Respond in JSON format.".to_string()),
                );
            }
            Some(json!({"format": {"type": "json_object"}}))
        }
        Some(AiResponseFormat::Text) | None => None,
    };

    Ok(ResponsesRequest {
        model,
        input,
        tools,
        max_output_tokens: max_tokens,
        text,
        reasoning: effort.map(|effort| json!({"effort": effort})),
        include: vec!["reasoning.encrypted_content".to_string()],
        store: false,
        prompt_cache_options: None,
    })
}

fn translate_response(response: ResponsesResponse) -> Result<AiResponse> {
    let incomplete_reason = response
        .incomplete_details
        .as_ref()
        .and_then(|details| details.reason.as_deref());
    if response.status.as_deref() == Some("incomplete")
        && incomplete_reason != Some("max_output_tokens")
    {
        return Err(OpenAiCompatError::ApiError(
            StatusCode::OK,
            format!(
                "Responses API returned an incomplete response: {}",
                incomplete_reason.unwrap_or("unknown reason")
            ),
        )
        .into());
    }
    let truncated = incomplete_reason == Some("max_output_tokens");

    let replay_items = sanitize_replay_items(response.output.iter().cloned());
    let has_reasoning = replay_items
        .iter()
        .any(|item| item_type(item) == Some("reasoning"));

    let mut content_parts = Vec::new();
    let mut tool_calls = Vec::new();
    for item in &response.output {
        match item_type(item) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let text = match item_type(part) {
                        Some("output_text") => part.get("text"),
                        Some("refusal") => part.get("refusal"),
                        _ => None,
                    };
                    if let Some(text) = text.and_then(Value::as_str) {
                        content_parts.push(text.to_string());
                    }
                }
            }
            Some("function_call") => {
                let field = |name: &str| {
                    item.get(name)
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("Responses function_call is missing {name}"))
                };
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .map(|arguments| serde_json::from_str(arguments).unwrap_or(Value::Null))
                    .unwrap_or(Value::Null);
                tool_calls.push(ToolCall {
                    id: field("call_id")?.to_string(),
                    function_name: field("name")?.to_string(),
                    arguments,
                    thought_signature: None,
                });
            }
            _ => {}
        }
    }

    let usage = response.usage.map(|usage| AiUsage {
        prompt_tokens: usage.input_tokens as usize,
        completion_tokens: usage.output_tokens as usize,
        total_tokens: usage.total_tokens as usize,
        cached_tokens: usage
            .input_tokens_details
            .and_then(|details| details.cached_tokens)
            .filter(|tokens| *tokens > 0)
            .map(|tokens| tokens as usize),
    });
    if truncated {
        tracing::warn!(
            "{}bedrock-mantle output was truncated at max_output_tokens.",
            crate::ai::get_log_prefix()
        );
    }

    Ok(AiResponse {
        content: (!content_parts.is_empty()).then(|| content_parts.join("")),
        thought: None,
        thought_signature: has_reasoning.then(|| encode_replay(replay_items)),
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        usage,
        truncated,
    })
}

#[async_trait]
impl AiProvider for MantleClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let mut body = translate_request(
            request,
            self.model.clone(),
            self.max_tokens,
            self.effort.clone(),
        )?;
        if self.prompt_caching {
            apply_prompt_cache(&mut body, self.prompt_cache_ttl.as_deref());
        }
        translate_response(self.post_request(&body).await?)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: self.context_window_size,
        }
    }

    fn cache_identity(&self) -> String {
        let max_tokens = self.max_tokens.to_string();
        crate::ai::cache_identity_with(
            &self.model,
            &[
                ("api", Some("mantle")),
                ("region", self.region.as_deref()),
                ("base_url", self.base_url.as_deref()),
                ("effort", self.effort.as_deref()),
                ("max_tokens", Some(max_tokens.as_str())),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::test_http;
    use crate::ai::{AiMessage, AiTool, ClassifyAiError, classify_ai_error};

    fn message(role: AiRole, content: Option<&str>) -> AiMessage {
        AiMessage {
            role,
            content: content.map(str::to_string),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn request(messages: Vec<AiMessage>) -> AiRequest {
        AiRequest {
            system: None,
            messages,
            tools: None,
            temperature: Some(0.0),
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
        }
    }

    fn request_value(req: AiRequest, effort: Option<&str>) -> Result<Value> {
        let translated = translate_request(req, "openai.gpt".into(), 1000, effort.map(Into::into))?;
        Ok(serde_json::to_value(translated)?)
    }

    fn response(value: Value) -> Result<AiResponse> {
        translate_response(serde_json::from_value(value)?)
    }

    #[test]
    fn translates_request_without_temperature() -> Result<()> {
        let mut assistant = message(AiRole::Assistant, Some("Looking"));
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call_1".into(),
            function_name: "read_file".into(),
            arguments: json!({"path": "a.c"}),
            thought_signature: None,
        }]);
        let mut tool = message(AiRole::Tool, Some("int x;"));
        tool.tool_call_id = Some("call_1".into());
        let mut req = request(vec![message(AiRole::User, Some("Review")), assistant, tool]);
        req.system = Some("Be precise".into());
        req.tools = Some(vec![AiTool {
            name: "read_file".into(),
            description: "Read a file".into(),
            parameters: json!({"type": "object"}),
        }]);

        let value = request_value(req, Some("high"))?;

        assert_eq!(
            value,
            json!({
                "model": "openai.gpt",
                "input": [
                    {"type": "message", "role": "system", "content": "Be precise"},
                    {"type": "message", "role": "user", "content": "Review"},
                    {"type": "message", "role": "assistant", "content": "Looking"},
                    {"type": "function_call", "call_id": "call_1", "name": "read_file",
                     "arguments": "{\"path\":\"a.c\"}"},
                    {"type": "function_call_output", "call_id": "call_1", "output": "int x;"},
                ],
                "tools": [{"type": "function", "name": "read_file",
                           "description": "Read a file", "parameters": {"type": "object"}}],
                "max_output_tokens": 1000,
                "reasoning": {"effort": "high"},
                "include": ["reasoning.encrypted_content"],
                "store": false,
            })
        );
        Ok(())
    }

    #[test]
    fn json_mode_adds_instruction_only_when_missing() -> Result<()> {
        let mut req = request(vec![message(AiRole::User, Some("Score it"))]);
        req.response_format = Some(AiResponseFormat::Json { schema: None });
        let value = request_value(req, None)?;
        assert_eq!(value["text"], json!({"format": {"type": "json_object"}}));
        assert_eq!(value["input"][0]["content"], "Respond in JSON format.");

        let mut req = request(vec![message(AiRole::User, Some("Reply in JSON"))]);
        req.response_format = Some(AiResponseFormat::Json { schema: None });
        let value = request_value(req, None)?;
        assert_eq!(value["input"].as_array().unwrap().len(), 1);
        Ok(())
    }

    #[test]
    fn reasoning_round_trips_through_thought_signature() -> Result<()> {
        let output = vec![
            json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "opaque"}),
            json!({"type": "function_call", "call_id": "call_1", "name": "inspect",
                   "arguments": "{\"x\":1}"}),
            json!({"type": "reasoning", "id": "rs_orphan", "encrypted_content": "tail"}),
        ];
        let resp = response(json!({"status": "completed", "output": output}))?;
        let calls = resp.tool_calls.clone().unwrap();
        assert_eq!(calls[0].function_name, "inspect");
        assert_eq!(calls[0].arguments, json!({"x": 1}));

        let mut assistant = message(AiRole::Assistant, None);
        assistant.tool_calls = resp.tool_calls;
        assistant.thought_signature = resp.thought_signature;
        let mut tool = message(AiRole::Tool, Some("result"));
        tool.tool_call_id = Some("call_1".into());
        let value = request_value(
            request(vec![message(AiRole::User, Some("go")), assistant, tool]),
            None,
        )?;

        let input = value["input"].as_array().unwrap();
        assert_eq!(input[1..3], output[..2]);
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input.len(), 4);
        Ok(())
    }

    #[test]
    fn foreign_thought_signature_falls_back_to_normalized_fields() -> Result<()> {
        let mut assistant = message(AiRole::Assistant, Some("done"));
        assistant.thought_signature = Some("gemini-signature".into());
        let value = request_value(request(vec![assistant]), None)?;
        assert_eq!(
            value["input"],
            json!([{"type": "message", "role": "assistant", "content": "done"}])
        );
        Ok(())
    }

    #[test]
    fn response_without_reasoning_has_no_signature() -> Result<()> {
        let resp = response(json!({
            "status": "completed",
            "output": [{"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": "ok"}]}],
            "usage": {"input_tokens": 10, "output_tokens": 2, "total_tokens": 12,
                      "input_tokens_details": {"cached_tokens": 8}},
        }))?;
        assert_eq!(resp.content.as_deref(), Some("ok"));
        assert!(resp.thought_signature.is_none());
        let usage = resp.usage.unwrap();
        assert_eq!((usage.prompt_tokens, usage.cached_tokens), (10, Some(8)));
        Ok(())
    }

    #[test]
    fn incomplete_responses_truncate_or_fail() -> Result<()> {
        let truncated = response(json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [],
        }))?;
        assert!(truncated.truncated);

        let filtered = response(json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "content_filter"},
            "output": [],
        }));
        assert!(filtered.unwrap_err().to_string().contains("content_filter"));
        Ok(())
    }

    #[test]
    fn endpoint_and_region_from_base_url() -> Result<()> {
        assert_eq!(
            responses_endpoint("https://bedrock-mantle.us-east-1.api.aws/openai/v1/")?,
            "https://bedrock-mantle.us-east-1.api.aws/openai/v1/responses"
        );
        assert_eq!(
            responses_endpoint("http://127.0.0.1:9/v1/responses")?,
            "http://127.0.0.1:9/v1/responses"
        );
        assert!(responses_endpoint("https://bedrock-mantle.us-east-1.api.aws/").is_err());
        assert!(responses_endpoint("not a url").is_err());

        let region = |url| region_from_base_url(url);
        assert_eq!(
            region("https://bedrock-mantle.eu-north-1.api.aws/v1").as_deref(),
            Some("eu-north-1")
        );
        assert_eq!(
            region("https://bedrock-runtime.us-west-2.amazonaws.com/openai/v1").as_deref(),
            Some("us-west-2")
        );
        assert_eq!(region("https://api.openai.com/v1"), None);
        Ok(())
    }

    fn cache_request() -> AiRequest {
        let mut req = request(vec![
            message(AiRole::User, Some("first")),
            message(AiRole::Assistant, Some("answer")),
            message(AiRole::User, Some("second")),
        ]);
        req.system = Some("Be precise".into());
        req.tools = Some(vec![AiTool {
            name: "read_file".into(),
            description: "Read a file".into(),
            parameters: json!({"type": "object"}),
        }]);
        req
    }

    fn cached_value(req: AiRequest, ttl: Option<&str>) -> Result<Value> {
        let mut translated = translate_request(req, "openai.gpt".into(), 100, None)?;
        apply_prompt_cache(&mut translated, ttl);
        Ok(serde_json::to_value(translated)?)
    }

    fn breakpoints(value: &Value) -> usize {
        value.to_string().matches("prompt_cache_breakpoint").count()
    }

    #[test]
    fn prompt_cache_marks_system_and_last_user() -> Result<()> {
        let value = cached_value(cache_request(), Some("30m"))?;

        assert_eq!(
            value["prompt_cache_options"],
            json!({"mode": "implicit", "ttl": "30m"})
        );
        let marked = |text: &str| {
            json!([{
                "type": "input_text",
                "text": text,
                "prompt_cache_breakpoint": {"mode": "explicit"},
            }])
        };
        assert_eq!(value["input"][0]["content"], marked("Be precise"));
        assert_eq!(value["input"][1]["content"], "first");
        assert_eq!(value["input"][2]["content"], "answer");
        assert_eq!(value["input"][3]["content"], marked("second"));
        assert_eq!(breakpoints(&value), 2);
        assert_eq!(breakpoints(&value["tools"]), 0);
        Ok(())
    }

    #[test]
    fn prompt_cache_without_ttl_omits_it() -> Result<()> {
        let value = cached_value(cache_request(), None)?;
        assert_eq!(value["prompt_cache_options"], json!({"mode": "implicit"}));
        Ok(())
    }

    #[test]
    fn prompt_cache_marks_injected_json_instruction() -> Result<()> {
        let mut req = request(vec![message(AiRole::User, Some("Score it"))]);
        req.response_format = Some(AiResponseFormat::Json { schema: None });
        let value = cached_value(req, None)?;

        assert_eq!(
            value["input"][0]["content"][0]["text"],
            "Respond in JSON format."
        );
        assert_eq!(
            value["input"][0]["content"][0]["prompt_cache_breakpoint"],
            json!({"mode": "explicit"})
        );
        assert_eq!(breakpoints(&value), 2);
        Ok(())
    }

    #[test]
    fn prompt_cache_leaves_replayed_items_unchanged() -> Result<()> {
        let items = vec![
            json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "opaque"}),
            json!({"type": "message", "role": "assistant",
                   "content": [{"type": "output_text", "text": "done"}]}),
            json!({"type": "function_call", "call_id": "call_1", "name": "inspect",
                   "arguments": "{}"}),
        ];
        let mut assistant = message(AiRole::Assistant, None);
        assistant.thought_signature = Some(encode_replay(items.clone()));
        let mut tool = message(AiRole::Tool, Some("contents"));
        tool.tool_call_id = Some("call_1".into());
        let mut req = request(vec![message(AiRole::User, Some("go")), assistant, tool]);
        req.system = Some("Be precise".into());

        let value = cached_value(req, None)?;
        let input = value["input"].as_array().unwrap();
        assert_eq!(input[2..5], items[..]);
        assert_eq!(input[5]["type"], "function_call_output");
        assert_eq!(breakpoints(&value), 2);
        Ok(())
    }

    fn local_client(url: &str) -> MantleClient {
        MantleClient::new(MantleOptions {
            model: "openai.gpt-test".into(),
            region: Some("us-east-1".into()),
            base_url: Some(format!("{url}/v1")),
            max_tokens: 100,
            api_timeout_secs: 10,
            api_key: Some("test-key".into()),
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn posts_to_responses_endpoint_and_classifies_errors() -> Result<()> {
        let ok = json!({
            "status": "completed",
            "output": [{"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": "hi"}]}],
        })
        .to_string();
        let (server, bodies) = test_http::serve_sequence(vec![
            test_http::complete("200 OK", "", &ok),
            test_http::complete("429 Too Many Requests", "Retry-After: 7\r\n", "{}"),
            test_http::complete("400 Bad Request", "", r#"{"error":{"message":"bad"}}"#),
        ])
        .await;
        let client = local_client(&server.url);

        let first = client
            .generate_content(request(vec![message(AiRole::User, Some("x"))]))
            .await?;
        let limited = client
            .generate_content(request(vec![message(AiRole::User, Some("y"))]))
            .await
            .unwrap_err();
        let rejected = client
            .generate_content(request(vec![message(AiRole::User, Some("z"))]))
            .await
            .unwrap_err();
        server.finish().await;

        assert_eq!(first.content.as_deref(), Some("hi"));
        let sent: Value = serde_json::from_str(&bodies.lock().unwrap()[0])?;
        assert_eq!(sent["model"], "openai.gpt-test");
        assert!(sent.get("temperature").is_none());
        assert_eq!(
            classify_ai_error(&limited),
            OpenAiCompatError::RateLimitExceeded(Duration::from_secs(7)).ai_error_class()
        );
        assert_eq!(classify_ai_error(&rejected), crate::ai::AiErrorClass::Fatal);
        Ok(())
    }

    #[tokio::test]
    async fn prompt_caching_setting_reaches_the_request() -> Result<()> {
        let ok = json!({"status": "completed", "output": []}).to_string();
        let (server, bodies) = test_http::serve_sequence(vec![
            test_http::complete("200 OK", "", &ok),
            test_http::complete("200 OK", "", &ok),
        ])
        .await;
        let mut cached = local_client(&server.url);
        cached.prompt_caching = true;
        cached.prompt_cache_ttl = Some("1h".into());
        let uncached = local_client(&server.url);

        cached.generate_content(cache_request()).await?;
        uncached.generate_content(cache_request()).await?;
        server.finish().await;

        let sent: Vec<Value> = bodies
            .lock()
            .unwrap()
            .iter()
            .map(|body| serde_json::from_str(body).unwrap())
            .collect();
        assert_eq!(
            sent[0]["prompt_cache_options"],
            json!({"mode": "implicit", "ttl": "1h"})
        );
        assert_eq!(breakpoints(&sent[0]), 2);
        assert!(sent[1].get("prompt_cache_options").is_none());
        assert_eq!(breakpoints(&sent[1]), 0);
        Ok(())
    }

    #[test]
    fn cache_identity_names_the_api_and_knobs() {
        let client = local_client("http://127.0.0.1:9");
        let identity = client.cache_identity();
        assert!(
            identity.starts_with("openai.gpt-test|api=mantle"),
            "{identity}"
        );
        assert!(identity.contains("max_tokens=100"), "{identity}");
    }
}
