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
    AiUsage, ClassifyAiError, MAX_PROVIDER_METADATA_BYTES, ProviderCapabilities, ToolCall,
    classify_status_code, provider_metadata_size,
};
use crate::utils::redact_secret;
use anyhow::{Context, Result};
use async_trait::async_trait;
use regex::Regex;
use reqwest::{Client, Response};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::sync::LazyLock;
use std::time::Duration;

static RETRY_AFTER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Please retry in ([0-9.]+)s")
        .expect("the hard-coded OpenAI retry delay regex is valid")
});
static BEARER_SECRET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(authorization\s*:\s*bearer\s+|bearer\s+)[^\s,;]+")
        .expect("the hard-coded bearer secret regex is valid")
});
static OPENAI_SECRET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\bsk-[A-Za-z0-9_-]+").expect("the hard-coded OpenAI secret regex is valid")
});

const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(30);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(5 * 60);
const MAX_RESPONSE_BODY_BYTES: usize = MAX_PROVIDER_METADATA_BYTES + 1024 * 1024;
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_PROVIDER_LOG_BYTES: usize = 4 * 1024;
const MAX_RESPONSE_JSON_VALUES: usize = 128 * 1024;
const MAX_RESPONSE_OUTPUT_ITEMS: usize = 4_096;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

// ── Response types ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ResponsesResponse {
    #[allow(dead_code)]
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub error: Option<ResponsesError>,
    /// Preserve the API's output items verbatim. Their shape evolves as new
    /// built-in tools and reasoning features are added, and callers managing
    /// state manually must replay every item without reconstruction.
    pub output: Vec<Value>,
    #[serde(default)]
    pub incomplete_details: Option<ResponsesIncompleteDetails>,
    #[serde(default, deserialize_with = "lenient_usage")]
    pub usage: ResponsesUsage,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResponsesError {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResponsesIncompleteDetails {
    #[serde(default)]
    pub reason: Option<String>,
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

fn lenient_usage<'de, D>(deserializer: D) -> Result<ResponsesUsage, D::Error>
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
    #[error("OpenAI Responses generation failed (status={status}, code={code}): {message}")]
    ResponseFailed {
        status: String,
        code: String,
        message: String,
    },
    #[error("OpenAI Responses response exceeds the {limit}-byte limit")]
    ResponseTooLarge { limit: usize },
    #[error(
        "OpenAI Responses response contains {count} output items, exceeding the limit of {limit}"
    )]
    TooManyOutputItems { count: usize, limit: usize },
    #[error(
        "OpenAI Responses continuation metadata is {size} bytes, exceeding the limit of {limit}"
    )]
    ContinuationTooLarge { size: usize, limit: usize },
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
            OpenAiError::ResponseFailed { code, .. }
                if matches!(code.as_str(), "server_error" | "server_is_overloaded") =>
            {
                AiErrorClass::Transient {
                    retry_after: DEFAULT_RETRY_AFTER,
                }
            }
            OpenAiError::ResponseFailed { .. }
            | OpenAiError::ResponseTooLarge { .. }
            | OpenAiError::TooManyOutputItems { .. }
            | OpenAiError::ContinuationTooLarge { .. } => AiErrorClass::Fatal,
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

        let base_url = Self::normalize_base_url(&base_url)?;
        let disable_proxy = base_url.starts_with("http://");
        let client = Self::create_http_client(&api_key, api_timeout_secs, disable_proxy)?;

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

    fn create_http_client(
        api_key: &str,
        api_timeout_secs: u64,
        disable_proxy: bool,
    ) -> Result<Client> {
        let mut headers = reqwest::header::HeaderMap::new();
        if !api_key.is_empty() {
            let value = reqwest::header::HeaderValue::from_str(&format!("Bearer {api_key}"))
                .context("OpenAI API key is not a valid HTTP header value")?;
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }

        let mut builder = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(api_timeout_secs));

        if disable_proxy {
            builder = builder.no_proxy();
        }

        builder
            .build()
            .context("Failed to build OpenAI Responses HTTP client")
    }

    /// Normalize an OpenAI API root or full Responses endpoint.
    fn normalize_base_url(url: &str) -> Result<String> {
        let mut parsed = url::Url::parse(url)
            .map_err(|_| anyhow::anyhow!("Invalid OpenAI Responses url {url}"))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            anyhow::bail!("Invalid OpenAI Responses url {url}");
        }
        if parsed.scheme() == "http" && !Self::is_loopback_host(&parsed) {
            anyhow::bail!(
                "Invalid OpenAI Responses url {url}; use HTTPS for non-loopback endpoints"
            );
        }

        let path = parsed.path().trim_end_matches('/');
        let normalized_path = match path {
            "" => "/responses".to_string(),
            "/v1" => "/v1/responses".to_string(),
            "/api/v1" => "/api/v1/responses".to_string(),
            path if path.ends_with("/responses") => path.to_string(),
            _ => anyhow::bail!(
                "Invalid OpenAI Responses url {url}; provide an API root or a full /responses endpoint"
            ),
        };
        parsed.set_path(&normalized_path);
        Ok(parsed.to_string().trim_end_matches('/').to_string())
    }

    fn is_loopback_host(url: &url::Url) -> bool {
        match url.host() {
            Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            None => false,
        }
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
        let res = match self.client.post(&self.base_url).json(body).send().await {
            Ok(res) => res,
            Err(e) => {
                let err_str = redact_secret(&format!("{:#}", anyhow::Error::from(e)));
                tracing::error!(
                    "{}OpenAI Responses request failed (transport): {}",
                    crate::ai::get_log_prefix(),
                    err_str
                );
                return Err(OpenAiError::TransientError(
                    Duration::from_secs(30),
                    err_str,
                ));
            }
        };

        let status = res.status();
        let retry_after_header = res
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body_limit = if status.is_success() {
            MAX_RESPONSE_BODY_BYTES
        } else {
            MAX_ERROR_BODY_BYTES
        };
        let body = match Self::read_response_body(res, body_limit).await {
            Ok(body) => body,
            Err(OpenAiError::ResponseTooLarge { .. }) if !status.is_success() => {
                let retry_after = retry_after_delay(retry_after_header.as_deref(), "");
                if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    return Err(OpenAiError::RateLimitExceeded(retry_after));
                }
                if status.is_server_error() {
                    return Err(OpenAiError::TransientError(
                        retry_after,
                        format!("server error {status} with oversized response body"),
                    ));
                }
                return Err(OpenAiError::ApiError(
                    status,
                    format!("error {status} with oversized response body"),
                ));
            }
            Err(e) => return Err(e),
        };

        if status.is_success() {
            return decode_response(&body).map_err(|e| {
                let error = sanitize_provider_text(&e.to_string());
                tracing::error!(
                    "{}Failed to decode OpenAI Responses response: {}",
                    crate::ai::get_log_prefix(),
                    error
                );
                OpenAiError::ApiError(status, format!("Decode error: {error}"))
            });
        }

        // Error handling — same pattern as openai.rs: parse status, extract
        // retry-after for rate limits, classify 401/403 as auth errors, etc.
        let body_text = String::from_utf8_lossy(&body);
        let retry_after = retry_after_delay(retry_after_header.as_deref(), &body_text);

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(OpenAiError::RateLimitExceeded(retry_after));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            let error = sanitize_provider_text(&body_text);
            tracing::error!(
                "{}OpenAI Responses authentication failed: {}",
                crate::ai::get_log_prefix(),
                error
            );
            return Err(OpenAiError::AuthenticationError(error));
        }
        if status.is_server_error() {
            return Err(OpenAiError::TransientError(
                retry_after,
                sanitize_provider_text(&body_text),
            ));
        }
        let error = sanitize_provider_text(&body_text);
        tracing::error!(
            "{}OpenAI Responses API error {}: {}",
            crate::ai::get_log_prefix(),
            status,
            error
        );
        Err(OpenAiError::ApiError(status, error))
    }

    async fn read_response_body(
        mut response: Response,
        limit: usize,
    ) -> Result<Vec<u8>, OpenAiError> {
        Self::read_response_body_with_limit(&mut response, limit).await
    }

    async fn read_response_body_with_limit(
        response: &mut Response,
        limit: usize,
    ) -> Result<Vec<u8>, OpenAiError> {
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(OpenAiError::ResponseTooLarge { limit });
        }

        let capacity = response
            .content_length()
            .map(|length| length as usize)
            .unwrap_or_default();
        let mut body = Vec::with_capacity(capacity);
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            let err_str = redact_secret(&format!("{:#}", anyhow::Error::from(error)));
            tracing::error!(
                "{}Failed to read OpenAI Responses response body: {}",
                crate::ai::get_log_prefix(),
                err_str
            );
            OpenAiError::TransientError(DEFAULT_RETRY_AFTER, err_str)
        })? {
            if chunk.len() > limit.saturating_sub(body.len()) {
                return Err(OpenAiError::ResponseTooLarge { limit });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

fn sanitize_provider_text(input: &str) -> String {
    let prefix = crate::utils::utf8_prefix(input, MAX_PROVIDER_LOG_BYTES);
    let redacted = redact_secret(prefix);
    let redacted = BEARER_SECRET_RE.replace_all(&redacted, "$1[REDACTED]");
    let redacted = OPENAI_SECRET_RE.replace_all(&redacted, "[REDACTED]");
    let mut sanitized = String::with_capacity(redacted.len());
    for character in redacted.chars() {
        if character.is_control() {
            sanitized.extend(character.escape_default());
        } else {
            sanitized.push(character);
        }
    }
    if prefix.len() < input.len() {
        sanitized.push_str("...[truncated]");
    }
    sanitized
}

struct JsonValueBudget {
    remaining: usize,
}

impl JsonValueBudget {
    fn consume<E: de::Error>(&mut self) -> std::result::Result<(), E> {
        self.remaining = self
            .remaining
            .checked_sub(1)
            .ok_or_else(|| E::custom("response contains too many JSON values"))?;
        Ok(())
    }
}

struct JsonBudgetVisitor<'a> {
    budget: &'a mut JsonValueBudget,
}

impl<'de> DeserializeSeed<'de> for &mut JsonValueBudget {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        self.consume::<D::Error>()?;
        deserializer.deserialize_any(JsonBudgetVisitor { budget: self })
    }
}

