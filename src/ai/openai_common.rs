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

//! Plumbing common to the OpenAI transports: the pieces that depend on the
//! vendor rather than on the endpoint the request goes to.  Authentication,
//! error classification, retry timing, and token estimation are identical
//! whichever endpoint serves the request; only the request and response
//! bodies differ.

use crate::ai::{AiErrorClass, ClassifyAiError, classify_status_code};
use crate::utils::redact_secret;
use anyhow::{Context, Result};
use regex::Regex;
use reqwest::Client;
use reqwest::header::HeaderMap;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum OpenAiCompatError {
    #[error("Rate limit exceeded, retry after {0:?}")]
    RateLimitExceeded(Duration),
    /// A 429 the endpoint will keep returning until the account is funded.
    /// It shares a status with the rate limit but not a remedy.
    #[error("Quota exhausted: {0}")]
    QuotaExhausted(String),
    #[error("Transient error: {1}, retry after {0:?}")]
    TransientError(Duration, String),
    #[error("Authentication error: {0}")]
    AuthenticationError(String),
    #[error("API error {0}: {1}")]
    ApiError(reqwest::StatusCode, String),
    /// A failure the endpoint reports inside a 200 reply rather than as an
    /// HTTP status.  Only /v1/responses does this.
    #[error("Response failed: {0}")]
    ResponseFailed(String),
}

impl ClassifyAiError for OpenAiCompatError {
    fn ai_error_class(&self) -> AiErrorClass {
        match self {
            OpenAiCompatError::RateLimitExceeded(retry_after) => AiErrorClass::RateLimit {
                retry_after: *retry_after,
            },
            OpenAiCompatError::QuotaExhausted(_) => AiErrorClass::Fatal,
            OpenAiCompatError::TransientError(retry_after, _) => AiErrorClass::Transient {
                retry_after: *retry_after,
            },
            OpenAiCompatError::AuthenticationError(_) => AiErrorClass::Fatal,
            OpenAiCompatError::ApiError(status, _) => {
                classify_status_code(*status).unwrap_or(AiErrorClass::Fatal)
            }
            OpenAiCompatError::ResponseFailed(_) => AiErrorClass::Fatal,
        }
    }
}

/// Deserializes a usage sub-object, dropping it when it does not fit the
/// shape we model.  The counts are accounting, and losing them costs less
/// than losing a completion that arrived intact.
pub fn lenient_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or(None))
}

/// Builds the HTTP client both transports use.  The key comes from the
/// environment rather than from settings, so an instance that has none
/// still builds a client and fails at the endpoint with a 401.  A builder
/// failure is returned rather than papered over with a bare client: that
/// one would carry neither the key nor the timeout, and its 401 would
/// point at the key.
pub fn build_http_client(api_timeout_secs: u64) -> Result<Client> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("LLM_API_KEY"))
        .unwrap_or_default();

    let mut headers = reqwest::header::HeaderMap::new();
    if !api_key.is_empty()
        && let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", api_key))
    {
        headers.insert("Authorization", value);
    }

    reqwest::Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(api_timeout_secs))
        .build()
        .context("Failed to build the OpenAI HTTP client")
}

/// Normalize a base URL so it always ends with `endpoint_path`.
///
/// LM Studio and other OpenAI-compatible servers document the base URL as
/// `http://localhost:1234/v1`, expecting the client to append the endpoint
/// path.  A client POSTs directly to the URL this returns, so the full path
/// has to be present.
pub fn normalize_base_url(url: &str, endpoint_path: &str) -> Result<String> {
    let trimmed = url.trim_end_matches('/');

    let (base, path) = match trimmed.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('/') {
            Some((host, path)) => (format!("{scheme}://{host}"), format!("/{}", path)),
            None => (trimmed.to_string(), String::new()),
        },
        None => return Err(anyhow::anyhow!("Invalid url scheme in OpenAI url {}", url)),
    };

    // If the caller supplied a full URL that already targets the endpoint,
    // accept it verbatim. This allows any OpenAI-compatible provider to be
    // configured via `base_url` alone, including endpoints whose path is not
    // otherwise recognised such as z.ai's coding-plan gateway
    // (https://api.z.ai/api/coding/paas/v4/chat/completions).
    if path.ends_with(endpoint_path) {
        return Ok(format!("{base}{path}"));
    }

    let path = match path.as_str() {
        "" => endpoint_path.to_string(),
        "/v1" => format!("/v1{endpoint_path}"),
        "/api/v1" => format!("/api/v1{endpoint_path}"),
        _ => return Err(anyhow::anyhow!("Invalid OpenAI url {}", url)),
    };

    Ok(format!("{base}{path}"))
}

