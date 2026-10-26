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

use std::io;
use std::process::Output;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;

/// Transfers a prompt while draining both output pipes. The caller owns the
/// overall timeout and configures kill_on_drop before spawning the child.
pub(super) async fn collect_cli_output(mut child: Child, prompt: &str) -> io::Result<Output> {
    let stdin = child.stdin.take();
    let write_prompt = async move {
        if let Some(mut stdin) = stdin {
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.flush().await?;
        }
        Ok::<(), io::Error>(())
    };
    // A CLI may fill stderr before reading its prompt, so both directions
    // must make progress together rather than wait for prompt delivery first.
    let (written, output) = tokio::join!(write_prompt, child.wait_with_output());
    let output = output?;
    // An early nonzero exit may close stdin before the prompt was delivered.
    // Return its output so the provider can preserve the actual diagnostics.
    if output.status.success() {
        written.map_err(|error| {
            io::Error::new(error.kind(), format!("failed to write prompt: {error}"))
        })?;
    }
    Ok(output)
}

#[cfg(all(test, unix))]
mod tests {
    use crate::ai::codex_cli::CodexCliProvider;
    use crate::ai::copilot_cli::CopilotCliProvider;
    use crate::ai::{AiMessage, AiRequest, AiResponse, AiRole};
    use std::ffi::OsStr;
    use std::time::Duration;
    use tokio::time::timeout;

    #[derive(Clone, Copy, Debug)]
    enum Provider {
        Codex,
        Copilot,
    }

    impl Provider {
        async fn generate(
            self,
            binary: &OsStr,
            call_timeout: Duration,
        ) -> anyhow::Result<AiResponse> {
            let request = AiRequest {
                messages: vec![AiMessage {
                    role: AiRole::User,
                    content: Some("x".repeat(1024 * 1024)),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    tool_call_id: None,
                }],
                ..AiRequest::default()
            };
            match self {
                Self::Codex => {
                    CodexCliProvider {
                        model: "test-model".to_string(),
                        effort: None,
                    }
                    .generate_content_with_binary(request, binary, call_timeout)
                    .await
                }
                Self::Copilot => {
                    CopilotCliProvider {
                        model: "test-model".to_string(),
                    }
                    .generate_content_with_binary(request, binary, call_timeout)
                    .await
                }
            }
        }

        fn successful_output(self) -> &'static str {
            match self {
                Self::Codex => concat!(
                    r#"{"type":"item.completed","item":{"text":"OK"}}"#,
                    "\n",
                    r#"{"type":"turn.completed","usage":{"input_tokens":7,"output_tokens":3,"cached_input_tokens":2}}"#,
                    "\n",
                ),
                Self::Copilot => concat!(
                    r#"{"type":"assistant.message","data":{"content":"OK","outputTokens":3}}"#,
                    "\n",
                    r#"{"type":"result","data":{"exitCode":0,"usage":{}}}"#,
                    "\n",
                ),
            }
        }
    }

    fn fixture(script: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        crate::test_support::write_executable_script(&dir.path().join("cli"), script);
        dir
    }

    async fn assert_stdin_is_bounded(provider: Provider) {
        let dir = fixture("#!/bin/sh\nexec sleep 1\n");
        let error = timeout(
            Duration::from_secs(3),
            provider.generate(
                dir.path().join("cli").as_os_str(),
                Duration::from_millis(100),
            ),
        )
        .await
        .expect("prompt transfer must respect the call deadline")
        .unwrap_err();
        assert!(
            error.to_string().contains("timed out"),
            "{provider:?}: {error}"
        );
    }

    async fn assert_stderr_is_drained(provider: Provider) {
        let script = format!(
            "#!/bin/sh\ndd if=/dev/zero bs=131072 count=1 2>/dev/null | tr '\\000' x >&2\ncat >/dev/null\nprintf '%s' '{}'\n",
            provider.successful_output(),
        );
        let dir = fixture(&script);
        let response = timeout(
            Duration::from_secs(10),
            provider.generate(dir.path().join("cli").as_os_str(), Duration::from_secs(5)),
        )
        .await
        .expect("large prompt and startup output must make progress concurrently")
        .unwrap();
        assert_eq!(response.content.as_deref(), Some("OK"));
        let usage = response.usage.unwrap();
        assert_eq!(usage.completion_tokens, 3);
        match provider {
            Provider::Codex => {
                assert_eq!(usage.prompt_tokens, 7);
                assert_eq!(usage.total_tokens, 10);
                assert_eq!(usage.cached_tokens, Some(2));
            }
            Provider::Copilot => {
                assert_eq!(usage.prompt_tokens, 0);
                assert_eq!(usage.total_tokens, 3);
                assert_eq!(usage.cached_tokens, None);
            }
        }
    }

    async fn assert_early_exit_keeps_diagnostics(provider: Provider) {
        let dir = fixture(
            "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"error\",\"message\":\"test stdout diagnostic\"}'\nprintf '%s\\n' 'test stderr diagnostic' >&2\nexit 1\n",
        );
        let error = timeout(
            Duration::from_secs(3),
            provider.generate(dir.path().join("cli").as_os_str(), Duration::from_secs(2)),
        )
        .await
        .expect("an early exit must not hang")
        .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("test stderr diagnostic"),
            "{provider:?}: {message}"
        );
        if matches!(provider, Provider::Codex) {
            assert!(message.contains("test stdout diagnostic"), "{message}");
        }
    }

    async fn assert_incomplete_prompt_is_rejected(provider: Provider) {
        let script = format!(
            "#!/bin/sh\nprintf '%s' '{}'\nexit 0\n",
            provider.successful_output(),
        );
        let dir = fixture(&script);
        timeout(
            Duration::from_secs(3),
            provider.generate(dir.path().join("cli").as_os_str(), Duration::from_secs(2)),
        )
        .await
        .expect("an early successful exit must not hang")
        .expect_err("a successful exit cannot mask an undelivered prompt");
    }

    #[tokio::test]
    async fn codex_prompt_io_bounds_stdin() {
        assert_stdin_is_bounded(Provider::Codex).await;
    }

    #[tokio::test]
    async fn copilot_prompt_io_bounds_stdin() {
        assert_stdin_is_bounded(Provider::Copilot).await;
    }

    #[tokio::test]
    async fn codex_prompt_io_drains_stderr() {
        assert_stderr_is_drained(Provider::Codex).await;
    }

    #[tokio::test]
    async fn copilot_prompt_io_drains_stderr() {
        assert_stderr_is_drained(Provider::Copilot).await;
    }

    #[tokio::test]
    async fn codex_prompt_io_keeps_early_exit_diagnostics() {
        assert_early_exit_keeps_diagnostics(Provider::Codex).await;
    }

    #[tokio::test]
    async fn copilot_prompt_io_keeps_early_exit_diagnostics() {
        assert_early_exit_keeps_diagnostics(Provider::Copilot).await;
    }

    #[tokio::test]
    async fn codex_prompt_io_rejects_incomplete_delivery() {
        assert_incomplete_prompt_is_rejected(Provider::Codex).await;
    }

    #[tokio::test]
    async fn copilot_prompt_io_rejects_incomplete_delivery() {
        assert_incomplete_prompt_is_rejected(Provider::Copilot).await;
    }
}
