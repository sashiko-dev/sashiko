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

//! Pure patch-to-patch matching used by `Database::reconcile_patch_lineage`.

use std::collections::{HashMap, HashSet};

/// Minimum length of a subject summary that may link two patches whose
/// subsystem prefixes differ.
const MIN_SUMMARY_LEN: usize = 16;
/// Longest token accepted as one subsystem prefix component.
const MAX_PREFIX_COMPONENT_LEN: usize = 40;

/// One patch of a series as seen by lineage reconciliation.
#[derive(Debug, Clone)]
pub(super) struct LineagePatch {
    pub id: i64,
    pub part_index: i64,
    pub git_patch_id: Option<String>,
    pub clean_subject: String,
    pub existing_prev_id: Option<i64>,
    pub existing_prev_status: Option<String>,
}

/// `curr patch id -> (previous patch id, previous_patch_status)`.
pub(super) type LineageAssignments = HashMap<i64, (Option<i64>, Option<&'static str>)>;

/// Runs the content-based matching tiers in order and returns the links
/// found together with the previous patch IDs they consumed.
///
/// 1. Identical `git_patch_id` links as `unchanged`.
/// 2. Identical cleaned subject links as `modified`.
/// 3. Identical summary under a compatible subsystem prefix links as
///    `modified` (see `summaries_link`).
pub(super) fn match_patch_lineage(
    prev_patches: &[LineagePatch],
    curr_patches: &[LineagePatch],
) -> (LineageAssignments, HashSet<i64>) {
    let mut tiers = LineageTiers {
        prev_patches,
        curr_patches,
        matched_prev_ids: HashSet::new(),
        assignments: HashMap::new(),
    };

    tiers.assign(
        "unchanged",
        |prev, curr| {
            curr.git_patch_id.is_some()
                && prev.git_patch_id.as_deref() == curr.git_patch_id.as_deref()
        },
        |prev, curr| usize::from(prev.clean_subject != curr.clean_subject),
    );

    tiers.assign(
        "modified",
        |prev, curr| !curr.clean_subject.is_empty() && prev.clean_subject == curr.clean_subject,
        |_, _| 0,
    );

    let prev_counts = summary_counts(prev_patches);
    let curr_counts = summary_counts(curr_patches);
    tiers.assign(
        "modified",
        |prev, curr| {
            let (Some(p), Some(c)) = (
                SubjectParts::parse(&prev.clean_subject),
                SubjectParts::parse(&curr.clean_subject),
            ) else {
                return false;
            };
            // A summary shared by several patches on either side is a
            // janitorial pattern ("<driver>: Use devm_* helper"), not an
            // identity, so it never links anything.
            let key = p.summary_key();
            prev_counts.get(&key) == Some(&1)
                && curr_counts.get(&key) == Some(&1)
                && summaries_link(&p, &c)
        },
        |_, _| 0,
    );

    (tiers.assignments, tiers.matched_prev_ids)
}

struct LineageTiers<'a> {
    prev_patches: &'a [LineagePatch],
    curr_patches: &'a [LineagePatch],
    matched_prev_ids: HashSet<i64>,
    assignments: LineageAssignments,
}

impl LineageTiers<'_> {
    /// Links every still-unassigned current patch to the unmatched previous
    /// patch accepted by `matches`, preferring the lowest `rank`, then the
    /// closest part index, then the earliest part.
    fn assign(
        &mut self,
        status: &'static str,
        matches: impl Fn(&LineagePatch, &LineagePatch) -> bool,
        rank: impl Fn(&LineagePatch, &LineagePatch) -> usize,
    ) {
        for curr in self.curr_patches {
            if self.assignments.contains_key(&curr.id) {
                continue;
            }
            let best_prev = self
                .prev_patches
                .iter()
                .filter(|prev| !self.matched_prev_ids.contains(&prev.id) && matches(prev, curr))
                .min_by_key(|prev| {
                    let dist = prev.part_index.abs_diff(curr.part_index);
                    (rank(prev, curr), dist, prev.part_index)
                });
            if let Some(prev) = best_prev {
                self.matched_prev_ids.insert(prev.id);
                self.assignments
                    .insert(curr.id, (Some(prev.id), Some(status)));
            }
        }
    }
}

/// A cleaned subject split into its leading subsystem prefix components and
/// the summary that follows them.
#[derive(Debug, PartialEq, Eq)]
struct SubjectParts<'a> {
    prefix: Vec<&'a str>,
    summary: &'a str,
}

impl<'a> SubjectParts<'a> {
    /// Splits `"ufs: core: delegate x"` into `["ufs", "core"]` and
    /// `"delegate x"`. A component is a leading token without whitespace that
    /// is followed by `": "`, so a colon inside the summary
    /// (`"fix build error: undefined ref"`) stays in the summary. Returns
    /// `None` when there is no prefix or the summary is too short to be
    /// distinctive.
    fn parse(subject: &'a str) -> Option<Self> {
        let mut prefix = Vec::new();
        let mut rest = subject.trim();
        while let Some((head, tail)) = rest.split_once(':') {
            let head = head.trim();
            let is_component = !head.is_empty()
                && head.len() <= MAX_PREFIX_COMPONENT_LEN
                && !head.contains(char::is_whitespace)
                && tail.starts_with(char::is_whitespace);
            if !is_component {
                break;
            }
            prefix.push(head);
            rest = tail.trim_start();
        }
        let summary = rest.trim();
        (!prefix.is_empty() && summary.len() >= MIN_SUMMARY_LEN).then_some(Self { prefix, summary })
    }

    fn summary_key(&self) -> String {
        self.summary.to_lowercase()
    }
}

