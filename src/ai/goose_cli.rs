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

//! AI provider that shells out to `goose acp`.
//!
//! [goose](https://goose-docs.ai/) is an open-source agent that speaks
//! the Agent Client Protocol on stdio and can front any of its own configured
//! backends, including a local vLLM or Ollama server. Sashiko drives it as a
//! pure completion backend.
//!
//! goose is part of the Agentic AI Foundation (AAIF) at the Linux Foundation,
//! as is Sashiko.
//!
//! Each request runs in a throwaway configuration directory whose `config.yaml`
//! pins goose to chat mode, so goose never executes a tool of its own:
//! Sashiko's ToolBox stays the only tool layer. The directory also keeps the
//! user's own goose configuration and session history out of a review.
//!
//! goose reports token usage in its `session/prompt` result, which is used
//! verbatim when present. Note that goose prepends its own system prompt and
//! platform tool schemas to every request, so its input token count is
//! noticeably higher than the prompt Sashiko sends. Budget for that when
//! setting `context_window_size`.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use tracing::debug;

use super::acp::AcpProcess;
use super::claude_cli::{build_prompt, parse_inner_response};
use super::token_budget::TokenBudget;
use crate::ai::{AiProvider, AiRequest, AiResponse, AiUsage, ProviderCapabilities};

pub struct GooseCliProvider {
    /// Model identifier passed to goose as GOOSE_MODEL.
    pub model: String,
    /// goose executable.
    pub binary: String,
    /// Backend goose itself talks to, passed as GOOSE_PROVIDER.
    pub goose_provider: String,
    /// Extra environment for the child, e.g. OPENAI_HOST for a local vLLM
    /// server. The parent environment is inherited; these entries win.
    pub env: BTreeMap<String, String>,
    pub context_window_size: usize,
    pub timeout_secs: u64,
}

/// Isolated goose configuration. Chat mode is what makes goose a completion
/// backend: it answers with text and never calls a tool.
const CONFIG_YAML: &str = "GOOSE_MODE: chat\n\
GOOSE_TELEMETRY_ENABLED: false\n\
extensions: {}\n";

/// Builds the arguments that put goose into ACP stdio mode.
fn build_args() -> Vec<String> {
    vec!["acp".to_string()]
}

/// Creates the throwaway configuration directory for one goose run. The
/// returned TempDir must outlive the child process.
fn create_isolated_config() -> Result<TempDir> {
    let tmp = tempfile::tempdir()?;
    let config_dir = tmp.path().join("goose");
    std::fs::create_dir_all(&config_dir)?;
    std::fs::write(config_dir.join("config.yaml"), CONFIG_YAML)?;
    Ok(tmp)
}

/// Environment for one goose run. `env` overrides are applied last so a
/// configuration file can correct any default.
fn build_env(
    model: &str,
    goose_provider: &str,
    context_window_size: usize,
    config_home: &str,
    extra: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("XDG_CONFIG_HOME".to_string(), config_home.to_string());
    env.insert("GOOSE_PROVIDER".to_string(), goose_provider.to_string());
    env.insert("GOOSE_MODEL".to_string(), model.to_string());
    env.insert("GOOSE_MODE".to_string(), "chat".to_string());
    env.insert("GOOSE_DISABLE_KEYRING".to_string(), "1".to_string());
    env.insert("GOOSE_TELEMETRY_ENABLED".to_string(), "false".to_string());
    env.insert(
        "GOOSE_CONTEXT_LIMIT".to_string(),
        context_window_size.to_string(),
    );
    for (key, value) in extra {
        env.insert(key.clone(), value.clone());
    }
    env
}

/// Reads goose's own token accounting out of a `session/prompt` result.
fn usage_from_result(result: &Value) -> Option<AiUsage> {
    let usage = result.get("usage")?;
    let field = |name: &str| usage.get(name).and_then(Value::as_u64).map(|v| v as usize);

    let prompt_tokens = field("inputTokens")?;
    let completion_tokens = field("outputTokens")?;
    let total_tokens = field("totalTokens").unwrap_or(prompt_tokens + completion_tokens);

    Some(AiUsage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cached_tokens: None,
    })
}

impl GooseCliProvider {
    async fn run_acp_prompt(&self, prompt: &str) -> Result<(String, Value)> {
        let tmp = create_isolated_config()?;
        let config_home = tmp.path().to_string_lossy().to_string();

        let process = AcpProcess {
            label: "goose acp".to_string(),
            binary: self.binary.clone(),
            args: build_args(),
            working_dir: Some(tmp.path().to_path_buf()),
            env: build_env(
                &self.model,
                &self.goose_provider,
                self.context_window_size,
                &config_home,
                &self.env,
            ),
            // goose rejects a relative session cwd.
            session_cwd: config_home,
        };

        let prompt = process.run_prompt(prompt).await?;
        Ok((prompt.text, prompt.result))
    }
}

