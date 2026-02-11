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

mod common;

use common::trace_replayer::TraceReplayer;
use common::{MockEnv, SashikoProcess};
use std::time::Duration;
use wiremock::MockServer;

#[tokio::test]
async fn test_e2e_trace_replay() {
    common::setup_tracing();
    // 1. Setup Mock Environment
    let env = MockEnv::setup().await;

    // Create the buggy commit in the mock remote
    let repo_dir = &env.remote_dir;
    let mm_dir = repo_dir.join("mm");
    std::fs::create_dir_all(&mm_dir).unwrap();
    std::fs::write(
        mm_dir.join("mempool.c"),
        r#"
void mempool_kfree(void *element, void *pool_data)
{
	kfree(element);
}
"#,
    )
    .unwrap();

    let output = std::process::Command::new("git")
        .current_dir(repo_dir)
        .args(["add", "mm/mempool.c"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let output = std::process::Command::new("git")
        .current_dir(repo_dir)
        .env("GIT_AUTHOR_DATE", "2026-02-06T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-02-06T12:00:00Z")
        .args(["commit", "-m", "Initial baseline"])
        .output()
        .unwrap();
    assert!(output.status.success());

    // The buggy commit
    std::fs::write(
        mm_dir.join("mempool.c"),
        r#"
void mempool_kfree(void *element, void *pool_data)
{
	kfree(element);
	kfree(element);
}
"#,
    )
    .unwrap();

    let output = std::process::Command::new("git")
        .current_dir(repo_dir)
        .args(["add", "mm/mempool.c"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let output = std::process::Command::new("git")
        .current_dir(repo_dir)
        .env("GIT_AUTHOR_DATE", "2026-02-06T12:05:00Z")
        .env("GIT_COMMITTER_DATE", "2026-02-06T12:05:00Z")
        .args(["commit", "-m", "mm: fix mempool_kfree double free bug"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let target_sha = env.get_head_sha();
    let remote_url = env.get_remote_url();

    // Populate prompts
    let prompts_dir = &env.prompts_dir;
    std::fs::write(prompts_dir.join("review-core.md"), "Review Protocol").unwrap();
    std::fs::write(
        prompts_dir.join("technical-patterns.md"),
        "Technical Patterns",
    )
    .unwrap();
    std::fs::write(prompts_dir.join("mm.md"), "Memory Management Patterns").unwrap();
    std::fs::write(prompts_dir.join("inline-template.md"), "Inline Template").unwrap();

    // 2. Start Mock Gemini Server
    let mock_gemini = MockServer::start().await;
    let replayer = TraceReplayer::new();
    replayer.mount_all(&mock_gemini).await;

    // 3. Spawn Sashiko Process
    let sashiko = SashikoProcess::spawn(
        &env,
        env!("CARGO_BIN_EXE_sashiko"),
        vec![
            ("SASHIKO__AI__BASE_URL".to_string(), mock_gemini.uri()),
            ("LLM_API_KEY".to_string(), "dummy".to_string()),
        ],
    );

    // Wait for server to start
    sashiko.wait_ready().await;

    // 4. Trigger Review
    let client = reqwest::Client::new();
    let port = env.settings.server.port;
    let resp = client
        .post(format!("http://127.0.0.1:{}/api/submit", port))
        .json(&serde_json::json!({
            "type": "remote",
            "sha": target_sha,
            "repo": remote_url
        }))
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // 5. Poll for Completion
    let mut success = false;
    for i in 0..20 {
        tracing::info!("Polling iteration {}", i);
        let patchsets: serde_json::Value = client
            .get(format!("http://127.0.0.1:{}/api/patchsets", port))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        if let Some(items) = patchsets["items"].as_array() {
            if let Some(ps) = items.iter().find(|i| {
                let msg_id = i["message_id"].as_str().unwrap_or("");
                msg_id == target_sha || msg_id == format!("{}@sashiko.local", target_sha)
            }) {
                if ps["status"] == "Reviewed" {
                    success = true;
                    break;
                }
                if ps["status"] == "Failed" || ps["status"] == "Failed To Apply" {
                    panic!(
                        "Review failed with status: {} at iteration {}",
                        ps["status"], i
                    );
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    assert!(success, "Review timed out");

    // 6. Verify Verdict
    // First get the patchset to find the review ID
    let patchset: serde_json::Value = client
        .get(format!(
            "http://127.0.0.1:{}/api/patch?id={}",
            port, target_sha
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let reviews = patchset["reviews"].as_array().expect("No reviews found");
    assert!(!reviews.is_empty(), "Expected at least one review");
    let review_id = reviews[0]["id"].as_i64().expect("Review ID not found");

    // Get the review details
    let review_details: serde_json::Value = client
        .get(format!(
            "http://127.0.0.1:{}/api/review?id={}",
            port, review_id
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Parse the raw output (which contains the findings)
    let output_raw = review_details["output"].as_str().expect("No output field");
    let output_json: serde_json::Value =
        serde_json::from_str(output_raw).expect("Failed to parse output JSON");

    let findings = output_json["findings"]
        .as_array()
        .expect("No findings found in output");
    assert!(!findings.is_empty(), "Expected findings for double free");

    let has_double_free = findings.iter().any(|f| {
        f["message"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("double free")
    });
    assert!(has_double_free, "Expected 'double free' in findings");
}
