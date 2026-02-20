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

    struct MultiTurnMockClient {
        responses: Arc<Mutex<VecDeque<anyhow::Result<AiResponse>>>>,
    }

    impl MultiTurnMockClient {
        fn new(responses: Vec<anyhow::Result<AiResponse>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(VecDeque::from(responses))),
            }
        }
    }

    #[async_trait]
    impl AiProvider for MultiTurnMockClient {
        async fn generate_content(&self, _req: AiRequest) -> anyhow::Result<AiResponse> {
            let mut responses = self.responses.lock().unwrap();
            if let Some(res) = responses.pop_front() {
                return res;
            }
            panic!("Mock client ran out of responses");
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

    fn create_tool_response(name: &str, args: serde_json::Value) -> anyhow::Result<AiResponse> {
        Ok(AiResponse {
            content: None,
            thought: None,
            tool_calls: Some(vec![ToolCall {
                id: format!("call_{}", name),
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

    fn get_test_paths() -> (PathBuf, PathBuf) {
        let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let linux_path = root.clone();
        let prompts_path = root.join("third_party/prompts/kernel");
        (linux_path, prompts_path)
    }

    #[tokio::test]
    async fn test_worker_multi_turn_full_flow() {
        let (linux_path, prompts_path) = get_test_paths();

        let client = Arc::new(MultiTurnMockClient::new(vec![
            // 1. Exploration Stage
            create_tool_response(
                "cmd_submit_exploration",
                json!({
                    "hypotheses": [{"problem_description": "Possible NULL deref", "potential_impact": "Panic"}],
                    "exploration_complete": true
                }),
            ),
            // 2. Verification Stage
            create_tool_response(
                "cmd_submit_verification",
                json!({
                    "verifications": [{"evidence": "Path X leads to NULL", "suggestion": "Add check", "is_confirmed": true}],
                    "verification_complete": true
                }),
            ),
            // 3. Reporting Stage
            create_tool_response(
                "cmd_submit_report",
                json!({
                    "findings": [{
                        "problem": "Confirmed NULL deref",
                        "suggestion": "Fix it",
                        "severity_explanation": "HOT path",
                        "severity": "High"
                    }],
                    "summary": "Found one bug",
                    "review_inline": "commit 123
Author: Me

Summary

> diff

Fix it"
                }),
            ),
        ]));

        let mut worker = Worker::new(
            client,
            ToolBox::new(linux_path, None),
            PromptRegistry::new(prompts_path),
            WorkerConfig {
                max_input_tokens: 1000,
                max_interactions: 10,
                temperature: 0.0,
                cache_name: None,
                custom_prompt: None,
                series_range: None,
            },
        );

        let result = worker.run(json!({"patches": []})).await.unwrap();
        let output = result.output.unwrap();

        assert_eq!(output["summary"], "Found one bug");
        assert_eq!(output["findings"][0]["severity"], "High");
    }

    #[tokio::test]
    async fn test_worker_multi_turn_early_exit() {
        let (linux_path, prompts_path) = get_test_paths();

        let client = Arc::new(MultiTurnMockClient::new(vec![
            // 1. Exploration Stage - No findings
            create_tool_response(
                "cmd_submit_exploration",
                json!({
                    "hypotheses": [],
                    "exploration_complete": true
                }),
            ),
            // 2. Reporting Stage - Generates report directly
            create_tool_response(
                "cmd_submit_report",
                json!({
                    "findings": [],
                    "summary": "Clean patch",
                    "review_inline": "commit 123
Author: Me

Summary

Clean!"
                }),
            ),
        ]));

        let mut worker = Worker::new(
            client,
            ToolBox::new(linux_path, None),
            PromptRegistry::new(prompts_path),
            WorkerConfig {
                max_input_tokens: 1000,
                max_interactions: 10,
                temperature: 0.0,
                cache_name: None,
                custom_prompt: None,
                series_range: None,
            },
        );

        let result = worker.run(json!({"patches": []})).await.unwrap();
        let output = result.output.unwrap();

        assert_eq!(output["summary"], "Clean patch");
        assert!(output["findings"].as_array().unwrap().is_empty());
    }
}