impl<'de> Visitor<'de> for JsonBudgetVisitor<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value within the response complexity limit")
    }

    fn visit_bool<E>(self, _value: bool) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_str<E>(self, _value: &str) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_string<E>(self, _value: String) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_none<E>(self) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_some<D>(self, deserializer: D) -> std::result::Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        self.budget.deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<(), A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element_seed(&mut *self.budget)?.is_some() {}
        Ok(())
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<(), A::Error>
    where
        A: MapAccess<'de>,
    {
        while map.next_key_seed(&mut *self.budget)?.is_some() {
            map.next_value_seed(&mut *self.budget)?;
        }
        Ok(())
    }
}

fn decode_response(body: &[u8]) -> serde_json::Result<ResponsesResponse> {
    let mut budget = JsonValueBudget {
        remaining: MAX_RESPONSE_JSON_VALUES,
    };
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    (&mut budget).deserialize(&mut deserializer)?;
    deserializer.end()?;
    serde_json::from_slice(body)
}

fn parse_retry_after_seconds(value: &str) -> Option<Duration> {
    let seconds = value.trim().parse::<f64>().ok()?;
    if seconds.is_nan() || seconds.is_sign_negative() {
        return None;
    }
    Some(Duration::from_secs_f64(
        seconds.min(MAX_RETRY_AFTER.as_secs_f64()),
    ))
}

fn retry_after_delay(header: Option<&str>, body: &str) -> Duration {
    header
        .and_then(parse_retry_after_seconds)
        .or_else(|| {
            RETRY_AFTER_RE
                .captures(body)
                .and_then(|captures| captures.get(1))
                .and_then(|value| parse_retry_after_seconds(value.as_str()))
        })
        .unwrap_or(DEFAULT_RETRY_AFTER)
}

fn parse_arguments_bounded(arguments: &str, call_id: &str) -> Result<Value> {
    let mut budget = JsonValueBudget {
        remaining: MAX_RESPONSE_JSON_VALUES,
    };
    let mut de = serde_json::Deserializer::from_str(arguments);
    match (&mut budget).deserialize(&mut de) {
        Ok(()) => {}
        Err(e) if e.to_string().contains("too many JSON values") => {
            anyhow::bail!(
                "OpenAI function_call {call_id} arguments exceed the JSON complexity limit"
            );
        }
        Err(_) => {
            // Invalid JSON; fall through to the normal parse which gives a
            // better error context.
        }
    }
    let args: Value = serde_json::from_str(arguments)
        .with_context(|| format!("OpenAI function_call {call_id} has invalid JSON arguments"))?;
    if !args.is_object() {
        anyhow::bail!("OpenAI function_call {call_id} arguments must be a JSON object");
    }
    Ok(args)
}

fn prepare_response_schema(schema: Value) -> Result<Value> {
    if !schema
        .get("type")
        .is_some_and(|schema_type| schema_type == "object")
    {
        anyhow::bail!("OpenAI Responses structured output schema must have an object root");
    }
    validate_response_schema_node(&schema)?;
    Ok(schema)
}

