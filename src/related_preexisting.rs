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

//! Reporting pre-existing bugs that relate to the patch under review.
//!
//! See designs/DESIGN_RELATED_PREEXISTING_BUGS.md.

use crate::db::{Severity, SeverityFloor};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

/// Problem descriptions sharing at least this fraction of their words
/// (Jaccard similarity of the word sets) describe the same bug when they also
/// point at the same file.
pub const SAME_PROBLEM_SIMILARITY: f64 = 0.5;

/// Patches changing at most this many lines may report one related bug;
/// larger ones may report two.
pub const SMALL_PATCH_CHANGED_LINES: usize = 30;

/// Words too common in bug descriptions to tell two bugs apart.
const STOP_WORDS: [&str; 24] = [
    "the", "and", "for", "with", "that", "this", "from", "when", "into", "can", "may", "not",
    "its", "are", "was", "but", "via", "after", "before", "without", "missing", "bug", "issue",
    "path",
];

/// Identifies a bug well enough to tell whether two reports describe the
/// same one. Built from a concern or finding, or from a recorded disclosure.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BugKey {
    /// Hash of the first location and the normalized problem; equal for the
    /// same concern object across a review, its rerun and the bug pipeline.
    pub fingerprint: String,
    /// Files the locations point at.
    pub files: Vec<String>,
    /// Normalized function or symbol names of the locations.
    pub symbols: Vec<String>,
    /// Distinctive lowercase words of the problem description.
    pub problem_words: Vec<String>,
}

fn normalize_symbol(raw: &str) -> String {
    raw.trim()
        .trim_end_matches("()")
        .trim()
        .to_ascii_lowercase()
}

