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

//! Recovery for review stages with completed candidate concerns.

use crate::workflow::{Workflow, WorkflowStep};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::num::NonZeroUsize;

use crate::workflow::{ExecutableStage, StageOutcome, StateMutation, WorkflowEnv, WorkflowEvent};

use super::linux_patch_review::{LinuxPatchReviewState, ReviewStageError, ReviewStageKind};

/// Controls whether a review preserves partial results after a stage failure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReviewFailurePolicy {
    #[default]
    Abort,
    PreservePartial,
}

/// Tracks verified work independently of the number of unresolved candidates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VerificationProgress {
    #[default]
    Pending,
    PostVerification {
        remaining: NonZeroUsize,
        completed: usize,
    },
    Complete,
}

impl VerificationProgress {
    pub fn after_verification(hard_cases: usize) -> Self {
        match NonZeroUsize::new(hard_cases) {
            Some(remaining) => Self::PostVerification {
                remaining,
                completed: 0,
            },
            None => Self::Complete,
        }
    }

    pub fn resolve_cases(&mut self, count: usize) {
        if let Self::PostVerification {
            remaining,
            completed,
        } = *self
        {
            *self = match NonZeroUsize::new(remaining.get().saturating_sub(count)) {
                Some(remaining) => Self::PostVerification {
                    remaining,
                    completed: completed.saturating_add(count),
                },
                None => Self::Complete,
            };
        }
    }

    pub fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }

    pub fn rank(self) -> (u8, usize) {
        match self {
            Self::Pending => (0, 0),
            Self::PostVerification { completed, .. } => (1, completed),
            Self::Complete => (2, 0),
        }
    }
}

pub struct RecoverableStage {
    inner: Box<dyn ExecutableStage<LinuxPatchReviewState>>,
    kind: ReviewStageKind,
    candidates: Vec<Value>,
}

impl RecoverableStage {
    pub fn boxed(
        inner: Box<dyn ExecutableStage<LinuxPatchReviewState>>,
        kind: ReviewStageKind,
    ) -> Box<dyn ExecutableStage<LinuxPatchReviewState>> {
        Box::new(Self {
            inner,
            kind,
            candidates: Vec::new(),
        })
    }
    pub fn boxed_with_candidates(
        inner: Box<dyn ExecutableStage<LinuxPatchReviewState>>,
        candidates: Vec<Value>,
    ) -> Box<dyn ExecutableStage<LinuxPatchReviewState>> {
        Box::new(Self {
            inner,
            kind: ReviewStageKind::PostVerification,
            candidates,
        })
    }
}

/// Preserves the workflow topology while making validation and reporting recoverable.
pub fn recover_workflow(
    mut workflow: Workflow<LinuxPatchReviewState>,
) -> Workflow<LinuxPatchReviewState> {
    workflow.steps = workflow
        .steps
        .into_iter()
        .map(|step| match step {
            WorkflowStep::DynamicParallel {
                planner,
                resolver,
                policy,
            } if planner.name() == "verification" => WorkflowStep::DynamicParallel {
                planner: RecoverableStage::boxed(planner, ReviewStageKind::Verification),
                resolver,
                policy,
            },
            WorkflowStep::Stage(stage) if stage.name() == "report" => {
                WorkflowStep::Stage(RecoverableStage::boxed(stage, ReviewStageKind::Report))
            }
            WorkflowStep::Stage(stage) if stage.name() == "summary" => {
                WorkflowStep::Stage(RecoverableStage::boxed(stage, ReviewStageKind::Summary))
            }
            step => step,
        })
        .collect();
    workflow
}

#[async_trait]
impl ExecutableStage<LinuxPatchReviewState> for RecoverableStage {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    async fn execute_isolated(
        &self,
        env: &WorkflowEnv<'_>,
        state: &LinuxPatchReviewState,
        event_cb: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
    ) -> Result<(StageOutcome, StateMutation<LinuxPatchReviewState>)> {
        match self.inner.execute_isolated(env, state, event_cb).await {
            Ok((outcome, mutation)) => {
                let verification_skipped =
                    self.kind == ReviewStageKind::Verification && outcome.skipped;
                Ok((
                    outcome,
                    Box::new(move |state| {
                        mutation(state);
                        if verification_skipped {
                            state.verification_progress = VerificationProgress::Complete;
                        }
                    }),
                ))
            }
            Err(error) => {
                let stage = self.name();
                let message = format!("{stage}: {error}");
                tracing::warn!("Review stage failed; continuing with candidates: {message}");
                let outcome = crate::workflow::engine::stage_outcome_from_error(&error);
                if let Some(cb) = event_cb {
                    cb(WorkflowEvent::StageFinished {
                        stage_name: stage,
                        tokens_in: outcome.tokens_in,
                        tokens_out: outcome.tokens_out,
                        tokens_cached: outcome.tokens_cached,
                    });
                }
                let kind = self.kind;
                let candidates = self.candidates.clone();
                Ok((
                    outcome,
                    Box::new(move |state| {
                        recover(state, kind, message);
                        state.unverified_concerns.extend(candidates);
                    }),
                ))
            }
        }
    }
}

