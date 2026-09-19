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
//! Each request runs in a throwaway set of XDG directories whose
//! `config.yaml` pins goose to chat mode, so goose never executes a tool of
//! its own: Sashiko's ToolBox stays the only tool layer. The same directories
//! keep the user's own goose configuration out of a review and take goose's
//! session database and logs with them when the request ends.
//!
//! goose reports token usage in its `session/prompt` result, which is used
//! verbatim when present. Note that goose prepends its own system prompt and
//! platform tool schemas to every request, so its input token count is
//! noticeably higher than the prompt Sashiko sends. Budget for that when
//! setting `context_window_size`.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
    /// server. The parent environment is inherited and these entries win
    /// over it, but not over the variables Sashiko pins to keep goose a
    /// completion backend.
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

/// Throwaway XDG directories for one goose run.
///
/// goose keeps its configuration in `XDG_CONFIG_HOME`, its session database
/// in `XDG_DATA_HOME` and its logs in `XDG_STATE_HOME`. All three are
/// redirected here. Redirecting the configuration is what pins goose to chat
/// mode and hides the user's own extensions; redirecting the other two keeps
/// a review from recording a session and a log file per request in the
/// user's home directory, and keeps concurrent reviews off a single session
/// database.
///
/// The directories are removed when this value is dropped, so it has to
/// outlive the child process.
struct IsolatedHome {
    tmp: TempDir,
}

impl IsolatedHome {
    /// Lays out the directories and writes the configuration that pins goose
    /// to chat mode.
    fn create() -> Result<Self> {
        let home = Self {
            tmp: tempfile::tempdir()?,
        };
        let config_dir = home.config_home().join("goose");
        std::fs::create_dir_all(&config_dir)?;
        std::fs::create_dir_all(home.data_home())?;
        std::fs::create_dir_all(home.state_home())?;
        std::fs::write(config_dir.join("config.yaml"), CONFIG_YAML)?;
        Ok(home)
    }

    fn config_home(&self) -> PathBuf {
        self.tmp.path().join("config")
    }

    fn data_home(&self) -> PathBuf {
        self.tmp.path().join("data")
    }

    fn state_home(&self) -> PathBuf {
        self.tmp.path().join("state")
    }

    /// Working directory of the child, and the `cwd` of its session. goose
    /// rejects a relative session cwd, and the child has no business outside
    /// the throwaway directory.
    fn root(&self) -> &Path {
        self.tmp.path()
    }
}

/// Environment for one goose run. The caller's entries land first, so they
/// add to and override whatever goose would otherwise inherit from Sashiko,
/// and the variables Sashiko derives from its own settings are pinned after
/// them. A stray GOOSE_MODE or XDG_CONFIG_HOME in a configuration file
/// therefore cannot hand goose back its own tools, its own session history
/// or the user's own configuration, and GOOSE_MODEL cannot drift away from
/// the model the response cache is keyed on.
fn build_env(
    model: &str,
    goose_provider: &str,
    context_window_size: usize,
    home: &IsolatedHome,
    extra: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let path = |dir: PathBuf| dir.to_string_lossy().to_string();

    let mut env = extra.clone();
    env.insert("XDG_CONFIG_HOME".to_string(), path(home.config_home()));
    env.insert("XDG_DATA_HOME".to_string(), path(home.data_home()));
    env.insert("XDG_STATE_HOME".to_string(), path(home.state_home()));
    env.insert("GOOSE_PROVIDER".to_string(), goose_provider.to_string());
    env.insert("GOOSE_MODEL".to_string(), model.to_string());
    env.insert("GOOSE_MODE".to_string(), "chat".to_string());
    env.insert("GOOSE_DISABLE_KEYRING".to_string(), "1".to_string());
    env.insert("GOOSE_TELEMETRY_ENABLED".to_string(), "false".to_string());
    env.insert(
        "GOOSE_CONTEXT_LIMIT".to_string(),
        context_window_size.to_string(),
    );
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
        let home = IsolatedHome::create()?;

        let process = AcpProcess {
            label: "goose acp".to_string(),
            binary: self.binary.clone(),
            args: build_args(),
            working_dir: Some(home.root().to_path_buf()),
            env: build_env(
                &self.model,
                &self.goose_provider,
                self.context_window_size,
                &home,
                &self.env,
            ),
            session_cwd: home.root().to_string_lossy().to_string(),
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
            let prompt_tokens = TokenBudget::approximate_tokens(&prompt);
            let completion_tokens = TokenBudget::approximate_tokens(&text);
            Some(AiUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
                cached_tokens: None,
            })
        });

        parse_inner_response(&text, usage)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: self.context_window_size,
        }
    }

    fn cache_identity(&self) -> String {
        // Everything that picks the backend belongs here. goose_provider
        // names the family, and the env table points at the instance:
        // OPENAI_HOST is goose's base_url, so without it two local servers
        // both serving "qwen3-8b-ov" replay each other's answers. The table
        // can hold an API key, so it contributes a digest rather than its
        // contents. GOOSE_CONTEXT_LIMIT reaches the child and decides how
        // much of the prompt the backend sees, exactly as num_ctx does for
        // ollama, so it counts too.
        let context_limit = self.context_window_size.to_string();
        let env_digest = digest_env(&self.env);
        crate::ai::cache_identity_with(
            &self.model,
            &[
                ("provider", Some(&self.goose_provider)),
                ("context_limit", Some(context_limit.as_str())),
                ("env", env_digest.as_deref()),
            ],
        )
    }
}

