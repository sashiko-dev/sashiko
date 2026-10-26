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
    ProviderCapabilities, ToolCall, classify_status_code,
};
use crate::utils::redact_secret;
use anyhow::{Context, Result};
use async_trait::async_trait;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::RwLock;

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
    ThoughtSignature {
        #[serde(rename = "thoughtSignature")]
        thought_signature: String,
        #[serde(default)]
        thought: bool,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FunctionCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FunctionResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingConfig {
    pub include_thoughts: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
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
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<ThinkingConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptFeedback {
    pub block_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentResponse {
    pub candidates: Option<Vec<Candidate>>,
    pub prompt_feedback: Option<PromptFeedback>,
    pub usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    #[serde(default)]
    pub content: Content,
    pub finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_token: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageMetadata {
    pub prompt_token_count: u32,
    pub candidates_token_count: Option<u32>,
    pub total_token_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_content_token_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thoughts_token_count: Option<u32>,
    #[serde(flatten)]
    pub extra: Option<std::collections::HashMap<String, Value>>,
}

impl UsageMetadata {
    /// Total output tokens billed by Gemini, combining visible candidate tokens
    /// and internal reasoning (`thoughtsTokenCount`) tokens.
    pub fn output_token_count(&self) -> u32 {
        self.candidates_token_count
            .unwrap_or(0)
            .saturating_add(self.thoughts_token_count.unwrap_or(0))
    }
}

#[async_trait]
pub trait GenAiClient: Send + Sync {
    async fn generate_content(
        &self,
        request: GenerateContentRequest,
    ) -> Result<GenerateContentResponse>;
}

fn extract_gemini_error_reason(error_text: &str) -> Option<String> {
    let json = serde_json::from_str::<serde_json::Value>(error_text).ok()?;
    let err = json.get("error")?;

    let reason = err
        .get("details")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .and_then(|f| f.get("reason"))
        .and_then(|r| r.as_str());

    let message = err.get("message").and_then(|m| m.as_str());

    if let Some(r) = reason {
        return Some(r.to_string());
    } else if let Some(m) = message {
        return Some(m.to_string());
    }

    None
}

fn summarize_gemini_error(status: reqwest::StatusCode, error_text: &str) -> String {
    if error_text.contains("DECODE_PREEMPTED") {
        return format!("{} (request preempted in decode queue)", status);
    }
    if error_text.contains("Overloaded prefill queue") {
        return format!("{} (request preempted in prefill queue)", status);
    }
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(error_text)
        && let Some(err) = json.get("error")
        && let Some(msg) = err.get("message").and_then(|m| m.as_str())
    {
        let first_line = msg.lines().next().unwrap_or(msg);
        let clean = first_line
            .split(" [type.googleapis.com/")
            .next()
            .unwrap_or(first_line)
            .split(" (see ")
            .next()
            .unwrap_or(first_line)
            .trim();
        return format!("{}: {}", status, clean);
    }
    let first_line = error_text.lines().next().unwrap_or(error_text).trim();
    format!("{}: {}", status, first_line)
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
            GeminiError::PermissionDenied(msg) => {
                if let Some(reason) = extract_gemini_error_reason(msg) {
                    write!(f, "Permission denied (reason: {}): {}", reason, msg)
                } else {
                    write!(f, "Permission denied: {}", msg)
                }
            }
            GeminiError::ApiError(s, msg) => {
                if let Some(reason) = extract_gemini_error_reason(msg) {
                    write!(f, "Gemini API error ({}) (reason: {}): {}", s, reason, msg)
                } else {
                    write!(f, "Gemini API error ({}): {}", s, msg)
                }
            }
            GeminiError::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for GeminiError {}

impl ClassifyAiError for GeminiError {
    fn ai_error_class(&self) -> AiErrorClass {
        match self {
            GeminiError::QuotaExceeded(retry_after) => AiErrorClass::RateLimit {
                retry_after: *retry_after,
            },
            GeminiError::TransientError(retry_after, _) => AiErrorClass::Transient {
                retry_after: *retry_after,
            },
            GeminiError::PermissionDenied(_) => AiErrorClass::Fatal,
            GeminiError::ApiError(status, _) => {
                classify_status_code(*status).unwrap_or(AiErrorClass::Fatal)
            }
            GeminiError::Other(_) => AiErrorClass::Fatal,
        }
    }
}

/// Rejects delays that are negative, non-finite or too large for a Duration;
/// caps the others at the quota manager's maximum.
fn valid_retry_after_secs(secs: f64) -> Option<Duration> {
    Duration::try_from_secs_f64(secs)
        .ok()
        .map(|d| d.min(crate::ai::quota::MAX_RETRY_AFTER))
}

const TRANSPORT_RETRY_AFTER: Duration = Duration::from_secs(30);

/// Types a transport failure as transient unless retrying cannot fix it.
pub(crate) fn transport_error(provider: &str, e: reqwest::Error) -> anyhow::Error {
    let is_permanent = crate::ai::is_permanent_transport_error(&e);
    let err_str = redact_secret(&format!("{:#}", anyhow::Error::from(e)));
    if is_permanent {
        return anyhow::anyhow!("{provider} request failed: {err_str}");
    }
    tracing::error!(
        "{}{} request failed (transport): {}",
        crate::ai::get_log_prefix(),
        provider,
        err_str
    );
    GeminiError::TransientError(TRANSPORT_RETRY_AFTER, err_str).into()
}

fn retry_after_from_body(error_text: &str) -> Option<f64> {
    let hint = Regex::new(r"Please retry in ([0-9.]+)s").ok()?;
    let secs = hint
        .captures(error_text)?
        .get(1)?
        .as_str()
        .parse::<f64>()
        .ok()?;
    valid_retry_after_secs(secs).map(|_| secs)
}

fn reason_suffix(error_text: &str) -> String {
    extract_gemini_error_reason(error_text)
        .map(|reason| format!(" (reason: {})", reason))
        .unwrap_or_default()
}

fn classify_generate_content_failure(
    status: reqwest::StatusCode,
    retry_after_secs: Option<f64>,
    error_text: &str,
) -> GeminiError {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = retry_after_secs
            .and_then(valid_retry_after_secs)
            .or_else(|| retry_after_from_body(error_text).and_then(valid_retry_after_secs))
            .unwrap_or(Duration::from_secs(30));
        tracing::warn!(
            "Gemini 429 Quota Exceeded. Retry suggested in {}s. Body: {}",
            retry_after.as_secs_f64(),
            error_text
        );
        let quota_delay = retry_after
            .checked_add(Duration::from_secs(1))
            .unwrap_or(Duration::from_secs(31))
            .min(crate::ai::quota::MAX_RETRY_AFTER);
        return GeminiError::QuotaExceeded(quota_delay);
    }

    if status == reqwest::StatusCode::FORBIDDEN {
        tracing::error!(
            "Gemini Permission Denied (403){}: {}",
            reason_suffix(error_text),
            error_text
        );
        return GeminiError::PermissionDenied(error_text.to_string());
    }

    if status.is_server_error() || status.as_u16() == 499 {
        let retry_after = retry_after_secs
            .and_then(valid_retry_after_secs)
            .unwrap_or(Duration::from_secs(0));
        tracing::debug!(
            "Gemini API Transient Error: status={}, body={}",
            status,
            error_text
        );
        let summary = summarize_gemini_error(status, error_text);
        return GeminiError::TransientError(retry_after, summary);
    }

    tracing::error!(
        "Gemini API Error{}: status={}, body={}",
        reason_suffix(error_text),
        status,
        error_text
    );
    GeminiError::ApiError(status, error_text.to_string())
}

pub(crate) async fn read_generate_content_response(
    res: reqwest::Response,
) -> Result<GenerateContentResponse> {
    let status = res.status();

    if status.is_success() {
        let body_text = res.text().await.map_err(|e| transport_error("Gemini", e))?;
        let response: GenerateContentResponse = match serde_json::from_str(&body_text) {
            Ok(response) => response,
            Err(e) => {
                tracing::error!("Failed to decode Gemini response: {}", e);
                anyhow::bail!("Failed to decode response: {}. Body: {}", e, body_text);
            }
        };

        match &response.usage_metadata {
            Some(usage) => {
                let cached = usage.cached_content_token_count.unwrap_or(0);
                tracing::info!(
                    "{}Gemini response received. Tokens: in={}, cached={}, out={} (candidates={}, thoughts={})",
                    crate::ai::get_log_prefix(),
                    usage.prompt_token_count.saturating_sub(cached),
                    cached,
                    usage.output_token_count(),
                    usage.candidates_token_count.unwrap_or(0),
                    usage.thoughts_token_count.unwrap_or(0)
                );
            }
            None => tracing::info!(
                "{}Gemini response received. No usage metadata.",
                crate::ai::get_log_prefix()
            ),
        }

        return Ok(response);
    }

    let retry_after_secs = res
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|&secs| valid_retry_after_secs(secs).is_some());

    // A failed read of the error body must not hide the status.
    let error_text =
        redact_secret(&res.text().await.unwrap_or_else(|e| {
            format!("<failed to read error body: {:#}>", anyhow::Error::from(e))
        }));

    Err(classify_generate_content_failure(status, retry_after_secs, &error_text).into())
}

struct ProxyGuard {
    child: std::sync::Mutex<Option<std::process::Child>>,
    port_file: std::path::PathBuf,
}

impl Drop for ProxyGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.child.lock()
            && let Some(mut child) = guard.take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.port_file);
    }
}

