#[cfg(test)]
mod integration_test;
pub mod prompts;
pub mod tools;
#[cfg(test)]
mod tools_test;

use crate::ai::gemini::{
    Content, FunctionResponse, GenAiClient, GenerateContentRequest,
    GenerateContentWithCacheRequest, GenerationConfig, Part,
};
use crate::ai::token_budget::TokenBudget;
use crate::worker::prompts::PromptRegistry;
use crate::worker::tools::ToolBox;
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tracing::{info, warn};

pub struct Worker {
    client: Box<dyn GenAiClient>,
    tools: ToolBox,
    prompts: PromptRegistry,
    history: Vec<Content>,
    max_input_words: usize,
    max_interactions: usize,
    temperature: f32,
    cache_name: Option<String>,
}

pub struct WorkerResult {
    pub output: Option<Value>,
    pub error: Option<String>,
    pub input_context: String,
    pub history: Vec<Content>,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
}

impl Worker {
    pub fn new(
        client: Box<dyn GenAiClient>,
        tools: ToolBox,
        prompts: PromptRegistry,
        max_input_words: usize,
        max_interactions: usize,
        temperature: f32,
        cache_name: Option<String>,
    ) -> Self {
        Self {
            client,
            tools,
            prompts,
            history: Vec::new(),
            max_input_words,
            max_interactions,
            temperature,
            cache_name,
        }
    }

    fn estimate_history_tokens(&self, system_instruction: &Option<Content>) -> usize {
        let mut count = 0;

        // Count system instruction
        if let Some(content) = system_instruction {
            count += self.estimate_content_tokens(content);
        }

        // Count history
        for content in &self.history {
            count += self.estimate_content_tokens(content);
        }

        count
    }

    fn estimate_content_tokens(&self, content: &Content) -> usize {
        let mut count = 0;
        for part in &content.parts {
            match part {
                Part::Text { text, .. } => {
                    count += TokenBudget::estimate_tokens(text);
                }
                Part::FunctionCall { function_call, .. } => {
                    count += TokenBudget::estimate_tokens(&function_call.name);
                    count += TokenBudget::estimate_tokens(&function_call.args.to_string());
                }
                Part::FunctionResponse { function_response } => {
                    count += TokenBudget::estimate_tokens(&function_response.name);
                    count += TokenBudget::estimate_tokens(&function_response.response.to_string());
                }
            }
        }
        count
    }

    fn prune_history(&mut self, system_instruction: &Option<Content>) {
        let limit = self.max_input_words; // Treating max_input_words as max_tokens for now
        let mut current_tokens = self.estimate_history_tokens(system_instruction);

        if current_tokens <= limit {
            return;
        }

        info!(
            "Context size ({} tokens) exceeds limit ({}). Pruning history...",
            current_tokens, limit
        );

        // Keep index 0 (Task Prompt). Prune from index 1.
        // We also want to avoid pruning the very last message if possible, but budget is strict.
        // Prune oldest messages first (after index 0).
        while current_tokens > limit && self.history.len() > 1 {
            // Remove the oldest message after the prompt.
            let removed = self.history.remove(1);
            let removed_tokens = self.estimate_content_tokens(&removed);
            current_tokens = current_tokens.saturating_sub(removed_tokens);
            info!(
                "Pruned message with {} tokens. Remaining: {}",
                removed_tokens, current_tokens
            );
        }
    }

