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

//! AI provider that shells out to `kiro-cli acp`.
//!
//! By default, Kiro runs under an isolated temporary agent with all native
//! tools disabled. A deny-all pre-tool hook acts as a defensive backstop.
//! This makes the provider a pure completion backend: Sashiko's own ToolBox
//! remains the only tool execution layer.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::debug;

use super::claude_cli::{build_prompt, parse_inner_response};
use super::token_budget::TokenBudget;
use crate::ai::{
    AiErrorClass, AiProvider, AiRequest, AiResponse, AiUsage, ClassifyAiError, DEFAULT_RETRY_AFTER,
    ProviderCapabilities,
};
use crate::utils::redact_secret;

pub struct KiroCliProvider {
    pub model: String,
    pub binary: String,
    pub agent: Option<String>,
    pub context_window_size: usize,
    pub timeout_secs: u64,
}

type StderrPreview = Arc<Mutex<String>>;
type StdoutPreview = Arc<Mutex<String>>;
const STDERR_PREVIEW_LIMIT: usize = 4096;
const STDOUT_PREVIEW_LIMIT: usize = 4096;

const KIRO_RETRY_AFTER: Duration = Duration::from_secs(1);
// Classification only needs enough provider text to catch known Kiro/AWS error
// markers. Error data can include verbose payloads, so keep marker scans bounded.
const KIRO_MAX_CLASSIFICATION_TEXT_CHARS: usize = 64 * 1024;
const KIRO_PERMANENT_MARKERS: &[&str] = &[
    "validationerror",
    "validationexception",
    "accessdeniederror",
    "accessdeniedexception",
    "servicequotaexceedederror",
    "contentlengthexceedsthreshold",
    "invalidconversationid",
    "invalidmodelid",
    "invalid conversation history",
    "prompt is too long",
    "contextwindowoverflow",
    "monthlylimitreached",
    "unauthorized",
    "autherror",
];
const KIRO_STREAM_CONTEXT_MARKERS: &[&str] = &[
    "codewhispererchatresponsestream",
    "qdeveloperchatresponsestream",
    "chatresponsestream",
    "chatresponsestreamerror",
    "chatresponsestreamconversestreamerror",
    "chatresponsestreamunmarshaller",
];
const KIRO_STRONG_STREAM_FAILURE_MARKERS: &[&str] = &[
    "recverrorstreamtimeout",
    "failed to receive the next event",
    "failed to receive the next message",
    "encountered an error in the response stream",
    "request or response body error",
    "connection closed before message completed",
];
const KIRO_WEAK_TRANSIENT_MARKERS: &[&str] = &[
    "recverrorunknown",
    "dispatchfailure",
    "timedout",
    "timeouterror",
    "operation timed out",
    "dispatch failure",
    "error trying to connect",
];
const KIRO_RATE_LIMIT_MARKERS: &[&str] = &[
    "throttlingerror",
    "throttlingexception",
    "too many requests",
    "rate limit",
    "ratelimit",
];
const KIRO_PROVIDER_TRANSIENT_MARKERS: &[&str] = &[
    "serviceunavailableerror",
    "modeltemporarilyunavailable",
    "modeloverloadederror",
    "requesttimeoutexception",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KiroMarkerClass {
    Permanent,
    StrongStreamFailure,
    StreamContextWeakTransient,
    RateLimit,
    ProviderAvailability,
    Unclassified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KiroAcpErrorClassification {
    class: AiErrorClass,
    marker_class: KiroMarkerClass,
    matched_marker: Option<&'static str>,
}

impl KiroAcpErrorClassification {
    fn new(
        class: AiErrorClass,
        marker_class: KiroMarkerClass,
        matched_marker: Option<&'static str>,
    ) -> Self {
        Self {
            class,
            marker_class,
            matched_marker,
        }
    }
}

fn find_marker(text: &str, markers: &'static [&'static str]) -> Option<&'static str> {
    markers.iter().copied().find(|marker| text.contains(marker))
}

fn normalized_acp_error_text(message: &str, data: Option<&Value>) -> String {
    let mut text = String::new();
    push_ascii_lowercase_bounded(&mut text, message, KIRO_MAX_CLASSIFICATION_TEXT_CHARS);
    if let Some(data) = data {
        push_ascii_lowercase_bounded(&mut text, "\n", KIRO_MAX_CLASSIFICATION_TEXT_CHARS);
        match data {
            Value::String(s) => {
                push_ascii_lowercase_bounded(&mut text, s, KIRO_MAX_CLASSIFICATION_TEXT_CHARS)
            }
            _ => {
                push_json_value_for_marker_scan(&mut text, data, KIRO_MAX_CLASSIFICATION_TEXT_CHARS)
            }
        }
    }
    text
}

