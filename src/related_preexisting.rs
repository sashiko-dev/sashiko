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

/// Placeholder in `report_text` that the mailer replaces with the footnote
/// reference (`[N]`) of the disclosed bug, or strips when no link is emitted.
pub const BUG_REF_PLACEHOLDER: &str = "[BUG_REF]";

/// Returns the placeholder token for the `idx`-th related pre-existing bug on a
/// patch (`"[BUG_REF]"` for `idx == 0`, `"[BUG_REF:2]"` for `idx == 1`, etc.).
///
/// Note that `"[BUG_REF]"` is not a substring of `"[BUG_REF:2]"`, so each item's
/// placeholder matches unambiguously in the `report` stage output.
pub fn bug_ref_placeholder(idx: usize) -> String {
    if idx == 0 {
        BUG_REF_PLACEHOLDER.to_string()
    } else {
        format!("[BUG_REF:{}]", idx + 1)
    }
}

fn strip_specific_placeholder(text: &str, placeholder: &str) -> String {
    text.replace(&format!(" {placeholder}"), "")
        .replace(&format!("{placeholder} "), "")
        .replace(placeholder, "")
}

/// Strips all `[BUG_REF]` and `[BUG_REF:N]` placeholders from `text`.
pub fn strip_all_bug_ref_placeholders(text: &str) -> String {
    let mut out = strip_specific_placeholder(text, BUG_REF_PLACEHOLDER);
    for idx in 1..=8 {
        let ph = bug_ref_placeholder(idx);
        if out.contains(&ph) {
            out = strip_specific_placeholder(&out, &ph);
        }
    }
    out
}

