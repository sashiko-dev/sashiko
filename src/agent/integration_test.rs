#[cfg(test)]
mod tests {
    use crate::agent::{Agent, prompts::PromptRegistry, tools::ToolBox};
    use crate::ai::gemini::{
        Candidate, Content, FunctionCall, GenAiClient, GenerateContentRequest,
        GenerateContentResponse, Part, UsageMetadata,
    };
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    struct StatefulMockClient {
        responses: Arc<Mutex<VecDeque<anyhow::Result<GenerateContentResponse>>>>,
    }

    impl StatefulMockClient {
        fn new(responses: Vec<anyhow::Result<GenerateContentResponse>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(VecDeque::from(responses))),
            }
        }
    }

    #[async_trait]
    impl GenAiClient for StatefulMockClient {
        async fn generate_content(
            &self,
            _req: GenerateContentRequest,
        ) -> anyhow::Result<GenerateContentResponse> {
            let mut responses = self.responses.lock().unwrap();
            
            // Check if we have a response queued
            if let Some(res) = responses.pop_front() {
                return res;
            }

            // Fallback if we run out of responses (shouldn't happen in well-defined tests)
            // Return a generic "Stop" response to avoid infinite loops if the test is buggy
            Ok(GenerateContentResponse {
                candidates: Some(vec![Candidate {
                    content: Content {
                        role: "model".to_string(),
                        parts: vec![Part::Text {
                            text: "```json\n{\"summary\": \"Fallback\", \"score\": 0, \"verdict\": \"Skip\", \"findings\": [], \"analysis_trace\": []}
```".to_string(),
                            thought_signature: None,
                        }],
                    },
                    finish_reason: Some("STOP".to_string()),
                }]),
                usage_metadata: Some(UsageMetadata {
                    prompt_token_count: 0,
                    candidates_token_count: Some(0),
                    total_token_count: 0,
                    extra: None,
                }),
            })
        }
    }

    fn get_test_paths() -> (PathBuf, PathBuf) {
        let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let linux_path = root.join("linux");
        let prompts_path = root.join("review-prompts");
        (linux_path, prompts_path)
    }

    fn create_text_response(text: &str) -> anyhow::Result<GenerateContentResponse> {
        Ok(GenerateContentResponse {
            candidates: Some(vec![Candidate {
                content: Content {
                    role: "model".to_string(),
                    parts: vec![Part::Text {
                        text: text.to_string(),
                        thought_signature: None,
                    }],
                },
                finish_reason: Some("STOP".to_string()),
            }]),
            usage_metadata: Some(UsageMetadata {
                prompt_token_count: 10,
                candidates_token_count: Some(10),
                total_token_count: 20,
                extra: None,
            }),
        })
    }

    fn create_tool_call_response(name: &str, args: serde_json::Value) -> anyhow::Result<GenerateContentResponse> {
        Ok(GenerateContentResponse {
            candidates: Some(vec![Candidate {
                content: Content {
                    role: "model".to_string(),
                    parts: vec![Part::FunctionCall {
                        function_call: FunctionCall {
                            name: name.to_string(),
                            args,
                        },
                        thought_signature: Some("I need to check the file.".to_string()),
                    }],
                },
                finish_reason: Some("STOP".to_string()),
            }]),
            usage_metadata: Some(UsageMetadata {
                prompt_token_count: 10,
                candidates_token_count: Some(10),
                total_token_count: 20,
                extra: None,
            }),
        })
    }

    #[tokio::test]
    async fn test_agent_integration_sanity() {
        let _ = tracing_subscriber::fmt::try_init();
        let (linux_path, prompts_path) = get_test_paths();

        let mock_response = json!({
            "analysis_trace": ["Trace 1"],
            "summary": "Mock summary",
            "score": 10,
            "verdict": "Pass",
            "findings": []
        });
        
        let client = Box::new(StatefulMockClient::new(vec![
            create_text_response(&format!("```json\n{}\n```", mock_response))
        ]));
        
        let tools = ToolBox::new(linux_path, prompts_path);
        let prompts = PromptRegistry::new(PathBuf::from("review-prompts"));
        let mut agent = Agent::new(client, tools, prompts, 150_000);

        let patchset = json!({
            "subject": "Test Patch",
            "author": "Test",
            "patches": []
        });

        let result = agent.run(patchset).await.expect("Agent run failed");
        let review = result.output.expect("No output");
        assert_eq!(review["summary"], "Mock summary");
    }

    #[tokio::test]
    async fn test_agent_tool_use() {
        let _ = tracing_subscriber::fmt::try_init();
        let (linux_path, prompts_path) = get_test_paths();

        // Sequence of responses:
        // 1. Tool call: read_file("README")
        // 2. Final JSON response (after receiving tool output)
        
        let final_response = json!({
            "analysis_trace": ["Read README", "Analyzed"],
            "summary": "README is good",
            "score": 5,
            "verdict": "Pass",
            "findings": []
        });

        let client = Box::new(StatefulMockClient::new(vec![
            create_tool_call_response("read_file", json!({"path": "README"})),
            create_text_response(&format!("```json\n{}\n```", final_response))
        ]));

        let tools = ToolBox::new(linux_path, prompts_path);
        let prompts = PromptRegistry::new(PathBuf::from("review-prompts"));
        let mut agent = Agent::new(client, tools, prompts, 150_000);

        let patchset = json!({
            "subject": "Docs update",
            "author": "Test",
            "patches": []
        });

        let result = agent.run(patchset).await.expect("Agent run failed");
        
        // Verify history has the tool call and response
        // History: 
        // 0: User (System+Prompt) - Handled by Agent setup but history only contains what's pushed.
        // In Agent::run:
        // history[0] = User message (Task)
        // history[1] = Model response (Tool Call)
        // history[2] = Function response (Tool Output)
        // history[3] = Model response (Final JSON)
        
        assert!(result.history.len() >= 4, "History should contain at least 4 turns (User, Model-Call, Function-Res, Model-Final)");
        
        let tool_call = &result.history[1];
        if let Part::FunctionCall { function_call, .. } = &tool_call.parts[0] {
            assert_eq!(function_call.name, "read_file");
        } else {
            panic!("Expected tool call in history[1]");
        }

        let tool_res = &result.history[2];
        if let Part::FunctionResponse { function_response } = &tool_res.parts[0] {
            assert_eq!(function_response.name, "read_file");
            // Verify content is from the actual README file on disk
            let content_str = function_response.response["content"].as_str().unwrap();
            assert!(content_str.contains("Linux kernel"), "README content should contain 'Linux kernel'");
        } else {
            panic!("Expected function response in history[2]");
        }

        let review = result.output.expect("No output");
        assert_eq!(review["summary"], "README is good");
    }
}