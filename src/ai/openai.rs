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
    NewAssembler, OpenAiCompatError, StreamAssembler, build_http_client, event_json,
    lenient_option, normalize_base_url, post_json, status_error, stream_protocol_error,
};
use crate::ai::sse::{Flow, SseEvent};
use crate::ai::{
    AiProvider, AiRequest, AiResponse, AiResponseFormat, AiRole, AiUsage, ProviderCapabilities,
    ToolCall,
};
use crate::settings::ServiceTier;
use crate::utils::redact_secret;
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
    /// Ask for an event stream, which keeps bytes flowing while the model
    /// works; see ChatStream.
    stream: bool,
    client: Client,
}

/// Fields a streamed request carries on top of the ones it shares with a
/// plain one.  `stream_options.include_usage` is what makes a stream
/// report token counts at all.
const STREAM_REQUEST_FIELDS: [&str; 2] = ["stream", "stream_options"];

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
            stream: false,
        })
    }

    /// Asks for each reply as an event stream rather than one document.
    pub fn with_stream(mut self, stream: bool) -> Result<Self> {
        self.stream = stream;
        self.check_stream_fields()?;
        Ok(self)
    }

    /// A streamed request sets its stream fields itself, and extra_body
    /// overriding them would change what the reply looks like.  Checked
    /// only when streaming, so a plain request keeps accepting them.
    fn check_stream_fields(&self) -> Result<()> {
        if !self.stream {
            return Ok(());
        }
        if let Some(field) = STREAM_REQUEST_FIELDS
            .iter()
            .find(|field| self.extra_body.contains_key(**field))
        {
            anyhow::bail!("extra_body may not set {field:?} when stream is enabled");
        }
        Ok(())
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
        self.check_stream_fields()?;
        Ok(self)
    }

    fn request_body(&self, request: &OpenAiRequest) -> Result<Value> {
        let mut body = serde_json::to_value(request)?;
        if let Some(object) = body.as_object_mut() {
            object.extend(self.extra_body.clone());
            if self.stream {
                object.insert("stream".to_string(), Value::Bool(true));
                object.insert(
                    "stream_options".to_string(),
                    serde_json::json!({"include_usage": true}),
                );
            }
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
            self.stream.then_some(ChatStream::boxed as NewAssembler),
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

/// Rebuilds a streamed chat completion from its chunks.
///
/// Each chunk carries deltas for one or more choices.  Text fields are
/// concatenated; tool calls are keyed by their `index`, take their id,
/// type and name from the first delta that has them, and concatenate
/// their argument fragments.  `finish_reason`, `usage` and `service_tier`
/// keep the last value sent.  `data: [DONE]` ends the stream; one that
/// ends without it was cut off.
///
/// The rebuilt document is what the endpoint returns without streaming,
/// so a field that is missing from every chunk is missing from it too and
/// fails to decode exactly as the plain reply would.
#[derive(Debug, Default)]
struct ChatStream {
    choices: std::collections::BTreeMap<u64, ChoiceDeltas>,
    usage: Option<Value>,
    service_tier: Option<Value>,
}

#[derive(Debug, Default)]
struct ChoiceDeltas {
    role: Option<String>,
    content: String,
    reasoning_content: String,
    reasoning: String,
    tool_calls: ToolCalls,
    finish_reason: Option<Value>,
}

/// The tool calls of one choice, in the order their first fragment came.
#[derive(Debug, Default)]
struct ToolCalls {
    calls: Vec<ToolCallDeltas>,
    /// Which call each wire `index` currently names.
    by_index: std::collections::BTreeMap<u64, usize>,
}

impl ToolCalls {
    /// The call a delta continues, or a new one.  OpenAI tags every delta
    /// with the call's `index`.  Some compatible servers leave the index
    /// out and send each call whole, and some give every call index 0, so
    /// a delta naming an id other than the one its slot already has starts
    /// a new call, and a delta without an index continues the last call.
    fn slot(&mut self, index: Option<u64>, id: Option<&str>) -> &mut ToolCallDeltas {
        let starts_new = |call: &ToolCallDeltas| {
            id.is_some_and(|id| call.id.as_deref().is_some_and(|known| known != id))
        };
        let existing = match index {
            Some(index) => self.by_index.get(&index).copied(),
            None => self.calls.len().checked_sub(1),
        }
        .filter(|&position| !starts_new(&self.calls[position]));
        let position = existing.unwrap_or_else(|| {
            self.calls.push(ToolCallDeltas::default());
            self.calls.len() - 1
        });
        if let Some(index) = index {
            self.by_index.insert(index, position);
        }
        &mut self.calls[position]
    }
}

#[derive(Debug, Default)]
struct ToolCallDeltas {
    id: Option<String>,
    call_type: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ChatStream {
    fn boxed() -> Box<dyn StreamAssembler> {
        Box::new(Self::default())
    }

    fn on_choice(&mut self, choice: &Value) -> Result<(), OpenAiCompatError> {
        // A server that sends one choice may leave out its index.
        let index = match choice.get("index") {
            None | Some(Value::Null) => 0,
            Some(index) => index
                .as_u64()
                .ok_or_else(|| stream_protocol_error("choice index is not a number"))?,
        };
        let deltas = self.choices.entry(index).or_default();
        if let Some(reason) = choice.get("finish_reason").filter(|r| !r.is_null()) {
            deltas.finish_reason = Some(reason.clone());
        }
        let Some(delta) = choice.get("delta").filter(|d| !d.is_null()) else {
            return Ok(());
        };
        set_once(&mut deltas.role, delta.get("role"), "role")?;
        append(&mut deltas.content, delta.get("content"), "content")?;
        append(
            &mut deltas.reasoning_content,
            delta.get("reasoning_content"),
            "reasoning_content",
        )?;
        append(&mut deltas.reasoning, delta.get("reasoning"), "reasoning")?;
        match delta.get("tool_calls") {
            None | Some(Value::Null) => Ok(()),
            Some(Value::Array(calls)) => calls
                .iter()
                .try_for_each(|call| on_tool_call(&mut deltas.tool_calls, call)),
            Some(_) => Err(stream_protocol_error("tool_calls delta is not an array")),
        }
    }
}

fn on_tool_call(calls: &mut ToolCalls, call: &Value) -> Result<(), OpenAiCompatError> {
    let index = match call.get("index") {
        None | Some(Value::Null) => None,
        Some(index) => Some(
            index
                .as_u64()
                .ok_or_else(|| stream_protocol_error("tool call index is not a number"))?,
        ),
    };
    let id = call
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let deltas = calls.slot(index, id);
    set_once(&mut deltas.id, call.get("id"), "tool call id")?;
    set_once(&mut deltas.call_type, call.get("type"), "tool call type")?;
    if let Some(function) = call.get("function").filter(|f| !f.is_null()) {
        set_once(&mut deltas.name, function.get("name"), "tool call name")?;
        append(
            &mut deltas.arguments,
            function.get("arguments"),
            "tool call arguments",
        )?;
    }
    Ok(())
}

/// Takes a field the first chunk carrying it sets.  Some servers repeat
/// it on every chunk, which is accepted; a different value is not, since
/// neither can be the right one.  An empty string counts as absent: some
/// servers send `"id": ""` on every fragment after the first.
fn set_once(
    slot: &mut Option<String>,
    value: Option<&Value>,
    what: &str,
) -> Result<(), OpenAiCompatError> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(value)) if value.is_empty() => Ok(()),
        Some(Value::String(value)) => match slot {
            Some(existing) if existing != value => Err(stream_protocol_error(format_args!(
                "{what} changed from {existing:?} to {value:?}"
            ))),
            Some(_) => Ok(()),
            None => {
                *slot = Some(value.clone());
                Ok(())
            }
        },
        Some(_) => Err(stream_protocol_error(format_args!(
            "{what} is not a string"
        ))),
    }
}

