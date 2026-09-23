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

pub mod guard;
pub mod linux_bug;
pub mod linux_patch_review;
mod reachability;
pub mod sashiko_patch_review;

/// Returns the short UI label for a review stage belonging to `project`.
pub fn stage_short_label(project: ProjectId, stage: &str) -> Option<&'static str> {
    match project {
        ProjectId::Linux => linux_patch_review::stage_short_label(stage),
        ProjectId::Sashiko => sashiko_patch_review::stage_short_label(stage),
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
                .filter(|n| linux_patch_review::analysis_stage_by_name(n).is_some())
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
                .filter(|n| sashiko_patch_review::analysis_stage_by_name(n).is_some())
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
    }
}
