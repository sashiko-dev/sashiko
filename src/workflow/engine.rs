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
use super::stage::{ExecutableStage, WorkflowEnv};

/// Execution outcome and aggregated metrics from running a workflow.
#[derive(Debug, Clone, Default)]
pub struct WorkflowOutcome {
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
    pub history: Vec<AiMessage>,
    pub early_exit: bool,
    pub early_exit_reason: Option<&'static str>,
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

        for step in &workflow.steps {
            match step {
                WorkflowStep::Stage(stage) => {
                    let stage_outcome = stage.execute(env, state, event_cb).await?;
                    outcome.tokens_in += stage_outcome.tokens_in;
                    outcome.tokens_out += stage_outcome.tokens_out;
                    outcome.tokens_cached += stage_outcome.tokens_cached;
                    outcome.history.extend(stage_outcome.history);
                }

                WorkflowStep::Parallel { stages, policy } => {
                    execute_parallel_batch(stages, *policy, env, state, event_cb, &mut outcome)
                        .await?;
                }

                WorkflowStep::DynamicParallel {
                    planner,
                    resolver,
                    policy,
                } => {
                    let planner_outcome = planner.execute(env, state, event_cb).await?;
                    outcome.tokens_in += planner_outcome.tokens_in;
                    outcome.tokens_out += planner_outcome.tokens_out;
                    outcome.tokens_cached += planner_outcome.tokens_cached;
                    outcome.history.extend(planner_outcome.history);

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
                            &mut outcome,
                        )
                        .await?;
                    }
                }

                WorkflowStep::Branch {
                    condition,
                    then_flow,
                    else_flow,
                } => {
                    let branch_outcome = if condition(state) {
                        Box::pin(Self::execute(then_flow, env, state, event_cb)).await?
                    } else if let Some(else_flow) = else_flow {
                        Box::pin(Self::execute(else_flow, env, state, event_cb)).await?
                    } else {
                        WorkflowOutcome::default()
                    };

                    outcome.tokens_in += branch_outcome.tokens_in;
                    outcome.tokens_out += branch_outcome.tokens_out;
                    outcome.tokens_cached += branch_outcome.tokens_cached;
                    outcome.history.extend(branch_outcome.history);

                    if branch_outcome.early_exit {
                        outcome.early_exit = true;
                        outcome.early_exit_reason = branch_outcome.early_exit_reason;
                        break;
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

        if let Some(cb) = event_cb {
            cb(WorkflowEvent::WorkflowFinished {
                name: workflow.name,
                total_tokens: outcome.tokens_in + outcome.tokens_out,
            });
        }

        Ok(outcome)
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
            let futures = stages
                .iter()
                .map(|stage| stage.execute_isolated(env, state, event_cb));
            let results = futures::future::try_join_all(futures).await?;

            for (stage_outcome, mutation) in results {
                mutation(state);
                outcome.tokens_in += stage_outcome.tokens_in;
                outcome.tokens_out += stage_outcome.tokens_out;
                outcome.tokens_cached += stage_outcome.tokens_cached;
                outcome.history.extend(stage_outcome.history);
            }
        }

        ParallelPolicy::BestEffort => {
            let futures = stages
                .iter()
                .map(|stage| stage.execute_isolated(env, state, event_cb));
            let results = futures::future::join_all(futures).await;

            for (stage, res) in stages.iter().zip(results) {
                match res {
                    Ok((stage_outcome, mutation)) => {
                        mutation(state);
                        outcome.tokens_in += stage_outcome.tokens_in;
                        outcome.tokens_out += stage_outcome.tokens_out;
                        outcome.tokens_cached += stage_outcome.tokens_cached;
                        outcome.history.extend(stage_outcome.history);
                    }
                    Err(err) => {
                        warn!(
                            "Parallel stage '{}' failed under BestEffort policy: {}",
                            stage.name(),
                            err
                        );
                    }
                }
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
            Ok(AiResponse {
                content: Some(content),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
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

    #[tokio::test]
    async fn test_workflow_sequential_and_early_exit() {
        let provider = Arc::new(MockProvider::single(r#"{"items": ["leak in foo"]}"#));
        let tmp = tempfile::tempdir().unwrap();
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf(), None));
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
        let tools = Arc::new(ToolBox::new(tmp.path().to_path_buf(), None));
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
}
