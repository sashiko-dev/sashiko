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
//! The prompt carries untrusted patch text; `--print` alone keeps native tools.
//! `build_claude_args` disables tools and user/project MCP, skills, hooks,
//! instructions and automatic memory. Requests run in fresh temporary directories.
//! CLI authentication and inherited environment remain available for routing.
//! Managed policy remains authoritative, including mandatory hooks and
//! instructions; managed MCP configurations can prevent strict-mode startup.
//! These restrictions disable agent functionality, not operating-system
//! access: the CLI is still trusted to read credentials and contact the model.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tracing::debug;

use super::cli_common::{
    CliIoError, IDLE_TIMEOUT, build_prompt, parse_inner_response, spawn_cli, write_prompt_and_wait,
};
use crate::ai::{
    AiErrorClass, AiProvider, AiRequest, AiResponse, AiUsage, ClassifyAiError,
    ProviderCapabilities, cache_identity_with,
};
use crate::utils::{redact_secret, utf8_prefix};

#[derive(Debug, thiserror::Error)]
pub enum ClaudeCliError {
    #[error("Failed to spawn claude CLI: {0}")]
    Spawn(String),
    #[error("Failed to create claude CLI working directory: {0}")]
    WorkingDirectory(#[source] std::io::Error),
    #[error("claude CLI produced no output for {0}s")]
    Timeout(u64),
    #[error("claude CLI wait error: {0}")]
    Wait(String),
    #[error("claude CLI stdin write failed: {0}")]
    Stdin(String),
    #[error("claude CLI error: {0}")]
    Cli(String),
    #[error("Failed to parse claude CLI JSON output: {0}")]
    Parse(String),
}

impl ClassifyAiError for ClaudeCliError {
    fn ai_error_class(&self) -> AiErrorClass {
        match self {
            ClaudeCliError::Spawn(_) => AiErrorClass::Fatal,
            ClaudeCliError::WorkingDirectory(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::InvalidInput
                        | std::io::ErrorKind::InvalidFilename
                        | std::io::ErrorKind::NotADirectory
                        | std::io::ErrorKind::ReadOnlyFilesystem
                        | std::io::ErrorKind::Unsupported
                ) =>
            {
                AiErrorClass::Fatal
            }
            // Resource pressure can clear during backoff, including OS errors
            // that std::io does not map to a more specific ErrorKind.
            ClaudeCliError::WorkingDirectory(_) => AiErrorClass::Transient {
                retry_after: Duration::from_secs(30),
            },
            ClaudeCliError::Timeout(_) => AiErrorClass::Transient {
                retry_after: Duration::from_secs(30),
            },
            ClaudeCliError::Wait(_) => AiErrorClass::Transient {
                retry_after: Duration::from_secs(30),
            },
            ClaudeCliError::Stdin(_) => AiErrorClass::Transient {
                retry_after: Duration::from_secs(30),
            },
            ClaudeCliError::Cli(_) => AiErrorClass::Fatal,
            ClaudeCliError::Parse(_) => AiErrorClass::Fatal,
        }
    }
}

pub(crate) fn log_and_redact_stderr(label: &str, stderr: &[u8]) -> String {
    if stderr.is_empty() {
        return String::new();
    }
    let redacted = redact_secret(&String::from_utf8_lossy(stderr));
    for line in redacted.lines() {
        if !line.trim().is_empty() {
            debug!("[{label} stderr] {line}");
        }
    }
    redacted.trim().to_string()
}

fn format_cli_exit_error(
    status: impl std::fmt::Display,
    raw: &str,
    stderr: &str,
) -> ClaudeCliError {
    // Try to extract the actual error message from the JSON output.
    // The CLI emits a JSON object with is_error=true and the reason
    // in the "result" field even when it exits non-zero.
    if let Ok(outer) = serde_json::from_str::<Value>(raw)
        && let Some(msg) = outer["result"].as_str()
    {
        return ClaudeCliError::Cli(redact_secret(msg));
    }
    ClaudeCliError::Cli(format!("exited with {status}: {stderr}"))
}

fn extract_cli_result(outer: &Value) -> Result<String, ClaudeCliError> {
    if outer["is_error"].as_bool().unwrap_or(false) {
        return Err(ClaudeCliError::Cli(redact_secret(
            outer["result"].as_str().unwrap_or("unknown error"),
        )));
    }
    Ok(outer["result"].as_str().unwrap_or("").trim().to_string())
}

pub struct ClaudeCliProvider {
    pub model: String,
    pub effort: Option<String>,
}

/// Returns the argv passed to `claude` (the prompt goes on stdin). Extracted
/// so tests can assert that no tool survives.
pub fn build_claude_args(model: &str, effort: Option<&str>) -> Vec<String> {
    // Isolation arguments/settings verified with Claude Code 2.1.170.
    let mut args = vec![
        "--print".to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--no-session-persistence".to_string(),
        "--tools".to_string(),
        String::new(),
        "--disallowedTools".to_string(),
        "mcp__*".to_string(),
        "--strict-mcp-config".to_string(),
        "--mcp-config".to_string(),
        r#"{"mcpServers":{}}"#.to_string(),
        "--setting-sources".to_string(),
        String::new(),
        "--disable-slash-commands".to_string(),
        "--settings".to_string(),
        serde_json::json!({
            "verbose": false,
            "disableAllHooks": true,
            "autoMemoryEnabled": false,
            "claudeMdExcludes": ["**"],
        })
        .to_string(),
        "--model".to_string(),
        model.to_string(),
    ];

    if let Some(effort) = effort {
        args.push("--effort".to_string());
        args.push(effort.to_string());
    }

    args
}

impl ClaudeCliProvider {
    async fn generate_content_with_binary(
        &self,
        request: AiRequest,
        binary: &std::ffi::OsStr,
        idle_timeout: Duration,
    ) -> Result<AiResponse> {
        let prompt = build_prompt(&request);

        debug!("claude-cli prompt length: {} chars", prompt.len());

        let working_directory = tempfile::tempdir().map_err(ClaudeCliError::WorkingDirectory)?;
        let mut command = Command::new(binary);
        command
            .args(build_claude_args(&self.model, self.effort.as_deref()))
            .current_dir(working_directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = spawn_cli(&mut command).map_err(|e| ClaudeCliError::Spawn(e.to_string()))?;

        // An idle deadline per CLI call — a hung claude process won't block
        // forever, while one that is still reporting progress runs as long as
        // the review takes. The write is inside it, so a prompt that cannot be
        // delivered is bounded the same way.
        let output = write_prompt_and_wait("claude-cli", child, prompt, idle_timeout)
            .await
            .map_err(|e| match e {
                CliIoError::Write(e) => ClaudeCliError::Stdin(e.to_string()),
                CliIoError::Read(e) => ClaudeCliError::Wait(e.to_string()),
                CliIoError::Wait(e) => ClaudeCliError::Wait(e.to_string()),
                CliIoError::Idle(idle) => ClaudeCliError::Timeout(idle.after.as_secs()),
            })?;

        let stderr = log_and_redact_stderr("claude-cli", &output.stderr);
        let raw = String::from_utf8_lossy(&output.stdout);

        if !output.status.success() {
            return Err(format_cli_exit_error(output.status, &raw, &stderr).into());
        }
        let outer = parse_cli_output(&raw)?;
        let result_text = extract_cli_result(&outer)?;

        // Parse usage from the outer JSON
        let usage = parse_usage(&outer);

        // Parse the inner response — tool calls or content
        parse_inner_response("claude-cli", &result_text, usage)
    }
}

#[async_trait]
impl AiProvider for ClaudeCliProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        self.generate_content_with_binary(request, std::ffi::OsStr::new("claude"), IDLE_TIMEOUT)
            .await
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: context_window_for_model(&self.model),
        }
    }

    fn cache_identity(&self) -> String {
        // Responses produced with ambient tools/settings must not be replayed
        // for requests using the restricted completion profile.
        cache_identity_with(
            &self.model,
            &[
                ("effort", self.effort.as_deref()),
                ("completion_profile", Some("restricted-v1")),
            ],
        )
    }
}

