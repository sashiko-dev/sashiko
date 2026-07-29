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

//! The cherry-pick (merge-conflict resolution) review pipeline.
//!
//! Expresses the original hack (commit 66fb0a5) as a principled [`Pipeline`]:
//! cherry-specific analysis stages 1-3 + shared analysis 4-7, then the shared
//! synthesis tail (dedup 8, resolution 9, verification 10), an origin
//! classification stage (11), a cherry-specific finding filter, and a final
//! conflict report (12). The three-commit context is hydrated from git at
//! review time; the database only stores the minimal `ReviewKind::CherryPick`.

use anyhow::Result;
use std::path::Path;

use serde_json::Value;

use crate::pipelines::{
    Capture, Cx, Pipeline, PipelineContext, PipelineState, StagePrompt, StageSpec,
};
use crate::review_kind::ReviewKind;

use super::synthesis::{self, ANALYSIS_FORMAT_GUIDANCE};
use super::{
    CONFLICT_REPORT, DROPPED_CHANGES, MERGE_CORRECTNESS, ORIGIN_CLASSIFICATION, SEMANTIC_INTENT,
    VERIFICATION, filter_cherry_pick_findings,
};

/// A cherry-pick / merge-conflict resolution review pipeline.
pub struct CherryPickReviewPipeline {
    /// SHA of the original patch being ported (commit 1).
    pub original_sha: String,
    /// SHA of the target base branch the patch was applied onto (commit 2).
    pub base_sha: Option<String>,
    /// SHA of the resolution commit under review (commit 3).
    pub resolution_sha: String,
}

impl CherryPickReviewPipeline {
    /// Build the pipeline from the persisted review context and resolution SHA.
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

fn override_stage(number: u8, text: &str, capture: Capture) -> StageSpec {
    StageSpec::new(
        number,
        StagePrompt::Override {
            content: text.to_string(),
            clean: text.to_string(),
        },
        capture,
    )
}

#[async_trait::async_trait]
impl Pipeline for CherryPickReviewPipeline {
    fn name(&self) -> &'static str {
        "cherry-pick"
    }

    async fn run(&self, cx: &mut Cx<'_>) -> Result<()> {
        // Assemble the shared system context (git hydration + dynamic context).
        let ctx = self.build_shared_context(cx).await?;

        // The Planning phase decides which of stages 4-7 to include. It reuses
        // the exact system context the stages will see.
        let selected = plan_stages(
            cx.env.provider.as_ref(),
            cx.env.stages.clone(),
            &ctx.system_with_log,
        )
        .await;
        cx.set_context(ctx);

        run_analysis(cx, &selected).await?;
        if cx.state.concerns.is_empty() {
            return Ok(());
        }
        run_synthesis(cx).await
    }
}

impl CherryPickReviewPipeline {
    /// Hydrate the three-commit context from git and assemble the shared system
    /// context (with/without review log, plus the clean/templated variants used
    /// for logging).
    async fn build_shared_context(&self, cx: &Cx<'_>) -> Result<PipelineContext> {
        let env = cx.env;

        let (static_context, clean_static_context) = env.prompts.build_context(None).await?;
        let worktree = env.tools.get_worktree_path();

        // Hydrate the three-commit context from git at review time.
        let original_subject = git_subject(worktree, &self.original_sha).await;
        let base_subject = match self.base_sha.as_deref() {
            Some(sha) => git_subject(worktree, sha).await,
            None => None,
        };
        let resolution_subject = match cx.patchset.get("subject").and_then(|v| v.as_str()) {
            Some(s) => Some(s.to_string()),
            None => git_subject(worktree, &self.resolution_sha).await,
        };
        let original_diff = git_show(worktree, &self.original_sha).await;
        let resolution_diff = extract_resolution_diff(&cx.patchset);

        let git_metadata = build_git_metadata(
            &self.original_sha,
            self.base_sha.as_deref(),
            &self.resolution_sha,
            original_subject.as_deref(),
            base_subject.as_deref(),
            resolution_subject.as_deref(),
            original_diff.as_deref(),
        );

        // Mirror Worker::run's dynamic-context assembly (with/without review log).
        let mut dynamic = dynamic_base(&git_metadata, "\n\nTarget Commit:\n", &resolution_diff);
        let mut dynamic_no_log =
            dynamic_base(&git_metadata, "\n\nTarget Commit Diff:\n", &resolution_diff);
        let mut clean_dynamic = dynamic.clone();
        let mut clean_dynamic_no_log = dynamic_no_log.clone();

        if let Ok(prefetched) =
            crate::worker::prefetch::prefetch_context(worktree, &resolution_diff).await
            && !prefetched.is_empty()
        {
            append_prefetch(&mut dynamic, &prefetched);
            append_prefetch(&mut dynamic_no_log, &prefetched);
            append_prefetch(&mut clean_dynamic, "{{prefetched_context}}");
            append_prefetch(&mut clean_dynamic_no_log, "{{prefetched_context}}");
        }

        Ok(PipelineContext {
            system_with_log: format!("{}{}", static_context, dynamic),
            system_without_log: format!("{}{}", static_context, dynamic_no_log),
            clean_with_log: format!("{}{}", clean_static_context, clean_dynamic),
            clean_without_log: format!("{}{}", clean_static_context, clean_dynamic_no_log),
        })
    }
}

