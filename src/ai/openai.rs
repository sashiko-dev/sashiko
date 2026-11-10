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

use crate::ai::openai_common::{
    OpenAiCompatError, build_http_client, lenient_option, normalize_base_url, post_json,
};
use crate::ai::{
    AiProvider, AiRequest, AiResponse, AiResponseFormat, AiRole, AiUsage, ProviderCapabilities,
    ToolCall,
};
use crate::settings::ServiceTier;
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::AtomicBool;

/// Path this transport speaks, appended to a `base_url` that names only the
/// API root.
const ENDPOINT_PATH: &str = "/chat/completions";

/// Top-level request fields Sashiko sets itself. Extra body fields may
/// not name them: overriding the model or the messages from configuration
/// would silently change what every stage asks for.
const RESERVED_REQUEST_FIELDS: &[&str] = &[
    "model",
    "messages",
    "tools",
    "temperature",
    "max_tokens",
    "max_completion_tokens",
    "response_format",
    "reasoning_effort",
    "service_tier",
];

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
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
    /// Processing tier.  Omitted unless configured, which leaves the
    /// request on the account's default tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
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
    /// Chain of thought of a reasoning model. Assistant turns carry it
    /// back when send_reasoning_content is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// The name OpenRouter and newer vLLM releases use for the chain of
    /// thought, read when reasoning_content is absent. A separate field
    /// rather than a serde alias, which would reject a response carrying
    /// both names as a duplicate field. Never sent.
    #[serde(default, skip_serializing)]
    pub reasoning: Option<String>,
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
    #[serde(default)]
    pub usage: OpenAiUsage,
    /// The tier that processed the request; see service_tier_mismatch().
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenAiChoice {
    pub index: u32,
    pub message: OpenAiMessage,
    pub finish_reason: String,
}

/// Every field defaults, and the object itself defaults on the response.
/// A compatible endpoint reports whichever counts it keeps, and some
/// report none at all.  The counts are accounting, and losing them costs
/// less than losing a completion that arrived intact.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct OpenAiUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    /// Absent on the many compatible endpoints that do not report cache
    /// hits.  A value that does not fit the documented shape is dropped
    /// rather than failing the response.
    #[serde(
        default,
        deserialize_with = "lenient_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub prompt_tokens_details: Option<OpenAiPromptTokensDetails>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct OpenAiPromptTokensDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
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
    effort: Option<String>,
    service_tier: Option<ServiceTier>,
    send_reasoning_content: bool,
    extra_headers: reqwest::header::HeaderMap,
    /// Carries each request's conversation ID when set.
    session_affinity_header: Option<reqwest::header::HeaderName>,
    extra_body: serde_json::Map<String, Value>,
    send_empty_tool_call_content: bool,
    /// Learned by post_json; see there.
    temperature_unsupported: AtomicBool,
    client: Client,
}

impl OpenAiCompatClient {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_url: String,
        provider_type: OpenAiProviderType,
        model: String,
        context_window_size: usize,
        max_tokens: u32,
        api_timeout_secs: u64,
        effort: Option<String>,
        service_tier: Option<ServiceTier>,
    ) -> Result<Self> {
        let client = build_http_client(api_timeout_secs)?;
        let base_url = normalize_base_url(&base_url, ENDPOINT_PATH)?;

        Ok(Self {
            model,
            base_url,
            context_window_size,
            max_tokens,
            provider_type,
            effort,
            service_tier,
            send_reasoning_content: false,
            client,
            extra_headers: reqwest::header::HeaderMap::new(),
            session_affinity_header: None,
            extra_body: serde_json::Map::new(),
            temperature_unsupported: AtomicBool::new(false),
            send_empty_tool_call_content: false,
        })
    }

    /// Sends each assistant turn's thought back as `reasoning_content`.
    pub fn with_reasoning_content(mut self, send: bool) -> Self {
        self.send_reasoning_content = send;
        self
    }

    pub fn with_extra_headers(
        mut self,
        headers: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self> {
        use reqwest::header::{HeaderName, HeaderValue};

        for (name, value) in headers {
            let header_name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| anyhow::anyhow!("invalid extra header name {name:?}: {e}"))?;
            let mut header_value = HeaderValue::from_str(value)
                .map_err(|e| anyhow::anyhow!("invalid value for extra header {name:?}: {e}"))?;
            header_value.set_sensitive(true);
            // Header names are case-insensitive, so two configured keys
            // differing only in case would collapse into one and drop a
            // credential without a word.
            if let Some(owner) = self.header_owner(&header_name) {
                anyhow::bail!("extra header {name:?} is already set by {owner}");
            }
            self.extra_headers.insert(header_name, header_value);
        }
        Ok(self)
    }

    /// Sends each request's conversation ID in the named header,
    /// replacing any name set before.
    pub fn with_session_affinity_header(mut self, name: Option<&str>) -> Result<Self> {
        self.session_affinity_header = None;
        let Some(name) = name else {
            return Ok(self);
        };
        let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| anyhow::anyhow!("invalid session_affinity_header {name:?}: {e}"))?;
        if let Some(owner) = self.header_owner(&header_name) {
            anyhow::bail!("session_affinity_header {name:?} is already set by {owner}");
        }
        self.session_affinity_header = Some(header_name);
        Ok(self)
    }

    /// Returns the setting that already uses this header name, if any.
    /// Both setters use this check to reject conflicting header settings.
    fn header_owner(&self, name: &reqwest::header::HeaderName) -> Option<&'static str> {
        if self.extra_headers.contains_key(name) {
            Some("extra_headers")
        } else if self.session_affinity_header.as_ref() == Some(name) {
            Some("session_affinity_header")
        } else {
            None
        }
    }

    /// Returns the extra headers for this request.
    /// Adds the affinity header if configured and a conversation ID exists.
    fn request_headers(
        &self,
        conversation_id: Option<uuid::Uuid>,
    ) -> Result<std::borrow::Cow<'_, reqwest::header::HeaderMap>> {
        let (Some(name), Some(id)) = (&self.session_affinity_header, conversation_id) else {
            return Ok(std::borrow::Cow::Borrowed(&self.extra_headers));
        };
        let value = reqwest::header::HeaderValue::from_str(&format!("sashiko-conversation-{id}"))?;
        let mut headers = self.extra_headers.clone();
        headers.insert(name.clone(), value);
        Ok(std::borrow::Cow::Owned(headers))
    }

    pub fn with_extra_body(mut self, fields: serde_json::Map<String, Value>) -> Result<Self> {
        if let Some(field) = fields
            .keys()
            .find(|key| RESERVED_REQUEST_FIELDS.contains(&key.as_str()))
        {
            anyhow::bail!("extra_body may not set {field:?}, which Sashiko sets itself");
        }
        self.extra_body = fields;
        Ok(self)
    }

    fn request_body(&self, request: &OpenAiRequest) -> Result<Value> {
        let mut body = serde_json::to_value(request)?;
        if let Some(object) = body.as_object_mut() {
            object.extend(self.extra_body.clone());
        }
        Ok(body)
    }

    pub fn with_empty_tool_call_content(mut self, enabled: bool) -> Self {
        self.send_empty_tool_call_content = enabled;
        self
    }

    pub fn default_base_url_for_model(model: &str) -> String {
        if model.starts_with("glm-") {
            "https://open.bigmodel.cn/api/paas/v4/chat/completions".to_string()
        } else if model.starts_with("moonshot-") {
            "https://api.moonshot.cn/v1/chat/completions".to_string()
        } else if model.starts_with("abab7-") || model.starts_with("MiniMax-") {
            "https://api.minimax.chat/v1/text/chatcompletion_v2".to_string()
        } else {
            "https://api.openai.com/v1/chat/completions".to_string()
        }
    }

    pub fn default_context_window_for_model(model: &str) -> usize {
        if model.starts_with("glm-") || model.starts_with("moonshot-") {
            128_000
        } else if model.starts_with("abab7-") || model.starts_with("MiniMax-") {
            245_760
        } else if model.starts_with("gpt-4o") || model.starts_with("gpt-4-turbo") {
            128_000
        } else if model.starts_with("gpt-3.5") {
            16_385
        } else {
            128_000
        }
    }

    fn build_request(&self, request: AiRequest) -> Result<OpenAiRequest> {
        let mut openai_req = translate_ai_request(
            request,
            self.max_tokens,
            self.provider_type,
            self.send_reasoning_content,
        )?;
        openai_req.model = self.model.clone();
        if self.send_empty_tool_call_content {
            fill_empty_tool_call_content(&mut openai_req.messages);
        }
        // Omitted unless configured: some endpoints that do not implement
        // reasoning_effort reject the whole request over an unknown field.
        openai_req.reasoning_effort = self.effort.clone();
        openai_req.service_tier = self.service_tier;
        Ok(openai_req)
    }

    async fn post_request(
        &self,
        headers: &reqwest::header::HeaderMap,
        body: &mut Value,
    ) -> Result<OpenAiResponse, OpenAiCompatError> {
        let response: OpenAiResponse = post_json(
            &self.client,
            &self.base_url,
            headers,
            body,
            &self.temperature_unsupported,
        )
        .await?;
        tracing::debug!(
            "OpenAI response received. Tokens: in={}, out={}, tier={}",
            response.usage.prompt_tokens,
            response.usage.completion_tokens,
            response.service_tier.as_deref().unwrap_or("-"),
        );
        crate::ai::openai_common::warn_on_service_tier_mismatch(
            self.service_tier,
            response.service_tier.as_deref(),
        );
        Ok(response)
    }
}

