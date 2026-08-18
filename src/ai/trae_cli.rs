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

fn command_args(model: &str) -> Vec<String> {
    [
        "exec",
        "--json",
        "--skip-git-repo-check",
        "--ignore-user-config",
        "--ephemeral",
        "--disallowed-tool",
        "unified_exec",
        "--disallowed-tool",
        "exec_command",
        "--disallowed-tool",
        "apply_patch",
        "--disallowed-tool",
        "web_search",
        "--disallowed-tool",
        "update_plan",
        "--sandbox",
        "read-only",
        "-m",
        model,
        "-",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn parse_events(raw: &str) -> (String, Option<AiUsage>) {
    let mut text_parts = Vec::new();
    let mut usage = None;

    for line in raw.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
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
                    let output = usage_value["output_tokens"].as_u64().unwrap_or(0) as usize;
                    let cached = usage_value["cached_input_tokens"].as_u64().unwrap_or(0) as usize;
                    usage = Some(AiUsage {
                        prompt_tokens: input,
                        completion_tokens: output,
                        total_tokens: input + output,
                        cached_tokens: (cached > 0).then_some(cached),
                    });
                }
            }
            _ => {}
        }
    }

    (text_parts.join("\n"), usage)
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

        let workspace = tempfile::tempdir()?;
        let mut child = Command::new("traecli")
            .args(command_args(&self.model))
            .current_dir(workspace.path())
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
        let (response_text, usage) = parse_events(&raw);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_disables_native_execution_tools_and_user_configuration() {
        let args = command_args("test-model");

        assert_eq!(
            args,
            vec![
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--ignore-user-config",
                "--ephemeral",
                "--disallowed-tool",
                "unified_exec",
                "--disallowed-tool",
                "exec_command",
                "--disallowed-tool",
                "apply_patch",
                "--disallowed-tool",
                "web_search",
                "--disallowed-tool",
                "update_plan",
                "--sandbox",
                "read-only",
                "-m",
                "test-model",
                "-",
            ]
        );
    }

    #[test]
    fn parses_text_and_usage_from_jsonl_events() {
        let raw = concat!(
            "{\"type\":\"thread.started\"}\n",
            "{\"type\":\"item.completed\",\"item\":{\"text\":\"first\"}}\n",
            "not-json\n",
            "{\"type\":\"item.completed\",\"item\":{\"text\":\"second\"}}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":10,",
            "\"output_tokens\":4,\"cached_input_tokens\":3}}\n"
        );

        let (text, usage) = parse_events(raw);
        let usage = usage.expect("usage should be present");

        assert_eq!(text, "first\nsecond");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 4);
        assert_eq!(usage.total_tokens, 14);
        assert_eq!(usage.cached_tokens, Some(3));
    }

    #[test]
    fn ignores_events_without_completed_text() {
        let (text, usage) =
            parse_events("{\"type\":\"item.started\",\"item\":{\"text\":\"partial\"}}\n");

        assert!(text.is_empty());
        assert!(usage.is_none());
    }
}
