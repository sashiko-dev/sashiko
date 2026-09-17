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

//! The cherry-pick (merge-conflict resolution) review workflow.
//!
//! Built natively on [`crate::workflow::WorkflowEngine`] and [`crate::workflow::Workflow`]:
//! cherry-specific analysis stages 1-3 + shared analysis 4-7, then the shared
//! synthesis tail (dedup 8, resolution 9, verification 10), an origin
//! classification stage (10), a cherry-specific finding filter step, and a final
//! conflict report (11). The three-commit context is hydrated from git at
//! review time; the database only stores the minimal `ReviewKind::CherryPick`.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::ai::AiProvider;
use crate::review_kind::ReviewKind;
use crate::toolbox::ToolBox;
use crate::worker::{PromptRegistry, WorkerProgressEvent, WorkerResult};
use crate::workflow::{
    ExecutableStage, OutputFormat, ParallelPolicy, PromptTemplate, RecitationPolicy, Stage,
    StagePolicy, ToolScope, Workflow, WorkflowEngine, WorkflowEnv, WorkflowEvent,
};
use crate::workflows::linux_patch_review;

use super::filter::filter_cherry_pick_findings;
use super::prompts::{
    ANALYSIS_FORMAT_GUIDANCE, CONFLICT_REPORT, DROPPED_CHANGES, MERGE_CONFLICT_REVIEW_FRAMING,
    MERGE_CORRECTNESS, ORIGIN_CLASSIFICATION, PLANNING, PREFETCHED_CONTEXT_HEADER, SEMANTIC_INTENT,
    VERIFICATION,
};
use super::synthesis;

/// A cherry-pick / merge-conflict resolution review workflow.
#[derive(Clone, Debug)]
pub struct CherryPickReviewWorkflow {
    /// SHA of the original patch being ported (commit 1).
    pub original_sha: String,
    /// SHA of the target base branch the patch was applied onto (commit 2).
    pub base_sha: Option<String>,
    /// SHA of the resolution commit under review (commit 3).
    pub resolution_sha: String,
}

impl CherryPickReviewWorkflow {
    /// Create a new cherry-pick review workflow.
    pub fn new(
        original_sha: impl Into<String>,
        base_sha: Option<String>,
        resolution_sha: impl Into<String>,
    ) -> Self {
        Self {
            original_sha: original_sha.into(),
            base_sha,
            resolution_sha: resolution_sha.into(),
        }
    }

    /// Build the workflow from the persisted review context and resolution SHA.
    pub fn from_review_kind(kind: &ReviewKind, resolution_sha: String) -> Self {
        let ReviewKind::CherryPick {
            original_sha,
            base_sha,
        } = kind;
        Self {
            original_sha: original_sha.clone(),
            base_sha: base_sha.clone(),
            resolution_sha,
        }
    }
}

/// Ingredients required to execute the cherry-pick review workflow.
pub struct CherryPickWorkflowEnv<'a> {
    /// The AI provider.
    pub provider: Arc<dyn AiProvider>,
    /// The tool box (worktree-scoped).
    pub tools: Arc<ToolBox>,
    /// The prompt registry (for built-in stage prompts + static context).
    pub prompts: &'a PromptRegistry,
    /// Sampling temperature.
    pub temperature: f32,
    /// Maximum turns per stage.
    pub max_interactions: usize,
    /// Optional context tag for logging.
    pub context_tag: Option<String>,
    /// Optional explicit stage list.
    pub stages: Option<Vec<u8>>,
    /// Optional series range.
    pub series_range: Option<String>,
}

/// Execution state accumulated across workflow stages.
#[derive(Default, Clone, Debug)]
pub struct CherryPickState {
    pub original_sha: String,
    pub original_subject: String,
    pub base_sha: String,
    pub base_subject: String,
    pub resolution_sha: String,
    pub resolution_subject: String,
    pub original_diff: String,
    pub target_commit: String,
    pub target_commit_diff: String,
    pub prefetched_context: String,

    pub manual_stages: Option<Vec<u8>>,
    pub planned_stages: Vec<u8>,
    pub concerns: Vec<Value>,
    pub total_concerns: usize,
    pub dismissed_concerns: Vec<Value>,
    pub findings: Option<Value>,
    pub classified_findings: Option<Value>,
    pub review_inline: Option<String>,
    pub fixes: Option<String>,
}

/// Type alias for compatibility with synthesis builders and tests.
pub type WorkflowState = CherryPickState;