fn push_ascii_lowercase_bounded(buffer: &mut String, text: &str, max_chars: usize) {
    let mut remaining = max_chars.saturating_sub(buffer.chars().count());
    for ch in text.chars() {
        if remaining == 0 {
            break;
        }
        buffer.push(ch.to_ascii_lowercase());
        remaining -= 1;
    }
}

fn push_json_value_for_marker_scan(buffer: &mut String, value: &Value, max_chars: usize) {
    if buffer.chars().count() >= max_chars {
        return;
    }

    match value {
        Value::Null => push_ascii_lowercase_bounded(buffer, "null", max_chars),
        Value::Bool(value) => push_ascii_lowercase_bounded(buffer, &value.to_string(), max_chars),
        Value::Number(value) => push_ascii_lowercase_bounded(buffer, &value.to_string(), max_chars),
        Value::String(value) => push_ascii_lowercase_bounded(buffer, value, max_chars),
        Value::Array(values) => {
            push_ascii_lowercase_bounded(buffer, "[", max_chars);
            for value in values {
                if buffer.chars().count() >= max_chars {
                    break;
                }
                push_json_value_for_marker_scan(buffer, value, max_chars);
                push_ascii_lowercase_bounded(buffer, ",", max_chars);
            }
            push_ascii_lowercase_bounded(buffer, "]", max_chars);
        }
        Value::Object(values) => {
            push_ascii_lowercase_bounded(buffer, "{", max_chars);
            for (key, value) in values {
                if buffer.chars().count() >= max_chars {
                    break;
                }
                push_ascii_lowercase_bounded(buffer, key, max_chars);
                push_ascii_lowercase_bounded(buffer, ":", max_chars);
                push_json_value_for_marker_scan(buffer, value, max_chars);
                push_ascii_lowercase_bounded(buffer, ",", max_chars);
            }
            push_ascii_lowercase_bounded(buffer, "}", max_chars);
        }
    }
}

#[cfg(test)]
fn classify_kiro_acp_error(code: i64, message: &str, data: Option<&Value>) -> AiErrorClass {
    classify_kiro_acp_error_with_details(code, message, data).class
}

fn classify_kiro_acp_error_with_details(
    code: i64,
    message: &str,
    data: Option<&Value>,
) -> KiroAcpErrorClassification {
    if code != -32603 {
        return KiroAcpErrorClassification::new(
            AiErrorClass::Fatal,
            KiroMarkerClass::Unclassified,
            None,
        );
    }

    let text = normalized_acp_error_text(message, data);
    if let Some(marker) = find_marker(&text, KIRO_PERMANENT_MARKERS) {
        return KiroAcpErrorClassification::new(
            AiErrorClass::Fatal,
            KiroMarkerClass::Permanent,
            Some(marker),
        );
    }

    if let Some(marker) = find_marker(&text, KIRO_STRONG_STREAM_FAILURE_MARKERS) {
        return KiroAcpErrorClassification::new(
            AiErrorClass::Transient {
                retry_after: KIRO_RETRY_AFTER,
            },
            KiroMarkerClass::StrongStreamFailure,
            Some(marker),
        );
    }

    if find_marker(&text, KIRO_STREAM_CONTEXT_MARKERS).is_some()
        && let Some(marker) = find_marker(&text, KIRO_WEAK_TRANSIENT_MARKERS)
    {
        return KiroAcpErrorClassification::new(
            AiErrorClass::Transient {
                retry_after: KIRO_RETRY_AFTER,
            },
            KiroMarkerClass::StreamContextWeakTransient,
            Some(marker),
        );
    }

    if let Some(marker) = find_marker(&text, KIRO_RATE_LIMIT_MARKERS) {
        return KiroAcpErrorClassification::new(
            AiErrorClass::RateLimit {
                retry_after: DEFAULT_RETRY_AFTER,
            },
            KiroMarkerClass::RateLimit,
            Some(marker),
        );
    }

    if let Some(marker) = find_marker(&text, KIRO_PROVIDER_TRANSIENT_MARKERS) {
        return KiroAcpErrorClassification::new(
            AiErrorClass::Transient {
                retry_after: KIRO_RETRY_AFTER,
            },
            KiroMarkerClass::ProviderAvailability,
            Some(marker),
        );
    }

    KiroAcpErrorClassification::new(AiErrorClass::Fatal, KiroMarkerClass::Unclassified, None)
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct KiroCliError {
    message: String,
    class: AiErrorClass,
}

impl KiroCliError {
    fn new(message: String, class: AiErrorClass) -> Self {
        Self { message, class }
    }
}

impl ClassifyAiError for KiroCliError {
    fn ai_error_class(&self) -> AiErrorClass {
        self.class
    }
}

/// Agent JSON for the isolated no-tool Sashiko provider agent.
const AGENT_JSON: &str = r#"{
  "name": "sashiko-provider",
  "description": "Stateless Sashiko completion backend. Native Kiro tools are disabled.",
  "prompt": "Follow the user-provided instructions exactly.",
  "mcpServers": {},
  "tools": [],
  "allowedTools": [],
  "resources": [],
  "includeMcpJson": false,
  "hooks": {
    "preToolUse": [
      {
        "command": ".kiro/hooks/deny-all-tools.sh"
      }
    ]
  }
}"#;

