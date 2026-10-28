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

use crate::project::ProjectId;

pub mod code_notes;
pub mod gcc_patch_review;
pub mod guard;
#[cfg(feature = "server")]
pub mod linux_bug;
pub mod linux_patch_review;
pub mod notes_agenda;
pub mod notes_corpus;
pub mod prior_attribution;
pub mod prior_feedback;
pub mod qemu_patch_review;
pub mod review_map;
pub mod sashiko_patch_review;
pub mod verification_shards;

/// Returns the short UI label for a review stage belonging to `project`.
pub fn stage_short_label(project: ProjectId, stage: &str) -> Option<&'static str> {
    match project {
        ProjectId::Linux => linux_patch_review::stage_short_label(stage),
        ProjectId::Sashiko => sashiko_patch_review::stage_short_label(stage),
        ProjectId::Gcc => gcc_patch_review::stage_short_label(stage),
        ProjectId::Qemu => qemu_patch_review::stage_short_label(stage),
    }
}

/// Returns the default total stage count (all analysis + consolidation stages)
/// when dynamic planning has not yet narrowed the fan-out.
pub fn default_stage_count(project: ProjectId) -> usize {
    match project {
        ProjectId::Linux => {
            linux_patch_review::ANALYSIS_STAGES.len()
                + linux_patch_review::CONSOLIDATION_STAGES.len()
        }
        ProjectId::Sashiko => {
            sashiko_patch_review::ANALYSIS_STAGES.len()
                + sashiko_patch_review::CONSOLIDATION_STAGES.len()
        }
        ProjectId::Gcc => {
            gcc_patch_review::ANALYSIS_STAGES.len() + gcc_patch_review::CONSOLIDATION_STAGES.len()
        }
        ProjectId::Qemu => {
            qemu_patch_review::ANALYSIS_STAGES.len() + qemu_patch_review::CONSOLIDATION_STAGES.len()
        }
    }
}

/// Whether a stage counts towards the review progress display.
///
/// The analysis and consolidation stages are the ones `planned_stages_from()`
/// totals in advance. The pre-screen and the planner cannot be totalled that
/// way, because whether either runs depends on `--stages`, so the display counts
/// them as it sees them start instead. Either way a stage that finishes has to
/// say so, or the bar stops short of the work it did.
pub fn is_counted_stage(project: ProjectId, name: &str) -> bool {
    stage_short_label(project, name).is_some() || matches!(name, "pre-screen" | "planning")
}

/// Resolves the ordered list of stages a review will run (analysis fan-out
/// followed by consolidation stages).
pub fn planned_stages_from(project: ProjectId, stage_names: &[&'static str]) -> Vec<String> {
    match project {
        ProjectId::Linux => {
            let mut planned: Vec<String> = stage_names
                .iter()
                .filter(|n| {
                    linux_patch_review::analysis_stage_by_name(n).is_some()
                        || prior_feedback::is_prior_feedback_stage_name(n)
                })
                .map(|n| n.to_string())
                .collect();
            if !planned.is_empty() {
                planned.extend(
                    linux_patch_review::CONSOLIDATION_STAGES
                        .iter()
                        .map(|s| s.name.to_string()),
                );
            }
            planned
        }
        ProjectId::Sashiko => {
            let mut planned: Vec<String> = stage_names
                .iter()
                .filter(|n| {
                    sashiko_patch_review::analysis_stage_by_name(n).is_some()
                        || prior_feedback::is_prior_feedback_stage_name(n)
                })
                .map(|n| n.to_string())
                .collect();
            if !planned.is_empty() {
                planned.extend(
                    sashiko_patch_review::CONSOLIDATION_STAGES
                        .iter()
                        .map(|s| s.name.to_string()),
                );
            }
            planned
        }
        ProjectId::Gcc => {
            let mut planned: Vec<String> = stage_names
                .iter()
                .filter(|n| {
                    gcc_patch_review::analysis_stage_by_name(n).is_some()
                        || prior_feedback::is_prior_feedback_stage_name(n)
                })
                .map(|n| n.to_string())
                .collect();
            if !planned.is_empty() {
                planned.extend(
                    gcc_patch_review::CONSOLIDATION_STAGES
                        .iter()
                        .map(|s| s.name.to_string()),
                );
            }
            planned
        }
        ProjectId::Qemu => {
            let mut planned: Vec<String> = stage_names
                .iter()
                .filter(|n| qemu_patch_review::analysis_stage_by_name(n).is_some())
                .map(|n| n.to_string())
                .collect();
            if !planned.is_empty() {
                planned.extend(
                    qemu_patch_review::CONSOLIDATION_STAGES
                        .iter()
                        .map(|s| s.name.to_string()),
                );
            }
            planned
        }
    }
}

/// Replaces a dynamic consolidation placeholder (`"verification"` or
/// `"post-verification"`) in `planned_stages` with the concrete stages
/// resolved at runtime (for example `"verification-1..M"` when verification
/// shards >10 candidate clusters, or 0..=10 `"post-verification-*"` stages).
pub fn refine_planned_stages_with_post_verification(
    planned_stages: &[String],
    resolved_stages: &[&'static str],
) -> Vec<String> {
    if resolved_stages
        .first()
        .is_some_and(|name| prior_attribution::is_prior_attribution_stage_name(name))
    {
        // `prior-attribution` has no placeholder: it runs only when there
        // are findings and prior context, so its batches are inserted ahead
        // of the report stages once resolved.
        let insert_at = planned_stages
            .iter()
            .position(|s| s == "report" || s == "summary")
            .unwrap_or(planned_stages.len());
        let mut updated = planned_stages.to_vec();
        updated.splice(
            insert_at..insert_at,
            resolved_stages.iter().map(|s| (*s).to_string()),
        );
        return updated;
    }
    let target_placeholder = if resolved_stages
        .first()
        .is_some_and(|name| name.starts_with(linux_patch_review::VERIFICATION.name))
    {
        linux_patch_review::VERIFICATION.name
    } else {
        linux_patch_review::POST_VERIFICATION.name
    };
    let mut updated =
        Vec::with_capacity(planned_stages.len().saturating_sub(1) + resolved_stages.len());
    for stage in planned_stages {
        if stage == target_placeholder {
            updated.extend(resolved_stages.iter().map(|s| (*s).to_string()));
        } else {
            updated.push(stage.clone());
        }
    }
    updated
}