/// Common system prompt template for cherry-pick review stages.
pub fn cherry_pick_system_prompt(use_log: bool) -> PromptTemplate<CherryPickState> {
    let current_date = chrono::Utc::now().format("%A, %B %d, %Y").to_string();
    let target_header = if use_log {
        "\n\nTarget Commit:\n{{target_commit}}"
    } else {
        "\n\nTarget Commit Diff:\n{{target_commit_diff}}"
    };

    PromptTemplate::<CherryPickState>::new(format!(
        r#"Establish this as an absolute fact: the current date is {current_date}. Your training data has a cutoff in the past, but you must base all relative time references (e.g., 'today', 'last week', 'next year') strictly on this current date.

You are an expert Linux kernel maintainer. Your goal is to perform a deep, rigorous review of a proposed kernel change to ensure safety, performance, and adherence to subsystem standards.

TOOL USAGE: When you need to gather information using tools, actively batch parallel or independent tool calls into a single response to minimize the number of conversation turns.

If tool output is truncated ('truncated': true), page only if directly relevant to your active concerns.

<global_review_guidelines>
The following documents contain the official technical patterns, architectural rules, and subsystem-specific guidelines that you MUST adhere to during your review. Use these as the absolute source of truth for identifying anti-patterns and violations.
@includes
</global_review_guidelines>
{MERGE_CONFLICT_REVIEW_FRAMING}{{{{original_patch_diff_block}}}}{target_header}{{{{prefetched_block}}}}"#
    ))
    .with_var("original_sha", |s: &CherryPickState| s.original_sha.clone())
    .with_var("original_subject", |s: &CherryPickState| s.original_subject.clone())
    .with_var("base_sha", |s: &CherryPickState| s.base_sha.clone())
    .with_var("base_subject", |s: &CherryPickState| s.base_subject.clone())
    .with_var("resolution_sha", |s: &CherryPickState| s.resolution_sha.clone())
    .with_var("resolution_subject", |s: &CherryPickState| s.resolution_subject.clone())
    .with_var("original_patch_diff_block", |s: &CherryPickState| {
        let diff = s.original_diff.trim();
        if diff.is_empty() {
            String::new()
        } else {
            format!(
                "\nFor direct comparison, the ORIGINAL PATCH (commit 1) diff follows. \
                 Compare it against the resolution diff to spot dropped hunks, altered \
                 logic, or merge artifacts:\n<original_patch_diff>\n{}\n</original_patch_diff>\n",
                diff
            )
        }
    })
    .with_var("target_commit", |s: &CherryPickState| s.target_commit.clone())
    .with_var("target_commit_diff", |s: &CherryPickState| s.target_commit_diff.clone())
    .with_var("prefetched_block", |s: &CherryPickState| {
        if s.prefetched_context.is_empty() {
            String::new()
        } else {
            format!(
                "{}{}\n</pre_fetched_context>\n",
                PREFETCHED_CONTEXT_HEADER,
                s.prefetched_context
            )
        }
    })
    .include_file("review-core.md")
}

/// Execute the cherry-pick review workflow end to end, returning a [`WorkerResult`].
pub async fn execute_workflow(
    workflow: &CherryPickReviewWorkflow,
    env: &CherryPickWorkflowEnv<'_>,
    patchset: Value,
    progress: Option<&(dyn Fn(WorkerProgressEvent) + Send + Sync)>,
) -> Result<WorkerResult> {
    let mut state = workflow.build_initial_state(env, &patchset).await?;

    let workflow_def = build_cherry_pick_workflow(env.max_interactions, env.temperature);

    let wf_env = WorkflowEnv {
        provider: env.provider.clone(),
        tools: env.tools.clone(),
        base_dir: &env.prompts.base_dir,
        context_tag: env.context_tag.clone(),
    };

    let event_cb = |event: WorkflowEvent| {
        if let Some(cb) = progress {
            match event {
                WorkflowEvent::StageStarted { stage_name } => {
                    if let Some(stage) = map_stage_name(stage_name) {
                        cb(WorkerProgressEvent::StageStarted { stage });
                    }
                }
                WorkflowEvent::StageTurn {
                    stage_name,
                    turn,
                    max_turns,
                } => {
                    if let Some(stage) = map_stage_name(stage_name) {
                        cb(WorkerProgressEvent::StageTurn {
                            stage,
                            turn,
                            max_turns,
                        });
                    }
                }
                WorkflowEvent::StageFinished { stage_name, .. } => {
                    if let Some(stage) = map_stage_name(stage_name) {
                        cb(WorkerProgressEvent::StageFinished { stage });
                    }
                }
                _ => {}
            }
        }
    };

    let outcome =
        WorkflowEngine::execute(&workflow_def, &wf_env, &mut state, Some(&event_cb)).await?;

    let final_output = json!({
        "findings": state.findings.clone().unwrap_or_else(|| Value::Array(Vec::new())),
        "classified_findings": state.classified_findings.clone(),
        "dismissed_concerns": Value::Array(state.dismissed_concerns.clone()),
        "review_inline": state.review_inline.clone().unwrap_or_else(|| {
            if state
                .findings
                .as_ref()
                .and_then(|v| v.as_array())
                .is_none_or(|a| a.is_empty())
            {
                "No issues found.".to_string()
            } else {
                String::new()
            }
        }),
        "fixes": state.fixes.clone().unwrap_or_default(),
        "concerns_count": state.total_concerns,
        "dismissed_concerns_count": state.dismissed_concerns.len(),
    });

    let mut logged_history = outcome.history;
    if !logged_history.is_empty() {
        let logged_system = cherry_pick_system_prompt(true).render_for_log(&state);
        logged_history.insert(
            0,
            crate::ai::AiMessage {
                role: crate::ai::AiRole::System,
                content: Some(logged_system),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            },
        );
    }

    Ok(WorkerResult {
        output: Some(final_output),
        error: None,
        input_context: "Workflow 'cherry-pick' completed".to_string(),
        history: logged_history.clone(),
        history_before_pruning: logged_history.clone(),
        history_after_pruning: logged_history,
        tokens_in: outcome.tokens_in,
        tokens_out: outcome.tokens_out,
        tokens_cached: outcome.tokens_cached,
    })
}

