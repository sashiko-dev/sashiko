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

use crate::ai::{AiMessage, AiProvider, AiRequest, AiResponseFormat, AiRole};
use crate::fixups::{
    FixupGenerationConfig, PreparedCandidateFixup, candidate_fixup_output_schema,
    parse_candidate_fixup_output, prepare_candidate_fixups,
};
use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;

const STAGE1_FIXUP_SYSTEM_PROMPT: &str = r#"You generate conservative candidate fixup patches for Linux kernel patch review.

Generate only Stage 1 candidate fixups:
- spelling fixes in comments or documentation;
- comment typo fixes;
- documentation grammar or wording fixes;
- small kerneldoc wording fixes.

Do not generate behavior-changing code patches. Do not generate subjective style churn.
Do not rewrite large sections. Do not invent fixes for unverified issues. If there is no
small, obvious, low-risk fixup, return an empty candidate_fixups array.

Every candidate fixup must be a unified git diff that applies on top of the reviewed
worktree. Keep each patch small, local, and easy for a human author to inspect. Label
risk and confidence conservatively. Prefer returning no fixup over returning a risky or
uncertain patch."#;

/// Shared request object for candidate-fixup generation.
#[derive(Debug, Clone)]
pub struct FixupGenerationRequest<'a> {
    pub patchset: &'a serde_json::Value,
    pub review: Option<&'a serde_json::Value>,
    pub worktree: Option<&'a Path>,
    pub temperature: Option<f32>,
    pub context_tag: Option<String>,
}

/// Generate, parse, and validate candidate fixups using the shared policy path.
///
/// This service owns only AI orchestration. The caller remains responsible for
/// persistence so local and daemon review paths can choose when and how to store
/// generated candidates.
pub async fn generate_candidate_fixups(
    provider: Arc<dyn AiProvider>,
    config: &FixupGenerationConfig,
    request: FixupGenerationRequest<'_>,
) -> Result<Vec<PreparedCandidateFixup>> {
    if !config.enabled || config.max_fixups_per_patchset == 0 {
        return Ok(Vec::new());
    }

    let ai_request = build_fixup_ai_request(config, &request)?;
    let response = provider
        .generate_content(ai_request)
        .await
        .context("candidate fixup generation request failed")?;

    let raw = response
        .content
        .as_deref()
        .context("candidate fixup generation returned no content")?;
    let output = parse_candidate_fixup_output(raw)
        .context("candidate fixup generation returned invalid structured output")?;

    Ok(prepare_candidate_fixups(
        output.candidate_fixups,
        &config.validation_policy,
        request.worktree,
        config.max_fixups_per_patchset,
    )
    .await)
}

