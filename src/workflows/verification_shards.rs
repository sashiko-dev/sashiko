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

//! Sharded tool-enabled `verification` stages shared by the Linux, Sashiko,
//! and GCC review workflows. Each project supplies only its prompt wording,
//! system prompt, and stage table through a [`VerificationShardProfile`].

use serde_json::Value;

use crate::workflow::output::OutputFormat;
use crate::workflow::policy::{StagePolicy, ToolScope};
use crate::workflow::prompt::PromptTemplate;
use crate::workflow::stage::{ExecutableStage, Stage};
use crate::workflows::linux_patch_review::{
    AnalysisStage, LinuxPatchReviewState, SERIES_CONTEXT_PLACEHOLDER, VERIFICATION_STAGE_NAMES,
    VerificationOutput, apply_verification_shard_output, batch_clusters_for_verification,
    effective_clusters, extra_prompt_paths_for_clusters, format_verification_shard_feedback,
    render_clusters_with_untrusted_prior_context, validate_verification_shard_output,
};

/// Project-specific inputs of a verification shard stage.
#[derive(Clone, Copy)]
pub struct VerificationShardProfile {
    /// Single-shard stage name (`"verification"`).
    pub stage_name: &'static str,
    /// Classification rules shared with the legacy single verification stage.
    pub instruction: &'static str,
    /// Description of the `"problem"` key of a finding (naming conventions).
    pub problem_rule: &'static str,
    /// Optional example output appended to the schema (may be empty).
    pub example_output: &'static str,
    pub wants_series_context: bool,
    pub system_prompt: fn(bool) -> PromptTemplate<LinuxPatchReviewState>,
    pub stage_lookup: fn(&str) -> Option<&'static AnalysisStage>,
}

const CLUSTER_DIRECTIVE: &str = r#"CRITICAL REVIEW DIRECTIVE: Evaluate each pre-grouped candidate cluster ("G*") below (using Git and file tools when helpful). Never dismiss a cluster that contains any raised concern ("concerns" non-empty) in this stage: every cluster containing a raised concern must be placed either in 'findings' (ONLY if uncontested with zero 'dismissed_concerns' in the cluster and backed by concrete code proof) or in 'inconclusive_concerns' (if contested, speculative, involving prior discussion, or requiring deeper verification). Inspect standalone dismissal clusters ('concerns' empty, 'dismissed_concerns' non-empty) critically: if a dismissal relies on an unverified assumption, single-caller proof, or asymmetry rationalization, promote the cluster into 'inconclusive_concerns' with '"signal_reason": "speculative_dismissal"'; otherwise record its cluster ID in 'dismissed_ids'."#;

const LINEAGE_RULES: &str = r#"Return ONLY a JSON object with 'findings', 'inconclusive_concerns', and 'dismissed_ids' arrays.
- LINEAGE REQUIREMENT: Every cluster in Candidate Clusters has an "id" (e.g. "G1"). Every cluster ID in this shard MUST be accounted for in 'findings[].source_ids', 'inconclusive_concerns[].source_ids', or 'dismissed_ids'.
  - When a cluster contains more than one raised concern ("concerns" has 2 or more items), ALSO list each of its raw concern IDs ("C*"/"PC*") in the 'source_ids' of the finding or inconclusive concern that covers it, using a separate item for each distinct bug, so no merged bug is lost.
  - Category 1a 'findings[].source_ids' may ONLY reference uncontested concern clusters (clusters with non-empty "concerns" and empty "dismissed_concerns").
  - Category 2 'inconclusive_concerns[].source_ids' may reference any contested, speculative, or promoted cluster.
  - Category 1b 'dismissed_ids' is an array of cluster ID strings (e.g. ["G2", "G4"]) for well-justified standalone dismissal clusters ONLY (clusters with empty "concerns" and concrete local proof in "dismissed_concerns"). Do NOT re-serialize dismissed_concerns objects—Rust preserves the full original dismissal records automatically from 'dismissed_ids'."#;

const INCONCLUSIVE_RULE: &str = r#"- Each object in 'inconclusive_concerns' MUST use the keys: "source_ids" (non-empty array of cluster G* IDs), "type", "description" (plain text, never use markdown backticks), "estimated_severity" ("Low", "Medium", "High", "Critical", or "Unknown"), "signal_reason" ("mixed_signals", "speculative_concern", "speculative_dismissal", "series_interaction", or "other"), "concern_arguments" (plain text, never use markdown backticks), "dismissal_arguments" (plain text, never use markdown backticks), "verification_question" (plain text, never use markdown backticks), "preexisting" (boolean), and "locations" (array of location objects with file, function_or_symbol, line, code_snippet, side, and why_this_location_matters; side MUST be "PARENT" or "REVISION")."#;