/// Whether an error is the OpenAI API, or a compatible endpoint, refusing
/// the temperature field for this model.  The API names the field in param
/// and sends type invalid_request_error; code is not consulted for that
/// shape, because /v1/chat/completions sends unsupported_value where
/// /v1/responses sends null for the same refusal.  Gateways that leave
/// param out are recognized by the message text instead.
pub fn rejects_temperature_parameter(error: &OpenAiCompatError) -> bool {
    let OpenAiCompatError::ApiError(status, body) = error else {
        return false;
    };
    if !matches!(
        *status,
        reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::UNPROCESSABLE_ENTITY
    ) {
        return false;
    }

    let parsed = serde_json::from_str::<Value>(body).ok();
    let detail = parsed.as_ref().map(|value| &value["error"]);
    let param = detail.and_then(|value| value["param"].as_str());
    let kind = detail.and_then(|value| value["type"].as_str());
    let code = detail.and_then(|value| value["code"].as_str());
    if param == Some("temperature")
        && (kind == Some("invalid_request_error") || code == Some("unsupported_parameter"))
    {
        return true;
    }

    let message = detail
        .and_then(|value| value["message"].as_str())
        .unwrap_or(body)
        .to_ascii_lowercase();
    message.contains("unsupported parameter: 'temperature'")
        || message.contains("unsupported parameter: \"temperature\"")
        || message.contains("unsupported parameter: temperature")
        || message.contains("temperature is not supported")
        || message.contains("'temperature' is not supported")
        || message.contains("does not support temperature") // LiteLLM
}

fn strip_temperature(body: &mut Value) -> bool {
    body.as_object_mut()
        .and_then(|fields| fields.remove("temperature"))
        .is_some()
}

/// Whether a 429 body reports an exhausted account rather than a rate
/// limit.  Both arrive as 429; only the body separates them.  Match on
/// `error.type`, which has named this condition across several wordings
/// of the message and of `error.code`.  A body that will not parse falls
/// back to a substring search: mistaking a rate limit for exhaustion
/// ends one review, while the reverse retries forever.
fn is_insufficient_quota(error_text: &str) -> bool {
    match serde_json::from_str::<Value>(error_text) {
        Ok(body) => body["error"]["type"] == "insufficient_quota",
        Err(_) => error_text.contains("insufficient_quota"),
    }
}

/// POSTs `body` to `url` and decodes the reply, mapping every failure to the
/// class the retry path acts on.  The endpoint decides the shape of `T`; the
/// status handling above it does not depend on which endpoint answered.
///
/// A reasoning model samples at a fixed temperature of 1 and answers any
/// other value with a 400 or a 422.  The planning phases ask for 0.0, so that
/// status, which classifies as fatal, would end the review before the
/// model sees the patch.  On such a reply the field is dropped and the
/// request sent once more, and `temperature_unsupported` records the
/// refusal so later requests through the same client omit the field up
/// front.  The refusal is learned from the reply rather than the model
/// name, so a new model or a compatible endpoint needs no list to
/// maintain.
pub async fn post_json<T: DeserializeOwned>(
    client: &Client,
    url: &str,
    headers: &HeaderMap,
    body: &mut Value,
    temperature_unsupported: &AtomicBool,
) -> Result<T, OpenAiCompatError> {
    if temperature_unsupported.load(Ordering::Relaxed) {
        strip_temperature(body);
    }
    match post_json_once(client, url, headers, body).await {
        Err(ref error) if rejects_temperature_parameter(error) && strip_temperature(body) => {
            tracing::info!("{url} takes only the default temperature; resending without the field");
            temperature_unsupported.store(true, Ordering::Relaxed);
            post_json_once(client, url, headers, body).await
        }
        other => other,
    }
}

