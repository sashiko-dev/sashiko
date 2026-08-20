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

//! Declarative workflow for reviewing Sashiko's Linux kernel prompt modifications.
//!
//! This module specifies a 4-stage review pipeline as a declarative [`Workflow`]
//! operating over [`LinuxPromptReviewState`]:
//! 1. Stage 1: Factual & Guideline Constraints (No action instructions, no trivial facts).
//! 2. Stage 2: Codebase Verification against Linux Source Tree (Linus tree HEAD).
//! 3. Stage 3: Index & Placement Verification (Registration in subsystem index and correct file location).
//! 4. Stage 4: Concern Aggregation & Report Generation (LKML-inspired plain-text review report).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::workflow::graph::Workflow;
use crate::workflow::output::OutputFormat;
use crate::workflow::policy::{ParallelPolicy, StagePolicy, ToolScope};
use crate::workflow::prompt::PromptTemplate;
use crate::workflow::stage::Stage;

/// Complete execution state of a Linux kernel prompt review.
#[derive(Clone, Debug, Default)]
pub struct LinuxPromptReviewState {
    pub ps_id: String,
    pub p_id: String,
    pub commit_sha: Option<String>,
    pub commit_subject: Option<String>,
    pub target_prompt_diff: String,

    /// Stage 1 concerns (Factual correctness & actionability constraints).
    pub stage_1_concerns: Vec<Value>,
    /// Stage 2 concerns (Codebase verification against kernel sources).
    pub stage_2_concerns: Vec<Value>,
    /// Stage 3 concerns (Index registration & directory placement).
    pub stage_3_concerns: Vec<Value>,

    /// Aggregated raw concerns collected from Stages 1-3.
    pub all_concerns: Vec<Value>,
    /// Aggregated raw dismissed concerns collected from Stages 1-3.
    pub all_dismissed_concerns: Vec<Value>,

    /// Synthesized plain text review report from Stage 4.
    pub report: String,
}

// ---------------------------------------------------------------------------
// Typed Output Structures for Stage Serialization
// ---------------------------------------------------------------------------

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct StageConcernsOutput {
    #[serde(default)]
    pub concerns: Vec<Value>,
    #[serde(default)]
    pub dismissed_concerns: Vec<Value>,
}

// ---------------------------------------------------------------------------
// Common System Prompt Template
// ---------------------------------------------------------------------------

pub fn prompt_review_system_prompt() -> PromptTemplate<LinuxPromptReviewState> {
    let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();

    PromptTemplate::<LinuxPromptReviewState>::new(format!(
        r#"Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references strictly on this current date.

You are an expert prompt engineer and Linux kernel maintainer evaluating proposed additions or modifications to Sashiko's Linux review prompts knowledge base.

Your task is to review proposed prompt changes to ensure they are:
1. Factually accurate and free from impossible action instructions (like compiling code or searching the web) and free from trivial C syntax explanations.
2. Verified against the Linux kernel source code (Linus tree HEAD) where applicable.
3. Placed into the appropriate directory (api/ for API caller rules, subsystems/ for subsystem internals, generic/ for formats and policies) and registered in index.md if necessary.
4. Synthesized into a polite, constructive, plain-text review report.

TOOL USAGE: When you need to gather information using tools (e.g. verifying kernel symbols in the source tree), actively batch parallel or independent tool calls into a single response to minimize conversation turns.

=== Target Prompt Diff ===
{{{{target_prompt_diff}}}}
=========================="
"#
    ))
    .include_file("system.md")
    .include_file("linux_prompts/system.md")
    .with_var("target_prompt_diff", |s: &LinuxPromptReviewState| {
        s.target_prompt_diff.clone()
    })
}

// ---------------------------------------------------------------------------
// JSON Schema Helpers
// ---------------------------------------------------------------------------

fn concerns_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "concerns": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "type": { "type": "string" },
                        "description": { "type": "string" },
                        "reasoning": { "type": "string" },
                        "locations": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "file": { "type": "string" },
                                    "line": { "type": ["integer", "null"] },
                                    "code_snippet": { "type": "string" },
                                    "why_this_location_matters": { "type": "string" }
                                },
                                "required": ["file", "why_this_location_matters"]
                            }
                        }
                    },
                    "required": ["type", "description", "reasoning"]
                }
            },
            "dismissed_concerns": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "type": { "type": "string" },
                        "description": { "type": "string" },
                        "reasoning": { "type": "string" },
                        "locations": {
                            "type": "array",
                            "items": { "type": "object" }
                        }
                    },
                    "required": ["type", "description", "reasoning"]
                }
            }
        },
        "required": ["concerns", "dismissed_concerns"]
    })
}

