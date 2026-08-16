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

//! OpenAI's `/v1/responses` endpoint.
//!
//! The chat transport in `openai.rs` serves every OpenAI-compatible vendor
//! sashiko supports and stays the default.  This one reaches what that
//! endpoint cannot: the gpt-5.6 family refuses a request carrying function
//! tools unless reasoning is off, and a project whose gpt-5.4 grant is
//! scoped to this endpoint gets "does not have access" from the other one.
//!
//! See designs/DESIGN_OPENAI_RESPONSES_TRANSPORT.md.

use crate::ai::openai_common::{
    OpenAiCompatError, build_http_client, lenient_option, normalize_base_url, post_json,
};
use crate::ai::{
    AiMessage, AiProvider, AiRequest, AiResponse, AiResponseFormat, AiRole, AiTool, AiUsage,
    ProviderCapabilities, ToolCall,
};
use anyhow::Result;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Path this transport speaks, appended to a `base_url` that names only the
/// API root.
const ENDPOINT_PATH: &str = "/responses";

/// Asks for the reasoning blob in the reply.  Nothing is kept server-side,
/// so this is the only way the next turn can hand the chain back.
const INCLUDE_ENCRYPTED_REASONING: &str = "reasoning.encrypted_content";

#[derive(Debug, Serialize, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    /// Where the system prompt goes.  There is no system message in the
    /// input array.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub input: Vec<ResponsesItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponsesTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ResponsesReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<Value>,
    /// Defaults to true at the endpoint, which retains the prompt.  A patch
    /// review bot has no reason to leave copies of its prompts there.
    pub store: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesReasoningConfig {
    pub effort: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// One element of the `input` or `output` array.  Both directions use the
/// same item types, which is what lets a reasoning item go back out the way
/// it came in.  An item type this build does not model deserializes as
/// `Other` rather than failing the whole reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponsesItem {
    Message(ResponsesMessage),
    FunctionCall(ResponsesFunctionCall),
    FunctionCallOutput(ResponsesFunctionCallOutput),
    Reasoning(ResponsesReasoning),
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesMessage {
    pub role: String,
    pub content: ResponsesContent,
}

/// A message sent as input carries its text directly; one that comes back
/// carries a list of parts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesContent {
    Text(String),
    Parts(Vec<ResponsesContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesContentPart {
    #[serde(rename = "type")]
    pub part_type: String,
    #[serde(default)]
    pub text: String,
    /// Set on a part of type "refusal", which carries the model's reason
    /// for declining in place of any text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesFunctionCall {
    /// The identifier a result refers to.  The item also carries an `id`
    /// (`fc_...`), which is not it.
    pub call_id: String,
    pub name: String,
    /// A JSON object, encoded as a string.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesFunctionCallOutput {
    pub call_id: String,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponsesReasoning {
    pub id: String,
    /// Empty unless the request asked for a summary.  Serialized even so:
    /// the endpoint requires the field on an item sent back as input.
    #[serde(default)]
    pub summary: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResponsesResponse {
    #[serde(default)]
    pub status: Option<String>,
    /// Set on a reply whose `status` is "failed".  The endpoint answers
    /// 200 in that case, so this is the only account of what went wrong.
    #[serde(default)]
    pub error: Option<ResponsesError>,
    #[serde(default)]
    pub incomplete_details: Option<ResponsesIncompleteDetails>,
    #[serde(default)]
    pub output: Vec<ResponsesItem>,
    #[serde(default)]
    pub usage: ResponsesUsage,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ResponsesError {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ResponsesIncompleteDetails {
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ResponsesUsage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(
        default,
        deserialize_with = "lenient_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub input_tokens_details: Option<ResponsesInputTokensDetails>,
    #[serde(
        default,
        deserialize_with = "lenient_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub output_tokens_details: Option<ResponsesOutputTokensDetails>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ResponsesInputTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u32>,
    /// Reported alongside the cache hit and consumed by nothing today.
    #[serde(default)]
    pub cache_write_tokens: Option<u32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ResponsesOutputTokensDetails {
    /// Part of `output_tokens`, and billed as output.  `AiUsage` has no
    /// field for it, so it reaches the log and stops there.
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

impl ResponsesContent {
    /// The refusal text, when the message declines rather than answers.
    fn refusal(&self) -> Option<&str> {
        match self {
            ResponsesContent::Text(_) => None,
            ResponsesContent::Parts(parts) => parts.iter().find_map(|part| part.refusal.as_deref()),
        }
    }

    /// The text of each part in order, borrowed rather than joined.
    fn texts(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        match self {
            ResponsesContent::Text(text) => Box::new(std::iter::once(text.as_str())),
            ResponsesContent::Parts(parts) => Box::new(parts.iter().map(|part| part.text.as_str())),
        }
    }

    fn text(&self) -> String {
        self.texts().collect::<Vec<_>>().concat()
    }
}

pub struct OpenAiResponsesClient {
    model: String,
    base_url: String,
    context_window_size: usize,
    max_tokens: u32,
    effort: Option<String>,
    /// Learned by post_json; see there.
    temperature_unsupported: AtomicBool,
    client: Client,
}

impl OpenAiResponsesClient {
    pub fn new(
        base_url: String,
        model: String,
        context_window_size: usize,
        max_tokens: u32,
        api_timeout_secs: u64,
        effort: Option<String>,
    ) -> Result<Self> {
        let client = build_http_client(api_timeout_secs);
        let base_url = normalize_base_url(&base_url, ENDPOINT_PATH)?;

        Ok(Self {
            model,
            base_url,
            context_window_size,
            max_tokens,
            effort,
            temperature_unsupported: AtomicBool::new(false),
            client,
        })
    }

    /// OpenAI is the only vendor serving this endpoint, so the default does
    /// not vary with the model the way the chat transport's does.
    pub fn default_base_url() -> String {
        "https://api.openai.com/v1/responses".to_string()
    }

    fn build_request(&self, request: AiRequest) -> Result<ResponsesRequest> {
        let mut responses_req = translate_ai_request(request, self.max_tokens)?;
        responses_req.model = self.model.clone();
        // Omitted unless configured, which leaves the model at whatever
        // effort it defaults to.
        responses_req.reasoning = self
            .effort
            .clone()
            .map(|effort| ResponsesReasoningConfig { effort });
        Ok(responses_req)
    }

    async fn post_request(&self, body: &mut Value) -> Result<ResponsesResponse, OpenAiCompatError> {
        // No gateway-specific headers: only OpenAI's own API serves this
        // endpoint.
        let response: ResponsesResponse = post_json(
            &self.client,
            &self.base_url,
            &reqwest::header::HeaderMap::new(),
            body,
            &self.temperature_unsupported,
        )
        .await?;
        let details = response.usage.output_tokens_details.as_ref();
        tracing::info!(
            "OpenAI response received. Tokens: in={}, out={}, reasoning={}, cache_write={}",
            response.usage.input_tokens,
            response.usage.output_tokens,
            details.and_then(|d| d.reasoning_tokens).unwrap_or(0),
            response
                .usage
                .input_tokens_details
                .as_ref()
                .and_then(|d| d.cache_write_tokens)
                .unwrap_or(0),
        );
        Ok(response)
    }
}

/// Rebuilds the reasoning items an earlier turn returned.
///
/// A signature written by another provider does not parse as items, and an
/// item that lost its blob cannot be replayed once the turn it belongs to
/// is gone from the endpoint.  Both drop out here: sending them earns a
/// 400, while sending none costs the model its chain and nothing else.
fn replayed_reasoning(signature: &str) -> Vec<ResponsesItem> {
    let items: Vec<ResponsesItem> = match serde_json::from_str(signature) {
        Ok(items) => items,
        Err(e) => {
            tracing::warn!("Dropping a thought signature this endpoint cannot read: {e}");
            return Vec::new();
        }
    };

    items
        .into_iter()
        .filter(|item| {
            matches!(item, ResponsesItem::Reasoning(reasoning)
                if reasoning.encrypted_content.is_some())
        })
        .collect()
}

/// Packs the reasoning items an output item came out of into that item's
/// thought signature, leaving nothing behind: the next item in the reply
/// gets only the reasoning that followed this one.
fn take_signature(reasoning: &mut Vec<ResponsesItem>) -> Result<Option<String>> {
    if reasoning.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::to_string(&std::mem::take(reasoning))?))
}

fn input_message(role: &str, content: Option<String>) -> ResponsesItem {
    ResponsesItem::Message(ResponsesMessage {
        role: role.to_string(),
        content: ResponsesContent::Text(content.unwrap_or_default()),
    })
}

/// An assistant turn becomes several input items: the text it produced,
/// preceded by the reasoning that led to it, then each call it made,
/// preceded by the reasoning that led to that call.
///
/// The pairing is the point.  The endpoint reads a reasoning item as
/// belonging to the item that follows it and rejects the request when that
/// is not the item it produced, so a turn that thought between two calls
/// cannot have its reasoning collected into one block up front.  Reasoning
/// with nothing to follow it is dropped for the same reason: the message
/// signature goes back only alongside a message, and a turn that produced
/// neither text nor calls sends nothing at all.
fn push_assistant_turn(
    input: &mut Vec<ResponsesItem>,
    content: Option<String>,
    thought_signature: Option<String>,
    tool_calls: Option<Vec<ToolCall>>,
) {
    let content = content.filter(|text| !text.is_empty());
    let tool_calls = tool_calls.unwrap_or_default();
    if content.is_none() && tool_calls.is_empty() {
        return;
    }

    if content.is_some() {
        if let Some(signature) = thought_signature {
            input.extend(replayed_reasoning(&signature));
        }
        input.push(input_message("assistant", content));
    }
    for call in tool_calls {
        if let Some(signature) = call.thought_signature {
            input.extend(replayed_reasoning(&signature));
        }
        input.push(ResponsesItem::FunctionCall(ResponsesFunctionCall {
            call_id: call.id,
            name: call.function_name,
            arguments: serde_json::to_string(&call.arguments).unwrap_or_default(),
        }));
    }
}

/// The input items a conversation becomes, in order.
fn translate_input(messages: Vec<AiMessage>) -> Vec<ResponsesItem> {
    let mut input = Vec::new();

    for msg in messages {
        match msg.role {
            AiRole::System => input.push(input_message("system", msg.content)),
            AiRole::User => input.push(input_message("user", msg.content)),
            AiRole::Assistant => push_assistant_turn(
                &mut input,
                msg.content,
                msg.thought_signature,
                msg.tool_calls,
            ),
            AiRole::Tool => {
                input.push(ResponsesItem::FunctionCallOutput(
                    ResponsesFunctionCallOutput {
                        call_id: msg.tool_call_id.unwrap_or_default(),
                        output: msg.content.unwrap_or_default(),
                    },
                ));
            }
        }
    }

    input
}

/// The function tools on offer, or none when the list is empty.
fn translate_tools(tools: Option<Vec<AiTool>>) -> Option<Vec<ResponsesTool>> {
    tools.filter(|t| !t.is_empty()).map(|t| {
        t.into_iter()
            .map(|tool| ResponsesTool {
                tool_type: "function".to_string(),
                name: tool.name,
                description: tool.description,
                parameters: tool.parameters,
            })
            .collect()
    })
}

fn translate_ai_request(request: AiRequest, max_tokens: u32) -> Result<ResponsesRequest> {
    let instructions = request.system;
    let mut input = translate_input(request.messages);
    let tools = translate_tools(request.tools);

    let text = request.response_format.map(|rf| match rf {
        AiResponseFormat::Json { .. } => serde_json::json!({"format": {"type": "json_object"}}),
        AiResponseFormat::Text => serde_json::json!({"format": {"type": "text"}}),
    });

    // The endpoint requires the word "json" among the input items when the
    // reply is constrained to a JSON object, and scans nothing else: a
    // marker in the instructions field draws "Response input messages must
    // contain the word 'json'" instead.  Append rather than prepend so the
    // cached prompt prefix survives.
    if text
        .as_ref()
        .is_some_and(|t| t["format"]["type"] == "json_object")
        && !mentions_json(&input)
    {
        input.push(input_message(
            "system",
            Some("Respond in JSON format.".to_string()),
        ));
    }

    Ok(ResponsesRequest {
        // Set by the caller, which holds the configured model.
        model: String::new(),
        instructions,
        input,
        tools,
        temperature: request.temperature,
        // Set by the caller, which holds the configured level.
        reasoning: None,
        max_output_tokens: Some(max_tokens),
        text,
        store: false,
        include: vec![INCLUDE_ENCRYPTED_REASONING.to_string()],
    })
}

/// Whether the input items already ask for JSON.
///
/// The instructions field is not consulted: the endpoint does not scan it
/// when it validates a json_object request, so a mention there does not
/// spare the caller the marker.
fn mentions_json(input: &[ResponsesItem]) -> bool {
    input.iter().any(|item| match item {
        ResponsesItem::Message(message) => message.content.texts().any(says_json),
        _ => false,
    })
}

/// Whether `text` contains "json" in any case.  The input holds the patch
/// and the whole transcript so far, so the search borrows it rather than
/// lowercasing a copy of it on every request.
fn says_json(text: &str) -> bool {
    text.as_bytes()
        .windows(4)
        .any(|window| window.eq_ignore_ascii_case(b"json"))
}

/// The failure an endpoint reports inside a 200 reply, as an error of the
/// kind the equivalent HTTP status would have produced.
///
/// The reply carries no output in this case, so without this the caller
/// bails on the empty array instead, and an anyhow error the classifier
/// cannot downcast is fatal by default.  That ends the review for a
/// server-side failure that a 5xx saying the same thing would have
/// retried, and drops the endpoint's own account of it on the way.
fn response_failure(status: Option<&str>, error: Option<ResponsesError>) -> Option<anyhow::Error> {
    if status != Some("failed") && error.is_none() {
        return None;
    }

    let error = error.unwrap_or_default();
    let code = error.code.unwrap_or_else(|| "no code".to_string());
    let message = error.message.unwrap_or_else(|| "no message".to_string());
    let reported = format!("{}: {}", code, message);
    tracing::warn!(
        "{}OpenAI response failed: {}",
        crate::ai::get_log_prefix(),
        reported
    );

    // server_error is this reply's 5xx, so it retries on the same terms:
    // no delay of its own, leaving the pace to the caller's backoff.  Any
    // other code names something the request did, which another attempt
    // repeats.
    Some(match code.as_str() {
        "server_error" => {
            OpenAiCompatError::TransientError(Duration::from_secs(0), reported).into()
        }
        _ => OpenAiCompatError::ResponseFailed(reported).into(),
    })
}

fn translate_ai_response(resp: ResponsesResponse) -> Result<AiResponse> {
    if let Some(failure) = response_failure(resp.status.as_deref(), resp.error) {
        return Err(failure);
    }

    let truncated = resp.status.as_deref() == Some("incomplete");
    if truncated {
        tracing::warn!(
            "{}OpenAI response truncated: {}",
            crate::ai::get_log_prefix(),
            resp.incomplete_details
                .and_then(|d| d.reason)
                .unwrap_or_else(|| "reason unreported".to_string())
        );
    }

    let output = walk_output(resp.output)?;

    if output.texts.is_empty() && output.tool_calls.is_empty() && !truncated {
        anyhow::bail!("No message or tool call in response");
    }

    // The blob is the whole continuity across a tool call, since no history
    // is kept at the endpoint.  It travels as the assistant message's
    // thought signature, the way a Gemini one does, and comes back through
    // push_assistant_turn on the next request.  This one holds the
    // reasoning that led to the message; the reasoning that led to a call
    // rides on that call.  Anything left pending here produced nothing at
    // all, and the endpoint rejects an item with no following item, so it
    // goes no further.
    let mut turn_reasoning = output.turn_reasoning;
    let thought_signature = take_signature(&mut turn_reasoning)?;

    Ok(AiResponse {
        content: if output.texts.is_empty() {
            None
        } else {
            Some(output.texts.concat())
        },
        thought: if output.summaries.is_empty() {
            None
        } else {
            Some(output.summaries.join("\n"))
        },
        thought_signature,
        tool_calls: if output.tool_calls.is_empty() {
            None
        } else {
            Some(output.tool_calls)
        },
        usage: Some(translate_usage(resp.usage)),
        truncated,
    })
}

/// What one walk over a reply's output items yields.
struct OutputWalk {
    texts: Vec<String>,
    summaries: Vec<String>,
    tool_calls: Vec<ToolCall>,
    /// The reasoning items that led to the message, for the caller to
    /// turn into the turn's thought signature.
    turn_reasoning: Vec<ResponsesItem>,
}

/// Walks the output items, pairing each reasoning item with the message or
/// call that follows it.
fn walk_output(items: Vec<ResponsesItem>) -> Result<OutputWalk> {
    let mut texts = Vec::new();
    let mut summaries = Vec::new();
    // The reasoning items seen since the last item that could have come
    // out of them.  A turn that thinks between calls produces several, and
    // each one belongs to the single item that follows it, so they are
    // filed there rather than on the turn as a whole.
    let mut pending = Vec::new();
    let mut turn_reasoning = Vec::new();
    let mut tool_calls = Vec::new();

    for item in items {
        match item {
            ResponsesItem::Message(message) => {
                // A refusal part carries no text, so the message would
                // otherwise read as an empty reply and cost a retry that
                // never learns why.
                if let Some(refusal) = message.content.refusal() {
                    anyhow::bail!("OpenAI refused the request: {}", refusal);
                }
                texts.push(message.content.text());
                turn_reasoning.append(&mut pending);
            }
            ResponsesItem::FunctionCall(call) => tool_calls.push(ToolCall {
                id: call.call_id,
                function_name: call.name,
                arguments: serde_json::from_str(&call.arguments).unwrap_or(Value::Null),
                thought_signature: take_signature(&mut pending)?,
            }),
            ResponsesItem::Reasoning(item) => {
                summaries.extend(
                    item.summary
                        .iter()
                        .filter_map(|part| part["text"].as_str().map(str::to_string)),
                );
                pending.push(ResponsesItem::Reasoning(item));
            }
            _ => {}
        }
    }

    Ok(OutputWalk {
        texts,
        summaries,
        tool_calls,
        turn_reasoning,
    })
}

/// input_tokens already counts the cached prefix, so cached_tokens is a
/// breakdown of it rather than an addend.  A larger count means the
/// endpoint reports the prefix alongside the prompt instead, which
/// offers no usable breakdown, so it is dropped.
fn translate_usage(usage: ResponsesUsage) -> AiUsage {
    let cached = usage
        .input_tokens_details
        .and_then(|d| d.cached_tokens)
        .filter(|&c| c <= usage.input_tokens)
        .unwrap_or(0);

    AiUsage {
        prompt_tokens: usage.input_tokens as usize,
        completion_tokens: usage.output_tokens as usize,
        total_tokens: usage.total_tokens as usize,
        cached_tokens: if cached > 0 {
            Some(cached as usize)
        } else {
            None
        },
    }
}

#[async_trait]
impl AiProvider for OpenAiResponsesClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        tracing::info!("Sending OpenAI responses request...");

        let responses_req = self.build_request(request)?;

        let mut body = serde_json::to_value(&responses_req)?;
        let resp = self.post_request(&mut body).await?;
        translate_ai_response(resp)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: self.context_window_size,
        }
    }

    fn cache_identity(&self) -> String {
        // Same knobs as the chat transport, plus the endpoint itself.  One
        // model answers differently over the two, and reaches gpt-5.4 over
        // only one of them, so their entries must not collide.
        let max_tokens = self.max_tokens.to_string();
        crate::ai::cache_identity_with(
            &self.model,
            &[
                ("max_tokens", Some(max_tokens.as_str())),
                ("base_url", Some(self.base_url.as_str())),
                ("api", Some("responses")),
                ("effort", self.effort.as_deref()),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiMessage, AiTool};
    use serde_json::json;

    /// Replies recorded against the live API.  The first two are whole
    /// captures; the other two recombine their output items to reach the
    /// shapes a tool stage produces.
    const MESSAGE: &str = include_str!("testdata/responses_message.json");
    const REASONING_MESSAGE: &str = include_str!("testdata/responses_reasoning_message.json");
    const FUNCTION_CALL: &str = include_str!("testdata/responses_function_call.json");
    const REASONING_FUNCTION_CALL: &str =
        include_str!("testdata/responses_reasoning_function_call.json");

    fn parse(fixture: &str) -> ResponsesResponse {
        serde_json::from_str(fixture).expect("a recorded reply parses")
    }

    fn user_message(text: &str) -> AiMessage {
        AiMessage {
            role: AiRole::User,
            content: Some(text.to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn sample_request() -> AiRequest {
        AiRequest {
            system: None,
            messages: vec![user_message("hi")],
            tools: None,
            temperature: None,
            response_format: None,
            reasoning_effort: None,
            context_tag: None,
        }
    }

    fn test_client(base_url: &str, model: &str, effort: Option<&str>) -> OpenAiResponsesClient {
        OpenAiResponsesClient::new(
            base_url.to_string(),
            model.to_string(),
            400_000,
            65536,
            60,
            effort.map(str::to_string),
        )
        .unwrap()
    }

    #[test]
    fn test_normalize_base_url_appends_responses() {
        assert_eq!(
            test_client("https://api.openai.com/v1", "gpt-5.4", None).base_url,
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(
            test_client(&OpenAiResponsesClient::default_base_url(), "gpt-5.4", None).base_url,
            "https://api.openai.com/v1/responses"
        );
        // A chat endpoint is not this transport's, and saying so at
        // construction beats a 404 on every request.
        assert!(
            OpenAiResponsesClient::new(
                "https://api.openai.com/v1/chat/completions".to_string(),
                "gpt-5.4".to_string(),
                400_000,
                65536,
                60,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn test_translate_request_system_and_user() -> Result<()> {
        let mut request = sample_request();
        request.system = Some("You are helpful.".to_string());

        let req = translate_ai_request(request, 4096)?;

        // The system prompt is not a message here.
        assert_eq!(req.instructions.as_deref(), Some("You are helpful."));
        assert_eq!(req.input.len(), 1);
        let json = serde_json::to_value(&req)?;
        assert_eq!(json["input"][0]["type"], "message");
        assert_eq!(json["input"][0]["role"], "user");
        assert_eq!(json["input"][0]["content"], "hi");
        assert_eq!(json["max_output_tokens"], 4096);
        assert_eq!(json["store"], false);
        assert_eq!(json["include"][0], "reasoning.encrypted_content");
        assert!(json.get("reasoning").is_none());

        Ok(())
    }

    #[test]
    fn test_translate_request_tools_are_flat() -> Result<()> {
        let mut request = sample_request();
        request.tools = Some(vec![AiTool {
            name: "my_tool".to_string(),
            description: "Does something.".to_string(),
            parameters: json!({"type": "object"}),
        }]);

        let json = serde_json::to_value(translate_ai_request(request, 4096)?)?;

        // Chat nests all three under "function"; this endpoint does not.
        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["name"], "my_tool");
        assert_eq!(json["tools"][0]["description"], "Does something.");
        assert_eq!(json["tools"][0]["parameters"]["type"], "object");
        assert!(json["tools"][0].get("function").is_none());

        Ok(())
    }

    #[test]
    fn test_translate_request_empty_tools() -> Result<()> {
        let mut request = sample_request();
        request.tools = Some(vec![]);

        assert!(translate_ai_request(request, 4096)?.tools.is_none());

        Ok(())
    }

    #[test]
    fn test_translate_request_json_format() -> Result<()> {
        let mut request = sample_request();
        request.response_format = Some(AiResponseFormat::Json { schema: None });

        let req = translate_ai_request(request, 4096)?;

        assert_eq!(req.text, Some(json!({"format": {"type": "json_object"}})));
        // The marker goes among the input items.  The endpoint rejects a
        // json_object request carrying it only in the instructions.
        assert!(req.instructions.is_none());
        assert_eq!(req.input.len(), 2);
        assert!(mentions_json(&req.input));

        // Input that already says it is left alone.
        let mut request = sample_request();
        request.messages = vec![user_message("Answer in JSON.")];
        request.response_format = Some(AiResponseFormat::Json { schema: None });
        let req = translate_ai_request(request, 4096)?;
        assert_eq!(req.input.len(), 1);

        // A mention in the system prompt does not spare the marker.
        let mut request = sample_request();
        request.system = Some("Answer in JSON.".to_string());
        request.response_format = Some(AiResponseFormat::Json { schema: None });
        let req = translate_ai_request(request, 4096)?;
        assert_eq!(req.instructions.as_deref(), Some("Answer in JSON."));
        assert_eq!(req.input.len(), 2);

        Ok(())
    }

    #[test]
    fn test_translate_request_tool_result_keys_on_call_id() -> Result<()> {
        let request = AiRequest {
            messages: vec![
                user_message("read it"),
                AiMessage {
                    role: AiRole::Assistant,
                    content: None,
                    thought: None,
                    thought_signature: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call_abc".to_string(),
                        function_name: "read_file".to_string(),
                        arguments: json!({"path": "/etc/hostname"}),
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
                    tool_call_id: Some("call_abc".to_string()),
                },
            ],
            ..sample_request()
        };

        let json = serde_json::to_value(translate_ai_request(request, 4096)?)?;

        assert_eq!(json["input"][1]["type"], "function_call");
        assert_eq!(json["input"][1]["call_id"], "call_abc");
        assert_eq!(json["input"][1]["name"], "read_file");
        assert_eq!(json["input"][1]["arguments"], r#"{"path":"/etc/hostname"}"#);
        // The result is an item of its own, not a message with a role.
        assert_eq!(json["input"][2]["type"], "function_call_output");
        assert_eq!(json["input"][2]["call_id"], "call_abc");
        assert_eq!(json["input"][2]["output"], r#"{"ok":true}"#);

        Ok(())
    }

    /// The reasoning a turn produced has to reach the next request intact,
    /// or the model loses its chain across the tool call.  Nothing errors
    /// when it does not, which is why this goes through both translations.
    #[test]
    fn test_reasoning_round_trips_through_the_thought_signature() -> Result<()> {
        let resp = translate_ai_response(parse(REASONING_FUNCTION_CALL))?;
        // This turn produced a call, so its reasoning travels on the call
        // rather than on the message.
        let signature = resp
            .tool_calls
            .as_ref()
            .and_then(|calls| calls[0].thought_signature.clone())
            .expect("a reasoning item carries a signature");

        let request = AiRequest {
            messages: vec![
                user_message("read it"),
                AiMessage {
                    role: AiRole::Assistant,
                    content: resp.content.clone(),
                    thought: resp.thought.clone(),
                    thought_signature: resp.thought_signature.clone(),
                    tool_calls: resp.tool_calls.clone(),
                    tool_call_id: None,
                },
            ],
            ..sample_request()
        };

        let json = serde_json::to_value(translate_ai_request(request, 4096)?)?;

        assert_eq!(json["input"][1]["type"], "reasoning");
        assert!(
            json["input"][1]["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("rs_"))
        );
        assert!(json["input"][1]["summary"].is_array());
        let blob = json["input"][1]["encrypted_content"]
            .as_str()
            .expect("the blob survives the round trip");
        assert!(signature.contains(blob));
        // The reasoning precedes the call it led to.
        assert_eq!(json["input"][2]["type"], "function_call");

        Ok(())
    }

    /// A model that thinks between calls returns reasoning, a call, more
    /// reasoning, and another call.  Each blob has to go back in front of
    /// the call it produced: collecting them into one block up front earns
    /// "provided without its required following item", which is a 400 and
    /// therefore fatal to the review.
    #[test]
    fn test_reasoning_interleaved_with_calls_keeps_its_pairing() -> Result<()> {
        let mut reply: Value = serde_json::from_str(REASONING_FUNCTION_CALL)?;
        let output = reply["output"].as_array().expect("a recorded output");
        let (first_reasoning, first_call) = (output[0].clone(), output[1].clone());
        let mut second_reasoning = first_reasoning.clone();
        second_reasoning["id"] = json!("rs_second");
        second_reasoning["encrypted_content"] = json!("second_blob");
        let mut second_call = first_call.clone();
        second_call["call_id"] = json!("call_second");
        reply["output"] = json!([first_reasoning, first_call, second_reasoning, second_call]);

        let resp = translate_ai_response(serde_json::from_value(reply)?)?;
        let calls = resp.tool_calls.clone().expect("two calls");
        assert_eq!(calls.len(), 2);

        let request = AiRequest {
            messages: vec![
                user_message("read them"),
                AiMessage {
                    role: AiRole::Assistant,
                    content: resp.content.clone(),
                    thought: resp.thought.clone(),
                    thought_signature: resp.thought_signature.clone(),
                    tool_calls: resp.tool_calls,
                    tool_call_id: None,
                },
            ],
            ..sample_request()
        };

        let json = serde_json::to_value(translate_ai_request(request, 4096)?)?;
        let input = json["input"].as_array().expect("an input array");

        assert_eq!(input.len(), 5);
        assert_eq!(input[1]["type"], "reasoning");
        assert_ne!(input[1]["encrypted_content"], json!("second_blob"));
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], calls[0].id);
        assert_eq!(input[3]["type"], "reasoning");
        assert_eq!(input[3]["encrypted_content"], json!("second_blob"));
        assert_eq!(input[4]["type"], "function_call");
        assert_eq!(input[4]["call_id"], "call_second");

        Ok(())
    }

    #[test]
    fn test_reasoning_without_a_blob_is_not_replayed() -> Result<()> {
        let signature = json!([{"type": "reasoning", "id": "rs_1", "summary": []}]).to_string();

        let request = AiRequest {
            messages: vec![AiMessage {
                role: AiRole::Assistant,
                content: Some("done".to_string()),
                thought: None,
                thought_signature: Some(signature),
                tool_calls: None,
                tool_call_id: None,
            }],
            ..sample_request()
        };

        let json = serde_json::to_value(translate_ai_request(request, 4096)?)?;

        assert_eq!(json["input"].as_array().map(Vec::len), Some(1));
        assert_eq!(json["input"][0]["type"], "message");

        Ok(())
    }

    /// A signature from another provider reaches this one through a stale
    /// conversation.  It is not items, and the request has to survive it.
    #[test]
    fn test_a_foreign_thought_signature_is_dropped() -> Result<()> {
        let request = AiRequest {
            messages: vec![AiMessage {
                role: AiRole::Assistant,
                content: Some("done".to_string()),
                thought: None,
                thought_signature: Some("ErwDCrkDARFNMg9YVicQUo".to_string()),
                tool_calls: None,
                tool_call_id: None,
            }],
            ..sample_request()
        };

        let json = serde_json::to_value(translate_ai_request(request, 4096)?)?;

        assert_eq!(json["input"].as_array().map(Vec::len), Some(1));
        assert_eq!(json["input"][0]["type"], "message");

        Ok(())
    }

    /// A reasoning item whose turn produced nothing else has no following
    /// item, and the endpoint rejects it on those grounds.
    #[test]
    fn test_an_orphan_reasoning_item_is_dropped() -> Result<()> {
        let resp = translate_ai_response(parse(REASONING_MESSAGE))?;

        let request = AiRequest {
            messages: vec![AiMessage {
                role: AiRole::Assistant,
                content: None,
                thought: None,
                thought_signature: resp.thought_signature,
                tool_calls: None,
                tool_call_id: None,
            }],
            ..sample_request()
        };

        let json = serde_json::to_value(translate_ai_request(request, 4096)?)?;

        assert_eq!(json["input"].as_array().map(Vec::len), Some(0));

        Ok(())
    }

    #[test]
    fn test_translate_response_message() -> Result<()> {
        let resp = translate_ai_response(parse(MESSAGE))?;

        assert!(
            resp.content
                .as_deref()
                .is_some_and(|c| c.contains("socket already in use"))
        );
        assert_eq!(resp.tool_calls, None);
        assert_eq!(resp.thought_signature, None);
        assert!(!resp.truncated);

        Ok(())
    }

    #[test]
    fn test_translate_response_reasoning_and_message() -> Result<()> {
        let resp = translate_ai_response(parse(REASONING_MESSAGE))?;

        assert!(resp.content.is_some());
        // The summary was not asked for, so there is no thought text, but
        // the blob that keeps the chain is there.
        assert_eq!(resp.thought, None);
        assert!(resp.thought_signature.is_some());

        Ok(())
    }

    #[test]
    fn test_translate_response_function_call() -> Result<()> {
        let resp = translate_ai_response(parse(FUNCTION_CALL))?;

        assert_eq!(resp.content, None);
        let calls = resp.tool_calls.expect("the reply carries a call");
        assert_eq!(calls.len(), 1);
        // call_id, not the fc_ id the same item carries.
        assert_eq!(calls[0].id, "call_Gkbd34E81BuPmpxyaSAre5tI");
        assert_eq!(calls[0].function_name, "read_file");
        assert_eq!(calls[0].arguments["path"], "/etc/hostname");

        Ok(())
    }

    #[test]
    fn test_translate_response_usage() -> Result<()> {
        let usage = translate_ai_response(parse(REASONING_MESSAGE))?
            .usage
            .expect("the reply carries usage");

        assert_eq!(usage.prompt_tokens, 56);
        assert_eq!(usage.completion_tokens, 267);
        assert_eq!(usage.total_tokens, 323);
        assert_eq!(usage.cached_tokens, None);

        Ok(())
    }

    /// cached_tokens is a breakdown of the prompt count, not an addend, so
    /// a consumer subtracting one from the other must not go negative.
    #[test]
    fn test_translate_response_cached_tokens_break_down_the_prompt() -> Result<()> {
        let with_usage = |usage: Value| {
            let mut reply: Value = serde_json::from_str(MESSAGE).unwrap();
            reply["usage"] = usage;
            translate_ai_response(serde_json::from_value(reply).unwrap())
                .unwrap()
                .usage
                .unwrap()
        };

        let usage = with_usage(json!({
            "input_tokens": 2048,
            "output_tokens": 20,
            "total_tokens": 2068,
            "input_tokens_details": {"cached_tokens": 1920, "cache_write_tokens": 0},
        }));
        assert_eq!(usage.prompt_tokens, 2048);
        assert_eq!(usage.cached_tokens, Some(1920));
        assert!(usage.cached_tokens.unwrap() <= usage.prompt_tokens);

        // An endpoint reporting the prefix alongside the prompt offers no
        // usable breakdown, so the whole prompt stays uncached input.
        let usage = with_usage(json!({
            "input_tokens": 2048,
            "output_tokens": 20,
            "total_tokens": 2068,
            "input_tokens_details": {"cached_tokens": 3000},
        }));
        assert_eq!(usage.prompt_tokens, 2048);
        assert_eq!(usage.cached_tokens, None);

        Ok(())
    }

    #[test]
    fn test_translate_response_truncated() -> Result<()> {
        let mut reply: Value = serde_json::from_str(REASONING_MESSAGE)?;
        reply["status"] = json!("incomplete");
        reply["incomplete_details"] = json!({"reason": "max_output_tokens"});
        reply["output"] = json!([]);

        let resp = translate_ai_response(serde_json::from_value(reply)?)?;

        assert!(resp.truncated);
        assert_eq!(resp.content, None);

        Ok(())
    }

    /// A declined request comes back as a message whose only part is a
    /// refusal.  It is a failure that names its reason, not an empty reply.
    #[test]
    fn test_translate_response_refusal_fails_with_its_reason() {
        let mut reply: Value = serde_json::from_str(MESSAGE).unwrap();
        reply["output"] = json!([{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "refusal", "refusal": "I can't help with that"}],
        }]);

        let err = translate_ai_response(serde_json::from_value(reply).unwrap())
            .expect_err("a refusal is an error");

        assert!(err.to_string().contains("I can't help with that"));
    }

    #[test]
    fn test_translate_response_without_output() {
        let mut reply: Value = serde_json::from_str(MESSAGE).unwrap();
        reply["output"] = json!([]);

        let resp = translate_ai_response(serde_json::from_value(reply).unwrap());

        assert!(resp.is_err());
    }

    /// Some failures arrive as a 200 whose status is "failed", and the
    /// error object is the only account of them.  A server_error retries
    /// on the same terms as the 5xx it stands for; anything else names
    /// something the request did, which another attempt repeats.
    #[test]
    fn test_a_failed_status_carries_its_reason_and_class() {
        let failed = |code: &str| {
            let mut reply: Value = serde_json::from_str(MESSAGE).unwrap();
            reply["status"] = json!("failed");
            reply["output"] = json!([]);
            reply["error"] = json!({"code": code, "message": "upstream fell over"});
            translate_ai_response(serde_json::from_value(reply).unwrap())
                .expect_err("a failed reply is an error")
        };

        let transient = failed("server_error");
        assert!(transient.to_string().contains("upstream fell over"));
        assert!(matches!(
            crate::ai::classify_ai_error(&transient),
            crate::ai::AiErrorClass::Transient { .. }
        ));

        let fatal = failed("invalid_prompt");
        assert!(fatal.to_string().contains("upstream fell over"));
        assert!(matches!(
            crate::ai::classify_ai_error(&fatal),
            crate::ai::AiErrorClass::Fatal
        ));
    }

    /// The endpoint returns item types this build does not model, and a new
    /// one must not cost the reply it arrived with.
    #[test]
    fn test_translate_response_ignores_an_unmodelled_item() -> Result<()> {
        let mut reply: Value = serde_json::from_str(MESSAGE).unwrap();
        reply["output"]
            .as_array_mut()
            .unwrap()
            .insert(0, json!({"type": "web_search_call", "id": "ws_1"}));

        let resp = translate_ai_response(serde_json::from_value(reply)?)?;

        assert!(resp.content.is_some());

        Ok(())
    }

    #[test]
    fn configured_effort_reaches_the_request() {
        let client = test_client("https://api.openai.com/v1", "gpt-5.4", Some("high"));

        let built = client.build_request(sample_request()).unwrap();
        let json = serde_json::to_value(&built).unwrap();
        assert_eq!(json["reasoning"]["effort"], "high");
    }

    #[tokio::test]
    async fn temperature_refusal_resends_without_the_field() {
        use crate::ai::openai_common::tests::{TEMPERATURE_REFUSED_RESPONSES, serve_replies};

        let (base_url, seen) = serve_replies(vec![
            (400, TEMPERATURE_REFUSED_RESPONSES.to_string()),
            (200, MESSAGE.to_string()),
        ])
        .await;
        let client = test_client(&base_url, "gpt-5.4", None);
        let mut request = sample_request();
        request.temperature = Some(0.0);

        let response = client.generate_content(request).await.unwrap();
        assert!(response.content.is_some());

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0]["temperature"], 0.0);
        assert!(seen[1].get("temperature").is_none());
    }

    #[test]
    fn cache_identity_separates_the_two_transports() {
        use crate::ai::openai::{OpenAiCompatClient, OpenAiProviderType};

        let responses = test_client("https://api.openai.com/v1", "gpt-5.4", Some("medium"));
        let chat = OpenAiCompatClient::new(
            "https://api.openai.com/v1".to_string(),
            OpenAiProviderType::OpenAi,
            "gpt-5.4".to_string(),
            400_000,
            65536,
            60,
            Some("medium".to_string()),
        )
        .unwrap();

        assert_ne!(responses.cache_identity(), chat.cache_identity());

        let raised = test_client("https://api.openai.com/v1", "gpt-5.4", Some("high"));
        assert_ne!(responses.cache_identity(), raised.cache_identity());
    }
}