/// Condenses the child's environment into a short hex digest, or nothing
/// when there is none to report. The table is ordered, so the digest is
/// stable across runs.
fn digest_env(env: &BTreeMap<String, String>) -> Option<String> {
    if env.is_empty() {
        return None;
    }

    let mut hasher = Sha256::new();
    for (key, value) in env {
        hasher.update(key.as_bytes());
        hasher.update(b"=");
        hasher.update(value.as_bytes());
        hasher.update(b"\0");
    }
    Some(
        hasher.finalize()[..8]
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect(),
    )
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
                provider_metadata: None,
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
    fn isolated_home_pins_chat_mode() {
        let home = IsolatedHome::create().unwrap();
        let config = std::fs::read_to_string(home.config_home().join("goose/config.yaml")).unwrap();
        assert!(config.contains("GOOSE_MODE: chat"));
        assert!(config.contains("extensions: {}"));
    }

    #[test]
    fn isolated_home_separates_config_data_and_state() {
        let home = IsolatedHome::create().unwrap();
        for dir in [home.config_home(), home.data_home(), home.state_home()] {
            assert!(dir.is_dir());
            assert!(dir.starts_with(home.root()));
        }
        assert_ne!(home.data_home(), home.config_home());
        assert_ne!(home.state_home(), home.config_home());
    }

    #[test]
    fn env_pins_provider_model_and_directories() {
        let home = IsolatedHome::create().unwrap();
        let env = build_env("m1", "openai", 4096, &home, &BTreeMap::new());
        assert_eq!(env.get("GOOSE_MODEL").unwrap(), "m1");
        assert_eq!(env.get("GOOSE_PROVIDER").unwrap(), "openai");
        assert_eq!(env.get("GOOSE_MODE").unwrap(), "chat");
        assert_eq!(env.get("GOOSE_CONTEXT_LIMIT").unwrap(), "4096");
        for var in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"] {
            let dir = PathBuf::from(env.get(var).unwrap());
            assert!(dir.starts_with(home.root()), "{} escaped the home", var);
        }
    }

    #[test]
    fn env_entries_reach_the_child() {
        let home = IsolatedHome::create().unwrap();
        let mut extra = BTreeMap::new();
        extra.insert(
            "OPENAI_HOST".to_string(),
            "http://localhost:8000".to_string(),
        );
        let env = build_env("m1", "openai", 4096, &home, &extra);
        assert_eq!(env.get("OPENAI_HOST").unwrap(), "http://localhost:8000");
    }

    #[test]
    fn env_entries_cannot_unpin_the_isolation() {
        let home = IsolatedHome::create().unwrap();
        let mut extra = BTreeMap::new();
        extra.insert("GOOSE_MODE".to_string(), "auto".to_string());
        extra.insert("GOOSE_MODEL".to_string(), "some-other-model".to_string());
        for var in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"] {
            extra.insert(var.to_string(), "/home/user/escaped".to_string());
        }

        let env = build_env("m1", "openai", 4096, &home, &extra);

        assert_eq!(env.get("GOOSE_MODE").unwrap(), "chat");
        assert_eq!(env.get("GOOSE_MODEL").unwrap(), "m1");
        for var in ["XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"] {
            let dir = PathBuf::from(env.get(var).unwrap());
            assert!(dir.starts_with(home.root()), "{} escaped the home", var);
        }
    }

    #[test]
    fn cache_identity_tracks_backend_provider() {
        let openai = test_provider("goose", "openai");
        let ollama = test_provider("goose", "ollama");
        assert_ne!(openai.cache_identity(), ollama.cache_identity());
    }

    #[test]
    fn cache_identity_separates_two_hosts_serving_one_model() {
        let host = |url: &str| {
            let mut provider = test_provider("goose", "openai");
            provider
                .env
                .insert("OPENAI_HOST".to_string(), url.to_string());
            provider
        };

        assert_ne!(
            host("http://localhost:8000").cache_identity(),
            host("http://localhost:8001").cache_identity()
        );
    }

    #[test]
    fn cache_identity_tracks_the_context_limit() {
        let mut narrow = test_provider("goose", "openai");
        narrow.context_window_size = 8192;
        let mut wide = test_provider("goose", "openai");
        wide.context_window_size = 32768;
        assert_ne!(narrow.cache_identity(), wide.cache_identity());
    }

    #[test]
    fn cache_identity_keeps_the_env_out_of_the_key() {
        let mut provider = test_provider("goose", "openai");
        provider
            .env
            .insert("OPENAI_API_KEY".to_string(), "sk-secret".to_string());
        assert!(!provider.cache_identity().contains("sk-secret"));
    }

    #[test]
    fn env_digest_is_absent_without_entries() {
        assert!(digest_env(&BTreeMap::new()).is_none());
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
        // The agent waits for the initialize request before exiting, so the
        // client always reaches the read and reports the exit. Letting it
        // exit on a timer instead lets a loaded machine turn the write into
        // a broken pipe and report that instead.
        std::fs::write(
            &fake,
            r#"#!/bin/sh
printf '%s\n' 'provider rejected request api_key=abc123' >&2
IFS= read -r line
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