pub fn extra_headers_from_env(
    vars_by_header: &std::collections::BTreeMap<String, String>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<std::collections::BTreeMap<String, String>> {
    vars_by_header
        .iter()
        .map(|(header, var)| match lookup(var) {
            Some(value) if !value.is_empty() => Ok((header.clone(), value)),
            _ => Err(anyhow::anyhow!(
                "extra header {header:?} reads environment variable {var}, which is unset or empty"
            )),
        })
        .collect()
}

fn translate_ai_request(
    request: AiRequest,
    max_tokens: u32,
    provider_type: OpenAiProviderType,
    send_reasoning_content: bool,
) -> Result<OpenAiRequest> {
    let mut messages = Vec::new();

    if let Some(system_text) = request.system {
        messages.push(OpenAiMessage {
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
                messages.push(OpenAiMessage {
                    role: "system".to_string(),
                    content: msg.content,
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    reasoning: None,
                });
            }
            AiRole::User => {
                messages.push(OpenAiMessage {
                    role: "user".to_string(),
                    content: msg.content,
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    reasoning: None,
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
                    reasoning_content: if send_reasoning_content {
                        msg.thought.filter(|t| !t.is_empty())
                    } else {
                        None
                    },
                    reasoning: None,
                });
            }
            AiRole::Tool => {
                messages.push(OpenAiMessage {
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
                            parameters: tool.parameters,
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

    // Responses-backed gateways check user input for "json" and may not count
    // system instructions. Keep the hint on the first user message so later
    // turns preserve the same prompt prefix.
    if response_format
        .as_ref()
        .is_some_and(|rf| rf["type"] == "json_object")
    {
        if let Some(user_msg) = messages.iter_mut().find(|m| m.role == "user") {
            let content = user_msg.content.get_or_insert_default();
            if !content.to_lowercase().contains("json") {
                content.push_str("\nRespond in JSON format.");
            }
        } else {
            messages.push(OpenAiMessage {
                role: "user".to_string(),
                content: Some("Respond in JSON format.".to_string()),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                reasoning: None,
            });
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
        // Set by the caller, which holds the configured level; the translation
        // has no view of it.
        reasoning_effort: None,
        service_tier: None,
        response_format,
    })
}

/// serde_json keeps insertion order and the settings loader builds
/// tables from hash maps so return sorted json otherwise cache may be
/// random between runs
fn sorted_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries: Vec<_> = object.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), sorted_json(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted_json).collect()),
        other => other.clone(),
    }
}

fn fill_empty_tool_call_content(messages: &mut [OpenAiMessage]) {
    for message in messages {
        if message.role == "assistant" && message.tool_calls.is_some() {
            message.content.get_or_insert_default();
        }
    }
}

fn translate_ai_response(resp: OpenAiResponse) -> Result<AiResponse> {
    let choice = resp
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("No choices in response"))?;

    let content = choice.message.content;
    let thought = [choice.message.reasoning_content, choice.message.reasoning]
        .into_iter()
        .flatten()
        .find(|t| !t.is_empty());
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

    let truncated = choice.finish_reason == "length";

    if truncated {
        tracing::warn!(
            "{}OpenAI response truncated due to finish_reason = length.",
            crate::ai::get_log_prefix()
        );
    }

    // prompt_tokens already counts the cached prefix, so cached_tokens is a
    // breakdown of it rather than an addend the way Anthropic reports it.
    // A larger count means the endpoint reports the prefix alongside the
    // prompt instead.  Clamping to prompt_tokens leaves the two equal, so a
    // consumer subtracting for uncached input still gets zero and the token
    // budget still never trips.  Drop the count instead.
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
        cached_tokens: if cached > 0 {
            Some(cached as usize)
        } else {
            None
        },
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
impl AiProvider for OpenAiCompatClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        tracing::debug!("Sending OpenAI request...");

        let headers = self.request_headers(request.conversation_id)?;
        let openai_req = self.build_request(request)?;

        let mut resp_body = self.request_body(&openai_req)?;
        let resp = self.post_request(&headers, &mut resp_body).await?;
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
        // the request, so the bare model name cannot distinguish them. A
        // gpt-5.x call that hit the 4096 default comes back empty with
        // finish_reason "length"; raising max_tokens has to miss that entry
        // rather than replay it. base_url separates two endpoints serving
        // the same model name, and provider_type decides whether the request
        // carries max_tokens or max_completion_tokens.  The effort level is
        // the same case: it shapes the reply, and a run raised to "high" has
        // to miss the entry the "medium" run wrote rather than replay it.
        // Sending reasoning back needs every cached reply to carry its
        // thought, which replies cached before thoughts were read lack. The
        // setting enters the identity only when on, so other entries stay
        // valid. Extra headers enter by name only: their values are
        // credentials that must stay out of the cache file, and a header
        // whose value steers the reply is not supported. Extra body fields can
        // change the reply (a routing hint), so they join the identity in a
        // key order that does not depend on how the settings were parsed.
        //
        // The empty tool-call content changes the prompt the model sees
        // on chat templates that render the missing field, so it joins
        // the identity, but only when enabled to keep existing entries.
        let max_tokens = self.max_tokens.to_string();
        let extra_headers = (!self.extra_headers.is_empty()).then(|| {
            let mut names: Vec<&str> = self.extra_headers.keys().map(|n| n.as_str()).collect();
            names.sort_unstable();
            names.join(",")
        });
        let extra_body = (!self.extra_body.is_empty())
            .then(|| sorted_json(&Value::Object(self.extra_body.clone())).to_string());
        let provider_type = match self.provider_type {
            OpenAiProviderType::OpenAi => "openai",
            OpenAiProviderType::OpenAiCompatible => "openai-compatible",
        };
        crate::ai::cache_identity_with(
            &self.model,
            &[
                ("max_tokens", Some(max_tokens.as_str())),
                ("base_url", Some(self.base_url.as_str())),
                ("provider_type", Some(provider_type)),
                ("effort", self.effort.as_deref()),
                (
                    "send_reasoning_content",
                    self.send_reasoning_content.then_some("true"),
                ),
                ("extra_headers", extra_headers.as_deref()),
                ("extra_body", extra_body.as_deref()),
                (
                    "empty_tool_call_content",
                    self.send_empty_tool_call_content.then_some("true"),
                ),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiMessage, AiTool};
    use serde_json::json;
    use std::sync::atomic::Ordering;

    #[test]
    fn test_translate_request_system_and_user() -> Result<()> {
        let request = AiRequest {
            system: Some("You are helpful.".to_string()),
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
            reasoning_effort: None,
            conversation_id: None,
        };

        let openai_req =
            translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible, false)?;

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
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::User,
                    content: Some("Say hi.".to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
            conversation_id: None,
        };

        let openai_req =
            translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible, false)?;

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
            reasoning_effort: None,
            conversation_id: None,
        };

        let openai_req =
            translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible, false)?;

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
    fn test_translate_request_assistant_reasoning_content() -> Result<()> {
        let make_request = |thought: &str| AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::Assistant,
                content: None,
                thought: Some(thought.to_string()),
                thought_signature: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_123".to_string(),
                    function_name: "test_tool".to_string(),
                    arguments: json!({}),
                    thought_signature: None,
                }]),
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
            conversation_id: None,
        };

        let translate = |thought, send| {
            translate_ai_request(
                make_request(thought),
                4096,
                OpenAiProviderType::OpenAiCompatible,
                send,
            )
            .and_then(|req| Ok(serde_json::to_value(&req)?))
        };

        let body = translate("Need the file first.", true)?;
        assert_eq!(
            body["messages"][0]["reasoning_content"],
            "Need the file first."
        );
        assert!(body["messages"][0].get("reasoning").is_none());

        let body = translate("Need the file first.", false)?;
        assert!(body["messages"][0].get("reasoning_content").is_none());

        let body = translate("", true)?;
        assert!(body["messages"][0].get("reasoning_content").is_none());

        Ok(())
    }

    #[test]
    fn test_translate_response_reasoning_fields() -> Result<()> {
        let thought_of = |message: Value| -> Result<Option<String>> {
            let resp: OpenAiResponse = serde_json::from_value(json!({
                "choices": [{
                    "index": 0,
                    "message": message,
                    "finish_reason": "stop"
                }]
            }))?;
            Ok(translate_ai_response(resp)?.thought)
        };

        let base = json!({"role": "assistant", "content": "{}"});
        assert_eq!(thought_of(base.clone())?, None);

        let mut msg = base.clone();
        msg["reasoning_content"] = json!("Hmm.");
        assert_eq!(thought_of(msg)?.as_deref(), Some("Hmm."));

        let mut msg = base.clone();
        msg["reasoning"] = json!("Fallback.");
        assert_eq!(thought_of(msg.clone())?.as_deref(), Some("Fallback."));

        msg["reasoning_content"] = json!("Preferred.");
        assert_eq!(thought_of(msg.clone())?.as_deref(), Some("Preferred."));

        msg["reasoning_content"] = json!("");
        assert_eq!(thought_of(msg)?.as_deref(), Some("Fallback."));

        let mut msg = base.clone();
        msg["reasoning_content"] = json!("");
        assert_eq!(thought_of(msg)?, None);

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
                thought_signature: None,
                tool_calls: None,
                tool_call_id: Some("call_123".to_string()),
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
            conversation_id: None,
        };

        let openai_req =
            translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible, false)?;

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
    fn test_translate_request_tools_and_schemas() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![],
            tools: Some(vec![AiTool {
                name: "my_tool".to_string(),
                description: "Does something.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "mode": { "type": "string" }
                    }
                }),
            }]),
            temperature: None,
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
            conversation_id: None,
        };

        let openai_req =
            translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible, false)?;
        let tools = openai_req.tools.as_ref().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_type, "function");
        assert_eq!(tools[0].function.name, "my_tool");
        assert_eq!(tools[0].function.description, "Does something.");
        assert_eq!(tools[0].function.parameters["type"], "object");
        assert_eq!(
            tools[0].function.parameters["properties"]["mode"]["type"],
            "string"
        );

        let empty_tools_req = translate_ai_request(
            AiRequest {
                system: None,
                messages: vec![],
                tools: Some(vec![]),
                temperature: None,
                response_format: None,
                context_tag: None,
                reasoning_effort: None,
                conversation_id: None,
            },
            4096,
            OpenAiProviderType::OpenAiCompatible,
            false,
        )?;
        assert!(empty_tools_req.tools.is_none());

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
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::Assistant,
                    content: None,
                    thought: None,
                    thought_signature: None,
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
                    thought_signature: None,
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
            reasoning_effort: None,
            conversation_id: None,
        };

        let openai_req =
            translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible, false)?;

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
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: Some(AiResponseFormat::Json {
                schema: Some(schema.clone()),
            }),
            context_tag: None,
            reasoning_effort: None,
            conversation_id: None,
        };

        let openai_req =
            translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible, false)?;

        assert_eq!(
            openai_req.response_format,
            Some(json!({"type": "json_object"}))
        );
        assert_eq!(openai_req.messages[0].role, "user");
        assert_eq!(
            openai_req.messages[0].content,
            Some("Score this.\nRespond in JSON format.".to_string())
        );
        assert_eq!(openai_req.messages.len(), 1);

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
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: Some(AiResponseFormat::Json { schema: None }),
            context_tag: None,
            reasoning_effort: None,
            conversation_id: None,
        };

        let openai_req =
            translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible, false)?;

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
    fn test_json_hint_stays_on_first_user_across_turns() -> Result<()> {
        let user = |content: &str| AiMessage {
            role: AiRole::User,
            content: Some(content.to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        };
        let translate = |messages| {
            translate_ai_request(
                AiRequest {
                    system: None,
                    messages,
                    tools: None,
                    temperature: None,
                    response_format: Some(AiResponseFormat::Json { schema: None }),
                    context_tag: None,
                    reasoning_effort: None,
                    conversation_id: None,
                },
                4096,
                OpenAiProviderType::OpenAiCompatible,
                false,
            )
        };

        let first_turn = translate(vec![user("Inspect this.")])?;
        let next_turn = translate(vec![user("Inspect this."), user("Retry as JSON.")])?;
        assert_eq!(
            first_turn.messages[0].content,
            next_turn.messages[0].content
        );
        assert_eq!(
            next_turn.messages[1].content.as_deref(),
            Some("Retry as JSON.")
        );
        Ok(())
    }

    #[test]
    fn test_json_hint_adds_user_when_request_has_none() -> Result<()> {
        let request = AiRequest {
            system: Some("You are helpful.".to_string()),
            messages: vec![],
            tools: None,
            temperature: None,
            response_format: Some(AiResponseFormat::Json { schema: None }),
            context_tag: None,
            reasoning_effort: None,
            conversation_id: None,
        };

        let translated =
            translate_ai_request(request, 4096, OpenAiProviderType::OpenAiCompatible, false)?;
        assert_eq!(translated.messages[0].role, "system");
        assert_eq!(translated.messages[1].role, "user");
        assert_eq!(
            translated.messages[1].content.as_deref(),
            Some("Respond in JSON format.")
        );
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
                    reasoning_content: None,
                    reasoning: None,
                },
                finish_reason: "stop".to_string(),
            }],
            usage: OpenAiUsage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                prompt_tokens_details: None,
            },
            service_tier: None,
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
    fn test_translate_response_cached_token_handling() -> Result<()> {
        let make_resp = |prompt_tokens: u32, cached_tokens: u32| OpenAiResponse {
            choices: vec![OpenAiChoice {
                index: 0,
                message: OpenAiMessage {
                    role: "assistant".to_string(),
                    content: Some("Hello!".to_string()),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                    reasoning: None,
                },
                finish_reason: "stop".to_string(),
            }],
            usage: OpenAiUsage {
                prompt_tokens,
                completion_tokens: 20,
                total_tokens: prompt_tokens + 20,
                prompt_tokens_details: Some(OpenAiPromptTokensDetails {
                    cached_tokens: Some(cached_tokens),
                }),
            },
            service_tier: None,
        };

        // prompt_tokens stays whole: the cached count is a breakdown of it,
        // so uncached input is the difference.
        let usage = translate_ai_response(make_resp(2048, 1920))?.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 2048);
        assert_eq!(usage.cached_tokens, Some(1920));

        // Zero cached tokens maps to None.
        let usage = translate_ai_response(make_resp(10, 0))?.usage.unwrap();
        assert_eq!(usage.cached_tokens, None);

        // An endpoint reporting the prefix alongside prompt_tokens offers no
        // usable breakdown, so the whole prompt stays uncached input.
        let usage = translate_ai_response(make_resp(2048, 3000))?.usage.unwrap();
        assert_eq!(usage.cached_tokens, None);
        assert_eq!(usage.prompt_tokens, 2048);

        Ok(())
    }

    #[test]
    fn test_usage_and_response_deserialization_edge_cases() -> Result<()> {
        let usage: OpenAiUsage = serde_json::from_str(
            r#"{"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}"#,
        )?;
        assert!(usage.prompt_tokens_details.is_none());

        let usage: OpenAiUsage = serde_json::from_str(
            r#"{"prompt_tokens": 2048, "completion_tokens": 20, "total_tokens": 2068,
                "prompt_tokens_details": {"cached_tokens": 1920, "audio_tokens": 0}}"#,
        )?;
        assert_eq!(
            usage.prompt_tokens_details.and_then(|d| d.cached_tokens),
            Some(1920)
        );

        for details in [r#"{"cached_tokens": 1920.5}"#, r#""1920""#, "[]", "null"] {
            let body = format!(
                r#"{{"prompt_tokens": 2048, "completion_tokens": 20,
                     "total_tokens": 2068, "prompt_tokens_details": {details}}}"#
            );
            let usage: OpenAiUsage = serde_json::from_str(&body)?;
            let cached = usage.prompt_tokens_details.and_then(|d| d.cached_tokens);
            assert_eq!(cached, None, "{details}");
            assert_eq!(usage.prompt_tokens, 2048);
        }

        let resp: OpenAiResponse = serde_json::from_str(
            r#"{"choices": [{"index": 0, "finish_reason": "stop",
                 "message": {"role": "assistant", "content": "Hello!"}}]}"#,
        )?;
        assert_eq!(resp.choices[0].message.content.as_deref(), Some("Hello!"));
        assert_eq!(resp.usage.prompt_tokens, 0);
        assert_eq!(resp.usage.total_tokens, 0);

        let resp: OpenAiResponse = serde_json::from_str(
            r#"{"choices": [{"index": 0, "finish_reason": "stop",
                 "message": {"role": "assistant", "content": "Hello!"}}],
                 "usage": {"prompt_tokens": 10}}"#,
        )?;
        assert_eq!(resp.usage.prompt_tokens, 10);
        assert_eq!(resp.usage.completion_tokens, 0);

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
                    reasoning_content: None,
                    reasoning: None,
                },
                finish_reason: "tool_calls".to_string(),
            }],
            usage: OpenAiUsage {
                prompt_tokens: 15,
                completion_tokens: 25,
                total_tokens: 40,
                prompt_tokens_details: None,
            },
            service_tier: None,
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
                prompt_tokens_details: None,
            },
            service_tier: None,
        };

        let result = translate_ai_response(openai_resp);
        assert!(result.is_err());
    }

    #[test]
    fn test_max_tokens_field_by_provider_type() -> Result<()> {
        let request = || AiRequest {
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
            reasoning_effort: None,
            conversation_id: None,
        };

        let compat_req =
            translate_ai_request(request(), 4096, OpenAiProviderType::OpenAiCompatible, false)?;
        assert_eq!(compat_req.max_tokens, Some(4096));
        assert_eq!(compat_req.max_completion_tokens, None);
        let json = serde_json::to_value(&compat_req)?;
        assert_eq!(json["max_tokens"], 4096);
        assert!(json.get("max_completion_tokens").is_none());

        let openai_req = translate_ai_request(request(), 4096, OpenAiProviderType::OpenAi, false)?;
        assert_eq!(openai_req.max_tokens, None);
        assert_eq!(openai_req.max_completion_tokens, Some(4096));
        let json = serde_json::to_value(&openai_req)?;
        assert!(json.get("max_tokens").is_none());
        assert_eq!(json["max_completion_tokens"], 4096);

        Ok(())
    }

    fn test_client(base_url: &str, max_tokens: u32, effort: Option<&str>) -> OpenAiCompatClient {
        OpenAiCompatClient::new(
            base_url.to_string(),
            OpenAiProviderType::OpenAi,
            "gpt-5.1".to_string(),
            400_000,
            max_tokens,
            60,
            effort.map(str::to_string),
            None,
        )
        .unwrap()
    }

    #[test]
    fn cache_identity_tracks_max_tokens_and_base_url() {
        let capped = test_client("https://api.openai.com/v1", 4096, None);
        let raised = test_client("https://api.openai.com/v1", 65536, None);
        assert_ne!(capped.cache_identity(), raised.cache_identity());

        let elsewhere = test_client("http://localhost:1234/v1", 4096, None);
        assert_ne!(capped.cache_identity(), elsewhere.cache_identity());
    }

    #[test]
    fn cache_identity_tracks_extra_header_names_not_values() -> Result<()> {
        let headers = |value: &str| {
            std::collections::BTreeMap::from([("x-api-key".to_string(), value.to_string())])
        };
        let plain = test_client("https://api.openai.com/v1", 4096, None);
        let first = test_client("https://api.openai.com/v1", 4096, None)
            .with_extra_headers(&headers("secret-one"))?;
        let rotated = test_client("https://api.openai.com/v1", 4096, None)
            .with_extra_headers(&headers("secret-two"))?;
        assert_ne!(plain.cache_identity(), first.cache_identity());
        assert_eq!(first.cache_identity(), rotated.cache_identity());
        assert!(!first.cache_identity().contains("secret"));
        Ok(())
    }

    #[test]
    fn cache_identity_tracks_send_reasoning_content() {
        let off = test_client("https://api.openai.com/v1", 4096, None);
        let on = test_client("https://api.openai.com/v1", 4096, None).with_reasoning_content(true);
        assert_ne!(off.cache_identity(), on.cache_identity());
    }

    #[test]
    fn cache_identity_tracks_empty_tool_call_content() {
        let plain = test_client("https://api.openai.com/v1", 4096, None);
        let filled =
            test_client("https://api.openai.com/v1", 4096, None).with_empty_tool_call_content(true);
        assert_ne!(plain.cache_identity(), filled.cache_identity());
    }

    fn tool_call_request(content: Option<&str>) -> AiRequest {
        AiRequest {
            system: None,
            messages: vec![
                AiMessage {
                    role: AiRole::Assistant,
                    content: content.map(str::to_string),
                    thought: None,
                    thought_signature: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".to_string(),
                        function_name: "read_file".to_string(),
                        arguments: json!({}),
                        thought_signature: None,
                    }]),
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::Assistant,
                    content: None,
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
            conversation_id: None,
        }
    }

    #[test]
    fn tool_call_content_is_omitted_by_default() -> Result<()> {
        let client = test_client("https://api.openai.com/v1", 4096, None);
        let body = serde_json::to_value(client.build_request(tool_call_request(None))?)?;
        assert!(body["messages"][0].get("content").is_none());
        Ok(())
    }

    #[test]
    fn tool_call_content_is_filled_when_enabled() -> Result<()> {
        let client =
            test_client("https://api.openai.com/v1", 4096, None).with_empty_tool_call_content(true);
        let body = serde_json::to_value(client.build_request(tool_call_request(None))?)?;
        assert_eq!(body["messages"][0]["content"], "");
        // A message without tool calls keeps its missing content.
        assert!(body["messages"][1].get("content").is_none());

        let body =
            serde_json::to_value(client.build_request(tool_call_request(Some("Reading.")))?)?;
        assert_eq!(body["messages"][0]["content"], "Reading.");
        Ok(())
    }

    #[test]
    fn cache_identity_preserves_existing_format() {
        let client = test_client("https://api.openai.com/v1", 4096, None);
        assert_eq!(
            client.cache_identity(),
            "gpt-5.1|max_tokens=4096|base_url=https://api.openai.com/v1/chat/completions|provider_type=openai"
        );
    }

    /// Requests captured by a stub chat completions server.
    #[cfg(feature = "server")]
    type CapturedRequests = std::sync::Arc<tokio::sync::Mutex<Vec<(axum::http::HeaderMap, Value)>>>;

    /// Starts a stub chat completions server that records every request
    /// and answers it with a fixed reply. Returns the base URL to point a
    /// client at, the captured requests, and the server task.
    #[cfg(feature = "server")]
    async fn capturing_server() -> Result<(String, CapturedRequests, tokio::task::JoinHandle<()>)> {
        use axum::{Json, Router, http::HeaderMap, routing::post};

        let captured = CapturedRequests::default();
        let seen = std::sync::Arc::clone(&captured);
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let seen = std::sync::Arc::clone(&seen);
                async move {
                    seen.lock().await.push((headers, body));
                    Json(json!({"choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}]}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}/v1", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok((base_url, captured, server))
    }

    #[cfg(feature = "server")]
    fn empty_request() -> AiRequest {
        AiRequest {
            system: None,
            messages: vec![],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
            conversation_id: None,
        }
    }

    #[test]
    fn extra_headers_reject_invalid_names_and_values() {
        let headers = |name: &str, value: &str| {
            std::collections::BTreeMap::from([(name.to_string(), value.to_string())])
        };
        let client = || test_client("https://api.openai.com/v1", 4096, None);

        assert!(
            client()
                .with_extra_headers(&headers("x-api-key", "abc"))
                .is_ok()
        );
        assert!(
            client()
                .with_extra_headers(&headers("bad header", "abc"))
                .is_err()
        );
        assert!(
            client()
                .with_extra_headers(&headers("x-api-key", "a\nb"))
                .is_err()
        );
        let duplicated = std::collections::BTreeMap::from([
            ("X-Api-Key".to_string(), "a".to_string()),
            ("x-api-key".to_string(), "b".to_string()),
        ]);
        assert!(client().with_extra_headers(&duplicated).is_err());
    }

    #[test]
    fn extra_headers_read_their_values_from_the_environment() -> Result<()> {
        let vars_by_header = std::collections::BTreeMap::from([(
            "x-api-key".to_string(),
            "CLIENT_API_KEY".to_string(),
        )]);
        let env = |value: Option<&str>| {
            let value = value.map(str::to_string);
            move |var: &str| (var == "CLIENT_API_KEY").then(|| value.clone()).flatten()
        };

        let headers = extra_headers_from_env(&vars_by_header, env(Some("client-key")))?;
        assert_eq!(headers["x-api-key"], "client-key");

        for missing in [None, Some("")] {
            let error = extra_headers_from_env(&vars_by_header, env(missing))
                .expect_err("an unset or empty variable must fail")
                .to_string();
            assert!(error.contains("CLIENT_API_KEY"), "{error}");
        }
        Ok(())
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn extra_headers_are_sent_and_override_defaults() -> Result<()> {
        let (base_url, captured, server) = capturing_server().await?;
        let headers = std::collections::BTreeMap::from([
            ("x-api-key".to_string(), "client-key".to_string()),
            ("Authorization".to_string(), "Gateway token".to_string()),
        ]);
        let mut client = OpenAiCompatClient::new(
            base_url,
            OpenAiProviderType::OpenAiCompatible,
            "test-model".to_string(),
            8192,
            128,
            5,
            None,
            None,
        )?
        .with_extra_headers(&headers)?;
        // Stands in for the bearer token new() sets from the environment.
        client.client = reqwest::Client::builder()
            .default_headers(reqwest::header::HeaderMap::from_iter([(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_static("Bearer default"),
            )]))
            .build()?;

        client.generate_content(empty_request()).await?;

        let requests = captured.lock().await;
        let (headers, _) = &requests[0];
        assert_eq!(headers["x-api-key"], "client-key");
        assert_eq!(headers["authorization"], "Gateway token");
        assert_eq!(headers.get_all("authorization").iter().count(), 1);
        server.abort();
        Ok(())
    }

    #[test]
    fn reserved_request_fields_cover_every_request_field() -> Result<()> {
        let request = OpenAiRequest {
            model: String::new(),
            messages: vec![],
            tools: Some(vec![]),
            temperature: Some(0.0),
            max_tokens: Some(1),
            max_completion_tokens: Some(1),
            response_format: Some(json!({})),
            reasoning_effort: Some(String::new()),
            service_tier: Some(ServiceTier::Flex),
        };
        let body = serde_json::to_value(&request)?;
        let mut fields: Vec<&str> = body
            .as_object()
            .into_iter()
            .flat_map(|o| o.keys().map(String::as_str))
            .collect();
        let mut reserved = RESERVED_REQUEST_FIELDS.to_vec();
        fields.sort_unstable();
        reserved.sort_unstable();
        assert_eq!(fields, reserved);
        Ok(())
    }

    #[test]
    fn extra_body_rejects_reserved_fields() {
        let fields = |key: &str| {
            let mut map = serde_json::Map::new();
            map.insert(key.to_string(), json!("x"));
            map
        };
        let client = || test_client("https://api.openai.com/v1", 4096, None);

        assert!(client().with_extra_body(fields("client_name")).is_ok());
        assert!(client().with_extra_body(fields("model")).is_err());
        assert!(client().with_extra_body(fields("messages")).is_err());
        assert!(
            client()
                .with_extra_body(fields("reasoning_effort"))
                .is_err()
        );
    }

    #[test]
    fn cache_identity_tracks_extra_body_in_any_key_order() -> Result<()> {
        let body = |raw: &str| -> Result<serde_json::Map<String, Value>> {
            Ok(serde_json::from_str(raw)?)
        };
        let plain = test_client("https://api.openai.com/v1", 4096, None);
        let ab = test_client("https://api.openai.com/v1", 4096, None)
            .with_extra_body(body(r#"{"a": 1, "b": {"x": 1, "y": 2}}"#)?)?;
        let ba = test_client("https://api.openai.com/v1", 4096, None)
            .with_extra_body(body(r#"{"b": {"y": 2, "x": 1}, "a": 1}"#)?)?;
        assert_ne!(plain.cache_identity(), ab.cache_identity());
        assert_eq!(ab.cache_identity(), ba.cache_identity());
        Ok(())
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn extra_body_fields_are_merged_into_every_request() -> Result<()> {
        let (base_url, captured, server) = capturing_server().await?;
        let extra_body = serde_json::from_value(json!({
            "extraData": {"clientName": "sashiko", "sessionId": "review-1"}
        }))?;
        let client = OpenAiCompatClient::new(
            base_url,
            OpenAiProviderType::OpenAiCompatible,
            "test-model".to_string(),
            8192,
            128,
            5,
            None,
            None,
        )?
        .with_extra_body(extra_body)?;

        client.generate_content(empty_request()).await?;

        let requests = captured.lock().await;
        let (_, body) = &requests[0];
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["extraData"]["clientName"], "sashiko");
        assert_eq!(body["extraData"]["sessionId"], "review-1");
        server.abort();
        Ok(())
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn session_affinity_header_carries_the_conversation_id() -> Result<()> {
        let (base_url, captured, server) = capturing_server().await?;
        let client = |header: Option<&str>| -> Result<OpenAiCompatClient> {
            OpenAiCompatClient::new(
                base_url.clone(),
                OpenAiProviderType::OpenAiCompatible,
                "test-model".to_string(),
                8192,
                128,
                5,
                None,
                None,
            )?
            .with_session_affinity_header(header)
        };
        let request = |id: Option<uuid::Uuid>| AiRequest {
            conversation_id: id,
            ..empty_request()
        };
        let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());

        // One client handles multiple conversations.
        // Each request must use its own conversation's ID.
        // Requests without an ID must send no affinity header.
        let shared = client(Some("X-Session-Affinity"))?;
        for id in [Some(a), Some(b), Some(a), None] {
            shared.generate_content(request(id)).await?;
        }
        client(None)?.generate_content(request(Some(a))).await?;

        let requests = captured.lock().await;
        let sent: Vec<Option<&str>> = requests
            .iter()
            .map(|(headers, _)| {
                headers
                    .get("x-session-affinity")
                    .map(|v| v.to_str().unwrap())
            })
            .collect();
        let (header_a, header_b) = (
            format!("sashiko-conversation-{a}"),
            format!("sashiko-conversation-{b}"),
        );
        assert_eq!(
            sent,
            vec![
                Some(header_a.as_str()),
                Some(header_b.as_str()),
                Some(header_a.as_str()),
                None,
                None,
            ]
        );
        // The ID routes the request; it never reaches the prompt.
        for (_, body) in requests.iter() {
            let body = body.to_string();
            assert!(!body.contains(&a.to_string()) && !body.contains(&b.to_string()));
        }
        server.abort();
        Ok(())
    }

    /// Accepts the second answer, so a run makes a request and a retry.
    #[cfg(feature = "server")]
    struct RetryOnceSession {
        answers: usize,
    }

    #[cfg(feature = "server")]
    #[async_trait::async_trait]
    impl crate::ai::session::LlmSession for RetryOnceSession {
        type Output = ();

        fn system_prompt(&self) -> String {
            "system".to_string()
        }

        fn initial_user_prompt(&self) -> String {
            "review".to_string()
        }

        fn validate(
            &mut self,
            _response: &AiResponse,
        ) -> Result<(), crate::ai::session::ValidationError> {
            self.answers += 1;
            if self.answers < 2 {
                return Err(crate::ai::session::ValidationError::FormatViolation(
                    "retry".to_string(),
                ));
            }
            Ok(())
        }
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn configured_session_affinity_header_reaches_the_wire() -> Result<()> {
        let (base_url, captured, server) = capturing_server().await?;
        let mut settings = crate::settings::Settings::new()?;
        settings.ai.provider = "openai-compatible".to_string();
        settings.ai.model = "test-model".to_string();
        settings.ai.openai_compat = Some(crate::settings::OpenAiCompatSettings {
            base_url: Some(base_url),
            context_window_size: None,
            max_tokens: None,
            effort: None,
            send_reasoning_content: false,
            extra_headers: Default::default(),
            session_affinity_header: Some("x-session-affinity".to_string()),
            extra_body: Default::default(),
            send_empty_tool_call_content: false,
            api: None,
            service_tier: None,
        });
        let provider = crate::ai::create_provider(&settings)?;

        crate::ai::session::SessionRunner::new(provider.as_ref())
            .run(&mut RetryOnceSession { answers: 0 })
            .await?;
        crate::ai::session::SessionRunner::new(provider.as_ref())
            .run(&mut RetryOnceSession { answers: 0 })
            .await?;

        let requests = captured.lock().await;
        let sent: Vec<String> = requests
            .iter()
            .map(|(headers, _)| {
                headers
                    .get("x-session-affinity")
                    .expect("every request carries the header")
                    .to_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(sent.len(), 4, "two runs of a request and a retry each");
        assert!(sent[0].starts_with("sashiko-conversation-"), "{}", sent[0]);
        assert_eq!(sent[0], sent[1], "a retry stays in its conversation");
        assert_eq!(sent[2], sent[3]);
        assert_ne!(sent[0], sent[2], "each run is a new conversation");
        server.abort();
        Ok(())
    }

    #[test]
    fn session_affinity_header_rejects_invalid_and_duplicate_names() -> Result<()> {
        let client = || test_client("https://api.openai.com/v1", 4096, None);
        assert!(client().with_session_affinity_header(None).is_ok());
        assert!(
            client()
                .with_session_affinity_header(Some("bad header"))
                .is_err()
        );
        assert!(client().with_session_affinity_header(Some("")).is_err());

        let extra = std::collections::BTreeMap::from([(
            "X-Session-Affinity".to_string(),
            "value".to_string(),
        )]);
        let error = match client()
            .with_extra_headers(&extra)?
            .with_session_affinity_header(Some("x-session-affinity"))
        {
            Ok(_) => panic!("a header already in extra_headers must be refused"),
            Err(e) => e.to_string(),
        };
        assert!(error.contains("extra_headers"), "{error}");

        // The conflict is caught whichever setter runs first.
        let error = match client()
            .with_session_affinity_header(Some("x-session-affinity"))?
            .with_extra_headers(&extra)
        {
            Ok(_) => panic!("an extra header that is the affinity header must be refused"),
            Err(e) => e.to_string(),
        };
        assert!(error.contains("session_affinity_header"), "{error}");

        // Setting the name again replaces it rather than conflicting.
        assert!(
            client()
                .with_session_affinity_header(Some("x-session-affinity"))?
                .with_session_affinity_header(Some("x-session-affinity"))
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn cache_identity_ignores_session_affinity_header() -> Result<()> {
        let plain = test_client("https://api.openai.com/v1", 4096, None);
        let affine = test_client("https://api.openai.com/v1", 4096, None)
            .with_session_affinity_header(Some("x-session-affinity"))?;
        assert_eq!(plain.cache_identity(), affine.cache_identity());
        Ok(())
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn temperature_fallback_retries_once_and_remembers_rejection() -> Result<()> {
        use axum::{Json, Router, http::StatusCode, routing::post};
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests = Arc::clone(&captured);
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let requests = Arc::clone(&requests);
                async move {
                    let has_temperature = body.get("temperature").is_some();
                    requests.lock().await.push(body);
                    if has_temperature {
                        (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error": {"message": "Unsupported parameter: 'temperature' is not supported with this model."}})),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            Json(json!({"choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}]})),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}/v1", listener.local_addr()?);
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let client = OpenAiCompatClient::new(
            base_url,
            OpenAiProviderType::OpenAiCompatible,
            "test-model".to_string(),
            8192,
            128,
            5,
            None,
            None,
        )?;
        let request = AiRequest {
            system: None,
            messages: vec![],
            tools: None,
            temperature: Some(0.0),
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
            conversation_id: None,
        };

        client.generate_content(request.clone()).await?;
        client.generate_content(request).await?;
        let bodies = captured.lock().await;
        assert_eq!(bodies.len(), 3);
        assert_eq!(bodies[0]["temperature"], 0.0);
        assert!(bodies[1].get("temperature").is_none());
        assert!(bodies[2].get("temperature").is_none());
        server.abort();
        Ok(())
    }

    #[cfg(feature = "server")]
    #[tokio::test]
    async fn temperature_fallback_does_not_retry_other_requests() -> Result<()> {
        use axum::{Json, Router, http::StatusCode, routing::post};
        use std::sync::{Arc, atomic::AtomicUsize};

        async fn check(message: &str, temperature: Option<f32>) -> Result<()> {
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = Arc::clone(&calls);
            let error_message = message.to_string();
            let app = Router::new().route(
                "/v1/chat/completions",
                post(move || {
                    let seen = Arc::clone(&seen);
                    let error_message = error_message.clone();
                    async move {
                        seen.fetch_add(1, Ordering::Relaxed);
                        (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error": {"message": error_message}})),
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let base_url = format!("http://{}/v1", listener.local_addr()?);
            let server = tokio::spawn(async move { axum::serve(listener, app).await });
            let client = OpenAiCompatClient::new(
                base_url,
                OpenAiProviderType::OpenAiCompatible,
                "test-model".to_string(),
                8192,
                128,
                5,
                None,
                None,
            )?;
            let request = AiRequest {
                system: None,
                messages: vec![],
                tools: None,
                temperature,
                response_format: None,
                context_tag: None,
                reasoning_effort: None,
                conversation_id: None,
            };

            assert!(client.generate_content(request).await.is_err());
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            server.abort();
            Ok(())
        }

        check("Unsupported parameter: 'max_tokens'", Some(0.0)).await?;
        check("Unsupported parameter: 'temperature'", None).await?;
        Ok(())
    }

    #[test]
    fn cache_identity_tracks_effort() {
        let unset = test_client("https://api.openai.com/v1", 65536, None);
        let medium = test_client("https://api.openai.com/v1", 65536, Some("medium"));
        let high = test_client("https://api.openai.com/v1", 65536, Some("high"));

        assert_ne!(unset.cache_identity(), medium.cache_identity());
        assert_ne!(medium.cache_identity(), high.cache_identity());
    }

    fn sample_request() -> AiRequest {
        AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("hi".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
            conversation_id: None,
        }
    }

    #[test]
    fn configured_effort_reaches_the_request() {
        let client = test_client("https://api.openai.com/v1", 65536, Some("high"));

        let built = client.build_request(sample_request()).unwrap();
        assert_eq!(built.reasoning_effort.as_deref(), Some("high"));

        let json = serde_json::to_value(&built).unwrap();
        assert_eq!(json["reasoning_effort"], "high");
    }

    #[test]
    fn configured_service_tier_reaches_the_request() {
        let mut client = test_client("https://api.openai.com/v1", 65536, None);
        let built = client.build_request(sample_request()).unwrap();
        let json = serde_json::to_value(&built).unwrap();
        assert!(json.get("service_tier").is_none());

        client.service_tier = Some(ServiceTier::Flex);
        let built = client.build_request(sample_request()).unwrap();
        let json = serde_json::to_value(&built).unwrap();
        assert_eq!(json["service_tier"], "flex");
    }

    fn request_at_zero_temperature() -> AiRequest {
        let mut request = sample_request();
        request.temperature = Some(0.0);
        request
    }

    use crate::ai::openai_common::tests::{TEMPERATURE_REFUSED, serve_replies};

    const COMPLETION: &str = r#"{"id":"x","object":"chat.completion","created":0,"model":"gpt-6-luna","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;

    #[tokio::test]
    async fn temperature_refusal_resends_without_the_field() {
        let (base_url, seen) = serve_replies(vec![
            (400, TEMPERATURE_REFUSED.to_string()),
            (200, COMPLETION.to_string()),
        ])
        .await;
        let client = test_client(&base_url, 65536, None);

        let response = client
            .generate_content(request_at_zero_temperature())
            .await
            .unwrap();
        assert_eq!(response.content.as_deref(), Some("ok"));

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0]["temperature"], 0.0);
        assert!(seen[1].get("temperature").is_none());
        assert!(client.temperature_unsupported.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn other_bad_request_is_not_resent() {
        let refused_top_p =
            TEMPERATURE_REFUSED.replace(r#""param": "temperature""#, r#""param": "top_p""#);
        let (base_url, seen) = serve_replies(vec![(400, refused_top_p)]).await;
        let client = test_client(&base_url, 65536, None);

        assert!(
            client
                .generate_content(request_at_zero_temperature())
                .await
                .is_err()
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(!client.temperature_unsupported.load(Ordering::Relaxed));
    }

    #[test]
    fn reasoning_effort_omitted_unless_configured() {
        let unset = test_client("https://api.openai.com/v1", 65536, None);
        let built = unset.build_request(sample_request()).unwrap();
        assert_eq!(built.reasoning_effort, None);

        let translated =
            translate_ai_request(sample_request(), 4096, OpenAiProviderType::OpenAi, false)
                .unwrap();
        assert_eq!(translated.reasoning_effort, None);

        let json = serde_json::to_value(&translated).unwrap();
        assert!(json.get("reasoning_effort").is_none());
    }
}
