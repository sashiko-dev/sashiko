use sashiko::ai::gemini::GeminiClient;
use sashiko::worker::Worker;
use sashiko::worker::prompts::PromptRegistry;
use sashiko::worker::tools::ToolBox;
use serde_json::json;
use std::fs;
use tempfile::tempdir;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

#[tokio::test]
async fn test_worker_e2e_mock() {
    common::setup_tracing();
    // 1. Start Mock Server
    let mock_server = MockServer::start().await;

    // 2. Setup dependencies
    let temp_worktree = tempdir().unwrap();
    let temp_prompts = tempdir().unwrap();

    // Create a dummy review-core.md to avoid file not found warnings
    fs::write(temp_prompts.path().join("review-core.md"), "Dummy Protocol").unwrap();

    let toolbox = ToolBox::new(temp_worktree.path().to_path_buf(), None);
    let prompts = PromptRegistry::new(temp_prompts.path().to_path_buf());

    // Configure GeminiClient using builder methods to point to the mock server
    let client = Box::new(
        GeminiClient::new("gemini-pro".to_string(), None)
            .with_base_url(mock_server.uri())
            .with_api_key("dummy_key".to_string()),
    );

    let mut worker = Worker::new(
        client, toolbox, prompts, 1_000_000, // Large token limit to avoid pruning in test
        5,         // Max interactions
        0.0,       // Temperature
        None,      // No cache
    );

    // 3. Define Mock Response (Simulation of a Model Verdict)
    let model_response_text = json!({
        "analysis_trace": ["Read patch", "Analyzed logic"],
        "verdict": "LGTM",
        "findings": []
    });
    // We need to escape the JSON string for the "text" field
    let model_response_str = model_response_text.to_string();

    let mock_response_body = json!({
      "candidates": [
        {
          "content": {
            "parts": [
              {
                "text": model_response_str
              }
            ],
            "role": "model"
          },
          "finishReason": "STOP",
          "index": 0,
          "safetyRatings": []
        }
      ],
      "usageMetadata": {
        "promptTokenCount": 500,
        "candidatesTokenCount": 50,
        "totalTokenCount": 550
      }
    });

    Mock::given(method("POST"))
        .and(path_regex(r"^/v1beta/models/gemini-pro:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_response_body))
        .mount(&mock_server)
        .await;

    // 4. Prepare Dummy Patch
    let patchset = json!({
        "patches": [
            {
                "commit_id": "abc1234",
                "subject": "Fix null pointer dereference",
                "author": "Jane Doe <jane@example.com>",
                "date_string": "2023-10-27",
                "diff": "diff --git a/file.c b/file.c\nindex ...\n--- a/file.c\n+++ b/file.c\n@@ -10,6 +10,8 @@ void func(int *ptr) {\n+    if (!ptr) return;\n     *ptr = 42;\n }"
            }
        ]
    });

    // 5. Run Worker
    let result = worker.run(patchset).await.expect("Worker run failed");

    // 6. Verify
    assert!(result.error.is_none());
    assert!(result.output.is_some());

    let output = result.output.unwrap();
    assert_eq!(output["verdict"], "LGTM");
    assert_eq!(output["findings"].as_array().unwrap().len(), 0);
    assert_eq!(result.tokens_in, 500);
    assert_eq!(result.tokens_out, 50);
}