fn build_fixup_ai_request(
    config: &FixupGenerationConfig,
    request: &FixupGenerationRequest<'_>,
) -> Result<AiRequest> {
    let user_prompt = build_fixup_user_prompt(config, request)?;
    Ok(AiRequest {
        system: Some(STAGE1_FIXUP_SYSTEM_PROMPT.to_string()),
        messages: vec![AiMessage {
            role: AiRole::User,
            content: Some(user_prompt),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        tools: None,
        temperature: request.temperature,
        response_format: Some(AiResponseFormat::Json {
            schema: Some(candidate_fixup_output_schema()),
        }),
        context_tag: request.context_tag.clone(),
    })
}

fn build_fixup_user_prompt(
    config: &FixupGenerationConfig,
    request: &FixupGenerationRequest<'_>,
) -> Result<String> {
    let patchset_json = serde_json::to_string_pretty(request.patchset)
        .context("failed to serialize patchset for fixup generation")?;
    let review_json = match request.review {
        Some(review) => serde_json::to_string_pretty(review)
            .context("failed to serialize review result for fixup generation")?,
        None => "null".to_string(),
    };

    Ok(format!(
        "Generate candidate fixup patches for this review.\n\n\
         Generation mode: {mode}\n\
         Maximum candidates: {max_fixups}\n\
         Maximum patch lines per candidate: {max_lines}\n\
         Minimum confidence: {min_confidence}\n\
         Maximum risk: {max_risk}\n\n\
         Only return Stage 1 trivial or low-risk documentation/comment/spelling/kerneldoc fixups.\n\
         Return an empty candidate_fixups array when no safe candidate exists.\n\n\
         Patchset JSON:\n{patchset_json}\n\n\
         Final review JSON, if available:\n{review_json}",
        mode = config.mode,
        max_fixups = config.max_fixups_per_patchset,
        max_lines = config.validation_policy.max_lines,
        min_confidence = config.validation_policy.min_confidence,
        max_risk = config.validation_policy.max_risk,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiResponse, AiUsage, ProviderCapabilities};
    use crate::fixups::{CandidateFixup, FixupCategory, FixupConfidence, FixupRisk};
    use async_trait::async_trait;
    use serde_json::json;
    use tokio::sync::Mutex;

    struct MockProvider {
        response: String,
        requests: Mutex<Vec<AiRequest>>,
    }

    #[async_trait]
    impl AiProvider for MockProvider {
        async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
            self.requests.lock().await.push(request);
            Ok(AiResponse {
                content: Some(self.response.clone()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: Some(AiUsage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cached_tokens: None,
                }),
                truncated: false,
            })
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            1
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".to_string(),
                context_window_size: 1000,
            }
        }
    }

    fn spelling_fixup() -> CandidateFixup {
        CandidateFixup {
            title: "Fix spelling in comment".to_string(),
            category: FixupCategory::Spelling,
            rationale: "The comment says recieve instead of receive.".to_string(),
            confidence: FixupConfidence::High,
            applies_to_finding_id: None,
            applies_to_suggestion_id: None,
            patch: "diff --git a/drivers/foo/bar.c b/drivers/foo/bar.c\n--- a/drivers/foo/bar.c\n+++ b/drivers/foo/bar.c\n@@ -1 +1 @@\n-/* recieve */\n+/* receive */\n".to_string(),
            files_touched: vec!["drivers/foo/bar.c".to_string()],
            risk: FixupRisk::Trivial,
            requires_human_testing: false,
        }
    }

    #[tokio::test]
    async fn disabled_generation_does_not_call_provider() -> Result<()> {
        let provider = Arc::new(MockProvider {
            response: json!({"candidate_fixups": []}).to_string(),
            requests: Mutex::new(Vec::new()),
        });
        let config = FixupGenerationConfig::disabled();

        let prepared = generate_candidate_fixups(
            provider.clone(),
            &config,
            FixupGenerationRequest {
                patchset: &json!({"id": 1}),
                review: None,
                worktree: None,
                temperature: None,
                context_tag: None,
            },
        )
        .await?;

        assert!(prepared.is_empty());
        assert!(provider.requests.lock().await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn generation_parses_and_prepares_fixups() -> Result<()> {
        let response = serde_json::to_string(&json!({
            "candidate_fixups": [spelling_fixup()]
        }))?;
        let provider = Arc::new(MockProvider {
            response,
            requests: Mutex::new(Vec::new()),
        });
        let mut config = FixupGenerationConfig::disabled();
        config.enabled = true;
        config.mode = crate::fixups::FixupMode::Trivial;
        config.max_fixups_per_patchset = 3;
        config.validation_policy = crate::fixups::FixupValidationPolicy::trivial(50);

        let prepared = generate_candidate_fixups(
            provider.clone(),
            &config,
            FixupGenerationRequest {
                patchset: &json!({"id": 1, "patches": []}),
                review: Some(&json!({"findings": []})),
                worktree: None,
                temperature: Some(0.2),
                context_tag: Some("[fixups:test]".to_string()),
            },
        )
        .await?;

        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].fixup.title, "Fix spelling in comment");
        assert_eq!(
            prepared[0].validation_status,
            crate::fixups::FixupValidationStatus::Pending
        );

        let requests = provider.requests.lock().await;
        assert_eq!(requests.len(), 1);
        assert!(matches!(
            &requests[0].response_format,
            Some(AiResponseFormat::Json { .. })
        ));
        assert_eq!(requests[0].context_tag.as_deref(), Some("[fixups:test]"));
        Ok(())
    }
}