fn problem_words(problem: &str) -> Vec<String> {
    problem
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .map(str::to_ascii_lowercase)
        .filter(|w| w.len() >= 3 && !STOP_WORDS.contains(&w.as_str()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The problem text of a concern (`description`) or finding (`problem`).
pub fn problem_of(item: &Value) -> &str {
    item.get("problem")
        .or_else(|| item.get("description"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

impl BugKey {
    /// Builds the key of a concern or finding from its problem and locations.
    pub fn from_item(item: &Value) -> Self {
        Self::new(problem_of(item), item.get("locations"))
    }

    pub fn new(problem: &str, locations: Option<&Value>) -> Self {
        let locations = locations
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let field = |loc: &Value, key: &str| {
            loc.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let files: BTreeSet<String> = locations.iter().filter_map(|l| field(l, "file")).collect();
        let symbols: BTreeSet<String> = locations
            .iter()
            .filter_map(|l| field(l, "function_or_symbol"))
            .map(|s| normalize_symbol(&s))
            .filter(|s| !s.is_empty())
            .collect();
        let words = problem_words(problem);

        let first = locations.first();
        let mut hasher = Sha256::new();
        hasher.update(
            first
                .and_then(|l| field(l, "file"))
                .unwrap_or_default()
                .as_bytes(),
        );
        hasher.update(b"\0");
        let first_symbol = first
            .and_then(|l| field(l, "function_or_symbol"))
            .map(|s| normalize_symbol(&s))
            .unwrap_or_default();
        hasher.update(first_symbol.as_bytes());
        hasher.update(b"\0");
        hasher.update(words.join(" ").as_bytes());
        let fingerprint = hasher
            .finalize()
            .iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect();

        Self {
            fingerprint,
            files: files.into_iter().collect(),
            symbols: symbols.into_iter().collect(),
            problem_words: words,
        }
    }

    fn problem_similarity(&self, other: &Self) -> f64 {
        let a: BTreeSet<&str> = self.problem_words.iter().map(String::as_str).collect();
        let b: BTreeSet<&str> = other.problem_words.iter().map(String::as_str).collect();
        let union = a.union(&b).count();
        if union == 0 {
            return 0.0;
        }
        a.intersection(&b).count() as f64 / union as f64
    }

    /// Whether two keys describe the same bug: the same fingerprint, or the
    /// same file together with the same function or a near-identical
    /// problem description. Deliberately eager to match, because reporting
    /// a bug twice is worse than skipping a second bug in the same function.
    pub fn is_same_bug(&self, other: &Self) -> bool {
        if self.fingerprint == other.fingerprint {
            return true;
        }
        if !self.files.iter().any(|f| other.files.contains(f)) {
            return false;
        }
        self.symbols.iter().any(|s| other.symbols.contains(s))
            || self.problem_similarity(other) >= SAME_PROBLEM_SIMILARITY
    }
}

/// Files a patch touches, split by whether they survive it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatchFiles {
    /// Post-image paths of files the patch modifies, adds or renames into.
    pub surviving: BTreeSet<String>,
    /// Paths of files the patch deletes.
    pub deleted: BTreeSet<String>,
}

impl PatchFiles {
    pub fn from_diff(diff: &str) -> Self {
        let mut files = Self::default();
        let mut current: Option<String> = None;
        let mut current_deleted = false;
        let finish = |path: Option<String>, deleted: bool, files: &mut Self| {
            if let Some(path) = path {
                if deleted {
                    files.deleted.insert(path);
                } else {
                    files.surviving.insert(path);
                }
            }
        };
        for line in diff.lines() {
            if let Some(rest) = line.strip_prefix("diff --git ") {
                finish(current.take(), current_deleted, &mut files);
                current_deleted = false;
                current = rest
                    .rsplit_once(" b/")
                    .map(|(_, b)| b.trim().trim_matches('"').to_string());
            } else if line.starts_with("deleted file mode") || line.trim_end() == "+++ /dev/null" {
                current_deleted = true;
            }
        }
        finish(current, current_deleted, &mut files);
        files
    }
}

/// How many related bugs the review of a patch may report.
pub fn patch_budget(diff: &str) -> usize {
    let changed = diff
        .lines()
        .filter(|l| {
            (l.starts_with('+') && !l.starts_with("+++"))
                || (l.starts_with('-') && !l.starts_with("---"))
        })
        .count();
    if changed <= SMALL_PATCH_CHANGED_LINES {
        1
    } else {
        2
    }
}

/// Deterministic first cut of verified pre-existing concerns: severe
/// enough, located in a file the patch touches and keeps, and not the same
/// bug as one already reported (`known`) or as an earlier candidate.
///
/// Returned candidates carry an `id` (`RP1`, `RP2`, ...) for the selection
/// stage to cite. Lines and sides are not trusted here because concern
/// locations are not grounded until the workflow ends; the selection and
/// disclosure-check stages judge proximity with the diff and tools.
pub fn prefilter_candidates(
    concerns: &[Value],
    diff: &str,
    min_severity: SeverityFloor,
    known: &[BugKey],
) -> Vec<Value> {
    let files = PatchFiles::from_diff(diff);
    let mut accepted: Vec<BugKey> = Vec::new();
    let mut out = Vec::new();
    for concern in concerns {
        let severity = Severity::from_str(
            concern
                .get("severity")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        if !min_severity.admits(severity) {
            continue;
        }
        let key = BugKey::from_item(concern);
        if !key.files.iter().any(|f| files.surviving.contains(f)) {
            continue;
        }
        if known
            .iter()
            .chain(accepted.iter())
            .any(|k| k.is_same_bug(&key))
        {
            continue;
        }
        let mut candidate = concern.clone();
        if let Some(map) = candidate.as_object_mut() {
            map.insert("id".to_string(), json!(format!("RP{}", out.len() + 1)));
        }
        out.push(candidate);
        accepted.push(key);
    }
    out
}

/// Whether a patch review looks for related pre-existing bugs to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RelatedPreexistingMode {
    #[default]
    Disabled,
    Enabled {
        /// Bugs below this severity are never reported.
        min_severity: SeverityFloor,
    },
}

impl RelatedPreexistingMode {
    /// The mode a worker runs in given its `--related-preexisting-min-severity`.
    pub fn from_min_severity(min_severity: Option<SeverityFloor>) -> Self {
        match min_severity {
            Some(min_severity) => Self::Enabled { min_severity },
            None => Self::Disabled,
        }
    }

    pub fn min_severity(self) -> Option<SeverityFloor> {
        match self {
            Self::Enabled { min_severity } => Some(min_severity),
            Self::Disabled => None,
        }
    }
}

#[cfg(feature = "server")]
pub use gate::{GateInputs, GateSkipReason, evaluate_gate};
#[cfg(feature = "server")]
pub use reporting::{
    disclosure_candidates, related_email_divider, related_email_header, related_email_section,
    same_disclosed_bug,
};

#[cfg(feature = "server")]
mod reporting {
    use super::{BugKey, problem_of};
    use crate::db::{
        DisclosureRelation, NewPreexistingDisclosure, PreexistingDisclosure, Severity,
    };
    use serde_json::Value;

    /// The related bugs a worker reported (`related_preexisting` in its
    /// review output), ready to reserve. Items without a note or with an
    /// unknown relation are skipped rather than mailed half-formed.
    pub fn disclosure_candidates(review: &Value) -> Vec<NewPreexistingDisclosure> {
        let Some(items) = review.get("related_preexisting").and_then(Value::as_array) else {
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|item| {
                let report_text = item.get("report_text")?.as_str()?.trim();
                if report_text.is_empty() {
                    return None;
                }
                let relation: DisclosureRelation =
                    serde_json::from_value(item.get("relation")?.clone()).ok()?;
                let key = BugKey::from_item(item);
                Some(NewPreexistingDisclosure {
                    fingerprint: key.fingerprint.clone(),
                    problem: problem_of(item).trim().to_string(),
                    severity: Severity::from_str(
                        item.get("severity")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    ),
                    relation,
                    locations_json: item
                        .get("locations")
                        .map(Value::to_string)
                        .unwrap_or_else(|| "[]".to_string()),
                    bug_key_json: serde_json::to_string(&key).ok()?,
                    report_text: report_text.to_string(),
                    files: key.files,
                })
            })
            .collect()
    }

    /// The duplicate rule for reservations. A key that cannot be read
    /// counts as the same bug, so a damaged row blocks instead of letting a
    /// bug be reported twice.
    pub fn same_disclosed_bug(
        candidate: &NewPreexistingDisclosure,
        existing: &PreexistingDisclosure,
    ) -> bool {
        let (Ok(a), Ok(b)) = (
            serde_json::from_str::<BugKey>(&candidate.bug_key_json),
            serde_json::from_str::<BugKey>(&existing.bug_key_json),
        ) else {
            return true;
        };
        a.is_same_bug(&b)
    }

    use crate::email_text::{bullet, count, wrap, wrap_prose};

    fn summary_lines(rows: &[PreexistingDisclosure]) -> String {
        rows.iter()
            .map(|d| bullet(d.severity.as_str(), &d.problem))
            .collect()
    }

    fn issues(n: usize) -> String {
        count(
            n,
            "related pre-existing issue",
            "related pre-existing issues",
        )
    }

    /// The line that sets the related notes apart from the inline review
    /// when the email also carries new findings.
    pub fn related_email_divider(n: usize) -> String {
        if n == 1 {
            "Related pre-existing issue, not introduced by this patch:".to_string()
        } else {
            "Related pre-existing issues, not introduced by this patch:".to_string()
        }
    }

    /// The opening lines of a review email that carries related bugs,
    /// wrapped at 72 columns. `new_issues` is the number of findings
    /// the patch introduces.
    pub fn related_email_header(new_issues: usize, rows: &[PreexistingDisclosure]) -> String {
        if rows.is_empty() {
            return String::new();
        }
        if new_issues == 0 {
            let sentence = format!(
                "Thank you for your contribution! Sashiko AI review found no new issues in this patch, but noticed {} it does not introduce:",
                issues(rows.len())
            );
            format!("{}{}--\n\n", wrap(&sentence, "", ""), summary_lines(rows))
        } else {
            let sentence = format!(
                "It also noticed {} this patch does not introduce:",
                issues(rows.len())
            );
            format!("{}{}", wrap(&sentence, "", ""), summary_lines(rows))
        }
    }

    /// The notes for the related bugs, appended after the inline review,
    /// with their prose wrapped at 72 columns.
    pub fn related_email_section(rows: &[PreexistingDisclosure]) -> String {
        rows.iter()
            .map(|d| wrap_prose(&d.report_text))
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[cfg(feature = "server")]
mod gate {
    use crate::db::SeverityFloor;
    use crate::email_policy::EmailPolicyConfig;
    use crate::project::ProjectId;

    /// Why a patchset does not get related pre-existing bugs reported.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum GateSkipReason {
        /// Only the Linux review workflow implements the extra stages.
        UnsupportedProject,
        /// A mailing list the patchset went to turned the feature off.
        DisabledByPolicy,
        /// Requests for comments usually discuss a design, not final code.
        Rfc,
        /// No earlier posting is known, but the version tag says there were.
        NotFirstVersion,
        /// An earlier non-RFC posting of the series is known.
        FollowsNonRfcPosting,
    }

    /// What the gate needs to know about a patchset.
    #[derive(Debug, Clone)]
    pub struct GateInputs<'a> {
        pub project: ProjectId,
        /// The stored patchset subject.
        pub subject: &'a str,
        /// The pull request title, for patchsets that came from a forge.
        pub mr_title: Option<&'a str>,
        pub effective_version: u32,
        /// Titles of the earlier postings of the series (see
        /// `Database::get_patchset_predecessor_titles`).
        pub predecessor_titles: &'a [String],
        /// To and Cc addresses of the patchset.
        pub recipients: &'a [String],
    }

    /// Decides whether a patchset's review reports related pre-existing
    /// bugs, returning the severity floor to apply when it does.
    ///
    /// Only the first non-RFC posting of a series is eligible. RFCs never
    /// report, so a posting that follows nothing but RFCs is eligible
    /// whatever its version tag; one with no known predecessor must also be
    /// tagged as a first version, since a later tag means earlier rounds
    /// exist that were not linked.
    pub fn evaluate_gate(
        inputs: &GateInputs<'_>,
        policy: &EmailPolicyConfig,
    ) -> Result<SeverityFloor, GateSkipReason> {
        if inputs.project != ProjectId::Linux {
            return Err(GateSkipReason::UnsupportedProject);
        }
        let resolved = policy.related_preexisting_for(inputs.recipients.iter().map(String::as_str));
        if !resolved.enabled {
            return Err(GateSkipReason::DisabledByPolicy);
        }
        let title = inputs.mr_title.unwrap_or(inputs.subject);
        if crate::patch::is_rfc_subject(title) || crate::patch::is_rfc_subject(inputs.subject) {
            return Err(GateSkipReason::Rfc);
        }
        if inputs
            .predecessor_titles
            .iter()
            .any(|t| !crate::patch::is_rfc_subject(t))
        {
            return Err(GateSkipReason::FollowsNonRfcPosting);
        }
        if inputs.predecessor_titles.is_empty() && inputs.effective_version != 1 {
            return Err(GateSkipReason::NotFirstVersion);
        }
        Ok(resolved.min_severity)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn inputs<'a>(subject: &'a str, recipients: &'a [String]) -> GateInputs<'a> {
            GateInputs {
                project: ProjectId::Linux,
                subject,
                mr_title: None,
                effective_version: 1,
                predecessor_titles: &[],
                recipients,
            }
        }

        fn policy() -> EmailPolicyConfig {
            EmailPolicyConfig::parse_content(
                r#"
                ["netdev@vger.kernel.org".related_preexisting]
                enabled = false

                ["linux-mm@kvack.org".related_preexisting]
                min_severity = "critical"
                "#,
            )
            .unwrap()
        }

        #[test]
        fn test_first_version_is_eligible_with_the_resolved_floor() {
            let none: Vec<String> = Vec::new();
            assert_eq!(
                evaluate_gate(&inputs("[PATCH] mm: fix a leak", &none), &policy()),
                Ok(SeverityFloor::High)
            );
            let mm = vec!["linux-mm@kvack.org".to_string()];
            assert_eq!(
                evaluate_gate(&inputs("[PATCH v1] mm: fix a leak", &mm), &policy()),
                Ok(SeverityFloor::Critical)
            );
        }

        #[test]
        fn test_each_skip_reason() {
            let none: Vec<String> = Vec::new();
            let net = vec!["Netdev <netdev@vger.kernel.org>".to_string()];
            let policy = policy();

            let mut sashiko = inputs("[PATCH] fix", &none);
            sashiko.project = ProjectId::Sashiko;
            assert_eq!(
                evaluate_gate(&sashiko, &policy),
                Err(GateSkipReason::UnsupportedProject)
            );
            assert_eq!(
                evaluate_gate(&inputs("[PATCH] net: fix", &net), &policy),
                Err(GateSkipReason::DisabledByPolicy)
            );
            assert_eq!(
                evaluate_gate(&inputs("[RFC PATCH] mm: idea", &none), &policy),
                Err(GateSkipReason::Rfc)
            );
            let mut v2 = inputs("[PATCH v2] mm: fix", &none);
            v2.effective_version = 2;
            assert_eq!(
                evaluate_gate(&v2, &policy),
                Err(GateSkipReason::NotFirstVersion)
            );
            let earlier = vec!["[PATCH] mm: fix".to_string()];
            let mut resend = inputs("[PATCH v1] mm: fix", &none);
            resend.predecessor_titles = &earlier;
            assert_eq!(
                evaluate_gate(&resend, &policy),
                Err(GateSkipReason::FollowsNonRfcPosting)
            );
        }

        #[test]
        fn test_first_posting_after_rfcs_is_eligible() {
            let none: Vec<String> = Vec::new();
            let rfcs = vec![
                "[RFC v2] mm: idea".to_string(),
                "[RFC] mm: idea".to_string(),
            ];
            let mut after_rfc = inputs("[PATCH] mm: idea", &none);
            after_rfc.predecessor_titles = &rfcs;
            assert_eq!(
                evaluate_gate(&after_rfc, &policy()),
                Ok(SeverityFloor::High)
            );

            // The tag does not matter after RFCs: RFC v3 -> PATCH v4.
            let mut renumbered = inputs("[PATCH v4] mm: idea", &none);
            renumbered.effective_version = 4;
            renumbered.predecessor_titles = &rfcs;
            assert_eq!(
                evaluate_gate(&renumbered, &policy()),
                Ok(SeverityFloor::High)
            );

            // A non-RFC posting anywhere earlier in the chain disqualifies it.
            let mixed = vec![
                "[RFC v2] mm: idea".to_string(),
                "[PATCH] mm: idea".to_string(),
            ];
            let mut later = inputs("[PATCH v2] mm: idea", &none);
            later.effective_version = 2;
            later.predecessor_titles = &mixed;
            assert_eq!(
                evaluate_gate(&later, &policy()),
                Err(GateSkipReason::FollowsNonRfcPosting)
            );
        }

        #[test]
        fn test_forge_title_marks_an_rfc() {
            let none: Vec<String> = Vec::new();
            let mut pr = inputs("#513: RFC: rework the queue", &none);
            pr.mr_title = Some("RFC: rework the queue");
            assert_eq!(evaluate_gate(&pr, &policy()), Err(GateSkipReason::Rfc));

            let mut pr = inputs("#514: fix the queue", &none);
            pr.mr_title = Some("fix the queue");
            assert_eq!(evaluate_gate(&pr, &policy()), Ok(SeverityFloor::High));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "\
diff --git a/mm/foo.c b/mm/foo.c
--- a/mm/foo.c
+++ b/mm/foo.c
@@ -10,3 +10,3 @@ int foo(void)
-\tspin_lock(&a);
+\tspin_lock_irq(&a);
diff --git a/mm/old.c b/mm/old.c
deleted file mode 100644
--- a/mm/old.c
+++ /dev/null
@@ -1,2 +0,0 @@
-int old;
-int older;
diff --git a/mm/a.c b/mm/b.c
similarity index 90%
rename from mm/a.c
rename to mm/b.c
";

    fn concern(problem: &str, severity: &str, file: &str, symbol: &str) -> Value {
        json!({
            "description": problem,
            "severity": severity,
            "preexisting": true,
            "locations": [{"file": file, "function_or_symbol": symbol, "line": 12}],
        })
    }

    #[test]
    fn test_patch_files_split_surviving_and_deleted() {
        let files = PatchFiles::from_diff(DIFF);
        assert_eq!(
            files.surviving.into_iter().collect::<Vec<_>>(),
            vec!["mm/b.c".to_string(), "mm/foo.c".to_string()]
        );
        assert_eq!(
            files.deleted.into_iter().collect::<Vec<_>>(),
            vec!["mm/old.c".to_string()]
        );
    }

    #[test]
    fn test_patch_budget_scales_with_changed_lines() {
        assert_eq!(patch_budget(DIFF), 1);
        let big: String = (0..31).map(|i| format!("+line {i}\n")).collect();
        assert_eq!(patch_budget(&format!("+++ b/x\n{big}")), 2);
    }

    #[test]
    fn test_bug_key_matching() {
        let a = BugKey::from_item(&concern(
            "missing unlock of dev->lock on the error path",
            "High",
            "mm/foo.c",
            "foo_probe()",
        ));
        // Same concern object: same fingerprint.
        assert_eq!(
            a.fingerprint,
            BugKey::from_item(&concern(
                "missing unlock of dev->lock on the error path",
                "Critical",
                "mm/foo.c",
                "foo_probe"
            ))
            .fingerprint
        );
        // Same file and function, different wording: same bug.
        let same_fn = BugKey::from_item(&concern("refcount leak", "High", "mm/foo.c", "FOO_PROBE"));
        assert!(a.is_same_bug(&same_fn));
        // Same file, other function, near-identical wording: same bug.
        let reworded = BugKey::from_item(&concern(
            "dev->lock unlock missing on error path",
            "High",
            "mm/foo.c",
            "foo_remove",
        ));
        assert!(a.is_same_bug(&reworded));
        // Same file, other function, different problem: different bugs.
        let other = BugKey::from_item(&concern(
            "integer overflow computing the buffer size",
            "High",
            "mm/foo.c",
            "foo_alloc",
        ));
        assert!(!a.is_same_bug(&other));
        // Same wording in another file: different bugs.
        let elsewhere = BugKey::from_item(&concern(
            "missing unlock of dev->lock on the error path",
            "High",
            "mm/bar.c",
            "bar_probe",
        ));
        assert!(!a.is_same_bug(&elsewhere));
    }

    #[test]
    fn test_prefilter_keeps_severe_bugs_in_surviving_touched_files() {
        let concerns = vec![
            concern("use after free of the queue", "High", "mm/foo.c", "foo"),
            concern("too minor", "Medium", "mm/foo.c", "foo_minor"),
            concern("bug in a deleted file", "Critical", "mm/old.c", "old"),
            concern(
                "bug in an untouched file",
                "Critical",
                "mm/other.c",
                "other",
            ),
            concern("bug in the renamed file", "Critical", "mm/b.c", "b"),
            concern("same bug, reported twice", "High", "mm/foo.c", "foo"),
            concern("no severity", "Unknown", "mm/foo.c", "foo_unknown"),
        ];
        let kept = prefilter_candidates(&concerns, DIFF, SeverityFloor::High, &[]);
        let problems: Vec<&str> = kept.iter().map(problem_of).collect();
        assert_eq!(
            problems,
            vec!["use after free of the queue", "bug in the renamed file"]
        );
        assert_eq!(kept[0]["id"], "RP1");
        assert_eq!(kept[1]["id"], "RP2");

        let with_medium = prefilter_candidates(&concerns, DIFF, SeverityFloor::Medium, &[]);
        assert_eq!(with_medium.len(), 3);

        // Already reported elsewhere: dropped.
        let known = vec![BugKey::from_item(&concern(
            "use after free of the queue",
            "High",
            "mm/foo.c",
            "foo",
        ))];
        let kept = prefilter_candidates(&concerns, DIFF, SeverityFloor::High, &known);
        assert_eq!(
            kept.iter().map(problem_of).collect::<Vec<_>>(),
            vec!["bug in the renamed file"]
        );
    }

    #[cfg(feature = "server")]
    #[test]
    fn test_disclosure_candidates_and_email_text() {
        use crate::db::{DisclosureRelation, PreexistingDisclosure};

        let review = json!({
            "related_preexisting": [
                {
                    "description": "lock held on the -EINVAL return",
                    "severity": "High",
                    "relation": "SamePattern",
                    "report_text": "  > return -EINVAL;\n\nnote one  ",
                    "locations": [{"file": "mm/foo.c", "function_or_symbol": "foo_probe"}],
                },
                {"description": "no note", "relation": "SameCode", "report_text": " "},
                {"description": "bad relation", "relation": "Nearby", "report_text": "x"},
            ]
        });
        let candidates = disclosure_candidates(&review);
        assert_eq!(candidates.len(), 1);
        let c = &candidates[0];
        assert_eq!(c.relation, DisclosureRelation::SamePattern);
        assert_eq!(c.severity, Severity::High);
        assert_eq!(c.files, vec!["mm/foo.c".to_string()]);
        assert_eq!(c.report_text, "> return -EINVAL;\n\nnote one");

        let row = |key: &str| PreexistingDisclosure {
            id: 1,
            patch_message_id: "p@x".to_string(),
            fingerprint: c.fingerprint.clone(),
            problem: c.problem.clone(),
            severity: c.severity,
            relation: c.relation,
            locations_json: c.locations_json.clone(),
            bug_key_json: key.to_string(),
            report_text: c.report_text.clone(),
            reserved_at: 0,
            disclosed_at: None,
            bug_id: None,
            files: c.files.clone(),
        };
        assert!(same_disclosed_bug(c, &row(&c.bug_key_json)));
        // An unreadable key blocks rather than risking a second report.
        assert!(same_disclosed_bug(c, &row("not json")));

        let rows = vec![row(&c.bug_key_json)];
        let alone = related_email_header(0, &rows);
        assert_eq!(
            alone,
            "Thank you for your contribution! Sashiko AI review found no new issues\n\
             in this patch, but noticed 1 related pre-existing issue it does not\n\
             introduce:\n\
             - [High] lock held on the -EINVAL return\n\
             --\n\n"
        );
        let with_findings = related_email_header(2, &[rows[0].clone(), rows[0].clone()]);
        assert!(
            with_findings.starts_with("It also noticed 2 related pre-existing issues this"),
            "{with_findings}"
        );
        assert!(related_email_header(0, &[]).is_empty());
        assert_eq!(
            related_email_section(&rows),
            "> return -EINVAL;\n\nnote one"
        );

        // Long problems wrap with a hanging indent; every line fits.
        let mut long = rows[0].clone();
        long.problem = "net: foo: priv->lock held on the -EINVAL return in foo_probe() after the config parse fails".to_string();
        let header = related_email_header(0, &[long]);
        assert!(
            header
                .lines()
                .all(|l| l.len() <= crate::email_text::WRAP_WIDTH),
            "{header}"
        );
        assert!(header.contains("\n  "), "{header}");
    }

    #[cfg(feature = "server")]
    #[test]
    fn test_related_email_section_wraps_prose_but_not_quotes() {
        use crate::db::{DisclosureRelation, PreexistingDisclosure};
        let quote =
            "> \tif (!cfg && some_rather_long_condition_name(priv, cfg, FOO_FLAG_A, FOO_FLAG_B))";
        let note = format!(
            "This isn't a bug introduced by this patch, but the same missing unlock exists a few lines below the path you fixed:\n\n{quote}\n> \t\treturn -EINVAL;\n\nfoo_probe() returns here with priv->lock\nstill held."
        );
        let row = PreexistingDisclosure {
            id: 1,
            patch_message_id: "p@x".to_string(),
            fingerprint: "fp".to_string(),
            problem: "p".to_string(),
            severity: Severity::High,
            relation: DisclosureRelation::SamePattern,
            locations_json: "[]".to_string(),
            bug_key_json: "{}".to_string(),
            report_text: note,
            reserved_at: 0,
            disclosed_at: None,
            bug_id: None,
            files: Vec::new(),
        };
        let section = related_email_section(&[row]);
        assert_eq!(
            section,
            format!(
                "This isn't a bug introduced by this patch, but the same missing unlock\n\
                 exists a few lines below the path you fixed:\n\
                 \n\
                 {quote}\n\
                 > \t\treturn -EINVAL;\n\
                 \n\
                 foo_probe() returns here with priv->lock still held."
            )
        );
    }
}