fn parse_cli_output(raw: &str) -> Result<Value, ClaudeCliError> {
    serde_json::from_str(raw).map_err(|e| {
        let redacted = redact_secret(raw);
        ClaudeCliError::Parse(format!("{}\nRaw: {}", e, utf8_prefix(&redacted, 200)))
    })
}

/// Pick the context window to advertise for a given model name. The value is
/// currently metadata only (no consumer gates on it), so the mapping is coarse.
/// Opus 4.7 ships with a 1M window by default via Claude Code, and any model
/// can be selected with the `[1m]` suffix to opt into the 1M variant.
/// Verified against Claude Code 2.1.132 for opus-4-7, sonnet-4-6, sonnet-4-6[1m],
/// haiku-4-5.
fn context_window_for_model(model: &str) -> usize {
    if model.contains("[1m]") || model.contains("opus-4-7") {
        1_000_000
    } else {
        200_000
    }
}

fn parse_usage(outer: &Value) -> Option<AiUsage> {
    let u = &outer["usage"];
    if u.is_null() {
        return None;
    }
    let input = u["input_tokens"].as_u64().unwrap_or(0) as usize;
    let output = u["output_tokens"].as_u64().unwrap_or(0) as usize;
    let cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0) as usize;
    let cache_write = u["cache_creation_input_tokens"].as_u64().unwrap_or(0) as usize;
    // input_tokens arrives without the cached prefix, so the two cache
    // counts fold back in here.  cached_tokens is a breakdown of
    // prompt_tokens, and a consumer subtracts it to get uncached input.
    let total_input = input + cache_read + cache_write;
    Some(AiUsage {
        prompt_tokens: total_input,
        completion_tokens: output,
        total_tokens: total_input + output,
        cached_tokens: if cache_read > 0 {
            Some(cache_read)
        } else {
            None
        },
    })
}