fn append(slot: &mut String, value: Option<&Value>, what: &str) -> Result<(), OpenAiCompatError> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(fragment)) => {
            slot.push_str(fragment);
            Ok(())
        }
        Some(_) => Err(stream_protocol_error(format_args!(
            "{what} is not a string"
        ))),
    }
}

/// An error a server reports inside the stream, typed like the HTTP
/// status it stands for.  Servers name that status as a numeric code, as
/// a numeric string, or only by kind: OpenAI sends `type: "server_error"`
/// with a null code for a failure a plain request would get as a 500.
/// An error none of these place is fatal.
///
/// The text keeps the `{"error": ...}` envelope a non-streamed error body
/// carries, because status_error and rejects_temperature_parameter read
/// the quota and temperature refusal fields from under that key.
fn stream_error(error: &Value) -> OpenAiCompatError {
    let text = redact_secret(&serde_json::json!({ "error": error }).to_string());
    match stream_error_status(error) {
        Some(status) => status_error(status, None, text),
        None => {
            tracing::error!("OpenAI stream reported an error: {}", text);
            OpenAiCompatError::ResponseFailed(text)
        }
    }
}

fn stream_error_status(error: &Value) -> Option<reqwest::StatusCode> {
    use reqwest::StatusCode;
    let code = error.get("code");
    let numeric = code
        .and_then(|code| {
            code.as_u64()
                .or_else(|| code.as_str().and_then(|text| text.trim().parse().ok()))
        })
        .and_then(|code| u16::try_from(code).ok())
        .and_then(|code| StatusCode::from_u16(code).ok())
        .filter(|status| status.is_client_error() || status.is_server_error());
    if numeric.is_some() {
        return numeric;
    }
    [code, error.get("type")]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find_map(|kind| match kind {
            "invalid_request_error" | "unsupported_value" | "unsupported_parameter" => {
                Some(StatusCode::BAD_REQUEST)
            }
            "server_error" | "api_error" | "internal_error" | "internal_server_error" => {
                Some(StatusCode::INTERNAL_SERVER_ERROR)
            }
            "overloaded_error" | "service_unavailable" => Some(StatusCode::SERVICE_UNAVAILABLE),
            "rate_limit_exceeded" | "rate_limit_error" => Some(StatusCode::TOO_MANY_REQUESTS),
            _ => None,
        })
}

