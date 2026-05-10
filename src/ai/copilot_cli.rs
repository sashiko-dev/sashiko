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

//! AI provider that shells out to the `copilot` CLI (GitHub Copilot).
//! Uses the local GitHub Copilot CLI installation with subscription auth.
//!
//! ## Safety
//!
//! Unlike `claude --print`, the `copilot -p` flag runs in non-interactive
//! mode but retains tool access. To minimize side effects we pass
//! `--disable-builtin-mcps` (no MCP servers) and `--no-custom-instructions`
//! (ignores local AGENTS.md). The provider is used as a text-completion
//! backend only.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

use super::claude_cli::{build_prompt, parse_inner_response};
use crate::ai::{AiProvider, AiRequest, AiResponse, AiUsage, ProviderCapabilities};

pub struct CopilotCliProvider {
    pub model: String,
}

#[async_trait]
impl AiProvider for CopilotCliProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let prompt = build_prompt(&request);

        debug!("copilot-cli prompt length: {} chars", prompt.len());

        let child = Command::new("copilot")
            .args([
                "-p",
                &prompt,
                "--output-format",
                "json",
                "-s",
                "--model",
                &self.model,
                "--disable-builtin-mcps",
                "--no-custom-instructions",
                "--allow-all-tools",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn copilot CLI: {}. Is it installed?", e))?;

        let output = timeout(Duration::from_secs(600), child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("copilot CLI timed out after 10 minutes"))?
            .map_err(|e| anyhow::anyhow!("copilot CLI wait error: {}", e))?;

        if !output.stderr.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            for line in stderr.lines() {
                if !line.trim().is_empty() {
                    debug!("[copilot-cli stderr] {}", line);
                }
            }
        }

        let raw = String::from_utf8_lossy(&output.stdout);

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "copilot CLI exited with {}: {}",
                output.status,
                stderr.trim()
            );
        }

        parse_jsonl_events(&raw)
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        let chars: usize = request
            .messages
            .iter()
            .filter_map(|m| m.content.as_ref())
            .map(|c| c.len())
            .sum();
        chars / 4
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: 200_000,
        }
    }
}

