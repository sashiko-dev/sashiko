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

//! AI provider that shells out to `kiro-cli acp`.
//!
//! By default, Kiro runs under an isolated temporary agent with all native
//! tools disabled. A deny-all pre-tool hook acts as a defensive backstop.
//! This makes the provider a pure completion backend: Sashiko's own ToolBox
//! remains the only tool execution layer.

use anyhow::Result;
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use tracing::debug;

use super::acp::AcpProcess;
use super::claude_cli::{build_prompt, parse_inner_response};
use super::token_budget::TokenBudget;
use crate::ai::{AiProvider, AiRequest, AiResponse, AiUsage, ProviderCapabilities};

pub struct KiroCliProvider {
    pub model: String,
    pub binary: String,
    pub agent: Option<String>,
    pub context_window_size: usize,
    pub timeout_secs: u64,
}

/// Agent JSON for the isolated no-tool Sashiko provider agent.
const AGENT_JSON: &str = r#"{
  "name": "sashiko-provider",
  "description": "Stateless Sashiko completion backend. Native Kiro tools are disabled.",
  "prompt": "Follow the user-provided instructions exactly.",
  "mcpServers": {},
  "tools": [],
  "allowedTools": [],
  "resources": [],
  "includeMcpJson": false,
  "hooks": {
    "preToolUse": [
      {
        "command": ".kiro/hooks/deny-all-tools.sh"
      }
    ]
  }
}"#;

/// Shell script that denies all Kiro native tool invocations.
const DENY_ALL_HOOK: &str = "#!/bin/sh\n\
echo \"Kiro native tools are disabled for the Sashiko provider\" >&2\n\
exit 1\n";

/// Build the kiro-cli command arguments.
fn build_args(model: &str, agent: &str) -> Vec<String> {
    vec![
        "acp".to_string(),
        "--agent".to_string(),
        agent.to_string(),
        "--model".to_string(),
        model.to_string(),
    ]
}

/// Create the isolated temporary workspace with a no-tool agent and deny-all hook.
/// Returns the TempDir (must be kept alive for the duration of the process).
fn create_isolated_workspace() -> Result<TempDir> {
    let tmp = tempfile::tempdir()?;
    let agents_dir = tmp.path().join(".kiro/agents");
    std::fs::create_dir_all(&agents_dir)?;
    std::fs::write(agents_dir.join("sashiko-provider.json"), AGENT_JSON)?;

    let hooks_dir = tmp.path().join(".kiro/hooks");
    std::fs::create_dir_all(&hooks_dir)?;
    let hook_path = hooks_dir.join("deny-all-tools.sh");
    std::fs::write(&hook_path, DENY_ALL_HOOK)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))?;
    }

    Ok(tmp)
}

impl KiroCliProvider {
    async fn run_acp_prompt(
        &self,
        prompt: &str,
        agent_name: &str,
        isolated_workspace: Option<&TempDir>,
    ) -> Result<String> {
        let process = AcpProcess {
            label: "kiro-cli ACP".to_string(),
            binary: self.binary.clone(),
            args: build_args(&self.model, agent_name),
            working_dir: isolated_workspace.map(|tmp| tmp.path().to_path_buf()),
            env: BTreeMap::new(),
            session_cwd: ".".to_string(),
        };

        Ok(process.run_prompt(prompt).await?.text)
    }
}