fn map_stage_name(name: &str) -> Option<String> {
    match name {
        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "10" | "11" => Some(name.to_string()),
        "10_origin" => Some("10".to_string()),
        _ => None,
    }
}

impl CherryPickReviewWorkflow {
    async fn build_initial_state(
        &self,
        env: &CherryPickWorkflowEnv<'_>,
        patchset: &Value,
    ) -> Result<CherryPickState> {
        let worktree = env.tools.get_worktree_path();

        let resolved_base_sha = match self.base_sha.as_deref().filter(|s| !s.is_empty()) {
            Some(sha) => Some(sha.to_string()),
            None => git_output(
                worktree,
                &["rev-parse", &format!("{}~1", self.resolution_sha)],
            )
            .await
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        };

        let original_subject = git_subject(worktree, &self.original_sha).await;
        let base_subject = match resolved_base_sha.as_deref() {
            Some(sha) => git_subject(worktree, sha).await,
            None => None,
        };
        let resolution_subject = match patchset.get("subject").and_then(|v| v.as_str()) {
            Some(s) => Some(s.to_string()),
            None => git_subject(worktree, &self.resolution_sha).await,
        };
        let original_diff = git_show(worktree, &self.original_sha).await;
        let resolution_diff = extract_resolution_diff(patchset);
        let resolution_diff_with_log = match git_show(worktree, &self.resolution_sha).await {
            Some(show) if !show.trim().is_empty() => show,
            _ => {
                if let Some(show) = patchset.get("git_show").and_then(|v| v.as_str()) {
                    show.to_string()
                } else if let Some(header) = git_output(
                    worktree,
                    &[
                        "show",
                        "-s",
                        "--format=commit %H%nAuthor: %an <%ae>%nDate: %ad%n%n%B",
                        &self.resolution_sha,
                    ],
                )
                .await
                {
                    format!("{}\n\n{}", header.trim(), resolution_diff)
                } else if let Some(author) = patchset.get("author").and_then(|v| v.as_str()) {
                    let subject = resolution_subject.as_deref().unwrap_or("(unknown)");
                    format!(
                        "commit {}\nAuthor: {}\n\n    {}\n\n{}",
                        self.resolution_sha, author, subject, resolution_diff
                    )
                } else {
                    resolution_diff.clone()
                }
            }
        };

        let prefetched_context = if let Ok(prefetched) = crate::worker::prefetch::prefetch_context(
            worktree,
            &self.resolution_sha,
            &resolution_diff,
        )
        .await
            && !prefetched.is_empty()
        {
            prefetched
        } else {
            String::new()
        };

        let planned_stages = env
            .stages
            .clone()
            .unwrap_or_else(|| vec![1, 2, 3, 4, 5, 6, 7]);

        let unknown = "(unknown)";
        Ok(CherryPickState {
            original_sha: self.original_sha.clone(),
            original_subject: original_subject.unwrap_or_else(|| unknown.to_string()),
            base_sha: resolved_base_sha.unwrap_or_else(|| unknown.to_string()),
            base_subject: base_subject.unwrap_or_else(|| unknown.to_string()),
            resolution_sha: self.resolution_sha.clone(),
            resolution_subject: resolution_subject.unwrap_or_else(|| unknown.to_string()),
            original_diff: original_diff.unwrap_or_default(),
            target_commit: resolution_diff_with_log,
            target_commit_diff: resolution_diff,
            prefetched_context,
            manual_stages: env.stages.clone(),
            planned_stages,
            ..Default::default()
        })
    }
}