pub struct GeminiClient {
    model: String,
    base_url: String,
    api_key: String,
    proxy_command: Option<String>,
    auth_token_command: Option<String>,
    thinking_level: Option<String>,
    max_tokens: u32,
    proxy_url: tokio::sync::OnceCell<String>,
    proxy_guard: tokio::sync::Mutex<Option<std::sync::Arc<ProxyGuard>>>,
    client: RwLock<Client>,
}

impl GeminiClient {
    pub fn new(model: String) -> Self {
        Self::new_with_settings(model, None)
    }

    pub fn new_with_settings(
        model: String,
        settings: Option<&crate::settings::GeminiSettings>,
    ) -> Self {
        let api_key = std::env::var("GEMINI_API_KEY")
            .or_else(|_| std::env::var("LLM_API_KEY"))
            .unwrap_or_default();
        let base_url = std::env::var("GOOGLE_GEMINI_BASE_URL")
            .or_else(|_| std::env::var("GEMINI_BASE_URL"))
            .ok()
            .or_else(|| settings.and_then(|s| s.base_url.clone()))
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "https://generativelanguage.googleapis.com".to_string());

        let proxy_command = std::env::var("GEMINI_PROXY_COMMAND")
            .ok()
            .or_else(|| settings.and_then(|s| s.proxy_command.clone()))
            .filter(|s| !s.trim().is_empty());

        let auth_token_command = std::env::var("GEMINI_AUTH_TOKEN_COMMAND")
            .ok()
            .or_else(|| settings.and_then(|s| s.auth_token_command.clone()))
            .filter(|s| !s.trim().is_empty());

        let thinking_level = settings
            .and_then(|s| s.thinking_level.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_uppercase);

        let max_tokens = settings
            .and_then(|s| s.max_tokens)
            .unwrap_or_else(|| crate::settings::default_gemini_max_tokens_for_model(&model));

        let client = Self::create_http_client(&api_key);

        Self {
            model,
            base_url,
            api_key,
            proxy_command,
            auth_token_command,
            thinking_level,
            max_tokens,
            proxy_url: tokio::sync::OnceCell::new(),
            proxy_guard: tokio::sync::Mutex::new(None),
            client: RwLock::new(client),
        }
    }

    fn create_http_client(api_key: &str) -> Client {
        let mut headers = reqwest::header::HeaderMap::new();
        if !api_key.is_empty()
            && let Ok(value) = reqwest::header::HeaderValue::from_str(api_key)
        {
            headers.insert("x-goog-api-key", value);
        }

        reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(600))
            // Reliability Tuning: Prune stale connections proactively
            .tcp_keepalive(Duration::from_secs(60))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(5)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    }

    fn find_auto_proxy_helper() -> Option<String> {
        let mut candidates = vec![std::path::PathBuf::from("scripts/gemini-proxy-helper.sh")];
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            candidates.push(dir.join("../../scripts/gemini-proxy-helper.sh"));
            candidates.push(dir.join("../scripts/gemini-proxy-helper.sh"));
        }
        for candidate in candidates {
            if !candidate.is_file() {
                continue;
            }
            if let Ok(status) = std::process::Command::new(&candidate)
                .arg("--check")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                && status.success()
            {
                return Some(format!("{} --port-file {{port_file}}", candidate.display()));
            }
        }
        None
    }

    async fn start_proxy_command(
        cmd_template: &str,
    ) -> Result<(String, std::sync::Arc<ProxyGuard>)> {
        static PROXY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = PROXY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let port_file = std::env::temp_dir().join(format!(
            "sashiko-gemini-proxy-{}-{}.port",
            std::process::id(),
            seq
        ));
        let _ = std::fs::remove_file(&port_file);

        let port_file_str = port_file.to_string_lossy();
        let full_cmd = if cmd_template.contains("{port_file}") {
            cmd_template.replace("{port_file}", &port_file_str)
        } else {
            format!("{} --port-file {}", cmd_template, port_file_str)
        };

        tracing::info!("Starting local Gemini proxy: {}", full_cmd);
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(&full_cmd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .with_context(|| format!("Failed to spawn Gemini proxy command: {}", full_cmd))?;

        let guard = std::sync::Arc::new(ProxyGuard {
            child: std::sync::Mutex::new(Some(child)),
            port_file: port_file.clone(),
        });

        for _ in 0..150 {
            if let Ok(content) = tokio::fs::read_to_string(&port_file).await {
                let trimmed = content.trim();
                if !trimmed.is_empty()
                    && let Ok(port) = trimmed.parse::<u16>()
                {
                    let url = format!("http://localhost:{}", port);
                    tracing::info!("Local Gemini proxy ready at {}", url);
                    return Ok((url, guard));
                }
            }
            if let Ok(mut child_lock) = guard.child.lock()
                && let Some(ref mut child_proc) = *child_lock
                && let Ok(Some(status)) = child_proc.try_wait()
            {
                anyhow::bail!(
                    "Gemini proxy command exited prematurely with status {} before writing port file",
                    status
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        anyhow::bail!(
            "Timed out waiting for Gemini proxy command to write port file: {}",
            port_file.display()
        )
    }

    async fn effective_base_url(&self) -> Result<String> {
        if self.base_url != "https://generativelanguage.googleapis.com" {
            return Ok(self.base_url.clone());
        }
        if !self.api_key.is_empty() && self.proxy_command.is_none() {
            return Ok(self.base_url.clone());
        }
        let url = self
            .proxy_url
            .get_or_try_init(|| async {
                let cmd = self
                    .proxy_command
                    .clone()
                    .or_else(Self::find_auto_proxy_helper);
                if let Some(cmd_str) = cmd {
                    let (url, guard) = Self::start_proxy_command(&cmd_str).await?;
                    *self.proxy_guard.lock().await = Some(guard);
                    Ok::<String, anyhow::Error>(url)
                } else {
                    Ok(self.base_url.clone())
                }
            })
            .await?;
        Ok(url.clone())
    }

    fn resolve_bearer_token(&self) -> Result<Option<String>> {
        if let Ok(token) = std::env::var("GEMINI_AUTH_TOKEN") {
            let trimmed = token.trim();
            if !trimmed.is_empty() {
                return Ok(Some(trimmed.to_string()));
            }
        }
        if let Some(ref cmd) = self.auth_token_command {
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .output()
                .with_context(|| format!("Failed to execute auth_token_command: {}", cmd))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!(
                    "auth_token_command '{}' failed with status {}: {}",
                    cmd,
                    output.status,
                    stderr.trim()
                );
            }
            let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !token.is_empty() {
                return Ok(Some(token));
            }
        }
        Ok(None)
    }

    /// Replaces the internal HTTP client with a fresh one.
    /// This is used to recover from degraded connection pools without restarting the service.
    async fn refresh_client(&self) {
        tracing::warn!("Refreshing Gemini HTTP client due to transport errors...");
        let new_client = Self::create_http_client(&self.api_key);
        let mut guard = self.client.write().await;
        *guard = new_client;
    }

    pub async fn generate_content_single(
        &self,
        request: &GenerateContentRequest,
    ) -> Result<GenerateContentResponse> {
        tracing::info!("Sending Gemini request...");

        let base_url = self.effective_base_url().await?;
        let url = format!("{}/v1beta/models/{}:generateContent", base_url, self.model);
        self.post_request(&url, request).await
    }

    async fn post_request<T: Serialize>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<GenerateContentResponse> {
        let client = self.client.read().await.clone();
        let mut req_builder = client.post(url).json(body);
        if let Some(token) = self.resolve_bearer_token()? {
            req_builder =
                req_builder.header(reqwest::header::AUTHORIZATION, format!("Bearer {}", token));
        }

        let res = match req_builder.send().await {
            Ok(res) => res,
            Err(e) => {
                let err = transport_error("Gemini", e);
                if err.downcast_ref::<GeminiError>().is_some() {
                    // Trigger self-healing: Refresh the client for the next attempt
                    self.refresh_client().await;
                }
                return Err(err);
            }
        };

        let status = res.status();
        let result = read_generate_content_response(res).await;
        // A success body cut off mid-read is a transport failure too.
        if status.is_success()
            && let Err(e) = &result
            && matches!(
                e.downcast_ref::<GeminiError>(),
                Some(GeminiError::TransientError(..))
            )
        {
            self.refresh_client().await;
        }
        result
    }
}

/// Maximum number of continuation hops per `generateContent` turn.
/// At 32K tokens per hop, 16 hops bounds even a raised `max_tokens` to 512K
/// output tokens and prevents an infinite loop if a backend returns non-empty
/// continuation tokens indefinitely.
pub(crate) const MAX_CONTINUATION_HOPS: usize = 16;

pub(crate) fn next_continuation_token(
    resp: &GenerateContentResponse,
    last_sent_token: Option<&str>,
) -> Option<String> {
    if resp
        .prompt_feedback
        .as_ref()
        .and_then(|f| f.block_reason.as_ref())
        .is_some()
    {
        return None;
    }
    let candidate = resp.candidates.as_ref()?.first()?;
    if candidate.finish_reason.as_deref() != Some("CONTINUATION") {
        return None;
    }
    let token = candidate.continuation_token.as_deref()?.trim();
    if token.is_empty() || last_sent_token == Some(token) {
        return None;
    }
    Some(token.to_string())
}

fn sum_optional_u32(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.saturating_add(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

pub(crate) fn merge_continuation_response(
    acc: &mut GenerateContentResponse,
    next: GenerateContentResponse,
) {
    if next
        .prompt_feedback
        .as_ref()
        .and_then(|f| f.block_reason.as_ref())
        .is_some()
    {
        acc.prompt_feedback = next.prompt_feedback;
    }

    if let Some(mut next_cands) = next.candidates
        && let Some(next_cand) = next_cands.drain(..).next()
    {
        if let Some(acc_cands) = acc.candidates.as_mut()
            && let Some(acc_cand) = acc_cands.first_mut()
        {
            acc_cand.content.parts.extend(next_cand.content.parts);
            acc_cand.finish_reason = next_cand.finish_reason;
            acc_cand.continuation_token = next_cand.continuation_token;
        } else {
            acc.candidates = Some(vec![next_cand]);
        }
    }

    match (acc.usage_metadata.as_mut(), next.usage_metadata) {
        (Some(acc_u), Some(next_u)) => {
            acc_u.prompt_token_count = acc_u
                .prompt_token_count
                .saturating_add(next_u.prompt_token_count);
            acc_u.candidates_token_count =
                sum_optional_u32(acc_u.candidates_token_count, next_u.candidates_token_count);
            acc_u.total_token_count = acc_u
                .total_token_count
                .saturating_add(next_u.total_token_count);
            acc_u.cached_content_token_count = sum_optional_u32(
                acc_u.cached_content_token_count,
                next_u.cached_content_token_count,
            );
            acc_u.thoughts_token_count =
                sum_optional_u32(acc_u.thoughts_token_count, next_u.thoughts_token_count);
        }
        (None, Some(next_u)) => {
            acc.usage_metadata = Some(next_u);
        }
        _ => {}
    }
}

#[async_trait]
impl GenAiClient for GeminiClient {
    async fn generate_content(
        &self,
        mut request: GenerateContentRequest,
    ) -> Result<GenerateContentResponse> {
        let mut acc = self.generate_content_single(&request).await?;
        for hop in 1..=MAX_CONTINUATION_HOPS {
            let Some(token) = next_continuation_token(&acc, request.continuation_token.as_deref())
            else {
                return Ok(acc);
            };
            tracing::info!(
                "{}Gemini response reached per-hop limit (finishReason=CONTINUATION); requesting continuation hop {}/{}...",
                crate::ai::get_log_prefix(),
                hop,
                MAX_CONTINUATION_HOPS
            );
            request.continuation_token = Some(token);
            let next_resp = self.generate_content_single(&request).await?;
            merge_continuation_response(&mut acc, next_resp);
        }
        if next_continuation_token(&acc, request.continuation_token.as_deref()).is_some() {
            tracing::warn!(
                "{}Gemini continuation loop reached MAX_CONTINUATION_HOPS ({}); returning partial response.",
                crate::ai::get_log_prefix(),
                MAX_CONTINUATION_HOPS
            );
        }
        Ok(acc)
    }
}

// The registry and writer are process-wide, so holding them in fields would
// only cache what the accessors already return.
pub struct StdioGeminiClient;

impl StdioGeminiClient {
    pub fn new() -> Self {
        Self
    }
}

impl Default for StdioGeminiClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl AiProvider for StdioGeminiClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        crate::ai::ensure_stdin_reader();

        let registry = crate::ai::ipc_registry();
        let tx_id = registry.next_id();
        let envelope = json!({
            "type": "ai_request",
            "tx_id": tx_id,
            "payload": request
        });

        let line = serde_json::to_string(&envelope)?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        registry.register(tx_id, tx).await?;

        crate::ai::ipc_writer().write_line(&line).await?;

        match rx.await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(remote_err)) => Err(remote_err.into()),
            Err(_) => Err(anyhow::anyhow!(
                "IPC channel disconnected waiting for response"
            )),
        }
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: "stdio-gemini".to_string(),
            context_window_size: 1_000_000,
        }
    }

    async fn forget(&self, request: &AiRequest) {
        crate::ai::ipc_forget(request).await
    }
}