/// Analysis stage with an override prompt and the shared analysis format guidance.
fn analysis_override(number: u8, text: &str) -> StageSpec {
    override_stage(number, text, Capture::MergeConcerns)
        .with_format_guidance(ANALYSIS_FORMAT_GUIDANCE)
}

/// Assemble the dynamic-context body: git metadata, a target header, and the
/// resolution diff.
fn dynamic_base(git_metadata: &str, target_header: &str, resolution_diff: &str) -> String {
    let mut s = String::new();
    s.push_str(git_metadata);
    s.push_str(target_header);
    s.push_str(resolution_diff);
    s
}

/// Append the pre-fetched-context block. `body` is the real prefetched content,
/// or the `{{prefetched_context}}` placeholder for the clean/templated variant.
fn append_prefetch(s: &mut String, body: &str) {
    const HDR: &str = include_str!("prompts/prefetched_context_header.md");
    s.push_str(HDR);
    s.push_str(body);
    s.push_str("\n</pre_fetched_context>\n");
}

/// Build the Planning-phase AI request from the shared system context.
fn planning_request(shared_ctx: &str) -> crate::ai::AiRequest {
    let planning_prompt = include_str!("prompts/planning.md");
    crate::ai::AiRequest {
        system: None,
        messages: vec![crate::ai::AiMessage {
            role: crate::ai::AiRole::User,
            content: Some(format!("{}\n\n{}", shared_ctx, planning_prompt)),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        tools: None,
        temperature: Some(0.0),
        response_format: Some(crate::ai::AiResponseFormat::Json {
            schema: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "relevant_stages": {
                        "type": "array",
                        "items": { "type": "integer" }
                    }
                },
                "required": ["relevant_stages"]
            })),
        }),
        context_tag: None,
    }
}

/// Run the Planning phase (json_request with retry) to decide which of stages
/// 4-7 to include. Skips when stages were set explicitly. Planning stays an
/// UNLOGGED side request (not folded into history). Returns `None` to mean
/// "run all stages".
async fn plan_stages(
    provider: &dyn crate::ai::AiProvider,
    explicit: Option<Vec<u8>>,
    shared_ctx: &str,
) -> Option<Vec<u8>> {
    if let Some(stages) = explicit {
        return Some(stages);
    }

    let val = crate::pipelines::json_request(provider, planning_request(shared_ctx), |v| {
        v.get("relevant_stages")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "missing 'relevant_stages' array".to_string())
            .map(|_| ())
    })
    .await;

    if let Some(val) = val
        && let Some(arr) = val["relevant_stages"].as_array()
    {
        let mut stages = vec![1u8, 2, 3];
        for v in arr {
            if let Some(n) = v.as_u64()
                && (4..=7).contains(&n)
            {
                stages.push(n as u8);
            }
        }
        tracing::info!("Planning phase selected stages: {:?}", stages);
        Some(stages)
    } else {
        // If planning fails, run all stages.
        None
    }
}

