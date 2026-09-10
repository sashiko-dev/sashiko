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

use super::claude_cli::{build_prompt, parse_inner_response};
use crate::ai::{
    AiProvider, AiRequest, AiResponse, AiUsage, ProviderCapabilities, cache_identity_with,
};

pub struct CodexCliProvider {
    pub model: String,
    pub effort: Option<String>,
}

#[async_trait]
impl AiProvider for CodexCliProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let prompt = build_prompt(&request);

        debug!("codex-cli prompt length: {} chars", prompt.len());

        // codex exec --json --sandbox read-only -m MODEL \
        //     [-c model_reasoning_effort=EFFORT]
        // Prompt is passed via stdin to avoid ARG_MAX issues with large prompts.
        let mut args = vec![
            "exec".to_string(),
            "--json".to_string(),
            "--sandbox".to_string(),
            "read-only".to_string(),
            "-m".to_string(),
            self.model.clone(),
        ];

        // A `-c` override outranks ~/.codex/config.toml. It does not outrank
        // an enterprise-managed requirements layer, which overrides a
        // configured value whatever its origin and reports the substitution as
        // an error event.
        if let Some(effort) = &self.effort {
            args.push("-c".to_string());
            args.push(format!("model_reasoning_effort={effort}"));
        }

        let mut child = Command::new("codex")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
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
            let stdout = String::from_utf8_lossy(&output.stdout);
            // codex reports why it rejected an invocation -- an unsupported
            // model_reasoning_effort value, say -- as a JSON error event on
            // stdout, while stderr carries only progress chatter. Either one
            // can hold the cause the other is missing: a per-turn error event
            // on stdout does not explain a login failure printed on stderr, so
            // report both.
            let mut detail = collect_error_messages(&stdout);
            let stderr = stderr.trim();
            if !stderr.is_empty() {
                detail.push(stderr.to_string());
            }
            anyhow::bail!(
                "codex CLI exited with {}: {}",
                output.status,
                detail.join("; ")
            );
        }

        let raw = String::from_utf8_lossy(&output.stdout);

        // Codex outputs line-delimited JSON events:
        //   {"type": "item.completed", "item": {"text": "..."}}
        //   {"type": "turn.completed", "usage": {"input_tokens": N, "output_tokens": N}}
        //   {"type": "error", "message": "..."}
        let mut text_parts = Vec::new();
        let mut usage: Option<AiUsage> = None;

        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<Value>(trimmed) {
                if let Some(message) = error_message(&event) {
                    self.check_error_message(message)?;
                }
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

    fn cache_identity(&self) -> String {
        cache_identity_with(&self.model, &[("effort", self.effort.as_deref())])
    }
}

impl CodexCliProvider {
    /// Handles an error event from a run that exited 0. A requirements layer
    /// that substitutes its own value for a setting reports the substitution
    /// this way and lets the run proceed.
    fn check_error_message(&self, message: &str) -> Result<()> {
        if is_approval_policy_substitution(message) {
            debug!("codex-cli: {}", message);
        } else if self.effort.is_some() && is_effort_substitution(message) {
            // The response cache files the reply under the configured effort,
            // so a reply produced at the substituted one must not be returned.
            anyhow::bail!("codex CLI did not run at the configured effort: {message}");
        } else {
            warn!("codex-cli: {}", message);
        }
        Ok(())
    }
}

/// Returns the explanation an error event carries. A rejected invocation
/// surfaces either as a bare `{"type":"error"}` line or as an
/// `item.completed` wrapping an error item; both carry it in `message`.
fn error_message(event: &Value) -> Option<&str> {
    [event, &event["item"]]
        .into_iter()
        .find(|candidate| candidate["type"] == "error")
        .and_then(|candidate| candidate["message"].as_str())
}

/// Pulls the distinct error messages out of a run's stdout.
fn collect_error_messages(raw: &str) -> Vec<String> {
    let mut messages = Vec::new();
    for line in raw.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if let Some(message) = error_message(&event)
            && !messages.iter().any(|m| m == message)
        {
            messages.push(message.to_string());
        }
    }
    messages
}

/// Recognizes the one substitution that carries nothing to act on. `codex
/// exec` resolves `approval_policy` to `Never` itself, a non-interactive run
/// having nobody to prompt, so a requirements layer that disallows `Never`
/// substitutes its own value on every invocation. Nothing here configures the
/// policy and a `-c` override does not reach the check, so the message names
/// no setting the operator can change. The sandbox stays read-only whichever
/// policy wins, so an escalation request is refused under the substituted
/// value exactly as it was under `Never`.
fn is_approval_policy_substitution(message: &str) -> bool {
    message.contains("`approval_policy`")
}

/// Recognizes a substitution of the effort this provider passed on the
/// command line.
fn is_effort_substitution(message: &str) -> bool {
    message.contains("`model_reasoning_effort`")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_approval_policy_substitution_is_demoted() {
        assert!(is_approval_policy_substitution(
            "Configured value for `approval_policy` is disallowed by \
             requirements; falling back to required value OnRequest."
        ));
        assert!(!is_approval_policy_substitution(
            "Configured value for `model_reasoning_effort` is disallowed by \
             requirements; falling back to required value medium."
        ));
    }

    #[test]
    fn a_substituted_effort_fails_the_run() {
        let message = "Configured value for `model_reasoning_effort` is disallowed by \
                       requirements; falling back to required value medium.";
        let raised = CodexCliProvider {
            model: "gpt-5-codex".to_string(),
            effort: Some("xhigh".to_string()),
        };
        assert!(raised.check_error_message(message).is_err());

        // With no effort configured the identity carries none, so the
        // substituted run is recorded under the identity it ran at.
        let plain = CodexCliProvider {
            model: "gpt-5-codex".to_string(),
            effort: None,
        };
        assert!(plain.check_error_message(message).is_ok());
        assert!(
            raised
                .check_error_message(
                    "Configured value for `approval_policy` is disallowed by \
                     requirements; falling back to required value OnRequest."
                )
                .is_ok()
        );
    }

    #[test]
    fn error_messages_come_from_both_event_shapes() {
        let raw = concat!(
            r#"{"type":"session.created"}"#,
            "\n",
            r#"{"type":"error","message":"[reasoning.effort] Invalid value: 'higest'"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"type":"error","message":"disallowed by requirements"}}"#,
            "\n",
            r#"not json"#,
            "\n",
        );
        assert_eq!(
            collect_error_messages(raw),
            vec![
                "[reasoning.effort] Invalid value: 'higest'".to_string(),
                "disallowed by requirements".to_string(),
            ]
        );
    }

    #[test]
    fn cache_identity_tracks_effort() {
        let plain = CodexCliProvider {
            model: "gpt-5-codex".to_string(),
            effort: None,
        };
        let raised = CodexCliProvider {
            model: "gpt-5-codex".to_string(),
            effort: Some("xhigh".to_string()),
        };
        assert_ne!(plain.cache_identity(), raised.cache_identity());
    }
}