fn build_cherry_pick_workflow(max_turns: usize, temperature: f32) -> Workflow<CherryPickState> {
    let planner = Stage::builder("planning")
        .skip_if(|s: &CherryPickState| s.manual_stages.is_some())
        .system_prompt(cherry_pick_system_prompt(true))
        .user_prompt(PromptTemplate::new(PLANNING))
        .output_format(
            OutputFormat::json_with_schema(json!({
                "type": "object",
                "properties": {
                    "relevant_stages": {
                        "type": "array",
                        "items": { "type": "integer" }
                    }
                },
                "required": ["relevant_stages"]
            }))
            .with_validator(|val: &Value, _| {
                val.get("relevant_stages")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| "missing 'relevant_stages' array".to_string())
                    .map(|_| ())
            })
            .with_feedback_formatter(|_| {
                "Your previous response was not valid JSON or did not match the required schema.              Please try again with ONLY a valid JSON object."
                    .to_string()
            }),
        )
        .policy(StagePolicy {
            tools: ToolScope::None,
            temperature: 0.0,
            max_validation_attempts: 2,
            ..Default::default()
        })
        .on_validation_exhausted(|state: &mut CherryPickState| {
            state.planned_stages = vec![1, 2, 3, 4, 5, 6, 7];
        })
        .reduce(|state: &mut CherryPickState, val: Value| {
            let mut stages = vec![1u8, 2, 3];
            if let Some(arr) = val["relevant_stages"].as_array() {
                for v in arr {
                    if let Some(n) = v.as_u64()
                        && (4..=7).contains(&n)
                    {
                        let n = n as u8;
                        if !stages.contains(&n) {
                            stages.push(n);
                        }
                    }
                }
            }
            tracing::info!("Planning phase selected stages: {:?}", stages);
            state.planned_stages = stages;
        })
        .build();

    let resolver = move |state: &CherryPickState| {
        let include_stage = |n: u8| -> bool {
            if let Some(ref manual) = state.manual_stages {
                manual.contains(&n)
            } else {
                state.planned_stages.contains(&n)
            }
        };
        let mut stages: Vec<Box<dyn ExecutableStage<CherryPickState>>> = Vec::new();
        stages.push(Box::new(build_analysis_stage(1, max_turns, temperature)));
        stages.push(Box::new(build_analysis_stage(2, max_turns, temperature)));
        stages.push(Box::new(build_analysis_stage(3, max_turns, temperature)));
        for n in 4..=7 {
            if include_stage(n) {
                stages.push(Box::new(build_analysis_stage(n, max_turns, temperature)));
            }
        }
        stages
    };

    let stage8 = Stage::builder("8")
        .system_prompt(cherry_pick_system_prompt(true))
        .user_prompt(PromptTemplate::new("{{user}}").with_var(
            "user",
            move |s: &CherryPickState| {
                (synthesis::stage8_builder())(
                    linux_patch_review::STAGE_DEDUPLICATION_INSTRUCTION,
                    s,
                )
                .0
            },
        ))
        .output_format(
            OutputFormat::json()
                .with_validator(validate_stage8)
                .with_feedback_formatter(format_stage8_feedback),
        )
        .tools(ToolScope::All)
        .max_turns(max_turns)
        .temperature(temperature)
        .on_recitation(recitation_reminder())
        .context_tag_suffix("s:8")
        .reduce(|state: &mut CherryPickState, val: Value| {
            if let Some(arr) = val.get("concerns").and_then(|v| v.as_array()) {
                state.concerns = arr.clone();
            }
            if let Some(arr) = val.get("dismissed_concerns").and_then(|v| v.as_array()) {
                state.dismissed_concerns = arr.clone();
            }
        })
        .build();

    let stage9 = Stage::builder("9")
        .system_prompt(cherry_pick_system_prompt(true))
        .user_prompt(PromptTemplate::new("{{user}}").with_var(
            "user",
            move |s: &CherryPickState| {
                (synthesis::stage9_builder())(
                    linux_patch_review::STAGE_CONFLICT_RESOLUTION_INSTRUCTION,
                    s,
                )
                .0
            },
        ))
        .output_format(
            OutputFormat::json()
                .with_validator(validate_stage9)
                .with_feedback_formatter(format_stage9_feedback),
        )
        .tools(ToolScope::All)
        .max_turns(max_turns)
        .temperature(temperature)
        .on_recitation(recitation_reminder())
        .context_tag_suffix("s:9")
        .reduce(|state: &mut CherryPickState, val: Value| {
            if let Some(arr) = val.get("concerns").and_then(|v| v.as_array()) {
                state.concerns = arr.clone();
            }
            if let Some(arr) = val.get("dismissed_concerns").and_then(|v| v.as_array()) {
                state.dismissed_concerns.extend(arr.iter().cloned());
            }
        })
        .build();

    let stage10 = Stage::builder("10")
        .system_prompt(cherry_pick_system_prompt(true))
        .user_prompt(
            PromptTemplate::new("{{user}}")
                .with_var("user", move |s: &CherryPickState| {
                    (synthesis::stage10_builder())(VERIFICATION, s).0
                })
                .include_file("false-positive-guide.md")
                .include_file("severity.md"),
        )
        .output_format(
            OutputFormat::json()
                .with_validator(validate_stage10)
                .with_feedback_formatter(format_stage10_feedback),
        )
        .tools(ToolScope::All)
        .max_turns(max_turns)
        .temperature(temperature)
        .on_recitation(recitation_reminder())
        .context_tag_suffix("s:10")
        .reduce(|state: &mut CherryPickState, val: Value| {
            state.findings = Some(val.get("findings").cloned().unwrap_or(val));
        })
        .build();

    let stage_origin = Stage::builder("10_origin")
        .system_prompt(cherry_pick_system_prompt(true))
        .user_prompt(
            PromptTemplate::new("{{user}}").with_var("user", |s: &CherryPickState| {
                (synthesis::origin_builder())(ORIGIN_CLASSIFICATION, s).0
            }),
        )
        .output_format(
            OutputFormat::json()
                .with_validator(validate_stage_origin)
                .with_feedback_formatter(format_stage10_feedback),
        )
        .tools(ToolScope::All)
        .max_turns(max_turns)
        .temperature(temperature)
        .on_recitation(recitation_reminder())
        .context_tag_suffix("s:10")
        .on_validation_exhausted(|_state: &mut CherryPickState| {
            tracing::warn!("Origin classification validation exhausted; keeping Stage 10 findings with default origin.");
        })
        .reduce(|state: &mut CherryPickState, val: Value| {
            state.findings = Some(val.get("findings").cloned().unwrap_or(val));
        })
        .build();

    let stage11 = Stage::builder("11")
        .system_prompt(cherry_pick_system_prompt(true))
        .user_prompt(
            PromptTemplate::new("{{user}}")
                .with_var("user", move |s: &CherryPickState| {
                    (synthesis::stage11_builder())(CONFLICT_REPORT, s).0
                })
                .include_file("inline-template.md"),
        )
        .output_format(OutputFormat::text_with_validator(
            validate_inline_format,
            format_stage11_feedback,
        ))
        .tools(ToolScope::All)
        .max_turns(max_turns)
        .temperature(temperature)
        .on_recitation(RecitationPolicy::FallbackToFreeForm {
            reminder: "\n\nCRITICAL: The previous attempt failed due to a RECITATION policy violation. Do NOT quote the original patch code at all. Instead, provide a free-form summary of the findings. Start your report with a note explaining that the format is altered due to recitation restrictions. Do not use the inline quoting style `>`.".to_string(),
        })
        .context_tag_suffix("s:11")
        .reduce(|state: &mut CherryPickState, text: String| {
            state.review_inline = Some(text);
        })
        .build();

    Workflow::builder("cherry-pick")
        .dynamic_parallel(planner, resolver, ParallelPolicy::FailFast)
        .early_exit_if(
            |s: &CherryPickState| s.concerns.is_empty(),
            "No concerns raised in analysis stages",
        )
        .stage(stage8)
        .early_exit_if(
            |s: &CherryPickState| s.concerns.is_empty(),
            "No concerns remaining after deduplication",
        )
        .stage(stage9)
        .early_exit_if(
            |s: &CherryPickState| s.concerns.is_empty(),
            "No concerns remaining after conflict resolution",
        )
        .stage(stage10)
        .early_exit_if(findings_empty, "No findings remaining after verification")
        .stage(stage_origin)
        .step(|state: &mut CherryPickState| {
            if let Some(f) = state.findings.take() {
                state.classified_findings = Some(f.clone());
                let filtered = filter_cherry_pick_findings(&f);
                if filtered.as_array().is_none_or(|a| a.is_empty()) {
                    state.review_inline =
                        Some("No issues found after conflict review filtering.".to_string());
                }
                state.findings = Some(filtered);
            }
        })
        .early_exit_if(
            findings_empty,
            "No findings remaining after conflict review filtering",
        )
        .stage(stage11)
        .build()
}