/// Run the analysis stages: cherry-specific 1-3 plus any planning-selected 4-7,
/// all in parallel.
async fn run_analysis(cx: &mut Cx<'_>, selected: &Option<Vec<u8>>) -> Result<()> {
    let include_stage = |n: u8| -> bool {
        match selected {
            Some(stages) => stages.contains(&n),
            None => true,
        }
    };
    let mut analysis = vec![
        analysis_override(1, SEMANTIC_INTENT),
        analysis_override(2, DROPPED_CHANGES),
        analysis_override(3, MERGE_CORRECTNESS).with_guides(3),
    ];
    for n in 4..=7 {
        if include_stage(n) {
            analysis.push(
                StageSpec::new(n, StagePrompt::Builtin, Capture::MergeConcerns)
                    .with_format_guidance(ANALYSIS_FORMAT_GUIDANCE),
            );
        }
    }
    cx.parallel(analysis).await
}

/// Run the synthesis tail: dedup (8), resolution (9), verification (10), origin
/// classification (10), the cherry-pick finding filter, and the final report
/// (11). Each step short-circuits when nothing remains to act on.
async fn run_synthesis(cx: &mut Cx<'_>) -> Result<()> {
    // Stage 8: dedup.
    cx.stage(
        StageSpec::new(8, StagePrompt::Builtin, Capture::ReplaceConcerns)
            .with_template(synthesis::stage8_builder()),
    )
    .await?;
    if cx.state.concerns.is_empty() {
        return Ok(());
    }

    // Stage 9: conflict resolution.
    cx.stage(
        StageSpec::new(9, StagePrompt::Builtin, Capture::ReplaceConcerns)
            .with_template(synthesis::stage9_builder()),
    )
    .await?;
    if cx.state.concerns.is_empty() {
        return Ok(());
    }

    // Stage 10: verification.
    cx.stage(
        override_stage(10, VERIFICATION, Capture::Findings)
            .with_template(synthesis::stage10_builder())
            .with_guides(10),
    )
    .await?;
    if findings_empty(&cx.state) {
        return Ok(());
    }

    // Stage 10: origin classification.
    cx.stage(
        override_stage(10, ORIGIN_CLASSIFICATION, Capture::Findings)
            .with_template(synthesis::origin_builder()),
    )
    .await?;

    // Cherry-pick finding filter.
    if let Some(f) = cx.state.findings.take() {
        // Preserve the classified findings for DB output; the filter only
        // controls Stage 11 and the early exit.
        cx.state.classified_findings = Some(f.clone());
        let filtered = filter_cherry_pick_findings(&f);
        // Set a distinct review_inline when the filter drops all findings.
        if filtered.as_array().is_none_or(|a| a.is_empty()) {
            cx.state.review_inline =
                Some("No issues found after conflict review filtering.".to_string());
        }
        cx.state.findings = Some(filtered);
    }
    if findings_empty(&cx.state) {
        return Ok(());
    }

    // Stage 11: conflict report.
    cx.stage(
        override_stage(11, CONFLICT_REPORT, Capture::Inline)
            .with_template(synthesis::stage11_builder())
            .with_guides(11),
    )
    .await
}

/// True when there are no findings to act on.
fn findings_empty(state: &PipelineState) -> bool {
    state
        .findings
        .as_ref()
        .and_then(|v| v.as_array())
        .is_none_or(|a| a.is_empty())
}

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
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
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
    git_output(worktree, &["show", "-s", "--format=%s", sha])
        .await
        .map(|s| s.trim().to_string())
}

async fn git_show(worktree: &Path, sha: &str) -> Option<String> {
    git_output(worktree, &["show", sha]).await
}

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

    let mut m = include_str!("prompts/merge_conflict_review_framing.md")
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
    use crate::pipelines::PipelineState;
    use crate::pipelines::cherry_pick_review::synthesis;

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
        let p = CherryPickReviewPipeline::from_review_kind(&kind, "res".to_string());
        assert_eq!(p.original_sha, "orig");
        assert_eq!(p.base_sha.as_deref(), Some("base"));
        assert_eq!(p.resolution_sha, "res");
    }

    #[test]
    fn stage8_builder_injects_state() {
        let state = PipelineState {
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
}