impl StreamAssembler for ChatStream {
    fn terminal_event(&self) -> &'static str {
        "[DONE]"
    }

    /// Some servers close the stream after the last finish_reason without
    /// sending `[DONE]`.  Every choice having finished means the generation
    /// did; only a usage chunk can still be missing.  A finish reason of
    /// "error", which OpenRouter sends for a generation that failed, does
    /// not count.
    fn complete_at_eof(&self) -> bool {
        !self.choices.is_empty()
            && self.choices.values().all(|choice| {
                choice
                    .finish_reason
                    .as_ref()
                    .is_some_and(|reason| reason.as_str() != Some("error"))
            })
    }

    fn on_event(&mut self, event: SseEvent) -> Result<Flow, OpenAiCompatError> {
        if event.data == "[DONE]" {
            return Ok(Flow::Done);
        }
        let chunk = event_json(&event)?;
        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            return Err(stream_error(error));
        }
        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(usage.clone());
        }
        if let Some(tier) = chunk.get("service_tier").filter(|t| !t.is_null()) {
            self.service_tier = Some(tier.clone());
        }
        match chunk.get("choices") {
            None | Some(Value::Null) => Ok(Flow::Continue),
            Some(Value::Array(choices)) => {
                for choice in choices {
                    self.on_choice(choice)?;
                }
                Ok(Flow::Continue)
            }
            Some(_) => Err(stream_protocol_error("choices is not an array")),
        }
    }

    fn into_document(self: Box<Self>) -> Value {
        let choices: Vec<Value> = self
            .choices
            .into_iter()
            .map(|(index, deltas)| deltas.into_choice(index))
            .collect();
        let mut document = serde_json::Map::new();
        document.insert("choices".to_string(), Value::Array(choices));
        if let Some(usage) = self.usage {
            document.insert("usage".to_string(), usage);
        }
        if let Some(tier) = self.service_tier {
            document.insert("service_tier".to_string(), tier);
        }
        Value::Object(document)
    }
}

