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

/// The stages behind a finding, as a suffix for a one-line report of it:
/// `" (Resource Mgmt)"`, or `" (Locking & Sync, Security Audit)"` where more
/// than one stage raised it.
///
/// Empty when the finding names no stage, which is what consolidation leaves
/// when a model dropped the provenance on the way through, so a caller can
/// append this without asking first. A name with no short label is left out
/// rather than printed raw: the label table is what decides how a stage is
/// spelled for a reader.
///
/// `project` says which table to spell them from, and the other project's table
/// answers whatever it cannot. A caller does not always know which project
/// produced a finding: the remote client reads findings from a server that may
/// review a different one, and has only its own configuration to go on, so
/// insisting on its guess would drop every label for a review it did not run.
pub fn finding_stage_suffix(project: ProjectId, finding: &serde_json::Value) -> String {
    let label = |name: &str| {
        stage_short_label(project, name).or_else(|| match project {
            ProjectId::Linux => stage_short_label(ProjectId::Sashiko, name),
            ProjectId::Sashiko => stage_short_label(ProjectId::Linux, name),
        })
    };

    let labels: Vec<&str> = finding
        .get("stages")
        .and_then(|v| v.as_array())
        .map(|names| {
            names
                .iter()
                .filter_map(|name| name.as_str())
                .filter_map(label)
                .collect()
        })
        .unwrap_or_default();

    if labels.is_empty() {
        String::new()
    } else {
        format!(" ({})", labels.join(", "))
    }
}

/// The findings as a report stage should see them: each `stages` entry replaced
/// by the label a reader is shown everywhere else.
///
/// The stage names in a finding are identifiers — `locking`, `resources` — and
/// how they are spelled for a reader is decided by the label table, which a
/// model has no way to consult. So the report is handed the labels themselves
/// and asked to copy them, rather than the names and a mapping it would have to
/// invent. What the review persists keeps the names, for anything reading the
/// findings as data.
pub fn findings_for_report(
    project: ProjectId,
    findings: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    findings
        .iter()
        .map(|finding| {
            let labels: Vec<&str> = finding
                .get("stages")
                .and_then(|v| v.as_array())
                .map(|names| {
                    names
                        .iter()
                        .filter_map(|name| name.as_str())
                        .filter_map(|name| stage_short_label(project, name))
                        .collect()
                })
                .unwrap_or_default();

            let mut shown = finding.clone();
            if let Some(map) = shown.as_object_mut() {
                if labels.is_empty() {
                    map.remove("stages");
                } else {
                    map.insert("stages".to_string(), serde_json::json!(labels));
                }
            }
            shown
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_a_findings_stages_are_named_by_their_labels() {
        let suffix = |finding| finding_stage_suffix(ProjectId::Linux, &finding);

        assert_eq!(suffix(json!({"stages": ["resources"]})), " (Resource Mgmt)");
        assert_eq!(
            suffix(json!({"stages": ["locking", "security"]})),
            " (Locking & Sync, Security Audit)"
        );

        // Nothing to say, said as nothing: a caller appends this either way, and
        // a review whose model dropped the provenance reads as it did before.
        assert_eq!(suffix(json!({"problem": "mm: leak"})), "");
        assert_eq!(suffix(json!({"stages": []})), "");

        // A name the label table does not know is left out rather than printed
        // raw, so the reader only ever sees stages this review has.
        assert_eq!(suffix(json!({"stages": ["not-a-stage"]})), "");
        assert_eq!(
            suffix(json!({"stages": ["not-a-stage", "locking"]})),
            " (Locking & Sync)"
        );

        // A stage only the other project has is still spelled out. The remote
        // client reads findings from a server that may review a project its own
        // configuration does not name, and dropping the label there would leave a
        // reader with no provenance at all rather than the wrong word for it.
        assert_eq!(
            finding_stage_suffix(ProjectId::Linux, &json!({"stages": ["llm-pipeline"]})),
            " (LLM Pipeline)"
        );
        assert_eq!(
            finding_stage_suffix(ProjectId::Sashiko, &json!({"stages": ["hardware"]})),
            " (Hardware Review)"
        );
    }

    #[test]
    fn test_a_report_is_handed_labels_rather_than_stage_names() {
        // The report quotes what it is given straight into prose, so it is given
        // the spelling a reader sees elsewhere. A finding with nothing to say
        // loses the key rather than carrying an empty array into the prompt.
        let shown = findings_for_report(
            ProjectId::Linux,
            &[
                json!({"problem": "mm: leak", "stages": ["locking", "security"]}),
                json!({"problem": "mm: other", "stages": ["not-a-stage"]}),
                json!({"problem": "mm: third"}),
            ],
        );

        assert_eq!(
            shown[0]["stages"],
            json!(["Locking & Sync", "Security Audit"])
        );
        assert!(shown[1].get("stages").is_none());
        assert!(shown[2].get("stages").is_none());

        // Everything else about a finding is left alone.
        assert_eq!(shown[0]["problem"], "mm: leak");
    }
}
