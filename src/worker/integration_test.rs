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
    use crate::ai::{AiProvider, AiRequest, AiResponse, AiUsage, ProviderCapabilities, ToolCall};
    use crate::worker::{Worker, WorkerConfig, prompts::PromptRegistry, tools::ToolBox};
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
                thought: None,
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

    fn create_tool_call_response(
        name: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<AiResponse> {
        Ok(AiResponse {
            content: None,
            thought: None,
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

        let final_report = json!({
            "summary": "Mock summary",
            "findings": [],
            "review_inline": "commit 123\nAuthor: Me\n\nSummary\n\nClean!"
        });

        let client = Arc::new(StatefulMockClient::new(vec![
            // Exploration
            create_tool_call_response(
                "cmd_submit_results",
                json!({
                    "json_text": json!({
                        "hypotheses": [],
                        "exploration_complete": true
                    }).to_string()
                }),
            ),
            // Reporting
            create_tool_call_response(
                "cmd_submit_results",
                json!({
                    "json_text": final_report.to_string()
                }),
            ),
        ]));

        let tools = ToolBox::new(linux_path, None);
        let prompts = PromptRegistry::new(prompts_path);
        let mut worker = Worker::new(
            client,
            tools,
            prompts,
            WorkerConfig {
                max_input_tokens: 150_000,
                max_interactions: 25,
                temperature: 1.0,
                cache_name: None,
                custom_prompt: None,
                series_range: None,
            },
        );

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

        let final_report = json!({
            "summary": "README is good",
            "findings": [],
            "review_inline": "commit 123\nAuthor: Me\n\nREADME is good\n\n> diff"
        });

        let client = Arc::new(StatefulMockClient::new(vec![
            // Turn 1: Research
            create_tool_call_response("read_files", json!({ "files": [{ "path": "README.md" }] })),
            // Turn 2: Exploration results
            create_tool_call_response(
                "cmd_submit_results",
                json!({
                    "json_text": json!({
                        "hypotheses": [],
                        "exploration_complete": true
                    }).to_string()
                }),
            ),
            // Turn 3: Final Report
            create_tool_call_response(
                "cmd_submit_results",
                json!({
                    "json_text": final_report.to_string()
                }),
            ),
        ]));

        let tools = ToolBox::new(linux_path, None);
        let prompts = PromptRegistry::new(prompts_path);
        let mut worker = Worker::new(
            client,
            tools,
            prompts,
            WorkerConfig {
                max_input_tokens: 150_000,
                max_interactions: 25,
                temperature: 1.0,
                cache_name: None,
                custom_prompt: None,
                series_range: None,
            },
        );

        let patchset = json!({
            "subject": "Docs update",
            "author": "Test",
            "patches": []
        });

        let result = worker.run(patchset).await.expect("Worker run failed");
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
        ]));

        let tools = ToolBox::new(linux_path, None);
        let prompts = PromptRegistry::new(prompts_path);
        let mut worker = Worker::new(
            client,
            tools,
            prompts,
            WorkerConfig {
                max_input_tokens: 150_000,
                max_interactions: 25,
                temperature: 1.0,
                cache_name: None,
                custom_prompt: None,
                series_range: None,
            },
        );

        let patchset = json!({
            "subject": "Loop Test",
            "author": "Test",
            "patches": []
        });

        let result = worker.run(patchset).await.expect("Worker run failed");

        assert!(result.history.iter().any(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.contains("Loop detected"))
        }));
    }

    #[tokio::test]
    async fn test_worker_json_extraction_from_conversational_text() {
        let _ = tracing_subscriber::fmt::try_init();
        let (linux_path, prompts_path) = get_test_paths();

        let final_report = json!({
            "summary": "Extracted",
            "findings": [],
            "review_inline": "commit 123\nAuthor: Me\n\nSummary\n\nClean!"
        });

        let client = Arc::new(StatefulMockClient::new(vec![
            // Exploration
            create_tool_call_response(
                "cmd_submit_results",
                json!({
                    "json_text": json!({
                        "hypotheses": [],
                        "exploration_complete": true
                    }).to_string()
                }),
            ),
            // Reporting
            create_tool_call_response(
                "cmd_submit_results",
                json!({
                    "json_text": final_report.to_string()
                }),
            ),
        ]));

        let tools = ToolBox::new(linux_path, None);
        let prompts = PromptRegistry::new(prompts_path);
        let mut worker = Worker::new(
            client,
            tools,
            prompts,
            WorkerConfig {
                max_input_tokens: 150_000,
                max_interactions: 25,
                temperature: 1.0,
                cache_name: None,
                custom_prompt: None,
                series_range: None,
            },
        );

        let patchset = json!({
            "subject": "Extraction Test",
            "author": "Test",
            "patches": []
        });

        let result = worker.run(patchset).await.expect("Worker run failed");
        let review = result.output.expect("No output extracted");
        assert_eq!(review["summary"], "Extracted");
    }

    #[tokio::test]
    async fn test_worker_custom_prompt() {
        let _ = tracing_subscriber::fmt::try_init();
        let (linux_path, prompts_path) = get_test_paths();

        let final_report = json!({
            "summary": "Mock summary",
            "findings": [],
            "review_inline": "commit 123\nAuthor: Me\n\nSummary\n\nClean!"
        });

        let client = Arc::new(StatefulMockClient::new(vec![
            create_tool_call_response(
                "cmd_submit_results",
                json!({
                    "json_text": json!({
                        "hypotheses": [],
                        "exploration_complete": true
                    }).to_string()
                }),
            ),
            create_tool_call_response(
                "cmd_submit_results",
                json!({
                    "json_text": final_report.to_string()
                }),
            ),
        ]));

        let tools = ToolBox::new(linux_path, None);
        let prompts = PromptRegistry::new(prompts_path);

        let custom_prompt = "IMPORTANT: Focus on security vulnerabilities.";
        let mut worker = Worker::new(
            client,
            tools,
            prompts,
            WorkerConfig {
                max_input_tokens: 150_000,
                max_interactions: 25,
                temperature: 1.0,
                cache_name: None,
                custom_prompt: Some(custom_prompt.to_string()),
                series_range: None,
            },
        );

        let patchset = json!({
            "subject": "Test Patch",
            "author": "Test",
            "patches": [
                {
                    "subject": "Test Patch",
                    "author": "Test",
                    "git_show": "diff --git a/file b/file\nindex 0000000..1111111 100644\n--- a/file\n+++ b/file\n@@ -0,0 +1 @@\n+test"
                }
            ]
        });

        let result = worker.run(patchset).await.expect("Worker run failed");

        let initial_message = &result.history[0];
        if let Some(text) = &initial_message.content {
            assert!(
                text.contains(custom_prompt),
                "User prompt should contain custom prompt"
            );
        } else {
            panic!("Expected content in history[0]");
        }
    }
}