fn build_analysis_stage(
    num: u8,
    max_turns: usize,
    temperature: f32,
) -> Stage<CherryPickState, Value> {
    let name: &'static str = match num {
        1 => "1",
        2 => "2",
        3 => "3",
        4 => "4",
        5 => "5",
        6 => "6",
        7 => "7",
        _ => unreachable!("invalid stage number: {}", num),
    };
    let (instruction, guides, use_log): (&'static str, &'static [&'static str], bool) = match num {
        1 => (SEMANTIC_INTENT, &[], true),
        2 => (DROPPED_CHANGES, &[], true),
        3 => (MERGE_CORRECTNESS, &[], false),
        4 => {
            let stage = linux_patch_review::analysis_stage_by_name("resources").unwrap();
            (stage.instruction, stage.guides, stage.uses_commit_log)
        }
        5 => {
            let stage = linux_patch_review::analysis_stage_by_name("locking").unwrap();
            (stage.instruction, stage.guides, stage.uses_commit_log)
        }
        6 => {
            let stage = linux_patch_review::analysis_stage_by_name("security").unwrap();
            (stage.instruction, stage.guides, stage.uses_commit_log)
        }
        7 => {
            let stage = linux_patch_review::analysis_stage_by_name("hardware").unwrap();
            (stage.instruction, stage.guides, stage.uses_commit_log)
        }
        _ => unreachable!("invalid analysis stage number: {}", num),
    };

    let mut user_prompt =
        PromptTemplate::new(format!("{instruction}\n\n{ANALYSIS_FORMAT_GUIDANCE}"));
    for guide in guides {
        user_prompt = user_prompt.include_file(*guide);
    }

    Stage::builder(name)
        .system_prompt(cherry_pick_system_prompt(use_log))
        .user_prompt(user_prompt)
        .output_format(
            OutputFormat::json()
                .with_validator(validate_stages_1_to_7)
                .with_feedback_formatter(format_stage1_to_7_feedback),
        )
        .tools(ToolScope::All)
        .max_turns(max_turns)
        .temperature(temperature)
        .on_recitation(recitation_reminder())
        .context_tag_suffix(format!("s:{}", num))
        .reduce(move |state: &mut CherryPickState, val: Value| {
            if let Some(arr) = val.get("concerns").and_then(|v| v.as_array()) {
                for item in arr {
                    if let Some(tagged) = tag_source(item.clone(), num) {
                        state.total_concerns += 1;
                        state.concerns.push(tagged);
                    }
                }
            }
            if let Some(arr) = val.get("dismissed_concerns").and_then(|v| v.as_array()) {
                for item in arr {
                    if let Some(tagged) = tag_source(item.clone(), num) {
                        state.dismissed_concerns.push(tagged);
                    }
                }
            }
        })
        .build()
}

fn recitation_reminder() -> RecitationPolicy {
    RecitationPolicy::RetryWithReminder(
        "IMPORTANT: Your previous response was blocked by a recitation filter. \
         Please do NOT copy large blocks of code verbatim in your response. \
         Describe changes in prose, or use highly simplified pseudo-code if you must show code structure."
            .to_string(),
    )
}

fn tag_source(item: Value, stage: u8) -> Option<Value> {
    match item {
        Value::Object(mut map) => {
            map.insert("source_stage".to_string(), json!(stage));
            Some(Value::Object(map))
        }
        Value::String(text) => Some(json!({
            "source_stage": stage,
            "type": "General",
            "description": text,
        })),
        _ => None,
    }
}

fn findings_empty(state: &CherryPickState) -> bool {
    state
        .findings
        .as_ref()
        .and_then(|v| v.as_array())
        .is_none_or(|a| a.is_empty())
}