/// Shell script that denies all Kiro native tool invocations.
const DENY_ALL_HOOK: &str = "#!/bin/sh\n\
echo \"Kiro native tools are disabled for the Sashiko provider\" >&2\n\
exit 1\n";

/// Build the kiro-cli command arguments.
fn build_args(model: &str, agent: &str) -> Vec<String> {
    vec![
        "acp".to_string(),
        "--agent".to_string(),
        agent.to_string(),
        "--model".to_string(),
        model.to_string(),
    ]
}

/// Create the isolated temporary workspace with a no-tool agent and deny-all hook.
/// Returns the TempDir (must be kept alive for the duration of the process).
fn create_isolated_workspace() -> Result<TempDir> {
    let tmp = tempfile::tempdir()?;
    let agents_dir = tmp.path().join(".kiro/agents");
    std::fs::create_dir_all(&agents_dir)?;
    std::fs::write(agents_dir.join("sashiko-provider.json"), AGENT_JSON)?;

    let hooks_dir = tmp.path().join(".kiro/hooks");
    std::fs::create_dir_all(&hooks_dir)?;
    let hook_path = hooks_dir.join("deny-all-tools.sh");
    std::fs::write(&hook_path, DENY_ALL_HOOK)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))?;
    }

    Ok(tmp)
}

async fn write_rpc(stdin: &mut ChildStdin, id: u64, method: &str, params: Value) -> Result<()> {
    let msg = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let mut line = serde_json::to_string(&msg)?;
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

async fn write_rpc_checked(
    stdin: &mut ChildStdin,
    id: u64,
    method: &str,
    params: Value,
    stderr_preview: &StderrPreview,
    stdout_preview: &StdoutPreview,
) -> Result<()> {
    if let Err(e) = write_rpc(stdin, id, method, params).await {
        anyhow::bail!(
            "kiro-cli ACP write failed for {}: {}{}",
            method,
            e,
            diagnostic_context(stderr_preview, stdout_preview).await
        );
    }
    Ok(())
}

async fn read_rpc_response(
    lines: &mut Lines<BufReader<ChildStdout>>,
    target_id: u64,
    stderr_preview: &StderrPreview,
    stdout_preview: &StdoutPreview,
    mut chunks: Option<&mut Vec<String>>,
) -> Result<Value> {
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => {
                anyhow::bail!(
                    "kiro-cli ACP exited before response {}{}",
                    target_id,
                    diagnostic_context(stderr_preview, stdout_preview).await
                );
            }
            Err(e) => {
                anyhow::bail!(
                    "kiro-cli ACP stdout read failed before response {}: {}{}",
                    target_id,
                    e,
                    diagnostic_context(stderr_preview, stdout_preview).await
                );
            }
        };
        let msg: Value = match serde_json::from_str(&line) {
            Ok(msg) => msg,
            Err(e) => {
                record_malformed_stdout_line(stdout_preview, &line).await;
                debug!("Ignoring malformed ACP stdout line: {} ({})", line, e);
                continue;
            }
        };

        if msg.get("id").and_then(Value::as_u64) == Some(target_id) {
            if let Some(error) = msg.get("error") {
                let code = error.get("code").and_then(Value::as_i64).unwrap_or(-1);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown ACP error");
                let data = error.get("data");
                let classification = classify_kiro_acp_error_with_details(code, message, data);
                let data_context = acp_error_data_context(error);
                let diagnostic = diagnostic_context(stderr_preview, stdout_preview).await;
                let message = format!(
                    "kiro-cli ACP error {}: {}{}{}",
                    code, message, data_context, diagnostic
                );
                return Err(KiroCliError::new(message, classification.class).into());
            }
            return Ok(msg.get("result").cloned().unwrap_or(Value::Null));
        }

        if let Some(text) = extract_acp_text_chunk(&msg)
            && let Some(chunks) = chunks.as_deref_mut()
        {
            chunks.push(text);
        }
    }
}

