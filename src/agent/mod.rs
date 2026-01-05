#[cfg(test)]
mod integration_test;
pub mod prompts;
pub mod tools;
#[cfg(test)]
mod tools_test;

use crate::agent::prompts::PromptRegistry;
use crate::agent::tools::ToolBox;
use crate::ai::gemini::{
    Content, FunctionResponse, GeminiClient, GenerateContentRequest, GenerationConfig, Part,
};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tracing::{info, warn};

pub struct Agent {
    client: GeminiClient,
    tools: ToolBox,
    prompts: PromptRegistry,
    history: Vec<Content>,
}

pub struct AgentResult {
    pub output: Value,
    pub input_context: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
}

impl Agent {
    pub fn new(client: GeminiClient, tools: ToolBox, prompts: PromptRegistry) -> Self {
        Self {
            client,
            tools,
            prompts,
            history: Vec::new(),
        }
    }

    pub async fn run(&mut self, patchset: Value) -> Result<AgentResult> {
        let system_prompt = self.prompts.get_system_prompt().await?;
        let review_core =
            tokio::fs::read_to_string(self.prompts.get_base_dir().join("review-core.md"))
                .await
                .unwrap_or_else(|_| "Deep dive regression analysis protocol.".to_string());

        let initial_user_message = format!(
            "Using the prompt review-prompts/review-core.md run a deep dive regression analysis of the top commit in the Linux source tree.\n\n\
             The 'top commit' to analyze is:\n\
             Subject: {}\n\
             Author: {}\n\n\
             ## Review Protocol (review-core.md)\n\
             {}",
            patchset["subject"].as_str().unwrap_or("Unknown"),
            patchset["author"].as_str().unwrap_or("Unknown"),
            review_core
        );

        let input_context = format!(
            "System: {}\n\nUser: {}",
            system_prompt, initial_user_message
        );

        let system_content = Content {
            role: "user".to_string(), // Using user role for system instruction placeholder if needed, but we use the field.
            parts: vec![Part::Text {
                text: system_prompt,
                thought_signature: None,
            }],
        };

        self.history.push(Content {
            role: "user".to_string(),
            parts: vec![Part::Text {
                text: initial_user_message,
                thought_signature: None,
            }],
        });

        let mut turns = 0;
        const MAX_TURNS: usize = 10;
        let mut total_tokens_in = 0;
        let mut total_tokens_out = 0;

        loop {
            turns += 1;
            if turns > MAX_TURNS {
                return Err(anyhow!("Agent exceeded maximum turns ({})", MAX_TURNS));
            }

            let response_schema = json!({
                "type": "object",
                "properties": {
                    "analysis_trace": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
                    "summary": { "type": "string" },
                    "score": { "type": "number" },
                    "verdict": { "type": "string" },
                    "findings": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "file": { "type": "string" },
                                "line": { "type": "integer" },
                                "severity": { "type": "string" },
                                "message": { "type": "string" },
                                "suggestion": { "type": "string" }
                            },
                            "required": ["file", "line", "severity", "message"]
                        }
                    }
                },
                "required": ["analysis_trace", "summary", "score", "verdict", "findings"]
            });

            let req = GenerateContentRequest {
                contents: self.history.clone(),
                tools: Some(vec![self.tools.get_declarations()]),
                system_instruction: Some(system_content.clone()),
                generation_config: Some(GenerationConfig {
                    response_mime_type: Some("application/json".to_string()),
                    response_schema: Some(response_schema),
                    temperature: Some(0.2),
                }),
            };

            info!("Sending request to Gemini...");
            let resp = self.client.generate_content(req).await?;

            if let Some(usage) = &resp.usage_metadata {
                total_tokens_in += usage.prompt_token_count;
                total_tokens_out += usage.candidates_token_count.unwrap_or(0);
            }

            let candidate = resp
                .candidates
                .as_ref()
                .and_then(|c| c.first())
                .ok_or_else(|| anyhow!("No candidates returned"))?;

            let content = &candidate.content;
            self.history.push(content.clone());

            // Check for function calls
            let mut function_responses = Vec::new();
            let mut has_calls = false;
            let mut final_text = String::new();

            for part in &content.parts {
                match part {
                    Part::FunctionCall {
                        function_call: call,
                        ..
                    } => {
                        has_calls = true;
                        info!("Tool Call: {} args: {}", call.name, call.args);

                        let result = match self.tools.call(&call.name, call.args.clone()).await {
                            Ok(val) => val,
                            Err(e) => {
                                warn!("Tool execution failed: {}", e);
                                json!({ "error": e.to_string() })
                            }
                        };

                        function_responses.push(Part::FunctionResponse {
                            function_response: FunctionResponse {
                                name: call.name.clone(),
                                response: result,
                            },
                        });
                    }
                    Part::Text { text, .. } => {
                        final_text.push_str(text);
                    }
                    _ => {}
                }
            }

            if has_calls {
                let response_content = Content {
                    role: "function".to_string(),
                    parts: function_responses,
                };
                self.history.push(response_content);
                // Continue loop to get model response to tool outputs
            } else {
                // Try to clean up markdown code blocks if present (some models still add them despite JSON mode)
                let clean_text = final_text.trim();
                let clean_text = if clean_text.starts_with("```json") {
                    clean_text
                        .strip_prefix("```json")
                        .unwrap_or(clean_text)
                        .strip_suffix("```")
                        .unwrap_or(clean_text)
                        .trim()
                } else if clean_text.starts_with("```") {
                    clean_text
                        .strip_prefix("```")
                        .unwrap_or(clean_text)
                        .strip_suffix("```")
                        .unwrap_or(clean_text)
                        .trim()
                } else {
                    clean_text
                };

                let json_val: Value = serde_json::from_str(clean_text).map_err(|e| {
                    anyhow!("Failed to parse JSON response: {}. Text: {}", e, final_text)
                })?;

                return Ok(AgentResult {
                    output: json_val,
                    input_context,
                    tokens_in: total_tokens_in,
                    tokens_out: total_tokens_out,
                });
            }
        }
    }
}