#[async_trait]
impl AiProvider for KiroCliProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let prompt = build_prompt(&request);
        debug!("kiro-cli prompt length: {} chars", prompt.len());

        let (agent_name, isolated_workspace) = match &self.agent {
            Some(a) => (a.clone(), None),
            None => {
                let tmp = create_isolated_workspace()?;
                ("sashiko-provider".to_string(), Some(tmp))
            }
        };

        let text = timeout(
            Duration::from_secs(self.timeout_secs),
            self.run_acp_prompt(&prompt, &agent_name, isolated_workspace.as_ref()),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("kiro-cli ACP timed out after {} seconds", self.timeout_secs)
        })??;

        // Synthesize usage from token estimates since kiro-cli does not
        // expose provider token counts.
        let prompt_tokens = TokenBudget::estimate_tokens(&prompt);
        let completion_tokens = TokenBudget::estimate_tokens(&text);
        let usage = Some(AiUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
            cached_tokens: None,
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
        // context_window_size stays out. Unlike ollama's num_ctx it never
        // reaches the wire; it only budgets the prompt the cache hashes.
        crate::ai::cache_identity_with(&self.model, &[("agent", self.agent.as_deref())])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiMessage, AiRole, AiTool, create_provider};
    use crate::settings::Settings;

    fn test_provider(agent: Option<&str>) -> KiroCliProvider {
        KiroCliProvider {
            model: "claude-sonnet-4-6".to_string(),
            binary: "kiro-cli".to_string(),
            agent: agent.map(str::to_string),
            context_window_size: 200_000,
            timeout_secs: 60,
        }
    }

    #[test]
    fn cache_identity_tracks_agent() {
        let base = test_provider(None);
        let named = test_provider(Some("reviewer"));
        assert_ne!(base.cache_identity(), named.cache_identity());
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
    fn test_factory_creates_provider() {
        let mut settings = Settings::new().expect("Failed to load settings");
        settings.ai.provider = "kiro-cli".to_string();
        settings.ai.model = "claude-sonnet-4".to_string();

        let provider = create_provider(&settings).unwrap();
        let caps = provider.get_capabilities();
        assert_eq!(caps.model_name, "claude-sonnet-4");
        assert_eq!(caps.context_window_size, 200_000);
    }

    #[test]
    fn test_command_args_default() {
        let args = build_args("claude-sonnet-4", "sashiko-provider");
        assert_eq!(args[0], "acp");
        assert_eq!(args[1], "--agent");
        assert_eq!(args[2], "sashiko-provider");
        assert_eq!(args[3], "--model");
        assert_eq!(args[4], "claude-sonnet-4");
    }

    #[test]
    fn test_command_args_custom_agent() {
        let args = build_args("claude-sonnet-4", "my-agent");
        assert_eq!(
            args,
            vec![
                "acp".to_string(),
                "--agent".to_string(),
                "my-agent".to_string(),
                "--model".to_string(),
                "claude-sonnet-4".to_string(),
            ]
        );
    }

    #[test]
    fn test_isolated_workspace_agent_json() {
        let tmp = create_isolated_workspace().unwrap();
        let agent_path = tmp.path().join(".kiro/agents/sashiko-provider.json");
        assert!(agent_path.exists());

        let content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&agent_path).unwrap()).unwrap();

        assert_eq!(content["tools"], serde_json::json!([]));
        assert_eq!(content["allowedTools"], serde_json::json!([]));
        assert_eq!(content["mcpServers"], serde_json::json!({}));
        assert_eq!(content["resources"], serde_json::json!([]));
        assert_eq!(content["includeMcpJson"], serde_json::json!(false));
        assert!(
            content["prompt"]
                .as_str()
                .unwrap()
                .contains("Follow the user-provided instructions exactly")
        );

        // Verify deny-all hook is wired
        let hooks = &content["hooks"]["preToolUse"];
        assert!(hooks.is_array());
        let hook_cmd = hooks[0]["command"].as_str().unwrap();
        assert_eq!(hook_cmd, ".kiro/hooks/deny-all-tools.sh");
    }

    #[test]
    fn test_deny_all_hook_exits_nonzero() {
        let tmp = create_isolated_workspace().unwrap();
        let hook_path = tmp.path().join(".kiro/hooks/deny-all-tools.sh");
        assert!(hook_path.exists());

        let output = std::process::Command::new("sh")
            .arg(&hook_path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("disabled"));
    }

    #[test]
    fn test_parse_tool_calls_json() {
        let text = r#"{"tool_calls":[{"id":"c1","function_name":"read_file","arguments":{"path":"README.md"}}]}"#;
        let resp = parse_inner_response(text, None).unwrap();
        assert!(resp.tool_calls.is_some());
        let calls = resp.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function_name, "read_file");
        assert_eq!(calls[0].arguments["path"], "README.md");
    }

    #[test]
    fn test_parse_plain_content() {
        let text = r#"{"content":"No issues found in this patch."}"#;
        let resp = parse_inner_response(text, None).unwrap();
        assert_eq!(
            resp.content.as_deref(),
            Some("No issues found in this patch.")
        );
        assert!(resp.tool_calls.is_none());
    }

    #[test]
    fn test_parse_raw_text_fallback() {
        let text = "This is not JSON at all.";
        let resp = parse_inner_response(text, None).unwrap();
        assert_eq!(resp.content.as_deref(), Some(text));
    }

    #[test]
    fn test_estimate_tokens_uses_token_budget() {
        let provider = KiroCliProvider {
            model: "test".to_string(),
            binary: "kiro-cli".to_string(),
            agent: None,
            context_window_size: 200_000,
            timeout_secs: 300,
        };
        let req = sample_request();
        let estimate = provider.estimate_tokens(&req);
        // The prompt is non-empty, so estimate should be > 0
        assert!(estimate > 0);
    }

    #[tokio::test]
    async fn test_generate_content_with_fake_acp_server() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
i=0
while IFS= read -r line; do
  case "$i" in
    0) printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1}}' ;;
    1) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"sessionId":"s1"}}' ;;
    2)
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"{\"content\":\"ok\"}"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn"}}'
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

        let provider = KiroCliProvider {
            model: "test".to_string(),
            binary: fake.to_string_lossy().to_string(),
            agent: None,
            context_window_size: 200_000,
            timeout_secs: 5,
        };
        let response = provider.generate_content(sample_request()).await.unwrap();
        assert_eq!(response.content.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn test_generate_content_includes_redacted_stderr_on_startup_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("fake-kiro-cli");
        std::fs::write(
            &fake,
            r#"#!/bin/sh
printf '%s\n' 'authentication failed token=abc123' >&2
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

        let provider = KiroCliProvider {
            model: "test".to_string(),
            binary: fake.to_string_lossy().to_string(),
            agent: None,
            context_window_size: 200_000,
            timeout_secs: 5,
        };

        let err = provider
            .generate_content(sample_request())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("kiro-cli ACP exited before response 0"));
        assert!(err.contains("stderr: authentication failed token=[REDACTED]"));
        assert!(!err.contains("abc123"));
    }
}