async fn record_stderr_line(stderr_preview: &StderrPreview, line: &str) {
    let redacted = redact_secret(line);
    debug!("[kiro-cli acp stderr] {}", redacted);

    if redacted.trim().is_empty() {
        return;
    }

    let mut preview = stderr_preview.lock().await;
    if !preview.is_empty() {
        preview.push('\n');
    }
    preview.push_str(redacted.trim_end());
    trim_stderr_preview(&mut preview);
}

async fn record_malformed_stdout_line(stdout_preview: &StdoutPreview, line: &str) {
    let redacted = redact_secret(line);

    if redacted.trim().is_empty() {
        return;
    }

    let mut preview = stdout_preview.lock().await;
    if !preview.is_empty() {
        preview.push('\n');
    }
    preview.push_str(redacted.trim_end());
    trim_stdout_preview(&mut preview);
}

fn trim_stderr_preview(preview: &mut String) {
    if preview.len() <= STDERR_PREVIEW_LIMIT {
        return;
    }

    let excess = preview.len() - STDERR_PREVIEW_LIMIT;
    let drain_to = preview
        .char_indices()
        .find_map(|(idx, _)| (idx >= excess).then_some(idx))
        .unwrap_or(preview.len());
    preview.drain(..drain_to);
}

fn trim_stdout_preview(preview: &mut String) {
    if preview.len() <= STDOUT_PREVIEW_LIMIT {
        return;
    }

    let excess = preview.len() - STDOUT_PREVIEW_LIMIT;
    let drain_to = preview
        .char_indices()
        .find_map(|(idx, _)| (idx >= excess).then_some(idx))
        .unwrap_or(preview.len());
    preview.drain(..drain_to);
}

fn acp_error_data_context(error: &Value) -> String {
    match error.get("data") {
        Some(data) if !data.is_null() => match serde_json::to_string(data) {
            Ok(data) => format!("; data: {}", redact_secret(&data)),
            Err(_) => String::new(),
        },
        _ => String::new(),
    }
}

async fn diagnostic_context(
    stderr_preview: &StderrPreview,
    stdout_preview: &StdoutPreview,
) -> String {
    let stderr = stderr_preview.lock().await.trim().to_string();
    let stdout = stdout_preview.lock().await.trim().to_string();

    match (stderr.is_empty(), stdout.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("; stderr: {}", stderr),
        (true, false) => format!("; malformed stdout: {}", stdout),
        (false, false) => format!("; stderr: {}; malformed stdout: {}", stderr, stdout),
    }
}

fn extract_acp_text_chunk(msg: &Value) -> Option<String> {
    if msg.get("method")?.as_str()? != "session/update" {
        return None;
    }

    let update = msg.get("params")?.get("update")?;
    let update_type = update
        .get("sessionUpdate")
        .or_else(|| update.get("type"))?
        .as_str()?;
    if !matches!(update_type, "AgentMessageChunk" | "agent_message_chunk") {
        return None;
    }

    extract_text_content(update.get("content")?)
}