fn validate_response_schema_node(schema: &Value) -> Result<()> {
    let Value::Object(schema) = schema else {
        return Ok(());
    };

    let is_object = schema.get("type").is_some_and(|schema_type| {
        schema_type == "object"
            || schema_type
                .as_array()
                .is_some_and(|types| types.iter().any(|schema_type| schema_type == "object"))
    }) || schema.contains_key("properties");

    if is_object {
        match schema.get("additionalProperties") {
            Some(Value::Bool(false)) => {}
            _ => anyhow::bail!(
                "OpenAI Responses structured output schemas must set additionalProperties to false for every object"
            ),
        }

        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            let required = schema
                .get("required")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "OpenAI Responses structured output schemas must require every object property"
                    )
                })?;
            let missing: Vec<&str> = properties
                .keys()
                .map(String::as_str)
                .filter(|name| !required.iter().any(|required| required == *name))
                .collect();
            if !missing.is_empty() {
                anyhow::bail!(
                    "OpenAI Responses structured output schema has optional properties: {}",
                    missing.join(", ")
                );
            }
        }
    }

    for keyword in ["items", "contains", "not", "if", "then", "else"] {
        if let Some(subschema) = schema.get(keyword) {
            validate_response_schema_node(subschema)?;
        }
    }
    for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(subschemas) = schema.get(keyword).and_then(Value::as_array) {
            for subschema in subschemas {
                validate_response_schema_node(subschema)?;
            }
        }
    }
    for keyword in ["properties", "patternProperties", "$defs", "definitions"] {
        if let Some(subschemas) = schema.get(keyword).and_then(Value::as_object) {
            for subschema in subschemas.values() {
                validate_response_schema_node(subschema)?;
            }
        }
    }
    if let Some(subschemas) = schema.get("dependentSchemas").and_then(Value::as_object) {
        for subschema in subschemas.values() {
            validate_response_schema_node(subschema)?;
        }
    }
    Ok(())
}

fn record_pending_function_calls(
    output_items: &[Value],
    pending_call_ids: &mut HashSet<String>,
) -> Result<()> {
    for item in output_items {
        if item.get("type").and_then(Value::as_str) != Some("function_call") {
            continue;
        }
        required_non_empty_string(item, "id")
            .context("OpenAI Responses continuation function_call")?;
        let call_id = item.get("call_id").and_then(Value::as_str).ok_or_else(|| {
            anyhow::anyhow!("OpenAI Responses continuation function_call is missing call_id")
        })?;
        if call_id.trim().is_empty() {
            anyhow::bail!("OpenAI Responses continuation function_call has an empty call_id");
        }
        required_non_empty_string(item, "name")
            .context("OpenAI Responses continuation function_call")?;
        let arguments = item
            .get("arguments")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!("OpenAI Responses continuation function_call is missing arguments")
            })?;
        if serde_json::from_str::<Value>(arguments)
            .ok()
            .filter(Value::is_object)
            .is_none()
        {
            anyhow::bail!(
                "OpenAI Responses continuation function_call {call_id} has invalid or non-object arguments"
            );
        }
        if !pending_call_ids.insert(call_id.to_string()) {
            anyhow::bail!(
                "OpenAI Responses continuation contains duplicate pending call_id {call_id:?}"
            );
        }
    }
    Ok(())
}

// ── Translation ─────────────────────────────────────────────────

fn translate_ai_request(
    request: AiRequest,
    model: &str,
    max_tokens: u32,
    reasoning_effort: Option<&str>,
) -> Result<ResponsesRequest> {
    let mut input = Vec::new();
    let mut continuation_bytes = 0usize;
    let mut pending_call_ids = HashSet::new();

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
                    if output_items.len() > MAX_RESPONSE_OUTPUT_ITEMS {
                        anyhow::bail!(
                            "OpenAI Responses continuation metadata contains {} output items, exceeding the limit of {}",
                            output_items.len(),
                            MAX_RESPONSE_OUTPUT_ITEMS
                        );
                    }
                    let metadata_bytes = provider_metadata_size(metadata)?;
                    continuation_bytes = continuation_bytes.saturating_add(metadata_bytes);
                    if continuation_bytes > MAX_PROVIDER_METADATA_BYTES {
                        anyhow::bail!(
                            "OpenAI Responses continuation metadata is {} bytes, exceeding the limit of {}",
                            continuation_bytes,
                            MAX_PROVIDER_METADATA_BYTES
                        );
                    }
                    record_pending_function_calls(output_items, &mut pending_call_ids)?;
                    input.extend(output_items.iter().cloned().map(ResponsesInputItem::Raw));
                    continue;
                }
                if msg
                    .tool_calls
                    .as_ref()
                    .is_some_and(|tool_calls| !tool_calls.is_empty())
                {
                    anyhow::bail!(
                        "OpenAI Responses assistant tool calls require provider metadata"
                    );
                }
                // Assistant text content → "assistant" message
                if let Some(content) = msg.content {
                    input.push(ResponsesInputItem::Message {
                        role: "assistant".to_string(),
                        content,
                    });
                }
            }
            AiRole::Tool => {
                // Tool result → function_call_output
                let call_id = msg.tool_call_id.ok_or_else(|| {
                    anyhow::anyhow!("OpenAI Responses tool result is missing tool_call_id")
                })?;
                if call_id.trim().is_empty() {
                    anyhow::bail!("OpenAI Responses tool result has an empty tool_call_id");
                }
                let output = msg.content.ok_or_else(|| {
                    anyhow::anyhow!("OpenAI Responses tool result is missing content")
                })?;
                if !pending_call_ids.remove(&call_id) {
                    anyhow::bail!(
                        "OpenAI Responses tool result has unmatched or duplicate tool_call_id {call_id:?}"
                    );
                }
                input.push(ResponsesInputItem::FunctionCallOutput {
                    item_type: "function_call_output".to_string(),
                    call_id,
                    output,
                });
            }
        }
    }

    if !pending_call_ids.is_empty() {
        let mut missing: Vec<&str> = pending_call_ids.iter().map(String::as_str).collect();
        missing.sort_unstable();
        anyhow::bail!(
            "OpenAI Responses continuation is missing tool results for call IDs: {}",
            missing.join(", ")
        );
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

    let text = match request.response_format {
        Some(AiResponseFormat::Json {
            schema: Some(schema),
        }) => Some(TextConfig {
            format: TextFormatConfig {
                format_type: "json_schema".to_string(),
                name: Some("sashiko_response".to_string()),
                schema: Some(prepare_response_schema(schema)?),
                strict: Some(true),
            },
        }),
        Some(AiResponseFormat::Json { schema: None }) => Some(TextConfig {
            format: TextFormatConfig {
                format_type: "json_object".to_string(),
                name: None,
                schema: None,
                strict: None,
            },
        }),
        Some(AiResponseFormat::Text) => Some(TextConfig {
            format: TextFormatConfig {
                format_type: "text".to_string(),
                name: None,
                schema: None,
                strict: None,
            },
        }),
        None => None,
    };

    // JSON mode requires an explicit JSON instruction. Always use the same
    // leading message so later conversation content cannot change the cached
    // prompt prefix between turns.
    if text
        .as_ref()
        .is_some_and(|config| config.format.format_type == "json_object")
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
        temperature: if model_supports_temperature(model) {
            request.temperature
        } else {
            None
        },
        max_output_tokens: Some(max_tokens),
        reasoning,
        text,
        store: Some(false),
    })
}

