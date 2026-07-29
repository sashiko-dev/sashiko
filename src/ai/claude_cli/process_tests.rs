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

use super::*;
use crate::ai::{AiMessage, AiRole};
use std::path::PathBuf;
use tokio::time::timeout;

fn fake_cli(response: &str, status: u8) -> tempfile::TempDir {
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("response"), response).unwrap();
    std::fs::write(fixture.path().join("status"), status.to_string()).unwrap();
    crate::test_support::write_executable_script(
        &fixture.path().join("claude"),
        r#"#!/bin/sh
fixture_dir=${0%/*}
printf '%s\n' "$PWD" "$@" > "$fixture_dir/args"
ls -A > "$fixture_dir/cwd-entries"
cat > "$fixture_dir/stdin"
cat "$fixture_dir/response"
exit "$(cat "$fixture_dir/status")"
"#,
    );
    fixture
}

fn request() -> AiRequest {
    request_with_content("Review this patch without running it.".to_string())
}

fn request_with_content(content: String) -> AiRequest {
    AiRequest {
        system: None,
        messages: vec![AiMessage {
            role: AiRole::User,
            content: Some(content),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        tools: None,
        temperature: None,
        response_format: None,
        context_tag: None,
        reasoning_effort: None,
    }
}

#[tokio::test]
async fn cli_disables_native_tools_and_configuration_in_an_empty_directory() {
    let fixture = fake_cli(r#"{"result":"OK","is_error":false}"#, 0);
    let provider = ClaudeCliProvider {
        model: "test-model[1m]".to_string(),
        effort: Some("high".to_string()),
    };
    let expected_prompt = build_prompt(&request());
    let response = provider
        .generate_content_with_binary(
            request(),
            fixture.path().join("claude").as_os_str(),
            Duration::from_secs(600),
        )
        .await
        .unwrap();
    assert_eq!(response.content.as_deref(), Some("OK"));
    assert_eq!(
        std::fs::read_to_string(fixture.path().join("stdin")).unwrap(),
        expected_prompt
    );

    let captured = std::fs::read_to_string(fixture.path().join("args")).unwrap();
    let mut lines = captured.lines();
    let cwd = PathBuf::from(lines.next().unwrap());
    let args: Vec<_> = lines.collect();
    let value = |flag: &str| {
        let index = args
            .iter()
            .position(|arg| *arg == flag)
            .unwrap_or_else(|| panic!("missing CLI flag {flag}"));
        args[index + 1]
    };
    assert_eq!(value("--tools"), "");
    assert_eq!(value("--disallowedTools"), "mcp__*");
    assert_eq!(value("--setting-sources"), "");
    assert!(args.contains(&"--strict-mcp-config"));
    assert!(args.contains(&"--disable-slash-commands"));
    assert!(args.contains(&"--no-session-persistence"));
    assert_eq!(value("--model"), provider.model);
    assert_eq!(value("--effort"), "high");
    let mcp: Value = serde_json::from_str(value("--mcp-config")).unwrap();
    assert_eq!(mcp, serde_json::json!({"mcpServers": {}}));
    let settings: Value = serde_json::from_str(value("--settings")).unwrap();
    assert_eq!(settings["disableAllHooks"], true);
    assert_eq!(settings["autoMemoryEnabled"], false);
    assert_eq!(settings["claudeMdExcludes"], serde_json::json!(["**"]));
    assert_eq!(
        std::fs::read_to_string(fixture.path().join("cwd-entries")).unwrap(),
        ""
    );
    assert!(
        !cwd.exists(),
        "the completed call must remove its scratch directory"
    );
}

#[tokio::test]
async fn cli_removes_its_scratch_directory_after_an_error() {
    let fixture = fake_cli(r#"{"result":"test failure","is_error":true}"#, 1);
    let provider = ClaudeCliProvider {
        model: "test-model".to_string(),
        effort: None,
    };
    let error = provider
        .generate_content_with_binary(
            request(),
            fixture.path().join("claude").as_os_str(),
            Duration::from_secs(600),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("test failure"));
    let captured = std::fs::read_to_string(fixture.path().join("args")).unwrap();
    let cwd = PathBuf::from(captured.lines().next().unwrap());
    assert!(
        !cwd.exists(),
        "failed calls must remove their scratch directory"
    );
}

fn cli_script(script: &str) -> tempfile::TempDir {
    let fixture = tempfile::tempdir().unwrap();
    crate::test_support::write_executable_script(&fixture.path().join("claude"), script);
    fixture
}

#[tokio::test]
async fn cli_rejects_success_when_the_prompt_was_not_delivered() {
    let fixture = cli_script(
        r#"#!/bin/sh
printf '%s' '{"result":"OK","is_error":false}'
exit 0
"#,
    );
    let provider = ClaudeCliProvider {
        model: "test-model".to_string(),
        effort: None,
    };
    let error = provider
        .generate_content_with_binary(
            request_with_content("x".repeat(1024 * 1024)),
            fixture.path().join("claude").as_os_str(),
            Duration::from_secs(600),
        )
        .await
        .expect_err("a successful exit cannot mask an undelivered prompt");
    let error = error.downcast_ref::<ClaudeCliError>().unwrap();
    assert!(matches!(error, ClaudeCliError::Stdin(_)));
    assert!(matches!(
        error.ai_error_class(),
        AiErrorClass::Transient { retry_after }
            if retry_after == Duration::from_secs(30)
    ));
}

#[tokio::test]
async fn cli_preserves_early_exit_diagnostics_for_a_large_prompt() {
    let fixture = cli_script(
        r#"#!/bin/sh
printf '%s' '{"result":"managed MCP rejects strict mode","is_error":true}'
exit 1
"#,
    );
    let provider = ClaudeCliProvider {
        model: "test-model".to_string(),
        effort: None,
    };
    let error = timeout(
        Duration::from_secs(5),
        provider.generate_content_with_binary(
            request_with_content("x".repeat(1024 * 1024)),
            fixture.path().join("claude").as_os_str(),
            Duration::from_secs(600),
        ),
    )
    .await
    .expect("an early exit must not hang")
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("managed MCP rejects strict mode")
    );
}

#[tokio::test]
async fn cli_timeout_covers_a_stalled_stdin_write() {
    let fixture = cli_script("#!/bin/sh\nexec sleep 1\n");
    let provider = ClaudeCliProvider {
        model: "test-model".to_string(),
        effort: None,
    };
    let error = timeout(
        Duration::from_secs(3),
        provider.generate_content_with_binary(
            request_with_content("x".repeat(1024 * 1024)),
            fixture.path().join("claude").as_os_str(),
            Duration::from_millis(100),
        ),
    )
    .await
    .expect("stdin writing must be bounded")
    .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<ClaudeCliError>(),
        Some(ClaudeCliError::Timeout(_))
    ));
}

#[tokio::test]
async fn cli_drains_stderr_while_writing_a_large_prompt() {
    let fixture = cli_script(
        r#"#!/bin/sh
dd if=/dev/zero bs=131072 count=1 2>/dev/null | tr '\000' x >&2
cat >/dev/null
printf '%s' '{"result":"OK","is_error":false}'
"#,
    );
    let provider = ClaudeCliProvider {
        model: "test-model".to_string(),
        effort: None,
    };
    let response = timeout(
        Duration::from_secs(10),
        provider.generate_content_with_binary(
            request_with_content("x".repeat(1024 * 1024)),
            fixture.path().join("claude").as_os_str(),
            Duration::from_secs(5),
        ),
    )
    .await
    .expect("input and output must make progress together")
    .unwrap();
    assert_eq!(response.content.as_deref(), Some("OK"));
}