/// The pattern a 429 body states its retry hint in, compiled once for the
/// process rather than on every request.  The literal is fixed, so the
/// compile cannot fail.
fn retry_in_regex() -> &'static Regex {
    static RETRY_IN: OnceLock<Regex> = OnceLock::new();
    RETRY_IN.get_or_init(|| {
        Regex::new(r"Please retry in ([0-9.]+)s").expect("the retry hint pattern is a valid regex")
    })
}

async fn post_json_once<T: DeserializeOwned>(
    client: &Client,
    url: &str,
    headers: &HeaderMap,
    body: &Value,
) -> Result<T, OpenAiCompatError> {
    // Request headers replace default headers of the same name, so an extra
    // authorization header overrides the bearer token.
    let request = client.post(url).headers(headers.clone()).json(body);
    let res = match request.send().await {
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
        return match serde_json::from_str::<T>(&body_text) {
            Ok(response) => Ok(response),
            Err(e) => {
                tracing::error!("Failed to decode OpenAI response: {}", e);
                Err(OpenAiCompatError::ApiError(
                    status,
                    format!("Parse error: {}", e),
                ))
            }
        };
    }

    let status = res.status();
    let status_code = status.as_u16();

    let retry_after_duration = res
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs);

    let error_text = redact_secret(&res.text().await.unwrap_or_default());

    match status_code {
        429 if is_insufficient_quota(&error_text) => {
            tracing::error!("OpenAI quota exhausted: {}", error_text);
            Err(OpenAiCompatError::QuotaExhausted(error_text))
        }
        429 => {
            let default_retry = retry_after_duration.unwrap_or(Duration::from_secs(60));
            let retry_duration = retry_in_regex()
                .captures(&error_text)
                .and_then(|caps| caps[1].parse::<f64>().ok())
                .and_then(|secs| Duration::try_from_secs_f64(secs).ok())
                .unwrap_or(default_retry);
            tracing::warn!(
                "OpenAI 429 Rate Limit. Retry in {}s",
                retry_duration.as_secs_f64()
            );
            Err(OpenAiCompatError::RateLimitExceeded(retry_duration))
        }
        401 | 403 => Err(OpenAiCompatError::AuthenticationError(error_text)),
        500..=599 => {
            tracing::warn!("OpenAI Server Error {}: {}", status, error_text);
            Err(OpenAiCompatError::TransientError(
                retry_after_duration.unwrap_or(Duration::from_secs(0)),
                error_text,
            ))
        }
        _ => Err(OpenAiCompatError::ApiError(status, error_text)),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ai::DEFAULT_RETRY_AFTER;

    const CHAT: &str = "/chat/completions";

    /// The bodies gpt-6-luna and gpt-5.6-sol returned for temperature 0.0
    /// on 2026-09-24, from /v1/chat/completions and from /v1/responses.
    pub(crate) const TEMPERATURE_REFUSED: &str = r#"{
  "error": {
    "message": "Unsupported value: 'temperature' does not support 0.0 with this model. Only the default (1) value is supported.",
    "type": "invalid_request_error",
    "param": "temperature",
    "code": "unsupported_value"
  }
}"#;
    pub(crate) const TEMPERATURE_REFUSED_RESPONSES: &str = r#"{
  "error": {
    "message": "Unsupported parameter: 'temperature' is not supported with this model.",
    "type": "invalid_request_error",
    "param": "temperature",
    "code": null
  }
}"#;

    fn refused(body: &str) -> OpenAiCompatError {
        OpenAiCompatError::ApiError(reqwest::StatusCode::BAD_REQUEST, body.to_string())
    }

    #[test]
    fn rejects_temperature_parameter_reads_type_and_param() {
        assert!(rejects_temperature_parameter(&refused(TEMPERATURE_REFUSED)));
        assert!(rejects_temperature_parameter(&refused(
            TEMPERATURE_REFUSED_RESPONSES
        )));

        // Another unsupported field, and another kind of error naming the
        // temperature field, both keep the value in the request.
        let other_param =
            TEMPERATURE_REFUSED.replace(r#""param": "temperature""#, r#""param": "top_p""#);
        assert!(!rejects_temperature_parameter(&refused(&other_param)));
        let other_type = TEMPERATURE_REFUSED.replace(
            r#""type": "invalid_request_error""#,
            r#""type": "server_error""#,
        );
        assert!(!rejects_temperature_parameter(&refused(&other_type)));

        assert!(!rejects_temperature_parameter(&refused("")));
    }

    #[test]
    fn rejects_temperature_parameter_matches_only_explicit_unsupported_errors() {
        let error = |status, message: &str| {
            OpenAiCompatError::ApiError(
                status,
                serde_json::json!({"error": {"message": message}}).to_string(),
            )
        };
        assert!(rejects_temperature_parameter(&error(
            reqwest::StatusCode::BAD_REQUEST,
            "Unsupported parameter: 'temperature' is not supported with this model."
        )));
        assert!(rejects_temperature_parameter(&OpenAiCompatError::ApiError(
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            serde_json::json!({"error": {"param": "temperature", "code": "unsupported_parameter"}})
                .to_string(),
        )));
        assert!(!rejects_temperature_parameter(&error(
            reqwest::StatusCode::BAD_REQUEST,
            "Temperature must be between 0 and 1"
        )));
        assert!(!rejects_temperature_parameter(&error(
            reqwest::StatusCode::BAD_REQUEST,
            "Unsupported parameter: 'max_tokens'"
        )));
        assert!(!rejects_temperature_parameter(&error(
            reqwest::StatusCode::UNAUTHORIZED,
            "Unsupported parameter: 'temperature'"
        )));
    }

    /// Serve one canned reply per connection, in order, and record each
    /// request body.
    pub(crate) async fn serve_replies(
        replies: Vec<(u16, String)>,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        tokio::spawn(async move {
            for (status, body) in replies {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let mut n = 0;
                let request = loop {
                    n += sock.read(&mut buf[n..]).await.unwrap();
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    if let Some((head, tail)) = text.split_once("\r\n\r\n") {
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length: "))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        if tail.len() >= len {
                            break tail.to_string();
                        }
                    }
                };
                recorder
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&request).unwrap());
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(reply.as_bytes()).await.unwrap();
                sock.shutdown().await.unwrap();
            }
        });
        (format!("http://{addr}/v1"), seen)
    }

    #[test]
    fn strip_temperature_reports_whether_the_field_was_present() {
        let mut body = serde_json::json!({"model": "m", "temperature": 0.0});
        assert!(strip_temperature(&mut body));
        assert!(body.get("temperature").is_none());
        assert!(!strip_temperature(&mut body));
        assert_eq!(body["model"], "m");
    }

    #[test]
    fn test_openai_compat_error_classification() {
        let cases = [
            (
                OpenAiCompatError::RateLimitExceeded(Duration::from_secs(7)),
                AiErrorClass::RateLimit {
                    retry_after: Duration::from_secs(7),
                },
            ),
            (
                OpenAiCompatError::TransientError(Duration::from_secs(11), "busy".to_string()),
                AiErrorClass::Transient {
                    retry_after: Duration::from_secs(11),
                },
            ),
            (
                OpenAiCompatError::AuthenticationError("bad key".to_string()),
                AiErrorClass::Fatal,
            ),
            (
                OpenAiCompatError::QuotaExhausted("no credits".to_string()),
                AiErrorClass::Fatal,
            ),
            (
                OpenAiCompatError::ApiError(
                    reqwest::StatusCode::SERVICE_UNAVAILABLE,
                    "unavailable".to_string(),
                ),
                AiErrorClass::Transient {
                    retry_after: DEFAULT_RETRY_AFTER,
                },
            ),
            (
                OpenAiCompatError::ApiError(
                    reqwest::StatusCode::BAD_REQUEST,
                    "bad request".to_string(),
                ),
                AiErrorClass::Fatal,
            ),
        ];

        for (err, expected) in cases {
            assert_eq!(err.ai_error_class(), expected);
        }
    }

    /// The live body an exhausted account returns, and the one a genuine
    /// rate limit returns.  Both are 429; only `error.type` separates them.
    #[test]
    fn test_insufficient_quota_is_told_from_a_rate_limit() {
        let exhausted = r#"{"error":{"message":"You have no credits remaining.",
            "type":"insufficient_quota","param":null,
            "code":"credit_balance_exhausted"}}"#;
        let throttled = r#"{"error":{"message":"Rate limit reached. Please retry in 1.2s",
            "type":"requests","param":null,"code":"rate_limit_exceeded"}}"#;

        assert!(is_insufficient_quota(exhausted));
        assert!(!is_insufficient_quota(throttled));
        // An unparseable body still has to be told apart.
        assert!(is_insufficient_quota("502 Bad Gateway: insufficient_quota"));
        assert!(!is_insufficient_quota("502 Bad Gateway"));
    }

    #[test]
    fn test_normalize_base_url_appends_chat_completions() {
        // LM Studio style: just /v1
        assert_eq!(
            normalize_base_url("http://localhost:1234/v1", CHAT).unwrap(),
            "http://localhost:1234/v1/chat/completions"
        );
        // Trailing slash
        assert_eq!(
            normalize_base_url("http://localhost:1234/v1/", CHAT).unwrap(),
            "http://localhost:1234/v1/chat/completions"
        );
        // Already has full path
        assert_eq!(
            normalize_base_url("https://api.openai.com/v1/chat/completions", CHAT).unwrap(),
            "https://api.openai.com/v1/chat/completions"
        );
        // Full path with trailing slash
        assert_eq!(
            normalize_base_url("http://localhost:1234/v1/chat/completions/", CHAT).unwrap(),
            "http://localhost:1234/v1/chat/completions"
        );
        // Bare host
        assert_eq!(
            normalize_base_url("http://localhost:1234", CHAT).unwrap(),
            "http://localhost:1234/chat/completions"
        );
        // Test the specific nested bogus path scenario we analyzed
        assert!(normalize_base_url("http://localhost:1234/v1/text/completions", CHAT).is_err());
        // Bare host with different host
        assert_eq!(
            normalize_base_url("https://openai.com", CHAT).unwrap(),
            "https://openai.com/chat/completions"
        );
        // OpenRouter /api/v1 style paths
        assert_eq!(
            normalize_base_url("https://openrouter.ai/api/v1", CHAT).unwrap(),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            normalize_base_url("https://openrouter.ai/api/v1/", CHAT).unwrap(),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            normalize_base_url("https://openrouter.ai/api/v1/chat/completions", CHAT).unwrap(),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        // z.ai / Zhipu endpoints: full URLs ending in /chat/completions are
        // accepted verbatim, so providers with otherwise-unrecognised paths
        // (direct API and coding-plan gateway) can be used via base_url only.
        assert_eq!(
            normalize_base_url("https://api.z.ai/api/paas/v4/chat/completions", CHAT).unwrap(),
            "https://api.z.ai/api/paas/v4/chat/completions"
        );
        let coding = "https://api.z.ai/api/coding/paas/v4/chat/completions";
        assert_eq!(normalize_base_url(coding, CHAT).unwrap(), coding);
        // Trailing slash on a full endpoint URL is trimmed
        let coding_slash = format!("{coding}/");
        assert_eq!(normalize_base_url(&coding_slash, CHAT).unwrap(), coding);
        // Paths that are not full chat/completions URLs and are not a known
        // shorthand are still rejected.
        assert!(normalize_base_url("https://api.z.ai/api/paas/v4", CHAT).is_err());
        // Test arbitrary deep nested paths that shouldn't be accepted
        let nested = "http://localhost:1234/v1/v1v1/text/completions";
        assert!(normalize_base_url(nested, CHAT).is_err());
        // Test strings completely lacking a valid protocol scheme format
        assert!(normalize_base_url("completely-broken-input-string", CHAT).is_err());
    }

    #[test]
    fn test_lenient_option_drops_a_shape_it_cannot_read() -> Result<()> {
        #[derive(Debug, Default, Deserialize)]
        struct Details {
            #[serde(default)]
            cached_tokens: Option<u32>,
        }

        #[derive(Debug, Deserialize)]
        struct Usage {
            #[serde(default, deserialize_with = "lenient_option")]
            details: Option<Details>,
        }

        let usage: Usage = serde_json::from_str(r#"{"details": {"cached_tokens": 12}}"#)?;
        assert_eq!(usage.details.and_then(|d| d.cached_tokens), Some(12));

        for details in [r#"{"cached_tokens": 1920.5}"#, r#""1920""#, "null"] {
            let usage: Usage = serde_json::from_str(&format!(r#"{{"details": {details}}}"#))?;
            assert!(usage.details.is_none(), "{details}");
        }

        Ok(())
    }
}