impl ChoiceDeltas {
    /// The choice as a plain reply carries it.  Text that never arrived is
    /// left out rather than sent empty: a plain reply that only calls
    /// tools has no content, while the first chunk of its stream carries
    /// an empty one.
    fn into_choice(self, index: u64) -> Value {
        let mut message = serde_json::Map::new();
        message.insert(
            "role".to_string(),
            Value::String(self.role.unwrap_or_else(|| "assistant".to_string())),
        );
        for (name, text) in [
            ("content", self.content),
            ("reasoning_content", self.reasoning_content),
            ("reasoning", self.reasoning),
        ] {
            if !text.is_empty() {
                message.insert(name.to_string(), Value::String(text));
            }
        }
        if !self.tool_calls.calls.is_empty() {
            let calls = self
                .tool_calls
                .calls
                .into_iter()
                .map(ToolCallDeltas::into_call)
                .collect();
            message.insert("tool_calls".to_string(), Value::Array(calls));
        }
        let mut choice = serde_json::Map::new();
        choice.insert("index".to_string(), Value::from(index));
        choice.insert("message".to_string(), Value::Object(message));
        if let Some(reason) = self.finish_reason {
            choice.insert("finish_reason".to_string(), reason);
        }
        Value::Object(choice)
    }
}

impl ToolCallDeltas {
    fn into_call(self) -> Value {
        let mut function = serde_json::Map::new();
        if let Some(name) = self.name {
            function.insert("name".to_string(), Value::String(name));
        }
        function.insert("arguments".to_string(), Value::String(self.arguments));
        let mut call = serde_json::Map::new();
        if let Some(id) = self.id {
            call.insert("id".to_string(), Value::String(id));
        }
        call.insert(
            "type".to_string(),
            Value::String(self.call_type.unwrap_or_else(|| "function".to_string())),
        );
        call.insert("function".to_string(), Value::Object(function));
        Value::Object(call)
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

    const PLAIN_TEXT: &str = r#"{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"gpt-5.1","choices":[{"index":0,"message":{"role":"assistant","content":"Hello there","reasoning_content":"Thinking it over","refusal":null},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":4}},"service_tier":"default"}"#;

    const STREAMED_TEXT: &[&str] = &[
        r#"{"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"","refusal":null},"finish_reason":null}],"usage":null,"service_tier":"default"}"#,
        r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"reasoning_content":"Thinking "},"finish_reason":null}]}"#,
        r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"reasoning_content":"it over"}}]}"#,
        r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"}}]}"#,
        r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"content":" there"}}]}"#,
        r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        r#"{"id":"chatcmpl-1","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":4}},"service_tier":"default"}"#,
    ];

    const PLAIN_TOOL_CALLS: &str = r#"{"choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_a","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.rs\"}"}},{"id":"call_b","type":"function","function":{"name":"git_log","arguments":"{\"n\":3}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":20,"completion_tokens":9,"total_tokens":29}}"#;

    /// Two calls whose argument fragments interleave, the way a server
    /// streaming parallel tool calls sends them.
    const STREAMED_TOOL_CALLS: &[&str] = &[
        r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read_file","arguments":""}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_b","type":"function","function":{"name":"git_log","arguments":""}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"arguments":"th\":\"a.rs\"}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"name":"git_log","arguments":"{\"n\":3}"}}]}}]}"#,
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":9,"total_tokens":29}}"#,
    ];

    fn chunk_stream(chunks: &[&str]) -> String {
        let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        body.push_str("data: [DONE]\n\n");
        body
    }

    async fn chat(
        stream: bool,
        replies: Vec<(u16, &'static str, String)>,
    ) -> (Result<AiResponse>, Vec<Value>) {
        use crate::ai::openai_common::tests::serve_typed_replies;
        let (base_url, seen) = serve_typed_replies(replies).await;
        let client = test_client(&base_url, 4096, None)
            .with_stream(stream)
            .unwrap();
        let response = client.generate_content(sample_request()).await;
        let seen = seen.lock().unwrap().clone();
        (response, seen)
    }

    async fn chat_streamed(body: String) -> Result<AiResponse> {
        chat(true, vec![(200, "text/event-stream", body)]).await.0
    }

    /// A streamed reply reaches the stage as exactly the response the
    /// plain reply to the same request does.
    #[tokio::test]
    async fn a_streamed_chat_reply_matches_the_plain_one() {
        for (plain, chunks) in [
            (PLAIN_TEXT, STREAMED_TEXT),
            (PLAIN_TOOL_CALLS, STREAMED_TOOL_CALLS),
        ] {
            let (plain_response, plain_seen) =
                chat(false, vec![(200, "application/json", plain.to_string())]).await;
            let (streamed_response, streamed_seen) =
                chat(true, vec![(200, "text/event-stream", chunk_stream(chunks))]).await;

            let plain_response = plain_response.unwrap();
            assert_eq!(
                serde_json::to_value(streamed_response.unwrap()).unwrap(),
                serde_json::to_value(&plain_response).unwrap()
            );
            assert!(plain_response.usage.unwrap().prompt_tokens > 0);

            assert!(plain_seen[0].get("stream").is_none());
            assert!(plain_seen[0].get("stream_options").is_none());
            assert_eq!(streamed_seen[0]["stream"], true);
            assert_eq!(
                streamed_seen[0]["stream_options"],
                json!({"include_usage": true})
            );
            let mut unstreamed = streamed_seen[0].clone();
            let fields = unstreamed.as_object_mut().unwrap();
            fields.remove("stream");
            fields.remove("stream_options");
            assert_eq!(unstreamed, plain_seen[0], "only the stream fields differ");
        }
    }

    #[tokio::test]
    async fn a_streamed_tool_call_reply_has_no_content() {
        let response = chat_streamed(chunk_stream(STREAMED_TOOL_CALLS))
            .await
            .unwrap();
        assert_eq!(response.content, None);
        let calls = response.tool_calls.unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].arguments, json!({"path": "a.rs"}));
        assert_eq!(calls[1].function_name, "git_log");
        assert_eq!(calls[1].arguments, json!({"n": 3}));
    }

    #[tokio::test]
    async fn a_plain_reply_to_a_streamed_chat_request_is_accepted() {
        let (response, seen) = chat(
            true,
            vec![(200, "application/json", PLAIN_TEXT.to_string())],
        )
        .await;
        assert_eq!(response.unwrap().content.as_deref(), Some("Hello there"));
        assert_eq!(seen[0]["stream"], true);
    }

    #[tokio::test]
    async fn a_chat_stream_cut_before_it_finished_is_transient() {
        let body: String = STREAMED_TEXT[..5]
            .iter()
            .map(|c| format!("data: {c}\n\n"))
            .collect();
        let err = chat_streamed(body).await.expect_err("a cut stream fails");
        assert!(
            err.to_string().contains("stream ended before [DONE]"),
            "{err:#}"
        );
        assert!(matches!(
            crate::ai::classify_ai_error(&err),
            crate::ai::AiErrorClass::Transient { .. }
        ));
    }

    /// A server that closes after the last finish_reason without [DONE]
    /// has sent the whole generation.
    #[tokio::test]
    async fn a_finished_chat_stream_without_done_is_accepted() {
        let body: String = STREAMED_TEXT
            .iter()
            .map(|c| format!("data: {c}\n\n"))
            .collect();
        let (plain, _) = chat(
            false,
            vec![(200, "application/json", PLAIN_TEXT.to_string())],
        )
        .await;
        assert_eq!(
            serde_json::to_value(chat_streamed(body).await.unwrap()).unwrap(),
            serde_json::to_value(plain.unwrap()).unwrap()
        );

        let without_usage: String = STREAMED_TEXT[..6]
            .iter()
            .map(|c| format!("data: {c}\n\n"))
            .collect();
        let response = chat_streamed(without_usage).await.unwrap();
        assert_eq!(response.content.as_deref(), Some("Hello there"));
    }

    #[tokio::test]
    async fn an_error_finish_without_done_is_not_complete() {
        let chunks = [
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"par"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"error"}]}"#,
        ];
        let body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        let err = chat_streamed(body).await.expect_err("a failed generation");
        assert!(matches!(
            crate::ai::classify_ai_error(&err),
            crate::ai::AiErrorClass::Transient { .. }
        ));
    }

    #[tokio::test]
    async fn a_length_stop_in_a_stream_is_truncated() {
        let chunks = [
            r#"{"choices":[{"index":0,"delta":{"role":"assistant","content":"cut"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#,
        ];
        assert!(
            chat_streamed(chunk_stream(&chunks))
                .await
                .unwrap()
                .truncated
        );
    }

    /// A choice that never names a finish reason fails to decode, as the
    /// same plain reply would.
    #[tokio::test]
    async fn a_stream_without_a_finish_reason_fails_like_the_plain_reply() {
        let chunks = [r#"{"choices":[{"index":0,"delta":{"content":"hi"}}]}"#];
        let err = chat_streamed(chunk_stream(&chunks))
            .await
            .expect_err("no finish_reason");
        assert!(err.to_string().contains("finish_reason"), "{err:#}");
        assert!(matches!(
            crate::ai::classify_ai_error(&err),
            crate::ai::AiErrorClass::Fatal
        ));
    }

    #[tokio::test]
    async fn an_error_chunk_is_classified_like_its_status() {
        let cases = [
            (json!({"code": 429}), "rate limit"),
            (json!({"code": 503}), "transient"),
            (json!({"code": "502"}), "transient"),
            (json!({"code": 400}), "fatal"),
            (json!({"code": null, "type": "server_error"}), "transient"),
            (json!({"code": "server_error"}), "transient"),
            (json!({"code": "rate_limit_exceeded"}), "rate limit"),
            (
                json!({"code": "insufficient_quota", "type": "insufficient_quota"}),
                "fatal",
            ),
            (json!({"code": 429, "type": "insufficient_quota"}), "fatal"),
            (json!({"code": "context_length_exceeded"}), "fatal"),
            (json!({"code": null}), "fatal"),
        ];
        for (mut error, expected) in cases {
            error["message"] = json!("provider said no");
            let chunk = json!({"error": error}).to_string();
            let chunks = [STREAMED_TEXT[0], &chunk];
            let err = chat_streamed(chunk_stream(&chunks))
                .await
                .expect_err("an error chunk fails");
            let class = crate::ai::classify_ai_error(&err);
            let matches = match expected {
                "rate limit" => matches!(class, crate::ai::AiErrorClass::RateLimit { .. }),
                "transient" => matches!(class, crate::ai::AiErrorClass::Transient { .. }),
                _ => matches!(class, crate::ai::AiErrorClass::Fatal),
            };
            assert!(matches, "{error}: {class:?} for {err:#}");
        }
    }

    /// A temperature refusal sent inside the stream drops the field and
    /// resends, exactly like the same refusal sent as an HTTP error body.
    #[tokio::test]
    async fn an_in_stream_temperature_refusal_resends_without_it() {
        use crate::ai::openai_common::tests::serve_typed_replies;
        let errors = [
            json!({"code": 400, "type": "invalid_request_error"}),
            json!({"code": null, "type": "invalid_request_error"}),
            json!({"code": "unsupported_parameter"}),
            json!({
                "code": "unsupported_value",
                "message": "The requested 'temperature' is not supported with this model.",
            }),
        ];
        for mut error in errors {
            error["param"] = json!("temperature");
            let refusal = json!({"error": error}).to_string();
            let (base_url, seen) = serve_typed_replies(vec![
                (200, "text/event-stream", chunk_stream(&[&refusal])),
                (200, "text/event-stream", chunk_stream(STREAMED_TEXT)),
            ])
            .await;
            let client = test_client(&base_url, 4096, None)
                .with_stream(true)
                .unwrap();
            let response = client
                .generate_content(request_at_zero_temperature())
                .await
                .unwrap_or_else(|err| panic!("the retry for {error} failed: {err:#}"));
            let seen = seen.lock().unwrap();
            assert_eq!(response.content.as_deref(), Some("Hello there"));
            assert_eq!(seen.len(), 2);
            assert_eq!(seen[0]["temperature"], 0.0);
            assert!(seen[1].get("temperature").is_none());
        }
    }

    #[tokio::test]
    async fn a_malformed_chat_stream_is_fatal() {
        let cases = [
            "not json",
            r#"{"choices":{}}"#,
            r#"{"choices":[{"index":"0","delta":{}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":7}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":"0","id":"call_a"}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"role":"user"}}]}"#,
        ];
        for bad in cases {
            let chunks = [STREAMED_TEXT[0], bad];
            let err = chat_streamed(chunk_stream(&chunks))
                .await
                .expect_err("a malformed stream fails");
            assert!(err.to_string().contains("Stream error"), "{bad}: {err:#}");
            assert!(
                matches!(
                    crate::ai::classify_ai_error(&err),
                    crate::ai::AiErrorClass::Fatal
                ),
                "{bad}: {err:#}"
            );
        }
    }

    async fn streamed_calls(chunks: &[&str]) -> Vec<(String, String, Value)> {
        chat_streamed(chunk_stream(chunks))
            .await
            .unwrap()
            .tool_calls
            .unwrap()
            .into_iter()
            .map(|call| (call.id, call.function_name, call.arguments))
            .collect()
    }

    /// A server that gives every call index 0 is told apart by the ids.
    #[tokio::test]
    async fn a_reused_index_with_a_new_id_starts_a_new_call() {
        let calls = streamed_calls(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"f","arguments":"{\"a\""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":":1}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_b","type":"function","function":{"name":"g","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ])
        .await;
        assert_eq!(
            calls,
            [
                ("call_a".to_string(), "f".to_string(), json!({"a": 1})),
                ("call_b".to_string(), "g".to_string(), json!({})),
            ]
        );
    }

    /// Some servers leave the index out and send each call whole.
    #[tokio::test]
    async fn calls_without_an_index_are_kept_apart_by_id() {
        let calls = streamed_calls(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"id":"call_a","type":"function","function":{"name":"f","arguments":"{\"a\":1}"}},{"id":"call_b","type":"function","function":{"name":"g","arguments":"{}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ])
        .await;
        assert_eq!(
            calls,
            [
                ("call_a".to_string(), "f".to_string(), json!({"a": 1})),
                ("call_b".to_string(), "g".to_string(), json!({})),
            ]
        );
    }

    /// Some servers repeat id, type and name as empty strings on every
    /// fragment after the first, and some send an empty id first.
    #[tokio::test]
    async fn empty_identity_fields_in_fragments_are_ignored() {
        let calls = streamed_calls(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"","type":"","function":{"name":"","arguments":""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"f","arguments":"{\"a\""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"","type":"","function":{"name":"","arguments":":1}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        ])
        .await;
        assert_eq!(
            calls,
            [("call_a".to_string(), "f".to_string(), json!({"a": 1}))]
        );
    }

    #[tokio::test]
    async fn a_tool_call_whose_name_changes_is_fatal() {
        let chunks = [
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"f","arguments":""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"g","arguments":"{}"}}]}}]}"#,
        ];
        let err = chat_streamed(chunk_stream(&chunks))
            .await
            .expect_err("the name changed");
        assert!(
            err.to_string().contains("tool call name changed"),
            "{err:#}"
        );
    }

    #[test]
    fn stream_fields_in_extra_body_are_refused_only_when_streaming() -> Result<()> {
        let fields = |key: &str| {
            let mut map = serde_json::Map::new();
            map.insert(key.to_string(), json!(false));
            map
        };
        let client = || test_client("https://api.openai.com/v1", 4096, None);
        for field in STREAM_REQUEST_FIELDS {
            assert!(client().with_extra_body(fields(field)).is_ok());
            assert!(
                client()
                    .with_stream(false)?
                    .with_extra_body(fields(field))
                    .is_ok()
            );
            assert!(
                client()
                    .with_stream(true)?
                    .with_extra_body(fields(field))
                    .is_err()
            );
            assert!(
                client()
                    .with_extra_body(fields(field))?
                    .with_stream(true)
                    .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn the_stream_flag_stays_out_of_the_cache_identity() -> Result<()> {
        let plain = test_client("https://api.openai.com/v1", 4096, None);
        let streamed = test_client("https://api.openai.com/v1", 4096, None).with_stream(true)?;
        assert_eq!(plain.cache_identity(), streamed.cache_identity());
        Ok(())
    }
}