fn append_prompt_stage_items(
    dest: &mut Vec<Value>,
    src: &[Value],
    stage_num: u8,
    default_type: &str,
) {
    for item in src {
        let mut obj = item.clone();
        if let Some(map) = obj.as_object_mut() {
            if !map.contains_key("type")
                || map
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .is_empty()
            {
                map.insert("type".to_string(), json!(default_type));
            }
            map.insert("stage".to_string(), json!(stage_num));
        }
        dest.push(obj);
    }
}

// ---------------------------------------------------------------------------
// Validation Logic
// ---------------------------------------------------------------------------

fn validate_prompt_review_report(
    content: &str,
    _state: &LinuxPromptReviewState,
) -> Result<(), String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err("Report output is empty.".to_string());
    }
    if content
        .lines()
        .any(|l| l.trim_start().starts_with("```json"))
    {
        return Err(
            "Report contains a JSON code block. Please return raw plain text following report-template.md."
                .to_string(),
        );
    }
    let lower = trimmed.to_lowercase();
    if !lower.contains("summary")
        && !lower.contains("review")
        && !lower.contains("concern")
        && !lower.contains("issue")
        && !lower.contains("no issues")
        && !lower.contains("prompt")
    {
        return Err("Report does not appear to contain a review summary or feedback.".to_string());
    }
    Ok(())
}

fn format_prompt_report_feedback(violation: &str) -> String {
    format!(
        "\n\nPrevious report attempt was rejected: {}. Please format the report as plain text following the guidelines and report-template.md.",
        violation
    )
}

// ---------------------------------------------------------------------------
// Stage Builders
// ---------------------------------------------------------------------------

/// Stage 1: Factual and Guideline Constraints.
///
/// Ensures new prompt text contains ONLY factual information, contains NO action
/// instructions (like compiling code or searching the web), and NO trivial facts.
pub fn stage_1_factual_constraints(
    max_turns: usize,
    temperature: f32,
) -> Stage<LinuxPromptReviewState, StageConcernsOutput> {
    Stage::builder("stage_1_factual_constraints")
        .system_prompt(prompt_review_system_prompt())
        .user_prompt(
            PromptTemplate::<LinuxPromptReviewState>::new(
                r#"# Stage 1: Factual and Guideline Constraints

@include("stage1_factual_constraints.md")
@include("linux_prompts/stage1_factual_constraints.md")

Target Prompt Diff:
{{target_prompt_diff}}

Return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays."#,
            )
            .include_file("stage1_factual_constraints.md")
            .include_file("linux_prompts/stage1_factual_constraints.md")
            .with_var("target_prompt_diff", |s: &LinuxPromptReviewState| {
                s.target_prompt_diff.clone()
            }),
        )
        .output_format(OutputFormat::json_with_schema(concerns_schema()))
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce(|state, out: StageConcernsOutput| {
            append_prompt_stage_items(
                &mut state.stage_1_concerns,
                &out.concerns,
                1,
                "Factual/Actionability",
            );
            append_prompt_stage_items(
                &mut state.all_concerns,
                &out.concerns,
                1,
                "Factual/Actionability",
            );
            state.all_dismissed_concerns.extend(out.dismissed_concerns);
        })
        .build()
}

/// Stage 2: Linux Source Code Verification.
///
/// Verifies claims against the Linux kernel source code using latest Linus tree HEAD.
/// If verification is not possible, it does not do anything (no false alarms).
pub fn stage_2_codebase_verification(
    max_turns: usize,
    temperature: f32,
) -> Stage<LinuxPromptReviewState, StageConcernsOutput> {
    Stage::builder("stage_2_codebase_verification")
        .system_prompt(prompt_review_system_prompt())
        .user_prompt(
            PromptTemplate::<LinuxPromptReviewState>::new(
                r#"# Stage 2: Linux Source Code Verification

@include("stage2_codebase_verification.md")
@include("linux_prompts/stage2_codebase_verification.md")

Target Prompt Diff:
{{target_prompt_diff}}

Use available tools (git_grep, git_read_files, git_find_files, git_show, git_log) to inspect the Linux kernel repository if needed.
If verification against the kernel tree is not possible, return {"concerns": [], "dismissed_concerns": []}.

Return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays."#,
            )
            .include_file("stage2_codebase_verification.md")
            .include_file("linux_prompts/stage2_codebase_verification.md")
            .with_var("target_prompt_diff", |s: &LinuxPromptReviewState| {
                s.target_prompt_diff.clone()
            }),
        )
        .output_format(OutputFormat::json_with_schema(concerns_schema()))
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce(|state, out: StageConcernsOutput| {
            append_prompt_stage_items(
                &mut state.stage_2_concerns,
                &out.concerns,
                2,
                "Codebase Discrepancy",
            );
            append_prompt_stage_items(
                &mut state.all_concerns,
                &out.concerns,
                2,
                "Codebase Discrepancy",
            );
            state.all_dismissed_concerns.extend(out.dismissed_concerns);
        })
        .build()
}