/// Removes the inline comment block (and its dedicated quoted diff hunk) that
/// contains `placeholder` from `inline_review`.
///
/// If a dropped block's quoted hunk carried a leading `> diff --git` file
/// header that the next kept block in the same file relies on, or if two
/// comments shared the same quoted hunk, the needed quote lines are preserved
/// on the next kept block. When no comment blocks remain, returns
/// `"No issues found."`.
pub fn remove_inline_comment_with_placeholder(inline_review: &str, placeholder: &str) -> String {
    if !inline_review.contains(placeholder) {
        return inline_review.to_string();
    }
    let (raw_body, raw_footnotes, _) = split_inline_review_footnotes(inline_review);
    let is_quote_or_snip = |line: &str| {
        let trimmed = line.trim();
        trimmed.starts_with('>') || trimmed == "[ ... ]"
    };
    let is_severity_line = |line: &str| {
        let trimmed = line.trim();
        trimmed.starts_with("[Severity:") && trimmed.ends_with(']')
    };

    let all_lines: Vec<&str> = raw_body.lines().collect();
    let Some(first_quote_idx) = all_lines
        .iter()
        .position(|&l| is_quote_or_snip(l) || is_severity_line(l))
    else {
        return strip_specific_placeholder(inline_review, placeholder);
    };
    let mut header_lines: Vec<&str> = all_lines[..first_quote_idx].to_vec();
    while header_lines.last().is_some_and(|l| l.trim().is_empty()) {
        header_lines.pop();
    }

    struct CommentUnit<'a> {
        quote_lines: Vec<&'a str>,
        comment_lines: Vec<&'a str>,
    }

    let mut units: Vec<CommentUnit<'_>> = Vec::new();
    let mut cur_quote: Vec<&str> = Vec::new();
    let mut cur_comment: Vec<&str> = Vec::new();
    let mut in_comment = false;

    for &line in &all_lines[first_quote_idx..] {
        let trimmed = line.trim();
        if !in_comment {
            if is_quote_or_snip(line) || trimmed.is_empty() {
                cur_quote.push(line);
            } else {
                while cur_quote.last().is_some_and(|l| l.trim().is_empty()) {
                    cur_quote.pop();
                }
                in_comment = true;
                cur_comment.push(line);
            }
        } else if is_quote_or_snip(line) {
            while cur_comment.last().is_some_and(|l| l.trim().is_empty()) {
                cur_comment.pop();
            }
            units.push(CommentUnit {
                quote_lines: std::mem::take(&mut cur_quote),
                comment_lines: std::mem::take(&mut cur_comment),
            });
            in_comment = false;
            cur_quote.push(line);
        } else if is_severity_line(line) {
            while cur_comment.last().is_some_and(|l| l.trim().is_empty()) {
                cur_comment.pop();
            }
            units.push(CommentUnit {
                quote_lines: std::mem::take(&mut cur_quote),
                comment_lines: std::mem::take(&mut cur_comment),
            });
            cur_comment.push(line);
        } else {
            cur_comment.push(line);
        }
    }
    if !cur_comment.is_empty() {
        while cur_comment.last().is_some_and(|l| l.trim().is_empty()) {
            cur_comment.pop();
        }
        units.push(CommentUnit {
            quote_lines: cur_quote,
            comment_lines: cur_comment,
        });
    }

    let extract_file_header = |quotes: &[&str]| -> Vec<String> {
        let mut out = Vec::new();
        let mut seen_diff = false;
        for &l in quotes {
            let unquoted = l.trim_start().strip_prefix('>').unwrap_or(l).trim_start();
            if unquoted.starts_with("diff --git ") {
                seen_diff = true;
                out.push(l.to_string());
            } else if seen_diff
                && (unquoted.starts_with("index ")
                    || unquoted.starts_with("--- ")
                    || unquoted.starts_with("+++ ")
                    || unquoted.starts_with("new file ")
                    || unquoted.starts_with("deleted file ")
                    || unquoted.starts_with("similarity index ")
                    || unquoted.starts_with("rename "))
            {
                out.push(l.to_string());
            } else if seen_diff {
                break;
            }
        }
        out
    };
    let has_file_header = |quotes: &[&str]| -> bool {
        quotes.iter().any(|&l| {
            l.trim_start()
                .strip_prefix('>')
                .unwrap_or(l)
                .trim_start()
                .starts_with("diff --git ")
        })
    };

    let mut kept_units: Vec<(Vec<String>, Vec<String>)> = Vec::new();
    let mut carry_quote: Vec<String> = Vec::new();
    let mut carry_file_header: Vec<String> = Vec::new();

    for unit in units {
        let mut q: Vec<String> = unit.quote_lines.into_iter().map(str::to_string).collect();
        while q.first().is_some_and(|l| l.trim().is_empty()) {
            q.remove(0);
        }
        let c: Vec<String> = unit.comment_lines.into_iter().map(str::to_string).collect();
        let unit_matches =
            c.iter().any(|l| l.contains(placeholder)) || q.iter().any(|l| l.contains(placeholder));
        if unit_matches {
            if !q.is_empty() {
                let fh = extract_file_header(&q.iter().map(String::as_str).collect::<Vec<_>>());
                if !fh.is_empty() {
                    carry_file_header = fh;
                }
                carry_quote = q;
            }
        } else {
            if q.is_empty() && !carry_quote.is_empty() {
                q = std::mem::take(&mut carry_quote);
                carry_file_header.clear();
            } else {
                carry_quote.clear();
                let q_refs: Vec<&str> = q.iter().map(String::as_str).collect();
                if has_file_header(&q_refs) {
                    carry_file_header.clear();
                } else if !carry_file_header.is_empty() {
                    let mut merged = std::mem::take(&mut carry_file_header);
                    merged.push(String::new());
                    merged.extend(q);
                    q = merged;
                }
            }
            kept_units.push((q, c));
        }
    }

    if kept_units.is_empty() {
        return "No issues found.".to_string();
    }

    let mut sections: Vec<String> = Vec::new();
    if !header_lines.is_empty() {
        sections.push(header_lines.join("\n"));
    }
    for (q, c) in kept_units {
        if !q.is_empty() {
            sections.push(q.join("\n"));
        }
        if !c.is_empty() {
            sections.push(c.join("\n"));
        }
    }
    if !raw_footnotes.is_empty() {
        sections.push(raw_footnotes.join("\n"));
    }
    sections.join("\n\n")
}