    pub async fn run(&mut self, _patchset: Value) -> Result<WorkerResult> {
        let system_prompt = PromptRegistry::get_system_identity().to_string();
        let initial_user_message = self
            .prompts
            .get_user_task_prompt(self.cache_name.is_some())
            .await?;

        let input_context = format!(
            "System: {}\n\nUser: {}",
            system_prompt, initial_user_message
        );

        let system_content = Content {
            role: "user".to_string(),
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
        let mut total_tokens_in = 0;
        let mut total_tokens_out = 0;
        let mut total_tokens_cached = 0;
        let mut last_tool_call: Option<(String, Value)> = None;
        let mut consecutive_tool_count = 0;

        loop {
            turns += 1;
            if turns > self.max_interactions {
                return Ok(WorkerResult {
                    output: None,
                    error: Some(format!(
                        "Worker exceeded maximum turns ({})",
                        self.max_interactions
                    )),
                    input_context,
                    history: self.history.clone(),
                    tokens_in: total_tokens_in,
                    tokens_out: total_tokens_out,
                    tokens_cached: total_tokens_cached,
                });
            }

            let response_schema = json!({
                "type": "object",
                "properties": {
                    "analysis_trace": {
                        "type": "array",
                        "items": { "type": "string" }
                    },
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
                "required": ["analysis_trace", "verdict", "findings"]
            });

            // Enforce token budget by pruning
            self.prune_history(&Some(system_content.clone()));

            let tools_config = Some(vec![self.tools.get_declarations()]);
            let generation_config = Some(GenerationConfig {
                response_mime_type: Some("application/json".to_string()),
                response_schema: Some(response_schema),
                temperature: Some(self.temperature),
            });

            let resp = if let Some(cache_name) = &self.cache_name {
                let req = GenerateContentWithCacheRequest {
                    cached_content: cache_name.clone(),
                    contents: self.history.clone(),
                    tools: None, // Tools are baked into the cache
                    generation_config,
                };
                info!("Sending request to Gemini (cached: {})...", cache_name);
                let resp = self.client.generate_content_with_cache(req).await?;

                // Check for cache update from parent
                if let Some(usage) = &resp.usage_metadata {
                    if let Some(extra) = &usage.extra {
                        if let Some(new_name) = extra.get("new_cache_name").and_then(|v| v.as_str())
                        {
                            info!(
                                "Updating cache name from response: {} -> {}",
                                cache_name, new_name
                            );
                            self.cache_name = Some(new_name.to_string());
                        }
                    }
                }
                resp
            } else {
                let req = GenerateContentRequest {
                    contents: self.history.clone(),
                    tools: tools_config,
                    system_instruction: Some(system_content.clone()),
                    generation_config,
                };

                let token_count = self.estimate_history_tokens(&req.system_instruction);
                info!(
                    "Sending request to Gemini ({} estimated tokens)...",
                    token_count
                );
                self.client.generate_content(req).await?
            };

            if let Some(usage) = &resp.usage_metadata {
                total_tokens_in += usage.prompt_token_count;
                total_tokens_out += usage.candidates_token_count.unwrap_or(0);
                total_tokens_cached += usage.cached_content_token_count.unwrap_or(0);
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

                        if let Some((last_name, last_args)) = &last_tool_call {
                            if *last_name == call.name && *last_args == call.args {
                                consecutive_tool_count += 1;
                            } else {
                                consecutive_tool_count = 1;
                                last_tool_call = Some((call.name.clone(), call.args.clone()));
                            }
                        } else {
                            consecutive_tool_count = 1;
                            last_tool_call = Some((call.name.clone(), call.args.clone()));
                        }

                        if consecutive_tool_count >= 3 {
                            let error_msg = format!(
                                "Loop detected: Tool '{}' called with same arguments 3 times in a row. Terminating.",
                                call.name
                            );
                            warn!("{}", error_msg);
                            return Ok(WorkerResult {
                                output: None,
                                error: Some(error_msg),
                                input_context: input_context.clone(),
                                history: self.history.clone(),
                                tokens_in: total_tokens_in,
                                tokens_out: total_tokens_out,
                                tokens_cached: total_tokens_cached,
                            });
                        }

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

                return Ok(WorkerResult {
                    output: Some(json_val),
                    error: None,
                    input_context,
                    history: self.history.clone(),
                    tokens_in: total_tokens_in,
                    tokens_out: total_tokens_out,
                    tokens_cached: total_tokens_cached,
                });
            }
        }
    }
}
