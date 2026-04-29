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

//! AI provider that shells out to the `claude` CLI instead of calling the API directly.
//! This uses the local Claude Code installation (subscription auth) rather than API credits.
//!
//! ## Safety
//!
//! The `claude --print` flag runs in text-completion mode: no tools, no file
//! access, no session persistence, no network calls. The CLI reads a prompt
//! from stdin and writes a response to stdout — it cannot modify the
//! filesystem or execute commands. This makes it inherently safe for use as
//! a completion backend without any additional sandboxing.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::ai::{
    AiProvider, AiRequest, AiResponse, AiRole, AiUsage, ProviderCapabilities, ToolCall,
};

pub struct ClaudeCliProvider {
    pub model: String,
}

#[async_trait]
impl AiProvider for ClaudeCliProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let prompt = build_prompt(&request);

        debug!("claude-cli prompt length: {} chars", prompt.len());

        let mut args = vec![
            "--print".to_string(),
            "--output-format".to_string(),
            "json".to_string(),
            "--no-session-persistence".to_string(),
        ];

        args.push("--model".to_string());
        args.push(self.model.clone());

        let mut child = Command::new("claude")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn claude CLI: {}. Is it installed?", e))?;

        // Write prompt to stdin then close it
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.flush().await?;
        }

        // 10-minute timeout per CLI call — a hung claude process won't block forever
        let output = timeout(Duration::from_secs(600), child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("claude CLI timed out after 10 minutes"))?
            .map_err(|e| anyhow::anyhow!("claude CLI wait error: {}", e))?;

        if !output.stderr.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            for line in stderr.lines() {
                if !line.trim().is_empty() {
                    debug!("[claude-cli stderr] {}", line);
                }
            }
        }

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "claude CLI exited with {}: {}",
                output.status,
                stderr.trim()
            );
        }

        let raw = String::from_utf8_lossy(&output.stdout);
        let outer: Value = serde_json::from_str(&raw).map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse claude CLI JSON output: {}\nRaw: {}",
                e,
                &raw[..raw.len().min(200)]
            )
        })?;

        if outer["is_error"].as_bool().unwrap_or(false) {
            anyhow::bail!(
                "claude CLI returned error: {}",
                outer["result"].as_str().unwrap_or("unknown error")
            );
        }

        let result_text = outer["result"].as_str().unwrap_or("").trim().to_string();

        // Parse usage from the outer JSON
        let usage = parse_usage(&outer);

        // Parse the inner response — tool calls or content
        parse_inner_response(&result_text, usage)
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

/// Build the full text prompt from the AiRequest.
/// Embeds system prompt, conversation history, tool definitions, and instructions.
pub fn build_prompt(request: &AiRequest) -> String {
    let mut out = String::new();

    // System prompt
    if let Some(sys) = &request.system {
        out.push_str("<system>\n");
        out.push_str(sys);
        out.push_str("\n</system>\n\n");
    }

    // Conversation history
    for msg in &request.messages {
        match &msg.role {
            AiRole::System => {
                // Already handled above; skip embedded system messages
            }
            AiRole::User => {
                out.push_str("<user>\n");
                if let Some(c) = &msg.content {
                    out.push_str(c);
                }
                out.push_str("\n</user>\n\n");
            }
            AiRole::Assistant => {
                out.push_str("<assistant>\n");
                if let Some(c) = &msg.content {
                    out.push_str(c);
                }
                if let Some(calls) = &msg.tool_calls {
                    for call in calls {
                        out.push_str(&format!(
                            "<tool_call id=\"{}\" name=\"{}\">\n{}\n</tool_call>\n",
                            call.id, call.function_name, call.arguments
                        ));
                    }
                }
                out.push_str("</assistant>\n\n");
            }
            AiRole::Tool => {
                let id = msg.tool_call_id.as_deref().unwrap_or("?");
                out.push_str(&format!("<tool_result id=\"{}\">\n", id));
                if let Some(c) = &msg.content {
                    out.push_str(c);
                }
                out.push_str("\n</tool_result>\n\n");
            }
        }
    }

    // Tool definitions and response instructions
    if let Some(tools) = &request.tools
        && !tools.is_empty()
    {
        out.push_str("<available_tools>\n");
        for tool in tools {
            out.push_str(&format!(
                "- name: {}\n  description: {}\n  parameters: {}\n\n",
                tool.name, tool.description, tool.parameters
            ));
        }
        out.push_str("</available_tools>\n\n");
        out.push_str(
            "RESPONSE FORMAT: You MUST respond with a SINGLE valid JSON object only (no markdown, no explanation).\n\
             To call tools: {\"tool_calls\": [{\"id\": \"c1\", \"function_name\": \"TOOL_NAME\", \"arguments\": {ARGS}}, {\"id\": \"c2\", \"function_name\": \"OTHER_TOOL\", \"arguments\": {ARGS2}}]}\n\
             Put ALL tool calls in ONE tool_calls array. Do NOT output multiple JSON objects.\n\
             For your final answer: {\"content\": \"YOUR RESPONSE\"}\n\
             Do not mix both. Output exactly one JSON object.\n",
        );
    }

    out
}