/// Stage 3: Index and Placement Verification.
///
/// Verifies that changes are placed in the correct top-level directory (api/ for API
/// caller rules, subsystems/ for subsystem internals, generic/ for formats and policies)
/// and registered in index.md if necessary.
pub fn stage_3_index_placement(
    max_turns: usize,
    temperature: f32,
) -> Stage<LinuxPromptReviewState, StageConcernsOutput> {
    Stage::builder("stage_3_index_placement")
        .system_prompt(prompt_review_system_prompt())
        .user_prompt(
            PromptTemplate::<LinuxPromptReviewState>::new(
                r#"# Stage 3: Index and Placement Verification

@include("stage3_index_placement.md")
@include("linux_prompts/stage3_index_placement.md")

Target Prompt Diff:
{{target_prompt_diff}}

Return ONLY a JSON object with 'concerns' and 'dismissed_concerns' arrays."#,
            )
            .include_file("stage3_index_placement.md")
            .include_file("linux_prompts/stage3_index_placement.md")
            .with_var("target_prompt_diff", |s: &LinuxPromptReviewState| {
                s.target_prompt_diff.clone()
            }),
        )
        .output_format(OutputFormat::json_with_schema(concerns_schema()))
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce(|state, out: StageConcernsOutput| {
            append_prompt_stage_items(
                &mut state.stage_3_concerns,
                &out.concerns,
                3,
                "Index/Placement",
            );
            append_prompt_stage_items(&mut state.all_concerns, &out.concerns, 3, "Index/Placement");
            state.all_dismissed_concerns.extend(out.dismissed_concerns);
        })
        .build()
}

/// Stage 4: Concern Aggregation & Report Generation.
///
/// Aggregates concerns from Stages 1-3 and generates a report matching Sashiko's style.
pub fn stage_4_report_generation(
    max_turns: usize,
    temperature: f32,
) -> Stage<LinuxPromptReviewState, String> {
    Stage::builder("stage_4_report_generation")
        .system_prompt(prompt_review_system_prompt())
        .user_prompt(
            PromptTemplate::<LinuxPromptReviewState>::new(
                r#"# Stage 4: Concern Aggregation & Review Report Generation

@include("stage4_report_generation.md")
@include("linux_prompts/stage4_report_generation.md")
@include("report-template.md")
@include("linux_prompts/report-template.md")

Target Prompt Diff:
{{target_prompt_diff}}

Aggregated Concerns:
{{all_concerns}}

Aggregated Dismissed Concerns:
{{all_dismissed_concerns}}

Generate the final plain-text review report following Sashiko's review style. Return raw text output, not JSON."#,
            )
            .include_file("stage4_report_generation.md")
            .include_file("linux_prompts/stage4_report_generation.md")
            .include_file("report-template.md")
            .include_file("linux_prompts/report-template.md")
            .with_var("target_prompt_diff", |s: &LinuxPromptReviewState| {
                s.target_prompt_diff.clone()
            })
            .with_var("all_concerns", |s: &LinuxPromptReviewState| {
                serde_json::to_string_pretty(&s.all_concerns).unwrap_or_default()
            })
            .with_var("all_dismissed_concerns", |s: &LinuxPromptReviewState| {
                serde_json::to_string_pretty(&s.all_dismissed_concerns).unwrap_or_default()
            }),
        )
        .output_format(OutputFormat::text_with_validator(
            validate_prompt_review_report,
            format_prompt_report_feedback,
        ))
        .policy(StagePolicy {
            tools: ToolScope::None,
            max_turns,
            temperature,
            ..Default::default()
        })
        .reduce(|state, out: String| {
            state.report = out;
        })
        .build()
}

// ---------------------------------------------------------------------------
// Complete Linux Prompt Review Workflow Graph
// ---------------------------------------------------------------------------

/// Constructs the default declarative workflow for reviewing Linux prompt modifications.
pub fn build_linux_prompt_review_workflow() -> Workflow<LinuxPromptReviewState> {
    build_linux_prompt_review_workflow_with_options(10, 0.0)
}