#[async_trait]
impl AiProvider for GooseCliProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let prompt = build_prompt(&request);
        debug!("goose prompt length: {} chars", prompt.len());

        let (text, result) = timeout(
            Duration::from_secs(self.timeout_secs),
            self.run_acp_prompt(&prompt),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("goose acp timed out after {} seconds", self.timeout_secs)
        })??;

        // goose reports its own token counts; fall back to estimates when a
        // build omits them.
        let usage = usage_from_result(&result).or_else(|| {
            let prompt_tokens = TokenBudget::estimate_tokens(&prompt);
            let completion_tokens = TokenBudget::estimate_tokens(&text);
            Some(AiUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
                cached_tokens: None,
            })
        });

        parse_inner_response(&text, usage)
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        let prompt = build_prompt(request);
        TokenBudget::estimate_tokens(&prompt)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: self.context_window_size,
        }
    }

    fn cache_identity(&self) -> String {
        // The backend goose fronts changes the answer, so it belongs in the
        // identity. context_window_size stays out: it only budgets the prompt
        // the cache already hashes.
        crate::ai::cache_identity_with(&self.model, &[("provider", Some(&self.goose_provider))])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiMessage, AiRole, AiTool, create_provider};
    use crate::settings::Settings;

    fn test_provider(binary: &str, goose_provider: &str) -> GooseCliProvider {
        GooseCliProvider {
            model: "qwen3-8b-ov".to_string(),
            binary: binary.to_string(),
            goose_provider: goose_provider.to_string(),
            env: BTreeMap::new(),
            context_window_size: 8192,
            timeout_secs: 60,
        }
    }

    fn sample_request() -> AiRequest {
        AiRequest {
            system: Some("You are a kernel reviewer.".to_string()),
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Review this patch.".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: Some(vec![AiTool {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }]),
            temperature: None,
            response_format: None,
            context_tag: None,
        }
    }

    #[test]
    fn args_request_acp_mode() {
        assert_eq!(build_args(), vec!["acp".to_string()]);
    }

    #[test]
    fn isolated_config_pins_chat_mode() {
        let tmp = create_isolated_config().unwrap();
        let config = std::fs::read_to_string(tmp.path().join("goose/config.yaml")).unwrap();
        assert!(config.contains("GOOSE_MODE: chat"));
        assert!(config.contains("extensions: {}"));
    }

    #[test]
    fn env_pins_provider_model_and_config_home() {
        let env = build_env("m1", "openai", 4096, "/tmp/cfg", &BTreeMap::new());
        assert_eq!(env.get("GOOSE_MODEL").unwrap(), "m1");
        assert_eq!(env.get("GOOSE_PROVIDER").unwrap(), "openai");
        assert_eq!(env.get("GOOSE_MODE").unwrap(), "chat");
        assert_eq!(env.get("GOOSE_CONTEXT_LIMIT").unwrap(), "4096");
        assert_eq!(env.get("XDG_CONFIG_HOME").unwrap(), "/tmp/cfg");
    }

    #[test]
    fn env_overrides_win() {
        let mut extra = BTreeMap::new();
        extra.insert(
            "OPENAI_HOST".to_string(),
            "http://localhost:8000".to_string(),
        );
        extra.insert("GOOSE_MODE".to_string(), "auto".to_string());
        let env = build_env("m1", "openai", 4096, "/tmp/cfg", &extra);
        assert_eq!(env.get("OPENAI_HOST").unwrap(), "http://localhost:8000");
        assert_eq!(env.get("GOOSE_MODE").unwrap(), "auto");
    }

    #[test]
    fn cache_identity_tracks_backend_provider() {
        let openai = test_provider("goose", "openai");
        let ollama = test_provider("goose", "ollama");
        assert_ne!(openai.cache_identity(), ollama.cache_identity());
    }

    #[test]
    fn usage_is_read_from_prompt_result() {
        let result = serde_json::json!({
            "stopReason": "end_turn",
            "usage": {"totalTokens": 5114, "inputTokens": 4992, "outputTokens": 122}
        });
        let usage = usage_from_result(&result).unwrap();
        assert_eq!(usage.prompt_tokens, 4992);
        assert_eq!(usage.completion_tokens, 122);
        assert_eq!(usage.total_tokens, 5114);
    }

    #[test]
    fn usage_is_absent_without_counts() {
        let result = serde_json::json!({"stopReason": "end_turn"});
        assert!(usage_from_result(&result).is_none());
    }

    #[test]
    fn estimate_tokens_is_non_zero() {
        let provider = test_provider("goose", "openai");
        assert!(provider.estimate_tokens(&sample_request()) > 0);
    }

    #[test]
    fn factory_creates_provider() {
        let mut settings = Settings::new().expect("Failed to load settings");
        settings.ai.provider = "goose".to_string();
        settings.ai.model = "qwen3-8b-ov".to_string();

        let provider = create_provider(&settings).unwrap();
        let caps = provider.get_capabilities();
        assert_eq!(caps.model_name, "qwen3-8b-ov");
        assert_eq!(caps.context_window_size, 128_000);
    }

    /// Fake ACP agent: replies to initialize, session/new and session/prompt.
    fn fake_goose(dir: &std::path::Path) -> std::path::PathBuf {
        let fake = dir.join("fake-goose");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
i=0
while IFS= read -r line; do
  case "$i" in
    0) printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1}}' ;;
    1) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"sessionId":"20260911_1"}}' ;;
    2)
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"20260911_1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"{\"content\":\"ok\"}"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn","usage":{"totalTokens":30,"inputTokens":20,"outputTokens":10}}}'
      exit 0
      ;;
  esac
  i=$((i + 1))
done
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        fake
    }

    #[tokio::test]
    async fn generate_content_with_fake_acp_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = fake_goose(tmp.path());
        let provider = test_provider(&fake.to_string_lossy(), "openai");

        let response = provider.generate_content(sample_request()).await.unwrap();
        assert_eq!(response.content.as_deref(), Some("ok"));
        let usage = response.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 20);
        assert_eq!(usage.completion_tokens, 10);
    }

    #[tokio::test]
    async fn generate_content_reports_redacted_stderr_on_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("failing-goose");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
printf '%s\n' 'provider rejected request api_key=abc123' >&2
sleep 0.1
exit 2
"#,
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let provider = test_provider(&fake.to_string_lossy(), "openai");
        let err = provider
            .generate_content(sample_request())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("goose acp exited before response 0"));
        assert!(err.contains("api_key=[REDACTED]"));
        assert!(!err.contains("abc123"));
    }
}
