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

//! Plain-text layout for the review emails Sashiko writes itself: the
//! summary header and the related pre-existing bug notes. Lines are wrapped
//! at 72 columns, the convention of kernel mailing lists.

/// Column review email text is wrapped at.
pub const WRAP_WIDTH: usize = 72;

/// Greedy word wrap of `text` at `WRAP_WIDTH`. The first line starts with
/// `first`, the others with `rest`; a word longer than the width keeps a
/// line to itself. Every line ends with a newline.
pub fn wrap(text: &str, first: &str, rest: &str) -> String {
    let mut out = String::new();
    let mut line = first.to_string();
    let mut prefix_len = first.len();
    for word in text.split_whitespace() {
        let empty = line.len() == prefix_len;
        if !empty && line.len() + 1 + word.len() > WRAP_WIDTH {
            out.push_str(&line);
            out.push('\n');
            line = rest.to_string();
            prefix_len = rest.len();
        } else if !empty {
            line.push(' ');
        }
        line.push_str(word);
    }
    out.push_str(&line);
    out.push('\n');
    out
}

/// A `- [Severity] problem` summary line, wrapped with a hanging indent.
pub fn bullet(severity: &str, problem: &str) -> String {
    wrap(&format!("[{severity}] {}", problem.trim()), "- ", "  ")
}

/// `"1 <singular>"` or `"<n> <plural>"`.
pub fn count(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {plural}")
    }
}

/// The opening sentence of a review email with `n` new findings.
pub fn findings_header(n: usize) -> String {
    wrap(
        &format!(
            "Thank you for your contribution! Sashiko AI review found {} to consider:",
            count(n, "potential issue", "potential issues")
        ),
        "",
        "",
    )
}

/// Re-wraps the prose of free text at `WRAP_WIDTH`: consecutive prose lines
/// form a paragraph, while quoted (`>`), indented and empty lines are kept
/// verbatim so quoted code is never reflowed. No trailing newline.
pub fn wrap_prose(text: &str) -> String {
    let mut out = String::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let flush = |paragraph: &mut Vec<&str>, out: &mut String| {
        if !paragraph.is_empty() {
            out.push_str(&wrap(&paragraph.join(" "), "", ""));
            paragraph.clear();
        }
    };
    for line in text.trim().lines() {
        let verbatim =
            line.trim().is_empty() || line.starts_with('>') || line.starts_with([' ', '\t']);
        if verbatim {
            flush(&mut paragraph, &mut out);
            out.push_str(line.trim_end());
            out.push('\n');
        } else {
            paragraph.push(line.trim());
        }
    }
    flush(&mut paragraph, &mut out);
    out.trim_end().to_string()
}

/// Formats sorted patch numbers compactly for English prose. Contiguous runs
/// of 3 or more collapse to `start-end`; shorter runs stay explicit.
///
/// Examples:
/// - `[3]` -> `"3"`
/// - `[7, 9]` -> `"7 and 9"`
/// - `[1, 2, 3, 4, 5, 6, 8, 10]` -> `"1-6, 8, and 10"`
pub fn format_patch_ranges(indices: &[i64]) -> String {
    let mut sorted = indices.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.is_empty() {
        return String::new();
    }

    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            i += 1;
            end = sorted[i];
        }
        let run_len = end - start + 1;
        if run_len >= 3 {
            parts.push(format!("{start}-{end}"));
        } else if run_len == 2 {
            parts.push(start.to_string());
            parts.push(end.to_string());
        } else {
            parts.push(start.to_string());
        }
        i += 1;
    }

    match parts.len() {
        0 => String::new(),
        1 => parts[0].clone(),
        2 => format!("{} and {}", parts[0], parts[1]),
        _ => {
            let last = parts.pop().unwrap_or_default();
            format!("{}, and {}", parts.join(", "), last)
        }
    }
}

