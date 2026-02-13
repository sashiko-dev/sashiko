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
mod integration_test;
pub mod prompts;
pub mod tools;
#[cfg(test)]
mod tools_test;

use crate::ai::{AiMessage, AiProvider, AiRequest, AiResponseFormat, AiRole};
use crate::worker::prompts::PromptRegistry;
use crate::worker::tools::ToolBox;
use anyhow::Result;
use regex::Regex;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::{debug, warn};

pub struct Worker {
    provider: Arc<dyn AiProvider>,
    tools: ToolBox,
    prompts: PromptRegistry,
    history: Vec<AiMessage>,
    max_input_tokens: usize,
    max_interactions: usize,
    temperature: f32,
    cache_name: Option<String>,
}

pub struct WorkerResult {
    pub output: Option<Value>,
    pub error: Option<String>,
    pub input_context: String,
    pub history: Vec<AiMessage>,
    pub history_before_pruning: Vec<AiMessage>,
    pub history_after_pruning: Vec<AiMessage>,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
}

fn validate_inline_format(content: &str) -> Result<(), String> {
    // Check for markdown headers (lines starting with '#')
    if content.lines().any(|l| l.trim_start().starts_with("#")) {
        return Err("The `review_inline` field contains Markdown headers (lines starting with '#'). It must be plain text as per `inline-template.md`.".to_string());
    }

    // Check for quoting (lines starting with '>')
    if !content.lines().any(|l| l.trim_start().starts_with(">")) {
        return Err("The `review_inline` field does not appear to quote any code or context using '>'. Please follow the quoting style in `inline-template.md`.".to_string());
    }

    Ok(())
}

impl Worker {
    pub fn new(
        provider: Arc<dyn AiProvider>,
        tools: ToolBox,
        prompts: PromptRegistry,
        max_input_tokens: usize,
        max_interactions: usize,
        temperature: f32,
        cache_name: Option<String>,
    ) -> Self {
        Self {
            provider,
            tools,
            prompts,
            history: Vec::new(),
            max_input_tokens,
            max_interactions,
            temperature,
            cache_name,
        }
    }

    fn estimate_history_tokens(&self, system_message: &Option<AiMessage>) -> usize {
        let mut messages = Vec::new();
        if let Some(msg) = system_message {
            messages.push(msg.clone());
        }
        messages.extend(self.history.clone());

        let request = AiRequest {
            messages,
            tools: Some(self.tools.get_declarations_generic()),
            temperature: Some(self.temperature),
            preloaded_context: self.cache_name.clone(),
            response_format: Some(AiResponseFormat::Json { schema: None }),
        };

        self.provider.estimate_tokens(&request)
    }

    fn prune_history(
        &mut self,
        system_message: &Option<AiMessage>,
    ) -> (Vec<AiMessage>, Vec<AiMessage>) {
        let before_pruning = self.history.clone();
        let limit = self.max_input_tokens;
        let mut current_tokens = self.estimate_history_tokens(system_message);

        debug!(
            "Pruning check: {} tokens vs limit {}",
            current_tokens, limit
        );

        if current_tokens <= limit {
            return (before_pruning, self.history.clone());
        }

        // Keep index 0 (Task Prompt). Prune from index 1.
        while current_tokens > limit && self.history.len() > 1 {
            // Remove the oldest message after the prompt.
            let removed_idx = 1;
            let _removed = self.history.remove(removed_idx);

            current_tokens = self.estimate_history_tokens(system_message);
            debug!("Pruned message. New total: {}", current_tokens);
        }

        (before_pruning, self.history.clone())
    }

    fn validate_review_inline(&self, content: &str) -> Result<(), String> {
        validate_inline_format(content)
    }