fn recover(state: &mut LinuxPatchReviewState, kind: ReviewStageKind, message: String) {
    state.stage_errors.push(ReviewStageError {
        stage: kind,
        message,
    });
    if kind == ReviewStageKind::Verification {
        state.unverified_concerns.clone_from(&state.all_concerns);
        state
            .unverified_concerns
            .extend_from_slice(&state.all_dismissed_concerns);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::ai::{AiProvider, AiRequest, AiResponse, ProviderCapabilities};
    use crate::toolbox::ToolBox;
    use crate::workflow::{Workflow, WorkflowEngine};
    use serde_json::json;

    struct FailingProvider;

    #[test]
    fn verification_progress_tracks_successful_batches() {
        let mut progress = VerificationProgress::after_verification(3);
        assert!(!progress.is_complete());
        let initial = progress.rank();
        progress.resolve_cases(2);
        assert!(progress.rank() > initial);
        assert!(!progress.is_complete());
        progress.resolve_cases(1);
        assert!(progress.is_complete());
        assert!(VerificationProgress::after_verification(0).is_complete());
    }

    #[async_trait]
    impl AiProvider for FailingProvider {
        async fn generate_content(&self, _: AiRequest) -> Result<AiResponse> {
            anyhow::bail!("simulated provider failure")
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            UnusedProvider.get_capabilities()
        }
    }

    #[tokio::test]
    async fn production_stages_recover_only_under_local_policy() -> Result<()> {
        use crate::project::ProjectId;
        use crate::workflows::{linux_patch_review as linux, sashiko_patch_review as sashiko};

        let temp = tempfile::tempdir()?;
        let env = WorkflowEnv {
            provider: Arc::new(FailingProvider),
            tools: Arc::new(ToolBox::new(temp.path().to_path_buf(), None)),
            base_dir: temp.path(),
            context_tag: None,
        };
        for project in [ProjectId::Linux, ProjectId::Sashiko] {
            for policy in [
                ReviewFailurePolicy::default(),
                ReviewFailurePolicy::PreservePartial,
            ] {
                for stage_name in ["verification", "post-verification", "report", "summary"] {
                    if stage_name == "summary" && project == ProjectId::Linux {
                        continue;
                    }
                    let candidate = json!({"description": "candidate", "reasoning": "evidence"});
                    let mut state = LinuxPatchReviewState {
                        failure_policy: policy,
                        verification_progress: if stage_name == "post-verification" {
                            VerificationProgress::after_verification(1)
                        } else if matches!(stage_name, "report" | "summary") {
                            VerificationProgress::Complete
                        } else {
                            VerificationProgress::Pending
                        },
                        all_concerns: vec![candidate.clone()],
                        hard_cases: if stage_name == "post-verification" {
                            vec![candidate]
                        } else {
                            Vec::new()
                        },
                        findings: if stage_name == "report" {
                            vec![json!({"problem": "verified"})]
                        } else {
                            Vec::new()
                        },
                        ..Default::default()
                    };
                    let mut workflow = match project {
                        ProjectId::Linux => {
                            linux::build_linux_patch_review_workflow_with_options(1, 0.0)
                        }
                        ProjectId::Sashiko => {
                            sashiko::build_sashiko_patch_review_workflow_with_options(1, 0.0)
                        }
                    };
                    if stage_name == "post-verification" {
                        let stages = match project {
                            ProjectId::Linux => {
                                linux::resolve_post_verification_stages_with_options(&state, 1, 0.0)
                            }
                            ProjectId::Sashiko => {
                                sashiko::resolve_post_verification_stages_with_options(
                                    &state, 1, 0.0,
                                )
                            }
                        };
                        assert!(!stages.is_empty());
                        workflow.steps = vec![WorkflowStep::Parallel {
                            stages,
                            policy: crate::workflow::ParallelPolicy::BestEffort,
                        }];
                    } else {
                        workflow.steps.retain(|step| match step {
                            WorkflowStep::Stage(stage) => stage.name() == stage_name,
                            WorkflowStep::DynamicParallel { planner, .. } => {
                                planner.name() == stage_name
                            }
                            _ => false,
                        });
                    }
                    assert_eq!(workflow.steps.len(), 1);
                    if policy == ReviewFailurePolicy::PreservePartial {
                        workflow = recover_workflow(workflow);
                    }
                    let result = WorkflowEngine::execute(&workflow, &env, &mut state, None).await;
                    match policy {
                        ReviewFailurePolicy::Abort => {
                            let error = result.unwrap_err();
                            assert!(
                                format!("{error:#}").contains("simulated provider failure"),
                                "{project:?} {stage_name}: {error:#}"
                            );
                            assert!(state.stage_errors.is_empty());
                            assert!(state.unverified_concerns.is_empty());
                        }
                        ReviewFailurePolicy::PreservePartial => {
                            result?;
                            assert_eq!(state.stage_errors.len(), 1);
                            assert!(
                                state.stage_errors[0]
                                    .message
                                    .contains("simulated provider failure")
                            );
                            if !matches!(stage_name, "report" | "summary") {
                                assert!(!state.unverified_concerns.is_empty());
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    struct UnusedProvider;

    #[async_trait]
    impl AiProvider for UnusedProvider {
        async fn generate_content(&self, _: AiRequest) -> Result<AiResponse> {
            anyhow::bail!("test stages do not call the provider")
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "unused".into(),
                context_window_size: 1000,
            }
        }
    }

    struct FailedVerification;

    #[async_trait]
    impl ExecutableStage<LinuxPatchReviewState> for FailedVerification {
        fn name(&self) -> &'static str {
            "verification"
        }

        async fn execute_isolated(
            &self,
            _: &WorkflowEnv<'_>,
            _: &LinuxPatchReviewState,
            _: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
        ) -> Result<(StageOutcome, StateMutation<LinuxPatchReviewState>)> {
            anyhow::bail!("output truncated")
        }
    }

    struct ValidateCandidates;

    #[async_trait]
    impl ExecutableStage<LinuxPatchReviewState> for ValidateCandidates {
        fn name(&self) -> &'static str {
            "verification"
        }

        async fn execute_isolated(
            &self,
            _: &WorkflowEnv<'_>,
            state: &LinuxPatchReviewState,
            _: Option<&(dyn Fn(WorkflowEvent) + Send + Sync)>,
        ) -> Result<(StageOutcome, StateMutation<LinuxPatchReviewState>)> {
            assert_eq!(state.unverified_concerns[0]["description"], "candidate");
            Ok((
                StageOutcome::default(),
                Box::new(|state| {
                    state.findings.push(json!({"problem": "verified"}));
                    state.unverified_concerns.clear();
                    state.verification_progress = VerificationProgress::Complete;
                }),
            ))
        }
    }

    #[tokio::test]
    async fn failed_verification_preserves_candidates_for_later_stages() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let env = WorkflowEnv {
            provider: Arc::new(UnusedProvider),
            tools: Arc::new(ToolBox::new(temp.path().to_path_buf(), None)),
            base_dir: temp.path(),
            context_tag: None,
        };
        let workflow = Workflow::builder("recover")
            .executable_stage(RecoverableStage::boxed(
                Box::new(FailedVerification),
                ReviewStageKind::Verification,
            ))
            .executable_stage(Box::new(ValidateCandidates))
            .build();
        let mut state = LinuxPatchReviewState {
            all_concerns: vec![json!({"description": "candidate"})],
            ..Default::default()
        };

        WorkflowEngine::execute(&workflow, &env, &mut state, None).await?;
        assert_eq!(state.stage_errors.len(), 1);
        assert_eq!(state.findings[0]["problem"], "verified");
        assert!(state.unverified_concerns.is_empty());
        Ok(())
    }

    #[test]
    fn failed_verification_keeps_candidates_for_partial_output() {
        let mut state = LinuxPatchReviewState {
            all_concerns: vec![json!({"description": "candidate"})],
            ..Default::default()
        };
        recover(
            &mut state,
            ReviewStageKind::Verification,
            "verification: timed out".into(),
        );
        assert_eq!(state.unverified_concerns, state.all_concerns);
        assert!(!state.verification_progress.is_complete());
    }

    #[tokio::test]
    async fn failed_post_verification_keeps_its_batch_and_completed_findings() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let env = WorkflowEnv {
            provider: Arc::new(UnusedProvider),
            tools: Arc::new(ToolBox::new(temp.path().to_path_buf(), None)),
            base_dir: temp.path(),
            context_tag: None,
        };
        let candidate = json!({"id": "H2", "description": "unresolved hard case"});
        let workflow = Workflow::builder("partial_post_verification")
            .executable_stage(RecoverableStage::boxed_with_candidates(
                Box::new(FailedVerification),
                vec![candidate.clone()],
            ))
            .build();
        let finding = json!({"problem": "completed finding"});
        let mut state = LinuxPatchReviewState {
            findings: vec![finding.clone()],
            verification_progress: VerificationProgress::after_verification(1),
            ..Default::default()
        };

        WorkflowEngine::execute(&workflow, &env, &mut state, None).await?;
        assert_eq!(state.findings, vec![finding]);
        assert_eq!(state.unverified_concerns, vec![candidate]);
        assert_eq!(
            state.stage_errors[0].stage,
            ReviewStageKind::PostVerification
        );
        assert!(!state.verification_progress.is_complete());
        Ok(())
    }
}