fn extract_text_content(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => map
            .get("text")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(extract_text_content)
                .collect::<Vec<_>>()
                .join("");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

impl KiroCliProvider {
    async fn run_acp_prompt(
        &self,
        prompt: &str,
        agent_name: &str,
        isolated_workspace: Option<&TempDir>,
    ) -> Result<String> {
        let args = build_args(&self.model, agent_name);

        let mut cmd = Command::new(&self.binary);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(tmp) = isolated_workspace {
            cmd.current_dir(tmp.path());
        }

        cmd.kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("Failed to spawn kiro-cli ACP: {}. Is it installed?", e)
        })?;

        let stderr_preview = Arc::new(Mutex::new(String::new()));
        let stdout_preview = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            let stderr_preview = stderr_preview.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    record_stderr_line(&stderr_preview, &line).await;
                }
            });
        }

        let mut stdin = child.stdin.take().context("kiro-cli ACP stdin missing")?;
        let stdout = child.stdout.take().context("kiro-cli ACP stdout missing")?;
        let mut lines = BufReader::new(stdout).lines();
        let mut next_id = 0u64;

        write_rpc_checked(
            &mut stdin,
            next_id,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {
                    "name": "sashiko",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
            &stderr_preview,
            &stdout_preview,
        )
        .await?;
        read_rpc_response(&mut lines, next_id, &stderr_preview, &stdout_preview, None).await?;
        next_id += 1;

        write_rpc_checked(
            &mut stdin,
            next_id,
            "session/new",
            json!({
                "cwd": ".",
                "mcpServers": [],
            }),
            &stderr_preview,
            &stdout_preview,
        )
        .await?;
        let session =
            read_rpc_response(&mut lines, next_id, &stderr_preview, &stdout_preview, None).await?;
        let session_id = match session.get("sessionId").and_then(Value::as_str) {
            Some(session_id) => session_id.to_string(),
            None => {
                anyhow::bail!(
                    "kiro-cli ACP session/new response missing sessionId{}",
                    diagnostic_context(&stderr_preview, &stdout_preview).await
                );
            }
        };
        next_id += 1;

        write_rpc_checked(
            &mut stdin,
            next_id,
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [
                    {
                        "type": "text",
                        "text": prompt,
                    }
                ],
            }),
            &stderr_preview,
            &stdout_preview,
        )
        .await?;
        let mut chunks = Vec::new();
        read_rpc_response(
            &mut lines,
            next_id,
            &stderr_preview,
            &stdout_preview,
            Some(&mut chunks),
        )
        .await?;

        drop(stdin);
        let _ = child.kill().await;

        Ok(chunks.join(""))
    }
}