    pub async fn run(&mut self, patchset: Value) -> Result<WorkerResult> {
        let system_prompt = PromptRegistry::get_system_identity().to_string();
        let initial_user_message = self
            .prompts
            .get_user_task_prompt(self.cache_name.is_some())
            .await?;

        // Extract and append patch content
        let mut patch_content = String::new();

        if let Some(patches) = patchset["patches"].as_array() {
            for p in patches {
                patch_content.push_str("```\n");

                if let Some(show) = p["git_show"].as_str() {
                    patch_content.push_str(show);
                } else {
                    let subject = p["subject"].as_str().unwrap_or("No Subject");
                    let author = p["author"].as_str().unwrap_or("Unknown");
                    let date = p["date_string"].as_str().unwrap_or("");
                    let commit_id = p["commit_id"]
                        .as_str()
                        .unwrap_or("0000000000000000000000000000000000000000");

                    patch_content.push_str(&format!("commit {}\n", commit_id));
                    patch_content.push_str(&format!("Author: {}\n", author));
                    if !date.is_empty() {
                        patch_content.push_str(&format!("Date:   {}\n", date));
                    }
                    patch_content.push('\n');
                    // Indent subject by 4 spaces
                    patch_content.push_str(&format!("    {}\n\n", subject));
                }

                patch_content.push_str("\n```\n\n");
            }
        }

        let input_context = format!(
            "System: {}\n\nUser: {}",
            system_prompt, initial_user_message
        );

        let system_message = AiMessage {
            role: AiRole::System,
            content: Some(system_prompt),
            tool_calls: None,
            tool_call_id: None,
        };

        let initial_message = AiMessage {
            role: AiRole::User,
            content: Some(initial_user_message),
            tool_calls: None,
            tool_call_id: None,
        };
        self.history.push(initial_message);

        let mut turns = 0;
        let mut total_tokens_in = 0;
        let mut total_tokens_out = 0;
        let mut total_tokens_cached = 0;
        let mut session_tool_history: Vec<(String, Value)> = Vec::new();

        // Track the final state of history for the last turn
        let mut final_history_before_pruning = Vec::new();
        let mut final_history_after_pruning = Vec::new();

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
                    history_before_pruning: final_history_before_pruning,
                    history_after_pruning: final_history_after_pruning,
                    tokens_in: total_tokens_in,
                    tokens_out: total_tokens_out,
                    tokens_cached: total_tokens_cached,
                });
            }

            let response_schema = json!({
                "type": "object",
                "properties": {
                    "summary": { "type": "string", "description": "High-level summary of the original change being reviewed." },
                    "review_inline": {
                        "type": "string",
                        "description": "The full content of the inline review (formatted according to inline-template.md). This MUST be populated if there are any findings."
                    },
                    "findings": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "severity": {
                                    "type": "string",
                                    "enum": ["Low", "Medium", "High", "Critical"],
                                    "description": "Severity of the finding."
                                },
                                "severity_explanation": {
                                    "type": "string",
                                    "description": "Concise explanation (e.g. 'memory leak on a hot path' or 'use after free can cause a memory corruption')."
                                },
                                "problem": {
                                    "type": "string",
                                    "description": "Description of the problem."
                                },
                                "suggestion": {
                                    "type": "string",
                                    "description": "Suggested fix."
                                }
                            },
                            "required": ["severity", "severity_explanation", "problem"]
                        }
                    }
                },
                "required": ["summary", "findings"]
            });

            // Enforce token budget by pruning
            let (before, after) = self.prune_history(&Some(system_message.clone()));
            final_history_before_pruning = before;
            final_history_after_pruning = after;

            let request = AiRequest {
                messages: {
                    let mut msgs = Vec::new();
                    msgs.push(system_message.clone());
                    msgs.extend(self.history.clone());
                    msgs
                },
                tools: Some(self.tools.get_declarations_generic()),
                temperature: Some(self.temperature),
                preloaded_context: self.cache_name.clone(),
                response_format: Some(AiResponseFormat::Json {
                    schema: Some(response_schema),
                }),
            };

            let resp = match self.provider.generate_content(request).await {
                Ok(resp) => {
                    // Check for cache update from provider (currently via metadata extra)
                    if resp.usage.is_some() {
                        // TODO: Add generic way to handle provider-specific session updates
                    }
                    resp
                }
                Err(e) => {
                    return Ok(WorkerResult {
                        output: None,
                        error: Some(format!("AI Provider Error: {}", e)),
                        input_context,
                        history: self.history.clone(),
                        history_before_pruning: final_history_before_pruning,
                        history_after_pruning: final_history_after_pruning,
                        tokens_in: total_tokens_in,
                        tokens_out: total_tokens_out,
                        tokens_cached: total_tokens_cached,
                    });
                }
            };

            if let Some(usage) = &resp.usage {
                total_tokens_in += usage.prompt_tokens as u32;
                total_tokens_out += usage.completion_tokens as u32;
                total_tokens_cached += usage.cached_tokens.unwrap_or(0) as u32;
            }

            let assistant_message = AiMessage {
                role: AiRole::Assistant,
                content: resp.content.clone(),
                tool_calls: resp.tool_calls.clone(),
                tool_call_id: None,
            };
            self.history.push(assistant_message);

            // Check for tool calls
            if let Some(tool_calls) = resp.tool_calls {
                let mut tool_responses = Vec::new();
                for call in tool_calls {
                    debug!("Tool Call: {} args: {}", call.function_name, call.arguments);

                    // Loop Detection & Prevention
                    let same_call_count = session_tool_history
                        .iter()
                        .filter(|(n, a)| *n == call.function_name && *a == call.arguments)
                        .count();

                    session_tool_history.push((call.function_name.clone(), call.arguments.clone()));

                    if same_call_count > 0 {
                        if same_call_count >= 2 {
                            let error_msg = format!(
                                "Error: Loop detected. You have already called tool '{}' with these exact arguments {} times. Please stop repeating yourself and proceed to the next step.",
                                call.function_name,
                                same_call_count + 1
                            );
                            warn!("{}", error_msg);

                            if same_call_count >= 10 {
                                return Ok(WorkerResult {
                                    output: None,
                                    error: Some(format!(
                                        "Terminating due to persistent tool loop: {}",
                                        error_msg
                                    )),
                                    input_context: input_context.clone(),
                                    history: self.history.clone(),
                                    history_before_pruning: final_history_before_pruning,
                                    history_after_pruning: final_history_after_pruning,
                                    tokens_in: total_tokens_in,
                                    tokens_out: total_tokens_out,
                                    tokens_cached: total_tokens_cached,
                                });
                            }

                            tool_responses.push(AiMessage {
                                role: AiRole::Tool,
                                content: Some(json!({ "error": error_msg }).to_string()),
                                tool_calls: None,
                                tool_call_id: Some(call.id.clone()),
                            });
                            continue;
                        }
                    }

                    let result = match self
                        .tools
                        .call(&call.function_name, call.arguments.clone())
                        .await
                    {
                        Ok(val) => val.to_string(),
                        Err(e) => {
                            debug!("Tool execution failed: {}", e);
                            json!({ "error": e.to_string() }).to_string()
                        }
                    };

                    tool_responses.push(AiMessage {
                        role: AiRole::Tool,
                        content: Some(result),
                        tool_calls: None,
                        tool_call_id: Some(call.id.clone()),
                    });
                }
                self.history.extend(tool_responses);
                // Continue loop to get model response to tool outputs
            } else if let Some(final_text) = resp.content {
                // Try to clean up markdown code blocks if present
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

                let json_val: Value = match serde_json::from_str(clean_text) {
                    Ok(v) => v,
                    Err(e) => {
                        // Fallback: try to find the first '{' and last '}'
                        let re = Regex::new(r"(?s)\{.*\}").unwrap();
                        if let Some(mat) = re.find(final_text.as_str()) {
                            let possible_json = mat.as_str();
                            match serde_json::from_str(possible_json) {
                                Ok(v) => v,
                                Err(_) => {
                                    return Ok(WorkerResult {
                                        output: None,
                                        error: Some(format!(
                                            "Failed to parse JSON response (even after extraction): {}. Text: {}",
                                            e, final_text
                                        )),
                                        input_context,
                                        history: self.history.clone(),
                                        history_before_pruning: final_history_before_pruning,
                                        history_after_pruning: final_history_after_pruning,
                                        tokens_in: total_tokens_in,
                                        tokens_out: total_tokens_out,
                                        tokens_cached: total_tokens_cached,
                                    });
                                }
                            }
                        } else {
                            return Ok(WorkerResult {
                                output: None,
                                error: Some(format!(
                                    "Failed to parse JSON response: {}. Text: {}",
                                    e, final_text
                                )),
                                input_context,
                                history: self.history.clone(),
                                history_before_pruning: final_history_before_pruning,
                                history_after_pruning: final_history_after_pruning,
                                tokens_in: total_tokens_in,
                                tokens_out: total_tokens_out,
                                tokens_cached: total_tokens_cached,
                            });
                        }
                    }
                };

                // Validate review_inline format if findings are present
                let findings_count = json_val
                    .get("findings")
                    .and_then(|v| v.as_array())
                    .map(|v| v.len())
                    .unwrap_or(0);

                if findings_count > 0 {
                    let review_inline = json_val.get("review_inline").and_then(|v| v.as_str());

                    if review_inline.is_none() || review_inline.unwrap().trim().is_empty() {
                        let error_msg = "Validation Error: 'findings' were detected but 'review_inline' is missing or empty. You must provide the inline review in 'review_inline' when reporting findings. Please retry.".to_string();
                        warn!("{}", error_msg);

                        self.history.push(AiMessage {
                            role: AiRole::User,
                            content: Some(error_msg),
                            tool_calls: None,
                            tool_call_id: None,
                        });
                        continue;
                    }

                    if let Err(e) = self.validate_review_inline(review_inline.unwrap()) {
                        let error_msg = format!(
                            "Validation Error: {}. Please retry and strictly follow `inline-template.md`.",
                            e
                        );
                        warn!("{}", error_msg);

                        self.history.push(AiMessage {
                            role: AiRole::User,
                            content: Some(error_msg),
                            tool_calls: None,
                            tool_call_id: None,
                        });
                        continue;
                    }
                }

                return Ok(WorkerResult {
                    output: Some(json_val),
                    error: None,
                    input_context,
                    history: self.history.clone(),
                    history_before_pruning: final_history_before_pruning,
                    history_after_pruning: final_history_after_pruning,
                    tokens_in: total_tokens_in,
                    tokens_out: total_tokens_out,
                    tokens_cached: total_tokens_cached,
                });
            } else {
                return Ok(WorkerResult {
                    output: None,
                    error: Some("AI returned no content or tool calls".to_string()),
                    input_context,
                    history: self.history.clone(),
                    history_before_pruning: final_history_before_pruning,
                    history_after_pruning: final_history_after_pruning,
                    tokens_in: total_tokens_in,
                    tokens_out: total_tokens_out,
                    tokens_cached: total_tokens_cached,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_inline_format_valid() {
        let content =
            "Commit 123\n\n> diff --git a/file b/file\n> index 123..456\n\nThis looks bad.";
        assert!(validate_inline_format(content).is_ok());
    }

    #[test]
    fn test_validate_inline_format_markdown_headers() {
        let content = "# Summary\n\n> diff --git ...";
        assert!(validate_inline_format(content).is_err());
    }

    #[test]
    fn test_validate_inline_format_no_quoting() {
        let content = "This looks bad.\nNo diff here.";
        assert!(validate_inline_format(content).is_err());
    }

    #[test]
    fn test_validate_inline_format_headers_in_diff_ok() {
        let content = "> #include <stdio.h>\n> void main() {}";
        assert!(validate_inline_format(content).is_ok());
    }
}