fn summary_counts(patches: &[LineagePatch]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for p in patches {
        if let Some(parts) = SubjectParts::parse(&p.clean_subject) {
            *counts.entry(parts.summary_key()).or_insert(0) += 1;
        }
    }
    counts
}

/// Two subjects name the same patch when their summaries match, their first
/// subsystem component matches, and one prefix only adds components to the
/// other (`ufs` -> `ufs: core`). Different drivers under one subsystem
/// (`iio: adc: foo` vs `iio: adc: bar`) or different architectures
/// (`x86` vs `arm64`) never link.
fn summaries_link(prev: &SubjectParts<'_>, curr: &SubjectParts<'_>) -> bool {
    let (Some(p_first), Some(c_first)) = (prev.prefix.first(), curr.prefix.first()) else {
        return false;
    };
    p_first.eq_ignore_ascii_case(c_first)
        && prev.summary.eq_ignore_ascii_case(curr.summary)
        && (is_component_subsequence(&prev.prefix, &curr.prefix)
            || is_component_subsequence(&curr.prefix, &prev.prefix))
}

fn is_component_subsequence(short: &[&str], long: &[&str]) -> bool {
    let mut long_iter = long.iter();
    short
        .iter()
        .all(|s| long_iter.any(|l| l.eq_ignore_ascii_case(s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(id: i64, part_index: i64, subject: &str) -> LineagePatch {
        LineagePatch {
            id,
            part_index,
            git_patch_id: None,
            clean_subject: subject.to_string(),
            existing_prev_id: None,
            existing_prev_status: None,
        }
    }

    #[test]
    fn test_subject_parts_keep_colons_inside_summary() {
        let parts = SubjectParts::parse("x86: fix build error: undefined reference").unwrap();
        assert_eq!(parts.prefix, vec!["x86"]);
        assert_eq!(parts.summary, "fix build error: undefined reference");

        let parts = SubjectParts::parse("arm64: dts: qcom: sm8550: add the thermal zones").unwrap();
        assert_eq!(parts.prefix, vec!["arm64", "dts", "qcom", "sm8550"]);

        assert!(SubjectParts::parse("no prefix at all in this subject").is_none());
        assert!(SubjectParts::parse("net: short").is_none());
    }

    #[test]
    fn test_tier2b_links_added_subsystem_component() {
        let prev = [patch(
            1,
            1,
            "ufs: delegate the interrupt handler to a threaded irq",
        )];
        let curr = [patch(
            10,
            1,
            "ufs: core: delegate the interrupt handler to a threaded irq",
        )];
        let (assignments, matched) = match_patch_lineage(&prev, &curr);
        assert_eq!(assignments.get(&10), Some(&(Some(1), Some("modified"))));
        assert!(matched.contains(&1));
    }

    #[test]
    fn test_tier2b_rejects_different_architectures_sharing_summary_tail() {
        let prev = [patch(
            1,
            1,
            "x86: fix build error: undefined reference to foo_init",
        )];
        let curr = [patch(
            10,
            1,
            "arm64: fix build error: undefined reference to foo_init",
        )];
        let (assignments, _) = match_patch_lineage(&prev, &curr);
        assert!(assignments.is_empty());
    }

    #[test]
    fn test_tier2b_rejects_janitorial_per_driver_series() {
        // v2 keeps the bar driver, drops foo, and adds baz.
        let prev = [
            patch(1, 1, "iio: adc: foo: Use devm_clk_get_enabled() helper"),
            patch(2, 2, "iio: adc: bar: Use devm_clk_get_enabled() helper"),
        ];
        let curr = [
            patch(10, 1, "iio: adc: bar: Use devm_clk_get_enabled() helper"),
            patch(11, 2, "iio: adc: baz: Use devm_clk_get_enabled() helper"),
        ];
        let (assignments, _) = match_patch_lineage(&prev, &curr);
        assert_eq!(assignments.get(&10), Some(&(Some(2), Some("modified"))));
        assert!(!assignments.contains_key(&11));

        // Same pattern with bare driver prefixes and a unique summary per side.
        let prev = [patch(1, 1, "drv-foo: Use devm_clk_get_enabled() helper")];
        let curr = [patch(10, 1, "drv-baz: Use devm_clk_get_enabled() helper")];
        assert!(match_patch_lineage(&prev, &curr).0.is_empty());

        // Sibling drivers under one subsystem never link even when unique.
        let prev = [patch(
            1,
            1,
            "iio: adc: foo: Use devm_clk_get_enabled() helper",
        )];
        let curr = [patch(
            10,
            1,
            "iio: adc: baz: Use devm_clk_get_enabled() helper",
        )];
        assert!(match_patch_lineage(&prev, &curr).0.is_empty());
    }

    #[test]
    fn test_tiers_prefer_git_patch_id_then_exact_subject() {
        let mut p1 = patch(1, 1, "net: rename the queue helper for clarity");
        p1.git_patch_id = Some("abc".to_string());
        let p2 = patch(2, 2, "net: fix the queue teardown ordering issue");
        let mut c1 = patch(10, 2, "net: rename the queue helper for clarity");
        c1.git_patch_id = Some("abc".to_string());
        let c2 = patch(11, 1, "net: fix the queue teardown ordering issue");
        let (assignments, matched) = match_patch_lineage(&[p1, p2], &[c1, c2]);
        assert_eq!(assignments.get(&10), Some(&(Some(1), Some("unchanged"))));
        assert_eq!(assignments.get(&11), Some(&(Some(2), Some("modified"))));
        assert_eq!(matched.len(), 2);
    }
}
