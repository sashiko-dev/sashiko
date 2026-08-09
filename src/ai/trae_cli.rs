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

//! AI provider that shells out to `traecli exec --json`.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

use super::claude_cli::{build_prompt, parse_inner_response};
use crate::ai::{AiProvider, AiRequest, AiResponse, AiUsage, ProviderCapabilities};

pub struct TraeCliProvider {
    pub model: String,
}

#[async_trait]
impl AiProvider for TraeCliProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let mut prompt = build_prompt(&request);
        if request
            .tools
            .as_ref()
            .is_some_and(|tools| !tools.is_empty())
        {
            prompt.push_str(
                "\nThe `tool_calls` JSON above is an application-level text protocol. \
                 Return it as ordinary response text when a tool is needed; Sashiko will \
                 execute it. Do not use TRAE CLI native tools.\n",
            );
        }

        debug!("traecli prompt length: {} chars", prompt.len());

        let mut child = Command::new("traecli")
            .args([
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--ignore-user-config",
                "--ephemeral",
                "--allowed-tool",
                "__sashiko_no_native_tools__",
                "--sandbox",
                "read-only",
                "-m",
                &self.model,
                "-",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                anyhow::anyhow!("Failed to spawn traecli: {}. Is it installed?", error)
            })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.flush().await?;
        }

        let output = timeout(Duration::from_secs(600), child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("traecli timed out after 10 minutes"))?
            .map_err(|error| anyhow::anyhow!("traecli wait error: {}", error))?;

        if !output.stderr.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            for line in stderr.lines() {
                if !line.trim().is_empty() {
                    debug!("[traecli stderr] {}", line);
                }
            }
        }

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("traecli exited with {}: {}", output.status, stderr.trim());
        }

        let raw = String::from_utf8_lossy(&output.stdout);
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
                        let usage_value = &event["usage"];
                        if !usage_value.is_null() {
                            let input = usage_value["input_tokens"].as_u64().unwrap_or(0) as usize;
                            let output_tokens =
                                usage_value["output_tokens"].as_u64().unwrap_or(0) as usize;
                            let cached =
                                usage_value["cached_input_tokens"].as_u64().unwrap_or(0) as usize;
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
            warn!("traecli: no item.completed text events found, using raw output");
            return parse_inner_response(&raw, usage);
        }

        parse_inner_response(&response_text, usage)
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        let chars: usize = request
            .messages
            .iter()
            .filter_map(|message| message.content.as_ref())
            .map(|content| content.len())
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