#[async_trait]
impl AiProvider for KiroCliProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let prompt = build_prompt(&request);
        debug!("kiro-cli prompt length: {} chars", prompt.len());
        let prompt_tokens = TokenBudget::estimate_tokens(&prompt);

        let (agent_name, isolated_workspace) = match &self.agent {
            Some(a) => (a.clone(), None),
            None => {
                let tmp = create_isolated_workspace()?;
                ("sashiko-provider".to_string(), Some(tmp))
            }
        };

        let text = timeout(
            Duration::from_secs(self.timeout_secs),
            self.run_acp_prompt(&prompt, &agent_name, isolated_workspace.as_ref()),
        )
        .await
        .map_err(|_| {
            KiroCliError::new(
                format!(
                    "kiro-cli ACP timed out after {} seconds (model={}, prompt_chars={}, estimated_prompt_tokens={})",
                    self.timeout_secs,
                    self.model,
                    prompt.len(),
                    prompt_tokens
                ),
                AiErrorClass::Transient {
                    retry_after: DEFAULT_RETRY_AFTER,
                },
            )
        })??;

        // Synthesize usage from token estimates since kiro-cli does not
        // expose provider token counts.
        let completion_tokens = TokenBudget::estimate_tokens(&text);
        let usage = Some(AiUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
            cached_tokens: None,
        });

        parse_inner_response(&text, usage)
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        let prompt = build_prompt(request);
        TokenBudget::estimate_tokens(&prompt)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: self.context_window_size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiMessage, AiRole, AiTool, create_provider};
    use crate::settings::Settings;

    fn sample_request() -> AiRequest {
        AiRequest {
            system: Some("You are a kernel reviewer.".to_string()),
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Review this patch.".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: Some(vec![AiTool {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }]),
            temperature: None,
            response_format: None,
            context_tag: None,
        }
    }

    #[test]
    fn test_factory_creates_provider() {
        let mut settings = Settings::new().expect("Failed to load settings");
        settings.ai.provider = "kiro-cli".to_string();
        settings.ai.model = "claude-sonnet-4".to_string();

        let provider = create_provider(&settings).unwrap();
        let caps = provider.get_capabilities();
        assert_eq!(caps.model_name, "claude-sonnet-4");
        assert_eq!(caps.context_window_size, 200_000);
    }

    #[test]
    fn test_command_args_default() {
        let args = build_args("claude-sonnet-4", "sashiko-provider");
        assert_eq!(args[0], "acp");
        assert_eq!(args[1], "--agent");
        assert_eq!(args[2], "sashiko-provider");
        assert_eq!(args[3], "--model");
        assert_eq!(args[4], "claude-sonnet-4");
    }

    #[test]
    fn test_command_args_custom_agent() {
        let args = build_args("claude-sonnet-4", "my-agent");
        assert_eq!(
            args,
            vec![
                "acp".to_string(),
                "--agent".to_string(),
                "my-agent".to_string(),
                "--model".to_string(),
                "claude-sonnet-4".to_string(),
            ]
        );
    }

    #[test]
    fn test_isolated_workspace_agent_json() {
        let tmp = create_isolated_workspace().unwrap();
        let agent_path = tmp.path().join(".kiro/agents/sashiko-provider.json");
        assert!(agent_path.exists());

        let content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&agent_path).unwrap()).unwrap();

        assert_eq!(content["tools"], serde_json::json!([]));
        assert_eq!(content["allowedTools"], serde_json::json!([]));
        assert_eq!(content["mcpServers"], serde_json::json!({}));
        assert_eq!(content["resources"], serde_json::json!([]));
        assert_eq!(content["includeMcpJson"], serde_json::json!(false));
        assert!(
            content["prompt"]
                .as_str()
                .unwrap()
                .contains("Follow the user-provided instructions exactly")
        );

        // Verify deny-all hook is wired
        let hooks = &content["hooks"]["preToolUse"];
        assert!(hooks.is_array());
        let hook_cmd = hooks[0]["command"].as_str().unwrap();
        assert_eq!(hook_cmd, ".kiro/hooks/deny-all-tools.sh");
    }

    #[test]
    fn test_deny_all_hook_exits_nonzero() {
        let tmp = create_isolated_workspace().unwrap();
        let hook_path = tmp.path().join(".kiro/hooks/deny-all-tools.sh");
        assert!(hook_path.exists());

        let output = std::process::Command::new("sh")
            .arg(&hook_path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("disabled"));
    }

    #[test]
    fn test_extract_acp_text_chunk_snake_case() {
        let input = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "hello"}
                }
            }
        });
        assert_eq!(extract_acp_text_chunk(&input).as_deref(), Some("hello"));
    }

    #[test]
    fn test_extract_acp_text_chunk_camel_case() {
        let input = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "AgentMessageChunk",
                    "content": [
                        {"type": "text", "text": "hel"},
                        {"type": "text", "text": "lo"}
                    ]
                }
            }
        });
        assert_eq!(extract_acp_text_chunk(&input).as_deref(), Some("hello"));
    }

    #[test]
    fn test_extract_acp_ignores_tool_updates() {
        let input = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {"sessionUpdate": "tool_call", "content": {"text": "ignored"}}
            }
        });
        assert!(extract_acp_text_chunk(&input).is_none());
    }

    #[test]
    fn test_parse_tool_calls_json() {
        let text = r#"{"tool_calls":[{"id":"c1","function_name":"read_file","arguments":{"path":"README.md"}}]}"#;
        let resp = parse_inner_response(text, None).unwrap();
        assert!(resp.tool_calls.is_some());
        let calls = resp.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function_name, "read_file");
        assert_eq!(calls[0].arguments["path"], "README.md");
    }

    #[test]
    fn test_parse_plain_content() {
        let text = r#"{"content":"No issues found in this patch."}"#;
        let resp = parse_inner_response(text, None).unwrap();
        assert_eq!(
            resp.content.as_deref(),
            Some("No issues found in this patch.")
        );
        assert!(resp.tool_calls.is_none());
    }

    #[test]
    fn test_parse_raw_text_fallback() {
        let text = "This is not JSON at all.";
        let resp = parse_inner_response(text, None).unwrap();
        assert_eq!(resp.content.as_deref(), Some(text));
    }

    #[test]
    fn test_estimate_tokens_uses_token_budget() {
        let provider = KiroCliProvider {
            model: "test".to_string(),
            binary: "kiro-cli".to_string(),
            agent: None,
            context_window_size: 200_000,
            timeout_secs: 300,
        };
        let req = sample_request();
        let estimate = provider.estimate_tokens(&req);
        // The prompt is non-empty, so estimate should be > 0
        assert!(estimate > 0);
    }

    #[tokio::test]
    async fn test_generate_content_with_fake_acp_server() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