fn shard_user_template(profile: &VerificationShardProfile) -> String {
    let series_context = if profile.wants_series_context {
        SERIES_CONTEXT_PLACEHOLDER
    } else {
        ""
    };
    let example = if profile.example_output.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", profile.example_output)
    };
    format!(
        "{instruction}\n\n\
         <false_positive_guide>\n@include(\"false-positive-guide.md\")\n</false_positive_guide>\n\n\
         <severity_guidelines>\n@include(\"severity.md\")\n</severity_guidelines>@includes\n\n\
         {CLUSTER_DIRECTIVE}{series_context}\n\n\
         Candidate Clusters to Verify:\n{{{{candidate_clusters}}}}\n\n\
         {LINEAGE_RULES}\n\
         - Each object in 'findings' MUST use the keys: \"source_ids\" (non-empty array of cluster G* IDs), {problem_rule}, \"severity\" (\"Low\", \"Medium\", \"High\", \"Critical\", or \"Unknown\"), \"severity_explanation\" (plain-text detailed reasoning and proof, NEVER using backquotes), \"preexisting\" (boolean), and \"locations\" (array of location objects with file, function_or_symbol, line, code_snippet, side, and why_this_location_matters; side MUST be \"PARENT\" for deleted/pre-patch code or \"REVISION\" for added/modified/context code).\n\
         {INCONCLUSIVE_RULE}{example}",
        instruction = profile.instruction,
        problem_rule = profile.problem_rule,
    )
}

/// Builds one verification shard stage over `shard_clusters`.
pub fn verification_shard_stage(
    profile: &VerificationShardProfile,
    stage_name: &'static str,
    shard_clusters: Vec<Value>,
    max_turns: usize,
    temperature: f32,
    effort: Option<crate::ai::EffortMode>,
) -> Stage<LinuxPatchReviewState, VerificationOutput> {
    let clusters_block = render_clusters_with_untrusted_prior_context(&shard_clusters);
    let clusters_for_prompts = shard_clusters.clone();
    let clusters_for_validate = shard_clusters.clone();
    let clusters_for_reduce = shard_clusters;
    let stage_lookup = profile.stage_lookup;

    let mut user_template =
        PromptTemplate::<LinuxPatchReviewState>::new(shard_user_template(profile))
            .include_file("false-positive-guide.md")
            .include_file("severity.md")
            .include_files_from_state(move |s: &LinuxPatchReviewState| {
                extra_prompt_paths_for_clusters(
                    &s.selected_guides,
                    &clusters_for_prompts,
                    stage_lookup,
                )
            })
            .with_var("candidate_clusters", move |_: &LinuxPatchReviewState| {
                clusters_block.clone()
            });
    if profile.wants_series_context {
        user_template =
            user_template.with_var("follow_up_series_section", |s: &LinuxPatchReviewState| {
                s.follow_up_series_context
                    .as_ref()
                    .map(|ctx| format!("\n\n{ctx}"))
                    .unwrap_or_default()
            });
    }

    Stage::builder(stage_name)
        .system_prompt((profile.system_prompt)(true))
        .user_prompt(user_template)
        .output_format(
            OutputFormat::json()
                .with_validator(move |out: &VerificationOutput, _: &LinuxPatchReviewState| {
                    validate_verification_shard_output(out, &clusters_for_validate)
                })
                .with_feedback_formatter(format_verification_shard_feedback),
        )
        .policy(StagePolicy {
            tools: ToolScope::All,
            max_turns,
            temperature,
            reasoning_effort: effort.map(crate::ai::EffortMode::verification_stage_effort),
            ..Default::default()
        })
        .skip_if(|s| s.all_concerns.is_empty() && s.all_dismissed_concerns.is_empty())
        .reduce(move |state, out: VerificationOutput| {
            apply_verification_shard_output(
                state,
                stage_name,
                &clusters_for_reduce,
                out,
                stage_lookup,
            );
        })
        .build()
}

/// Resolves one verification stage, or `verification-1..M` shards when
/// the candidate clusters exceed a single shard.
pub fn resolve_verification_stages(
    profile: &VerificationShardProfile,
    state: &LinuxPatchReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<LinuxPatchReviewState>>> {
    let effort = state.effort;
    let mut shards = batch_clusters_for_verification(&effective_clusters(state));
    if shards.len() <= 1 {
        let single = shards.pop().unwrap_or_default();
        return vec![Box::new(verification_shard_stage(
            profile,
            profile.stage_name,
            single,
            max_turns,
            temperature,
            effort,
        ))];
    }
    shards
        .into_iter()
        .zip(VERIFICATION_STAGE_NAMES.iter())
        .map(|(shard, &stage_name)| {
            Box::new(verification_shard_stage(
                profile,
                stage_name,
                shard,
                max_turns,
                temperature,
                effort,
            )) as Box<dyn ExecutableStage<LinuxPatchReviewState>>
        })
        .collect()
}
