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

#[cfg(test)]
mod tests {
    use crate::ai::{
        AiProvider, AiRequest, AiResponse, AiRole, AiUsage, ProviderCapabilities, ToolCall,
    };
    use crate::worker::{Worker, prompts::PromptRegistry, tools::ToolBox};
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    struct StatefulMockClient {
        responses: Arc<Mutex<VecDeque<anyhow::Result<AiResponse>>>>,
    }

    impl StatefulMockClient {
        fn new(responses: Vec<anyhow::Result<AiResponse>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(VecDeque::from(responses))),
            }
        }
    }

    #[async_trait]
    impl AiProvider for StatefulMockClient {
        async fn generate_content(&self, _req: AiRequest) -> anyhow::Result<AiResponse> {
            let mut responses = self.responses.lock().unwrap();

            if let Some(res) = responses.pop_front() {
                return res;
            }

            Ok(AiResponse {
                content: Some(
                    "```json\n{\"summary\": \"Fallback\", \"findings\": []}\n```".to_string(),
                ),
                tool_calls: None,
                usage: Some(AiUsage {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                    cached_tokens: None,
                }),
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            0
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    fn get_test_paths() -> (PathBuf, PathBuf) {
        let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let linux_path = root.clone();
        let prompts_path = root.join("third_party/prompts/kernel");
        (linux_path, prompts_path)
    }

    fn create_text_response(text: &str) -> anyhow::Result<AiResponse> {
        Ok(AiResponse {
            content: Some(text.to_string()),
            tool_calls: None,
            usage: Some(AiUsage {
                prompt_tokens: 10,
                completion_tokens: 10,
                total_tokens: 20,
                cached_tokens: None,
            }),
        })
    }

    fn create_tool_call_response(
        name: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<AiResponse> {
        Ok(AiResponse {
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: name.to_string(),
                function_name: name.to_string(),
                arguments: args,
                thought_signature: None,
            }]),
            usage: Some(AiUsage {
                prompt_tokens: 10,
                completion_tokens: 10,
                total_tokens: 20,
                cached_tokens: None,
            }),
        })
    }

    #[tokio::test]
    async fn test_worker_integration_sanity() {
        let _ = tracing_subscriber::fmt::try_init();
        let (linux_path, prompts_path) = get_test_paths();

        let mock_response = json!({
            "summary": "Mock summary",
            "findings": []
        });

        let client = Arc::new(StatefulMockClient::new(vec![create_text_response(
            &format!("```json\n{}\n```", mock_response),
        )]));

        let tools = ToolBox::new(linux_path, None);
        let prompts = PromptRegistry::new(prompts_path);
        let mut worker = Worker::new(client, tools, prompts, 150_000, 25, 1.0, None);

        let patchset = json!({
            "subject": "Test Patch",
            "author": "Test",
            "patches": []
        });

        let result = worker.run(patchset).await.expect("Worker run failed");
        let review = result.output.expect("No output");
        assert_eq!(review["summary"], "Mock summary");
    }

    #[tokio::test]
    async fn test_worker_tool_use() {
        let _ = tracing_subscriber::fmt::try_init();
        let (linux_path, prompts_path) = get_test_paths();

        let final_response = json!({
            "summary": "README is good",
            "findings": []
        });

        let client = Arc::new(StatefulMockClient::new(vec![
            create_tool_call_response("read_files", json!({ "files": [{ "path": "README.md" }] })),
            create_text_response(&format!("```json\n{}\n```", final_response)),
        ]));

        let tools = ToolBox::new(linux_path, None);
        let prompts = PromptRegistry::new(prompts_path);
        let mut worker = Worker::new(client, tools, prompts, 150_000, 25, 1.0, None);

        let patchset = json!({
            "subject": "Docs update",
            "author": "Test",
            "patches": []
        });

        let result = worker.run(patchset).await.expect("Worker run failed");

        assert!(
            result.history.len() >= 4,
            "History should contain at least 4 turns (User, Assistant-Call, Tool-Res, Assistant-Final)"
        );

        let tool_call_msg = &result.history[1];
        assert_eq!(tool_call_msg.role, AiRole::Assistant);
        let tool_calls = tool_call_msg
            .tool_calls
            .as_ref()
            .expect("Expected tool calls");
        assert_eq!(tool_calls[0].function_name, "read_files");

        let tool_res_msg = &result.history[2];
        assert_eq!(tool_res_msg.role, AiRole::Tool);
        assert_eq!(tool_res_msg.tool_call_id, Some("read_files".to_string()));
        let content = tool_res_msg.content.as_ref().expect("Expected content");
        let content_json: serde_json::Value = serde_json::from_str(content).expect("Valid JSON");

        let results = content_json["results"].as_array().expect("Results array");
        assert_eq!(results.len(), 1);
        let content_str = results[0]["content"].as_str().expect("Content string");
        assert!(
            content_str.contains("Sashiko"),
            "README.md content should contain 'Sashiko'"
        );

        let review = result.output.expect("No output");
        assert_eq!(review["summary"], "README is good");
    }

    #[tokio::test]
    async fn test_worker_loop_detection() {
        let _ = tracing_subscriber::fmt::try_init();
        let (linux_path, prompts_path) = get_test_paths();

        let client = Arc::new(StatefulMockClient::new(vec![
            create_tool_call_response("read_files", json!({ "files": [{ "path": "README.md" }] })),
            create_tool_call_response("read_files", json!({ "files": [{ "path": "README.md" }] })),
            create_tool_call_response("read_files", json!({ "files": [{ "path": "README.md" }] })),
            create_tool_call_response("read_files", json!({ "files": [{ "path": "README.md" }] })),
            create_tool_call_response("read_files", json!({ "files": [{ "path": "README.md" }] })),
            create_tool_call_response("read_files", json!({ "files": [{ "path": "README.md" }] })),
            create_tool_call_response("read_files", json!({ "files": [{ "path": "README.md" }] })),
            create_tool_call_response("read_files", json!({ "files": [{ "path": "README.md" }] })),
            create_tool_call_response("read_files", json!({ "files": [{ "path": "README.md" }] })),
            create_tool_call_response("read_files", json!({ "files": [{ "path": "README.md" }] })),
            create_tool_call_response("read_files", json!({ "files": [{ "path": "README.md" }] })),
            create_tool_call_response("read_files", json!({ "files": [{ "path": "README.md" }] })),
        ]));

        let tools = ToolBox::new(linux_path, None);
        let prompts = PromptRegistry::new(prompts_path);
        let mut worker = Worker::new(client, tools, prompts, 150_000, 25, 1.0, None);

        let patchset = json!({
            "subject": "Loop Test",
            "author": "Test",
            "patches": []
        });

        let result = worker
            .run(patchset)
            .await
            .expect("Worker run failed (should return Ok with error field)");

        assert!(result.output.is_none(), "Output was: {:?}", result.output);
        assert!(result.error.is_some());
        let err_msg = result.error.unwrap();
        assert!(err_msg.contains("Loop detected"));
        assert!(err_msg.contains("read_files"));
    }

    #[tokio::test]
    async fn test_worker_json_extraction_from_conversational_text() {
        let _ = tracing_subscriber::fmt::try_init();
        let (linux_path, prompts_path) = get_test_paths();

        let final_json = json!({
            "summary": "Extracted",
            "findings": []
        });

        let conversational_text = format!(
            "Hello! This is a conversational response.\n\nHere is the JSON you requested:\n\n```json\n{}\n```\n\nHope this helps!",
            final_json
        );

        let client = Arc::new(StatefulMockClient::new(vec![create_text_response(
            &conversational_text,
        )]));

        let tools = ToolBox::new(linux_path, None);
        let prompts = PromptRegistry::new(prompts_path);
        let mut worker = Worker::new(client, tools, prompts, 150_000, 25, 1.0, None);

        let patchset = json!({
            "subject": "Extraction Test",
            "author": "Test",
            "patches": []
        });

        let result = worker.run(patchset).await.expect("Worker run failed");
        let review = result.output.expect("No output extracted");
        assert_eq!(review["summary"], "Extracted");
    }
}
