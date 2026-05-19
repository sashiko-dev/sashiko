// Copyright 2026 Western Digital
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

//! AI provider that shells out to the `devin` CLI ("Devin for Terminal").
//! This uses the local Devin installation (subscription auth) rather than
//! calling an API directly.
//!
//! ## Safety
//!
//! For a strictly text-only backend, point `agent_config` at a JSON/YAML
//! file that disables all tools, or `config` at a Devin config file with a
//! deny-all permissions block. By default we pass neither and rely on the
//! `auto` permission mode plus our prompt instructing the model to reply
//! with JSON only.

use anyhow::Result;
use async_trait::async_trait;
use std::process::Stdio;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

use super::claude_cli::{build_prompt, parse_inner_response};
use crate::ai::{AiProvider, AiRequest, AiResponse, ProviderCapabilities};

pub struct DevinCliProvider {
    pub model: Option<String>,
    pub agent_config: Option<String>,
    pub config: Option<String>,
}

/// Returns the argv slice passed to `devin` (excluding the prompt, which is
/// passed via a temp file referenced by `--prompt-file`). Extracted so tests
/// can assert on the exact flag set.
pub fn build_devin_args(
    prompt_file: &str,
    model: Option<&str>,
    agent_config: Option<&str>,
    config: Option<&str>,
) -> Vec<String> {
    let mut args = vec![
        "--print".to_string(),
        "--prompt-file".to_string(),
        prompt_file.to_string(),
    ];

    if let Some(m) = model {
        args.push("--model".to_string());
        args.push(m.to_string());
    }

    if let Some(ac) = agent_config {
        args.push("--agent-config".to_string());
        args.push(ac.to_string());
    }

    if let Some(c) = config {
        args.push("--config".to_string());
        args.push(c.to_string());
    }

    args
}

#[async_trait]
impl AiProvider for DevinCliProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let prompt = build_prompt(&request);

        debug!("devin-cli prompt length: {} chars", prompt.len());

        let prompt_file = tokio::task::spawn_blocking({
            let prompt = prompt.clone();
            move || -> Result<NamedTempFile> {
                use std::io::Write;
                let mut f = tempfile::Builder::new()
                    .prefix("sashiko-devin-")
                    .suffix(".txt")
                    .tempfile()?;
                f.write_all(prompt.as_bytes())?;
                f.flush()?;
                Ok(f)
            }
        })
        .await
        .map_err(|e| anyhow::anyhow!("failed to write devin prompt file: {}", e))??;

        let args = build_devin_args(
            &prompt_file.path().to_string_lossy(),
            self.model.as_deref(),
            self.agent_config.as_deref(),
            self.config.as_deref(),
        );

        let child = Command::new("devin")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn devin CLI: {}. Is it installed?", e))?;

        // 10-minute timeout per CLI call — a hung devin process won't block forever.
        let output = timeout(Duration::from_secs(600), child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("devin CLI timed out after 10 minutes"))?
            .map_err(|e| anyhow::anyhow!("devin CLI wait error: {}", e))?;

        // Drop the temp file now that the child has exited.
        drop(prompt_file);

        if !output.stderr.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            for line in stderr.lines() {
                if !line.trim().is_empty() {
                    debug!("[devin-cli stderr] {}", line);
                }
            }
        }

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("devin CLI exited with {}: {}", output.status, stderr.trim());
        }

        let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();

        if raw.is_empty() {
            warn!("devin-cli produced empty stdout");
        }

        parse_inner_response(&raw, None)
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
            model_name: self.model.clone().unwrap_or_else(|| "default".to_string()),
            context_window_size: 200_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_args_minimal() {
        let args = build_devin_args("/tmp/p.txt", None, None, None);
        assert_eq!(args, vec!["--print", "--prompt-file", "/tmp/p.txt",]);
    }

    #[test]
    fn test_build_args_with_model_and_overrides() {
        let args = build_devin_args(
            "/tmp/p.txt",
            Some("opus"),
            Some("/etc/agent.json"),
            Some("/etc/devin.json"),
        );
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"opus".to_string()));
        assert!(args.contains(&"--agent-config".to_string()));
        assert!(args.contains(&"/etc/agent.json".to_string()));
        assert!(args.contains(&"--config".to_string()));
        assert!(args.contains(&"/etc/devin.json".to_string()));
    }

    #[test]
    fn test_prompt_never_in_argv() {
        let args = build_devin_args("/tmp/p.txt", Some("opus"), None, None);
        assert!(args.contains(&"--prompt-file".to_string()));
        assert!(!args.iter().any(|a| a == "-p" || a == "--print=prompt"));
    }
}