/// Replaces [`BUG_REF_PLACEHOLDER`] in `report_text` with `[n]` when a
/// footnote number is present, or removes the placeholder when rendering
/// without footnote links. If `report_text` lacks the placeholder and a
/// footnote number is given, inserts `[n]` into the first prose line.
pub fn render_report_text_with_footnote(report_text: &str, footnote: Option<usize>) -> String {
    match footnote {
        Some(n) => {
            let marker = format!("[{n}]");
            if report_text.contains(BUG_REF_PLACEHOLDER) {
                return report_text.replace(BUG_REF_PLACEHOLDER, &marker);
            }
            let mut inserted = false;
            let mut out = Vec::new();
            for line in report_text.lines() {
                let trimmed = line.trim();
                if !inserted && !trimmed.is_empty() && !trimmed.starts_with('>') {
                    inserted = true;
                    if let Some(pos) = line.rfind([':', '.']) {
                        let (head, tail) = line.split_at(pos);
                        out.push(format!("{head} {marker}{tail}"));
                    } else {
                        out.push(format!("{line} {marker}"));
                    }
                } else {
                    out.push(line.to_string());
                }
            }
            out.join("\n")
        }
        None => report_text
            .replace(&format!(" {BUG_REF_PLACEHOLDER}"), "")
            .replace(&format!("{BUG_REF_PLACEHOLDER} "), "")
            .replace(BUG_REF_PLACEHOLDER, ""),
    }
}

fn parse_footnote_definition_line(line: &str) -> Option<usize> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix('[')?;
    let (digits, after_bracket) = rest.split_once(']')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let num: usize = digits.parse().ok()?;
    if num == 0 {
        return None;
    }
    let url = after_bracket.trim_start();
    if (url.starts_with("http://") || url.starts_with("https://"))
        && !url.bytes().any(|b| b.is_ascii_whitespace())
    {
        Some(num)
    } else {
        None
    }
}

/// Splits a trailing block of `[N] https://...` footnote definitions off the
/// end of `inline_review`, returning `(body_without_footnotes, footnote_lines, next_ref_number)`.
///
/// Checking only the trailing footnote definition block avoids false matches on
/// C array indexing such as `buf[1]` or `arr[256]` inside quoted diff hunks.
pub fn split_inline_review_footnotes(inline_review: &str) -> (&str, Vec<&str>, usize) {
    let trimmed = inline_review.trim_end();
    if trimmed.is_empty() {
        return ("", Vec::new(), 1);
    }
    let lines: Vec<&str> = trimmed.lines().collect();
    let mut footnote_start = lines.len();
    let mut max_ref = 0usize;
    let mut footnotes = Vec::new();
    for (idx, &line) in lines.iter().enumerate().rev() {
        if line.trim().is_empty() {
            if !footnotes.is_empty() {
                break;
            }
            continue;
        }
        if let Some(n) = parse_footnote_definition_line(line) {
            footnote_start = idx;
            max_ref = max_ref.max(n);
            footnotes.push(line.trim());
        } else {
            break;
        }
    }
    if footnotes.is_empty() {
        return (trimmed, Vec::new(), 1);
    }
    footnotes.reverse();
    let footnote_offset = lines[footnote_start].as_ptr() as usize - trimmed.as_ptr() as usize;
    let body = trimmed[..footnote_offset].trim_end();
    (body, footnotes, max_ref + 1)
}