i=0
while IFS= read -r line; do
  case "$i" in
    0) printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1}}' ;;
    1) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s1"}}' ;;
    2)
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"{\"content\":\"ok\"}"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn"}}'
      exit 0
      ;;
  esac
  i=$((i + 1))
done
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let provider = KiroCliProvider {
            model: "test".to_string(),
            binary: fake.to_string_lossy().to_string(),
            agent: None,
            context_window_size: 200_000,
            timeout_secs: 5,
        };
        let response = provider.generate_content(sample_request()).await.unwrap();
        assert_eq!(response.content.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn test_generate_content_includes_redacted_stderr_on_startup_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
printf '%s\n' 'authentication failed token=abc123' >&2
sleep 0.1
exit 2
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let provider = KiroCliProvider {
            model: "test".to_string(),
            binary: fake.to_string_lossy().to_string(),
            agent: None,
            context_window_size: 200_000,
            timeout_secs: 5,
        };

        let err = provider
            .generate_content(sample_request())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("kiro-cli ACP exited before response 0"));
        assert!(err.contains("stderr: authentication failed token=[REDACTED]"));
        assert!(!err.contains("abc123"));
    }

    #[tokio::test]
    async fn test_generate_content_includes_redacted_acp_error_data() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' '{"jsonrpc":"2.0","id":0,"error":{"code":-32603,"message":"Internal error","data":{"kind":"ServiceFailure","reason":"Encountered an error in the response stream: CodewhispererChatResponseStream(DispatchFailure(TimedOut)) token=abc123"}}}'
  exit 0
done
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let provider = KiroCliProvider {
            model: "test".to_string(),
            binary: fake.to_string_lossy().to_string(),
            agent: None,
            context_window_size: 200_000,
            timeout_secs: 5,
        };

        let err = provider
            .generate_content(sample_request())
            .await
            .unwrap_err();
        assert!(matches!(
            crate::ai::classify_ai_error(&err),
            AiErrorClass::Transient { .. }
        ));
        let err = err.to_string();
        assert!(err.contains("kiro-cli ACP error -32603: Internal error"));
        assert!(err.contains("data:"));
        assert!(err.contains("ServiceFailure"));
        assert!(err.contains("CodewhispererChatResponseStream"));
        assert!(err.contains("token=[REDACTED]"));
        assert!(!err.contains("abc123"));
    }

    #[tokio::test]
    async fn test_generate_content_includes_redacted_malformed_stdout_context() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
while IFS= read -r line; do
  printf '%s\n' 'kiro warning token=abc123'
  printf '%s\n' '{"jsonrpc":"2.0","id":0,"error":{"code":-32603,"message":"Internal error"}}'
  exit 0
done
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let provider = KiroCliProvider {
            model: "test".to_string(),
            binary: fake.to_string_lossy().to_string(),
            agent: None,
            context_window_size: 200_000,
            timeout_secs: 5,
        };

        let err = provider
            .generate_content(sample_request())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("kiro-cli ACP error -32603: Internal error"));
        assert!(err.contains("malformed stdout: kiro warning token=[REDACTED]"));
        assert!(!err.contains("abc123"));
    }

    #[tokio::test]
    async fn test_generate_content_timeout_is_transient() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
