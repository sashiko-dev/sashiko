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

//! AI provider that shells out to the `codex` CLI (OpenAI Codex/GPT).
//! Uses the local Codex CLI installation with subscription auth.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::ai::{AiProvider, AiRequest, AiResponse, AiUsage, ProviderCapabilities};
use super::claude_cli::{build_prompt, parse_inner_response};

pub struct CodexCliProvider {
    pub model: String,
}

#[async_trait]
impl AiProvider for CodexCliProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let prompt = build_prompt(&request);

        debug!("codex-cli prompt length: {} chars", prompt.len());

        // codex exec --json --sandbox read-only -m MODEL
        // Prompt is passed via stdin to avoid ARG_MAX issues with large prompts.
        let mut child = Command::new("codex")
            .args([
                "exec",
                "--json",
                "--sandbox", "read-only",
                "-m", &self.model,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn codex CLI: {}. Is it installed?", e))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.flush().await?;
        }

        let output = timeout(Duration::from_secs(600), child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("codex CLI timed out after 10 minutes"))?
            .map_err(|e| anyhow::anyhow!("codex CLI wait error: {}", e))?;

        if !output.stderr.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            for line in stderr.lines() {
                if !line.trim().is_empty() {
                    debug!("[codex-cli stderr] {}", line);
                }
            }
        }

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("codex CLI exited with {}: {}", output.status, stderr.trim());
        }

        let raw = String::from_utf8_lossy(&output.stdout);

        // Codex outputs line-delimited JSON events:
        //   {"type": "item.completed", "item": {"text": "..."}}
        //   {"type": "turn.completed", "usage": {"input_tokens": N, "output_tokens": N}}
        let mut text_parts = Vec::new();
        let mut usage: Option<AiUsage> = None;

        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<Value>(trimmed) {
                match event["type"].as_str() {
                    Some("item.completed") => {
                        if let Some(text) = event["item"]["text"].as_str() {
                            text_parts.push(text.to_string());
                        }
                    }
                    Some("turn.completed") => {
                        let u = &event["usage"];
                        if !u.is_null() {
                            let input = u["input_tokens"].as_u64().unwrap_or(0) as usize;
                            let output_tokens = u["output_tokens"].as_u64().unwrap_or(0) as usize;
                            let cached = u["cached_input_tokens"].as_u64().unwrap_or(0) as usize;
                            usage = Some(AiUsage {
                                prompt_tokens: input,
                                completion_tokens: output_tokens,
                                total_tokens: input + output_tokens,
                                cached_tokens: if cached > 0 { Some(cached) } else { None },
                            });
                        }
                    }
                    _ => {}
                }
            }
        }

        let response_text = text_parts.join("\n");
        if response_text.is_empty() {
            // Fall back to raw output if no events parsed
            warn!("codex-cli: no item.completed events found, using raw output");
            return parse_inner_response(&raw, usage);
        }

        parse_inner_response(&response_text, usage)
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