// --- Translation Helpers ---

#[cfg(test)]
pub(crate) fn translate_ai_request(request: AiRequest) -> Result<GenerateContentRequest> {
    translate_ai_request_with_options(
        request,
        None,
        Some(crate::settings::default_gemini_max_tokens()),
    )
}

#[cfg(test)]
pub(crate) fn translate_ai_request_with_thinking_level(
    request: AiRequest,
    default_thinking_level: Option<&str>,
) -> Result<GenerateContentRequest> {
    translate_ai_request_with_options(
        request,
        default_thinking_level,
        Some(crate::settings::default_gemini_max_tokens()),
    )
}

pub(crate) fn translate_ai_request_with_options(
    request: AiRequest,
    default_thinking_level: Option<&str>,
    max_output_tokens: Option<u32>,
) -> Result<GenerateContentRequest> {
    let mut contents: Vec<Content> = Vec::new();
    let mut system_instruction = None;
    let mut call_id_to_name: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

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
                // Thought summaries (`msg.thought`) are output-only telemetry for
                // logs and UI; Gemini reconstructs prior-turn reasoning from
                // `thoughtSignature` alone.
                let mut pending_signature = msg.thought_signature.filter(|s| !s.is_empty());
                let call_needs_sig = msg.tool_calls.as_ref().is_some_and(|calls| {
                    calls
                        .iter()
                        .any(|c| c.thought_signature.as_deref().unwrap_or("").is_empty())
                });
                if let Some(text) = msg.content {
                    parts.push(Part::Text {
                        text,
                        thought_signature: if call_needs_sig {
                            None
                        } else {
                            pending_signature.take()
                        },
                        thought: false,
                    });
                }
                if let Some(tool_calls) = msg.tool_calls {
                    for call in tool_calls {
                        let call_id = if !call.id.is_empty() && call.id != call.function_name {
                            call_id_to_name.insert(call.id.clone(), call.function_name.clone());
                            Some(call.id)
                        } else {
                            None
                        };
                        let sig = call
                            .thought_signature
                            .filter(|s| !s.is_empty())
                            .or_else(|| pending_signature.take());
                        parts.push(Part::FunctionCall {
                            function_call: FunctionCall {
                                id: call_id,
                                name: call.function_name,
                                args: call.arguments,
                            },
                            thought_signature: sig,
                        });
                    }
                }
                if parts.is_empty()
                    && let Some(sig) = pending_signature.take()
                {
                    parts.push(Part::Text {
                        text: String::new(),
                        thought_signature: Some(sig),
                        thought: false,
                    });
                }
                // Gemini rejects a Content with an empty parts array, and a
                // turn with nothing in it carries no information anyway.
                if parts.is_empty() {
                    continue;
                }
                contents.push(Content {
                    role: "model".to_string(),
                    parts,
                });
            }
            AiRole::Tool => {
                // Gemini expects a 'user' role for FunctionResponse parts.
                // Consecutive tool responses and user messages must be merged into a single
                // turn to preserve strict user/model turn alternation.
                let raw_id = msg
                    .tool_call_id
                    .context("Tool message missing tool_call_id")?;
                let (resp_id, resp_name) = if let Some(fn_name) = call_id_to_name.get(&raw_id) {
                    (Some(raw_id), fn_name.clone())
                } else {
                    (None, raw_id)
                };
                let part = Part::FunctionResponse {
                    function_response: FunctionResponse {
                        id: resp_id,
                        name: resp_name,
                        response: serde_json::from_str(
                            &msg.content.unwrap_or_else(|| "{}".to_string()),
                        )
                        .unwrap_or(json!({})),
                    },
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
        }
    }

    let tools = request.tools.map(|t| {
        vec![Tool {
            function_declarations: t
                .into_iter()
                .map(|tool| FunctionDeclaration {
                    name: tool.name,
                    description: tool.description,
                    parameters: normalize_schema(tool.parameters),
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
                response_schema = schema.map(normalize_schema);
            }
        }
    }

    let thinking_level = request
        .reasoning_effort
        .map(|effort| effort.as_gemini_thinking_level().to_string())
        .or_else(|| {
            default_thinking_level
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_ascii_uppercase)
        });

    Ok(GenerateContentRequest {
        contents,
        tools,
        system_instruction,
        generation_config: Some(GenerationConfig {
            response_mime_type,
            response_schema,
            temperature: request.temperature,
            max_output_tokens: max_output_tokens.filter(|&n| n > 0),
            thinking_config: Some(ThinkingConfig {
                include_thoughts: true,
                thinking_level,
            }),
        }),
        continuation_token: None,
    })
}

fn normalize_schema(mut schema: Value) -> Value {
    if let Some(obj) = schema.as_object_mut() {
        if let Some(ty) = obj.get_mut("type")
            && let Some(s) = ty.as_str()
        {
            *ty = Value::String(s.to_uppercase());
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

pub(crate) fn translate_ai_response(resp: GenerateContentResponse) -> Result<AiResponse> {
    if let Some(reason) = resp.prompt_feedback.and_then(|f| f.block_reason) {
        return Err(anyhow::anyhow!(
            "Gemini request blocked by prompt feedback (reason: {})",
            reason
        ));
    }

    let candidate = resp
        .candidates
        .as_ref()
        .and_then(|c| c.first())
        .ok_or_else(|| anyhow::anyhow!("No candidates returned from Gemini"))?;

    if let Some(finish_reason) = &candidate.finish_reason {
        if finish_reason == "SAFETY"
            || finish_reason == "RECITATION"
            || finish_reason == "OTHER"
            || finish_reason == "BLOCKLIST"
            || finish_reason == "PROHIBITED_CONTENT"
        {
            return Err(anyhow::anyhow!(
                "Gemini candidate blocked (finish reason: {})",
                finish_reason
            ));
        } else if finish_reason == "MALFORMED_FUNCTION_CALL"
            || finish_reason == "UNEXPECTED_TOOL_CALL"
        {
            return Err(anyhow::anyhow!(
                "Gemini candidate failed tool call (finish reason: {})",
                finish_reason
            ));
        } else if finish_reason != "STOP"
            && finish_reason != "MAX_TOKENS"
            && finish_reason != "CONTINUATION"
        {
            tracing::warn!(
                "Gemini candidate finished with unexpected reason: {}",
                finish_reason
            );
        }
    }

    let mut content = String::new();
    let mut thought = String::new();
    let mut last_thought_signature: Option<String> = None;
    let mut tool_calls = Vec::new();

    for part in &candidate.content.parts {
        match part {
            Part::Text {
                text,
                thought_signature,
                thought: is_thought,
            } => {
                if let Some(sig) = thought_signature
                    && !sig.is_empty()
                {
                    last_thought_signature = Some(sig.clone());
                }
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
                let pending_signature = last_thought_signature.take();
                let effective_signature = thought_signature
                    .clone()
                    .filter(|s| !s.is_empty())
                    .or(pending_signature);
                let call_id = function_call
                    .id
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .unwrap_or_else(|| function_call.name.clone());
                tool_calls.push(ToolCall {
                    id: call_id,
                    function_name: function_call.name.clone(),
                    arguments: crate::toolbox::utils::normalize_json_numbers(
                        function_call.args.clone(),
                    ),
                    thought_signature: effective_signature,
                });
            }
            Part::ThoughtSignature {
                thought_signature, ..
            } => {
                if !thought_signature.is_empty() {
                    last_thought_signature = Some(thought_signature.clone());
                }
            }
            Part::FunctionResponse { .. } => {}
        }
    }

    let truncated = candidate
        .finish_reason
        .as_deref()
        .is_some_and(|r| r == "MAX_TOKENS" || r == "CONTINUATION");

    if truncated {
        tracing::warn!(
            "{}Gemini response truncated (finish reason: {}).",
            crate::ai::get_log_prefix(),
            candidate.finish_reason.as_deref().unwrap_or("UNKNOWN")
        );
    }

    let usage = resp.usage_metadata.map(|m| AiUsage {
        prompt_tokens: m.prompt_token_count as usize,
        completion_tokens: m.output_token_count() as usize,
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
        thought_signature: if tool_calls.is_empty() {
            last_thought_signature
        } else {
            None
        },
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        usage,
        truncated,
    })
}

#[async_trait]
impl AiProvider for GeminiClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let gen_req = translate_ai_request_with_options(
            request,
            self.thinking_level.as_deref(),
            Some(self.max_tokens),
        )?;
        let resp = GenAiClient::generate_content(self, gen_req).await?;
        translate_ai_response(resp)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: 1_000_000, // Gemini 1.5 Pro default
        }
    }

    fn cache_identity(&self) -> String {
        // base_url separates two endpoints serving the same model name;
        // thinking_level is the provider default when a request omits its own;
        // max_tokens bounds total output tokens across hops.
        let max_tokens = self.max_tokens.to_string();
        crate::ai::cache_identity_with(
            &self.model,
            &[
                ("base_url", Some(self.base_url.as_str())),
                ("thinking_level", self.thinking_level.as_deref()),
                ("max_tokens", Some(max_tokens.as_str())),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::test_http;
    use crate::ai::{
        AiErrorClass, AiMessage, AiResponseFormat, AiRole, AiTool, ClassifyAiError,
        DEFAULT_RETRY_AFTER, ToolCall,
    };
    use serde_json::json;

    #[test]
    fn cache_identity_tracks_base_url_thinking_level_and_max_tokens() {
        let client = |base_url: &str, thinking_level: Option<&str>, max_tokens: u32| GeminiClient {
            model: "gemini-2.5-pro".to_string(),
            base_url: base_url.to_string(),
            api_key: String::new(),
            proxy_command: None,
            auth_token_command: None,
            thinking_level: thinking_level.map(str::to_string),
            max_tokens,
            proxy_url: tokio::sync::OnceCell::new(),
            proxy_guard: tokio::sync::Mutex::new(None),
            client: RwLock::new(Client::new()),
        };
        assert_ne!(
            client("https://generativelanguage.googleapis.com", None, 131_072).cache_identity(),
            client("https://proxy.invalid", None, 131_072).cache_identity()
        );
        assert_ne!(
            client(
                "https://generativelanguage.googleapis.com",
                Some("MEDIUM"),
                131_072
            )
            .cache_identity(),
            client(
                "https://generativelanguage.googleapis.com",
                Some("HIGH"),
                131_072
            )
            .cache_identity()
        );
        assert_ne!(
            client("https://generativelanguage.googleapis.com", None, 65_536).cache_identity(),
            client("https://generativelanguage.googleapis.com", None, 131_072).cache_identity()
        );
    }

    #[tokio::test]
    async fn test_proxy_command_lifecycle_and_port_discovery() {
        let client = GeminiClient {
            model: "gemini-2.5-pro".to_string(),
            base_url: "https://generativelanguage.googleapis.com".to_string(),
            api_key: String::new(),
            proxy_command: Some("echo 18432 > {port_file}; sleep 10".to_string()),
            auth_token_command: None,
            thinking_level: None,
            max_tokens: 131_072,
            proxy_url: tokio::sync::OnceCell::new(),
            proxy_guard: tokio::sync::Mutex::new(None),
            client: RwLock::new(Client::new()),
        };

        let resolved = client.effective_base_url().await.unwrap();
        assert_eq!(resolved, "http://localhost:18432");

        let port_file = {
            let guard = client.proxy_guard.lock().await;
            guard.as_ref().unwrap().port_file.clone()
        };
        assert!(port_file.exists());

        drop(client);
        assert!(!port_file.exists());
    }

    #[test]
    fn test_auth_token_command_resolution() {
        let client = GeminiClient {
            model: "gemini-2.5-pro".to_string(),
            base_url: "https://generativelanguage.googleapis.com".to_string(),
            api_key: String::new(),
            proxy_command: None,
            auth_token_command: Some("echo mock-bearer-token-xyz".to_string()),
            thinking_level: None,
            max_tokens: 131_072,
            proxy_url: tokio::sync::OnceCell::new(),
            proxy_guard: tokio::sync::Mutex::new(None),
            client: RwLock::new(Client::new()),
        };

        let token = client.resolve_bearer_token().unwrap();
        assert_eq!(token, Some("mock-bearer-token-xyz".to_string()));
    }

    #[tokio::test]
    #[ignore = "requires local proxy daemon"]
    async fn test_live_auto_proxy_completion() {
        let client = GeminiClient {
            model: "gemini-3-flash-preview".to_string(),
            base_url: "https://generativelanguage.googleapis.com".to_string(),
            api_key: String::new(),
            proxy_command: None,
            auth_token_command: None,
            thinking_level: None,
            max_tokens: 131_072,
            proxy_url: tokio::sync::OnceCell::new(),
            proxy_guard: tokio::sync::Mutex::new(None),
            client: RwLock::new(Client::new()),
        };
        let req = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Reply with the exact word PONG".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            response_format: Some(AiResponseFormat::Text),
            temperature: Some(0.0),
            context_tag: None,
            reasoning_effort: None,
        };
        let resp = AiProvider::generate_content(&client, req)
            .await
            .expect("Live auto-proxy request should succeed");
        let text = resp.content.unwrap_or_default();
        println!("Live auto-proxy response: {}", text);
        assert!(text.to_uppercase().contains("PONG"));
    }

    #[test]
    fn test_classify_generate_content_failure_status_codes() {
        let err = classify_generate_content_failure(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            Some(12.0),
            "Please retry in 99.0s",
        );
        assert!(matches!(
            err,
            GeminiError::QuotaExceeded(d) if d == Duration::from_secs_f64(13.0)
        ));

        let err = classify_generate_content_failure(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            None,
            "Quota exceeded. Please retry in 12.5s.",
        );
        assert!(matches!(
            err,
            GeminiError::QuotaExceeded(d) if d == Duration::from_secs_f64(13.5)
        ));

        let err = classify_generate_content_failure(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            None,
            "Quota exceeded.",
        );
        assert!(matches!(
            err,
            GeminiError::QuotaExceeded(d) if d == Duration::from_secs_f64(31.0)
        ));

        let err =
            classify_generate_content_failure(reqwest::StatusCode::FORBIDDEN, None, "forbidden");
        assert!(matches!(err, GeminiError::PermissionDenied(body) if body == "forbidden"));

        let err = classify_generate_content_failure(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            Some(4.0),
            "Overloaded prefill queue.",
        );
        assert!(matches!(
            err,
            GeminiError::TransientError(d, summary)
                if d == Duration::from_secs(4)
                    && summary == "503 Service Unavailable (request preempted in prefill queue)"
        ));

        let status_499 = reqwest::StatusCode::from_u16(499).expect("499 is a valid status code");
        let err = classify_generate_content_failure(status_499, None, "cancelled");
        assert!(matches!(
            err,
            GeminiError::TransientError(d, _) if d == Duration::from_secs(0)
        ));

        let err = classify_generate_content_failure(
            reqwest::StatusCode::BAD_REQUEST,
            None,
            "bad request",
        );
        assert!(matches!(
            err,
            GeminiError::ApiError(status, body)
                if status == reqwest::StatusCode::BAD_REQUEST && body == "bad request"
        ));
    }

    #[test]
    fn test_classify_failure_invalid_retry_after_falls_back_cleanly() {
        let invalid_headers = [
            Some(-5.0),
            Some(f64::NAN),
            Some(f64::INFINITY),
            Some(f64::NEG_INFINITY),
            Some(f64::MAX),
        ];
        let invalid_bodies = [
            "Quota exceeded.",
            "Please retry in 1e100s",
            "Please retry in 999999999999999999999999999999s",
        ];

        for body in invalid_bodies {
            assert_eq!(retry_after_from_body(body), None);
        }

        for retry_after_secs in invalid_headers {
            for body in invalid_bodies {
                let err_429 = classify_generate_content_failure(
                    reqwest::StatusCode::TOO_MANY_REQUESTS,
                    retry_after_secs,
                    body,
                );
                assert!(matches!(
                    err_429,
                    GeminiError::QuotaExceeded(d) if d == Duration::from_secs(31)
                ));

                let err_503 = classify_generate_content_failure(
                    reqwest::StatusCode::SERVICE_UNAVAILABLE,
                    retry_after_secs,
                    body,
                );
                assert!(matches!(
                    err_503,
                    GeminiError::TransientError(d, _) if d == Duration::from_secs(0)
                ));
            }
        }
    }

    #[test]
    fn test_gemini_error_classification() {
        let cases = [
            (
                GeminiError::QuotaExceeded(Duration::from_secs(7)),
                AiErrorClass::RateLimit {
                    retry_after: Duration::from_secs(7),
                },
            ),
            (
                GeminiError::TransientError(Duration::from_secs(11), "busy".to_string()),
                AiErrorClass::Transient {
                    retry_after: Duration::from_secs(11),
                },
            ),
            (
                GeminiError::PermissionDenied("forbidden".to_string()),
                AiErrorClass::Fatal,
            ),
            (
                GeminiError::ApiError(
                    reqwest::StatusCode::SERVICE_UNAVAILABLE,
                    "unavailable".to_string(),
                ),
                AiErrorClass::Transient {
                    retry_after: DEFAULT_RETRY_AFTER,
                },
            ),
            (
                GeminiError::ApiError(reqwest::StatusCode::BAD_REQUEST, "bad request".to_string()),
                AiErrorClass::Fatal,
            ),
            (
                GeminiError::Other("unknown".to_string()),
                AiErrorClass::Fatal,
            ),
        ];

        for (err, expected) in cases {
            assert_eq!(err.ai_error_class(), expected);
        }
    }

    fn local_client(base_url: &str) -> GeminiClient {
        GeminiClient {
            model: "gemini-test".to_string(),
            base_url: base_url.to_string(),
            api_key: String::new(),
            proxy_command: None,
            auth_token_command: None,
            thinking_level: None,
            max_tokens: 131_072,
            proxy_url: tokio::sync::OnceCell::new(),
            proxy_guard: tokio::sync::Mutex::new(None),
            client: RwLock::new(test_http::test_client()),
        }
    }

    async fn post_to(response: Vec<u8>) -> Result<GenerateContentResponse> {
        let server = test_http::serve(response).await;
        let request = GenerateContentRequest {
            contents: vec![],
            tools: None,
            system_instruction: None,
            generation_config: None,
            continuation_token: None,
        };
        let url = format!("{}/v1beta/models/gemini-test:generateContent", server.url);
        let result = tokio::time::timeout(
            test_http::TEST_TIMEOUT,
            local_client(&server.url).post_request(&url, &request),
        )
        .await
        .expect("request did not complete in time");
        server.finish().await;
        result
    }

    async fn error_for(response: Vec<u8>) -> anyhow::Error {
        post_to(response).await.expect_err("request should fail")
    }

    fn transient_message(err: &anyhow::Error) -> &str {
        match err.downcast_ref::<GeminiError>() {
            Some(GeminiError::TransientError(_, msg)) => msg,
            _ => panic!("expected GeminiError::TransientError, got: {err:#}"),
        }
    }

    #[tokio::test]
    async fn test_valid_response_succeeds() {
        let response = post_to(test_http::complete("200 OK", "", r#"{"candidates":[]}"#))
            .await
            .expect("request should succeed");
        assert!(response.candidates.is_some_and(|c| c.is_empty()));
    }

    #[tokio::test]
    async fn test_body_dropped_mid_response_is_transient() {
        let err = error_for(test_http::truncated("200 OK", "", r#"{"candidates":"#)).await;
        assert_eq!(
            crate::ai::classify_ai_error(&err),
            AiErrorClass::Transient {
                retry_after: Duration::from_secs(30)
            },
            "{err:#}"
        );
        let msg = transient_message(&err);
        assert!(msg.contains(test_http::BODY_READ_FAILURE), "{msg}");
    }

    #[tokio::test]
    async fn test_error_body_dropped_on_server_error_is_transient() {
        for status in ["503 Service Unavailable", "499 Client Closed Request"] {
            let err = error_for(test_http::truncated(status, "", r#"{"error":"#)).await;
            assert!(
                matches!(
                    crate::ai::classify_ai_error(&err),
                    AiErrorClass::Transient { .. }
                ),
                "{status}: {err:#}"
            );
            let msg = transient_message(&err);
            assert!(msg.contains("failed to read error body"), "{msg}");
        }
    }

    #[tokio::test]
    async fn test_error_body_dropped_on_rate_limit_stays_rate_limit() {
        let err = error_for(test_http::truncated(
            "429 Too Many Requests",
            "",
            r#"{"error":"#,
        ))
        .await;
        assert_eq!(
            crate::ai::classify_ai_error(&err),
            AiErrorClass::RateLimit {
                retry_after: Duration::from_secs(31)
            },
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn test_error_body_dropped_on_client_error_stays_fatal() {
        let err = error_for(test_http::truncated("400 Bad Request", "", r#"{"error":"#)).await;
        assert_eq!(crate::ai::classify_ai_error(&err), AiErrorClass::Fatal);
        assert!(
            matches!(
                err.downcast_ref::<GeminiError>(),
                Some(GeminiError::ApiError(reqwest::StatusCode::BAD_REQUEST, _))
            ),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn test_invalid_retry_after_falls_back_to_default() {
        for value in ["-1", "inf", "NaN", "1e30"] {
            let header = format!("Retry-After: {value}\r\n");
            let err = error_for(test_http::complete("429 Too Many Requests", &header, "{}")).await;
            assert_eq!(
                crate::ai::classify_ai_error(&err),
                AiErrorClass::RateLimit {
                    retry_after: Duration::from_secs(31)
                },
                "429 with Retry-After {value}: {err:#}"
            );
            let err = error_for(test_http::complete(
                "503 Service Unavailable",
                &header,
                "{}",
            ))
            .await;
            assert_eq!(
                crate::ai::classify_ai_error(&err),
                AiErrorClass::Transient {
                    retry_after: Duration::ZERO
                },
                "503 with Retry-After {value}: {err:#}"
            );
        }
    }

    #[tokio::test]
    async fn test_valid_retry_delays_are_honoured() {
        let cap = crate::ai::quota::MAX_RETRY_AFTER;
        let body_hint = r#"{"error":{"message":"Please retry in 7.5s"}}"#;
        let huge_hint = r#"{"error":{"message":"Please retry in 100000s"}}"#;
        let rate_limit = |d: Duration| AiErrorClass::RateLimit { retry_after: d };
        let transient = |d: Duration| AiErrorClass::Transient { retry_after: d };
        let cases = [
            (
                test_http::complete("429 Too Many Requests", "Retry-After: 2.5\r\n", "{}"),
                rate_limit(Duration::from_millis(3500)),
            ),
            (
                test_http::truncated("429 Too Many Requests", "Retry-After: 2.5\r\n", "{"),
                rate_limit(Duration::from_millis(3500)),
            ),
            (
                test_http::complete("503 Service Unavailable", "Retry-After: 2.5\r\n", "{}"),
                transient(Duration::from_millis(2500)),
            ),
            (
                test_http::truncated("503 Service Unavailable", "Retry-After: 2.5\r\n", "{"),
                transient(Duration::from_millis(2500)),
            ),
            (
                test_http::truncated("499 Client Closed Request", "Retry-After: 2.5\r\n", "{"),
                transient(Duration::from_millis(2500)),
            ),
            (
                test_http::complete("429 Too Many Requests", "", body_hint),
                rate_limit(Duration::from_millis(8500)),
            ),
            (
                test_http::complete("429 Too Many Requests", "Retry-After: -1\r\n", body_hint),
                rate_limit(Duration::from_millis(8500)),
            ),
            (
                test_http::complete("429 Too Many Requests", "Retry-After: 2\r\n", body_hint),
                rate_limit(Duration::from_secs(3)),
            ),
            (
                test_http::complete("429 Too Many Requests", "Retry-After: 100000\r\n", "{}"),
                rate_limit(cap),
            ),
            (
                test_http::complete("429 Too Many Requests", "", huge_hint),
                rate_limit(cap),
            ),
            (
                test_http::complete("503 Service Unavailable", "Retry-After: 100000\r\n", "{}"),
                transient(cap),
            ),
        ];
        for (response, expected) in cases {
            let head = String::from_utf8_lossy(&response)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            let err = error_for(response).await;
            assert_eq!(
                crate::ai::classify_ai_error(&err),
                expected,
                "{head}: {err:#}"
            );
        }
    }

    async fn send_error(base_url: &str, url: &str) -> anyhow::Error {
        let request = GenerateContentRequest {
            contents: vec![],
            tools: None,
            system_instruction: None,
            generation_config: None,
            continuation_token: None,
        };
        tokio::time::timeout(
            test_http::TEST_TIMEOUT,
            local_client(base_url).post_request(url, &request),
        )
        .await
        .expect("request did not complete in time")
        .expect_err("request should fail")
    }

    #[tokio::test]
    async fn test_redirect_loop_stays_fatal() {
        let server = test_http::RedirectLoop::start().await;
        let err = send_error(server.url(), server.url()).await;
        assert_eq!(
            crate::ai::classify_ai_error(&err),
            AiErrorClass::Fatal,
            "{err:#}"
        );
        assert!(format!("{err:#}").contains("redirect"), "{err:#}");
    }

    #[tokio::test]
    async fn test_invalid_url_stays_fatal() {
        let err = send_error("not a url", "not a url").await;
        assert_eq!(
            crate::ai::classify_ai_error(&err),
            AiErrorClass::Fatal,
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn test_connection_refused_is_transient() {
        let port = test_http::RefusedPort::new();
        let err = send_error(&port.url, &port.url).await;
        let msg = transient_message(&err);
        assert!(msg.contains("error sending request"), "{msg}");
    }

    #[tokio::test]
    async fn test_invalid_json_stays_fatal() {
        let err = error_for(test_http::complete("200 OK", "", "not json")).await;
        assert_eq!(crate::ai::classify_ai_error(&err), AiErrorClass::Fatal);
    }

    #[test]
    fn test_translate_ai_request_system_and_user() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![
                AiMessage {
                    role: AiRole::System,
                    content: Some("You are a helpful assistant.".to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::User,
                    content: Some("Hello!".to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            tools: None,
            temperature: Some(0.7),
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
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
                thought_signature: None,
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
            reasoning_effort: None,
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
                            id: None,
                            name: "test_tool".to_string(),
                            args: json!({"arg1": "val1"}),
                        },
                        thought_signature: Some("thought_sig_xyz".to_string()),
                    }],
                },
                finish_reason: Some("STOP".to_string()),
                continuation_token: None,
            }]),
            prompt_feedback: None,
            usage_metadata: Some(UsageMetadata {
                prompt_token_count: 10,
                candidates_token_count: Some(20),
                total_token_count: 30,
                cached_content_token_count: None,
                thoughts_token_count: None,
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
    fn test_translate_function_call_id() -> Result<()> {
        let gemini_resp = GenerateContentResponse {
            candidates: Some(vec![Candidate {
                content: Content {
                    role: "model".to_string(),
                    parts: vec![Part::FunctionCall {
                        function_call: FunctionCall {
                            id: Some("call_1".to_string()),
                            name: "git_diff".to_string(),
                            args: json!({"commit": "HEAD"}),
                        },
                        thought_signature: None,
                    }],
                },
                finish_reason: Some("STOP".to_string()),
                continuation_token: None,
            }]),
            prompt_feedback: None,
            usage_metadata: None,
        };

        let ai_resp = translate_ai_response(gemini_resp)?;
        let tool_calls = ai_resp.tool_calls.as_ref().expect("Expected tool calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_1");
        assert_eq!(tool_calls[0].function_name, "git_diff");

        let follow_up = AiRequest {
            system: None,
            messages: vec![
                AiMessage {
                    role: AiRole::User,
                    content: Some("Inspect HEAD".to_string()),
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
                    tool_calls: Some(tool_calls.clone()),
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::Tool,
                    content: Some(r#"{"diff":"ok"}"#.to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: Some("call_1".to_string()),
                },
            ],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
        };

        let translated = translate_ai_request(follow_up)?;
        assert_eq!(translated.contents.len(), 3);
        if let Part::FunctionCall { function_call, .. } = &translated.contents[1].parts[0] {
            assert_eq!(function_call.id.as_deref(), Some("call_1"));
            assert_eq!(function_call.name, "git_diff");
        } else {
            panic!("Expected FunctionCall part");
        }
        if let Part::FunctionResponse { function_response } = &translated.contents[2].parts[0] {
            assert_eq!(function_response.id.as_deref(), Some("call_1"));
            assert_eq!(function_response.name, "git_diff");
        } else {
            panic!("Expected FunctionResponse part");
        }

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
                thought_signature: None,
                tool_calls: None,
                tool_call_id: Some("call_123".to_string()),
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
        };

        let gemini_req = translate_ai_request(request)?;

        assert_eq!(gemini_req.contents.len(), 1);
        assert_eq!(gemini_req.contents[0].role, "user");
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
            "type": "OBJECT",
            "properties": {
                "score": {"type": "NUMBER"}
            }
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
                        thought_signature: Some("s1".to_string()),
                    }]),
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::Tool,
                    content: Some("{\"ok\":true}".to_string()),
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
        };

        let gemini_req = translate_ai_request(request)?;

        assert_eq!(gemini_req.contents.len(), 3);
        assert_eq!(gemini_req.contents[0].role, "user");
        assert_eq!(gemini_req.contents[1].role, "model");
        assert_eq!(gemini_req.contents[2].role, "user");

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
    fn test_normalize_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "start_line": { "type": "integer" }
                        }
                    }
                }
            }
        });

        let normalized = normalize_schema(schema);

        assert_eq!(normalized["type"], "OBJECT");
        assert_eq!(normalized["properties"]["files"]["type"], "ARRAY");
        assert_eq!(normalized["properties"]["files"]["items"]["type"], "OBJECT");
        assert_eq!(
            normalized["properties"]["files"]["items"]["properties"]["path"]["type"],
            "STRING"
        );
        assert_eq!(
            normalized["properties"]["files"]["items"]["properties"]["start_line"]["type"],
            "INTEGER"
        );
    }

    #[test]
    fn test_translate_ai_request_merges_consecutive_tool_responses() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![
                AiMessage {
                    role: AiRole::Tool,
                    content: Some("{\"result\":\"res1\"}".to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: Some("call_1".to_string()),
                },
                AiMessage {
                    role: AiRole::Tool,
                    content: Some("{\"result\":\"res2\"}".to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: Some("call_2".to_string()),
                },
            ],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
        };

        let gemini_req = translate_ai_request(request)?;
        assert_eq!(gemini_req.contents.len(), 1);
        assert_eq!(gemini_req.contents[0].role, "user");
        assert_eq!(gemini_req.contents[0].parts.len(), 2);
        Ok(())
    }

    #[test]
    fn test_translate_ai_request_merges_user_text_and_tool_response() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![
                AiMessage {
                    role: AiRole::Tool,
                    content: Some("{\"result\":\"res1\"}".to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: Some("call_1".to_string()),
                },
                AiMessage {
                    role: AiRole::User,
                    content: Some("Now continue.".to_string()),
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
        };

        let gemini_req = translate_ai_request(request)?;
        assert_eq!(gemini_req.contents.len(), 1);
        assert_eq!(gemini_req.contents[0].role, "user");
        assert_eq!(gemini_req.contents[0].parts.len(), 2);
        assert!(matches!(
            gemini_req.contents[0].parts[0],
            Part::FunctionResponse { .. }
        ));
        assert!(matches!(gemini_req.contents[0].parts[1], Part::Text { .. }));
        Ok(())
    }

    #[test]
    fn test_translate_ai_request_drops_empty_assistant_turn() -> Result<()> {
        let request = AiRequest {
            system: None,
            messages: vec![
                AiMessage {
                    role: AiRole::User,
                    content: Some("Review this.".to_string()),
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
                    tool_calls: None,
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::User,
                    content: Some("Now continue.".to_string()),
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
        };

        let gemini_req = translate_ai_request(request)?;
        assert!(gemini_req.contents.iter().all(|c| !c.parts.is_empty()));
        assert_eq!(gemini_req.contents.len(), 1);
        assert_eq!(gemini_req.contents[0].role, "user");
        assert_eq!(gemini_req.contents[0].parts.len(), 2);
        Ok(())
    }

    #[test]
    fn test_summarize_gemini_error() {
        let preempted_json = json!({
            "error": {
                "code": 500,
                "status": "500",
                "message": "A retriable error could not be retried [jax.wiz.servo.ServoErrorDetail] { error_code: DECODE_PREEMPTED }\n=== Source Location Trace: ===\nfoo.cc:123"
            }
        })
        .to_string();

        assert_eq!(
            summarize_gemini_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, &preempted_json),
            "500 Internal Server Error (request preempted in decode queue)"
        );

        let prefill_json = json!({
            "error": {
                "code": 500,
                "status": "500",
                "message": "Overloaded prefill queue. (see go/debugonly for details)\n=== Source Location Trace: ==="
            }
        })
        .to_string();

        assert_eq!(
            summarize_gemini_error(reqwest::StatusCode::INTERNAL_SERVER_ERROR, &prefill_json),
            "500 Internal Server Error (request preempted in prefill queue)"
        );

        let go_link_json = json!({
            "error": {
                "code": 503,
                "message": "Service temporarily overloaded (see go/internal-link for info)"
            }
        })
        .to_string();

        assert_eq!(
            summarize_gemini_error(reqwest::StatusCode::SERVICE_UNAVAILABLE, &go_link_json),
            "503 Service Unavailable: Service temporarily overloaded"
        );
    }

    #[test]
    fn test_translate_ai_request_reasoning_effort_and_fallback() -> Result<()> {
        let base_req = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Check this.".to_string()),
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
        };

        let none_req = translate_ai_request(base_req.clone())?;
        assert_eq!(
            none_req
                .generation_config
                .unwrap()
                .thinking_config
                .unwrap()
                .thinking_level,
            None
        );

        let fallback_req =
            translate_ai_request_with_thinking_level(base_req.clone(), Some("medium"))?;
        assert_eq!(
            fallback_req
                .generation_config
                .unwrap()
                .thinking_config
                .unwrap()
                .thinking_level
                .as_deref(),
            Some("MEDIUM")
        );

        let mut override_req = base_req;
        override_req.reasoning_effort = Some(crate::ai::ReasoningEffort::High);
        let translated = translate_ai_request_with_thinking_level(override_req, Some("medium"))?;
        assert_eq!(
            translated
                .generation_config
                .unwrap()
                .thinking_config
                .unwrap()
                .thinking_level
                .as_deref(),
            Some("HIGH")
        );

        Ok(())
    }

    #[test]
    fn test_translate_ai_request_max_output_tokens_serialization() -> Result<()> {
        let req = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Check this.".to_string()),
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
        };

        let translated = translate_ai_request_with_options(req.clone(), None, Some(131_072))?;
        let json = serde_json::to_value(&translated)?;
        assert_eq!(json["generationConfig"]["maxOutputTokens"], json!(131_072));
        assert!(json.get("continuationToken").is_none());

        let zero = translate_ai_request_with_options(req, None, Some(0))?;
        let zero_json = serde_json::to_value(&zero)?;
        assert!(
            zero_json["generationConfig"]
                .get("maxOutputTokens")
                .is_none()
        );

        let pro_client =
            GeminiClient::new_with_settings("gemini-3.1-pro-preview".to_string(), None);
        assert_eq!(pro_client.max_tokens, 0);
        let pro_with_table = GeminiClient::new_with_settings(
            "gemini-3.1-pro-preview".to_string(),
            Some(&crate::settings::GeminiSettings::default()),
        );
        assert_eq!(pro_with_table.max_tokens, 0);

        let argon_client = GeminiClient::new_with_settings("gemini-4-argon".to_string(), None);
        assert_eq!(argon_client.max_tokens, 131_072);

        let explicit_settings = crate::settings::GeminiSettings {
            max_tokens: Some(65_536),
            ..Default::default()
        };
        let explicit_client = GeminiClient::new_with_settings(
            "gemini-3.1-pro-preview".to_string(),
            Some(&explicit_settings),
        );
        assert_eq!(explicit_client.max_tokens, 65_536);

        Ok(())
    }

    #[test]
    fn test_part_deserialization_thought_only_and_function_call() -> Result<()> {
        // A continuation hop that decodes only thoughts and ends with a
        // thoughtSignature may omit the `text` field on `Part`.
        let raw = json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"text": "thinking part 1 ", "thought": true},
                        {"thoughtSignature": "sig_from_hop_1"},
                        {"functionCall": {"name": "git_diff", "args": {"commit": "HEAD"}}}
                    ]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 100,
                "candidatesTokenCount": 50,
                "thoughtsTokenCount": 150,
                "totalTokenCount": 300
            }
        });

        let parsed: GenerateContentResponse = serde_json::from_value(raw)?;
        let ai_resp = translate_ai_response(parsed)?;
        assert_eq!(ai_resp.thought.as_deref(), Some("thinking part 1 "));
        let usage = ai_resp.usage.expect("expected usage metadata");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(
            usage.completion_tokens, 200,
            "completion_tokens must sum candidatesTokenCount and thoughtsTokenCount"
        );
        assert_eq!(usage.total_tokens, 300);
        let calls = ai_resp.tool_calls.expect("expected tool call");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function_name, "git_diff");
        assert_eq!(
            calls[0].thought_signature.as_deref(),
            Some("sig_from_hop_1"),
            "thoughtSignature from preceding part must propagate to FunctionCall"
        );
        assert_eq!(
            ai_resp.thought_signature, None,
            "signature consumed by FunctionCall must not be duplicated on AiResponse"
        );

        // Non-tool-call responses must retain last_thought_signature on AiResponse,
        // and re-serializing the turn must omit unsigned thought summary text while
        // preserving the signature on the text part.
        let raw_text_only = json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"text": "redacted summary", "thought": true, "thoughtSignature": "sig_text_turn"},
                        {"text": "final answer"}
                    ]
                },
                "finishReason": "STOP"
            }]
        });
        let text_resp = translate_ai_response(serde_json::from_value(raw_text_only)?)?;
        assert_eq!(text_resp.thought.as_deref(), Some("redacted summary"));
        assert_eq!(text_resp.content.as_deref(), Some("final answer"));
        assert_eq!(
            text_resp.thought_signature.as_deref(),
            Some("sig_text_turn")
        );

        let retry_req = translate_ai_request(AiRequest {
            system: None,
            messages: vec![
                AiMessage {
                    role: AiRole::User,
                    content: Some("Review this.".to_string()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::Assistant,
                    content: text_resp.content,
                    thought: text_resp.thought,
                    thought_signature: text_resp.thought_signature,
                    tool_calls: None,
                    tool_call_id: None,
                },
                AiMessage {
                    role: AiRole::User,
                    content: Some("Fix format.".to_string()),
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
        })?;
        assert_eq!(retry_req.contents.len(), 3);
        assert_eq!(retry_req.contents[1].role, "model");
        assert_eq!(
            retry_req.contents[1].parts.len(),
            1,
            "unsigned thought summary must not be replayed into model parts"
        );
        if let Part::Text {
            text,
            thought_signature,
            thought,
        } = &retry_req.contents[1].parts[0]
        {
            assert_eq!(text, "final answer");
            assert_eq!(thought_signature.as_deref(), Some("sig_text_turn"));
            assert!(!thought);
        } else {
            panic!("expected single signed Part::Text on model turn");
        }

        // If an Assistant turn has both text and a tool call that already carries
        // its own signature, the message-level thought_signature attaches to the
        // Part::Text rather than being dropped.
        let both_signed_req = translate_ai_request(AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::Assistant,
                content: Some("calling git_diff".to_string()),
                thought: Some("redacted summary".to_string()),
                thought_signature: Some("text_sig".to_string()),
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".to_string(),
                    function_name: "git_diff".to_string(),
                    arguments: json!({}),
                    thought_signature: Some("call_sig".to_string()),
                }]),
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
        })?;
        assert_eq!(both_signed_req.contents[0].parts.len(), 2);
        assert!(matches!(
            &both_signed_req.contents[0].parts[0],
            Part::Text {
                text,
                thought_signature: Some(sig),
                thought: false,
            } if text == "calling git_diff" && sig == "text_sig"
        ));
        assert!(matches!(
            &both_signed_req.contents[0].parts[1],
            Part::FunctionCall {
                thought_signature: Some(sig),
                ..
            } if sig == "call_sig"
        ));

        // When a FunctionCall carries its own explicit thoughtSignature, any
        // earlier pending signature must be cleared so it does not leak to a
        // subsequent FunctionCall in the same turn.
        let raw_multi = json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [
                        {"thoughtSignature": "stale_sig"},
                        {"functionCall": {"name": "first_call", "args": {}}, "thoughtSignature": "explicit_sig"},
                        {"functionCall": {"name": "second_call", "args": {}}}
                    ]
                },
                "finishReason": "STOP"
            }]
        });
        let multi_resp = translate_ai_response(serde_json::from_value(raw_multi)?)?;
        let multi_calls = multi_resp.tool_calls.expect("expected tool calls");
        assert_eq!(multi_calls.len(), 2);
        assert_eq!(
            multi_calls[0].thought_signature.as_deref(),
            Some("explicit_sig")
        );
        assert_eq!(multi_calls[1].thought_signature, None);

        assert!(
            serde_json::from_value::<Part>(json!({"executableCode": {"code": "1+1"}})).is_err(),
            "unrecognized Part variants must fail deserialization rather than match a catch-all"
        );
        assert!(
            serde_json::from_value::<Part>(json!({})).is_err(),
            "empty Part object must fail deserialization"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_multi_hop_continuation_merges_parts_and_usage() -> Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        let server = tokio::spawn(async move {
            let mut seen_tokens: Vec<Option<String>> = Vec::new();
            let responses = [
                json!({
                    "candidates": [{
                        "content": {
                            "role": "model",
                            "parts": [
                                {"text": "deep thought...", "thought": true, "thoughtSignature": "hop1_sig"},
                                {"text": "{\"findings\": ["}
                            ]
                        },
                        "finishReason": "CONTINUATION",
                        "continuationToken": "tok_hop_1"
                    }],
                    "usageMetadata": {
                        "promptTokenCount": 1000,
                        "candidatesTokenCount": 32768,
                        "thoughtsTokenCount": 1000,
                        "totalTokenCount": 34768,
                        "cachedContentTokenCount": 800
                    }
                }),
                json!({
                    "candidates": [{
                        "content": {
                            "role": "model",
                            "parts": [
                                {"text": "]}"}
                            ]
                        },
                        "finishReason": "STOP"
                    }],
                    "usageMetadata": {
                        "promptTokenCount": 33768,
                        "candidatesTokenCount": 120,
                        "thoughtsTokenCount": 200,
                        "totalTokenCount": 34088,
                        "cachedContentTokenCount": 33000
                    }
                }),
            ];

            for resp_json in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let header_end = loop {
                    let n = stream.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "client closed before sending request headers");
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
                let content_length = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                while buf.len() < header_end + content_length {
                    let n = stream.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "client closed before sending request body");
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body_json: Value =
                    serde_json::from_slice(&buf[header_end..header_end + content_length]).unwrap();
                seen_tokens.push(
                    body_json
                        .get("continuationToken")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                );

                let body_str = resp_json.to_string();
                let wire = test_http::complete("200 OK", "", &body_str);
                stream.write_all(&wire).await.unwrap();
                stream.flush().await.unwrap();
            }
            seen_tokens
        });

        let client = local_client(&format!("http://{addr}"));
        let req = AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Review patch.".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
            reasoning_effort: Some(crate::ai::ReasoningEffort::High),
        };

        let ai_resp = AiProvider::generate_content(&client, req).await?;
        let seen_tokens = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server did not receive all expected continuation hops")?;

        assert_eq!(seen_tokens, vec![None, Some("tok_hop_1".to_string())]);
        assert!(!ai_resp.truncated);
        assert_eq!(ai_resp.thought.as_deref(), Some("deep thought..."));
        assert_eq!(ai_resp.content.as_deref(), Some("{\"findings\": []}"));
        let usage = ai_resp.usage.expect("expected usage metadata");
        assert_eq!(usage.prompt_tokens, 34_768);
        assert_eq!(usage.completion_tokens, 34_088);
        assert_eq!(usage.total_tokens, 68_856);
        assert_eq!(usage.cached_tokens, Some(33_800));

        Ok(())
    }

    #[test]
    fn test_continuation_stops_on_max_tokens_or_duplicate_token() {
        let max_tokens_resp = GenerateContentResponse {
            candidates: Some(vec![Candidate {
                content: Content::default(),
                finish_reason: Some("MAX_TOKENS".to_string()),
                continuation_token: Some("tok_ignored".to_string()),
            }]),
            prompt_feedback: None,
            usage_metadata: None,
        };
        assert_eq!(next_continuation_token(&max_tokens_resp, None), None);

        let cont_resp = GenerateContentResponse {
            candidates: Some(vec![Candidate {
                content: Content::default(),
                finish_reason: Some("CONTINUATION".to_string()),
                continuation_token: Some("tok_same".to_string()),
            }]),
            prompt_feedback: None,
            usage_metadata: None,
        };
        assert_eq!(
            next_continuation_token(&cont_resp, None).as_deref(),
            Some("tok_same")
        );
        assert_eq!(
            next_continuation_token(&cont_resp, Some("tok_same")),
            None,
            "duplicate continuation token must stop the loop"
        );
    }

    #[test]
    fn test_translate_ai_response_rejects_malformed_and_unexpected_tool_calls() {
        for reason in ["MALFORMED_FUNCTION_CALL", "UNEXPECTED_TOOL_CALL"] {
            let resp = GenerateContentResponse {
                candidates: Some(vec![Candidate {
                    content: Content::default(),
                    finish_reason: Some(reason.to_string()),
                    continuation_token: None,
                }]),
                prompt_feedback: None,
                usage_metadata: None,
            };
            let err = translate_ai_response(resp)
                .expect_err("failed tool call finish reason must surface as Err");
            let msg = err.to_string();
            assert!(
                msg.contains(reason),
                "error message {msg:?} must preserve finish reason {reason}"
            );
            assert!(
                !msg.contains("blocked"),
                "tool-call failure error {msg:?} must not trigger recitation/block handling"
            );
        }
    }
}