sleep 2
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let provider = KiroCliProvider {
            model: "test".to_string(),
            binary: fake.to_string_lossy().to_string(),
            agent: None,
            context_window_size: 200_000,
            timeout_secs: 0,
        };

        let err = provider
            .generate_content(sample_request())
            .await
            .unwrap_err();
        assert!(matches!(
            crate::ai::classify_ai_error(&err),
            AiErrorClass::Transient { .. }
        ));
        let err = err.to_string();
        assert!(err.contains("kiro-cli ACP timed out after 0 seconds"));
        assert!(err.contains("model=test"));
        assert!(err.contains("prompt_chars="));
        assert!(err.contains("estimated_prompt_tokens="));
    }

    #[test]
    fn test_kiro_acp_stream_context_and_timedout_is_transient() {
        let data = json!("CodewhispererChatResponseStream(DispatchFailure(TimedOut))");
        assert!(matches!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::Transient { retry_after } if retry_after == KIRO_RETRY_AFTER
        ));
    }

    #[test]
    fn test_kiro_acp_strong_stream_marker_is_transient() {
        let data = json!("failed to receive the next event");
        assert!(matches!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::Transient { retry_after } if retry_after == KIRO_RETRY_AFTER
        ));
    }

    #[test]
    fn test_kiro_acp_weak_marker_alone_is_fatal() {
        let data = json!("TimedOut");
        assert_eq!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::Fatal
        );
    }

    #[test]
    fn test_kiro_acp_provider_operation_context_does_not_make_stream_timeout() {
        let data = json!("GenerateAssistantResponse TimedOut");
        assert_eq!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::Fatal
        );
    }

    #[test]
    fn test_kiro_acp_classification_text_is_bounded() {
        let marker_after_cap = format!(
            "{} unexpected eof",
            "x".repeat(KIRO_MAX_CLASSIFICATION_TEXT_CHARS + 1)
        );
        assert_eq!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&json!(marker_after_cap))),
            AiErrorClass::Fatal
        );

        let marker_before_cap = format!(
            "failed to receive the next event {}",
            "x".repeat(KIRO_MAX_CLASSIFICATION_TEXT_CHARS + 1)
        );
        assert!(matches!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&json!(marker_before_cap))),
            AiErrorClass::Transient { .. }
        ));
    }

    #[test]
    fn test_kiro_acp_classification_scans_bounded_structured_data() {
        let marker_before_cap = json!(["failed to receive the next event", {
            "padding": "x".repeat(KIRO_MAX_CLASSIFICATION_TEXT_CHARS + 1),
        }]);
        assert!(matches!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&marker_before_cap)),
            AiErrorClass::Transient { .. }
        ));

        let marker_after_cap = json!([
            "x".repeat(KIRO_MAX_CLASSIFICATION_TEXT_CHARS + 1),
            "failed to receive the next event"
        ]);
        assert_eq!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&marker_after_cap)),
            AiErrorClass::Fatal
        );
    }

    #[test]
    fn test_kiro_acp_permanent_marker_wins_over_stream_marker() {
        let data = json!(
            "invalid conversation history CodewhispererChatResponseStream DispatchFailure TimedOut"
        );
        assert_eq!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::Fatal
        );
    }

    #[test]
    fn test_kiro_acp_provider_availability_is_transient() {
        let data = json!({"kind": "ModelOverloadedError"});
        assert!(matches!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::Transient { retry_after } if retry_after == KIRO_RETRY_AFTER
        ));
    }

    #[test]
    fn test_kiro_acp_additional_strong_stream_markers_are_transient() {
        for marker in [
            "failed to receive the next message",
            "connection closed before message completed",
            "RecvErrorStreamTimeout",
        ] {
            let data = json!(marker);
            assert!(
                matches!(
                    classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
                    AiErrorClass::Transient { .. }
                ),
                "marker should be transient: {marker}"
            );
        }
    }

    #[test]
    fn test_kiro_acp_context_marker_alone_is_fatal() {
        let data = json!("ChatResponseStreamUnmarshaller");
        assert_eq!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::Fatal
        );
    }

    #[test]
    fn test_kiro_acp_object_data_with_only_timedout_is_fatal() {
        let data = json!({"reason": "TimedOut"});
        assert_eq!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::Fatal
        );
    }

    #[test]
    fn test_kiro_acp_throttling_is_rate_limit() {
        let data = json!({"kind": "ThrottlingError"});
        assert!(matches!(
            classify_kiro_acp_error(-32603, "Internal error", Some(&data)),
            AiErrorClass::RateLimit { retry_after } if retry_after == DEFAULT_RETRY_AFTER
        ));
    }

    #[test]
    fn test_kiro_acp_classification_reports_marker_details() {
        let data = json!("CodewhispererChatResponseStream(DispatchFailure(TimedOut))");
        let classification =
            classify_kiro_acp_error_with_details(-32603, "Internal error", Some(&data));
        assert_eq!(
            classification.marker_class,
            KiroMarkerClass::StreamContextWeakTransient
        );
        assert_eq!(classification.matched_marker, Some("dispatchfailure"));
    }

    #[test]
    fn test_redact_secret_available_for_error_previews() {
        let redacted = redact_secret("kiro failed with token=abc123");
        assert_eq!(redacted, "kiro failed with token=[REDACTED]");
    }
}