/// Parse the copilot CLI JSONL event stream.
///
/// The copilot CLI emits one JSON object per line. The relevant events are:
/// - `assistant.message`: contains `data.content` with the response text and
///   `data.outputTokens` with the output token count.
/// - `result`: contains `data.usage` with session-level usage statistics.
///
/// All other event types (deltas, MCP server status, etc.) are ignored.
pub fn parse_jsonl_events(raw: &str) -> Result<AiResponse> {
    let mut content: Option<String> = None;
    let mut usage: Option<AiUsage> = None;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let event: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        match event["type"].as_str() {
            Some("assistant.message") => {
                if let Some(text) = event["data"]["content"].as_str() {
                    content = Some(text.to_string());
                }
                // Per-message output tokens
                if usage.is_none()
                    && let Some(out_tokens) = event["data"]["outputTokens"].as_u64()
                {
                    usage = Some(AiUsage {
                        prompt_tokens: 0,
                        completion_tokens: out_tokens as usize,
                        total_tokens: out_tokens as usize,
                        cached_tokens: None,
                    });
                }
            }
            Some("result") => {
                let u = &event["data"]["usage"];
                if !u.is_null() {
                    // The result event has session-level metrics but no
                    // per-request token breakdown. Use premiumRequests as a
                    // rough indicator if present.
                    let api_duration_ms = u["totalApiDurationMs"].as_u64().unwrap_or(0);
                    debug!("copilot-cli session api duration: {}ms", api_duration_ms);
                }
                if let Some(exit_code) = event["data"]["exitCode"].as_i64()
                    && exit_code != 0
                {
                    warn!("copilot-cli session exited with code {}", exit_code);
                }
            }
            _ => {}
        }
    }

    // If we captured structured content, try to parse it for tool calls
    if let Some(text) = &content {
        return parse_inner_response(text, usage);
    }

    // No assistant.message events found — return empty
    warn!("copilot-cli: no assistant.message events in output");
    Ok(AiResponse {
        content: None,
        thought: None,
        thought_signature: None,
        tool_calls: None,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_simple_response() {
        let events = [
            json!({"type": "assistant.message", "data": {"content": "2 + 2 = 4", "outputTokens": 10}}),
            json!({"type": "assistant.turn_end", "data": {"turnId": "0"}}),
            json!({"type": "result", "data": {"exitCode": 0, "usage": {"premiumRequests": 1, "totalApiDurationMs": 3000}}}),
        ];
        let raw: String = events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n");

        let resp = parse_jsonl_events(&raw).unwrap();
        assert_eq!(resp.content.as_deref(), Some("2 + 2 = 4"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.completion_tokens, 10);
    }

    #[test]
    fn test_parse_tool_call_response() {
        let tool_json = json!({
            "tool_calls": [{
                "id": "c1",
                "function_name": "git_log",
                "arguments": {"n": 5}
            }]
        });
        let events = [
            json!({"type": "assistant.message", "data": {"content": serde_json::to_string(&tool_json).unwrap(), "outputTokens": 25}}),
            json!({"type": "result", "data": {"exitCode": 0, "usage": {}}}),
        ];
        let raw: String = events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n");

        let resp = parse_jsonl_events(&raw).unwrap();
        assert!(resp.content.is_none());
        let calls = resp.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function_name, "git_log");
    }

    #[test]
    fn test_parse_empty_output() {
        let resp = parse_jsonl_events("").unwrap();
        assert!(resp.content.is_none());
        assert!(resp.tool_calls.is_none());
    }

    #[test]
    fn test_parse_ignores_ephemeral_events() {
        let events = [
            json!({"type": "session.mcp_server_status_changed", "data": {}, "ephemeral": true}),
            json!({"type": "assistant.message_delta", "data": {"deltaContent": "hel"}, "ephemeral": true}),
            json!({"type": "assistant.message", "data": {"content": "hello world", "outputTokens": 5}}),
            json!({"type": "result", "data": {"exitCode": 0, "usage": {}}}),
        ];
        let raw: String = events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n");

        let resp = parse_jsonl_events(&raw).unwrap();
        assert_eq!(resp.content.as_deref(), Some("hello world"));
    }

    #[test]
    fn test_parse_non_zero_exit() {
        let events = [json!({"type": "result", "data": {"exitCode": 1, "usage": {}}})];
        let raw: String = events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n");

        // Should still parse without error (the warning is logged)
        let resp = parse_jsonl_events(&raw).unwrap();
        assert!(resp.content.is_none());
    }

    #[test]
    fn test_parse_malformed_lines_skipped() {
        let raw = "not json\n\
                    {\"type\": \"assistant.message\", \"data\": {\"content\": \"ok\", \"outputTokens\": 3}}\n\
                    also not json\n";

        let resp = parse_jsonl_events(raw).unwrap();
        assert_eq!(resp.content.as_deref(), Some("ok"));
    }

    #[test]
    fn test_parse_json_content_response() {
        // When the model returns a JSON object as content (e.g. {"concerns": [...]})
        let inner = json!({"concerns": [{"title": "Memory leak", "severity": "high"}]});
        let events = [
            json!({"type": "assistant.message", "data": {"content": serde_json::to_string(&inner).unwrap(), "outputTokens": 50}}),
            json!({"type": "result", "data": {"exitCode": 0, "usage": {}}}),
        ];
        let raw: String = events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect::<Vec<_>>()
            .join("\n");

        let resp = parse_jsonl_events(&raw).unwrap();
        // parse_inner_response should return the JSON as content string
        assert!(resp.content.is_some());
        let content = resp.content.unwrap();
        assert!(content.contains("Memory leak"));
    }
}
