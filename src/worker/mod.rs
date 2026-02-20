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
#[cfg(test)]
mod multi_turn_test;
pub mod prompts;
pub mod tools;
#[cfg(test)]
mod tools_test;

use crate::ai::{AiMessage, AiProvider, AiRequest, AiResponseFormat, AiRole};
use crate::worker::prompts::{PromptRegistry, ReviewStage};
use crate::worker::tools::ToolBox;
use anyhow::Result;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::{debug, info};

pub struct Worker {
    provider: Arc<dyn AiProvider>,
    tools: ToolBox,
    prompts: PromptRegistry,
    history: Vec<AiMessage>,
    max_input_tokens: usize,
    max_interactions: usize,
    temperature: f32,
    cache_name: Option<String>,
    custom_prompt: Option<String>,
    series_range: Option<String>,
}

use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct PatchInput {
    pub index: i64,
    pub diff: String,
    pub subject: Option<String>,
    pub author: Option<String>,
    pub date: Option<i64>,
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub commit_id: Option<String>,
}

pub fn calculate_series_range(
    patches: &[PatchInput],
    patches_to_review: &[PatchInput],
    patch_shas: &std::collections::HashMap<i64, String>,
    baseline_sha: &str,
) -> Option<String> {
    if patches.is_empty() {
        return None;
    }

    let max_patch_index = patches.iter().map(|p| p.index).max().unwrap_or(0);
    let is_last_patch_review =
        patches_to_review.len() == 1 && patches_to_review[0].index == max_patch_index;

    if is_last_patch_review {
        None
    } else {
        patches
            .iter()
            .map(|p| p.index)
            .max()
            .and_then(|max_idx| {
                patches
                    .iter()
                    .find(|p| p.index == max_idx)
                    .and_then(|p| p.commit_id.clone())
                    .or_else(|| patch_shas.get(&max_idx).cloned())
            })
            .map(|end_sha| format!("{}..{}", baseline_sha, end_sha))
    }
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

    // Check for markdown code blocks (lines starting with '```')
    if content.lines().any(|l| l.trim_start().starts_with("```")) {
        return Err("The `review_inline` field contains Markdown code blocks ('```'). It must be plain text as per `inline-template.md`.".to_string());
    }

    // Check for quoting (lines starting with '>')
    if !content.lines().any(|l| l.trim_start().starts_with(">")) {
        return Err("The `review_inline` field does not appear to quote any code or context using '>'. Please follow the quoting style in `inline-template.md`.".to_string());
    }

    // Check for Commit Header (must appear in the first few lines)
    // We look for "commit " at the start of a line.
    let has_commit_header = content
        .lines()
        .take(20) // Check first 20 lines to be safe (in case of long preamble)
        .any(|l| l.trim_start().to_lowercase().starts_with("commit "));

    if !has_commit_header {
        return Err("The `review_inline` field is missing the 'commit <hash>' header. Please start with the commit details (Commit, Author, Subject) as per `inline-template.md`.".to_string());
    }

    // Check for comments (lines that are NOT quoted and NOT headers)
    // We want to ensure the AI actually wrote some feedback, not just pasted the diff.
    let has_comments = content.lines().any(|l| {
        let trimmed = l.trim();
        if trimmed.is_empty() {
            return false;
        }
        if trimmed.starts_with(">") {
            return false;
        }
        // Ignore standard headers
        let lower = trimmed.to_lowercase();
        if lower.starts_with("commit ")
            || lower.starts_with("author:")
            || lower.starts_with("date:")
            || lower.starts_with("link:")
        {
            return false;
        }
        true
    });

    if !has_comments {
        return Err("The `review_inline` field appears to lack any comments or summary. You must include a summary and interspersed comments explaining the findings.".to_string());
    }

    Ok(())
}

pub struct WorkerConfig {
    pub max_input_tokens: usize,
    pub max_interactions: usize,
    pub temperature: f32,
    pub cache_name: Option<String>,
    pub custom_prompt: Option<String>,
    pub series_range: Option<String>,
}

impl Worker {
    pub fn new(
        provider: Arc<dyn AiProvider>,
        tools: ToolBox,
        prompts: PromptRegistry,
        config: WorkerConfig,
    ) -> Self {
        Self {
            provider,
            tools,
            prompts,
            history: Vec::new(),
            max_input_tokens: config.max_input_tokens,
            max_interactions: config.max_interactions,
            temperature: config.temperature,
            cache_name: config.cache_name,
            custom_prompt: config.custom_prompt,
            series_range: config.series_range,
        }
    }

