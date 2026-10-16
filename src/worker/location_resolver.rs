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
}
