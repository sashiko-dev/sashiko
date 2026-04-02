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

//! AI provider that shells out to the `gemini` CLI (Google Gemini).
//! Uses the local Gemini CLI installation with subscription auth.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::ai::{AiProvider, AiRequest, AiResponse, AiUsage, ProviderCapabilities};
use super::claude_cli::{build_prompt, parse_inner_response};

pub struct GeminiCliProvider {
    pub model: String,
}

#[async_trait]
impl AiProvider for GeminiCliProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let prompt = build_prompt(&request);

        debug!("gemini-cli prompt length: {} chars", prompt.len());

        // gemini -p "PROMPT" -o json --sandbox -m MODEL
        // --sandbox runs in a Docker container with read-only filesystem,
        // preventing Gemini's built-in tools from modifying anything.
        // Sashiko provides its own tools via the prompt text — we don't
        // want Gemini's CLI tools active. Requires Docker.
        let child = Command::new("gemini")
            .args([
                "-p", &prompt,
                "-o", "json",
                "--sandbox",
                "-m", &self.model,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn gemini CLI: {}. Is it installed?", e))?;

        let output = timeout(Duration::from_secs(600), child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("gemini CLI timed out after 10 minutes"))?
            .map_err(|e| anyhow::anyhow!("gemini CLI wait error: {}", e))?;

        if !output.stderr.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            for line in stderr.lines() {
                if !line.trim().is_empty() {
                    debug!("[gemini-cli stderr] {}", line);
                }
            }
        }

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gemini CLI exited with {}: {}", output.status, stderr.trim());
        }

        let raw = String::from_utf8_lossy(&output.stdout);

        // Gemini JSON output: may have non-JSON preamble, then a JSON object with:
        //   {"response": "...", "stats": {"models": {"model-name": {"tokens": {"input": N, "candidates": N}}}}}
        let json_str = find_json_object(&raw);
        let data: Value = serde_json::from_str(&json_str)
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to parse gemini CLI JSON output: {}\nRaw: {}",
                    e,
                    &raw[..raw.len().min(300)]
                )
            })?;

        let response_text = data["response"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();

        // Extract token usage from stats.models
        let usage = parse_gemini_usage(&data);

        if response_text.is_empty() {
            warn!("gemini-cli: empty response field");
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
            context_window_size: 1_000_000,
        }
    }
}

/// Extract token usage from Gemini CLI JSON stats.
/// Format: {"stats": {"models": {"model-name": {"tokens": {"input": N, "candidates": N, "thoughts": N}}}}}
/// Or:     {"stats": {"models": {"model-name": {"input_tokens": N, "output_tokens": N}}}}
fn parse_gemini_usage(data: &Value) -> Option<AiUsage> {
    let models = data["stats"]["models"].as_object()?;

    let mut total_in: usize = 0;
    let mut total_out: usize = 0;

    for (_name, model_data) in models {
        // Try nested "tokens" format first
        if let Some(tokens) = model_data["tokens"].as_object() {
            total_in += tokens["input"].as_u64().unwrap_or(0) as usize;
            total_out += tokens["candidates"].as_u64().unwrap_or(0) as usize;
        } else {
            // Flat format
            total_in += model_data["input_tokens"].as_u64()
                .or_else(|| model_data["input"].as_u64())
                .unwrap_or(0) as usize;
            total_out += model_data["output_tokens"].as_u64().unwrap_or(0) as usize;
        }
    }

    if total_in == 0 && total_out == 0 {
        return None;
    }

    Some(AiUsage {
        prompt_tokens: total_in,
        completion_tokens: total_out,
        total_tokens: total_in + total_out,
        cached_tokens: None,
    })
}

/// Find the first complete JSON object in text that may have non-JSON preamble.
fn find_json_object(text: &str) -> String {
    // Look for the first '{' and find its matching '}'
    if let Some(start) = text.find('{') {
        let bytes = text.as_bytes();
        let mut depth = 0;
        let mut in_string = false;
        let mut escape = false;

        for i in start..bytes.len() {
            let ch = bytes[i] as char;
            if escape {
                escape = false;
                continue;
            }
            if ch == '\\' && in_string {
                escape = true;
                continue;
            }
            if ch == '"' {
                in_string = !in_string;
                continue;
            }
            if in_string {
                continue;
            }
            if ch == '{' {
                depth += 1;
            } else if ch == '}' {
                depth -= 1;
                if depth == 0 {
                    return text[start..=i].to_string();
                }
            }
        }
    }
    text.trim().to_string()
}