/// Constructs the declarative Linux prompt review workflow with custom turns and temperature.
pub fn build_linux_prompt_review_workflow_with_options(
    max_turns: usize,
    temperature: f32,
) -> Workflow<LinuxPromptReviewState> {
    Workflow::builder("linux_kernel_prompt_review")
        .parallel(
            vec![
                Box::new(stage_1_factual_constraints(max_turns, temperature)),
                Box::new(stage_2_codebase_verification(max_turns, temperature)),
                Box::new(stage_3_index_placement(max_turns, temperature)),
            ],
            ParallelPolicy::BestEffort,
        )
        .stage(stage_4_report_generation(max_turns, temperature))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiProvider, AiRequest, AiResponse, AiRole, AiUsage, ProviderCapabilities};
    use crate::toolbox::ToolBox;
    use crate::workflow::WorkflowEngine;
    use crate::workflow::stage::WorkflowEnv;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MockPromptReviewProvider {
        call_count: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl AiProvider for MockPromptReviewProvider {
        async fn generate_content(&self, request: AiRequest) -> anyhow::Result<AiResponse> {
            let _count = self.call_count.fetch_add(1, Ordering::SeqCst);

            let last_user_msg = request
                .messages
                .iter()
                .rfind(|m| m.role == AiRole::User)
                .and_then(|m| m.content.as_deref())
                .unwrap_or("");

            let content = if last_user_msg.contains("Stage 1: Factual and Guideline Constraints") {
                r##"{
  "concerns": [
    {
      "type": "Action Instruction",
      "description": "Prompt instructs model to compile kernel with make",
      "reasoning": "Sashiko review workflow has no build environment or make tool.",
      "locations": [
        {
          "file": "prompts/linux/subsystem/sample.md",
          "line": 10,
          "code_snippet": "compile the code with make -j32",
          "why_this_location_matters": "Unsupported action"
        }
      ]
    }
  ],
  "dismissed_concerns": []
}"##
                .to_string()
            } else if last_user_msg.contains("Stage 2: Linux Source Code Verification") {
                r##"{
  "concerns": [
    {
      "type": "Codebase Discrepancy",
      "description": "Function foo_bar_helper does not exist in kernel source",
      "reasoning": "Searched tree with git_grep, symbol not found in upstream.",
      "locations": [
        {
          "file": "prompts/linux/subsystem/sample.md",
          "line": 15,
          "code_snippet": "foo_bar_helper()",
          "why_this_location_matters": "Nonexistent API"
        }
      ]
    }
  ],
  "dismissed_concerns": []
}"##
                .to_string()
            } else if last_user_msg.contains("Stage 3: Index and Placement Verification") {
                r##"{
  "concerns": [
    {
      "type": "Index/Placement",
      "description": "Locking API usage rules placed under subsystems/ instead of api/",
      "reasoning": "Rules describing how callers use mutex primitives belong in api/locking.md rather than subsystems/locking.md, and must be registered in index.md.",
      "locations": [
        {
          "file": "subsystems/locking.md",
          "line": 1,
          "code_snippet": "mutex usage rules for driver authors",
          "why_this_location_matters": "Belongs in api/locking.md and registered in index.md"
        }
      ]
    }
  ],
  "dismissed_concerns": []
}"##
                .to_string()
            } else if last_user_msg
                .contains("Stage 4: Concern Aggregation & Review Report Generation")
            {
                r##"Review Summary:
Reviewed proposed prompt changes in subsystems/locking.md.
Found 3 issues across actionability, codebase verification, and folder placement.

> +compile the code with make -j32

Can this action instruction be executed by Sashiko? Sashiko is a static review
workflow with read-only git tools and cannot compile or build kernel code.
Please rephrase into static code inspection guidance.

> +foo_bar_helper()

Does foo_bar_helper exist in upstream Linux? A search in Linus's tree returned
no occurrences.

> [subsystems/locking.md]

This file describes caller usage rules for locking primitives. Caller API rules
belong under 'api/locking.md', while 'subsystems/' is reserved for internal
subsystem implementation details. Please move to 'api/locking.md' and register
it in 'index.md'.
"##
                .to_string()
            } else {
                r#"{"concerns": [], "dismissed_concerns": []}"#.to_string()
            };

            Ok(AiResponse {
                content: Some(content),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: Some(AiUsage {
                    prompt_tokens: 50,
                    completion_tokens: 30,
                    total_tokens: 80,
                    cached_tokens: None,
                }),

                truncated: false,
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            100
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock-prompt-reviewer".to_string(),
                context_window_size: 32000,
            }
        }
    }

    #[test]
    fn test_build_linux_prompt_review_workflow_structure() {
        let workflow = build_linux_prompt_review_workflow();
        assert_eq!(workflow.name, "linux_kernel_prompt_review");
        assert_eq!(workflow.steps.len(), 2); // 1 parallel step (stages 1-3) + 1 stage (stage 4)
    }

    #[test]
    fn test_validate_prompt_review_report() {
        let state = LinuxPromptReviewState::default();

        assert!(validate_prompt_review_report("", &state).is_err());
        assert!(
            validate_prompt_review_report("```json\n{\"report\": \"ok\"}\n```", &state).is_err()
        );
        assert!(
            validate_prompt_review_report("Random completely unrelated text 123.", &state).is_err()
        );

        let valid_report = "Review Summary:\nNo issues found in the prompt modification.";
        assert!(validate_prompt_review_report(valid_report, &state).is_ok());
    }

    #[tokio::test]
    async fn test_execute_linux_prompt_review_workflow() {
        let temp_dir = tempfile::tempdir().unwrap();
        let prompts_dir = PathBuf::from("prompts/linux_prompts");
        let tools = Arc::new(ToolBox::new(temp_dir.path().to_path_buf(), None));
        let provider: Arc<dyn AiProvider> = Arc::new(MockPromptReviewProvider {
            call_count: AtomicUsize::new(0),
        });

        let mut state = LinuxPromptReviewState {
            ps_id: "ps_1".to_string(),
            p_id: "1".to_string(),
            commit_sha: Some("abc1234".to_string()),
            commit_subject: Some("prompts: add sample subsystem guide".to_string()),
            target_prompt_diff: r#"diff --git a/prompts/linux/subsystem/sample.md b/prompts/linux/subsystem/sample.md
new file mode 100644
--- /dev/null
+++ b/prompts/linux/subsystem/sample.md
@@ -0,0 +1,20 @@
+# Sample Subsystem
+compile the code with make -j32 to check warnings.
+foo_bar_helper() must be called with lock held.
+"#
            .to_string(),
            ..Default::default()
        };

        let workflow = build_linux_prompt_review_workflow_with_options(5, 0.0);
        let env = WorkflowEnv {
            provider: provider.clone(),
            tools: tools.clone(),
            base_dir: &prompts_dir,
            context_tag: Some("[prompt-review] ".to_string()),
        };

        let outcome = WorkflowEngine::execute(&workflow, &env, &mut state, None)
            .await
            .unwrap();

        assert_eq!(state.stage_1_concerns.len(), 1);
        assert_eq!(state.stage_2_concerns.len(), 1);
        assert_eq!(state.stage_3_concerns.len(), 1);
        assert_eq!(state.all_concerns.len(), 3);
        assert!(state.report.contains("Review Summary:"));
        assert!(state.report.contains("api/locking.md"));
        assert!(state.report.contains("index.md"));
        assert!(outcome.tokens_in > 0);
    }

    #[tokio::test]
    async fn test_render_prompt_templates() {
        let prompts_dir = PathBuf::from("prompts/linux_prompts");
        let state = LinuxPromptReviewState {
            target_prompt_diff: "diff --git a/foo.md b/foo.md\n+test line".to_string(),
            ..Default::default()
        };

        let sys_template = prompt_review_system_prompt();
        let rendered_sys = sys_template
            .render_for_model(&state, &prompts_dir)
            .await
            .unwrap();
        assert!(rendered_sys.contains("diff --git a/foo.md b/foo.md"));

        let stage1 = stage_1_factual_constraints(1, 0.0);
        let rendered_stage1 = stage1
            .user_prompt
            .render_for_model(&state, &prompts_dir)
            .await
            .unwrap();
        assert!(rendered_stage1.contains("Stage 1: Factual and Guideline Constraints"));
        assert!(rendered_stage1.contains("+test line"));

        let stage2 = stage_2_codebase_verification(1, 0.0);
        let rendered_stage2 = stage2
            .user_prompt
            .render_for_model(&state, &prompts_dir)
            .await
            .unwrap();
        assert!(rendered_stage2.contains("Stage 2: Linux Source Code Verification"));

        let stage3 = stage_3_index_placement(1, 0.0);
        let rendered_stage3 = stage3
            .user_prompt
            .render_for_model(&state, &prompts_dir)
            .await
            .unwrap();
        assert!(rendered_stage3.contains("Stage 3: Index and Placement Verification"));

        let stage4 = stage_4_report_generation(1, 0.0);
        let rendered_stage4 = stage4
            .user_prompt
            .render_for_model(&state, &prompts_dir)
            .await
            .unwrap();
        assert!(
            rendered_stage4.contains("Stage 4: Concern Aggregation & Review Report Generation")
        );
    }
}
