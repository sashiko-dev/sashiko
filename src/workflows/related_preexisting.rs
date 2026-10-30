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

//! Stages that pick the related pre-existing bugs a review reports.
//!
//! See designs/DESIGN_RELATED_PREEXISTING_BUGS.md. The deterministic
//! pre-filter lives in `crate::related_preexisting`; this module runs the
//! LLM selection over its candidates.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::DisclosureRelation;
use crate::related_preexisting::{patch_budget, prefilter_candidates};
use crate::workflow::output::OutputFormat;
use crate::workflow::policy::{StagePolicy, ToolScope};
use crate::workflow::prompt::PromptTemplate;
use crate::workflow::stage::{ExecutableStage, Stage};
use crate::workflows::guard::normalize_stage_name;
use crate::workflows::linux_patch_review::LinuxPatchReviewState;

pub const SELECTION_STAGE_NAME: &str = "related-preexisting-selection";

/// Progress label shared by every related pre-existing stage.
pub const RELATED_PREEXISTING_SHORT_LABEL: &str = "Related Pre-existing";

/// Whether `name` is one of this module's stages.
pub fn is_related_preexisting_stage_name(name: &str) -> bool {
    normalize_stage_name(name) == SELECTION_STAGE_NAME
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct SelectionOutput {
    #[serde(default)]
    pub selected: Vec<SelectedCandidate>,
    #[serde(default)]
    pub dismissed: Vec<DismissedCandidate>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct SelectedCandidate {
    pub id: String,
    pub relation: DisclosureRelation,
    pub reason: String,
    pub cited_code: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct DismissedCandidate {
    pub id: String,
    pub reason: String,
}

const SELECTION_INSTRUCTION: &str = r#"# Pick related pre-existing bugs worth telling the author about

While reviewing this patch, other reviewers verified the pre-existing bugs listed under "Candidates". None of them was introduced by this patch. The patch author will only be told about a candidate if it is clearly worth their attention in the context of THIS patch. Mentioning an unrelated bug is noise and wastes the author's time, so the default answer for every candidate is to dismiss it.

Select a candidate only when one of these holds, and you can cite the code that shows it:
- "SameCode": the bug sits in code the patch modifies, or in the same function right next to the modified lines, so the author is already working there.
- "SamePattern": the patch fixes a defect, and the candidate is the same kind of defect (same mistake, same API misuse, same missing check) in nearby or closely related code that the fix missed.
- "Other": a comparably strong, concrete connection to what the patch does. Explain it precisely; vague topical similarity does not count.

Dismiss a candidate when:
- it is only loosely related (same file or subsystem but different code and a different kind of problem);
- the patch already removes, rewrites or fixes the code in question;
- you would have to assume facts the diff and the candidate do not show.

Do not give a candidate the benefit of the doubt. If you are unsure, dismiss it.

You may select at most {{budget}} candidate(s). If more qualify, keep the ones most closely tied to the patch."#;

const SELECTION_SCHEMA: &str = r#"Return ONLY a JSON object with "selected" and "dismissed" arrays. Every candidate id MUST appear in exactly one of them.
- Each "selected" object MUST contain: "id" (the candidate id, e.g. "RP1"), "relation" ("SameCode", "SamePattern" or "Other"), "reason" (one or two plain-text sentences explaining the connection to this patch, never using markdown backticks), and "cited_code" (the verbatim code line(s) from the diff or the candidate that show the connection).
- Each "dismissed" object MUST contain: "id" and "reason" (one plain-text sentence).

Example Output:
```json
{
  "selected": [
    {
      "id": "RP1",
      "relation": "SamePattern",
      "reason": "The patch adds the missing unlock on the -ENOMEM path of foo_probe(); the -EINVAL path a few lines below returns with the lock still held.",
      "cited_code": "if (!buf)\n\treturn -EINVAL;"
    }
  ],
  "dismissed": [
    {
      "id": "RP2",
      "reason": "A refcount leak in an unrelated ioctl handler of the same driver."
    }
  ]
}
```"#;

fn selection_system_prompt() -> PromptTemplate<LinuxPatchReviewState> {
    PromptTemplate::<LinuxPatchReviewState>::new(
        r#"You are a senior Linux kernel maintainer deciding which already-verified pre-existing bugs are worth mentioning to the author of a patch.
No repository exploration tools are available in this stage; do not attempt to call tools. Decide strictly from the provided diff and candidates.

=== Target Commit ===
{{target_commit_diff}}"#,
    )
    .with_var("target_commit_diff", |s: &LinuxPatchReviewState| {
        s.target_commit_diff.clone()
    })
}

/// The pre-filtered candidates for the current state, or none when the
/// review does not report related bugs.
pub fn selection_candidates(state: &LinuxPatchReviewState) -> Vec<Value> {
    let Some(min_severity) = state.related_preexisting.min_severity() else {
        return Vec::new();
    };
    prefilter_candidates(
        &state.concerns,
        &state.target_commit_diff_only,
        min_severity,
        &state.known_disclosures,
    )
}

/// Checks a selection against the candidates it was given, returning an
/// error that names the exact rule broken.
pub fn validate_selection(
    out: &SelectionOutput,
    candidates: &[Value],
    budget: usize,
) -> Result<(), String> {
    let known: BTreeSet<&str> = candidates
        .iter()
        .filter_map(|c| c.get("id").and_then(Value::as_str))
        .collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let ids = out
        .selected
        .iter()
        .map(|s| s.id.as_str())
        .chain(out.dismissed.iter().map(|d| d.id.as_str()));
    for id in ids {
        if !known.contains(id) {
            return Err(format!(
                "Unknown candidate id {id:?}. Use only these ids: {}.",
                known.iter().copied().collect::<Vec<_>>().join(", ")
            ));
        }
        if !seen.insert(id) {
            return Err(format!(
                "Candidate {id} appears more than once. Put every candidate in exactly one of \"selected\" or \"dismissed\"."
            ));
        }
    }
    let missing: Vec<&str> = known.difference(&seen).copied().collect();
    if !missing.is_empty() {
        return Err(format!(
            "Candidates {} are missing. Put every candidate in exactly one of \"selected\" or \"dismissed\".",
            missing.join(", ")
        ));
    }
    if out.selected.len() > budget {
        return Err(format!(
            "{} candidates selected but at most {budget} may be. Move the less closely related ones to \"dismissed\".",
            out.selected.len()
        ));
    }
    for s in &out.selected {
        if s.reason.trim().is_empty() || s.cited_code.trim().is_empty() {
            return Err(format!(
                "Selected candidate {} needs a non-empty \"reason\" and \"cited_code\".",
                s.id
            ));
        }
    }
    for d in &out.dismissed {
        if d.reason.trim().is_empty() {
            return Err(format!(
                "Dismissed candidate {} needs a non-empty \"reason\".",
                d.id
            ));
        }
    }
    Ok(())
}

/// Records the selection: chosen candidates, annotated with how they relate
/// to the patch, move on to the disclosure check; dismissed ones are kept
/// with their reason for the logs.
pub fn apply_selection(
    state: &mut LinuxPatchReviewState,
    candidates: &[Value],
    out: SelectionOutput,
) {
    let by_id = |id: &str| {
        candidates
            .iter()
            .find(|c| c.get("id").and_then(Value::as_str) == Some(id))
            .cloned()
    };
    for s in out.selected {
        if let Some(mut candidate) = by_id(&s.id) {
            if let Some(map) = candidate.as_object_mut() {
                map.insert("relation".to_string(), json!(s.relation));
                map.insert("relation_reason".to_string(), json!(s.reason));
                map.insert("cited_code".to_string(), json!(s.cited_code));
            }
            state.related_preexisting_selected.push(candidate);
        }
    }
    for d in out.dismissed {
        if let Some(mut candidate) = by_id(&d.id) {
            if let Some(map) = candidate.as_object_mut() {
                map.insert("dismissed_by".to_string(), json!(SELECTION_STAGE_NAME));
                map.insert("dismissed_reason".to_string(), json!(d.reason));
            }
            state.related_preexisting_dismissed.push(candidate);
        }
    }
}

fn selection_stage(
    candidates: Vec<Value>,
    budget: usize,
    temperature: f32,
    reasoning_effort: Option<crate::ai::ReasoningEffort>,
) -> Box<dyn ExecutableStage<LinuxPatchReviewState>> {
    let rendered = serde_json::to_string_pretty(&candidates).unwrap_or_else(|_| "[]".to_string());
    let candidates_for_validate = candidates.clone();
    let candidates_for_reduce = candidates;
    let user_template = PromptTemplate::<LinuxPatchReviewState>::new(format!(
        "{}\n\nCandidates:\n{{{{candidates}}}}\n\n{SELECTION_SCHEMA}",
        SELECTION_INSTRUCTION.replace("{{budget}}", &budget.to_string())
    ))
    .with_var("candidates", move |_: &LinuxPatchReviewState| {
        rendered.clone()
    });

    Box::new(
        Stage::builder(SELECTION_STAGE_NAME)
            .system_prompt(selection_system_prompt())
            .user_prompt(user_template)
            .output_format(OutputFormat::json().with_validator(
                move |out: &SelectionOutput, _: &LinuxPatchReviewState| {
                    validate_selection(out, &candidates_for_validate, budget)
                },
            ))
            .policy(StagePolicy {
                tools: ToolScope::None,
                max_turns: 1,
                temperature,
                reasoning_effort,
                ..Default::default()
            })
            .reduce(
                move |state: &mut LinuxPatchReviewState, out: SelectionOutput| {
                    apply_selection(state, &candidates_for_reduce, out);
                },
            )
            .build(),
    )
}

/// The selection stage for the current state, or none when the review does
/// not report related bugs or no candidate survives the pre-filter.
pub fn resolve_selection_stage(
    state: &LinuxPatchReviewState,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<LinuxPatchReviewState>>> {
    let candidates = selection_candidates(state);
    if candidates.is_empty() {
        return Vec::new();
    }
    let budget = patch_budget(&state.target_commit_diff_only);
    let reasoning_effort = state
        .effort
        .map(crate::ai::EffortMode::lightweight_stage_effort);
    vec![selection_stage(
        candidates,
        budget,
        temperature,
        reasoning_effort,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SeverityFloor;
    use crate::related_preexisting::RelatedPreexistingMode;

    const DIFF: &str = "\
diff --git a/mm/foo.c b/mm/foo.c
--- a/mm/foo.c
+++ b/mm/foo.c
@@ -10,3 +10,4 @@ int foo_probe(void)
+\tmutex_unlock(&dev->lock);
";

    fn concern(problem: &str, file: &str, symbol: &str) -> Value {
        json!({
            "description": problem,
            "severity": "High",
            "preexisting": true,
            "locations": [{"file": file, "function_or_symbol": symbol}],
        })
    }

    fn state() -> LinuxPatchReviewState {
        LinuxPatchReviewState {
            related_preexisting: RelatedPreexistingMode::Enabled {
                min_severity: SeverityFloor::High,
            },
            target_commit_diff_only: DIFF.to_string(),
            concerns: vec![
                concern("lock held on the -EINVAL return", "mm/foo.c", "foo_probe"),
                concern("leak in foo_remove", "mm/foo.c", "foo_remove"),
                concern("unrelated file", "mm/bar.c", "bar"),
            ],
            ..Default::default()
        }
    }

    fn selected(id: &str) -> SelectedCandidate {
        SelectedCandidate {
            id: id.to_string(),
            relation: DisclosureRelation::SamePattern,
            reason: "same missing unlock".to_string(),
            cited_code: "return -EINVAL;".to_string(),
        }
    }

    fn dismissed(id: &str) -> DismissedCandidate {
        DismissedCandidate {
            id: id.to_string(),
            reason: "unrelated".to_string(),
        }
    }

    #[test]
    fn test_no_stage_when_disabled_or_without_candidates() {
        let mut s = state();
        assert_eq!(resolve_selection_stage(&s, 0.0).len(), 1);
        assert_eq!(selection_candidates(&s).len(), 2);

        s.related_preexisting = RelatedPreexistingMode::Disabled;
        assert!(resolve_selection_stage(&s, 0.0).is_empty());

        let mut s = state();
        s.concerns.truncate(0);
        assert!(resolve_selection_stage(&s, 0.0).is_empty());
    }

    #[test]
    fn test_validate_selection_rules() {
        let candidates = selection_candidates(&state());
        let ok = SelectionOutput {
            selected: vec![selected("RP1")],
            dismissed: vec![dismissed("RP2")],
        };
        assert_eq!(validate_selection(&ok, &candidates, 1), Ok(()));

        let over = SelectionOutput {
            selected: vec![selected("RP1"), selected("RP2")],
            dismissed: vec![],
        };
        let err = validate_selection(&over, &candidates, 1).unwrap_err();
        assert!(err.contains("at most 1"), "{err}");

        let missing = SelectionOutput {
            selected: vec![selected("RP1")],
            dismissed: vec![],
        };
        let err = validate_selection(&missing, &candidates, 1).unwrap_err();
        assert!(err.contains("RP2 are missing"), "{err}");

        let unknown = SelectionOutput {
            selected: vec![selected("RP9")],
            dismissed: vec![dismissed("RP1"), dismissed("RP2")],
        };
        let err = validate_selection(&unknown, &candidates, 1).unwrap_err();
        assert!(err.contains("Unknown candidate id"), "{err}");

        let twice = SelectionOutput {
            selected: vec![selected("RP1")],
            dismissed: vec![dismissed("RP1"), dismissed("RP2")],
        };
        let err = validate_selection(&twice, &candidates, 1).unwrap_err();
        assert!(err.contains("more than once"), "{err}");

        let mut no_code = selected("RP1");
        no_code.cited_code = " ".to_string();
        let empty = SelectionOutput {
            selected: vec![no_code],
            dismissed: vec![dismissed("RP2")],
        };
        let err = validate_selection(&empty, &candidates, 1).unwrap_err();
        assert!(err.contains("cited_code"), "{err}");
    }

    #[test]
    fn test_apply_selection_annotates_and_splits() {
        let mut s = state();
        let candidates = selection_candidates(&s);
        apply_selection(
            &mut s,
            &candidates,
            SelectionOutput {
                selected: vec![selected("RP1")],
                dismissed: vec![dismissed("RP2")],
            },
        );
        assert_eq!(s.related_preexisting_selected.len(), 1);
        let chosen = &s.related_preexisting_selected[0];
        assert_eq!(chosen["id"], "RP1");
        assert_eq!(chosen["relation"], "SamePattern");
        assert_eq!(chosen["cited_code"], "return -EINVAL;");
        assert_eq!(s.related_preexisting_dismissed.len(), 1);
        assert_eq!(
            s.related_preexisting_dismissed[0]["dismissed_reason"],
            "unrelated"
        );
    }

    #[test]
    fn test_selection_stage_is_planned_before_the_report() {
        let planned: Vec<String> = ["goal", "verification", "post-verification", "report"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let refined = crate::workflows::refine_planned_stages_with_post_verification(
            &planned,
            &[SELECTION_STAGE_NAME],
        );
        assert_eq!(
            refined,
            vec![
                "goal",
                "verification",
                "post-verification",
                SELECTION_STAGE_NAME,
                "report"
            ]
        );
        assert!(crate::workflows::linux_patch_review::is_known_stage(
            SELECTION_STAGE_NAME
        ));
    }

    #[test]
    fn test_selection_output_parses_relation_names() {
        let out: SelectionOutput = serde_json::from_str(
            r#"{"selected":[{"id":"RP1","relation":"SameCode","reason":"r","cited_code":"c"}],"dismissed":[]}"#,
        )
        .unwrap();
        assert_eq!(out.selected[0].relation, DisclosureRelation::SameCode);
        assert!(
            serde_json::from_str::<SelectionOutput>(
                r#"{"selected":[{"id":"RP1","relation":"Nearby","reason":"r","cited_code":"c"}]}"#
            )
            .is_err()
        );
    }
}