/// Formats `"Patch 3"` / `"Patches 1-6, 8, and 10"` (or lowercase when
/// `capitalized` is false).
pub fn patch_label_ranges(indices: &[i64], capitalized: bool) -> String {
    let mut unique = indices.to_vec();
    unique.sort_unstable();
    unique.dedup();
    let ranges = format_patch_ranges(&unique);
    let noun = match (unique.len() == 1, capitalized) {
        (true, true) => "Patch",
        (true, false) => "patch",
        (false, true) => "Patches",
        (false, false) => "patches",
    };
    format!("{noun} {ranges}")
}

/// Strips leading `[...]` subject prefix tags (e.g. `[PATCH v2 7/10] `)
/// to produce a concise patch title for summary bullets.
pub fn strip_subject_brackets(subject: &str) -> &str {
    let mut s = subject.trim();
    while s.starts_with('[') {
        let Some(close) = s.find(']') else {
            break;
        };
        s = s[close + 1..].trim_start();
    }
    s
}

fn truncate_title(title: &str, max_chars: usize) -> String {
    let clean = crate::patch::clean_display_name(title);
    let trimmed = clean.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let keep = max_chars.saturating_sub(3);
    let mut out: String = trimmed.chars().take(keep).collect();
    let trimmed_end = out.trim_end().len();
    out.truncate(trimmed_end);
    out.push_str("...");
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeriesPatchOutcomeKind {
    Clean,
    WithIssues {
        critical: usize,
        high: usize,
        medium: usize,
        low: usize,
        related_preexisting: usize,
    },
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesPatchOutcome {
    pub part_index: i64,
    pub subject: String,
    pub kind: SeriesPatchOutcomeKind,
}

fn format_severity_breakdown(critical: usize, high: usize, medium: usize, low: usize) -> String {
    let mut parts = Vec::new();
    if critical > 0 {
        parts.push(format!("{critical} Critical"));
    }
    if high > 0 {
        parts.push(format!("{high} High"));
    }
    if medium > 0 {
        parts.push(format!("{medium} Medium"));
    }
    if low > 0 {
        parts.push(format!("{low} Low"));
    }
    parts.join(", ")
}

/// Composes a consolidated plain-text review summary email for a multi-patch
/// series. Returns `None` for single-patch submissions or when zero patches
/// reviewed clean (since every patch with issues already receives its own
/// detailed reply).
pub fn compose_patchset_summary_email(
    outcomes: &[SeriesPatchOutcome],
    separate_replies_sent: bool,
    target_url: &str,
) -> Option<String> {
    if outcomes.len() <= 1 {
        return None;
    }

    let mut clean_indices = Vec::new();
    let mut issue_patches = Vec::new();
    let mut skipped_indices = Vec::new();

    for item in outcomes {
        match &item.kind {
            SeriesPatchOutcomeKind::Clean => clean_indices.push(item.part_index),
            SeriesPatchOutcomeKind::WithIssues { .. } => issue_patches.push(item),
            SeriesPatchOutcomeKind::Skipped => skipped_indices.push(item.part_index),
        }
    }

    if clean_indices.is_empty() {
        return None;
    }

    let total = outcomes.len();
    let mut body = String::new();

    if issue_patches.is_empty() && skipped_indices.is_empty() {
        body.push_str(&wrap(
            &format!("Sashiko has reviewed this {total}-patch series and found no issues."),
            "",
            "",
        ));
    } else {
        body.push_str(&wrap(
            &format!("Sashiko has finished reviewing this {total}-patch series:"),
            "",
            "",
        ));
        body.push('\n');

        body.push_str(&wrap(
            &format!(
                "{}: no issues found.",
                patch_label_ranges(&clean_indices, true)
            ),
            "- ",
            "  ",
        ));

        let mut issue_indices = Vec::new();
        for patch in &issue_patches {
            issue_indices.push(patch.part_index);
            let SeriesPatchOutcomeKind::WithIssues {
                critical,
                high,
                medium,
                low,
                related_preexisting,
            } = patch.kind
            else {
                continue;
            };

            let new_total = critical + high + medium + low;
            let title = truncate_title(strip_subject_brackets(&patch.subject), 55);
            let label = if title.is_empty() {
                format!("Patch {}", patch.part_index)
            } else {
                format!("Patch {} (\"{}\")", patch.part_index, title)
            };

            let detail = match (new_total > 0, related_preexisting > 0) {
                (true, false) => format!(
                    "{} ({})",
                    count(new_total, "potential issue", "potential issues"),
                    format_severity_breakdown(critical, high, medium, low)
                ),
                (false, true) => format!(
                    "{} noted",
                    count(
                        related_preexisting,
                        "related pre-existing bug",
                        "related pre-existing bugs"
                    )
                ),
                (true, true) => format!(
                    "{} ({}) and {}",
                    count(new_total, "potential issue", "potential issues"),
                    format_severity_breakdown(critical, high, medium, low),
                    count(
                        related_preexisting,
                        "related pre-existing bug",
                        "related pre-existing bugs"
                    )
                ),
                (false, false) => "issues noted".to_string(),
            };

            body.push_str(&wrap(&format!("{label}: {detail}"), "- ", "  "));
        }

        if !skipped_indices.is_empty() {
            body.push_str(&wrap(
                &format!(
                    "{}: skipped (not reviewed).",
                    patch_label_ranges(&skipped_indices, true)
                ),
                "- ",
                "  ",
            ));
        }

        if separate_replies_sent && !issue_indices.is_empty() {
            body.push('\n');
            let note = if issue_indices.len() == 1 {
                format!(
                    "A detailed review has been sent as a separate reply to {}.",
                    patch_label_ranges(&issue_indices, false)
                )
            } else {
                format!(
                    "Detailed reviews have been sent as separate replies to {}.",
                    patch_label_ranges(&issue_indices, false)
                )
            };
            body.push_str(&wrap(&note, "", ""));
        }
    }

    body.push_str(&format!("\n-- \nSashiko AI review · {target_url}\n"));
    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wrap_fits_and_hangs() {
        let text = bullet(
            "High",
            "net: foo: out_unlock frees priv->buf twice on the foo_hw_init() error path",
        );
        assert_eq!(
            text,
            "- [High] net: foo: out_unlock frees priv->buf twice on the foo_hw_init()\n  error path\n"
        );
        let long_word = "x".repeat(80);
        assert_eq!(
            wrap(&format!("a {long_word} b"), "", ""),
            format!("a\n{long_word}\nb\n")
        );
    }

    #[test]
    fn test_findings_header_counts_and_wraps() {
        assert_eq!(
            findings_header(1),
            "Thank you for your contribution! Sashiko AI review found 1 potential\nissue to consider:\n"
        );
        assert!(findings_header(2).contains("2 potential\nissues to consider:"));
    }

    #[test]
    fn test_wrap_prose_keeps_quotes_and_indents() {
        let text = "one two\nthree\n\n> \tif (a_very_long_condition_that_should_never_be_rewrapped_by_us(x, y, z))\n    indented stays\nfour";
        assert_eq!(
            wrap_prose(text),
            "one two three\n\n> \tif (a_very_long_condition_that_should_never_be_rewrapped_by_us(x, y, z))\n    indented stays\nfour"
        );
    }

    #[test]
    fn test_format_patch_ranges() {
        assert_eq!(format_patch_ranges(&[]), "");
        assert_eq!(format_patch_ranges(&[3]), "3");
        assert_eq!(format_patch_ranges(&[1, 2]), "1 and 2");
        assert_eq!(format_patch_ranges(&[7, 9]), "7 and 9");
        assert_eq!(format_patch_ranges(&[1, 2, 3]), "1-3");
        assert_eq!(
            format_patch_ranges(&[1, 2, 3, 4, 5, 6, 8, 10]),
            "1-6, 8, and 10"
        );
        assert_eq!(format_patch_ranges(&[1, 2, 4, 5, 6]), "1, 2, and 4-6");
        assert_eq!(
            patch_label_ranges(&[1, 2, 3, 4, 5, 6, 8, 10], true),
            "Patches 1-6, 8, and 10"
        );
        assert_eq!(patch_label_ranges(&[7], false), "patch 7");
    }

    #[test]
    fn test_compose_patchset_summary_email_variants() {
        let url = "https://sashiko.dev/#/patchset/cover@example.org";

        // Single-patch returns None (handled by single-patch positive reply).
        assert!(
            compose_patchset_summary_email(
                &[SeriesPatchOutcome {
                    part_index: 1,
                    subject: "[PATCH] single".to_string(),
                    kind: SeriesPatchOutcomeKind::Clean,
                }],
                true,
                url
            )
            .is_none()
        );

        // All-issues series returns None (no clean patches to summarize).
        assert!(
            compose_patchset_summary_email(
                &[
                    SeriesPatchOutcome {
                        part_index: 1,
                        subject: "[PATCH 1/2] first".to_string(),
                        kind: SeriesPatchOutcomeKind::WithIssues {
                            critical: 0,
                            high: 1,
                            medium: 0,
                            low: 0,
                            related_preexisting: 0,
                        },
                    },
                    SeriesPatchOutcome {
                        part_index: 2,
                        subject: "[PATCH 2/2] second".to_string(),
                        kind: SeriesPatchOutcomeKind::WithIssues {
                            critical: 0,
                            high: 0,
                            medium: 1,
                            low: 0,
                            related_preexisting: 0,
                        },
                    },
                ],
                true,
                url
            )
            .is_none()
        );

        // All-clean multi-patch series.
        let all_clean = compose_patchset_summary_email(
            &(1..=4)
                .map(|i| SeriesPatchOutcome {
                    part_index: i,
                    subject: format!("[PATCH {i}/4] clean {i}"),
                    kind: SeriesPatchOutcomeKind::Clean,
                })
                .collect::<Vec<_>>(),
            true,
            url,
        )
        .unwrap();
        assert_eq!(
            all_clean,
            format!(
                "Sashiko has reviewed this 4-patch series and found no issues.\n\n-- \nSashiko AI review · {url}\n"
            )
        );

        // Mixed 10-patch series: 1-6, 8, 10 clean; 7 and 9 have issues.
        let mut mixed_outcomes = Vec::new();
        for i in 1..=10 {
            let kind = match i {
                7 => SeriesPatchOutcomeKind::WithIssues {
                    critical: 0,
                    high: 1,
                    medium: 1,
                    low: 0,
                    related_preexisting: 0,
                },
                9 => SeriesPatchOutcomeKind::WithIssues {
                    critical: 0,
                    high: 1,
                    medium: 0,
                    low: 0,
                    related_preexisting: 0,
                },
                _ => SeriesPatchOutcomeKind::Clean,
            };
            mixed_outcomes.push(SeriesPatchOutcome {
                part_index: i,
                subject: format!("[PATCH v2 {i}/10] net: foo: patch {i}"),
                kind,
            });
        }
        let mixed = compose_patchset_summary_email(&mixed_outcomes, true, url).unwrap();
        assert!(mixed.contains("- Patches 1-6, 8, and 10: no issues found.\n"));
        assert!(
            mixed.contains(
                "- Patch 7 (\"net: foo: patch 7\"): 2 potential issues (1 High, 1 Medium)"
            )
        );
        assert!(mixed.contains("- Patch 9 (\"net: foo: patch 9\"): 1 potential issue (1 High)"));
        assert!(mixed.contains("separate replies to patches 7 and 9."));
        for line in mixed.lines() {
            if !line.contains("https://") {
                assert!(line.len() <= WRAP_WIDTH, "line exceeded 72 cols: {line:?}");
            }
        }
    }
}
