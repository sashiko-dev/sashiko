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

//! Recovery for consolidation stages with completed candidate concerns.

use anyhow::Result;
use async_trait::async_trait;

use crate::workflow::stage::StageFailure;
use crate::workflow::{ExecutableStage, StageOutcome, StateMutation, WorkflowEnv, WorkflowEvent};

use super::linux_patch_review::{LinuxPatchReviewState, ReviewStageError, ReviewStageKind};

pub struct RecoverableStage {
    inner: Box<dyn ExecutableStage<LinuxPatchReviewState>>,
    kind: ReviewStageKind,
}

impl RecoverableStage {
    pub fn boxed(
        inner: Box<dyn ExecutableStage<LinuxPatchReviewState>>,
        kind: ReviewStageKind,
    ) -> Box<dyn ExecutableStage<LinuxPatchReviewState>> {
        Box::new(Self { inner, kind })
    }
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
            Ok(result) => Ok(result),
            Err(error) => {
                let stage = self.name();
                let message = format!("{stage}: {error}");
                tracing::warn!("Review stage failed; continuing with candidates: {message}");
                let usage = error
                    .downcast_ref::<StageFailure>()
                    .map(|failure| failure.usage.clone())
                    .unwrap_or_default();
                if let Some(cb) = event_cb {
                    cb(WorkflowEvent::StageFinished {
                        stage_name: stage,
                        tokens_in: usage.tokens_in,
                        tokens_out: usage.tokens_out,
                        tokens_cached: usage.tokens_cached,
                    });
                }
                let kind = self.kind;
                Ok((usage, Box::new(move |state| recover(state, kind, message))))
            }
        }
    }
}

fn recover(state: &mut LinuxPatchReviewState, kind: ReviewStageKind, message: String) {
    state.stage_errors.push(ReviewStageError {
        stage: kind,
        message,
    });
    match kind {
        ReviewStageKind::Deduplication => {
            state.unverified_concerns.clone_from(&state.all_concerns);
        }
        ReviewStageKind::ConflictResolution => {
            state
                .unverified_concerns
                .clone_from(&state.deduplicated_concerns);
        }
        ReviewStageKind::Verification if state.unverified_concerns.is_empty() => {
            state.unverified_concerns.clone_from(&state.patch_concerns);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::Mutex;

    use crate::ai::{AiProvider, AiRequest, AiResponse, AiUsage, ProviderCapabilities};
    use crate::toolbox::ToolBox;
    use crate::workflow::{OutputFormat, PromptTemplate, Stage, Workflow, WorkflowEngine};
    use serde_json::json;

    struct UnusedProvider;

    struct SequenceProvider(Mutex<VecDeque<AiResponse>>);

    #[async_trait]
    impl AiProvider for SequenceProvider {
        async fn generate_content(&self, _: AiRequest) -> Result<AiResponse> {
            self.0
                .lock()
                .expect("test provider lock")
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("no more responses"))
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "test".into(),
                context_window_size: 1000,
            }
        }
    }

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

    struct FailedDeduplication;

    #[async_trait]
    impl ExecutableStage<LinuxPatchReviewState> for FailedDeduplication {
        fn name(&self) -> &'static str {
            "deduplication"
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
                    state.verification_complete = true;
                }),
            ))
        }
    }

    #[tokio::test]
    async fn failed_deduplication_continues_into_verification() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let env = WorkflowEnv {
            provider: Arc::new(UnusedProvider),
            tools: Arc::new(ToolBox::new(temp.path().to_path_buf(), None)),
            base_dir: temp.path(),
            context_tag: None,
        };
        let workflow = Workflow::builder("recover")
            .executable_stage(RecoverableStage::boxed(
                Box::new(FailedDeduplication),
                ReviewStageKind::Deduplication,
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

    #[tokio::test]
    async fn failed_stage_keeps_usage_from_retries_and_truncated_response() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let responses = [
            AiResponse {
                content: Some("invalid json".into()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: Some(AiUsage {
                    prompt_tokens: 10,
                    completion_tokens: 3,
                    total_tokens: 13,
                    cached_tokens: Some(2),
                }),
                truncated: false,
            },
            AiResponse {
                content: Some("{\"incomplete\":".into()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: Some(AiUsage {
                    prompt_tokens: 12,
                    completion_tokens: 5,
                    total_tokens: 17,
                    cached_tokens: Some(4),
                }),
                truncated: true,
            },
        ];
        let env = WorkflowEnv {
            provider: Arc::new(SequenceProvider(Mutex::new(responses.into()))),
            tools: Arc::new(ToolBox::new(temp.path().to_path_buf(), None)),
            base_dir: temp.path(),
            context_tag: None,
        };
        let stage: Stage<LinuxPatchReviewState, serde_json::Value> =
            Stage::builder("deduplication")
                .user_prompt(PromptTemplate::new("review"))
                .output_format(OutputFormat::json())
                .reduce(|_, _| {})
                .build();
        let workflow = Workflow::builder("recover")
            .executable_stage(RecoverableStage::boxed(
                Box::new(stage),
                ReviewStageKind::Deduplication,
            ))
            .build();
        let mut state = LinuxPatchReviewState::default();
        let events = Mutex::new(Vec::new());
        let event_cb = |event| events.lock().expect("test events lock").push(event);

        let outcome = WorkflowEngine::execute(&workflow, &env, &mut state, Some(&event_cb)).await?;
        assert_eq!(state.stage_errors.len(), 1);
        assert_eq!(outcome.tokens_in, 22);
        assert_eq!(outcome.tokens_out, 8);
        assert_eq!(outcome.tokens_cached, 6);
        assert!(
            events
                .lock()
                .expect("test events lock")
                .iter()
                .any(|event| {
                    matches!(
                        event,
                        WorkflowEvent::StageFinished {
                            stage_name: "deduplication",
                            tokens_in: 22,
                            tokens_out: 8,
                            tokens_cached: 6,
                        }
                    )
                })
        );
        Ok(())
    }

    #[test]
    fn failed_verification_keeps_candidates_for_partial_output() {
        let mut state = LinuxPatchReviewState {
            patch_concerns: vec![json!({"description": "candidate"})],
            ..Default::default()
        };
        recover(
            &mut state,
            ReviewStageKind::Verification,
            "verification: timed out".into(),
        );
        assert_eq!(state.unverified_concerns, state.patch_concerns);
        assert!(!state.verification_complete);
    }
}