const OPENAI_RESPONSES_PROVIDER_METADATA: &str = "openai.responses";
const OPENAI_RESPONSES_PROVIDER_METADATA_VERSION: u32 = 1;
pub(super) const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 16_384;

fn model_supports_temperature(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    !model.starts_with("gpt-5")
        && !model.starts_with("o1")
        && !model.starts_with("o3")
        && !model.starts_with("o4")
}

fn response_failed_error(status: &str, error: Option<&ResponsesError>) -> OpenAiError {
    let code = sanitize_provider_text(
        error
            .and_then(|error| error.code.as_deref())
            .unwrap_or("unknown_error"),
    );
    let message = sanitize_provider_text(
        error
            .and_then(|error| error.message.as_deref())
            .unwrap_or("response returned without error details"),
    );
    OpenAiError::ResponseFailed {
        status: sanitize_provider_text(status),
        code,
        message,
    }
}

fn required_non_empty_string<'a>(item: &'a Value, field: &str) -> Result<&'a str> {
    let value = item
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("OpenAI function_call is missing {field}"))?;
    if value.trim().is_empty() {
        anyhow::bail!("OpenAI function_call has empty {field}");
    }
    Ok(value)
}

fn translate_ai_response(resp: ResponsesResponse) -> Result<AiResponse> {
    if resp.output.len() > MAX_RESPONSE_OUTPUT_ITEMS {
        return Err(OpenAiError::TooManyOutputItems {
            count: resp.output.len(),
            limit: MAX_RESPONSE_OUTPUT_ITEMS,
        }
        .into());
    }

    let mut content_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut call_ids = HashSet::new();

    if resp.error.is_some() {
        return Err(response_failed_error(&resp.status, resp.error.as_ref()).into());
    }

    let truncated = match resp.status.as_str() {
        "completed" => false,
        "incomplete" => true,
        _ => return Err(response_failed_error(&resp.status, None).into()),
    };

    let provider_metadata = AiProviderMetadata {
        provider: OPENAI_RESPONSES_PROVIDER_METADATA.to_string(),
        version: OPENAI_RESPONSES_PROVIDER_METADATA_VERSION,
        data: Value::Array(resp.output),
    };
    let metadata_bytes = provider_metadata_size(&provider_metadata)?;
    if metadata_bytes > MAX_PROVIDER_METADATA_BYTES {
        return Err(OpenAiError::ContinuationTooLarge {
            size: metadata_bytes,
            limit: MAX_PROVIDER_METADATA_BYTES,
        }
        .into());
    }

    if truncated {
        let reason = resp
            .incomplete_details
            .as_ref()
            .and_then(|details| details.reason.as_deref())
            .unwrap_or("unknown");
        tracing::warn!(
            "{}OpenAI Responses response incomplete (reason={}, output_tokens={}).",
            crate::ai::get_log_prefix(),
            sanitize_provider_text(reason),
            resp.usage.output_tokens
        );
    }

    let output = provider_metadata
        .data
        .as_array()
        .expect("provider metadata was constructed from an output array");
    for item in output {
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
                            Some("refusal")
                                if part.get("refusal").and_then(Value::as_str).is_some() =>
                            {
                                tracing::warn!(
                                    "{}OpenAI model refused the request",
                                    crate::ai::get_log_prefix()
                                );
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some("function_call") => {
                required_non_empty_string(item, "id")?;
                let call_id = required_non_empty_string(item, "call_id")?;
                if !call_ids.insert(call_id) {
                    anyhow::bail!("OpenAI response contains duplicate call_id {call_id}");
                }
                let name = required_non_empty_string(item, "name")?;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("OpenAI function_call is missing arguments"))?;
                let args = parse_arguments_bounded(arguments, call_id)?;
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
        provider_metadata: Some(provider_metadata),
    })
}