    fn estimate_history_tokens(&self, system_message: &Option<AiMessage>) -> usize {
        let mut messages = Vec::new();
        if let Some(msg) = system_message {
            messages.push(msg.clone());
        }
        messages.extend(self.history.clone());

        let request = AiRequest {
            system: None,
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
            .get_user_task_prompt(self.cache_name.is_some(), self.series_range.clone())
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

        let mut full_user_message = initial_user_message;
        if let Some(custom) = &self.custom_prompt {
            full_user_message.push_str("\n\n");
            full_user_message.push_str(custom);
        }
        full_user_message.push_str("\n\n");
        full_user_message.push_str(&patch_content);

        let input_context = format!("System: {}\n\nUser: {}", system_prompt, full_user_message);

        let system_message = AiMessage {
            role: AiRole::System,
            content: Some(system_prompt),
            thought: None,
            tool_calls: None,
            tool_call_id: None,
        };

        let initial_message = AiMessage {
            role: AiRole::User,
            content: Some(full_user_message),
            thought: None,
            tool_calls: None,
            tool_call_id: None,
        };
        self.history.push(initial_message);

        let mut current_stage = ReviewStage::Exploration;
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

            // Enforce token budget by pruning
            let (before, after) = self.prune_history(&Some(system_message.clone()));
            final_history_before_pruning = before;
            final_history_after_pruning = after;

            let request = AiRequest {
                system: None,
                messages: {
                    let mut msgs = Vec::new();
                    msgs.push(system_message.clone());
                    msgs.extend(self.history.clone());
                    msgs
                },
                tools: Some(self.tools.get_declarations_generic()),
                temperature: Some(self.temperature),
                preloaded_context: self.cache_name.clone(),
                response_format: None, // Always standard text/tool mode for cache prefix stability
            };

            let resp = match self.provider.generate_content(request).await {
                Ok(resp) => resp,
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
                thought: resp.thought.clone(),
                tool_calls: resp.tool_calls.clone(),
                tool_call_id: None,
            };
            self.history.push(assistant_message);

            // Check for tool calls
            if let Some(tool_calls) = resp.tool_calls {
                let mut tool_responses = Vec::new();
                for call in tool_calls {
                    debug!("Tool Call: {} args: {}", call.function_name, call.arguments);

                    // Stage Completion Interception
                    match call.function_name.as_str() {
                        "cmd_submit_exploration" => {
                            if current_stage == ReviewStage::Exploration {
                                let hypotheses_len = call.arguments["hypotheses"]
                                    .as_array()
                                    .map(|a| a.len())
                                    .unwrap_or(0);
                                if hypotheses_len == 0 {
                                    info!("No hypotheses submitted. Skipping to Reporting.");
                                    current_stage = ReviewStage::Reporting;
                                    self.history.push(AiMessage {
                                        role: AiRole::User,
                                        content: Some(self.prompts.get_stage_reporting_prompt()),
                                        thought: None,
                                        tool_calls: None,
                                        tool_call_id: None,
                                    });
                                } else {
                                    current_stage = ReviewStage::Verification;
                                    self.history.push(AiMessage {
                                        role: AiRole::User,
                                        content: Some(self.prompts.get_stage_verification_prompt()),
                                        thought: None,
                                        tool_calls: None,
                                        tool_call_id: None,
                                    });
                                }
                                tool_responses.push(AiMessage {
                                    role: AiRole::Tool,
                                    content: Some(
                                        json!({ "status": "exploration_received" }).to_string(),
                                    ),
                                    thought: None,
                                    tool_calls: None,
                                    tool_call_id: Some(call.id.clone()),
                                });
                                continue;
                            }
                        }
                        "cmd_submit_verification" => {
                            if current_stage == ReviewStage::Verification {
                                current_stage = ReviewStage::Reporting;
                                self.history.push(AiMessage {
                                    role: AiRole::User,
                                    content: Some(self.prompts.get_stage_reporting_prompt()),
                                    thought: None,
                                    tool_calls: None,
                                    tool_call_id: None,
                                });
                                tool_responses.push(AiMessage {
                                    role: AiRole::Tool,
                                    content: Some(
                                        json!({ "status": "verification_received" }).to_string(),
                                    ),
                                    thought: None,
                                    tool_calls: None,
                                    tool_call_id: Some(call.id.clone()),
                                });
                                continue;
                            }
                        }
                        "cmd_submit_report" => {
                            if current_stage == ReviewStage::Reporting {
                                let findings = call.arguments["findings"].as_array();
                                let review_inline = call.arguments["review_inline"].as_str();

                                if findings.is_some_and(|f| !f.is_empty()) {
                                    if review_inline.is_none()
                                        || review_inline.unwrap().trim().is_empty()
                                    {
                                        tool_responses.push(AiMessage {
                                            role: AiRole::Tool,
                                            content: Some(json!({ "error": "Validation Error: 'findings' provided but 'review_inline' is missing." }).to_string()),
                                            thought: None,
                                            tool_calls: None,
                                            tool_call_id: Some(call.id.clone()),
                                        });
                                        continue;
                                    }
                                    if let Err(e) =
                                        self.validate_review_inline(review_inline.unwrap())
                                    {
                                        tool_responses.push(AiMessage {
                                            role: AiRole::Tool,
                                            content: Some(json!({ "error": format!("Validation Error: {}", e) }).to_string()),
                                            thought: None,
                                            tool_calls: None,
                                            tool_call_id: Some(call.id.clone()),
                                        });
                                        continue;
                                    }
                                }

                                return Ok(WorkerResult {
                                    output: Some(call.arguments.clone()),
                                    error: None,
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
                        _ => {}
                    }

                    // Loop Detection
                    let same_call_count = session_tool_history
                        .iter()
                        .filter(|(n, a)| *n == call.function_name && *a == call.arguments)
                        .count();
                    session_tool_history.push((call.function_name.clone(), call.arguments.clone()));

                    if same_call_count >= 2 {
                        let error_msg =
                            format!("Loop detected in tool usage. Proceed to next step.");
                        tool_responses.push(AiMessage {
                            role: AiRole::Tool,
                            content: Some(json!({ "error": error_msg }).to_string()),
                            thought: None,
                            tool_calls: None,
                            tool_call_id: Some(call.id.clone()),
                        });
                        continue;
                    }

                    let result = match self
                        .tools
                        .call(&call.function_name, call.arguments.clone())
                        .await
                    {
                        Ok(val) => val.to_string(),
                        Err(e) => json!({ "error": e.to_string() }).to_string(),
                    };

                    tool_responses.push(AiMessage {
                        role: AiRole::Tool,
                        content: Some(result),
                        thought: None,
                        tool_calls: None,
                        tool_call_id: Some(call.id.clone()),
                    });
                }
                self.history.extend(tool_responses);
                continue;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_inline_format_valid() {
        let content = "commit 1234567890abcdef\nAuthor: Jane Doe\n\nSummary of changes.\n\n> diff --git a/file b/file\n> index 123..456\n\nThis looks bad.";
        assert!(validate_inline_format(content).is_ok());
    }

    #[test]
    fn test_validate_inline_format_markdown_headers() {
        let content = "# Summary\n\n> diff --git ...";
        assert!(validate_inline_format(content).is_err());
    }

    #[test]
    fn test_validate_inline_format_markdown_code_blocks() {
        let content = "commit 123\n\n```\n> diff --git ...\n```\n\nComment";
        assert!(validate_inline_format(content).is_err());
    }

    #[test]
    fn test_validate_inline_format_no_quoting() {
        let content = "commit 123\n\nThis looks bad.\nNo diff here.";
        assert!(validate_inline_format(content).is_err());
    }

    #[test]
    fn test_validate_inline_format_missing_commit_header() {
        let content = "> diff --git a/file b/file\n> index 123..456\n\nThis looks bad.";
        assert!(validate_inline_format(content).is_err());
    }

    #[test]
    fn test_validate_inline_format_no_comments() {
        let content = "commit 123\nAuthor: Me\n\n> diff --git a/file b/file\n> + code";
        assert!(validate_inline_format(content).is_err());
    }

    #[test]
    fn test_validate_inline_format_headers_in_diff_ok() {
        let content = "commit 123\n\n> #include <stdio.h>\n> void main() {}\n\nComment";
        assert!(validate_inline_format(content).is_ok());
    }
    #[test]
    fn test_calculate_series_range_single_patch() {
        let p = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let patches = vec![p.clone()];
        let patches_to_review = vec![p.clone()];
        let patch_shas = std::collections::HashMap::new();

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            None
        );
    }

    #[test]
    fn test_calculate_series_range_multi_patch_last() {
        let p1 = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let p2 = PatchInput {
            index: 2,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha2".to_string()),
        };
        let patches = vec![p1.clone(), p2.clone()];
        let patches_to_review = vec![p2.clone()]; // Reviewing last
        let patch_shas = std::collections::HashMap::new();

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            None
        );
    }

    #[test]
    fn test_calculate_series_range_multi_patch_middle() {
        let p1 = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha1".to_string()),
        };
        let p2 = PatchInput {
            index: 2,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: Some("sha2".to_string()),
        };
        let patches = vec![p1.clone(), p2.clone()];
        let patches_to_review = vec![p1.clone()]; // Reviewing first
        let patch_shas = std::collections::HashMap::new();

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            Some("base..sha2".to_string())
        );
    }

    #[test]
    fn test_calculate_series_range_use_patch_shas_map() {
        let p1 = PatchInput {
            index: 1,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: None,
        };
        let p2 = PatchInput {
            index: 2,
            diff: "".to_string(),
            subject: None,
            author: None,
            date: None,
            message_id: None,
            commit_id: None,
        };
        let patches = vec![p1.clone(), p2.clone()];
        let patches_to_review = vec![p1.clone()];

        let mut patch_shas = std::collections::HashMap::new();
        patch_shas.insert(2, "sha2_resolved".to_string());

        assert_eq!(
            calculate_series_range(&patches, &patches_to_review, &patch_shas, "base"),
            Some("base..sha2_resolved".to_string())
        );
    }

    #[test]
    fn test_review_stage_transitions() {
        assert_eq!(ReviewStage::Exploration as i32, 0);
        // This is just a basic sanity check for the enum.
        // Logic transitions are handled in Worker::run loop.
    }
}
