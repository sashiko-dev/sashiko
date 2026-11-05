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
//! LLM selection over its candidates and then a tool-assisted disclosure
//! check of each selected one.

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
    let normalized = normalize_stage_name(name);
    normalized == SELECTION_STAGE_NAME || CHECK_STAGE_NAMES.contains(&normalized.as_str())
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

The pre-existing bugs listed under "Candidates" are verified open bugs tracked in the database for the files this patch touches. None of them was introduced by this patch. The patch author will only be told about a candidate if it is clearly worth their attention in the context of THIS patch. Mentioning an unrelated bug is noise and wastes the author's time, so the default answer for every candidate is to dismiss it.

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
///
/// Daemon reviews (`ps_id` > 0) select strictly from verified open database
/// bugs (`candidate_bugs`) so unverified or duplicate concerns from the
/// current review are never disclosed prematurely. Standalone local CLI
/// reviews (`ps_id == "0"`, `sashiko review --report-related-preexisting`)
/// have no daemon bug pipeline and fall back to `state.concerns` when
/// `candidate_bugs` is empty.
pub fn selection_candidates(state: &LinuxPatchReviewState) -> Vec<Value> {
    let Some(min_severity) = state.related_preexisting.min_severity() else {
        return Vec::new();
    };
    let is_daemon_review = state.ps_id.parse::<i64>().is_ok_and(|id| id > 0);
    let source = if is_daemon_review || !state.candidate_bugs.is_empty() {
        &state.candidate_bugs
    } else {
        &state.concerns
    };
    prefilter_candidates(
        source,
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

/// Static names for the per-candidate disclosure checks. Their number caps
/// how many selected candidates are checked, and the budget never exceeds it.
pub static CHECK_STAGE_NAMES: [&str; 2] =
    ["related-preexisting-check-1", "related-preexisting-check-2"];

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecuritySensitivity {
    NotSecuritySensitive,
    SecuritySensitive,
    Unknown,
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostPatchStatus {
    StillPresent,
    RemovedByPatch,
    FixedByPatch,
    MovedOrChanged,
    Unknown,
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisclosureDecision {
    Report,
    Drop,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct DisclosureCheckOutput {
    pub security_sensitivity: SecuritySensitivity,
    pub security_reasoning: String,
    pub post_patch_status: PostPatchStatus,
    #[serde(default)]
    pub post_patch_evidence: String,
    pub decision: DisclosureDecision,
    #[serde(default)]
    pub drop_reason: String,
    #[serde(default)]
    pub report_text: String,
}

impl DisclosureCheckOutput {
    /// Whether the candidate may be reported: only a bug that is not
    /// security-sensitive and still present after the patch, with the
    /// evidence and the note filled in. Anything else, including `Unknown`
    /// on either question, is dropped.
    pub fn passes(&self) -> bool {
        self.decision == DisclosureDecision::Report
            && self.security_sensitivity == SecuritySensitivity::NotSecuritySensitive
            && self.post_patch_status == PostPatchStatus::StillPresent
            && !self.post_patch_evidence.trim().is_empty()
            && !self.report_text.trim().is_empty()
    }
}

const CHECK_INSTRUCTION: &str = r#"# Check a pre-existing bug before telling the patch author about it

The bug under "Candidate" already existed before this patch. An earlier stage judged it closely related to the patch ("relation", "relation_reason", "cited_code"), so the review would mention it to the author in a public mailing list reply. Before that happens, answer two questions with the repository tools. The Target Commit SHA is the patched tree; use HEAD^ (or the Baseline SHA) for the code before the patch.

1. Is it security-sensitive? Follow the Linux kernel security policy: a bug belongs on security@kernel.org instead of a public list only when it is plausibly exploitable by an attacker without special privileges. Answer "SecuritySensitive" when an unprivileged local user, a remote network peer, a malicious device or filesystem image, or a guest can reach and trigger it, and it gives them memory corruption (use-after-free, double free, out-of-bounds write), privilege escalation, or a leak of kernel memory or data across a privilege boundary. Answer "NotSecuritySensitive" when it is not plausibly exploitable that way, for example: only root or a process holding CAP_SYS_ADMIN or a similar administrative capability can trigger it; it is a local denial of service such as a resource leak, a hang, a WARN or an oops on a NULL pointer; it is a deadlock or data race whose timing an attacker does not control; or it is a functional bug such as a wrong return value or a missing error path that corrupts no memory. In "security_reasoning", name who can trigger the bug and what it gives them. Answer "Unknown" only when you cannot tell who can reach and trigger it.

2. Is it still present once this patch is applied? Read the affected code at the Target Commit SHA. Answer "StillPresent" only if the defective code is still there and still reachable, and quote it from the patched tree in "post_patch_evidence" with file and line. Answer "RemovedByPatch" if the patch deletes the code, "FixedByPatch" if the patch fixes it, "MovedOrChanged" if the patch reworks it so the report would no longer match, and "Unknown" if you cannot tell.

Report the bug ("decision": "Report") only when it is "NotSecuritySensitive" and "StillPresent". Otherwise drop it and say why in "drop_reason". Do not give the candidate the benefit of the doubt: an unverified answer is "Unknown", and "Unknown" means drop.

When reporting, write "report_text": a short, self-contained note to the patch author in the style of a Linux kernel mailing list reply. Integrate the literal placeholder [BUG_REF] naturally into the opening sentence where the pre-existing issue is introduced (the mailer replaces [BUG_REF] with a footnote number such as [5] pointing to the tracked bug report at the end of the review). Quote the relevant lines from the patched tree prefixed with "> ", explain the problem in plain sentences, say how it relates to their patch, and suggest a fix. State clearly that the issue was not introduced by this patch. Plain text only: no markdown, no backticks, no headings, at most about 25 lines."#;

const CHECK_SCHEMA: &str = r#"Return ONLY a JSON object with these keys:
- "security_sensitivity": "NotSecuritySensitive", "SecuritySensitive" or "Unknown"
- "security_reasoning": plain-text explanation, never using markdown backticks
- "post_patch_status": "StillPresent", "RemovedByPatch", "FixedByPatch", "MovedOrChanged" or "Unknown"
- "post_patch_evidence": file:line and the verbatim code from the patched tree (required for "StillPresent")
- "decision": "Report" or "Drop"
- "drop_reason": why the bug is dropped (required for "Drop", empty for "Report")
- "report_text": the note to the author, integrating [BUG_REF] naturally into the opening sentence (required for "Report", empty for "Drop")

Example Output:
```json
{
  "security_sensitivity": "NotSecuritySensitive",
  "security_reasoning": "The leak needs CAP_SYS_ADMIN to trigger through the debugfs knob, so an unprivileged user cannot exhaust memory with it.",
  "post_patch_status": "StillPresent",
  "post_patch_evidence": "drivers/foo/foo.c:212: if (!buf)\n\treturn -EINVAL;",
  "decision": "Report",
  "drop_reason": "",
  "report_text": "This isn't a bug introduced by this patch, but the same missing unlock [BUG_REF] exists a few lines below the path you fixed:\n\n> \tif (!buf)\n> \t\treturn -EINVAL;\n\nfoo_probe() returns here with dev->lock still held. Would it make sense to route this through the same out_unlock label in this series?"
}
```"#;

/// Checks a disclosure-check answer for internal consistency, returning an
/// error that names the exact rule broken.
pub fn validate_disclosure_check(out: &DisclosureCheckOutput) -> Result<(), String> {
    if out.security_reasoning.trim().is_empty() {
        return Err("\"security_reasoning\" must explain the security answer.".to_string());
    }
    match out.decision {
        DisclosureDecision::Report => {
            if out.security_sensitivity != SecuritySensitivity::NotSecuritySensitive {
                return Err(
                    "\"decision\": \"Report\" requires \"security_sensitivity\": \"NotSecuritySensitive\"; otherwise use \"Drop\"."
                        .to_string(),
                );
            }
            if out.post_patch_status != PostPatchStatus::StillPresent {
                return Err(
                    "\"decision\": \"Report\" requires \"post_patch_status\": \"StillPresent\"; otherwise use \"Drop\"."
                        .to_string(),
                );
            }
            if out.post_patch_evidence.trim().is_empty() {
                return Err(
                    "\"StillPresent\" requires \"post_patch_evidence\" quoting the code from the patched tree."
                        .to_string(),
                );
            }
            if out.report_text.trim().is_empty() {
                return Err(
                    "\"decision\": \"Report\" requires a non-empty \"report_text\".".to_string(),
                );
            }
            if out.report_text.contains('`') {
                return Err(
                    "\"report_text\" must be plain text; remove every backtick.".to_string()
                );
            }
            if !out
                .report_text
                .contains(crate::related_preexisting::BUG_REF_PLACEHOLDER)
            {
                return Err(format!(
                    "\"report_text\" must include the {} placeholder integrated naturally into the opening sentence.",
                    crate::related_preexisting::BUG_REF_PLACEHOLDER
                ));
            }
        }
        DisclosureDecision::Drop => {
            if out.drop_reason.trim().is_empty() {
                return Err(
                    "\"decision\": \"Drop\" requires a non-empty \"drop_reason\".".to_string(),
                );
            }
        }
    }
    Ok(())
}

/// Records a disclosure check: a candidate that passes joins the related
/// bugs the review reports, anything else is kept with the reason.
pub fn apply_disclosure_check(
    state: &mut LinuxPatchReviewState,
    stage_name: &str,
    candidate: &Value,
    out: DisclosureCheckOutput,
) {
    let mut item = candidate.clone();
    let passes = out.passes();
    if let Some(map) = item.as_object_mut() {
        map.insert(
            "security_sensitivity".to_string(),
            json!(out.security_sensitivity),
        );
        map.insert(
            "security_reasoning".to_string(),
            json!(out.security_reasoning),
        );
        map.insert(
            "post_patch_status".to_string(),
            json!(out.post_patch_status),
        );
        map.insert(
            "post_patch_evidence".to_string(),
            json!(out.post_patch_evidence),
        );
        if passes {
            map.insert("report_text".to_string(), json!(out.report_text.trim()));
        } else {
            map.insert("dismissed_by".to_string(), json!(stage_name));
            let reason = if out.drop_reason.trim().is_empty() {
                "did not pass the disclosure check".to_string()
            } else {
                out.drop_reason
            };
            map.insert("dismissed_reason".to_string(), json!(reason));
        }
    }
    if passes {
        state.related_preexisting_reported.push(item);
    } else {
        state.related_preexisting_dismissed.push(item);
    }
}

fn disclosure_check_stage(
    stage_name: &'static str,
    candidate: Value,
    max_turns: usize,
    temperature: f32,
    reasoning_effort: Option<crate::ai::ReasoningEffort>,
) -> Box<dyn ExecutableStage<LinuxPatchReviewState>> {
    let rendered = serde_json::to_string_pretty(&candidate).unwrap_or_else(|_| "{}".to_string());
    let user_template = PromptTemplate::<LinuxPatchReviewState>::new(format!(
        "{CHECK_INSTRUCTION}\n\nCandidate:\n{{{{candidate}}}}\n\n{CHECK_SCHEMA}"
    ))
    .with_var("candidate", move |_: &LinuxPatchReviewState| {
        rendered.clone()
    });

    Box::new(
        Stage::builder(stage_name)
            .system_prompt(crate::workflows::linux_patch_review::linux_system_prompt(
                true,
            ))
            .user_prompt(user_template)
            .output_format(OutputFormat::json().with_validator(
                |out: &DisclosureCheckOutput, _: &LinuxPatchReviewState| {
                    validate_disclosure_check(out)
                },
            ))
            .policy(StagePolicy {
                tools: ToolScope::All,
                max_turns,
                temperature,
                reasoning_effort,
                ..Default::default()
            })
            .reduce(
                move |state: &mut LinuxPatchReviewState, out: DisclosureCheckOutput| {
                    apply_disclosure_check(state, stage_name, &candidate, out);
                },
            )
            .build(),
    )
}

/// One disclosure check per selected candidate, or none.
pub fn resolve_disclosure_check_stages(
    state: &LinuxPatchReviewState,
    max_turns: usize,
    temperature: f32,
) -> Vec<Box<dyn ExecutableStage<LinuxPatchReviewState>>> {
    let reasoning_effort = state
        .effort
        .map(crate::ai::EffortMode::verification_stage_effort);
    state
        .related_preexisting_selected
        .iter()
        .zip(CHECK_STAGE_NAMES.iter())
        .map(|(candidate, &name)| {
            disclosure_check_stage(
                name,
                candidate.clone(),
                max_turns,
                temperature,
                reasoning_effort,
            )
        })
        .collect()
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
            candidate_bugs: vec![
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
        s.candidate_bugs.truncate(0);
        assert!(resolve_selection_stage(&s, 0.0).is_empty());

        // Daemon review (ps_id > 0) strictly uses candidate_bugs and ignores
        // raw concerns from the current review, whereas local CLI review
        // (ps_id == "0") falls back to concerns when candidate_bugs is empty.
        s.concerns = vec![concern(
            "lock held on the -EINVAL return",
            "mm/foo.c",
            "foo_probe",
        )];
        s.ps_id = "42".to_string();
        assert!(selection_candidates(&s).is_empty());
        s.ps_id = "0".to_string();
        assert_eq!(selection_candidates(&s).len(), 1);
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
    fn check(decision: DisclosureDecision) -> DisclosureCheckOutput {
        DisclosureCheckOutput {
            security_sensitivity: SecuritySensitivity::NotSecuritySensitive,
            security_reasoning: "needs root".to_string(),
            post_patch_status: PostPatchStatus::StillPresent,
            post_patch_evidence: "mm/foo.c:20: return -EINVAL;".to_string(),
            decision,
            drop_reason: String::new(),
            report_text:
                "> return -EINVAL;\n\nThis isn't introduced by this patch, but the leak [BUG_REF] ..."
                    .to_string(),
        }
    }

    #[test]
    fn test_budget_never_exceeds_the_check_stages() {
        let huge: String = (0..1000).map(|i| format!("+line {i}\n")).collect();
        assert!(patch_budget(&huge) <= CHECK_STAGE_NAMES.len());
    }

    #[test]
    fn test_validate_disclosure_check_rules() {
        assert_eq!(
            validate_disclosure_check(&check(DisclosureDecision::Report)),
            Ok(())
        );

        let mut sensitive = check(DisclosureDecision::Report);
        sensitive.security_sensitivity = SecuritySensitivity::SecuritySensitive;
        assert!(
            validate_disclosure_check(&sensitive)
                .unwrap_err()
                .contains("NotSecuritySensitive")
        );

        let mut removed = check(DisclosureDecision::Report);
        removed.post_patch_status = PostPatchStatus::RemovedByPatch;
        assert!(
            validate_disclosure_check(&removed)
                .unwrap_err()
                .contains("StillPresent")
        );

        let mut no_evidence = check(DisclosureDecision::Report);
        no_evidence.post_patch_evidence = String::new();
        assert!(
            validate_disclosure_check(&no_evidence)
                .unwrap_err()
                .contains("post_patch_evidence")
        );

        let mut ticks = check(DisclosureDecision::Report);
        ticks.report_text = "use `foo()` [BUG_REF]".to_string();
        assert!(
            validate_disclosure_check(&ticks)
                .unwrap_err()
                .contains("backtick")
        );

        let mut no_bug_ref = check(DisclosureDecision::Report);
        no_bug_ref.report_text = "This isn't introduced by this patch.".to_string();
        assert!(
            validate_disclosure_check(&no_bug_ref)
                .unwrap_err()
                .contains("[BUG_REF]")
        );

        let drop_without_reason = check(DisclosureDecision::Drop);
        assert!(
            validate_disclosure_check(&drop_without_reason)
                .unwrap_err()
                .contains("drop_reason")
        );
    }

    #[test]
    fn test_apply_disclosure_check_reports_only_passing_candidates() {
        let mut s = state();
        let candidate = selection_candidates(&s).remove(0);

        apply_disclosure_check(
            &mut s,
            CHECK_STAGE_NAMES[0],
            &candidate,
            check(DisclosureDecision::Report),
        );
        assert_eq!(s.related_preexisting_reported.len(), 1);
        assert!(
            s.related_preexisting_reported[0]["report_text"]
                .as_str()
                .unwrap()
                .starts_with("> return")
        );

        let mut unknown = check(DisclosureDecision::Drop);
        unknown.security_sensitivity = SecuritySensitivity::Unknown;
        unknown.drop_reason = "cannot tell who can reach it".to_string();
        apply_disclosure_check(&mut s, CHECK_STAGE_NAMES[1], &candidate, unknown);
        assert_eq!(s.related_preexisting_reported.len(), 1);
        assert_eq!(s.related_preexisting_dismissed.len(), 1);
        assert_eq!(
            s.related_preexisting_dismissed[0]["dismissed_by"],
            CHECK_STAGE_NAMES[1]
        );
    }

    #[test]
    fn test_one_check_per_selected_candidate() {
        let mut s = state();
        assert!(resolve_disclosure_check_stages(&s, 5, 0.0).is_empty());
        let candidates = selection_candidates(&s);
        s.related_preexisting_selected = candidates;
        assert_eq!(resolve_disclosure_check_stages(&s, 5, 0.0).len(), 2);
        assert!(is_related_preexisting_stage_name(CHECK_STAGE_NAMES[1]));
    }
}
