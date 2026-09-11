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

//! Shared Agent Client Protocol (ACP) stdio transport.
//!
//! Several coding agents expose an ACP server on stdio. Sashiko drives one as
//! a stateless completion backend: initialize, open a session, send a single
//! prompt, collect the streamed agent text, then kill the child. The agent's
//! own tools are never used -- Sashiko's ToolBox stays the only tool layer, so
//! callers are expected to launch the agent in whatever no-tool mode it offers.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::debug;

use crate::utils::redact_secret;

pub type StderrPreview = Arc<Mutex<String>>;

const STDERR_PREVIEW_LIMIT: usize = 4096;

/// One ACP subprocess invocation.
pub struct AcpProcess {
    /// Prefix used in error messages, e.g. "kiro-cli ACP".
    pub label: String,
    /// Executable to spawn.
    pub binary: String,
    /// Arguments that put the executable into ACP stdio mode.
    pub args: Vec<String>,
    /// Working directory of the child process.
    pub working_dir: Option<PathBuf>,
    /// Environment overrides for the child. The parent environment is
    /// inherited; these entries win.
    pub env: BTreeMap<String, String>,
    /// Value sent as the `cwd` of `session/new`. Some agents require an
    /// absolute path here.
    pub session_cwd: String,
}

/// Result of a single ACP prompt.
pub struct AcpPrompt {
    /// Concatenated agent message chunks.
    pub text: String,
    /// Raw `session/prompt` result, which carries the stop reason and, on some
    /// agents, token usage.
    pub result: Value,
}

impl AcpProcess {
    /// Runs one prompt to completion against a freshly spawned ACP agent.
    pub async fn run_prompt(&self, prompt: &str) -> Result<AcpPrompt> {
        let mut cmd = Command::new(&self.binary);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(dir) = &self.working_dir {
            cmd.current_dir(dir);
        }
        for (key, value) in &self.env {
            cmd.env(key, value);
        }

        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("Failed to spawn {}: {}. Is it installed?", self.label, e)
        })?;

        let stderr_preview: StderrPreview = Arc::new(Mutex::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            let stderr_preview = stderr_preview.clone();
            let label = self.label.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    record_stderr_line(&stderr_preview, &label, &line).await;
                }
            });
        }

        let mut stdin = child
            .stdin
            .take()
            .with_context(|| format!("{} stdin missing", self.label))?;
        let stdout = child
            .stdout
            .take()
            .with_context(|| format!("{} stdout missing", self.label))?;
        let mut lines = BufReader::new(stdout).lines();
        let mut next_id = 0u64;

        self.write_rpc_checked(
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
        )
        .await?;
        self.read_rpc_response(&mut lines, next_id, &stderr_preview, None)
            .await?;
        next_id += 1;

        self.write_rpc_checked(
            &mut stdin,
            next_id,
            "session/new",
            json!({
                "cwd": self.session_cwd,
                "mcpServers": [],
            }),
            &stderr_preview,
        )
        .await?;
        let session = self
            .read_rpc_response(&mut lines, next_id, &stderr_preview, None)
            .await?;
        let session_id = match session.get("sessionId").and_then(Value::as_str) {
            Some(session_id) => session_id.to_string(),
            None => {
                anyhow::bail!(
                    "{} session/new response missing sessionId{}",
                    self.label,
                    stderr_context(&stderr_preview).await
                );
            }
        };
        next_id += 1;

        self.write_rpc_checked(
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
        )
        .await?;
        let mut chunks = Vec::new();
        let result = self
            .read_rpc_response(&mut lines, next_id, &stderr_preview, Some(&mut chunks))
            .await?;

        drop(stdin);
        let _ = child.kill().await;

        Ok(AcpPrompt {
            text: chunks.join(""),
            result,
        })
    }

    async fn write_rpc_checked(
        &self,
        stdin: &mut ChildStdin,
        id: u64,
        method: &str,
        params: Value,
        stderr_preview: &StderrPreview,
    ) -> Result<()> {
        if let Err(e) = write_rpc(stdin, id, method, params).await {
            anyhow::bail!(
                "{} write failed for {}: {}{}",
                self.label,
                method,
                e,
                stderr_context(stderr_preview).await
            );
        }
        Ok(())
    }

    async fn read_rpc_response(
        &self,
        lines: &mut Lines<BufReader<ChildStdout>>,
        target_id: u64,
        stderr_preview: &StderrPreview,
        mut chunks: Option<&mut Vec<String>>,
    ) -> Result<Value> {
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => {
                    anyhow::bail!(
                        "{} exited before response {}{}",
                        self.label,
                        target_id,
                        stderr_context(stderr_preview).await
                    );
                }
                Err(e) => {
                    anyhow::bail!(
                        "{} stdout read failed before response {}: {}{}",
                        self.label,
                        target_id,
                        e,
                        stderr_context(stderr_preview).await
                    );
                }
            };
            let msg: Value = match serde_json::from_str(&line) {
                Ok(msg) => msg,
                Err(e) => {
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
                    anyhow::bail!(
                        "{} error {}: {}{}",
                        self.label,
                        code,
                        message,
                        stderr_context(stderr_preview).await
                    );
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

async fn record_stderr_line(stderr_preview: &StderrPreview, label: &str, line: &str) {
    let redacted = redact_secret(line);
    debug!("[{} stderr] {}", label, redacted);

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

pub async fn stderr_context(stderr_preview: &StderrPreview) -> String {
    let preview = stderr_preview.lock().await.trim().to_string();
    if preview.is_empty() {
        String::new()
    } else {
        format!("; stderr: {}", preview)
    }
}

pub fn extract_acp_text_chunk(msg: &Value) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_snake_case_chunk() {
        let input = json!({
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
    fn extracts_camel_case_chunk_array() {
        let input = json!({
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
    fn ignores_tool_call_updates() {
        let input = json!({
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
    fn trims_preview_to_limit() {
        let mut preview = "x".repeat(STDERR_PREVIEW_LIMIT + 128);
        trim_stderr_preview(&mut preview);
        assert_eq!(preview.len(), STDERR_PREVIEW_LIMIT);
    }

    #[tokio::test]
    async fn records_and_redacts_stderr() {
        let preview: StderrPreview = Arc::new(Mutex::new(String::new()));
        record_stderr_line(&preview, "test", "boom token=abc123").await;
        let context = stderr_context(&preview).await;
        assert!(context.contains("token=[REDACTED]"));
        assert!(!context.contains("abc123"));
    }
}