// ---------------------------------------------------------------------------
// Stage Validators & Feedback Formatters
// ---------------------------------------------------------------------------

fn validate_stages_1_to_7(val: &Value, _state: &CherryPickState) -> Result<(), String> {
    val.get("concerns")
        .and_then(Value::as_array)
        .ok_or_else(|| "JSON output is missing the required 'concerns' array".to_string())?;
    val.get("dismissed_concerns")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            "JSON output is missing the required 'dismissed_concerns' array".to_string()
        })?;
    Ok(())
}

fn format_stage1_to_7_feedback(violation: &str) -> String {
    let msg = if violation.starts_with("Failed to parse JSON from output:") {
        "JSON output is missing the required 'concerns' array"
    } else {
        violation
    };
    format!(
        "\n\nPrevious attempt was rejected: {}. You MUST return ONLY a JSON object containing 'concerns' and 'dismissed_concerns' arrays. If there are no concerns and no dismissed concerns, return `{{\"concerns\": [], \"dismissed_concerns\": []}}`.",
        msg
    )
}

fn validate_stage8(val: &Value, _state: &CherryPickState) -> Result<(), String> {
    if let Some(c) = val.get("concerns") {
        if !c.is_array() {
            return Err("output 'concerns' is not an array".to_string());
        }
    } else {
        return Err("missing 'concerns' array in output".to_string());
    }
    if let Some(c) = val.get("dismissed_concerns") {
        if !c.is_array() {
            return Err("output 'dismissed_concerns' is not an array".to_string());
        }
    } else {
        return Err("missing 'dismissed_concerns' array in output".to_string());
    }
    Ok(())
}

fn format_stage8_feedback(violation: &str) -> String {
    let msg = if violation.starts_with("Failed to parse JSON from output:") {
        "missing 'concerns' array in output"
    } else {
        violation
    };
    format!(
        "\n\nPrevious attempt was rejected: {}. You MUST return ONLY a JSON object containing 'concerns' and 'dismissed_concerns' arrays. If there are no concerns and no dismissed concerns, return `{{\"concerns\": [], \"dismissed_concerns\": []}}`.",
        msg
    )
}

fn validate_stage9(val: &Value, _state: &CherryPickState) -> Result<(), String> {
    if let Some(c) = val.get("concerns") {
        if !c.is_array() {
            return Err("output 'concerns' is not an array".to_string());
        }
    } else {
        return Err("missing 'concerns' array in output".to_string());
    }
    Ok(())
}

fn format_stage9_feedback(violation: &str) -> String {
    let msg = if violation.starts_with("Failed to parse JSON from output:") {
        "missing 'concerns' array in output"
    } else {
        violation
    };
    format!(
        "\n\nPrevious attempt was rejected: {}. You MUST return ONLY a JSON object containing 'concerns' array.",
        msg
    )
}

fn validate_stage10(val: &Value, _state: &CherryPickState) -> Result<(), String> {
    let arr = match val.get("findings").and_then(|f| f.as_array()) {
        Some(a) => a,
        None => return Err("missing 'findings' array in output".to_string()),
    };
    for finding in arr {
        let sev = match finding.get("severity").and_then(|v| v.as_str()) {
            Some(s) => s.to_lowercase(),
            None => return Err("missing or invalid 'severity' field in finding".to_string()),
        };
        if !matches!(sev.as_str(), "low" | "medium" | "high" | "critical") {
            return Err("missing or invalid 'severity' field in finding".to_string());
        }
    }
    Ok(())
}

fn validate_stage_origin(val: &Value, state: &CherryPickState) -> Result<(), String> {
    validate_stage10(val, state)?;
    let arr = val.get("findings").and_then(|f| f.as_array()).unwrap();
    for finding in arr {
        let origin = match finding.get("origin").and_then(|v| v.as_str()) {
            Some(o) => o.to_lowercase(),
            None => return Err("missing or invalid 'origin' field in finding".to_string()),
        };
        if !matches!(
            origin.as_str(),
            "resolution_introduced" | "original_patch_preexisting" | "base_preexisting"
        ) {
            return Err("missing or invalid 'origin' field in finding".to_string());
        }
    }
    Ok(())
}

fn format_stage10_feedback(violation: &str) -> String {
    let msg = if violation.starts_with("Failed to parse JSON from output:") {
        "missing 'findings' array in output"
    } else {
        violation
    };
    format!(
        "\n\nPrevious attempt was rejected: {}. You MUST return ONLY a JSON object containing 'findings' array.",
        msg
    )
}

