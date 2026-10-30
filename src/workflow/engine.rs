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

//! Runtime execution engine for declarative workflows.

use anyhow::Result;
use tracing::{info, warn};

use crate::ai::AiMessage;

use super::events::WorkflowEvent;
use super::graph::{Workflow, WorkflowStep};
use super::policy::ParallelPolicy;
use super::stage::{ExecutableStage, StageOutcome, WorkflowEnv};

/// Execution telemetry and transcript slice bounds for a single stage.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct StageRunRecord {
    pub name: String,
    pub skipped: bool,
    pub turns: usize,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
    pub history_start: usize,
    pub history_end: usize,
}

/// Execution outcome and aggregated metrics from running a workflow.
#[derive(Debug, Clone, Default)]
pub struct WorkflowOutcome {
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
    pub history: Vec<AiMessage>,
    pub stage_runs: Vec<StageRunRecord>,
    pub early_exit: bool,
    pub early_exit_reason: Option<&'static str>,
}

/// Error wrapper attached by [`WorkflowEngine::execute`] when a workflow aborts,
/// preserving the partial [`WorkflowOutcome`] accumulated across all completed
/// and failed stages up to the point of failure.
#[derive(Debug, Clone)]
pub struct WorkflowFailure {
    pub message: String,
    pub outcome: WorkflowOutcome,
}

impl std::fmt::Display for WorkflowFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WorkflowFailure {}

fn stage_outcome_from_error(err: &anyhow::Error) -> StageOutcome {
    if let Some(failure) = err.downcast_ref::<crate::ai::SessionFailure>() {
        StageOutcome {
            tokens_in: u32::try_from(failure.usage.prompt_tokens).unwrap_or(u32::MAX),
            tokens_out: u32::try_from(failure.usage.completion_tokens).unwrap_or(u32::MAX),
            tokens_cached: u32::try_from(failure.usage.cached_tokens.unwrap_or(0))
                .unwrap_or(u32::MAX),
            history: failure.history.clone(),
            skipped: false,
        }
    } else {
        StageOutcome {
            skipped: false,
            ..Default::default()
        }
    }
}

fn record_stage_outcome(
    outcome: &mut WorkflowOutcome,
    stage_name: &str,
    stage_outcome: StageOutcome,
) {
    let history_start = outcome.history.len();
    let skipped = stage_outcome.skipped();
    let turns = stage_outcome.turns();
    outcome.tokens_in = outcome.tokens_in.saturating_add(stage_outcome.tokens_in);
    outcome.tokens_out = outcome.tokens_out.saturating_add(stage_outcome.tokens_out);
    outcome.tokens_cached = outcome
        .tokens_cached
        .saturating_add(stage_outcome.tokens_cached);
    outcome.history.extend(stage_outcome.history);
    let history_end = outcome.history.len();
    outcome.stage_runs.push(StageRunRecord {
        name: stage_name.to_string(),
        skipped,
        turns,
        tokens_in: stage_outcome.tokens_in,
        tokens_out: stage_outcome.tokens_out,
        tokens_cached: stage_outcome.tokens_cached,
        history_start,
        history_end,
    });
}

fn merge_workflow_outcome(outcome: &mut WorkflowOutcome, branch_outcome: WorkflowOutcome) {
    let base_history_len = outcome.history.len();
    outcome.tokens_in = outcome.tokens_in.saturating_add(branch_outcome.tokens_in);
    outcome.tokens_out = outcome.tokens_out.saturating_add(branch_outcome.tokens_out);
    outcome.tokens_cached = outcome
        .tokens_cached
        .saturating_add(branch_outcome.tokens_cached);
    outcome.history.extend(branch_outcome.history);
    for mut rec in branch_outcome.stage_runs {
        rec.history_start = rec.history_start.saturating_add(base_history_len);
        rec.history_end = rec.history_end.saturating_add(base_history_len);
        outcome.stage_runs.push(rec);
    }
}

/// The runtime engine that drives workflow execution.
pub struct WorkflowEngine;