#[cfg(feature = "server")]
pub use gate::{GateInputs, GateSkipReason, evaluate_gate};
#[cfg(feature = "server")]
pub use reporting::{
    compose_inline_review_with_related, disclosure_candidates, related_email_footnotes,
    related_email_header, related_email_header_from, related_email_section,
    related_email_section_from, retain_reserved_disclosures, same_disclosed_bug,
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
                let bug_id = item.get("bug_id").and_then(Value::as_i64);
                let bugid = item
                    .get("bugid")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
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
                    bug_id,
                    bugid,
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
        if let (Some(a), Some(b)) = (candidate.bug_id, existing.bug_id)
            && a == b
        {
            return true;
        }
        let (Ok(a), Ok(b)) = (
            serde_json::from_str::<BugKey>(&candidate.bug_key_json),
            serde_json::from_str::<BugKey>(&existing.bug_key_json),
        ) else {
            return true;
        };
        a.is_same_bug(&b)
    }

    use super::render_report_text_with_footnote;
    use crate::email_text::{bullet, count, wrap, wrap_prose};

    fn footnote_numbers(rows: &[PreexistingDisclosure], start_ref: usize) -> Vec<Option<usize>> {
        let mut next = start_ref.max(1);
        rows.iter()
            .map(|d| {
                if d.bugid.as_deref().is_some_and(|s| !s.trim().is_empty()) {
                    let cur = next;
                    next += 1;
                    Some(cur)
                } else {
                    None
                }
            })
            .collect()
    }

    fn summary_lines(rows: &[PreexistingDisclosure], start_ref: usize) -> String {
        let refs = footnote_numbers(rows, start_ref);
        rows.iter()
            .zip(refs)
            .map(|(d, r)| match r {
                Some(n) => bullet(d.severity.as_str(), &format!("{} [{n}]", d.problem)),
                None => bullet(d.severity.as_str(), &d.problem),
            })
            .collect()
    }

    fn issues(n: usize) -> String {
        count(
            n,
            "related pre-existing issue",
            "related pre-existing issues",
        )
    }

    /// The opening lines of a review email that carries related bugs,
    /// wrapped at 72 columns, numbering related bug footnotes from `start_ref`.
    pub fn related_email_header_from(
        new_issues: usize,
        rows: &[PreexistingDisclosure],
        start_ref: usize,
    ) -> String {
        if rows.is_empty() {
            return String::new();
        }
        let bullets = summary_lines(rows, start_ref);
        if new_issues == 0 {
            let sentence = format!(
                "Thank you for your contribution! Sashiko AI review found no new issues in this patch, but noticed {} it does not introduce:",
                issues(rows.len())
            );
            format!("{}{bullets}--\n\n", wrap(&sentence, "", ""))
        } else {
            let sentence = format!(
                "It also noticed {} this patch does not introduce:",
                issues(rows.len())
            );
            format!("{}{bullets}", wrap(&sentence, "", ""))
        }
    }

    /// The opening lines of a review email that carries related bugs,
    /// wrapped at 72 columns. `new_issues` is the number of findings
    /// the patch introduces.
    pub fn related_email_header(new_issues: usize, rows: &[PreexistingDisclosure]) -> String {
        related_email_header_from(new_issues, rows, 1)
    }

    /// The notes for the related bugs, appended after the inline review,
    /// with their footnote references substituted from `start_ref` and prose
    /// wrapped at 72 columns.
    pub fn related_email_section_from(rows: &[PreexistingDisclosure], start_ref: usize) -> String {
        let refs = footnote_numbers(rows, start_ref);
        rows.iter()
            .zip(refs)
            .map(|(d, r)| wrap_prose(&render_report_text_with_footnote(&d.report_text, r)))
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// The notes for the related bugs, appended after the inline review,
    /// with their prose wrapped at 72 columns.
    pub fn related_email_section(rows: &[PreexistingDisclosure]) -> String {
        related_email_section_from(rows, 1)
    }

    /// Footnote link definitions (`[N] https://<domain>/#/bug/<bugid>`) for
    /// disclosed related bugs that carry a canonical `bugid`, numbered from
    /// `start_ref`.
    pub fn related_email_footnotes(
        rows: &[PreexistingDisclosure],
        start_ref: usize,
        domain: &str,
    ) -> Vec<String> {
        let refs = footnote_numbers(rows, start_ref);
        rows.iter()
            .zip(refs)
            .filter_map(|(d, r)| {
                let n = r?;
                let bugid = d.bugid.as_deref()?.trim();
                Some(format!("[{n}] https://{domain}/#/bug/{bugid}"))
            })
            .collect()
    }

    fn strip_one_placeholder(text: &str, placeholder: &str) -> String {
        for pat in [
            format!(" {placeholder}"),
            format!("{placeholder} "),
            placeholder.to_string(),
        ] {
            if text.contains(&pat) {
                return text.replacen(&pat, "", 1);
            }
        }
        text.to_string()
    }

    /// Filters `review["related_preexisting"]` in place to keep only the
    /// disclosures that won reservation in `reserved`, moving any unreserved
    /// items into `review["related_preexisting_dismissed"]` and removing their
    /// inlined comment blocks from `review["review_inline"]` /
    /// `review["inline_review"]` so `output_raw`, `reviews.inline_review`, and
    /// outbound emails match the disclosures actually reserved.
    pub fn retain_reserved_disclosures(review: &mut Value, reserved: &[PreexistingDisclosure]) {
        let Some(obj) = review.as_object_mut() else {
            return;
        };
        let Some(items) = obj
            .get_mut("related_preexisting")
            .and_then(Value::as_array_mut)
        else {
            return;
        };
        if items.is_empty() {
            return;
        }
        let mut kept: Vec<(usize, Value)> = Vec::with_capacity(items.len());
        let mut dropped: Vec<(usize, Value)> = Vec::new();
        for (old_idx, mut item) in std::mem::take(items).into_iter().enumerate() {
            let fp = BugKey::from_item(&item).fingerprint;
            let bug_id = item.get("bug_id").and_then(Value::as_i64);
            let is_reserved = reserved
                .iter()
                .any(|r| r.fingerprint == fp || (bug_id.is_some() && r.bug_id == bug_id));
            if is_reserved {
                kept.push((old_idx, item));
            } else {
                if let Some(map) = item.as_object_mut() {
                    map.insert(
                        "dismissed_by".to_string(),
                        Value::String("reservation".to_string()),
                    );
                    map.insert(
                        "dismissed_reason".to_string(),
                        Value::String(
                            "Already reserved or disclosed by another patch review.".to_string(),
                        ),
                    );
                }
                dropped.push((old_idx, item));
            }
        }
        let kept_indices: Vec<usize> = kept.iter().map(|(idx, _)| *idx).collect();
        let dropped_indices: Vec<usize> = dropped.iter().map(|(idx, _)| *idx).collect();
        *items = kept.into_iter().map(|(_, item)| item).collect();
        let all_related_dropped = items.is_empty() && !dropped_indices.is_empty();
        if !dropped.is_empty() {
            let dismissed = obj
                .entry("related_preexisting_dismissed")
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Some(arr) = dismissed.as_array_mut() {
                arr.extend(dropped.into_iter().map(|(_, item)| item));
            }
        }
        if !dropped_indices.is_empty() {
            let has_any_findings = obj
                .get("findings")
                .and_then(Value::as_array)
                .is_some_and(|arr| !arr.is_empty());
            for field in ["review_inline", "inline_review"] {
                if let Some(text) = obj.get(field).and_then(Value::as_str).map(str::to_string) {
                    let mut updated = if all_related_dropped && !has_any_findings {
                        "No issues found.".to_string()
                    } else {
                        let mut s = text;
                        for &d_idx in &dropped_indices {
                            let ph = super::bug_ref_placeholder(d_idx);
                            s = super::remove_inline_comment_with_placeholder(&s, &ph);
                        }
                        for (new_idx, &old_idx) in kept_indices.iter().enumerate() {
                            if new_idx != old_idx {
                                let old_ph = super::bug_ref_placeholder(old_idx);
                                let new_ph = super::bug_ref_placeholder(new_idx);
                                s = s.replace(&old_ph, &new_ph);
                            }
                        }
                        s
                    };
                    if all_related_dropped && !has_any_findings {
                        updated = "No issues found.".to_string();
                    }
                    obj.insert(field.to_string(), Value::String(updated));
                }
            }
        }
    }

    /// Composes the inline review body (shared by `reviews.inline_review` and
    /// the outbound review email body) by substituting any `[BUG_REF]` /
    /// `[BUG_REF:N]` placeholders in `inline_review` with sequential `[N]`
    /// footnote numbers and appending trailing
    /// `[N] https://<domain>/#/bug/<bugid>` footnote definitions, returning
    /// `(start_ref, composed_body)`.
    ///
    /// When `inline_review` is empty or `"No issues found."` (for example in
    /// unit tests that call `queue_notifications` directly without running the
    /// `report` stage), falls back to `related_email_section_from`. Repeated
    /// calls on an already-composed `inline_review` are idempotent.
    pub fn compose_inline_review_with_related(
        inline_review: &str,
        has_any_findings: bool,
        rows: &[PreexistingDisclosure],
        domain: &str,
    ) -> (usize, String) {
        let (raw_body, raw_footnotes, _) = super::split_inline_review_footnotes(inline_review);
        let (kept_footnotes, start_ref) = if rows.is_empty() {
            let max_all = raw_footnotes
                .iter()
                .filter_map(|line| super::parse_footnote_definition_line(line))
                .max()
                .unwrap_or(0);
            (raw_footnotes, max_all + 1)
        } else {
            let kept: Vec<&str> = raw_footnotes
                .into_iter()
                .filter(|line| !line.contains("/#/bug/"))
                .collect();
            let max_kept = kept
                .iter()
                .filter_map(|line| super::parse_footnote_definition_line(line))
                .max()
                .unwrap_or(0);
            (kept, max_kept + 1)
        };

        let base_body =
            if !has_any_findings && (raw_body.is_empty() || raw_body == "No issues found.") {
                ""
            } else {
                raw_body
            };

        let mut body = base_body.to_string();
        if !rows.is_empty() {
            if body.contains("[BUG_REF") {
                let refs = footnote_numbers(rows, start_ref);
                for (idx, r) in refs.iter().copied().enumerate() {
                    let ph = super::bug_ref_placeholder(idx);
                    match r {
                        Some(n) => {
                            let target = if body.contains(&ph) {
                                ph.as_str()
                            } else {
                                super::BUG_REF_PLACEHOLDER
                            };
                            body = body.replacen(target, &format!("[{n}]"), 1);
                        }
                        None => {
                            let remaining_some =
                                refs.iter().skip(idx + 1).filter(|x| x.is_some()).count();
                            if idx > 0 && body.contains(&ph) {
                                body = strip_one_placeholder(&body, &ph);
                            } else if idx == 0 && body.matches("[BUG_REF").count() > remaining_some
                            {
                                body = strip_one_placeholder(&body, super::BUG_REF_PLACEHOLDER);
                            }
                        }
                    }
                }
                body = super::strip_all_bug_ref_placeholders(&body);
            } else if body.is_empty() {
                body = related_email_section_from(rows, start_ref);
            }
        } else if body.contains("[BUG_REF") {
            body = super::strip_all_bug_ref_placeholders(&body);
        }

        let related_footnotes = related_email_footnotes(rows, start_ref, domain);
        if !kept_footnotes.is_empty() || !related_footnotes.is_empty() {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            let all_footnotes: Vec<&str> = kept_footnotes
                .into_iter()
                .chain(related_footnotes.iter().map(String::as_str))
                .collect();
            body.push_str(&all_footnotes.join("\n"));
        }
        (start_ref, body)
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
                    "bug_id": 42,
                    "bugid": "linux-42-uuid",
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
        assert_eq!(c.bug_id, Some(42));
        assert_eq!(c.bugid.as_deref(), Some("linux-42-uuid"));

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
            bugid: None,
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
            bugid: None,
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

    #[cfg(feature = "server")]
    #[test]
    fn test_related_email_footnotes_and_inline_split() {
        use crate::db::{DisclosureRelation, PreexistingDisclosure};

        let inline = "> +buf[256] = 0;\n\nStill buggy (raised in v1 [4]).\n\n[4] https://lore.kernel.org/all/prior@example.com/\n";
        let (body, refs, next_ref) = split_inline_review_footnotes(inline);
        assert_eq!(body, "> +buf[256] = 0;\n\nStill buggy (raised in v1 [4]).");
        assert_eq!(
            refs,
            vec!["[4] https://lore.kernel.org/all/prior@example.com/"]
        );
        assert_eq!(next_ref, 5);

        let no_refs = "> +arr[9] = 1;\n\nPlain inline review.";
        let (body2, refs2, next_ref2) = split_inline_review_footnotes(no_refs);
        assert_eq!(body2, no_refs);
        assert!(refs2.is_empty());
        assert_eq!(next_ref2, 1);

        let crlf_inline = "> +buf[256] = 0;\r\n\r\nStill buggy — see v1 [2].\r\n\r\n[2] https://lore.kernel.org/all/v1@example.com/\r\n";
        let (body_crlf, refs_crlf, next_crlf) = split_inline_review_footnotes(crlf_inline);
        assert_eq!(
            body_crlf,
            "> +buf[256] = 0;\r\n\r\nStill buggy — see v1 [2]."
        );
        assert_eq!(
            refs_crlf,
            vec!["[2] https://lore.kernel.org/all/v1@example.com/"]
        );
        assert_eq!(next_crlf, 3);

        let row = PreexistingDisclosure {
            id: 1,
            patch_message_id: "p@x".to_string(),
            fingerprint: "fp".to_string(),
            problem: "mm: lock held on the -EINVAL return".to_string(),
            severity: Severity::High,
            relation: DisclosureRelation::SamePattern,
            locations_json: "[]".to_string(),
            bug_key_json: "{}".to_string(),
            report_text: "This isn't a bug introduced by this patch, but the same missing unlock [BUG_REF] exists a few lines below the path you fixed:\n\n> \treturn -EINVAL;".to_string(),
            reserved_at: 0,
            disclosed_at: None,
            bug_id: Some(7),
            bugid: Some("linux-7f9a2c4b-3e1d-4a80-9b5c-1d2e3f4a5b6c".to_string()),
            files: vec!["mm/foo.c".to_string()],
        };

        let header = related_email_header_from(1, std::slice::from_ref(&row), next_ref);
        assert!(
            header.contains("- [High] mm: lock held on the -EINVAL return [5]\n"),
            "{header}"
        );
        let section = related_email_section_from(std::slice::from_ref(&row), next_ref);
        assert!(
            section.contains("same missing unlock\n[5] exists"),
            "{section}"
        );
        assert!(!section.contains(BUG_REF_PLACEHOLDER), "{section}");
        let footnotes =
            related_email_footnotes(std::slice::from_ref(&row), next_ref, "sashiko.dev");
        assert_eq!(
            footnotes,
            vec!["[5] https://sashiko.dev/#/bug/linux-7f9a2c4b-3e1d-4a80-9b5c-1d2e3f4a5b6c"]
        );

        let inline_with_bug_ref = "> +buf[256] = 0;\n\n[Severity: High]\nStill buggy (raised in v1 [4]).\n\n> \treturn -EINVAL;\n\n[Severity: High]\nThis problem wasn't introduced by this patch, but the same missing unlock [BUG_REF] exists here.\n\n[4] https://lore.kernel.org/all/prior@example.com/\n";
        let (start_ref, composed) = compose_inline_review_with_related(
            inline_with_bug_ref,
            true,
            std::slice::from_ref(&row),
            "sashiko.dev",
        );
        assert_eq!(start_ref, 5);
        assert!(!composed.contains("Related pre-existing issue"));
        assert!(!composed.contains(BUG_REF_PLACEHOLDER));
        assert!(composed.contains("same missing unlock [5] exists here."));
        assert!(composed.contains("[4] https://lore.kernel.org/all/prior@example.com/\n[5] https://sashiko.dev/#/bug/linux-7f9a2c4b-3e1d-4a80-9b5c-1d2e3f4a5b6c"));

        // Re-composing an already-composed inline review must be idempotent.
        let (start_ref_again, composed_again) = compose_inline_review_with_related(
            &composed,
            true,
            std::slice::from_ref(&row),
            "sashiko.dev",
        );
        assert_eq!(start_ref_again, 5);
        assert_eq!(composed_again, composed);

        // Without new findings and with a placeholder string, falls back to the related note and footnotes.
        let (start_no_new, composed_no_new) = compose_inline_review_with_related(
            "No issues found.",
            false,
            std::slice::from_ref(&row),
            "sashiko.dev",
        );
        assert_eq!(start_no_new, 1);
        assert!(!composed_no_new.contains("Related pre-existing issue"));
        assert!(composed_no_new.contains("same missing unlock\n[1] exists"));
        assert!(composed_no_new.ends_with(
            "\n\n[1] https://sashiko.dev/#/bug/linux-7f9a2c4b-3e1d-4a80-9b5c-1d2e3f4a5b6c"
        ));

        // When row 0 has bugid: None and row 1 has bugid: Some, row 0's
        // [BUG_REF] is stripped and row 1's [BUG_REF:2] becomes [1].
        let mut row0_no_id = row.clone();
        row0_no_id.bug_id = None;
        row0_no_id.bugid = None;
        row0_no_id.fingerprint = "fp0".to_string();
        let two_inline = "> +a();\n\n[Severity: High]\nFirst pre-existing [BUG_REF] issue.\n\n> +b();\n\n[Severity: High]\nSecond pre-existing [BUG_REF:2] issue.";
        let (_, composed_two) = compose_inline_review_with_related(
            two_inline,
            false,
            &[row0_no_id, row.clone()],
            "sashiko.dev",
        );
        assert!(composed_two.contains("First pre-existing issue."));
        assert!(composed_two.contains("Second pre-existing [1] issue."));
        assert!(!composed_two.contains("[BUG_REF"));

        // When reservation drops the first related bug and keeps the second,
        // retain_reserved_disclosures removes the first bug's inline comment
        // block and renumbers the second bug's placeholder to [BUG_REF].
        let mut review_val = json!({
            "findings": [{"problem": "new bug", "severity": "High", "preexisting": false}],
            "related_preexisting": [
                {"problem": "dropped bug", "severity": "High", "locations": [{"file": "a.c"}]},
                {"problem": "mm: lock held on the -EINVAL return", "severity": "High", "bug_id": 7, "bugid": "linux-7f9a2c4b-3e1d-4a80-9b5c-1d2e3f4a5b6c", "locations": [{"file": "mm/foo.c"}]}
            ],
            "review_inline": "commit 0123456789abcdef\nAuthor: A <a@b.c>\n\n> diff --git a/mm/foo.c b/mm/foo.c\n> --- a/mm/foo.c\n> +++ b/mm/foo.c\n> @@ -1,3 +1,4 @@\n> +dropped();\n\n[Severity: High]\nThis problem wasn't introduced by this patch, but dropped [BUG_REF] here.\n\n[ ... ]\n\n> @@ -10,3 +10,4 @@\n> +new_bug();\n\n[Severity: High]\nNew bug comment.\n\n[ ... ]\n\n> @@ -20,3 +20,4 @@\n> +kept();\n\n[Severity: High]\nThis problem wasn't introduced by this patch, but kept [BUG_REF:2] here."
        });
        retain_reserved_disclosures(&mut review_val, std::slice::from_ref(&row));
        let inline_after = review_val["review_inline"].as_str().unwrap();
        assert!(!inline_after.contains("dropped"));
        assert!(inline_after.contains("> diff --git a/mm/foo.c b/mm/foo.c"));
        assert!(inline_after.contains("New bug comment."));
        assert!(inline_after.contains("kept [BUG_REF] here."));
    }
}