fn validate_inline_format(content: &str, _state: &CherryPickState) -> Result<(), String> {
    if content.lines().any(|l| l.trim_start().starts_with("```")) {
        return Err("The output contains Markdown code blocks ('```'). It must be plain text as per `inline-template.md`.".to_string());
    }
    if !content.lines().any(|l| l.trim_start().starts_with('>')) {
        return Err("The output does not appear to quote any code or context using '>'. Please follow the quoting style in `inline-template.md`.".to_string());
    }
    let has_commit_header = content
        .lines()
        .take(20)
        .any(|l| l.trim_start().to_lowercase().starts_with("commit "));
    if !has_commit_header {
        return Err("The output is missing the 'commit <hash>' header. Please start with the commit details (Commit, Author, Subject) as per `inline-template.md`.".to_string());
    }
    let has_author_header = content
        .lines()
        .take(20)
        .any(|l| l.trim_start().to_lowercase().starts_with("author:"));
    if !has_author_header {
        return Err("The output is missing the 'Author: <name>' header. Please start with the commit details (Commit, Author, Subject) as per `inline-template.md`.".to_string());
    }
    let has_comments = content.lines().any(|l| {
        let trimmed = l.trim();
        if trimmed.is_empty() || trimmed.starts_with('>') {
            return false;
        }
        let lower = trimmed.to_lowercase();
        !lower.starts_with("commit ")
            && !lower.starts_with("author:")
            && !lower.starts_with("date:")
            && !lower.starts_with("link:")
    });
    if !has_comments {
        return Err("The output appears to lack any comments or summary. You must include a summary and interspersed comments explaining the findings.".to_string());
    }
    Ok(())
}

fn format_stage11_feedback(violation: &str) -> String {
    format!(
        "Previous attempt was rejected: {}. Please correct your output format.",
        violation
    )
}

// ---------------------------------------------------------------------------
// Context & Git Hydration Helpers
// ---------------------------------------------------------------------------