impl WorkflowEngine {
    /// Executes a workflow against the given environment and mutable state.
    pub async fn execute<S: Send + Sync + 'static>(
        workflow: &Workflow<S>,
        env: &WorkflowEnv<'_>,
        state: &mut S,
        event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
    ) -> Result<WorkflowOutcome> {
        if let Some(cb) = event_cb {
            cb(WorkflowEvent::WorkflowStarted {
                name: workflow.name,
            });
        }

        let mut outcome = WorkflowOutcome::default();
        if let Err(err) = Self::execute_steps(workflow, env, state, event_cb, &mut outcome).await {
            let message = err.to_string();
            return Err(err.context(WorkflowFailure { message, outcome }));
        }

        if let Some(cb) = event_cb {
            cb(WorkflowEvent::WorkflowFinished {
                name: workflow.name,
                total_tokens: outcome.tokens_in.saturating_add(outcome.tokens_out),
            });
        }

        Ok(outcome)
    }

    async fn execute_steps<S: Send + Sync + 'static>(
        workflow: &Workflow<S>,
        env: &WorkflowEnv<'_>,
        state: &mut S,
        event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
        outcome: &mut WorkflowOutcome,
    ) -> Result<()> {
        for step in &workflow.steps {
            match step {
                WorkflowStep::Stage(stage) => match stage.execute(env, state, event_cb).await {
                    Ok(stage_outcome) => record_stage_outcome(outcome, stage.name(), stage_outcome),
                    Err(err) => {
                        record_stage_outcome(outcome, stage.name(), stage_outcome_from_error(&err));
                        return Err(err);
                    }
                },

                WorkflowStep::Parallel { stages, policy } => {
                    execute_parallel_batch(stages, *policy, env, state, event_cb, outcome).await?;
                }

                WorkflowStep::DynamicParallel {
                    planner,
                    resolver,
                    policy,
                } => {
                    let planner_outcome = match planner.execute(env, state, event_cb).await {
                        Ok(out) => out,
                        Err(err) => {
                            record_stage_outcome(
                                outcome,
                                planner.name(),
                                stage_outcome_from_error(&err),
                            );
                            return Err(err);
                        }
                    };
                    record_stage_outcome(outcome, planner.name(), planner_outcome);

                    let dynamic_stages = resolver(state);
                    if let Some(cb) = event_cb {
                        cb(WorkflowEvent::ParallelResolved {
                            stage_names: dynamic_stages.iter().map(|s| s.name()).collect(),
                        });
                    }
                    if !dynamic_stages.is_empty() {
                        execute_parallel_batch(
                            &dynamic_stages,
                            *policy,
                            env,
                            state,
                            event_cb,
                            outcome,
                        )
                        .await?;
                    }
                }

                WorkflowStep::ResolvedParallel { resolver, policy } => {
                    let dynamic_stages = resolver(state);
                    if let Some(cb) = event_cb {
                        cb(WorkflowEvent::ParallelResolved {
                            stage_names: dynamic_stages.iter().map(|s| s.name()).collect(),
                        });
                    }
                    if !dynamic_stages.is_empty() {
                        execute_parallel_batch(
                            &dynamic_stages,
                            *policy,
                            env,
                            state,
                            event_cb,
                            outcome,
                        )
                        .await?;
                    }
                }

                WorkflowStep::Branch {
                    condition,
                    then_flow,
                    else_flow,
                } => {
                    let branch_res = if condition(state) {
                        Some(Box::pin(Self::execute(then_flow, env, state, event_cb)).await)
                    } else if let Some(else_flow) = else_flow {
                        Some(Box::pin(Self::execute(else_flow, env, state, event_cb)).await)
                    } else {
                        None
                    };

                    if let Some(res) = branch_res {
                        match res {
                            Ok(branch_outcome) => {
                                let early_exit = branch_outcome.early_exit;
                                let early_exit_reason = branch_outcome.early_exit_reason;
                                merge_workflow_outcome(outcome, branch_outcome);
                                if early_exit {
                                    outcome.early_exit = true;
                                    outcome.early_exit_reason = early_exit_reason;
                                    break;
                                }
                            }
                            Err(err) => {
                                if let Some(wf) = err.downcast_ref::<WorkflowFailure>() {
                                    merge_workflow_outcome(outcome, wf.outcome.clone());
                                }
                                return Err(err);
                            }
                        }
                    }
                }

                WorkflowStep::EarlyExitIf { condition, reason } => {
                    if condition(state) {
                        info!("Workflow '{}' early exit: {}", workflow.name, reason);
                        if let Some(cb) = event_cb {
                            cb(WorkflowEvent::EarlyExitTriggered { reason });
                        }
                        outcome.early_exit = true;
                        outcome.early_exit_reason = Some(reason);
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}

async fn execute_parallel_batch<S: Send + Sync + 'static>(
    stages: &[Box<dyn ExecutableStage<S>>],
    policy: ParallelPolicy,
    env: &WorkflowEnv<'_>,
    state: &mut S,
    event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
    outcome: &mut WorkflowOutcome,
) -> Result<()> {
    info!("Running {} stages concurrently", stages.len());

    match policy {
        ParallelPolicy::FailFast => {
            let state_ref: &S = state;
            let completed_outcomes: Vec<std::sync::Mutex<Option<StageOutcome>>> = (0..stages.len())
                .map(|_| std::sync::Mutex::new(None))
                .collect();
            let outcomes_ref = &completed_outcomes;
            let futures = stages.iter().enumerate().map(|(idx, stage)| async move {
                match stage.execute_isolated(env, state_ref, event_cb).await {
                    Ok((stage_outcome, mutation)) => {
                        if let Ok(mut slot) = outcomes_ref[idx].lock() {
                            *slot = Some(stage_outcome);
                        }
                        Ok(mutation)
                    }
                    Err(err) => {
                        if let Ok(mut slot) = outcomes_ref[idx].lock() {
                            *slot = Some(stage_outcome_from_error(&err));
                        }
                        Err(err)
                    }
                }
            });
            match futures_util::future::try_join_all(futures).await {
                Ok(mutations) => {
                    for ((stage, slot), mutation) in
                        stages.iter().zip(completed_outcomes).zip(mutations)
                    {
                        mutation(state);
                        if let Ok(Some(stage_outcome)) = slot.into_inner() {
                            record_stage_outcome(outcome, stage.name(), stage_outcome);
                        }
                    }
                }
                Err(err) => {
                    for (stage, slot) in stages.iter().zip(completed_outcomes) {
                        if let Ok(Some(stage_outcome)) = slot.into_inner() {
                            record_stage_outcome(outcome, stage.name(), stage_outcome);
                        }
                    }
                    return Err(err);
                }
            }
        }

        ParallelPolicy::BestEffort => {
            let futures = stages
                .iter()
                .map(|stage| stage.execute_isolated(env, state, event_cb));
            let results = futures_util::future::join_all(futures).await;

            let mut successes = 0usize;
            let mut first_error = None;
            let mut required_error = None;

            for (stage, res) in stages.iter().zip(results) {
                match res {
                    Ok((stage_outcome, mutation)) => {
                        successes = successes.saturating_add(1);
                        mutation(state);
                        record_stage_outcome(outcome, stage.name(), stage_outcome);
                    }
                    Err(err) => {
                        warn!(
                            "Parallel stage '{}' failed under BestEffort policy: {}",
                            stage.name(),
                            err
                        );
                        record_stage_outcome(outcome, stage.name(), stage_outcome_from_error(&err));
                        if stage.is_required() {
                            if required_error.is_none() {
                                required_error = Some(err.context(format!(
                                    "Required parallel stage '{}' failed",
                                    stage.name()
                                )));
                            }
                        } else if first_error.is_none() {
                            first_error = Some(err.context(format!(
                                "All {} parallel stages failed under BestEffort policy (first failure in stage '{}')",
                                stages.len(),
                                stage.name()
                            )));
                        }
                    }
                }
            }

            if let Some(err) = required_error {
                return Err(err);
            }
            if !stages.is_empty()
                && successes == 0
                && let Some(err) = first_error
            {
                return Err(err);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiProvider, AiRequest, AiResponse, ProviderCapabilities};
    use crate::toolbox::ToolBox;
    use crate::workflow::output::OutputFormat;
    use crate::workflow::prompt::PromptTemplate;
    use crate::workflow::stage::Stage;
    use serde::Deserialize;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default, Clone)]
    struct DummyState {
        concerns: Vec<String>,
        #[allow(dead_code)]
        findings: Vec<String>,
        selected_stages: Vec<u8>,
    }

    #[derive(Deserialize, Debug)]
    struct DummyConcernsOutput {
        items: Vec<String>,
    }

    #[derive(Deserialize, Debug)]
    struct DummyPlanningOutput {
        stages: Vec<u8>,
    }

    struct MockProvider {
        response_json: String,
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
        call_count: AtomicUsize,
    }

    impl MockProvider {
        fn single(resp: &str) -> Self {
            Self {
                response_json: resp.to_string(),
                responses: std::sync::Mutex::new(std::collections::VecDeque::new()),
                call_count: AtomicUsize::new(0),
            }
        }

        fn queued(resps: Vec<String>) -> Self {
            Self {
                response_json: String::new(),
                responses: std::sync::Mutex::new(resps.into()),
                call_count: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl AiProvider for MockProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let content = {
                let mut queue = self.responses.lock().unwrap();
                queue
                    .pop_front()
                    .unwrap_or_else(|| self.response_json.clone())
            };
            if content == "__ERROR__" {
                anyhow::bail!("simulated fatal provider error");
            }
            Ok(AiResponse {
                content: Some(content),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: Some(crate::ai::AiUsage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cached_tokens: None,
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

    #[tokio::test]
    async fn test_workflow_sequential_and_early_exit() {
        let provider = Arc::new(MockProvider::single(r#"{"items": ["leak in foo"]}"#));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf()));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
        };

        let mut state = DummyState::default();

        let workflow = Workflow::builder("test_flow")
            .stage(
                Stage::builder("stage_1")
                    .user_prompt(PromptTemplate::new("Analyze"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.concerns.extend(out.items);
                    })
                    .build(),
            )
            .early_exit_if(|s| s.concerns.is_empty(), "no concerns")
            .stage(
                Stage::builder("stage_2")
                    .user_prompt(PromptTemplate::new("Verify"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.findings.extend(out.items);
                    })
                    .build(),
            )
            .build();

        let outcome = WorkflowEngine::execute(&workflow, &env, &mut state, None)
            .await
            .unwrap();

        assert!(!outcome.early_exit);
        assert_eq!(state.concerns, vec!["leak in foo".to_string()]);
        assert_eq!(state.findings, vec!["leak in foo".to_string()]);
    }

    #[tokio::test]
    async fn test_workflow_dynamic_parallel_planning() {
        let provider = Arc::new(MockProvider::queued(vec![
            r#"{"stages": [4, 5]}"#.to_string(),
            r#"{"items": ["concern_a"]}"#.to_string(),
            r#"{"items": ["concern_b"]}"#.to_string(),
        ]));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf()));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
        };

        let mut state = DummyState::default();

        let workflow = Workflow::builder("planning_flow")
            .dynamic_parallel(
                Stage::builder("planner")
                    .user_prompt(PromptTemplate::new("Plan stages"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyPlanningOutput| {
                        s.selected_stages = out.stages;
                    })
                    .build(),
                |s| {
                    let mut stages: Vec<Box<dyn ExecutableStage<DummyState>>> = Vec::new();
                    for &n in &s.selected_stages {
                        let stage_name: &'static str = match n {
                            4 => "stage_4",
                            5 => "stage_5",
                            _ => "unknown",
                        };
                        stages.push(Box::new(
                            Stage::builder(stage_name)
                                .user_prompt(PromptTemplate::new("Run dynamic stage"))
                                .output_format(OutputFormat::json())
                                .reduce(move |st: &mut DummyState, out: DummyConcernsOutput| {
                                    for item in out.items {
                                        st.concerns.push(format!("{}: {}", n, item));
                                    }
                                })
                                .build(),
                        ));
                    }
                    stages
                },
                ParallelPolicy::FailFast,
            )
            .build();

        let resolved = std::sync::Mutex::new(Vec::new());
        let record = |event: WorkflowEvent| {
            if let WorkflowEvent::ParallelResolved { stage_names } = event {
                resolved.lock().unwrap().extend(stage_names);
            }
        };
        let outcome = WorkflowEngine::execute(&workflow, &env, &mut state, Some(&record))
            .await
            .unwrap();

        assert_eq!(state.selected_stages, vec![4, 5]);
        assert_eq!(state.concerns.len(), 2);
        assert!(!outcome.early_exit);
        // The plan is reported as resolved, not guessed from the stage list.
        assert_eq!(*resolved.lock().unwrap(), vec!["stage_4", "stage_5"]);
    }

    #[tokio::test]
    async fn test_workflow_best_effort_partial_success() {
        let provider = Arc::new(MockProvider::queued(vec![
            "__ERROR__".to_string(),
            r#"{"items": ["concern_ok"]}"#.to_string(),
        ]));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf()));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
        };

        let mut state = DummyState::default();
        let workflow = Workflow::builder("best_effort_partial")
            .parallel(
                vec![
                    Box::new(
                        Stage::builder("stage_fail")
                            .user_prompt(PromptTemplate::new("Fail"))
                            .output_format(OutputFormat::json())
                            .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                                s.concerns.extend(out.items);
                            })
                            .build(),
                    ),
                    Box::new(
                        Stage::builder("stage_ok")
                            .user_prompt(PromptTemplate::new("Succeed"))
                            .output_format(OutputFormat::json())
                            .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                                s.concerns.extend(out.items);
                            })
                            .build(),
                    ),
                ],
                ParallelPolicy::BestEffort,
            )
            .build();

        let outcome = WorkflowEngine::execute(&workflow, &env, &mut state, None)
            .await
            .expect("BestEffort should succeed when at least one stage succeeds");
        assert!(!outcome.early_exit);
        assert_eq!(state.concerns, vec!["concern_ok".to_string()]);
        assert_eq!(outcome.stage_runs.len(), 2);
        assert_eq!(outcome.stage_runs[0].name, "stage_fail");
        assert!(!outcome.stage_runs[0].skipped);
        assert_eq!(outcome.stage_runs[0].history_start, 0);
        assert_eq!(outcome.stage_runs[0].history_end, 1);
        assert_eq!(outcome.stage_runs[1].name, "stage_ok");
        assert!(!outcome.stage_runs[1].skipped);
        assert_eq!(outcome.stage_runs[1].history_start, 1);
        assert_eq!(outcome.stage_runs[1].history_end, 3);
    }

    #[tokio::test]
    async fn test_workflow_best_effort_required_stage_failure_aborts() {
        let provider = Arc::new(MockProvider::queued(vec![
            "__ERROR__".to_string(),
            r#"{"items": ["concern_ok"]}"#.to_string(),
        ]));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf()));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
        };

        let mut state = DummyState::default();
        let workflow = Workflow::builder("best_effort_required")
            .parallel(
                vec![
                    Box::new(
                        Stage::builder("stage_required")
                            .user_prompt(PromptTemplate::new("Fail"))
                            .output_format(OutputFormat::json())
                            .required()
                            .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                                s.concerns.extend(out.items);
                            })
                            .build(),
                    ),
                    Box::new(
                        Stage::builder("stage_ok")
                            .user_prompt(PromptTemplate::new("Succeed"))
                            .output_format(OutputFormat::json())
                            .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                                s.concerns.extend(out.items);
                            })
                            .build(),
                    ),
                ],
                ParallelPolicy::BestEffort,
            )
            .build();

        let err = WorkflowEngine::execute(&workflow, &env, &mut state, None)
            .await
            .expect_err("a failed required stage must abort a BestEffort batch");
        assert!(format!("{err:#}").contains("stage_required"));
    }

    #[tokio::test]
    async fn test_workflow_best_effort_total_failure() {
        let provider = Arc::new(MockProvider::single("__ERROR__"));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf()));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
        };

        let mut state = DummyState::default();
        let workflow = Workflow::builder("best_effort_total_fail")
            .parallel(
                vec![
                    Box::new(
                        Stage::builder("stage_fail_1")
                            .user_prompt(PromptTemplate::new("Fail 1"))
                            .output_format(OutputFormat::json())
                            .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                                s.concerns.extend(out.items);
                            })
                            .build(),
                    ),
                    Box::new(
                        Stage::builder("stage_fail_2")
                            .user_prompt(PromptTemplate::new("Fail 2"))
                            .output_format(OutputFormat::json())
                            .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                                s.concerns.extend(out.items);
                            })
                            .build(),
                    ),
                ],
                ParallelPolicy::BestEffort,
            )
            .build();

        let res = WorkflowEngine::execute(&workflow, &env, &mut state, None).await;
        assert!(
            res.is_err(),
            "BestEffort should fail when all parallel stages fail"
        );
    }

    #[tokio::test]
    async fn test_workflow_stage_runs_telemetry_and_history_bounds() {
        let provider = Arc::new(MockProvider::queued(vec![
            r#"{"items": ["c1"]}"#.to_string(),
            r#"{"items": ["c2"]}"#.to_string(),
        ]));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf()));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
        };

        let mut state = DummyState::default();
        let workflow = Workflow::builder("telemetry_flow")
            .stage(
                Stage::builder("stage_run_1")
                    .user_prompt(PromptTemplate::new("First"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.concerns.extend(out.items);
                    })
                    .build(),
            )
            .stage(
                Stage::builder("stage_skipped")
                    .user_prompt(PromptTemplate::new("Skipped"))
                    .output_format(OutputFormat::json())
                    .skip_if(|_: &DummyState| true)
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.concerns.extend(out.items);
                    })
                    .build(),
            )
            .stage(
                Stage::builder("stage_run_2")
                    .user_prompt(PromptTemplate::new("Second"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.findings.extend(out.items);
                    })
                    .build(),
            )
            .build();

        let outcome = WorkflowEngine::execute(&workflow, &env, &mut state, None)
            .await
            .unwrap();

        assert_eq!(outcome.stage_runs.len(), 3);
        assert_eq!(outcome.stage_runs[0].name, "stage_run_1");
        assert!(!outcome.stage_runs[0].skipped);
        assert_eq!(outcome.stage_runs[0].turns, 1);
        assert_eq!(outcome.stage_runs[0].history_start, 0);
        assert_eq!(outcome.stage_runs[0].history_end, 2);

        assert_eq!(outcome.stage_runs[1].name, "stage_skipped");
        assert!(outcome.stage_runs[1].skipped);
        assert_eq!(outcome.stage_runs[1].turns, 0);
        assert_eq!(outcome.stage_runs[1].history_start, 2);
        assert_eq!(outcome.stage_runs[1].history_end, 2);

        assert_eq!(outcome.stage_runs[2].name, "stage_run_2");
        assert!(!outcome.stage_runs[2].skipped);
        assert_eq!(outcome.stage_runs[2].turns, 1);
        assert_eq!(outcome.stage_runs[2].history_start, 2);
        assert_eq!(outcome.stage_runs[2].history_end, 4);
        assert_eq!(outcome.history.len(), 4);
    }

    #[tokio::test]
    async fn test_skipped_stage_emits_skipped_event_and_no_started_or_finished() {
        let provider = Arc::new(MockProvider::single(r#"{"items": ["c1"]}"#));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf()));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
        };

        let mut state = DummyState::default();
        let workflow = Workflow::builder("skip_event_flow")
            .stage(
                Stage::builder("stage_skipped")
                    .user_prompt(PromptTemplate::new("Skipped"))
                    .output_format(OutputFormat::json())
                    .skip_if(|_: &DummyState| true)
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.concerns.extend(out.items);
                    })
                    .build(),
            )
            .stage(
                Stage::builder("stage_run")
                    .user_prompt(PromptTemplate::new("Run"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.concerns.extend(out.items);
                    })
                    .build(),
            )
            .build();

        let events = std::sync::Mutex::new(Vec::new());
        let record = |event: WorkflowEvent| {
            let label = match event {
                WorkflowEvent::StageStarted { stage_name } => format!("started:{stage_name}"),
                WorkflowEvent::StageFinished { stage_name, .. } => {
                    format!("finished:{stage_name}")
                }
                WorkflowEvent::StageSkipped { stage_name } => format!("skipped:{stage_name}"),
                _ => return,
            };
            events.lock().unwrap().push(label);
        };
        WorkflowEngine::execute(&workflow, &env, &mut state, Some(&record))
            .await
            .unwrap();

        // A skipped stage says so once, and never claims to have started or
        // finished, so a progress display can drop it from its plan.
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "skipped:stage_skipped".to_string(),
                "started:stage_run".to_string(),
                "finished:stage_run".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn test_workflow_failure_preserves_accumulated_outcome_on_sequential_error() {
        let provider = Arc::new(MockProvider::queued(vec![
            r#"{"items": ["c1"]}"#.to_string(),
            "not valid json".to_string(),
            "still not valid json".to_string(),
            "still not valid json".to_string(),
        ]));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf()));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
        };

        let mut state = DummyState::default();
        let workflow = Workflow::builder("failure_telemetry_flow")
            .stage(
                Stage::builder("stage_ok")
                    .user_prompt(PromptTemplate::new("First"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.concerns.extend(out.items);
                    })
                    .build(),
            )
            .stage(
                Stage::builder("stage_fail")
                    .user_prompt(PromptTemplate::new("Second"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                        s.findings.extend(out.items);
                    })
                    .build(),
            )
            .build();

        let err = WorkflowEngine::execute(&workflow, &env, &mut state, None)
            .await
            .expect_err("second stage should fail");
        let wf = err
            .downcast_ref::<WorkflowFailure>()
            .expect("WorkflowFailure should be attached to error");
        assert_eq!(wf.outcome.stage_runs.len(), 2);
        assert_eq!(wf.outcome.stage_runs[0].name, "stage_ok");
        assert_eq!(wf.outcome.stage_runs[1].name, "stage_fail");
        assert_eq!(wf.outcome.tokens_in, 40);
        assert_eq!(wf.outcome.tokens_out, 20);
        assert!(!wf.outcome.history.is_empty());
    }

    #[tokio::test]
    async fn test_workflow_fail_fast_preserves_completed_and_failed_sibling_outcomes() {
        let provider = Arc::new(MockProvider::queued(vec![
            r#"{"items": ["c1"]}"#.to_string(),
            "not valid json".to_string(),
            "still not valid json".to_string(),
            "still not valid json".to_string(),
        ]));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf()));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
        };

        let mut state = DummyState::default();
        let workflow = Workflow::builder("fail_fast_parallel_telemetry")
            .parallel(
                vec![
                    Box::new(
                        Stage::builder("stage_ok")
                            .user_prompt(PromptTemplate::new("First"))
                            .output_format(OutputFormat::json())
                            .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                                s.concerns.extend(out.items);
                            })
                            .build(),
                    ),
                    Box::new(
                        Stage::builder("stage_fail")
                            .user_prompt(PromptTemplate::new("Second"))
                            .output_format(OutputFormat::json())
                            .reduce(|s: &mut DummyState, out: DummyConcernsOutput| {
                                s.findings.extend(out.items);
                            })
                            .build(),
                    ),
                ],
                ParallelPolicy::FailFast,
            )
            .build();

        let err = WorkflowEngine::execute(&workflow, &env, &mut state, None)
            .await
            .expect_err("FailFast parallel batch should fail when a stage fails");
        assert!(
            state.concerns.is_empty(),
            "FailFast must not apply state mutations when any sibling stage fails"
        );
        let wf = err
            .downcast_ref::<WorkflowFailure>()
            .expect("WorkflowFailure should be attached to error");
        assert_eq!(wf.outcome.stage_runs.len(), 2);
        assert_eq!(wf.outcome.stage_runs[0].name, "stage_ok");
        assert_eq!(wf.outcome.stage_runs[1].name, "stage_fail");
        assert_eq!(wf.outcome.tokens_in, 40);
        assert_eq!(wf.outcome.tokens_out, 20);
    }

    #[tokio::test]
    async fn test_workflow_resolved_parallel_after_dynamic_parallel() {
        let provider = Arc::new(MockProvider::queued(vec![
            r#"{"stages": [1]}"#.to_string(),
            r#"{"items": ["from_dyn"]}"#.to_string(),
            r#"{"items": ["from_res_1"]}"#.to_string(),
            r#"{"items": ["from_res_2"]}"#.to_string(),
        ]));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf()));
        let env = WorkflowEnv {
            provider,
            tools,
            base_dir: tmp.path(),
            context_tag: None,
        };

        let mut state = DummyState::default();
        let workflow = Workflow::builder("resolved_parallel_flow")
            .dynamic_parallel(
                Stage::builder("planner")
                    .user_prompt(PromptTemplate::new("Plan"))
                    .output_format(OutputFormat::json())
                    .reduce(|s: &mut DummyState, out: DummyPlanningOutput| {
                        s.selected_stages = out.stages;
                    })
                    .build(),
                |s: &DummyState| {
                    s.selected_stages
                        .iter()
                        .map(|idx| {
                            let name = match *idx {
                                1 => "dyn_1",
                                _ => "dyn_other",
                            };
                            Box::new(
                                Stage::builder(name)
                                    .user_prompt(PromptTemplate::new("Dyn"))
                                    .output_format(OutputFormat::json())
                                    .reduce(|st: &mut DummyState, out: DummyConcernsOutput| {
                                        st.concerns.extend(out.items);
                                    })
                                    .build(),
                            )
                                as Box<dyn crate::workflow::stage::ExecutableStage<DummyState>>
                        })
                        .collect()
                },
                ParallelPolicy::BestEffort,
            )
            .resolved_parallel(
                |s: &DummyState| {
                    if s.concerns.is_empty() {
                        return Vec::new();
                    }
                    vec![
                        Box::new(
                            Stage::builder("res_1")
                                .user_prompt(PromptTemplate::new("Res 1"))
                                .output_format(OutputFormat::json())
                                .reduce(|st: &mut DummyState, out: DummyConcernsOutput| {
                                    st.findings.extend(out.items);
                                })
                                .build(),
                        ),
                        Box::new(
                            Stage::builder("res_2")
                                .user_prompt(PromptTemplate::new("Res 2"))
                                .output_format(OutputFormat::json())
                                .reduce(|st: &mut DummyState, out: DummyConcernsOutput| {
                                    st.findings.extend(out.items);
                                })
                                .build(),
                        ),
                    ]
                },
                ParallelPolicy::BestEffort,
            )
            .build();

        let resolved_batches = std::sync::Mutex::new(Vec::new());
        let record = |event: WorkflowEvent| {
            if let WorkflowEvent::ParallelResolved { stage_names } = event {
                resolved_batches.lock().unwrap().push(stage_names);
            }
        };

        let outcome = WorkflowEngine::execute(&workflow, &env, &mut state, Some(&record))
            .await
            .unwrap();
        assert!(!outcome.early_exit);
        assert_eq!(state.concerns, vec!["from_dyn"]);
        assert_eq!(state.findings, vec!["from_res_1", "from_res_2"]);
        assert_eq!(
            *resolved_batches.lock().unwrap(),
            vec![
                vec!["dyn_1".to_string()],
                vec!["res_1".to_string(), "res_2".to_string()],
            ]
        );
    }
}