#[async_trait]
impl AiProvider for OpenAiClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        tracing::info!(
            "{}Sending OpenAI Responses request...",
            crate::ai::get_log_prefix()
        );

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
                ("max_tokens", Some(max_tokens.as_str())),
                ("base_url", Some(self.base_url.as_str())),
                ("provider_type", Some("openai-responses")),
                ("reasoning_effort", self.reasoning_effort.as_deref()),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiMessage, AiResponseFormat, AiTool, classify_ai_error};
    use axum::{Json, Router, http::StatusCode, response::Redirect, routing::post};
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

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

    fn continuation_request(raw_call_ids: &[&str], tool_call_ids: &[&str]) -> AiRequest {
        let output = raw_call_ids
            .iter()
            .enumerate()
            .map(|(index, call_id)| {
                json!({
                    "type": "function_call",
                    "id": format!("fc_{index}"),
                    "call_id": call_id,
                    "name": "read_file",
                    "arguments": "{}"
                })
            })
            .collect();
        let mut messages = vec![AiMessage {
            role: AiRole::Assistant,
            content: None,
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
            provider_metadata: Some(AiProviderMetadata {
                provider: OPENAI_RESPONSES_PROVIDER_METADATA.to_string(),
                version: OPENAI_RESPONSES_PROVIDER_METADATA_VERSION,
                data: Value::Array(output),
            }),
        }];
        messages.extend(tool_call_ids.iter().map(|call_id| AiMessage {
            role: AiRole::Tool,
            content: Some("result".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: Some((*call_id).to_string()),
            provider_metadata: None,
        }));
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
            "GPT-5 reasoning models do not accept temperature"
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
            "GPT-5 reasoning models do not accept temperature"
        );
        Ok(())
    }

    #[test]
    fn test_translate_request_preserves_temperature_for_supported_models() -> Result<()> {
        for model in ["gpt-4o", "gpt-4o-2024-11-20", "future-chat-model"] {
            let request = AiRequest {
                system: None,
                messages: vec![user_msg("Test")],
                tools: None,
                temperature: Some(0.0),
                response_format: None,
                context_tag: None,
            };

            let req = translate_ai_request(request, model, 4096, None)?;
            assert_eq!(req.temperature, Some(0.0), "model: {model}");
        }
        Ok(())
    }

    #[test]
    fn test_translate_request_suppresses_temperature_for_reasoning_models() -> Result<()> {
        for model in ["gpt-5.6-sol", "gpt-5", "o1", "o3-mini", "o4-mini"] {
            let request = AiRequest {
                system: None,
                messages: vec![user_msg("Test")],
                tools: None,
                temperature: Some(0.0),
                response_format: None,
                context_tag: None,
            };

            let req = translate_ai_request(request, model, 4096, None)?;
            assert_eq!(req.temperature, None, "model: {model}");
        }
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
    fn test_translate_request_json_format_uses_stable_leading_instruction() -> Result<()> {
        let request = AiRequest {
            system: Some("Return JSON only.".to_string()),
            messages: vec![user_msg("JSON is also mentioned in the history.")],
            tools: None,
            temperature: None,
            response_format: Some(AiResponseFormat::Json { schema: None }),
            context_tag: None,
        };

        let req = translate_ai_request(request, "gpt-5.6-sol", 4096, None)?;
        assert_eq!(req.input.len(), 3);
        assert!(matches!(
            req.input.first(),
            Some(ResponsesInputItem::Message { role, content })
                if role == "system" && content == "Respond in JSON format."
        ));
        Ok(())
    }

    #[test]
    fn test_translate_request_uses_json_schema() -> Result<()> {
        let schema = json!({
            "type": "object",
            "properties": {
                "answer": {"type": "string"},
                "details": {
                    "type": "object",
                    "properties": {"source": {"type": "string"}},
                    "required": ["source"],
                    "additionalProperties": false
                }
            },
            "required": ["answer", "details"],
            "additionalProperties": false
        });
        let request = AiRequest {
            system: None,
            messages: vec![user_msg("Test")],
            tools: None,
            temperature: None,
            response_format: Some(AiResponseFormat::Json {
                schema: Some(schema.clone()),
            }),
            context_tag: None,
        };

        let req = translate_ai_request(request, "gpt-5.6-sol", 4096, None)?;
        let format = req.text.unwrap().format;
        assert_eq!(format.format_type, "json_schema");
        assert_eq!(format.name.as_deref(), Some("sashiko_response"));
        let translated = format.schema.unwrap();
        assert_eq!(translated, schema);
        assert_eq!(
            translated["properties"]["details"]["additionalProperties"],
            false
        );
        assert!(matches!(
            req.input.first(),
            Some(ResponsesInputItem::Message { role, .. }) if role == "user"
        ));
        Ok(())
    }

    #[test]
    fn test_schema_validation_only_visits_subschemas() -> Result<()> {
        let schema = json!({
            "type": "object",
            "properties": {
                "properties": {"type": "string"},
                "literal": {
                    "type": "string",
                    "const": {"type": "object", "properties": {}}
                }
            },
            "required": ["properties", "literal"],
            "additionalProperties": false
        });

        let translated = prepare_response_schema(schema.clone())?;
        assert_eq!(translated, schema);
        assert_eq!(
            translated["properties"]["literal"]["const"],
            json!({"type": "object", "properties": {}})
        );
        Ok(())
    }

    #[test]
    fn test_translate_request_rejects_incompatible_json_schema() {
        for (schema, expected) in [
            (
                json!({
                    "type": "object",
                    "properties": {"optional": {"type": "string"}},
                    "required": [],
                    "additionalProperties": false
                }),
                "optional properties: optional",
            ),
            (
                json!({
                    "type": "object",
                    "properties": {},
                    "required": [],
                    "additionalProperties": true
                }),
                "must set additionalProperties to false",
            ),
            (
                json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                }),
                "must set additionalProperties to false",
            ),
        ] {
            let request = AiRequest {
                system: None,
                messages: vec![user_msg("Test")],
                tools: None,
                temperature: None,
                response_format: Some(AiResponseFormat::Json {
                    schema: Some(schema),
                }),
                context_tag: None,
            };

            let error = translate_ai_request(request, "gpt-5.6-sol", 4096, None).unwrap_err();
            assert!(error.to_string().contains(expected));
        }
    }

    #[test]
    fn test_translate_request_rejects_unmanaged_function_calls() {
        let request = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::Assistant,
                content: Some("I will read the file.".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: Some(vec![ToolCall {
                    id: "call_7".to_string(),
                    function_name: "read_file".to_string(),
                    arguments: json!({"path": "/tmp/test"}),
                    thought_signature: None,
                }]),
                tool_call_id: None,
                provider_metadata: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let error = translate_ai_request(request, "gpt-5.6-sol", 4096, None).unwrap_err();
        assert!(error.to_string().contains("require provider metadata"));
    }

    #[test]
    fn test_translate_request_rejects_malformed_tool_results() {
        for (call_id, content, expected) in [
            (None, Some("result"), "missing tool_call_id"),
            (Some(""), Some("result"), "empty tool_call_id"),
            (Some("call_1"), None, "missing content"),
        ] {
            let request = AiRequest {
                system: None,
                messages: vec![AiMessage {
                    role: AiRole::Tool,
                    content: content.map(str::to_string),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: call_id.map(str::to_string),
                    provider_metadata: None,
                }],
                tools: None,
                temperature: None,
                response_format: None,
                context_tag: None,
            };

            let error = translate_ai_request(request, "gpt-5.6-sol", 4096, None).unwrap_err();
            assert!(error.to_string().contains(expected));
        }
    }

    #[test]
    fn test_translate_request_correlates_tool_results_to_raw_calls() -> Result<()> {
        translate_ai_request(
            continuation_request(&["call_1"], &["call_1"]),
            "gpt-5.6-sol",
            4096,
            None,
        )?;

        for (raw_call_ids, tool_call_ids, expected) in [
            (
                vec!["call_1"],
                vec!["call_unrelated"],
                "unmatched or duplicate tool_call_id",
            ),
            (
                vec!["call_1"],
                vec!["call_1", "call_1"],
                "unmatched or duplicate tool_call_id",
            ),
            (vec!["call_1"], vec![], "missing tool results for call IDs"),
            (
                vec!["call_1", "call_1"],
                vec!["call_1"],
                "duplicate pending call_id",
            ),
        ] {
            let error = translate_ai_request(
                continuation_request(&raw_call_ids, &tool_call_ids),
                "gpt-5.6-sol",
                4096,
                None,
            )
            .unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
        Ok(())
    }

    #[test]
    fn test_translate_request_rejects_invalid_provider_metadata() {
        for (version, data, expected) in [
            (
                OPENAI_RESPONSES_PROVIDER_METADATA_VERSION + 1,
                json!([]),
                "unsupported OpenAI Responses provider metadata version",
            ),
            (
                OPENAI_RESPONSES_PROVIDER_METADATA_VERSION,
                json!({}),
                "metadata must contain an array",
            ),
        ] {
            let request = AiRequest {
                system: None,
                messages: vec![AiMessage {
                    role: AiRole::Assistant,
                    content: None,
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                    provider_metadata: Some(AiProviderMetadata {
                        provider: OPENAI_RESPONSES_PROVIDER_METADATA.to_string(),
                        version,
                        data,
                    }),
                }],
                tools: None,
                temperature: None,
                response_format: None,
                context_tag: None,
            };

            let error = translate_ai_request(request, "gpt-5.6-sol", 4096, None).unwrap_err();
            assert!(error.to_string().contains(expected));
        }
    }

    #[test]
    fn test_translate_request_rejects_excessive_replayed_output_items() {
        let request = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::Assistant,
                content: None,
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
                provider_metadata: Some(AiProviderMetadata {
                    provider: OPENAI_RESPONSES_PROVIDER_METADATA.to_string(),
                    version: OPENAI_RESPONSES_PROVIDER_METADATA_VERSION,
                    data: Value::Array(vec![Value::Null; MAX_RESPONSE_OUTPUT_ITEMS + 1]),
                }),
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let error = translate_ai_request(request, "gpt-5.6-sol", 4096, None).unwrap_err();
        assert!(error.to_string().contains("4097 output items"));
    }

    #[test]
    fn test_translate_response_rejects_incomplete_function_calls() {
        for (field, expected) in [
            ("id", "missing id"),
            ("call_id", "missing call_id"),
            ("name", "missing name"),
            ("arguments", "missing arguments"),
        ] {
            let mut function_call = json!({
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "read_file",
                "arguments": r#"{"path":"/tmp/test"}"#
            });
            function_call.as_object_mut().unwrap().remove(field);
            let resp = ResponsesResponse {
                id: "resp_incomplete_call".to_string(),
                status: "completed".to_string(),
                error: None,
                output: vec![function_call],
                incomplete_details: None,
                usage: ResponsesUsage::default(),
            };

            let error = translate_ai_response(resp).unwrap_err();
            assert!(error.to_string().contains(expected));
        }
    }

    #[test]
    fn test_translate_response_rejects_empty_and_duplicate_function_call_ids() {
        for field in ["id", "call_id", "name"] {
            let mut function_call = json!({
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "read_file",
                "arguments": "{}"
            });
            function_call[field] = json!("");
            let response = ResponsesResponse {
                id: "resp_empty_call".to_string(),
                status: "completed".to_string(),
                error: None,
                output: vec![function_call],
                incomplete_details: None,
                usage: ResponsesUsage::default(),
            };

            let error = translate_ai_response(response).unwrap_err();
            assert!(error.to_string().contains(&format!("empty {field}")));
        }

        let function_call = json!({
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_1",
            "name": "read_file",
            "arguments": "{}"
        });
        let mut duplicate = function_call.clone();
        duplicate["id"] = json!("fc_2");
        let response = ResponsesResponse {
            id: "resp_duplicate_call".to_string(),
            status: "completed".to_string(),
            error: None,
            output: vec![function_call, duplicate],
            incomplete_details: None,
            usage: ResponsesUsage::default(),
        };

        let error = translate_ai_response(response).unwrap_err();
        assert!(error.to_string().contains("duplicate call_id call_1"));
    }

    #[test]
    fn test_translate_response_rejects_invalid_function_arguments() {
        for (arguments, expected) in [
            ("not json", "invalid JSON arguments"),
            ("null", "arguments must be a JSON object"),
            ("[]", "arguments must be a JSON object"),
        ] {
            let resp = ResponsesResponse {
                id: "resp_invalid_arguments".to_string(),
                status: "completed".to_string(),
                error: None,
                output: vec![json!({
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "read_file",
                    "arguments": arguments
                })],
                incomplete_details: None,
                usage: ResponsesUsage::default(),
            };

            let error = translate_ai_response(resp).unwrap_err();
            assert!(error.to_string().contains(expected));
        }
    }

    #[test]
    fn test_translate_response_rejects_complex_function_arguments() {
        let deep_args =
            serde_json::to_string(&json!({"items": vec![Value::Null; MAX_RESPONSE_JSON_VALUES]}))
                .unwrap();
        let resp = ResponsesResponse {
            id: "resp_complex_args".to_string(),
            status: "completed".to_string(),
            error: None,
            output: vec![json!({
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "read_file",
                "arguments": deep_args
            })],
            incomplete_details: None,
            usage: ResponsesUsage::default(),
        };

        let error = translate_ai_response(resp).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("arguments exceed the JSON complexity limit")
        );
    }

    #[test]
    fn test_translate_request_json_schema_enables_strict_mode() -> Result<()> {
        let schema = json!({
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"],
            "additionalProperties": false
        });
        let request = AiRequest {
            system: None,
            messages: vec![user_msg("Test")],
            tools: None,
            temperature: None,
            response_format: Some(AiResponseFormat::Json {
                schema: Some(schema),
            }),
            context_tag: None,
        };

        let req = translate_ai_request(request, "gpt-5.6-sol", 4096, None)?;
        let format = req.text.unwrap().format;
        assert_eq!(format.strict, Some(true));
        Ok(())
    }

    #[test]
    fn test_translate_request_opts_out_of_server_side_storage() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![user_msg("Test")],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let req = translate_ai_request(request, "gpt-5.6-sol", 4096, None)?;
        assert_eq!(req.store, Some(false));
        Ok(())
    }

    #[test]
    fn test_continuation_rejects_replayed_function_call_with_missing_fields() {
        for (field, expected) in [
            ("id", "missing id"),
            ("name", "missing name"),
            ("arguments", "missing arguments"),
        ] {
            let mut function_call = json!({
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "read_file",
                "arguments": "{}"
            });
            function_call.as_object_mut().unwrap().remove(field);
            let request = AiRequest {
                system: None,
                messages: vec![
                    AiMessage {
                        role: AiRole::Assistant,
                        content: None,
                        thought: None,
                        thought_signature: None,
                        tool_calls: None,
                        tool_call_id: None,
                        provider_metadata: Some(AiProviderMetadata {
                            provider: OPENAI_RESPONSES_PROVIDER_METADATA.to_string(),
                            version: OPENAI_RESPONSES_PROVIDER_METADATA_VERSION,
                            data: Value::Array(vec![function_call]),
                        }),
                    },
                    AiMessage {
                        role: AiRole::Tool,
                        content: Some("result".to_string()),
                        thought: None,
                        thought_signature: None,
                        tool_calls: None,
                        tool_call_id: Some("call_1".to_string()),
                        provider_metadata: None,
                    },
                ],
                tools: None,
                temperature: None,
                response_format: None,
                context_tag: None,
            };

            let error = translate_ai_request(request, "gpt-5.6-sol", 4096, None).unwrap_err();
            assert!(
                format!("{error:#}").contains(expected),
                "field={field}: {error:#}"
            );
        }
    }

    #[test]
    fn test_continuation_rejects_replayed_function_call_with_invalid_arguments() {
        for arguments in ["not json", "[]", "null"] {
            let function_call = json!({
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "read_file",
                "arguments": arguments
            });
            let request = AiRequest {
                system: None,
                messages: vec![
                    AiMessage {
                        role: AiRole::Assistant,
                        content: None,
                        thought: None,
                        thought_signature: None,
                        tool_calls: None,
                        tool_call_id: None,
                        provider_metadata: Some(AiProviderMetadata {
                            provider: OPENAI_RESPONSES_PROVIDER_METADATA.to_string(),
                            version: OPENAI_RESPONSES_PROVIDER_METADATA_VERSION,
                            data: Value::Array(vec![function_call]),
                        }),
                    },
                    AiMessage {
                        role: AiRole::Tool,
                        content: Some("result".to_string()),
                        thought: None,
                        thought_signature: None,
                        tool_calls: None,
                        tool_call_id: Some("call_1".to_string()),
                        provider_metadata: None,
                    },
                ],
                tools: None,
                temperature: None,
                response_format: None,
                context_tag: None,
            };

            let error = translate_ai_request(request, "gpt-5.6-sol", 4096, None).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("invalid or non-object arguments"),
                "arguments={arguments}: {error:#}"
            );
        }
    }

    #[test]
    fn test_translate_request_rejects_cumulative_continuation_metadata() {
        let messages = (0..2)
            .map(|_| AiMessage {
                role: AiRole::Assistant,
                content: None,
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
                provider_metadata: Some(AiProviderMetadata {
                    provider: OPENAI_RESPONSES_PROVIDER_METADATA.to_string(),
                    version: OPENAI_RESPONSES_PROVIDER_METADATA_VERSION,
                    data: json!([{"opaque": "x".repeat(MAX_PROVIDER_METADATA_BYTES / 2)}]),
                }),
            })
            .collect();
        let request = AiRequest {
            system: None,
            messages,
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let error = translate_ai_request(request, "gpt-5.6-sol", 4096, None).unwrap_err();
        assert!(error.to_string().contains("continuation metadata"));
    }

    #[test]
    fn test_translate_response_rejects_oversized_continuation_metadata() {
        let resp = ResponsesResponse {
            id: "resp_large".to_string(),
            status: "completed".to_string(),
            error: None,
            output: vec![json!({"opaque": "x".repeat(MAX_PROVIDER_METADATA_BYTES)})],
            incomplete_details: None,
            usage: ResponsesUsage::default(),
        };

        let error = translate_ai_response(resp).unwrap_err();
        assert!(error.to_string().contains("continuation metadata"));
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
            error: None,
            output: output.clone(),
            incomplete_details: None,
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
            error: None,
            output: vec![],
            incomplete_details: Some(ResponsesIncompleteDetails {
                reason: Some("max_output_tokens".to_string()),
            }),
            usage: ResponsesUsage {
                output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
                ..ResponsesUsage::default()
            },
        };

        assert_eq!(
            resp.incomplete_details
                .as_ref()
                .and_then(|details| details.reason.as_deref()),
            Some("max_output_tokens")
        );

        let ai_resp = translate_ai_response(resp)?;
        assert!(ai_resp.truncated);
        Ok(())
    }

    #[test]
    fn test_translate_response_usage_mapping() -> Result<()> {
        let resp = ResponsesResponse {
            id: "resp_4".to_string(),
            status: "completed".to_string(),
            error: None,
            output: vec![],
            incomplete_details: None,
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
    fn test_translate_response_failed_surfaces_provider_error() -> Result<()> {
        let resp: ResponsesResponse = serde_json::from_value(json!({
            "id": "resp_failed",
            "status": "failed",
            "error": {
                "code": "server_error",
                "message": "The model failed while generating a response."
            },
            "output": [],
            "usage": null
        }))?;

        let error = translate_ai_response(resp).unwrap_err();
        assert_eq!(
            error.to_string(),
            "OpenAI Responses generation failed (status=failed, code=server_error): \
             The model failed while generating a response."
        );
        assert_eq!(
            classify_ai_error(&error),
            AiErrorClass::Transient {
                retry_after: DEFAULT_RETRY_AFTER
            }
        );
        Ok(())
    }

    #[test]
    fn test_translate_response_non_transient_failure_is_fatal() -> Result<()> {
        let resp: ResponsesResponse = serde_json::from_value(json!({
            "id": "resp_failed",
            "status": "failed",
            "error": {
                "code": "invalid_prompt",
                "message": "The prompt was rejected."
            },
            "output": []
        }))?;

        let error = translate_ai_response(resp).unwrap_err();
        assert_eq!(classify_ai_error(&error), AiErrorClass::Fatal);
        Ok(())
    }

    #[test]
    fn test_translate_response_rejects_excessive_output_items() {
        let resp = ResponsesResponse {
            id: "resp_large".to_string(),
            status: "completed".to_string(),
            error: None,
            output: vec![Value::Null; MAX_RESPONSE_OUTPUT_ITEMS + 1],
            incomplete_details: None,
            usage: ResponsesUsage::default(),
        };

        let error = translate_ai_response(resp).unwrap_err();
        assert!(error.to_string().contains("4097 output items"));
        assert_eq!(classify_ai_error(&error), AiErrorClass::Fatal);
    }

    #[test]
    fn test_translate_response_error_wins_over_completed_status() -> Result<()> {
        let resp: ResponsesResponse = serde_json::from_value(json!({
            "id": "resp_conflict",
            "status": "completed",
            "error": {"code": "server_error", "message": "Generation failed."},
            "output": []
        }))?;

        let error = translate_ai_response(resp).unwrap_err();
        assert!(error.to_string().contains("Generation failed."));
        Ok(())
    }

    #[test]
    fn test_translate_response_rejects_unexpected_statuses() {
        for status in ["failed", "cancelled", "queued", "in_progress", "future"] {
            let resp = ResponsesResponse {
                id: "resp_unexpected".to_string(),
                status: status.to_string(),
                error: None,
                output: vec![],
                incomplete_details: None,
                usage: ResponsesUsage::default(),
            };

            let error = translate_ai_response(resp).unwrap_err();
            assert!(error.to_string().contains(&format!("status={status}")));
            assert_eq!(classify_ai_error(&error), AiErrorClass::Fatal);
        }
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

    #[test]
    fn test_cache_identity_distinguishes_protocol_and_reasoning_effort() -> Result<()> {
        let base = OpenAiClient::new(
            OpenAiClient::default_base_url(),
            "gpt-5.6-sol".to_string(),
            1_050_000,
            DEFAULT_MAX_OUTPUT_TOKENS,
            None,
            60,
        )?;
        let reasoning = OpenAiClient::new(
            OpenAiClient::default_base_url(),
            "gpt-5.6-sol".to_string(),
            1_050_000,
            DEFAULT_MAX_OUTPUT_TOKENS,
            Some("high".to_string()),
            60,
        )?;

        assert!(
            base.cache_identity()
                .contains("provider_type=openai-responses")
        );
        assert_ne!(base.cache_identity(), reasoning.cache_identity());
        Ok(())
    }

    #[test]
    fn test_normalize_base_url() -> Result<()> {
        for (input, expected) in [
            ("http://localhost:8080", "http://localhost:8080/responses"),
            ("http://localhost:8080/", "http://localhost:8080/responses"),
            (
                "http://localhost:8080/v1",
                "http://localhost:8080/v1/responses",
            ),
            (
                "http://localhost:8080/v1/",
                "http://localhost:8080/v1/responses",
            ),
            (
                "https://proxy.example/api/v1",
                "https://proxy.example/api/v1/responses",
            ),
            (
                "https://proxy.example/api/v1/",
                "https://proxy.example/api/v1/responses",
            ),
            (
                "https://api.openai.com/v1/responses",
                "https://api.openai.com/v1/responses",
            ),
            (
                "https://proxy.example/openai/v1/responses/",
                "https://proxy.example/openai/v1/responses",
            ),
        ] {
            assert_eq!(OpenAiClient::normalize_base_url(input)?, expected);
        }

        assert!(OpenAiClient::normalize_base_url("not-a-url").is_err());
        assert!(OpenAiClient::normalize_base_url("https://proxy.example/v2").is_err());
        assert!(OpenAiClient::normalize_base_url("http://proxy.example/v1").is_err());
        assert!(OpenAiClient::normalize_base_url("http://192.0.2.1/v1").is_err());
        assert_eq!(
            OpenAiClient::normalize_base_url("http://127.0.0.1:8080/v1")?,
            "http://127.0.0.1:8080/v1/responses"
        );
        assert_eq!(
            OpenAiClient::normalize_base_url("http://[::1]:8080/v1")?,
            "http://[::1]:8080/v1/responses"
        );
        Ok(())
    }

    #[test]
    fn test_retry_after_prefers_header_and_bounds_delay() {
        assert_eq!(
            retry_after_delay(Some("7"), "Please retry in 11s"),
            Duration::from_secs(7)
        );
        assert_eq!(
            retry_after_delay(None, "Please retry in 2.5s"),
            Duration::from_millis(2_500)
        );
        assert_eq!(
            retry_after_delay(Some("99999999999999999999"), ""),
            MAX_RETRY_AFTER
        );
        assert_eq!(
            retry_after_delay(None, "Please retry in 99999999999999999999s"),
            MAX_RETRY_AFTER
        );
        assert_eq!(retry_after_delay(Some("invalid"), ""), DEFAULT_RETRY_AFTER);
    }

    #[test]
    fn test_decode_response_rejects_excessive_json_values_before_building_tree() {
        let body = serde_json::to_vec(&json!({
            "id": "resp_complex",
            "status": "completed",
            "output": [{"nested": vec![Value::Null; MAX_RESPONSE_JSON_VALUES]}]
        }))
        .unwrap();

        let error = decode_response(&body).unwrap_err();
        assert!(error.to_string().contains("too many JSON values"));
    }

    #[test]
    fn test_provider_log_text_is_bounded_redacted_and_escaped() {
        let input = format!(
            "Authorization: Bearer sk-secret\n{}",
            "x".repeat(MAX_PROVIDER_LOG_BYTES)
        );
        let sanitized = sanitize_provider_text(&input);

        assert!(sanitized.contains("Bearer [REDACTED]"));
        assert!(!sanitized.contains("sk-secret"));
        assert!(!sanitized.contains('\n'));
        assert!(sanitized.ends_with("...[truncated]"));
    }

    #[tokio::test]
    async fn test_response_body_limit() {
        let response = axum::http::Response::builder().body(vec![0_u8; 5]).unwrap();
        let mut response = reqwest::Response::from(response);

        let error = OpenAiClient::read_response_body_with_limit(&mut response, 4)
            .await
            .unwrap_err();
        assert!(matches!(error, OpenAiError::ResponseTooLarge { limit: 4 }));
    }

    #[test]
    fn test_create_http_client_rejects_invalid_api_key() {
        let error = OpenAiClient::create_http_client("invalid\nkey", 60, false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("OpenAI API key is not a valid HTTP header value")
        );
    }

    #[tokio::test]
    async fn test_http_client_does_not_follow_redirects() -> Result<()> {
        let redirect_hits = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&redirect_hits);
        let redirect_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let redirect_address = redirect_listener.local_addr()?;
        let redirect_server = tokio::spawn(async move {
            axum::serve(
                redirect_listener,
                Router::new().route(
                    "/redirected",
                    post(move || {
                        let hits = Arc::clone(&hits);
                        async move {
                            hits.fetch_add(1, Ordering::SeqCst);
                            Json(json!({
                                "id": "resp_redirected",
                                "status": "completed",
                                "output": []
                            }))
                        }
                    }),
                ),
            )
            .await
            .unwrap();
        });

        let endpoint_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint_address = endpoint_listener.local_addr()?;
        let redirect_url = format!("http://{redirect_address}/redirected");
        let endpoint_server = tokio::spawn(async move {
            axum::serve(
                endpoint_listener,
                Router::new().route(
                    "/responses",
                    post(move || {
                        let redirect_url = redirect_url.clone();
                        async move { Redirect::temporary(&redirect_url) }
                    }),
                ),
            )
            .await
            .unwrap();
        });

        let client = OpenAiClient {
            model: "gpt-5.6-sol".to_string(),
            base_url: format!("http://{endpoint_address}/responses"),
            context_window_size: 1_050_000,
            max_tokens: 4096,
            reasoning_effort: None,
            client: OpenAiClient::create_http_client("secret", 60, true)?,
        };
        let error = client.post_request(&json!({})).await.unwrap_err();

        assert!(matches!(
            error,
            OpenAiError::ApiError(StatusCode::TEMPORARY_REDIRECT, _)
        ));
        assert_eq!(redirect_hits.load(Ordering::SeqCst), 0);
        endpoint_server.abort();
        redirect_server.abort();
        Ok(())
    }
}