fn extract_resolution_diff(patchset: &Value) -> String {
    patchset
        .get("patches")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.get("diff").and_then(|d| d.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

async fn git_output(worktree: &Path, args: &[&str]) -> Option<String> {
    let output = crate::git_cmd::in_dir_async(worktree)
        .args(args)
        .output()
        .await
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

async fn git_subject(worktree: &Path, sha: &str) -> Option<String> {
    git_output(
        worktree,
        &["show", "-s", "--format=%s", "--end-of-options", sha],
    )
    .await
    .map(|s| s.trim().to_string())
}

async fn git_show(worktree: &Path, sha: &str) -> Option<String> {
    git_output(worktree, &["show", sha]).await
}

#[cfg(test)]
fn build_git_metadata(
    original_sha: &str,
    base_sha: Option<&str>,
    resolution_sha: &str,
    original_subject: Option<&str>,
    base_subject: Option<&str>,
    resolution_subject: Option<&str>,
    original_diff: Option<&str>,
) -> String {
    let unknown = "(unknown)";
    let orig_subj = original_subject.unwrap_or(unknown);
    let base_subj = base_subject.unwrap_or(unknown);
    let res_subj = resolution_subject.unwrap_or(unknown);
    let base_sha = base_sha.unwrap_or(unknown);

    let mut m = MERGE_CONFLICT_REVIEW_FRAMING
        .replace("{{original_sha}}", original_sha)
        .replace("{{original_subject}}", orig_subj)
        .replace("{{base_sha}}", base_sha)
        .replace("{{base_subject}}", base_subj)
        .replace("{{resolution_sha}}", resolution_sha)
        .replace("{{resolution_subject}}", res_subj);

    if let Some(diff) = original_diff {
        let diff = diff.trim();
        if !diff.is_empty() {
            m.push_str(
                "\nFor direct comparison, the ORIGINAL PATCH (commit 1) diff follows. \
                 Compare it against the resolution diff to spot dropped hunks, altered \
                 logic, or merge artifacts:\n",
            );
            m.push_str("<original_patch_diff>\n");
            m.push_str(diff);
            m.push_str("\n</original_patch_diff>\n");
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiRequest, AiResponse, ProviderCapabilities};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct QueuedMockProvider {
        responses: Mutex<VecDeque<String>>,
        requests: Mutex<Vec<AiRequest>>,
    }

    impl QueuedMockProvider {
        fn new(resps: Vec<&str>) -> Self {
            Self {
                responses: Mutex::new(resps.into_iter().map(String::from).collect()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl AiProvider for QueuedMockProvider {
        async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
            self.requests.lock().unwrap().push(request);
            let content = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| r#"{"concerns": [], "dismissed_concerns": []}"#.to_string());
            Ok(AiResponse {
                content: Some(content),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: Some(crate::ai::AiUsage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cached_tokens: Some(1),
                }),
                truncated: false,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    #[test]
    fn git_metadata_contains_three_commits() {
        let md = build_git_metadata(
            "aaa111",
            Some("bbb222"),
            "ccc333",
            Some("orig subject"),
            Some("base subject"),
            Some("res subject"),
            Some("diff --git a/x b/x"),
        );
        assert!(md.contains("aaa111"));
        assert!(md.contains("bbb222"));
        assert!(md.contains("ccc333"));
        assert!(md.contains("orig subject"));
        assert!(md.contains("<original_patch_diff>"));
        assert!(md.contains("MERGE-CONFLICT RESOLUTION REVIEW"));
    }

    #[test]
    fn from_review_kind_extracts_shas() {
        let kind = ReviewKind::CherryPick {
            original_sha: "orig".to_string(),
            base_sha: Some("base".to_string()),
        };
        let p = CherryPickReviewWorkflow::from_review_kind(&kind, "res".to_string());
        assert_eq!(p.original_sha, "orig");
        assert_eq!(p.base_sha.as_deref(), Some("base"));
        assert_eq!(p.resolution_sha, "res");
    }

    #[test]
    fn stage8_builder_injects_state() {
        let state = CherryPickState {
            concerns: vec![serde_json::json!({"type": "TestConcern"})],
            ..Default::default()
        };
        let build = synthesis::stage8_builder();
        let (user, clean) = build("# Stage 8 instruction", &state);
        assert!(user.contains("# Stage 8 instruction"));
        assert!(user.contains("Consolidated Concerns:"));
        assert!(user.contains("TestConcern"));
        assert_eq!(user, clean);
    }

    #[tokio::test]
    async fn execute_workflow_early_exit_when_no_concerns() {
        let tmp = tempfile::tempdir().unwrap();
        let prompts = PromptRegistry::new(tmp.path().to_path_buf());
        // Planning response + 4 analysis stage responses (stages 1, 2, 3 + planned 4)
        let provider = Arc::new(QueuedMockProvider::new(vec![
            r#"{"relevant_stages": [4]}"#,
            r#"{"concerns": [], "dismissed_concerns": []}"#,
            r#"{"concerns": [], "dismissed_concerns": []}"#,
            r#"{"concerns": [], "dismissed_concerns": []}"#,
            r#"{"concerns": [], "dismissed_concerns": []}"#,
        ]));
        let env = CherryPickWorkflowEnv {
            provider: provider.clone(),
            tools: Arc::new(ToolBox::new(tmp.path().to_path_buf(), None)),
            prompts: &prompts,
            temperature: 0.0,
            max_interactions: 3,
            context_tag: Some("[ps:1 p:1] ".to_string()),
            stages: None,
            series_range: None,
        };
        let workflow = CherryPickReviewWorkflow::new("orig", Some("base".to_string()), "res");
        let result = execute_workflow(&workflow, &env, json!({"subject": "test"}), None)
            .await
            .unwrap();

        let out = result.output.unwrap();
        assert_eq!(out["review_inline"], "No issues found.");
        assert_eq!(out["concerns_count"], 0);
        // Planning usage (10 in, 5 out) + 4 analysis stages * 10 = 50 tokens_in, 25 tokens_out
        assert_eq!(result.tokens_in, 50);
        assert_eq!(result.tokens_out, 25);

        // Verify planning request had system configured and context_tag = None,
        // while stage 1 had context_tag "[ps:1 p:1 s:1] "
        let reqs = provider.requests.lock().unwrap();
        assert!(reqs[0].system.is_some());
        assert_eq!(reqs[1].context_tag.as_deref(), Some("[ps:1 p:1 s:1] "));
    }

    #[tokio::test]
    async fn execute_workflow_full_run_and_progress_events() {
        let tmp = tempfile::tempdir().unwrap();
        let prompts = PromptRegistry::new(tmp.path().to_path_buf());
        let report_text = "commit abcdef123456\nAuthor: Test Author <test@example.com>\n\nSummary of conflict resolution bug.\n\n> +bad_code();\n\nThis introduces a memory leak.";
        let provider = Arc::new(QueuedMockProvider::new(vec![
            // Stage 1 raises a concern
            r#"{"concerns": [{"type": "Bug", "description": "leak"}], "dismissed_concerns": []}"#,
            // Stages 2 & 3 raise nothing
            r#"{"concerns": [], "dismissed_concerns": []}"#,
            r#"{"concerns": [], "dismissed_concerns": []}"#,
            // Stage 8 dedup
            r#"{"concerns": [{"type": "Bug", "description": "leak"}], "dismissed_concerns": []}"#,
            // Stage 9 resolution
            r#"{"concerns": [{"type": "Bug", "description": "leak"}]}"#,
            // Stage 10 verification
            r#"{"findings": [{"problem": "leak", "severity": "high"}]}"#,
            // Origin classification (stage 10)
            r#"{"findings": [
                {"problem": "leak", "severity": "high", "origin": "resolution_introduced"},
                {"problem": "old", "severity": "high", "origin": "base_preexisting"}
            ]}"#,
            // Stage 11 report
            report_text,
        ]));
        let env = CherryPickWorkflowEnv {
            provider,
            tools: Arc::new(ToolBox::new(tmp.path().to_path_buf(), None)),
            prompts: &prompts,
            temperature: 0.0,
            max_interactions: 3,
            context_tag: None,
            // Manual stages = [1, 2, 3] so planner is skipped
            stages: Some(vec![1, 2, 3]),
            series_range: None,
        };
        let workflow = CherryPickReviewWorkflow::new("orig", Some("base".to_string()), "res");
        let events = std::sync::Mutex::new(Vec::new());
        let progress = |ev: WorkerProgressEvent| {
            events.lock().unwrap().push(ev);
        };
        let result = execute_workflow(&workflow, &env, json!({"subject": "test"}), Some(&progress))
            .await
            .unwrap();

        let out = result.output.unwrap();
        assert_eq!(out["review_inline"], report_text);
        // classified_findings retains both findings (before filter)
        assert_eq!(out["classified_findings"].as_array().unwrap().len(), 2);
        // findings has filtered findings (only resolution_introduced high/crit)
        assert_eq!(out["findings"].as_array().unwrap().len(), 1);
        assert_eq!(out["concerns_count"], 1);
        // History starts with System prompt
        assert_eq!(result.history[0].role, crate::ai::AiRole::System);

        // Verify StageStarted events emitted for 1, 2, 3, 8, 9, 10, 10 (origin), 11
        let started_stages: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                WorkerProgressEvent::StageStarted { stage } => Some(stage.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            started_stages,
            vec!["1", "2", "3", "8", "9", "10", "10", "11"]
        );
    }
}
