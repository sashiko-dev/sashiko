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

use regex::Regex;
use std::collections::HashMap;
use std::path::{Component, Path};
use std::sync::LazyLock;

static CHUNK_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").unwrap());

fn extract_target_file(line: &str) -> Option<String> {
    let rest = line.strip_prefix("+++ ")?.trim();
    if rest == "/dev/null" {
        return None;
    }
    let unquoted = if rest.starts_with('"') && rest.ends_with('"') && rest.len() >= 2 {
        &rest[1..rest.len() - 1]
    } else {
        rest
    };
    let unescaped = unquoted.replace(r#"\""#, "\"").replace(r#"\\"#, "\\");
    let target = if let Some(s) = unescaped.strip_prefix("b/") {
        s
    } else {
        &unescaped
    };
    Some(target.to_string())
}

fn is_diff_pre_image_header(line: &str) -> bool {
    if let Some(rest) = line.strip_prefix("--- ") {
        let trimmed = rest.trim();
        trimmed.starts_with("a/")
            || trimmed.starts_with("b/")
            || trimmed == "/dev/null"
            || trimmed.starts_with("\"a/")
            || trimmed.starts_with("\"b/")
            || rest.contains('\t')
    } else {
        false
    }
}

fn extract_diff_git_target(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git ")?.trim();
    if let Some(pos) = rest.rfind(r#" "b/"#) {
        let b_part = &rest[pos + 4..];
        let unquoted = b_part.trim_end_matches('"');
        return Some(unquoted.replace(r#"\""#, "\"").replace(r#"\\"#, "\\"));
    }
    if let Some(pos) = rest.rfind(" b/") {
        let b_part = &rest[pos + 3..];
        return Some(b_part.trim().to_string());
    }
    None
}

/// Parses a unified diff and returns a map of filename -> list of exact modified (added/replaced)
/// line ranges in the post-image. Line numbers are 0-based indices.
/// Unlike prefetch ranges, unmodified context lines and gap lines between hunks are never included.
pub fn parse_diff_ranges(diff: &str) -> HashMap<String, Vec<(usize, usize)>> {
    let mut files: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    let mut current_file: Option<String> = None;
    let mut current_post_line: usize = 0;
    let mut hunk_start_0: usize = 0;
    let mut hunk_count: usize = 0;
    let mut hunk_pre_target: usize = 0;
    let mut hunk_post_target: usize = 0;
    let mut hunk_pre_consumed: usize = 0;
    let mut hunk_post_consumed: usize = 0;
    let mut hunk_has_body = false;
    let mut hunk_added_ranges: Vec<(usize, usize)> = Vec::new();
    let mut in_hunk = false;

    let flush_hunk = |file: &Option<String>,
                      in_hunk: bool,
                      hunk_has_body: bool,
                      hunk_start_0: usize,
                      hunk_count: usize,
                      hunk_added_ranges: &mut Vec<(usize, usize)>,
                      files: &mut HashMap<String, Vec<(usize, usize)>>| {
        if !in_hunk {
            return;
        }
        if let Some(fname) = file {
            let ranges = files.entry(fname.clone()).or_default();
            if !hunk_has_body && hunk_count > 0 {
                // Header-only diff without body lines: fall back to header span
                ranges.push((
                    hunk_start_0,
                    hunk_start_0.saturating_add(hunk_count.saturating_sub(1)),
                ));
            } else {
                ranges.append(hunk_added_ranges);
            }
        }
        hunk_added_ranges.clear();
    };

    for line in diff.lines() {
        if in_hunk {
            if line.starts_with("@@") {
                flush_hunk(
                    &current_file,
                    in_hunk,
                    hunk_has_body,
                    hunk_start_0,
                    hunk_count,
                    &mut hunk_added_ranges,
                    &mut files,
                );
                if let Some(caps) = CHUNK_HEADER_RE.captures(line) {
                    let pre_count: usize = caps
                        .get(2)
                        .and_then(|m| m.as_str().parse().ok())
                        .unwrap_or(1);
                    let start: usize = caps
                        .get(3)
                        .and_then(|m| m.as_str().parse().ok())
                        .unwrap_or(1);
                    let count: usize = caps
                        .get(4)
                        .and_then(|m| m.as_str().parse().ok())
                        .unwrap_or(1);
                    current_post_line = start;
                    hunk_start_0 = start.saturating_sub(1);
                    hunk_count = count;
                    hunk_pre_target = pre_count;
                    hunk_post_target = count;
                    hunk_pre_consumed = 0;
                    hunk_post_consumed = 0;
                    in_hunk = true;
                    hunk_has_body = false;
                } else {
                    in_hunk = false;
                }
            } else if line.starts_with("diff --git ") {
                flush_hunk(
                    &current_file,
                    in_hunk,
                    hunk_has_body,
                    hunk_start_0,
                    hunk_count,
                    &mut hunk_added_ranges,
                    &mut files,
                );
                current_file = extract_diff_git_target(line);
                in_hunk = false;
                hunk_has_body = false;
            } else if hunk_pre_consumed >= hunk_pre_target && is_diff_pre_image_header(line) {
                flush_hunk(
                    &current_file,
                    in_hunk,
                    hunk_has_body,
                    hunk_start_0,
                    hunk_count,
                    &mut hunk_added_ranges,
                    &mut files,
                );
                in_hunk = false;
                hunk_has_body = false;
            } else if line.starts_with('+') {
                hunk_has_body = true;
                hunk_post_consumed = hunk_post_consumed.saturating_add(1);
                let line_0 = current_post_line.saturating_sub(1);
                if let Some(last) = hunk_added_ranges.last_mut()
                    && last.1.saturating_add(1) == line_0
                {
                    last.1 = line_0;
                } else {
                    hunk_added_ranges.push((line_0, line_0));
                }
                current_post_line = current_post_line.saturating_add(1);
                if hunk_post_consumed >= hunk_post_target && hunk_pre_consumed >= hunk_pre_target {
                    flush_hunk(
                        &current_file,
                        in_hunk,
                        hunk_has_body,
                        hunk_start_0,
                        hunk_count,
                        &mut hunk_added_ranges,
                        &mut files,
                    );
                    in_hunk = false;
                }
            } else if line.starts_with(' ') || line.is_empty() {
                hunk_has_body = true;
                hunk_pre_consumed = hunk_pre_consumed.saturating_add(1);
                hunk_post_consumed = hunk_post_consumed.saturating_add(1);
                current_post_line = current_post_line.saturating_add(1);
                if hunk_post_consumed >= hunk_post_target && hunk_pre_consumed >= hunk_pre_target {
                    flush_hunk(
                        &current_file,
                        in_hunk,
                        hunk_has_body,
                        hunk_start_0,
                        hunk_count,
                        &mut hunk_added_ranges,
                        &mut files,
                    );
                    in_hunk = false;
                }
            } else if line.starts_with('-') {
                hunk_has_body = true;
                hunk_pre_consumed = hunk_pre_consumed.saturating_add(1);
                if hunk_post_consumed >= hunk_post_target && hunk_pre_consumed >= hunk_pre_target {
                    flush_hunk(
                        &current_file,
                        in_hunk,
                        hunk_has_body,
                        hunk_start_0,
                        hunk_count,
                        &mut hunk_added_ranges,
                        &mut files,
                    );
                    in_hunk = false;
                }
            } else if line.starts_with('\\') {
                // \ No newline at end of file: ignore
            } else {
                // Non-hunk line (headers, signatures, commit logs): flush and exit hunk
                flush_hunk(
                    &current_file,
                    in_hunk,
                    hunk_has_body,
                    hunk_start_0,
                    hunk_count,
                    &mut hunk_added_ranges,
                    &mut files,
                );
                in_hunk = false;
                if line.starts_with("+++ ") {
                    current_file = extract_target_file(line);
                }
            }
        } else {
            // Outside of hunks: parse headers
            if line.starts_with("diff --git ") {
                if let Some(target) = extract_diff_git_target(line) {
                    current_file = Some(target);
                }
            } else if line.starts_with("+++ ") {
                current_file = extract_target_file(line);
            } else if line.starts_with("@@")
                && let Some(caps) = CHUNK_HEADER_RE.captures(line)
            {
                let pre_count: usize = caps
                    .get(2)
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(1);
                let start: usize = caps
                    .get(3)
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(1);
                let count: usize = caps
                    .get(4)
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(1);
                current_post_line = start;
                hunk_start_0 = start.saturating_sub(1);
                hunk_count = count;
                hunk_pre_target = pre_count;
                hunk_post_target = count;
                hunk_pre_consumed = 0;
                hunk_post_consumed = 0;
                in_hunk = true;
                hunk_has_body = false;
            }
        }
    }

    flush_hunk(
        &current_file,
        in_hunk,
        hunk_has_body,
        hunk_start_0,
        hunk_count,
        &mut hunk_added_ranges,
        &mut files,
    );

    files
}

/// Normalizes a relative or repository path by stripping leading `./` and optional `b/` or `a/` prefixes.
pub fn normalize_file_path(path: &str) -> Option<String> {
    normalize_file_path_with_worktree(None, path)
}

/// Normalizes a path, checking against an optional worktree directory before stripping `b/` or `a/`.
/// If `b/foo.c` exists in the worktree, it is preserved rather than stripped to `foo.c`.
pub fn normalize_file_path_with_worktree(worktree: Option<&Path>, path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Strip leading ./
    let cleaned = trimmed.strip_prefix("./").unwrap_or(trimmed);
    let p = Path::new(cleaned);

    // Reject absolute paths or paths with parent component traversal
    for comp in p.components() {
        match comp {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
            _ => {}
        }
    }

    // Check if path exists as-is in worktree before stripping b/ or a/
    if let Some(wt) = worktree
        && wt.join(cleaned).exists()
    {
        return Some(cleaned.to_string());
    }

    if let Some(rest) = cleaned.strip_prefix("b/") {
        Some(rest.to_string())
    } else if let Some(rest) = cleaned.strip_prefix("a/") {
        Some(rest.to_string())
    } else {
        Some(cleaned.to_string())
    }
}

/// Locates a snippet in a list of file lines.
/// Returns 0-based `(start_idx, end_idx)` if a match is found.
pub fn find_snippet_in_lines(
    file_lines: &[&str],
    snippet: &str,
    hint_line: Option<usize>,
    file_diff_ranges: Option<&Vec<(usize, usize)>>,
    symbol_line: Option<usize>,
) -> Option<(usize, usize)> {
    let raw_snippet_lines: Vec<&str> = snippet.lines().collect();
    let start_trim = raw_snippet_lines
        .iter()
        .position(|l| !l.trim().is_empty())
        .unwrap_or(raw_snippet_lines.len());
    let end_trim = raw_snippet_lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map(|p| p + 1)
        .unwrap_or(0);

    if start_trim >= end_trim || file_lines.is_empty() {
        return None;
    }

    let snippet_lines: Vec<&str> = raw_snippet_lines[start_trim..end_trim]
        .iter()
        .map(|l| l.trim())
        .collect();

    // 1. Multi-line trimmed match with blank-line tolerance
    let first_snip = snippet_lines
        .iter()
        .find(|l| !l.is_empty())
        .copied()
        .unwrap_or("");
    if !first_snip.is_empty() {
        let mut candidates = Vec::new();
        for (i, line) in file_lines.iter().enumerate() {
            if line.trim() == first_snip
                && let Some(end_idx) = match_snippet_subsequence(file_lines, &snippet_lines, i)
            {
                candidates.push((i, end_idx));
            }
        }
        if let Some(best) =
            pick_closest_candidate(candidates, hint_line, file_diff_ranges, symbol_line)
        {
            return Some(best);
        }
    }

    // 2. Fall back to matching the longest non-trivial trimmed line
    let mut longest_line = "";
    let mut longest_chars = 0;
    let mut longest_idx = 0;
    for (idx, line) in snippet_lines.iter().enumerate() {
        let count = line.chars().count();
        if count > longest_chars {
            longest_line = line;
            longest_chars = count;
            longest_idx = idx;
        }
    }

    if longest_chars >= 4 {
        let mut candidates = Vec::new();
        for (i, line) in file_lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed == longest_line || (longest_chars >= 8 && trimmed.contains(longest_line)) {
                candidates.push(i);
            }
        }

        if let Some(best_i) =
            pick_closest_line(candidates, hint_line, file_diff_ranges, symbol_line)
        {
            let mut start_idx = best_i;
            let mut end_idx = best_i;

            // Expand backwards against snippet lines, allowing intervening blank lines
            let mut curr_file_i = best_i;
            for &raw in snippet_lines[..longest_idx].iter().rev() {
                if raw.is_empty() {
                    continue;
                }
                let mut found = false;
                while curr_file_i > 0 {
                    curr_file_i -= 1;
                    let f_trim = file_lines[curr_file_i].trim();
                    if f_trim.is_empty() {
                        continue;
                    }
                    if f_trim == raw {
                        start_idx = curr_file_i;
                        found = true;
                        break;
                    } else {
                        break;
                    }
                }
                if !found {
                    break;
                }
            }

            // Expand forwards against snippet lines, allowing intervening blank lines
            let mut curr_file_i = best_i;
            for &raw in snippet_lines[longest_idx + 1..].iter() {
                if raw.is_empty() {
                    continue;
                }
                let mut found = false;
                while curr_file_i + 1 < file_lines.len() {
                    curr_file_i += 1;
                    let f_trim = file_lines[curr_file_i].trim();
                    if f_trim.is_empty() {
                        continue;
                    }
                    if f_trim == raw {
                        end_idx = curr_file_i;
                        found = true;
                        break;
                    } else {
                        break;
                    }
                }
                if !found {
                    break;
                }
            }

            return Some((start_idx, end_idx));
        }
    }

    None
}

fn match_snippet_subsequence(
    file_lines: &[&str],
    snippet_lines: &[&str],
    start_file_idx: usize,
) -> Option<usize> {
    let mut snip_idx = 0;
    let mut file_idx = start_file_idx;
    let mut last_matched_file_idx = start_file_idx;

    while snip_idx < snippet_lines.len() && file_idx < file_lines.len() {
        let s_trim = snippet_lines[snip_idx];
        if s_trim.is_empty() {
            snip_idx += 1;
            continue;
        }

        let f_trim = file_lines[file_idx].trim();
        if f_trim.is_empty() {
            file_idx += 1;
            continue;
        }

        if f_trim == s_trim {
            last_matched_file_idx = file_idx;
            snip_idx += 1;
            file_idx += 1;
        } else {
            return None;
        }
    }

    while snip_idx < snippet_lines.len() && snippet_lines[snip_idx].is_empty() {
        snip_idx += 1;
    }

    if snip_idx == snippet_lines.len() {
        Some(last_matched_file_idx)
    } else {
        None
    }
}

fn pick_closest_candidate(
    candidates: Vec<(usize, usize)>,
    hint_line: Option<usize>,
    file_diff_ranges: Option<&Vec<(usize, usize)>>,
    symbol_line: Option<usize>,
) -> Option<(usize, usize)> {
    if candidates.is_empty() {
        return None;
    }
    if let Some(hint) = hint_line {
        return candidates
            .into_iter()
            .min_by_key(|&(start, _)| (start + 1).abs_diff(hint));
    }

    // When hint_line is None:
    // 1. Prioritize candidates that overlap modified diff hunks.
    if let Some(diff) = file_diff_ranges {
        let in_diff: Vec<(usize, usize)> = candidates
            .iter()
            .copied()
            .filter(|&(s, e)| diff.iter().any(|&(ds, de)| s <= de && e >= ds))
            .collect();
        if in_diff.len() == 1 {
            return Some(in_diff[0]);
        }
        if in_diff.len() > 1 {
            if let Some(sym_line) = symbol_line {
                return in_diff
                    .into_iter()
                    .min_by_key(|&(s, _)| (s + 1).abs_diff(sym_line));
            }
            return Some(in_diff[0]);
        }
    }

    // 2. If no candidate overlaps diff, prioritize candidate nearest symbol_line if provided.
    if let Some(sym_line) = symbol_line {
        return candidates
            .into_iter()
            .min_by_key(|&(s, _)| (s + 1).abs_diff(sym_line));
    }

    candidates.into_iter().next()
}

fn pick_closest_line(
    candidates: Vec<usize>,
    hint_line: Option<usize>,
    file_diff_ranges: Option<&Vec<(usize, usize)>>,
    symbol_line: Option<usize>,
) -> Option<usize> {
    if candidates.is_empty() {
        return None;
    }
    if let Some(hint) = hint_line {
        return candidates
            .into_iter()
            .min_by_key(|&line| (line + 1).abs_diff(hint));
    }

    // 1. Prioritize candidates that overlap modified diff hunks.
    if let Some(diff) = file_diff_ranges {
        let in_diff: Vec<usize> = candidates
            .iter()
            .copied()
            .filter(|&l| diff.iter().any(|&(ds, de)| l >= ds && l <= de))
            .collect();
        if in_diff.len() == 1 {
            return Some(in_diff[0]);
        }
        if in_diff.len() > 1 {
            if let Some(sym_line) = symbol_line {
                return in_diff
                    .into_iter()
                    .min_by_key(|&l| (l + 1).abs_diff(sym_line));
            }
            return Some(in_diff[0]);
        }
    }

    // 2. Prioritize candidate nearest symbol_line if provided.
    if let Some(sym_line) = symbol_line {
        return candidates
            .into_iter()
            .min_by_key(|&l| (l + 1).abs_diff(sym_line));
    }

    candidates.into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_file_path() {
        assert_eq!(
            normalize_file_path("b/drivers/net/foo.c"),
            Some("drivers/net/foo.c".to_string())
        );
        assert_eq!(
            normalize_file_path("a/drivers/net/foo.c"),
            Some("drivers/net/foo.c".to_string())
        );
        assert_eq!(
            normalize_file_path("drivers/net/foo.c"),
            Some("drivers/net/foo.c".to_string())
        );
        assert_eq!(
            normalize_file_path("./drivers/net/foo.c"),
            Some("drivers/net/foo.c".to_string())
        );
        assert_eq!(normalize_file_path(""), None);
        assert_eq!(normalize_file_path("../foo.c"), None);
        assert_eq!(normalize_file_path("/etc/passwd"), None);
    }

    #[test]
    fn test_normalize_file_path_preserves_legitimate_root_dir() {
        let temp = tempfile::tempdir().unwrap();
        let b_dir = temp.path().join("b");
        std::fs::create_dir_all(&b_dir).unwrap();
        std::fs::write(b_dir.join("test.c"), "int x;\n").unwrap();

        let norm = normalize_file_path_with_worktree(Some(temp.path()), "b/test.c");
        assert_eq!(norm, Some("b/test.c".to_string()));
    }

    #[test]
    fn test_parse_diff_ranges_exact_and_no_gap_inflation() {
        let diff = r#"diff --git a/foo.c b/foo.c
--- a/foo.c
+++ b/foo.c
@@ -10,15 +10,15 @@ void func(void) {
     context_line_10();
+    added_line_11();
+    added_line_12();
     context_line_13();
     context_line_14();
+    added_line_15();
     context_line_16();
"#;
        let ranges = parse_diff_ranges(diff);
        let foo_ranges = ranges.get("foo.c").expect("foo.c ranges");
        // 0-based lines: 11 is 10, 12 is 11, 15 is 14
        assert_eq!(foo_ranges, &[(10, 11), (14, 14)]);
    }

    #[test]
    fn test_added_lines_starting_with_plus_plus_do_not_terminate_hunk() {
        let diff = r#"diff --git a/test.c b/test.c
--- a/test.c
+++ b/test.c
@@ -1,5 +1,6 @@
 int a = 1;
+++ i;
 int b = 2;
"#;
        let ranges = parse_diff_ranges(diff);
        let test_ranges = ranges.get("test.c").expect("test.c ranges");
        assert_eq!(test_ranges, &[(1, 1)]);
    }

    #[test]
    fn test_whitespace_stripped_empty_context_lines_do_not_drift() {
        // Line 2 is completely empty ("") with no leading space
        let diff = "diff --git a/test.c b/test.c\n--- a/test.c\n+++ b/test.c\n@@ -1,3 +1,4 @@\n context 1\n\n+added 3\n context 4\n";
        let ranges = parse_diff_ranges(diff);
        let test_ranges = ranges.get("test.c").unwrap();
        // Line 1 is idx 0, empty line 2 is idx 1, added line 3 is idx 2
        assert_eq!(test_ranges, &vec![(2, 2)]);
    }

    #[test]
    fn test_quoted_diff_header_file_paths() {
        let diff = r#"diff --git "a/path with spaces/foo.c" "b/path with spaces/foo.c"
--- "a/path with spaces/foo.c"
+++ "b/path with spaces/foo.c"
@@ -1,3 +1,4 @@
 context();
+added();
"#;
        let ranges = parse_diff_ranges(diff);
        assert!(ranges.contains_key("path with spaces/foo.c"));
        assert_eq!(ranges.get("path with spaces/foo.c").unwrap(), &[(1, 1)]);
    }

    #[test]
    fn test_multi_file_diff_without_diff_git_and_trailing_signatures() {
        let diff = r#"--- a/file1.c
+++ b/file1.c
@@ -1,2 +1,3 @@
 context1
+added1
 context2
--- a/file2.c
+++ b/file2.c
@@ -1,2 +1,3 @@
 context3
+added2
 context4
-- 
2.43.0
"#;
        let ranges = parse_diff_ranges(diff);
        let file1_ranges = ranges.get("file1.c").expect("file1.c should exist");
        assert_eq!(file1_ranges, &vec![(1, 1)]);

        let file2_ranges = ranges.get("file2.c").expect("file2.c should exist");
        assert_eq!(file2_ranges, &vec![(1, 1)]);
    }

    #[test]
    fn test_deleted_line_starting_with_triple_dash_does_not_abort_hunk() {
        let diff = r#"diff --git a/query.sql b/query.sql
--- a/query.sql
+++ b/query.sql
@@ -1,4 +1,4 @@
 SELECT 1;
--- old sql comment
+SELECT 2;
 SELECT 3;
"#;
        let ranges = parse_diff_ranges(diff);
        let sql_ranges = ranges.get("query.sql").expect("query.sql should exist");
        // Line 1: SELECT 1; (post line 1, idx 0)
        // Line 2: deleted `-- old sql comment` (pre-image only)
        // Line 3: added `SELECT 2;` (post line 2, idx 1)
        // Line 4: SELECT 3; (post line 3, idx 2)
        assert_eq!(sql_ranges, &vec![(1, 1)]);
    }

    #[test]
    fn test_parse_diff_ranges_saturating_add_on_extreme_values() {
        let diff = format!(
            "diff --git a/extreme.c b/extreme.c\n--- a/extreme.c\n+++ b/extreme.c\n@@ -1,1 +{},{} @@\n+added\n",
            usize::MAX,
            usize::MAX
        );
        let ranges = parse_diff_ranges(&diff);
        assert!(ranges.contains_key("extreme.c"));
    }

    #[test]
    fn test_find_snippet_multi_line_trimmed() {
        let code = [
            "int calculate(int x) {",
            "    if (x < 0) {",
            "        return -EINVAL;",
            "    }",
            "    return x * 2;",
            "}",
        ];
        let snippet = "if (x < 0) {\n    return -EINVAL;\n}";
        let res = find_snippet_in_lines(&code, snippet, None, None, None);
        assert_eq!(res, Some((1, 3)));
    }

    #[test]
    fn test_find_snippet_with_hint_disambiguation() {
        let code = [
            "int a() {",
            "    return 0;",
            "}",
            "int b() {",
            "    return 0;",
            "}",
        ];
        let snippet = "return 0;";
        // Hint for line 5 should pick the second occurrence (0-based index 4)
        let res = find_snippet_in_lines(&code, snippet, Some(5), None, None);
        assert_eq!(res, Some((4, 4)));

        // Hint for line 2 should pick the first occurrence (0-based index 1)
        let res1 = find_snippet_in_lines(&code, snippet, Some(2), None, None);
        assert_eq!(res1, Some((1, 1)));
    }

    #[test]
    fn test_find_snippet_fallback_longest_line() {
        let code = [
            "static int do_read(struct file *f) {",
            "    int ret = 0;",
            "    if (!f) return -EINVAL;",
            "    ret = vfs_read(f);",
            "    return ret;",
            "}",
        ];
        // Snippet where comments or formatting slightly diverge, but the core line is distinct
        let snippet = "    /* reading */\n    ret = vfs_read(f);\n    /* done */";
        let res = find_snippet_in_lines(&code, snippet, None, None, None);
        assert_eq!(res, Some((3, 3)));
    }

    #[test]
    fn test_snippet_with_blank_lines_matches_file_with_blank_lines() {
        let code = [
            "int handle_request(void) {",
            "    int ret = 0;",
            "",
            "    ret = do_work();",
            "",
            "    return ret;",
            "}",
        ];
        let snippet = "    ret = do_work();\n\n    return ret;";
        let res = find_snippet_in_lines(&code, snippet, None, None, None);
        assert_eq!(res, Some((3, 5)));
    }

    #[test]
    fn test_snippet_without_blank_lines_matches_file_with_blank_lines() {
        let code = [
            "int handle_request(void) {",
            "    int ret = 0;",
            "",
            "    ret = do_work();",
            "",
            "    return ret;",
            "}",
        ];
        // LLM omitted internal blank line in snippet
        let snippet = "    ret = do_work();\n    return ret;";
        let res = find_snippet_in_lines(&code, snippet, None, None, None);
        assert_eq!(res, Some((3, 5)));
    }

    #[test]
    fn test_repeated_snippet_without_hint_prefers_diff_hunk_occurrence() {
        let code = [
            "int old_func(void) {",
            "    return -ENOMEM;",
            "}",
            "",
            "// ... other code ...",
            "",
            "int new_func(void) {",
            "    return -ENOMEM;",
            "}",
        ];
        let snippet = "return -ENOMEM;";
        // Diff modified lines 7-9 (0-based 6..=8)
        let diff_ranges = vec![(6, 8)];

        // With no line hint and no diff ranges, it would pick line 2 (0-based 1).
        let unguided = find_snippet_in_lines(&code, snippet, None, None, None);
        assert_eq!(unguided, Some((1, 1)));

        // With diff_ranges, it must pick line 8 (0-based 7) inside the diff!
        let guided = find_snippet_in_lines(&code, snippet, None, Some(&diff_ranges), None);
        assert_eq!(guided, Some((7, 7)));
    }

    #[test]
    fn test_repeated_snippet_without_hint_prefers_symbol_occurrence() {
        let code = [
            "int first_func(void) {",
            "    return -EINVAL;",
            "}",
            "",
            "int target_func(void) {",
            "    return -EINVAL;",
            "}",
        ];
        let snippet = "return -EINVAL;";
        // Symbol definition line 5
        let symbol_line = Some(5);

        let guided = find_snippet_in_lines(&code, snippet, None, None, symbol_line);
        assert_eq!(guided, Some((5, 5)));
    }
}