fn parse_usage(outer: &Value) -> Option<AiUsage> {
    let u = &outer["usage"];
    if u.is_null() {
        return None;
    }
    let input = u["input_tokens"].as_u64().unwrap_or(0) as usize;
    let output = u["output_tokens"].as_u64().unwrap_or(0) as usize;
    let cached = u["cache_read_input_tokens"].as_u64().unwrap_or(0) as usize;
    Some(AiUsage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: input + output,
        cached_tokens: Some(cached),
    })
}

pub fn parse_inner_response(text: &str, usage: Option<AiUsage>) -> Result<AiResponse> {
    // Try extracting JSON (might be in a markdown code block)
    let json_str = extract_json(text);

    if let Ok(v) = serde_json::from_str::<Value>(&json_str) {
        return parse_single_json(&v, &json_str, usage);
    }

    // Try JSONL: multiple JSON objects on separate lines (model sometimes emits
    // separate tool_calls objects per line instead of one combined object)
    let mut merged_tool_calls: Vec<ToolCall> = Vec::new();
    let mut had_json = false;
    for line in json_str.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            had_json = true;
            if let Some(calls) = v["tool_calls"].as_array() {
                for c in calls {
                    if let Some(tc) = parse_tool_call(c) {
                        merged_tool_calls.push(tc);
                    }
                }
            }
        }
    }

    if !merged_tool_calls.is_empty() {
        debug!(
            "claude-cli: merged {} tool calls from JSONL response",
            merged_tool_calls.len()
        );
        return Ok(AiResponse {
            content: None,
            thought: None,
            thought_signature: None,
            tool_calls: Some(merged_tool_calls),
            usage,
        });
    }

    if had_json {
        // Had valid JSON lines but no tool calls — return original text as content
        // (json_str from extract_json may be mangled if text had multiple objects)
        return Ok(AiResponse {
            content: Some(text.to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            usage,
        });
    }

    // Not parseable as JSON — return raw text
    warn!("claude-cli response not valid JSON, returning as raw content");
    Ok(AiResponse {
        content: Some(text.to_string()),
        thought: None,
        thought_signature: None,
        tool_calls: None,
        usage,
    })
}

fn parse_tool_call(c: &Value) -> Option<ToolCall> {
    let id = c["id"].as_str().unwrap_or("c1").to_string();
    let name = c["function_name"].as_str()?.to_string();
    let args = c["arguments"].clone();
    Some(ToolCall {
        id,
        function_name: name,
        arguments: args,
        thought_signature: None,
    })
}

fn parse_single_json(v: &Value, json_str: &str, usage: Option<AiUsage>) -> Result<AiResponse> {
    // Tool calls?
    if let Some(calls) = v["tool_calls"].as_array() {
        let tool_calls: Vec<ToolCall> = calls.iter().filter_map(parse_tool_call).collect();

        if !tool_calls.is_empty() {
            return Ok(AiResponse {
                content: None,
                thought: None,
                thought_signature: None,
                tool_calls: Some(tool_calls),
                usage,
            });
        }
    }

    // Content field?
    if let Some(content) = v["content"].as_str() {
        return Ok(AiResponse {
            content: Some(content.to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            usage,
        });
    }

    // Any other JSON — return it as content string (e.g. {"concerns": [...]})
    Ok(AiResponse {
        content: Some(json_str.to_string()),
        thought: None,
        thought_signature: None,
        tool_calls: None,
        usage,
    })
}

/// Extract JSON from text that may be wrapped in markdown fences.
/// Returns the content inside the first fenced block, or the original text trimmed.
/// Does NOT try to find outermost braces — that can silently produce invalid JSON
/// when the text contains multiple objects (e.g. JSONL), which the JSONL fallback
/// in parse_inner_response handles better.
fn extract_json(text: &str) -> String {
    // Strip markdown fences — handle both LF and CRLF, and optional language tag
    let normalized = text.replace("\r\n", "\n");
    for fence_start in &["```json\n", "```JSON\n", "```\n"] {
        if let Some(start) = normalized.find(fence_start) {
            let after = &normalized[start + fence_start.len()..];
            if let Some(end) = after.find("\n```") {
                return after[..end].trim().to_string();
            }
        }
    }
    normalized.trim().to_string()
}