#[cfg(all(test, unix))]
mod process_tests;

#[cfg(test)]
mod isolation_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_error_preview_handles_multibyte_cutoff() {
        let raw = format!("{}🙂not-json", "a".repeat(199));

        let error = parse_cli_output(&raw).unwrap_err();

        assert!(matches!(&error, ClaudeCliError::Parse(_)));
        assert!(error.to_string().contains(&"a".repeat(199)));
    }

    #[test]
    fn test_claude_args_disable_every_tool() {
        let args = build_claude_args("sonnet", Some("high"));
        let tools = args.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(args[tools + 1], "");
        let denied = args.iter().position(|a| a == "--disallowedTools").unwrap();
        assert_eq!(args[denied + 1], "mcp__*");
        assert!(args.contains(&"--strict-mcp-config".to_string()));
        assert!(args.contains(&"--print".to_string()));
    }

    #[test]
    fn test_parse_usage_folds_cache_counts_into_prompt_tokens() {
        let outer = serde_json::json!({
            "usage": {
                "input_tokens": 500,
                "output_tokens": 20,
                "cache_read_input_tokens": 19500,
                "cache_creation_input_tokens": 1000,
            }
        });

        let usage = parse_usage(&outer).unwrap();

        // Uncached input is prompt_tokens less the cached breakdown, so it
        // covers the fresh input and the prefix the model had to write.
        assert_eq!(usage.prompt_tokens, 21000);
        assert_eq!(usage.cached_tokens, Some(19500));
        assert_eq!(usage.total_tokens, 21020);
    }

    #[test]
    fn test_parse_usage_without_a_cache_read_reports_no_cached_tokens() {
        let outer = serde_json::json!({
            "usage": {
                "input_tokens": 500,
                "output_tokens": 20,
            }
        });

        let usage = parse_usage(&outer).unwrap();

        assert_eq!(usage.prompt_tokens, 500);
        assert_eq!(usage.cached_tokens, None);
    }

    #[test]
    fn test_log_and_redact_stderr_strips_secrets() {
        let raw_stderr =
            b"Error connecting to https://user:pass@host/v1?key=sk-secret token=tok-123\n";
        let redacted = log_and_redact_stderr("claude-cli", raw_stderr);
        assert_eq!(
            redacted,
            "Error connecting to https://[REDACTED]:[REDACTED]@host/v1?key=[REDACTED] token=[REDACTED]"
        );
        assert!(!redacted.contains("sk-secret"));
        assert!(!redacted.contains("tok-123"));
        assert!(!redacted.contains("user:pass"));
    }

    #[test]
    fn test_cli_error_redacts_secrets_in_json_and_raw_output() {
        let err_parse =
            parse_cli_output("not-json https://user:pass@host?key=sk-secret token=tok-123")
                .unwrap_err()
                .to_string();
        assert!(err_parse.contains("https://[REDACTED]:[REDACTED]@host?key=[REDACTED]"));
        assert!(err_parse.contains("token=[REDACTED]"));
        assert!(!err_parse.contains("sk-secret"));
        assert!(!err_parse.contains("tok-123"));
        assert!(!err_parse.contains("user:pass"));

        let outer_err = json!({
            "is_error": true,
            "result": "upstream failed at https://user:pass@host with key=sk-secret and token=tok-123"
        });
        let err_result = extract_cli_result(&outer_err).unwrap_err().to_string();
        assert!(err_result.contains("https://[REDACTED]:[REDACTED]@host"));
        assert!(err_result.contains("key=[REDACTED]"));
        assert!(err_result.contains("token=[REDACTED]"));
        assert!(!err_result.contains("sk-secret"));
        assert!(!err_result.contains("tok-123"));
        assert!(!err_result.contains("user:pass"));

        let err_exit_json =
            format_cli_exit_error("exit status: 1", &outer_err.to_string(), "ignored stderr")
                .to_string();
        assert!(err_exit_json.contains("https://[REDACTED]:[REDACTED]@host"));
        assert!(err_exit_json.contains("key=[REDACTED]"));
        assert!(err_exit_json.contains("token=[REDACTED]"));
        assert!(!err_exit_json.contains("sk-secret"));
        assert!(!err_exit_json.contains("tok-123"));
        assert!(!err_exit_json.contains("user:pass"));
    }
}
