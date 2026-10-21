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
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Component, Path};
use std::sync::LazyLock;

static CHUNK_HEADER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").unwrap());

fn parse_c_quoted_path(s: &str) -> Option<(&str, &str)> {
    if !s.starts_with('"') {
        return None;
    }
    let bytes = s.as_bytes();
    let mut i = 1;
    let mut escaped = false;
    while i < bytes.len() {
        if escaped {
            escaped = false;
        } else if bytes[i] == b'\\' {
            escaped = true;
        } else if bytes[i] == b'"' {
            let inside = &s[1..i];
            let remainder = &s[i + 1..];
            return Some((inside, remainder));
        }
        i += 1;
    }
    None
}

fn unescape_c_path(s: &str) -> String {
    let unescaped = s.replace(r#"\""#, "\"").replace(r#"\\"#, "\\");
    if let Some(stripped) = unescaped.strip_prefix("a/") {
        stripped.to_string()
    } else if let Some(stripped) = unescaped.strip_prefix("b/") {
        stripped.to_string()
    } else {
        unescaped
    }
}

fn parse_diff_git_paths(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("diff --git ")?.trim();

    if rest.starts_with('"') {
        let (src_raw, remainder) = parse_c_quoted_path(rest)?;
        let src = unescape_c_path(src_raw);
        let rem = remainder.trim();
        let dst = if rem.starts_with('"') {
            let (dst_raw, _) = parse_c_quoted_path(rem)?;
            unescape_c_path(dst_raw)
        } else {
            let stripped = rem
                .strip_prefix("b/")
                .or_else(|| rem.strip_prefix("a/"))
                .unwrap_or(rem);
            stripped.to_string()
        };
        return Some((src, dst));
    }

    if let Some(pos) = rest.find(r#" "b/"#).or_else(|| rest.find(r#" ""#)) {
        let src_raw = &rest[..pos];
        let rem = rest[pos + 1..].trim();
        if let Some((dst_raw, _)) = parse_c_quoted_path(rem) {
            let src = src_raw.strip_prefix("a/").unwrap_or(src_raw).to_string();
            let dst = unescape_c_path(dst_raw);
            return Some((src, dst));
        }
    }

    // Symmetric non-rename check: a/<path> b/<path>
    if rest.starts_with("a/") && rest.len() >= 5 && (rest.len() - 5) % 2 == 0 {
        let k = (rest.len() - 5) / 2;
        if rest.get(2 + k..2 + k + 3) == Some(" b/")
            && let (Some(left), Some(right)) = (rest.get(2..2 + k), rest.get(2 + k + 3..))
            && left == right
        {
            return Some((left.to_string(), left.to_string()));
        }
    }

    let occurrences: Vec<usize> = rest.match_indices(" b/").map(|(i, _)| i).collect();
    if occurrences.len() == 1 {
        let pos = occurrences[0];
        let a_part = &rest[..pos];
        let b_part = &rest[pos + 3..];
        let src = a_part.strip_prefix("a/").unwrap_or(a_part);
        let dst = b_part.strip_prefix("b/").unwrap_or(b_part);
        return Some((src.to_string(), dst.to_string()));
    } else if let Some(pos) = rest.rfind(" b/") {
        let a_part = &rest[..pos];
        let b_part = &rest[pos + 3..];
        let src = a_part.strip_prefix("a/").unwrap_or(a_part);
        let dst = b_part.strip_prefix("b/").unwrap_or(b_part);
        return Some((src.to_string(), dst.to_string()));
    }

    None
}

fn extract_diff_git_source(line: &str) -> Option<String> {
    parse_diff_git_paths(line).map(|(src, _)| src)
}

fn extract_diff_git_target(line: &str) -> Option<String> {
    parse_diff_git_paths(line).map(|(_, dst)| dst)
}

fn extract_target_file(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("++++ ")
        .or_else(|| line.strip_prefix("+++ "))?
        .trim();
    if rest == "/dev/null" {
        return None;
    }
    if rest.starts_with('"')
        && let Some((inside, _)) = parse_c_quoted_path(rest)
    {
        return Some(unescape_c_path(inside));
    }
    let path_part = if let Some(pos) = rest.find('\t') {
        &rest[..pos]
    } else {
        rest
    };
    let unescaped = path_part.replace(r#"\""#, "\"").replace(r#"\\"#, "\\");
    let target = if let Some(s) = unescaped.strip_prefix("b/") {
        s
    } else if let Some(s) = unescaped.strip_prefix("a/") {
        s
    } else {
        &unescaped
    };
    Some(target.to_string())
}

fn extract_source_file(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("---- ")
        .or_else(|| line.strip_prefix("--- "))?
        .trim();
    if rest == "/dev/null" {
        return None;
    }
    if rest.starts_with('"')
        && let Some((inside, _)) = parse_c_quoted_path(rest)
    {
        return Some(unescape_c_path(inside));
    }
    let path_part = if let Some(pos) = rest.find('\t') {
        &rest[..pos]
    } else {
        rest
    };
    let unescaped = path_part.replace(r#"\""#, "\"").replace(r#"\\"#, "\\");
    let target = if let Some(s) = unescaped.strip_prefix("a/") {
        s
    } else if let Some(s) = unescaped.strip_prefix("b/") {
        s
    } else {
        &unescaped
    };
    Some(target.to_string())
}

fn is_diff_pre_image_header(line: &str) -> bool {
    let rest_opt = line
        .strip_prefix("---- ")
        .or_else(|| line.strip_prefix("--- "));
    if let Some(rest) = rest_opt {
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

fn is_diff_post_image_header(line: &str) -> bool {
    let rest_opt = line
        .strip_prefix("++++ ")
        .or_else(|| line.strip_prefix("+++ "));
    if let Some(rest) = rest_opt {
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

/// Two-sided diff parsing results tracking both post-image (added) and pre-image (deleted) ranges and contents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedDiff {
    /// Post-image (revision) added/modified line ranges (0-based start, 0-based end inclusive).
    pub post_ranges: HashMap<String, Vec<(usize, usize)>>,
    /// Pre-image (parent) deleted/modified line ranges (0-based start, 0-based end inclusive).
    pub pre_ranges: HashMap<String, Vec<(usize, usize)>>,
    /// Deleted lines in pre-image: map of filename -> list of (0-based pre line index, line text without leading '-').
    pub deleted_lines: HashMap<String, Vec<(usize, String)>>,
    /// Rename mapping from pre-image source path to post-image target path.
    pub pre_to_post: HashMap<String, String>,
    /// Rename mapping from post-image target path to pre-image source path.
    pub post_to_pre: HashMap<String, String>,
}

/// Parses a unified diff into two-sided line ranges and deleted line content.
pub fn parse_two_sided_diff(diff: &str) -> ParsedDiff {
    let mut post_files: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    let mut pre_files: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    let mut deleted_lines: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    let mut pre_to_post: HashMap<String, String> = HashMap::new();
    let mut post_to_pre: HashMap<String, String> = HashMap::new();

    let mut current_post_file: Option<String> = None;
    let mut current_pre_file: Option<String> = None;

    let mut current_pre_line: usize = 0;
    let mut current_post_line: usize = 0;

    let mut hunk_pre_start_0: usize = 0;
    let mut hunk_post_start_0: usize = 0;
    let mut hunk_pre_count: usize = 0;
    let mut hunk_post_count: usize = 0;

    let mut hunk_pre_target: usize = 0;
    let mut hunk_post_target: usize = 0;
    let mut hunk_pre_consumed: usize = 0;
    let mut hunk_post_consumed: usize = 0;

    let mut hunk_has_body = false;
    let mut hunk_added_ranges: Vec<(usize, usize)> = Vec::new();
    let mut hunk_deleted_ranges: Vec<(usize, usize)> = Vec::new();
    let mut hunk_deleted_lines: Vec<(usize, String)> = Vec::new();
    let mut in_hunk = false;

    let flush_hunk = |post_file: &Option<String>,
                      pre_file: &Option<String>,
                      in_hunk: bool,
                      hunk_has_body: bool,
                      hunk_post_start_0: usize,
                      hunk_post_count: usize,
                      hunk_pre_start_0: usize,
                      hunk_pre_count: usize,
                      hunk_added_ranges: &mut Vec<(usize, usize)>,
                      hunk_deleted_ranges: &mut Vec<(usize, usize)>,
                      hunk_deleted_lines: &mut Vec<(usize, String)>,
                      post_files: &mut HashMap<String, Vec<(usize, usize)>>,
                      pre_files: &mut HashMap<String, Vec<(usize, usize)>>,
                      deleted_lines: &mut HashMap<String, Vec<(usize, String)>>,
                      pre_to_post: &mut HashMap<String, String>,
                      post_to_pre: &mut HashMap<String, String>| {
        if !in_hunk {
            return;
        }
        if let Some(post) = post_file.as_ref() {
            let ranges = post_files.entry(post.clone()).or_default();
            if !hunk_has_body && hunk_post_count > 0 {
                ranges.push((
                    hunk_post_start_0,
                    hunk_post_start_0.saturating_add(hunk_post_count.saturating_sub(1)),
                ));
            } else {
                ranges.extend(hunk_added_ranges.iter().copied());
            }
        }

        if let Some(pre) = pre_file.as_ref() {
            let ranges = pre_files.entry(pre.clone()).or_default();
            if !hunk_has_body && hunk_pre_count > 0 {
                ranges.push((
                    hunk_pre_start_0,
                    hunk_pre_start_0.saturating_add(hunk_pre_count.saturating_sub(1)),
                ));
            } else {
                ranges.extend(hunk_deleted_ranges.iter().copied());
            }
            deleted_lines
                .entry(pre.clone())
                .or_default()
                .extend(hunk_deleted_lines.iter().cloned());
        }

        if let (Some(pre), Some(post)) = (pre_file.as_ref(), post_file.as_ref())
            && pre != post
        {
            pre_to_post.insert(pre.clone(), post.clone());
            post_to_pre.insert(post.clone(), pre.clone());
        }

        hunk_added_ranges.clear();
        hunk_deleted_ranges.clear();
        hunk_deleted_lines.clear();
    };

    for line in diff.lines() {
        if in_hunk {
            if line.starts_with("@@") {
                flush_hunk(
                    &current_post_file,
                    &current_pre_file,
                    in_hunk,
                    hunk_has_body,
                    hunk_post_start_0,
                    hunk_post_count,
                    hunk_pre_start_0,
                    hunk_pre_count,
                    &mut hunk_added_ranges,
                    &mut hunk_deleted_ranges,
                    &mut hunk_deleted_lines,
                    &mut post_files,
                    &mut pre_files,
                    &mut deleted_lines,
                    &mut pre_to_post,
                    &mut post_to_pre,
                );
                if let Some(caps) = CHUNK_HEADER_RE.captures(line) {
                    let pre_start: usize = caps
                        .get(1)
                        .and_then(|m| m.as_str().parse().ok())
                        .unwrap_or(1);
                    let pre_count: usize = caps
                        .get(2)
                        .and_then(|m| m.as_str().parse().ok())
                        .unwrap_or(1);
                    let post_start: usize = caps
                        .get(3)
                        .and_then(|m| m.as_str().parse().ok())
                        .unwrap_or(1);
                    let post_count: usize = caps
                        .get(4)
                        .and_then(|m| m.as_str().parse().ok())
                        .unwrap_or(1);

                    current_pre_line = pre_start;
                    current_post_line = post_start;

                    hunk_pre_start_0 = pre_start.saturating_sub(1);
                    hunk_post_start_0 = post_start.saturating_sub(1);
                    hunk_pre_count = pre_count;
                    hunk_post_count = post_count;

                    hunk_pre_target = pre_count;
                    hunk_post_target = post_count;
                    hunk_pre_consumed = 0;
                    hunk_post_consumed = 0;
                    in_hunk = true;
                    hunk_has_body = false;
                } else {
                    in_hunk = false;
                }
            } else if line.starts_with("diff --git ") {
                flush_hunk(
                    &current_post_file,
                    &current_pre_file,
                    in_hunk,
                    hunk_has_body,
                    hunk_post_start_0,
                    hunk_post_count,
                    hunk_pre_start_0,
                    hunk_pre_count,
                    &mut hunk_added_ranges,
                    &mut hunk_deleted_ranges,
                    &mut hunk_deleted_lines,
                    &mut post_files,
                    &mut pre_files,
                    &mut deleted_lines,
                    &mut pre_to_post,
                    &mut post_to_pre,
                );
                current_post_file = extract_diff_git_target(line);
                current_pre_file = extract_diff_git_source(line);
                if let (Some(pre), Some(post)) =
                    (current_pre_file.as_ref(), current_post_file.as_ref())
                    && pre != post
                {
                    pre_to_post.insert(pre.clone(), post.clone());
                    post_to_pre.insert(post.clone(), pre.clone());
                }
                in_hunk = false;
                hunk_has_body = false;
            } else if hunk_pre_consumed >= hunk_pre_target && is_diff_pre_image_header(line) {
                flush_hunk(
                    &current_post_file,
                    &current_pre_file,
                    in_hunk,
                    hunk_has_body,
                    hunk_post_start_0,
                    hunk_post_count,
                    hunk_pre_start_0,
                    hunk_pre_count,
                    &mut hunk_added_ranges,
                    &mut hunk_deleted_ranges,
                    &mut hunk_deleted_lines,
                    &mut post_files,
                    &mut pre_files,
                    &mut deleted_lines,
                    &mut pre_to_post,
                    &mut post_to_pre,
                );
                current_pre_file = extract_source_file(line);
                current_post_file = None;
                in_hunk = false;
                hunk_has_body = false;
            } else if hunk_post_consumed >= hunk_post_target && is_diff_post_image_header(line) {
                flush_hunk(
                    &current_post_file,
                    &current_pre_file,
                    in_hunk,
                    hunk_has_body,
                    hunk_post_start_0,
                    hunk_post_count,
                    hunk_pre_start_0,
                    hunk_pre_count,
                    &mut hunk_added_ranges,
                    &mut hunk_deleted_ranges,
                    &mut hunk_deleted_lines,
                    &mut post_files,
                    &mut pre_files,
                    &mut deleted_lines,
                    &mut pre_to_post,
                    &mut post_to_pre,
                );
                current_post_file = extract_target_file(line);
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
                        &current_post_file,
                        &current_pre_file,
                        in_hunk,
                        hunk_has_body,
                        hunk_post_start_0,
                        hunk_post_count,
                        hunk_pre_start_0,
                        hunk_pre_count,
                        &mut hunk_added_ranges,
                        &mut hunk_deleted_ranges,
                        &mut hunk_deleted_lines,
                        &mut post_files,
                        &mut pre_files,
                        &mut deleted_lines,
                        &mut pre_to_post,
                        &mut post_to_pre,
                    );
                    in_hunk = false;
                }
            } else if line.starts_with(' ') || line.is_empty() {
                hunk_has_body = true;
                hunk_pre_consumed = hunk_pre_consumed.saturating_add(1);
                hunk_post_consumed = hunk_post_consumed.saturating_add(1);
                current_pre_line = current_pre_line.saturating_add(1);
                current_post_line = current_post_line.saturating_add(1);
                if hunk_post_consumed >= hunk_post_target && hunk_pre_consumed >= hunk_pre_target {
                    flush_hunk(
                        &current_post_file,
                        &current_pre_file,
                        in_hunk,
                        hunk_has_body,
                        hunk_post_start_0,
                        hunk_post_count,
                        hunk_pre_start_0,
                        hunk_pre_count,
                        &mut hunk_added_ranges,
                        &mut hunk_deleted_ranges,
                        &mut hunk_deleted_lines,
                        &mut post_files,
                        &mut pre_files,
                        &mut deleted_lines,
                        &mut pre_to_post,
                        &mut post_to_pre,
                    );
                    in_hunk = false;
                }
            } else if line.starts_with('-') {
                hunk_has_body = true;
                hunk_pre_consumed = hunk_pre_consumed.saturating_add(1);
                let line_0 = current_pre_line.saturating_sub(1);
                if let Some(last) = hunk_deleted_ranges.last_mut()
                    && last.1.saturating_add(1) == line_0
                {
                    last.1 = line_0;
                } else {
                    hunk_deleted_ranges.push((line_0, line_0));
                }
                let text = line.strip_prefix('-').unwrap_or(line).to_string();
                hunk_deleted_lines.push((line_0, text));
                current_pre_line = current_pre_line.saturating_add(1);
                if hunk_post_consumed >= hunk_post_target && hunk_pre_consumed >= hunk_pre_target {
                    flush_hunk(
                        &current_post_file,
                        &current_pre_file,
                        in_hunk,
                        hunk_has_body,
                        hunk_post_start_0,
                        hunk_post_count,
                        hunk_pre_start_0,
                        hunk_pre_count,
                        &mut hunk_added_ranges,
                        &mut hunk_deleted_ranges,
                        &mut hunk_deleted_lines,
                        &mut post_files,
                        &mut pre_files,
                        &mut deleted_lines,
                        &mut pre_to_post,
                        &mut post_to_pre,
                    );
                    in_hunk = false;
                }
            } else if line.starts_with('\\') {
                // Git emits "\ No newline at end of file" when EOF newline changes; ignore without exiting hunk.
            } else {
                // Non-hunk line (headers, signatures, commit logs): flush and exit hunk
                flush_hunk(
                    &current_post_file,
                    &current_pre_file,
                    in_hunk,
                    hunk_has_body,
                    hunk_post_start_0,
                    hunk_post_count,
                    hunk_pre_start_0,
                    hunk_pre_count,
                    &mut hunk_added_ranges,
                    &mut hunk_deleted_ranges,
                    &mut hunk_deleted_lines,
                    &mut post_files,
                    &mut pre_files,
                    &mut deleted_lines,
                    &mut pre_to_post,
                    &mut post_to_pre,
                );
                in_hunk = false;
                hunk_has_body = false;
                if line.starts_with("diff --git ") {
                    current_post_file = extract_diff_git_target(line);
                    current_pre_file = extract_diff_git_source(line);
                    if let (Some(pre), Some(post)) =
                        (current_pre_file.as_ref(), current_post_file.as_ref())
                        && pre != post
                    {
                        pre_to_post.insert(pre.clone(), post.clone());
                        post_to_pre.insert(post.clone(), pre.clone());
                    }
                } else if is_diff_pre_image_header(line) {
                    current_pre_file = extract_source_file(line);
                    current_post_file = None;
                } else if is_diff_post_image_header(line) {
                    current_post_file = extract_target_file(line);
                }
            }
        } else {
            // Outside of hunks: parse headers
            if line.starts_with("diff --git ") {
                current_post_file = extract_diff_git_target(line);
                current_pre_file = extract_diff_git_source(line);
                if let (Some(pre), Some(post)) =
                    (current_pre_file.as_ref(), current_post_file.as_ref())
                    && pre != post
                {
                    pre_to_post.insert(pre.clone(), post.clone());
                    post_to_pre.insert(post.clone(), pre.clone());
                }
            } else if is_diff_pre_image_header(line) {
                current_pre_file = extract_source_file(line);
                current_post_file = None;
            } else if is_diff_post_image_header(line) {
                current_post_file = extract_target_file(line);
            } else if line.starts_with("@@")
                && let Some(caps) = CHUNK_HEADER_RE.captures(line)
            {
                let pre_start: usize = caps
                    .get(1)
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(1);
                let pre_count: usize = caps
                    .get(2)
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(1);
                let post_start: usize = caps
                    .get(3)
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(1);
                let post_count: usize = caps
                    .get(4)
                    .and_then(|m| m.as_str().parse().ok())
                    .unwrap_or(1);

                current_pre_line = pre_start;
                current_post_line = post_start;

                hunk_pre_start_0 = pre_start.saturating_sub(1);
                hunk_post_start_0 = post_start.saturating_sub(1);
                hunk_pre_count = pre_count;
                hunk_post_count = post_count;

                hunk_pre_target = pre_count;
                hunk_post_target = post_count;
                hunk_pre_consumed = 0;
                hunk_post_consumed = 0;
                in_hunk = true;
                hunk_has_body = false;
            }
        }
    }

    flush_hunk(
        &current_post_file,
        &current_pre_file,
        in_hunk,
        hunk_has_body,
        hunk_post_start_0,
        hunk_post_count,
        hunk_pre_start_0,
        hunk_pre_count,
        &mut hunk_added_ranges,
        &mut hunk_deleted_ranges,
        &mut hunk_deleted_lines,
        &mut post_files,
        &mut pre_files,
        &mut deleted_lines,
        &mut pre_to_post,
        &mut post_to_pre,
    );

    ParsedDiff {
        post_ranges: post_files,
        pre_ranges: pre_files,
        deleted_lines,
        pre_to_post,
        post_to_pre,
    }
}

/// Parses a unified diff and returns a map of filename -> list of exact modified (added/replaced)
/// line ranges in the post-image. Line numbers are 0-based indices.
/// Unlike prefetch ranges, unmodified context lines and gap lines between hunks are never included.
pub fn parse_diff_ranges(diff: &str) -> HashMap<String, Vec<(usize, usize)>> {
    parse_two_sided_diff(diff).post_ranges
}

/// Maximum file size (4 MiB) to inspect when resolving finding locations,
/// guarding against unbounded memory allocation on massive generated files or binary blobs.
pub const MAX_RESOLVE_FILE_SIZE: u64 = 4 * 1024 * 1024;

/// Resolved location with verified 1-based line numbers and character offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLocation {
    pub file: String,
    pub line: usize,
    pub end_line: usize,
    pub line_range: String,
    pub start_character: Option<usize>,
    pub end_character: Option<usize>,
    pub in_diff: bool,
    pub side: Option<String>,
    pub code_snippet: Option<String>,
}

/// Normalizes a repository-relative file path by stripping leading slashes and git diff prefixes,
/// and rejecting path traversal or non-normal components.
pub fn normalize_file_path(path: &str) -> Option<String> {
    normalize_file_path_with_worktree(None, path)
}

fn is_safe_git_ref(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('-')
}

fn path_exists_in_git(worktree: &Path, revision: &str, path: &str) -> bool {
    if !is_safe_git_ref(revision) || !is_safe_git_ref(path) || revision == "unknown" {
        return false;
    }
    let mut cmd = crate::git_cmd::in_dir(worktree);
    cmd.args(["cat-file", "-t", "--", &format!("{}:{}", revision, path)]);
    if let Ok(output) = cmd.output()
        && output.status.success()
    {
        output.stdout.starts_with(b"blob")
    } else {
        false
    }
}

/// Normalizes a repository-relative file path against an optional worktree root and revision.
/// Strips diff prefixes (`b/`, `a/`) while preserving legitimate paths that actually exist.
pub fn normalize_file_path_with_worktree_and_rev(
    worktree: Option<&Path>,
    revision: Option<&str>,
    path: &str,
) -> Option<String> {
    let trimmed = path.trim().trim_start_matches('/');
    if trimmed.is_empty() {
        return None;
    }

    let mut parts: Vec<&str> = Vec::new();
    for comp in Path::new(trimmed).components() {
        match comp {
            Component::Normal(c) => {
                let s = c.to_str()?;
                if s.starts_with('-') {
                    return None;
                }
                parts.push(s);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return None;
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    let cleaned = parts.join("/");

    let file_exists = |p_str: &str| -> bool {
        let Some(wt) = worktree else {
            return false;
        };
        if let Some(rev) = revision
            && is_safe_git_ref(rev)
            && rev != "unknown"
            && path_exists_in_git(wt, rev, p_str)
        {
            return true;
        }
        wt.join(p_str).exists()
    };

    // If path starts with b/ or a/, check if it is a legitimate repository directory
    if (cleaned.starts_with("b/") || cleaned.starts_with("a/")) && parts.len() > 1 {
        if file_exists(&cleaned) {
            return Some(cleaned);
        }
        let stripped = parts[1..].join("/");
        if file_exists(&stripped) {
            return Some(stripped);
        }
        return Some(stripped);
    }

    Some(cleaned)
}

/// Normalizes a repository-relative file path against an optional worktree root.
/// If the path exists as-is (e.g. `b/main.c` where `b` is a legitimate directory),
/// it preserves the prefix rather than stripping it.
pub fn normalize_file_path_with_worktree(worktree: Option<&Path>, path: &str) -> Option<String> {
    normalize_file_path_with_worktree_and_rev(worktree, None, path)
}

fn read_file_content(
    worktree_path: &Path,
    revision: Option<&str>,
    norm_file: &str,
) -> Option<String> {
    if !is_safe_git_ref(norm_file) {
        return None;
    }

    // 1. Try reading the git blob at the target revision (e.g. virtual_head or commit SHA)
    if let Some(rev) = revision
        && is_safe_git_ref(rev)
        && rev != "unknown"
    {
        // 1a. Ensure object is a blob (not a tree or directory)
        let mut type_cmd = crate::git_cmd::in_dir(worktree_path);
        type_cmd.args(["cat-file", "-t", "--", &format!("{}:{}", rev, norm_file)]);
        if let Ok(type_out) = type_cmd.output()
            && type_out.status.success()
            && type_out.stdout.starts_with(b"blob")
        {
            // 1b. Check size before buffering
            let mut size_cmd = crate::git_cmd::in_dir(worktree_path);
            size_cmd.args(["cat-file", "-s", "--", &format!("{}:{}", rev, norm_file)]);
            if let Ok(size_out) = size_cmd.output()
                && size_out.status.success()
            {
                let size_str = String::from_utf8_lossy(&size_out.stdout);
                if let Ok(size) = size_str.trim().parse::<u64>()
                    && size > MAX_RESOLVE_FILE_SIZE
                {
                    return None;
                }
            }

            // 1c. Buffer verified blob
            let mut blob_cmd = crate::git_cmd::in_dir(worktree_path);
            blob_cmd.args(["cat-file", "blob", "--", &format!("{}:{}", rev, norm_file)]);
            if let Ok(output) = blob_cmd.output()
                && output.status.success()
            {
                if (output.stdout.len() as u64) > MAX_RESOLVE_FILE_SIZE {
                    return None;
                }
                if let Ok(content) = String::from_utf8(output.stdout) {
                    return Some(content);
                }
            }
        }
    }

    // 2. Fall back to reading from the physical worktree disk ONLY when no explicit revision was requested
    if revision.is_some() {
        return None;
    }

    let file_path = worktree_path.join(norm_file);
    let Ok(metadata) = file_path.metadata() else {
        return None;
    };
    if !metadata.is_file() || metadata.len() > MAX_RESOLVE_FILE_SIZE {
        return None;
    }
    // Defense-in-depth: ensure canonical path does not escape worktree
    if let (Ok(canon_worktree), Ok(canon_file)) =
        (worktree_path.canonicalize(), file_path.canonicalize())
    {
        if !canon_file.starts_with(&canon_worktree) {
            return None;
        }
    } else {
        return None;
    }

    std::fs::read_to_string(&file_path).ok()
}

fn parse_snippet_lines<'a>(snippet: &'a str) -> Option<Vec<&'a str>> {
    let raw_snippet_lines: Vec<&'a str> = snippet.lines().collect();
    let start_trim = raw_snippet_lines
        .iter()
        .position(|l| !l.trim().is_empty())
        .unwrap_or(raw_snippet_lines.len());
    let end_trim = raw_snippet_lines
        .iter()
        .rposition(|l| !l.trim().is_empty())
        .map(|p| p + 1)
        .unwrap_or(0);

    if start_trim >= end_trim {
        return None;
    }

    Some(
        raw_snippet_lines[start_trim..end_trim]
            .iter()
            .map(|l| l.trim())
            .collect(),
    )
}

/// Locates a full multi-line snippet in a list of file lines as an exact subsequence.
pub fn find_snippet_in_lines_exact(
    file_lines: &[&str],
    snippet: &str,
    hint_line: Option<usize>,
    file_diff_ranges: Option<&Vec<(usize, usize)>>,
    symbol_line: Option<usize>,
) -> Option<(usize, usize)> {
    if file_lines.is_empty() {
        return None;
    }
    let snippet_lines = parse_snippet_lines(snippet)?;
    let first_snip = snippet_lines
        .iter()
        .find(|l| !l.is_empty())
        .copied()
        .unwrap_or("");
    if first_snip.is_empty() {
        return None;
    }

    let mut candidates = Vec::new();
    for (i, line) in file_lines.iter().enumerate() {
        if line.trim() == first_snip
            && let Some(end_idx) = match_snippet_subsequence(file_lines, &snippet_lines, i)
        {
            candidates.push((i, end_idx));
        }
    }
    pick_closest_candidate(candidates, hint_line, file_diff_ranges, symbol_line)
}

/// Locates a snippet in a list of file lines by falling back to matching candidate
/// lines in descending order of length (for lines >= 4 chars) and expanding outwards.
pub fn find_snippet_in_lines_fallback(
    file_lines: &[&str],
    snippet: &str,
    hint_line: Option<usize>,
    file_diff_ranges: Option<&Vec<(usize, usize)>>,
    symbol_line: Option<usize>,
) -> Option<(usize, usize)> {
    if file_lines.is_empty() {
        return None;
    }
    let snippet_lines = parse_snippet_lines(snippet)?;

    let mut candidate_lines: Vec<(usize, &str)> = snippet_lines
        .iter()
        .enumerate()
        .map(|(idx, &l)| (idx, l))
        .filter(|(_, l)| l.chars().count() >= 4)
        .collect();
    candidate_lines.sort_by_key(|(_, l)| std::cmp::Reverse(l.chars().count()));

    for (cand_idx, cand_line) in candidate_lines {
        let cand_chars = cand_line.chars().count();
        let mut candidates = Vec::new();
        for (i, line) in file_lines.iter().enumerate() {
            let trimmed = line.trim();
            if trimmed == cand_line || (cand_chars >= 8 && trimmed.contains(cand_line)) {
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
            for &raw in snippet_lines[..cand_idx].iter().rev() {
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
            for &raw in snippet_lines[cand_idx + 1..].iter() {
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

/// Locates a snippet in a list of file lines.
/// Returns 0-based `(start_idx, end_idx)` if a match is found.
pub fn find_snippet_in_lines(
    file_lines: &[&str],
    snippet: &str,
    hint_line: Option<usize>,
    file_diff_ranges: Option<&Vec<(usize, usize)>>,
    symbol_line: Option<usize>,
) -> Option<(usize, usize)> {
    find_snippet_in_lines_exact(
        file_lines,
        snippet,
        hint_line,
        file_diff_ranges,
        symbol_line,
    )
    .or_else(|| {
        find_snippet_in_lines_fallback(
            file_lines,
            snippet,
            hint_line,
            file_diff_ranges,
            symbol_line,
        )
    })
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

/// Resolves a single finding location against the worktree file and diff hunks.
fn compute_character_offsets(
    start_line_str: &str,
    end_line_str: &str,
    is_single_line: bool,
) -> (Option<usize>, Option<usize>) {
    let start_line_chars = start_line_str.chars().count();
    let end_line_chars = end_line_str.chars().count();

    let start_indent_chars = start_line_str
        .chars()
        .take_while(|c| c.is_whitespace())
        .count();
    let end_trimmed_chars = end_line_str.trim_end().chars().count();

    if is_single_line {
        if start_line_chars == 0 {
            (None, None)
        } else {
            let mut s = start_indent_chars.min(start_line_chars);
            let mut e = end_trimmed_chars.min(start_line_chars);
            if e <= s {
                if s > 0 {
                    s = 0;
                }
                e = e.max(s + 1).min(start_line_chars);
            }
            if s < e {
                (Some(s), Some(e))
            } else {
                (None, None)
            }
        }
    } else {
        let s = start_indent_chars.min(start_line_chars);
        let e = if end_line_chars == 0 {
            0
        } else {
            end_trimmed_chars.max(1).min(end_line_chars)
        };
        (Some(s), Some(e))
    }
}

/// Locates a full multi-line snippet in deleted lines with strict contiguity and blank-line tolerance.
pub fn find_snippet_in_deleted_lines_exact(
    deleted_lines: &[(usize, String)],
    snippet: &str,
    hint_line: Option<usize>,
) -> Option<(usize, usize)> {
    if deleted_lines.is_empty() {
        return None;
    }
    let snip_lines = parse_snippet_lines(snippet)?;
    let non_empty_snips: Vec<&str> = snip_lines
        .iter()
        .copied()
        .filter(|l| !l.is_empty())
        .collect();

    let &first_snip = non_empty_snips.first()?;
    let mut candidates = Vec::new();
    for (i, &(line_idx, ref text)) in deleted_lines.iter().enumerate() {
        if text.trim() == first_snip {
            let mut match_ok = true;
            let mut del_curr = i;
            let mut last_line_idx = line_idx;
            for &snip in &non_empty_snips[1..] {
                let mut found_next = false;
                while del_curr + 1 < deleted_lines.len() {
                    del_curr += 1;
                    let (next_idx, ref next_text) = deleted_lines[del_curr];
                    if next_idx != last_line_idx + 1 {
                        // Enforce contiguity across deleted lines; cannot cross unmodified context or hunks
                        break;
                    }
                    let trimmed = next_text.trim();
                    if trimmed.is_empty() {
                        last_line_idx = next_idx;
                        continue;
                    }
                    if trimmed == snip {
                        last_line_idx = next_idx;
                        found_next = true;
                        break;
                    } else {
                        break;
                    }
                }
                if !found_next {
                    match_ok = false;
                    break;
                }
            }
            if match_ok {
                candidates.push((line_idx, last_line_idx));
            }
        }
    }
    if !candidates.is_empty() {
        if let Some(hint) = hint_line {
            return candidates
                .into_iter()
                .min_by_key(|&(s, _)| (s + 1).abs_diff(hint));
        }
        return candidates.into_iter().next();
    }

    None
}

/// Locates a snippet in deleted lines by matching candidate lines in descending order of length.
pub fn find_snippet_in_deleted_lines_fallback(
    deleted_lines: &[(usize, String)],
    snippet: &str,
    hint_line: Option<usize>,
) -> Option<(usize, usize)> {
    if deleted_lines.is_empty() {
        return None;
    }
    let snip_lines = parse_snippet_lines(snippet)?;

    let mut candidate_lines: Vec<&str> = snip_lines
        .iter()
        .copied()
        .filter(|l| l.chars().count() >= 4)
        .collect();
    candidate_lines.sort_by_key(|l| std::cmp::Reverse(l.chars().count()));
    candidate_lines.dedup();

    for cand in candidate_lines {
        let cand_len = cand.chars().count();
        let mut candidates = Vec::new();
        for &(line_idx, ref text) in deleted_lines {
            let trimmed = text.trim();
            if trimmed == cand || (cand_len >= 8 && trimmed.contains(cand)) {
                candidates.push((line_idx, line_idx));
            }
        }
        if !candidates.is_empty() {
            if let Some(hint) = hint_line {
                return candidates
                    .into_iter()
                    .min_by_key(|&(s, _)| (s + 1).abs_diff(hint));
            }
            return candidates.into_iter().next();
        }
    }

    None
}

pub fn find_snippet_in_deleted_lines(
    deleted_lines: &[(usize, String)],
    snippet: &str,
    hint_line: Option<usize>,
) -> Option<(usize, usize)> {
    find_snippet_in_deleted_lines_exact(deleted_lines, snippet, hint_line)
        .or_else(|| find_snippet_in_deleted_lines_fallback(deleted_lines, snippet, hint_line))
}

fn make_resolved_deleted_location(
    norm_file: &str,
    dl: &[(usize, String)],
    del_s: usize,
    del_e: usize,
) -> Option<ResolvedLocation> {
    let start_line = del_s + 1;
    let end_line = del_e + 1;
    let s_text = dl
        .iter()
        .find(|e| e.0 == del_s)
        .map(|e| e.1.as_str())
        .unwrap_or("");
    let e_text = dl
        .iter()
        .find(|e| e.0 == del_e)
        .map(|e| e.1.as_str())
        .unwrap_or("");
    let (sc, ec) = compute_character_offsets(s_text, e_text, start_line == end_line);
    let line_range = if start_line == end_line {
        format!("{start_line}")
    } else {
        format!("{start_line}-{end_line}")
    };
    Some(ResolvedLocation {
        file: norm_file.to_string(),
        line: start_line,
        end_line,
        line_range,
        start_character: sc,
        end_character: ec,
        in_diff: true,
        side: Some("PARENT".to_string()),
        code_snippet: None,
    })
}

fn resolve_in_deleted_lines_exact(
    norm_file: &str,
    dl: &[(usize, String)],
    snippet: &str,
    hint_line: Option<usize>,
) -> Option<ResolvedLocation> {
    let (del_s, del_e) = find_snippet_in_deleted_lines_exact(dl, snippet, hint_line)?;
    make_resolved_deleted_location(norm_file, dl, del_s, del_e)
}

fn resolve_in_deleted_lines_fallback(
    norm_file: &str,
    dl: &[(usize, String)],
    snippet: &str,
    hint_line: Option<usize>,
) -> Option<ResolvedLocation> {
    let (del_s, del_e) = find_snippet_in_deleted_lines_fallback(dl, snippet, hint_line)?;
    make_resolved_deleted_location(norm_file, dl, del_s, del_e)
}

#[allow(dead_code)]
fn resolve_in_deleted_lines(
    norm_file: &str,
    dl: &[(usize, String)],
    snippet: &str,
    hint_line: Option<usize>,
) -> Option<ResolvedLocation> {
    resolve_in_deleted_lines_exact(norm_file, dl, snippet, hint_line)
        .or_else(|| resolve_in_deleted_lines_fallback(norm_file, dl, snippet, hint_line))
}

fn make_resolved_parent_location(
    norm_file: &str,
    plines: &[&str],
    pre_diff: Option<&Vec<(usize, usize)>>,
    ps: usize,
    pe: usize,
) -> Option<ResolvedLocation> {
    let start_line = ps + 1;
    let end_line = pe + 1;
    let (sc, ec) = compute_character_offsets(plines[ps], plines[pe], start_line == end_line);
    let in_diff = pre_diff.is_some_and(|ranges| ranges.iter().any(|&(s, e)| ps <= e && pe >= s));
    let line_range = if start_line == end_line {
        format!("{start_line}")
    } else {
        format!("{start_line}-{end_line}")
    };
    Some(ResolvedLocation {
        file: norm_file.to_string(),
        line: start_line,
        end_line,
        line_range,
        start_character: sc,
        end_character: ec,
        in_diff,
        side: Some("PARENT".to_string()),
        code_snippet: None,
    })
}

fn resolve_in_parent_lines_exact(
    norm_file: &str,
    plines: &[&str],
    snippet: &str,
    hint_line: Option<usize>,
    pre_diff: Option<&Vec<(usize, usize)>>,
    sym: &str,
) -> Option<ResolvedLocation> {
    let sym_line = if !sym.is_empty() {
        plines
            .iter()
            .enumerate()
            .find(|(_, l)| l.contains(sym))
            .map(|(i, _)| i + 1)
    } else {
        None
    };
    let (ps, pe) = find_snippet_in_lines_exact(plines, snippet, hint_line, pre_diff, sym_line)?;
    make_resolved_parent_location(norm_file, plines, pre_diff, ps, pe)
}

fn resolve_in_parent_lines_fallback(
    norm_file: &str,
    plines: &[&str],
    snippet: &str,
    hint_line: Option<usize>,
    pre_diff: Option<&Vec<(usize, usize)>>,
    sym: &str,
) -> Option<ResolvedLocation> {
    let sym_line = if !sym.is_empty() {
        plines
            .iter()
            .enumerate()
            .find(|(_, l)| l.contains(sym))
            .map(|(i, _)| i + 1)
    } else {
        None
    };
    let (ps, pe) = find_snippet_in_lines_fallback(plines, snippet, hint_line, pre_diff, sym_line)?;
    make_resolved_parent_location(norm_file, plines, pre_diff, ps, pe)
}

#[allow(dead_code)]
fn resolve_in_parent_lines(
    norm_file: &str,
    plines: &[&str],
    snippet: &str,
    hint_line: Option<usize>,
    pre_diff: Option<&Vec<(usize, usize)>>,
    sym: &str,
) -> Option<ResolvedLocation> {
    resolve_in_parent_lines_exact(norm_file, plines, snippet, hint_line, pre_diff, sym).or_else(
        || resolve_in_parent_lines_fallback(norm_file, plines, snippet, hint_line, pre_diff, sym),
    )
}

fn make_resolved_post_location(
    norm_file: &str,
    flines: &[&str],
    post_diff: Option<&Vec<(usize, usize)>>,
    start_idx: usize,
    end_idx: usize,
) -> Option<ResolvedLocation> {
    let in_diff =
        post_diff.is_some_and(|ranges| ranges.iter().any(|&(s, e)| start_idx <= e && end_idx >= s));
    let start_line = start_idx + 1;
    let end_line = end_idx + 1;
    let (sc, ec) =
        compute_character_offsets(flines[start_idx], flines[end_idx], start_line == end_line);
    let line_range = if start_line == end_line {
        format!("{start_line}")
    } else {
        format!("{start_line}-{end_line}")
    };
    Some(ResolvedLocation {
        file: norm_file.to_string(),
        line: start_line,
        end_line,
        line_range,
        start_character: sc,
        end_character: ec,
        in_diff,
        side: Some("REVISION".to_string()),
        code_snippet: None,
    })
}

fn resolve_in_post_lines_exact(
    norm_file: &str,
    flines: &[&str],
    snippet: &str,
    hint_line: Option<usize>,
    post_diff: Option<&Vec<(usize, usize)>>,
    sym: &str,
) -> Option<ResolvedLocation> {
    let sym_line = if !sym.is_empty() {
        flines
            .iter()
            .enumerate()
            .find(|(_, l)| l.contains(sym))
            .map(|(i, _)| i + 1)
    } else {
        None
    };
    let (start_idx, end_idx) =
        find_snippet_in_lines_exact(flines, snippet, hint_line, post_diff, sym_line)?;
    make_resolved_post_location(norm_file, flines, post_diff, start_idx, end_idx)
}

fn resolve_in_post_lines_fallback(
    norm_file: &str,
    flines: &[&str],
    snippet: &str,
    hint_line: Option<usize>,
    post_diff: Option<&Vec<(usize, usize)>>,
    sym: &str,
) -> Option<ResolvedLocation> {
    let sym_line = if !sym.is_empty() {
        flines
            .iter()
            .enumerate()
            .find(|(_, l)| l.contains(sym))
            .map(|(i, _)| i + 1)
    } else {
        None
    };
    let (start_idx, end_idx) =
        find_snippet_in_lines_fallback(flines, snippet, hint_line, post_diff, sym_line)?;
    make_resolved_post_location(norm_file, flines, post_diff, start_idx, end_idx)
}

#[allow(dead_code)]
fn resolve_in_post_lines(
    norm_file: &str,
    flines: &[&str],
    snippet: &str,
    hint_line: Option<usize>,
    post_diff: Option<&Vec<(usize, usize)>>,
    sym: &str,
) -> Option<ResolvedLocation> {
    resolve_in_post_lines_exact(norm_file, flines, snippet, hint_line, post_diff, sym).or_else(
        || resolve_in_post_lines_fallback(norm_file, flines, snippet, hint_line, post_diff, sym),
    )
}

/// Strips leading diff markers (`-` or `+`) from snippet lines only if all non-empty
/// lines uniformly carry the marker. Returns the stripped snippet and whether it was a minus marker.
fn try_strip_diff_markers(raw: &str) -> Option<(String, bool)> {
    let lines: Vec<&str> = raw.lines().collect();
    let non_empty: Vec<&str> = lines
        .iter()
        .map(|l| l.trim_start())
        .filter(|l| !l.is_empty())
        .collect();
    if non_empty.is_empty() {
        return None;
    }
    let all_minus = non_empty.iter().all(|l| l.starts_with('-'));
    let all_plus = non_empty.iter().all(|l| l.starts_with('+'));

    if all_minus && !all_plus {
        let stripped = lines
            .iter()
            .map(|l| {
                let trimmed = l.trim_start();
                if let Some(rest) = trimmed.strip_prefix('-') {
                    rest
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        Some((stripped, true))
    } else if all_plus && !all_minus {
        let stripped = lines
            .iter()
            .map(|l| {
                let trimmed = l.trim_start();
                if let Some(rest) = trimmed.strip_prefix('+') {
                    rest
                } else {
                    l
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        Some((stripped, false))
    } else {
        None
    }
}

/// Resolves a single finding location against post-image, pre-image, and two-sided diff hunks.
pub fn resolve_single_location_two_sided(
    worktree_path: &Path,
    revision: Option<&str>,
    parsed_diff: &ParsedDiff,
    location: &Value,
) -> Option<ResolvedLocation> {
    let raw_file = location.get("file").and_then(Value::as_str)?;
    let norm_file =
        normalize_file_path_with_worktree_and_rev(Some(worktree_path), revision, raw_file)
            .or_else(|| normalize_file_path(raw_file))?;

    let raw_snippet = location
        .get("code_snippet")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let hint_line = location
        .get("line")
        .and_then(Value::as_i64)
        .filter(|&l| l > 0)
        .and_then(|l| usize::try_from(l).ok());
    let sym = location
        .get("function_or_symbol")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();

    let explicit_side = location.get("side").and_then(Value::as_str).map(str::trim);
    let is_parent_hint = explicit_side.is_some_and(|s| {
        s.eq_ignore_ascii_case("PARENT")
            || s.eq_ignore_ascii_case("LEFT")
            || s.eq_ignore_ascii_case("OLD")
    });

    let post_file = parsed_diff
        .pre_to_post
        .get(&norm_file)
        .cloned()
        .unwrap_or_else(|| norm_file.clone());
    let parent_file = parsed_diff
        .post_to_pre
        .get(&norm_file)
        .cloned()
        .unwrap_or_else(|| norm_file.clone());

    let post_content = read_file_content(worktree_path, revision, &post_file);
    let post_lines: Option<Vec<&str>> = post_content.as_deref().map(|c| c.lines().collect());

    let parent_rev = revision.map(|r| format!("{r}~1"));
    let parent_content = parent_rev
        .as_deref()
        .and_then(|pr| read_file_content(worktree_path, Some(pr), &parent_file));
    let parent_lines: Option<Vec<&str>> = parent_content.as_deref().map(|c| c.lines().collect());

    let post_diff = parsed_diff
        .post_ranges
        .get(&norm_file)
        .or_else(|| parsed_diff.post_ranges.get(&post_file));
    let pre_diff = parsed_diff
        .pre_ranges
        .get(&norm_file)
        .or_else(|| parsed_diff.pre_ranges.get(&parent_file));
    let del_lines = parsed_diff
        .deleted_lines
        .get(&norm_file)
        .or_else(|| parsed_diff.deleted_lines.get(&parent_file));

    let try_resolve_exact = |snip: &str, prefer_parent: bool| -> Option<ResolvedLocation> {
        if snip.is_empty() {
            return None;
        }
        if prefer_parent {
            if let Some(dl) = del_lines
                && let Some(loc) = resolve_in_deleted_lines_exact(&norm_file, dl, snip, hint_line)
            {
                return Some(loc);
            }
            if let Some(plines) = parent_lines.as_deref()
                && let Some(loc) = resolve_in_parent_lines_exact(
                    &norm_file, plines, snip, hint_line, pre_diff, sym,
                )
            {
                return Some(loc);
            }
            if let Some(flines) = post_lines.as_deref()
                && let Some(loc) =
                    resolve_in_post_lines_exact(&norm_file, flines, snip, hint_line, post_diff, sym)
            {
                return Some(loc);
            }
        } else {
            if let Some(flines) = post_lines.as_deref()
                && let Some(loc) =
                    resolve_in_post_lines_exact(&norm_file, flines, snip, hint_line, post_diff, sym)
            {
                if !loc.in_diff
                    && let Some(dl) = del_lines
                    && let Some(del_loc) =
                        resolve_in_deleted_lines_exact(&norm_file, dl, snip, hint_line)
                {
                    return Some(del_loc);
                }
                return Some(loc);
            }
            if let Some(dl) = del_lines
                && let Some(loc) = resolve_in_deleted_lines_exact(&norm_file, dl, snip, hint_line)
            {
                return Some(loc);
            }
            if let Some(plines) = parent_lines.as_deref()
                && let Some(loc) = resolve_in_parent_lines_exact(
                    &norm_file, plines, snip, hint_line, pre_diff, sym,
                )
            {
                return Some(loc);
            }
        }
        None
    };

    let try_resolve_fallback = |snip: &str, prefer_parent: bool| -> Option<ResolvedLocation> {
        if snip.is_empty() {
            return None;
        }
        if prefer_parent {
            if let Some(dl) = del_lines
                && let Some(loc) =
                    resolve_in_deleted_lines_fallback(&norm_file, dl, snip, hint_line)
            {
                return Some(loc);
            }
            if let Some(plines) = parent_lines.as_deref()
                && let Some(loc) = resolve_in_parent_lines_fallback(
                    &norm_file, plines, snip, hint_line, pre_diff, sym,
                )
            {
                return Some(loc);
            }
            if let Some(flines) = post_lines.as_deref()
                && let Some(loc) = resolve_in_post_lines_fallback(
                    &norm_file, flines, snip, hint_line, post_diff, sym,
                )
            {
                return Some(loc);
            }
        } else {
            if let Some(flines) = post_lines.as_deref()
                && let Some(loc) = resolve_in_post_lines_fallback(
                    &norm_file, flines, snip, hint_line, post_diff, sym,
                )
            {
                if !loc.in_diff
                    && let Some(dl) = del_lines
                    && let Some(del_loc) =
                        resolve_in_deleted_lines_fallback(&norm_file, dl, snip, hint_line)
                {
                    return Some(del_loc);
                }
                return Some(loc);
            }
            if let Some(dl) = del_lines
                && let Some(loc) =
                    resolve_in_deleted_lines_fallback(&norm_file, dl, snip, hint_line)
            {
                return Some(loc);
            }
            if let Some(plines) = parent_lines.as_deref()
                && let Some(loc) = resolve_in_parent_lines_fallback(
                    &norm_file, plines, snip, hint_line, pre_diff, sym,
                )
            {
                return Some(loc);
            }
        }
        None
    };

    if !raw_snippet.is_empty() {
        let stripped_info = try_strip_diff_markers(raw_snippet);

        // Phase 1: Try exact matching across all candidate sources
        if let Some(resolved) = try_resolve_exact(raw_snippet, is_parent_hint) {
            return Some(resolved);
        }

        if let Some((ref stripped, is_minus)) = stripped_info {
            let stripped_trimmed = stripped.trim();
            if !stripped_trimmed.is_empty() {
                let prefer_parent = is_parent_hint || is_minus;
                if let Some(resolved) = try_resolve_exact(stripped_trimmed, prefer_parent) {
                    return Some(resolved);
                }
            }
        }

        // Phase 2: If exact matching across all sources returned None, fall back to partial line matching
        if let Some(resolved) = try_resolve_fallback(raw_snippet, is_parent_hint) {
            return Some(resolved);
        }

        if let Some((ref stripped, is_minus)) = stripped_info {
            let stripped_trimmed = stripped.trim();
            if !stripped_trimmed.is_empty() {
                let prefer_parent = is_parent_hint || is_minus;
                if let Some(resolved) = try_resolve_fallback(stripped_trimmed, prefer_parent) {
                    return Some(resolved);
                }
            }
        }
    }

    // Fall back to hint_line or symbol if available
    let fallback_lines = if is_parent_hint && parent_lines.is_some() {
        parent_lines.as_deref()
    } else {
        post_lines.as_deref()
    };
    let fallback_diff = if is_parent_hint && parent_lines.is_some() {
        pre_diff
    } else {
        post_diff
    };
    let fallback_side = if is_parent_hint && parent_lines.is_some() {
        "PARENT"
    } else {
        "REVISION"
    };

    if let Some(flines) = fallback_lines
        && !flines.is_empty()
    {
        let hint_range = hint_line.filter(|&l| l <= flines.len()).map(|l| {
            let idx = l.saturating_sub(1);
            (idx, idx)
        });
        let (start_idx, end_idx) = if let Some(range) = hint_range {
            range
        } else if !sym.is_empty() {
            let sym_match = flines
                .iter()
                .enumerate()
                .find(|(_, line)| line.contains(sym))
                .map(|(i, _)| (i, i));
            sym_match?
        } else {
            return None;
        };

        let start_line = start_idx + 1;
        let end_line = end_idx + 1;
        let (sc, ec) =
            compute_character_offsets(flines[start_idx], flines[end_idx], start_line == end_line);
        let line_range = if start_line == end_line {
            format!("{start_line}")
        } else {
            format!("{start_line}-{end_line}")
        };
        let in_diff = fallback_diff
            .is_some_and(|ranges| ranges.iter().any(|&(s, e)| start_idx <= e && end_idx >= s));
        let snippet_out = if raw_snippet.is_empty() {
            Some(flines[start_idx..=end_idx].join("\n"))
        } else {
            None
        };

        return Some(ResolvedLocation {
            file: norm_file,
            line: start_line,
            end_line,
            line_range,
            start_character: sc,
            end_character: ec,
            in_diff,
            side: Some(fallback_side.to_string()),
            code_snippet: snippet_out,
        });
    }

    None
}

/// Resolves a single finding location against the worktree file and diff hunks.
pub fn resolve_single_location(
    worktree_path: &Path,
    revision: Option<&str>,
    diff_ranges: &HashMap<String, Vec<(usize, usize)>>,
    location: &Value,
) -> Option<ResolvedLocation> {
    let parsed_diff = ParsedDiff {
        post_ranges: diff_ranges.clone(),
        ..Default::default()
    };
    resolve_single_location_two_sided(worktree_path, revision, &parsed_diff, location)
}

/// Resolves and enriches finding locations using pre-parsed two-sided diff details.
pub fn resolve_finding_locations_with_parsed_diff(
    worktree_path: &Path,
    revision: Option<&str>,
    parsed_diff: &ParsedDiff,
    findings: &mut [Value],
) {
    for finding in findings.iter_mut() {
        if let Some(locs) = finding.get_mut("locations").and_then(Value::as_array_mut) {
            for loc in locs.iter_mut() {
                if let Some(resolved) =
                    resolve_single_location_two_sided(worktree_path, revision, parsed_diff, loc)
                    && let Some(map) = loc.as_object_mut()
                {
                    map.insert("file".to_string(), json!(resolved.file));
                    map.insert("line".to_string(), json!(resolved.line));
                    map.insert("end_line".to_string(), json!(resolved.end_line));
                    map.insert("line_range".to_string(), json!(resolved.line_range));
                    if let Some(sc) = resolved.start_character {
                        map.insert("start_character".to_string(), json!(sc));
                    } else {
                        map.remove("start_character");
                    }
                    if let Some(ec) = resolved.end_character {
                        map.insert("end_character".to_string(), json!(ec));
                    } else {
                        map.remove("end_character");
                    }
                    map.insert("in_diff".to_string(), json!(resolved.in_diff));
                    if let Some(side) = &resolved.side {
                        map.insert("side".to_string(), json!(side));
                    }
                    if let Some(snip) = resolved.code_snippet {
                        map.insert("code_snippet".to_string(), json!(snip));
                    }
                }
            }
        }
    }
}

/// Resolves and enriches finding locations using pre-parsed post-image diff ranges.
pub fn resolve_finding_locations_with_ranges(
    worktree_path: &Path,
    revision: Option<&str>,
    diff_ranges: &HashMap<String, Vec<(usize, usize)>>,
    findings: &mut [Value],
) {
    let parsed_diff = ParsedDiff {
        post_ranges: diff_ranges.clone(),
        ..Default::default()
    };
    resolve_finding_locations_with_parsed_diff(worktree_path, revision, &parsed_diff, findings);
}

/// Resolves and enriches all finding locations in place using the target worktree and diff.
pub fn resolve_finding_locations(
    worktree_path: &Path,
    target_commit_diff: &str,
    findings: &mut [Value],
) {
    let parsed_diff = parse_two_sided_diff(target_commit_diff);
    resolve_finding_locations_with_parsed_diff(worktree_path, None, &parsed_diff, findings);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn test_normalize_file_path() {
        assert_eq!(
            normalize_file_path("b/drivers/net/e1000.c"),
            Some("drivers/net/e1000.c".to_string())
        );
        assert_eq!(
            normalize_file_path("a/drivers/net/e1000.c"),
            Some("drivers/net/e1000.c".to_string())
        );
        assert_eq!(
            normalize_file_path("/drivers/net/e1000.c"),
            Some("drivers/net/e1000.c".to_string())
        );
        assert_eq!(
            normalize_file_path("drivers/net/e1000.c"),
            Some("drivers/net/e1000.c".to_string())
        );

        // Path traversal rejection
        assert_eq!(normalize_file_path("../etc/passwd"), None);
        assert_eq!(normalize_file_path("b/../etc/passwd"), None);
        assert_eq!(normalize_file_path("drivers/../../etc/shadow"), None);
        assert_eq!(normalize_file_path(""), None);
        assert_eq!(normalize_file_path("/"), None);
        assert_eq!(normalize_file_path("."), None);
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
    fn test_resolve_single_location_in_worktree() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("kernel/sched/core.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();

        let mut f = File::create(&file_path).unwrap();
        writeln!(f, "// Header").unwrap();
        writeln!(f, "void balance(struct rq *rq) {{").unwrap();
        writeln!(f, "    raw_spin_lock(&rq->lock);").unwrap();
        writeln!(f, "    update_rq_clock(rq);").unwrap();
        writeln!(f, "    raw_spin_unlock(&rq->lock);").unwrap();
        writeln!(f, "}}").unwrap();

        let diff = r#"diff --git a/kernel/sched/core.c b/kernel/sched/core.c
index 123456..789abc 100644
--- a/kernel/sched/core.c
+++ b/kernel/sched/core.c
@@ -2,4 +2,5 @@
 void balance(struct rq *rq) {
+    raw_spin_lock(&rq->lock);
     update_rq_clock(rq);
     raw_spin_unlock(&rq->lock);
 }
"#;
        let diff_ranges = parse_diff_ranges(diff);

        // Location with null line, but exact snippet
        let loc = json!({
            "file": "b/kernel/sched/core.c",
            "code_snippet": "raw_spin_lock(&rq->lock);\nupdate_rq_clock(rq);",
            "function_or_symbol": "balance"
        });

        let resolved = resolve_single_location(dir.path(), None, &diff_ranges, &loc).unwrap();
        assert_eq!(resolved.file, "kernel/sched/core.c");
        assert_eq!(resolved.line, 3);
        assert_eq!(resolved.end_line, 4);
        assert_eq!(resolved.line_range, "3-4");
        assert_eq!(resolved.start_character, Some(4));
        assert!(resolved.in_diff);
    }

    #[test]
    fn test_resolve_single_location_from_git_blob_revision() {
        let dir = tempdir().unwrap();
        let repo_path = dir.path();

        let mut init_cmd = crate::git_cmd::in_dir(repo_path);
        init_cmd.args(["init"]);
        assert!(init_cmd.status().unwrap().success());

        let mut user_cmd = crate::git_cmd::in_dir(repo_path);
        user_cmd.args(["config", "user.email", "test@example.com"]);
        assert!(user_cmd.status().unwrap().success());
        let mut name_cmd = crate::git_cmd::in_dir(repo_path);
        name_cmd.args(["config", "user.name", "Test User"]);
        assert!(name_cmd.status().unwrap().success());

        let file_path = repo_path.join("kernel/sched/core.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(
            &file_path,
            "// Header\nvoid balance(struct rq *rq) {\n    raw_spin_lock(&rq->lock);\n    update_rq_clock(rq);\n    raw_spin_unlock(&rq->lock);\n}\n",
        )
        .unwrap();

        let mut add_cmd = crate::git_cmd::in_dir(repo_path);
        add_cmd.args(["add", "kernel/sched/core.c"]);
        assert!(add_cmd.status().unwrap().success());

        let mut commit_cmd = crate::git_cmd::in_dir(repo_path);
        commit_cmd.args(["commit", "-m", "Initial commit"]);
        assert!(commit_cmd.status().unwrap().success());

        let mut rev_cmd = crate::git_cmd::in_dir(repo_path);
        rev_cmd.args(["rev-parse", "HEAD"]);
        let rev_out = rev_cmd.output().unwrap();
        assert!(rev_out.status.success());
        let head_sha = String::from_utf8(rev_out.stdout)
            .unwrap()
            .trim()
            .to_string();

        // Remove file from disk to guarantee resolution reads the git blob at head_sha
        std::fs::remove_file(&file_path).unwrap();
        assert!(!file_path.exists());

        let content = read_file_content(repo_path, Some(&head_sha), "kernel/sched/core.c");
        assert!(content.is_some(), "read_file_content should load git blob");
        assert!(content.unwrap().contains("raw_spin_lock(&rq->lock);"));

        let diff = r#"diff --git a/kernel/sched/core.c b/kernel/sched/core.c
index 123456..789abc 100644
--- a/kernel/sched/core.c
+++ b/kernel/sched/core.c
@@ -2,4 +2,5 @@
 void balance(struct rq *rq) {
+    raw_spin_lock(&rq->lock);
     update_rq_clock(rq);
     raw_spin_unlock(&rq->lock);
 }
"#;
        let diff_ranges = parse_diff_ranges(diff);
        let loc = json!({
            "file": "b/kernel/sched/core.c",
            "code_snippet": "raw_spin_lock(&rq->lock);\nupdate_rq_clock(rq);",
            "function_or_symbol": "balance"
        });

        let resolved =
            resolve_single_location(repo_path, Some(&head_sha), &diff_ranges, &loc).unwrap();
        assert_eq!(resolved.file, "kernel/sched/core.c");
        assert_eq!(resolved.line, 3);
        assert_eq!(resolved.end_line, 4);
        assert_eq!(resolved.line_range, "3-4");
        assert_eq!(resolved.start_character, Some(4));
        assert!(resolved.in_diff);
    }

    #[test]
    fn test_resolve_single_location_parent_git_blob_fallback() {
        let dir = tempdir().unwrap();
        let repo_path = dir.path();

        let mut init_cmd = crate::git_cmd::in_dir(repo_path);
        init_cmd.args(["init"]);
        assert!(init_cmd.status().unwrap().success());

        let mut user_cmd = crate::git_cmd::in_dir(repo_path);
        user_cmd.args(["config", "user.email", "test@example.com"]);
        assert!(user_cmd.status().unwrap().success());
        let mut name_cmd = crate::git_cmd::in_dir(repo_path);
        name_cmd.args(["config", "user.name", "Test User"]);
        assert!(name_cmd.status().unwrap().success());

        let file_path = repo_path.join("kernel/sched/core.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();

        // 1. Initial parent commit
        std::fs::write(
            &file_path,
            "// Header\nvoid balance(struct rq *rq) {\n    old_code();\n    update_rq_clock(rq);\n}\n",
        )
        .unwrap();
        let mut add1 = crate::git_cmd::in_dir(repo_path);
        add1.args(["add", "kernel/sched/core.c"]);
        assert!(add1.status().unwrap().success());
        let mut commit1 = crate::git_cmd::in_dir(repo_path);
        commit1.args(["commit", "-m", "Parent commit"]);
        assert!(commit1.status().unwrap().success());

        // 2. Child commit removing old_code and adding new_code
        std::fs::write(
            &file_path,
            "// Header\nvoid balance(struct rq *rq) {\n    new_code();\n    update_rq_clock(rq);\n}\n",
        )
        .unwrap();
        let mut add2 = crate::git_cmd::in_dir(repo_path);
        add2.args(["add", "kernel/sched/core.c"]);
        assert!(add2.status().unwrap().success());
        let mut commit2 = crate::git_cmd::in_dir(repo_path);
        commit2.args(["commit", "-m", "Child commit"]);
        assert!(commit2.status().unwrap().success());

        let mut rev_cmd = crate::git_cmd::in_dir(repo_path);
        rev_cmd.args(["rev-parse", "HEAD"]);
        let rev_out = rev_cmd.output().unwrap();
        let child_sha = String::from_utf8(rev_out.stdout)
            .unwrap()
            .trim()
            .to_string();

        // Remove file from disk so resolution relies on git blobs
        std::fs::remove_file(&file_path).unwrap();

        let diff = r#"diff --git a/kernel/sched/core.c b/kernel/sched/core.c
--- a/kernel/sched/core.c
+++ b/kernel/sched/core.c
@@ -3,2 +3,2 @@
-    old_code();
+    new_code();
"#;
        let parsed = parse_two_sided_diff(diff);
        // Query an untouched line from parent that is not in deleted_lines.
        // With side: "PARENT" and disk file removed, this strictly exercises
        // reading and parsing the git blob at child_sha~1 via resolve_in_parent_lines.
        let loc = json!({
            "file": "kernel/sched/core.c",
            "code_snippet": "update_rq_clock(rq);",
            "function_or_symbol": "balance",
            "side": "PARENT"
        });

        let resolved =
            resolve_single_location_two_sided(repo_path, Some(&child_sha), &parsed, &loc).unwrap();
        assert_eq!(resolved.file, "kernel/sched/core.c");
        assert_eq!(resolved.line, 4);
        assert_eq!(resolved.end_line, 4);
        assert_eq!(resolved.side, Some("PARENT".to_string()));
        assert!(!resolved.in_diff);
    }

    #[test]
    fn test_resolve_finding_locations_mutates_json() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("mm/page_alloc.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();

        let mut f = File::create(&file_path).unwrap();
        for i in 1..=50 {
            if i == 25 {
                writeln!(f, "    kfree(ptr);").unwrap();
            } else {
                writeln!(f, "    // line {}", i).unwrap();
            }
        }

        let diff = r#"diff --git a/mm/page_alloc.c b/mm/page_alloc.c
--- a/mm/page_alloc.c
+++ b/mm/page_alloc.c
@@ -20,10 +20,10 @@
"#;

        let mut findings = vec![json!({
            "problem": "Double free in alloc path",
            "severity": "High",
            "locations": [
                {
                    "file": "mm/page_alloc.c",
                    "code_snippet": "kfree(ptr);",
                    "line": null
                }
            ]
        })];

        resolve_finding_locations(dir.path(), diff, &mut findings);

        let loc = &findings[0]["locations"][0];
        assert_eq!(loc["file"], "mm/page_alloc.c");
        assert_eq!(loc["line"], 25);
        assert_eq!(loc["end_line"], 25);
        assert_eq!(loc["line_range"], "25");
        assert_eq!(loc["start_character"], 4);
        assert_eq!(loc["in_diff"], true);
    }

    #[test]
    fn test_resolve_location_outside_diff_marks_not_in_diff() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("fs/ext4/inode.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();

        let mut f = File::create(&file_path).unwrap();
        for i in 1..=100 {
            if i == 80 {
                writeln!(f, "    brelse(bh);").unwrap();
            } else {
                writeln!(f, "    // line {}", i).unwrap();
            }
        }

        // Diff touches lines 10..20, location is at line 80
        let diff = r#"diff --git a/fs/ext4/inode.c b/fs/ext4/inode.c
--- a/fs/ext4/inode.c
+++ b/fs/ext4/inode.c
@@ -10,10 +10,10 @@
"#;

        let mut findings = vec![json!({
            "problem": "Unchecked buffer head release",
            "locations": [
                {
                    "file": "fs/ext4/inode.c",
                    "code_snippet": "brelse(bh);",
                    "line": null
                }
            ]
        })];

        resolve_finding_locations(dir.path(), diff, &mut findings);

        let loc = &findings[0]["locations"][0];
        assert_eq!(loc["line"], 80);
        assert_eq!(loc["in_diff"], false);
    }

    #[test]
    fn test_resolve_location_nonexistent_file_preserves_original() {
        let dir = tempdir().unwrap();
        let mut findings = vec![json!({
            "problem": "Dead code in removed file",
            "locations": [
                {
                    "file": "nonexistent/file.c",
                    "code_snippet": "foo();",
                    "line": null
                }
            ]
        })];

        resolve_finding_locations(dir.path(), "", &mut findings);

        // Location should remain untouched when file cannot be read
        let loc = &findings[0]["locations"][0];
        assert_eq!(loc["file"], "nonexistent/file.c");
        assert!(loc.get("line").unwrap().is_null());
        assert!(loc.get("in_diff").is_none());
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
    fn test_multiline_shorter_last_line_does_not_force_out_of_bounds_end_char() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("kernel/sched.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();

        let mut f = File::create(&file_path).unwrap();
        writeln!(f, "        if (err) {{").unwrap(); // indent 8
        writeln!(f, "}}").unwrap(); // length 1

        let diff_ranges = HashMap::new();
        let loc = json!({
            "file": "kernel/sched.c",
            "code_snippet": "        if (err) {\n}",
        });

        let resolved = resolve_single_location(dir.path(), None, &diff_ranges, &loc).unwrap();
        assert_eq!(resolved.line, 1);
        assert_eq!(resolved.end_line, 2);
        assert_eq!(resolved.start_character, Some(8));
        // end_character on line 2 must not be inflated by start_character (8)
        assert_eq!(resolved.end_character, Some(1));
    }

    #[test]
    fn test_multibyte_utf8_character_counts() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("drivers/misc.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();

        let mut f = File::create(&file_path).unwrap();
        // Line with copyright symbol (2 bytes in UTF-8, 1 char)
        writeln!(f, "    // Copyright © 2026").unwrap();

        let diff_ranges = HashMap::new();
        let loc = json!({
            "file": "drivers/misc.c",
            "line": 1,
        });

        let resolved = resolve_single_location(dir.path(), None, &diff_ranges, &loc).unwrap();
        assert_eq!(resolved.line, 1);
        assert_eq!(resolved.start_character, Some(4));
        // 4 spaces + 19 chars = 23 characters (24 bytes in UTF-8)
        assert_eq!(resolved.end_character, Some(23));
    }

    #[test]
    fn test_empty_line_omits_character_range() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("drivers/empty.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();

        let mut f = File::create(&file_path).unwrap();
        writeln!(f, "int a;").unwrap();
        writeln!(f).unwrap(); // empty line 2
        writeln!(f, "int b;").unwrap();

        let diff_ranges = HashMap::new();
        let loc = json!({
            "file": "drivers/empty.c",
            "line": 2,
        });

        let resolved = resolve_single_location(dir.path(), None, &diff_ranges, &loc).unwrap();
        assert_eq!(resolved.line, 2);
        assert_eq!(resolved.start_character, None);
        assert_eq!(resolved.end_character, None);
    }

    #[test]
    fn test_out_of_bounds_line_hint_falls_back_to_symbol() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("fs/buffer.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();

        let mut f = File::create(&file_path).unwrap();
        writeln!(f, "static void sync_buffers() {{").unwrap();
        writeln!(f, "    return;").unwrap();
        writeln!(f, "}}").unwrap();

        let diff_ranges = HashMap::new();
        // LLM hallucinated line 999 (out of bounds for 3-line file), but gave symbol
        let loc = json!({
            "file": "fs/buffer.c",
            "line": 999,
            "function_or_symbol": "sync_buffers",
        });

        let resolved = resolve_single_location(dir.path(), None, &diff_ranges, &loc).unwrap();
        assert_eq!(resolved.line, 1);
    }

    #[test]
    fn test_file_size_exceeding_max_is_skipped() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("drivers/giant.bin");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();

        let f = File::create(&file_path).unwrap();
        // Truncate to MAX_RESOLVE_FILE_SIZE + 1 byte
        f.set_len(MAX_RESOLVE_FILE_SIZE + 1).unwrap();

        let diff_ranges = HashMap::new();
        let loc = json!({
            "file": "drivers/giant.bin",
            "line": 1,
        });

        let resolved = resolve_single_location(dir.path(), None, &diff_ranges, &loc);
        assert!(resolved.is_none());
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
    fn test_resolve_finding_locations_inserts_missing_keys_without_panic() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("kernel/test.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();

        let mut f = File::create(&file_path).unwrap();
        writeln!(f, "void test_func(void) {{").unwrap();
        writeln!(f, "    return;").unwrap();
        writeln!(f, "}}").unwrap();

        // Object with ONLY file and code_snippet, missing all other keys
        let mut findings = vec![json!({
            "problem": "test problem",
            "locations": [
                {
                    "file": "kernel/test.c",
                    "code_snippet": "return;"
                }
            ]
        })];

        // Must not panic on missing keys!
        resolve_finding_locations(dir.path(), "", &mut findings);

        let loc = &findings[0]["locations"][0];
        assert_eq!(loc["line"], 2);
        assert_eq!(loc["end_line"], 2);
        assert_eq!(loc["line_range"], "2");
        assert_eq!(loc["start_character"], 4);
        assert_eq!(loc["end_character"], 11);
        assert_eq!(loc["in_diff"], false);
    }

    #[test]
    fn test_parse_diff_ranges_exact_and_no_gap_inflation() {
        let diff = r#"diff --git a/fs/ext4/super.c b/fs/ext4/super.c
--- a/fs/ext4/super.c
+++ b/fs/ext4/super.c
@@ -10,6 +10,7 @@
  context_line_10
  context_line_11
+ added_line_12
  context_line_13
  context_line_14
  context_line_15
@@ -20,5 +21,6 @@
  context_line_21
  context_line_22
+ added_line_23
  context_line_24
"#;
        let ranges = parse_diff_ranges(diff);
        let ext4_ranges = ranges.get("fs/ext4/super.c").unwrap();

        // Must ONLY contain lines 12 (idx 11) and 23 (idx 22).
        // Must NOT include context lines 10, 11, 13, 14, 15, 21, 22, 24.
        // Must NOT merge gap between line 12 and line 23!
        assert_eq!(ext4_ranges, &vec![(11, 11), (22, 22)]);
    }

    #[test]
    fn test_added_lines_starting_with_plus_plus_do_not_terminate_hunk() {
        let diff = r#"diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
 fn main() {
+++ b/fake_header;
     println!("hello");
 }
"#;
        let ranges = parse_diff_ranges(diff);
        let main_ranges = ranges.get("src/main.rs").unwrap();
        // Line 2 (idx 1) is the added line `++ b/fake_header;`
        assert_eq!(main_ranges, &vec![(1, 1)]);
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
        let diff = r#"diff --git "a/path with spaces.c" "b/path with spaces.c"
--- "a/path with spaces.c"
+++ "b/path with spaces.c"
@@ -1,2 +1,3 @@
 line 1
+line 2
"#;
        let ranges = parse_diff_ranges(diff);
        let space_ranges = ranges.get("path with spaces.c").unwrap();
        assert_eq!(space_ranges, &vec![(1, 1)]);
    }

    #[test]
    fn test_normalize_file_path_preserves_legitimate_root_dir() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("b/main.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, "int main() {}\n").unwrap();

        // When b/main.c actually exists in worktree, normalize_file_path_with_worktree must keep "b/main.c"
        let norm = normalize_file_path_with_worktree(Some(dir.path()), "b/main.c").unwrap();
        assert_eq!(norm, "b/main.c");

        // When b/other.c does NOT exist, but other.c exists, it strips "b/"
        let file_path2 = dir.path().join("other.c");
        std::fs::write(&file_path2, "int other() {}\n").unwrap();
        let norm2 = normalize_file_path_with_worktree(Some(dir.path()), "b/other.c").unwrap();
        assert_eq!(norm2, "other.c");
    }

    #[test]
    fn test_normalize_file_path_handles_leading_dot_slash_and_rejects_traversal() {
        assert_eq!(
            normalize_file_path("./kernel/sched.c"),
            Some("kernel/sched.c".to_string())
        );
        assert_eq!(
            normalize_file_path(".//kernel/sched.c"),
            Some("kernel/sched.c".to_string())
        );
        assert_eq!(
            normalize_file_path("/kernel/sched.c"),
            Some("kernel/sched.c".to_string())
        );
        assert_eq!(normalize_file_path("../kernel/sched.c"), None);
        assert_eq!(normalize_file_path("kernel/../sched.c"), None);
        assert_eq!(normalize_file_path("-v/kernel/sched.c"), None);
        assert_eq!(normalize_file_path(""), None);
        assert_eq!(normalize_file_path("."), None);
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

    #[test]
    fn test_parse_two_sided_diff_handles_file_renames_and_null_files() {
        let diff = r#"diff --git a/old_dir/foo.c b/new_dir/foo.c
similarity index 90%
rename from old_dir/foo.c
rename to new_dir/foo.c
--- a/old_dir/foo.c
+++ b/new_dir/foo.c
@@ -10,3 +10,4 @@ void run(void) {
-    old_work();
+    new_work();
+    more_work();
 }
diff --git a/created.c b/created.c
new file mode 100644
--- /dev/null
+++ b/created.c
@@ -0,0 +1,2 @@
+int new_func(void) { return 0; }
+"#;
        let parsed = parse_two_sided_diff(diff);

        // Pre-ranges and deleted lines belong to old path; post-ranges belong to new path
        assert!(parsed.deleted_lines.contains_key("old_dir/foo.c"));
        assert!(!parsed.deleted_lines.contains_key("new_dir/foo.c"));
        assert_eq!(
            parsed.deleted_lines.get("old_dir/foo.c").unwrap()[0].1,
            "    old_work();"
        );

        assert!(!parsed.post_ranges.contains_key("old_dir/foo.c"));
        assert!(parsed.post_ranges.contains_key("new_dir/foo.c"));

        // Rename mappings are tracked cleanly without cross-contaminating line ranges
        assert_eq!(
            parsed.pre_to_post.get("old_dir/foo.c"),
            Some(&"new_dir/foo.c".to_string())
        );
        assert_eq!(
            parsed.post_to_pre.get("new_dir/foo.c"),
            Some(&"old_dir/foo.c".to_string())
        );

        // Created file should have post-ranges but no pre-ranges
        assert!(parsed.post_ranges.contains_key("created.c"));
        assert!(!parsed.pre_ranges.contains_key("created.c"));
        assert!(!parsed.deleted_lines.contains_key("created.c"));
    }

    #[test]
    fn test_extract_diff_git_source_unquoted_with_spaces() {
        let line = "diff --git a/path with spaces/source.c b/path with spaces/target.c";
        assert_eq!(
            extract_diff_git_source(line),
            Some("path with spaces/source.c".to_string())
        );
        assert_eq!(
            extract_diff_git_target(line),
            Some("path with spaces/target.c".to_string())
        );
    }

    #[test]
    fn test_extract_diff_git_source_quoted_with_escaped_quotes() {
        let line = r#"diff --git "a/path \"with\" quotes/foo.c" "b/path \"with\" quotes/foo.c""#;
        assert_eq!(
            extract_diff_git_source(line),
            Some(r#"path "with" quotes/foo.c"#.to_string())
        );
        assert_eq!(
            extract_diff_git_target(line),
            Some(r#"path "with" quotes/foo.c"#.to_string())
        );
    }

    #[test]
    fn test_extract_diff_git_source_unquoted_with_embedded_b_slash() {
        let line = "diff --git a/sub b/file.txt b/sub b/file.txt";
        assert_eq!(
            extract_diff_git_source(line),
            Some("sub b/file.txt".to_string())
        );
        assert_eq!(
            extract_diff_git_target(line),
            Some("sub b/file.txt".to_string())
        );
    }

    #[test]
    fn test_extract_diff_git_multibyte_utf8_paths() {
        // Multi-byte UTF-8 in symmetric paths
        let line = "diff --git a/🦀/test.rs b/🦀/test.rs";
        assert_eq!(
            extract_diff_git_source(line),
            Some("🦀/test.rs".to_string())
        );
        assert_eq!(
            extract_diff_git_target(line),
            Some("🦀/test.rs".to_string())
        );

        // Path where (rest.len() - 5) / 2 falls on a non-char-boundary must not panic
        let malformed = "diff --git a/😊x😊";
        assert_eq!(extract_diff_git_source(malformed), None);
        assert_eq!(extract_diff_git_target(malformed), None);
    }

    #[test]
    fn test_multi_file_diff_without_diff_git_updates_pre_file_correctly() {
        let diff = r#"--- a/file1.c
+++ b/file1.c
@@ -1,1 +1,1 @@
-old1
+new1
--- a/file2.c
+++ b/file2.c
@@ -1,1 +1,1 @@
-old2
+new2
"#;
        let parsed = parse_two_sided_diff(diff);
        assert!(parsed.post_ranges.contains_key("file1.c"));
        assert!(parsed.post_ranges.contains_key("file2.c"));
        assert!(parsed.deleted_lines.contains_key("file1.c"));
        assert!(parsed.deleted_lines.contains_key("file2.c"));
        assert_eq!(
            parsed.deleted_lines["file1.c"],
            vec![(0, "old1".to_string())]
        );
        assert_eq!(
            parsed.deleted_lines["file2.c"],
            vec![(0, "old2".to_string())]
        );
    }

    #[test]
    fn test_unparseable_diff_git_header_does_not_leak_previous_file_state() {
        let diff = r#"diff --git a/valid.c b/valid.c
--- a/valid.c
+++ b/valid.c
@@ -1,1 +1,1 @@
-old_valid
+new_valid
diff --git unparseable_header
@@ -10,1 +10,1 @@
-old_corrupt
+new_corrupt
"#;
        let parsed = parse_two_sided_diff(diff);
        assert_eq!(parsed.post_ranges.get("valid.c"), Some(&vec![(0, 0)]));
        assert_eq!(
            parsed.deleted_lines.get("valid.c"),
            Some(&vec![(0, "old_valid".to_string())])
        );
        // The unparseable hunk must not leak into valid.c
        assert!(!parsed.post_ranges.contains_key("unparseable_header"));
    }

    #[test]
    fn test_combined_diff_header_terminates_hunk_correctly() {
        let diff = r#"--- a/file1.c
+++ b/file1.c
@@ -1,1 +1,1 @@
-old1
+new1
---- a/file2.c
++++ b/file2.c
@@ -1,1 +1,1 @@
-old2
+new2
"#;
        let parsed = parse_two_sided_diff(diff);
        assert!(parsed.post_ranges.contains_key("file1.c"));
        assert!(parsed.post_ranges.contains_key("file2.c"));
        assert!(parsed.deleted_lines.contains_key("file1.c"));
        assert!(parsed.deleted_lines.contains_key("file2.c"));
        assert_eq!(
            parsed.deleted_lines["file1.c"],
            vec![(0, "old1".to_string())]
        );
        assert_eq!(
            parsed.deleted_lines["file2.c"],
            vec![(0, "old2".to_string())]
        );
    }

    #[test]
    fn test_parse_two_sided_diff_with_no_newline_marker_and_empty_context_lines() {
        let diff = r#"diff --git a/foo.rs b/foo.rs
index 1111111..2222222 100644
--- a/foo.rs
+++ b/foo.rs
@@ -1,3 +1,3 @@
 context 1
-old line without newline
\ No newline at end of file
+new line without newline
\ No newline at end of file
 context 2
"#;
        let res = parse_two_sided_diff(diff);
        assert_eq!(res.post_ranges.get("foo.rs"), Some(&vec![(1, 1)]));
        assert_eq!(res.pre_ranges.get("foo.rs"), Some(&vec![(1, 1)]));
        assert_eq!(
            res.deleted_lines.get("foo.rs"),
            Some(&vec![(1, "old line without newline".to_string())])
        );

        // Also verify stripped empty context lines do not abort hunks
        let stripped_diff = "diff --git a/foo.rs b/foo.rs\n--- a/foo.rs\n+++ b/foo.rs\n@@ -1,4 +1,4 @@\n context 1\n\n-old line\n+new line\n context 2\n";
        let res_stripped = parse_two_sided_diff(stripped_diff);
        assert_eq!(res_stripped.post_ranges.get("foo.rs"), Some(&vec![(2, 2)]));
        assert_eq!(res_stripped.pre_ranges.get("foo.rs"), Some(&vec![(2, 2)]));
    }

    #[test]
    fn test_missing_post_image_header_does_not_leak_previous_file_target() {
        let diff = r#"diff --git a/file1.rs b/file1.rs
--- a/file1.rs
+++ b/file1.rs
@@ -1,1 +1,1 @@
-old1
+new1
--- a/file2.rs
@@ -1,1 +1,1 @@
-old2
+new2
"#;
        let res = parse_two_sided_diff(diff);
        assert_eq!(res.post_ranges.get("file1.rs"), Some(&vec![(0, 0)]));
        assert_eq!(res.post_ranges.get("file1.rs").unwrap().len(), 1);
        assert_eq!(res.pre_ranges.get("file1.rs"), Some(&vec![(0, 0)]));
        assert_eq!(res.pre_ranges.get("file2.rs"), Some(&vec![(0, 0)]));
    }

    #[test]
    fn test_parse_two_sided_diff_captures_pre_and_post_ranges_and_deleted_lines() {
        let diff = r#"diff --git a/kernel/bpf/verifier.c b/kernel/bpf/verifier.c
--- a/kernel/bpf/verifier.c
+++ b/kernel/bpf/verifier.c
@@ -100,6 +100,5 @@ int check_map(void) {
     validate_map();
-    if (btf_record_has_field(map, BPF_TIMER))
-        return -EINVAL;
+    check_timer();
     return 0;
 }
"#;
        let parsed = parse_two_sided_diff(diff);
        let post = parsed.post_ranges.get("kernel/bpf/verifier.c").unwrap();
        assert_eq!(post, &vec![(100, 100)]);

        let pre = parsed.pre_ranges.get("kernel/bpf/verifier.c").unwrap();
        assert_eq!(pre, &vec![(100, 101)]);

        let del = parsed.deleted_lines.get("kernel/bpf/verifier.c").unwrap();
        assert_eq!(del.len(), 2);
        assert_eq!(del[0].0, 100);
        assert_eq!(del[0].1, "    if (btf_record_has_field(map, BPF_TIMER))");
        assert_eq!(del[1].0, 101);
        assert_eq!(del[1].1, "        return -EINVAL;");
    }

    #[test]
    fn test_resolve_location_deleted_lines_resolves_to_parent() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("kernel/bpf/verifier.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();

        // In post-image, the if statement is deleted and replaced with check_timer()
        std::fs::write(
            &file_path,
            "int check_map(void) {\n    validate_map();\n    check_timer();\n    return 0;\n}\n",
        )
        .unwrap();

        let diff = r#"diff --git a/kernel/bpf/verifier.c b/kernel/bpf/verifier.c
--- a/kernel/bpf/verifier.c
+++ b/kernel/bpf/verifier.c
@@ -100,6 +100,5 @@ int check_map(void) {
     validate_map();
-    if (btf_record_has_field(map, BPF_TIMER))
-        return -EINVAL;
+    check_timer();
     return 0;
 }
"#;
        let parsed = parse_two_sided_diff(diff);
        let loc = json!({
            "file": "kernel/bpf/verifier.c",
            "line": 101,
            "code_snippet": "if (btf_record_has_field(map, BPF_TIMER))\n    return -EINVAL;",
            "function_or_symbol": "check_map"
        });

        let resolved = resolve_single_location_two_sided(dir.path(), None, &parsed, &loc).unwrap();
        assert_eq!(resolved.file, "kernel/bpf/verifier.c");
        assert_eq!(resolved.line, 101);
        assert_eq!(resolved.end_line, 102);
        assert_eq!(resolved.line_range, "101-102");
        assert_eq!(resolved.side, Some("PARENT".to_string()));
        assert!(resolved.in_diff);
    }

    #[test]
    fn test_resolve_location_leading_minus_stripped_resolves_parent() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("include/linux/mm.h");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, "struct page {\n    int refcount;\n};\n").unwrap();

        let diff = r#"diff --git a/include/linux/mm.h b/include/linux/mm.h
--- a/include/linux/mm.h
+++ b/include/linux/mm.h
@@ -150,5 +150,3 @@ struct page {
-#ifdef CONFIG_OFFLOAD
-    void *pool;
-#endif
     int refcount;
 };
"#;
        let parsed = parse_two_sided_diff(diff);
        let loc = json!({
            "file": "include/linux/mm.h",
            "line": 150,
            "code_snippet": "-#ifdef CONFIG_OFFLOAD\n-    void *pool;\n-#endif",
            "function_or_symbol": "struct page"
        });

        let resolved = resolve_single_location_two_sided(dir.path(), None, &parsed, &loc).unwrap();
        assert_eq!(resolved.file, "include/linux/mm.h");
        assert_eq!(resolved.line, 150);
        assert_eq!(resolved.end_line, 152);
        assert_eq!(resolved.side, Some("PARENT".to_string()));
        assert!(resolved.in_diff);
    }

    #[test]
    fn test_resolve_location_added_and_context_lines_marked_revision() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("fs/inode.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(
            &file_path,
            "void init(void) {\n    int x = 0;\n    mutex_init(&lock);\n    return;\n}\n",
        )
        .unwrap();

        let diff = r#"diff --git a/fs/inode.c b/fs/inode.c
--- a/fs/inode.c
+++ b/fs/inode.c
@@ -2,3 +2,4 @@ void init(void) {
     int x = 0;
+    mutex_init(&lock);
     return;
 }
"#;
        let parsed = parse_two_sided_diff(diff);

        // 1. Added line
        let added_loc = json!({
            "file": "fs/inode.c",
            "code_snippet": "+    mutex_init(&lock);",
            "line": 3
        });
        let resolved_added =
            resolve_single_location_two_sided(dir.path(), None, &parsed, &added_loc).unwrap();
        assert_eq!(resolved_added.line, 3);
        assert_eq!(resolved_added.side, Some("REVISION".to_string()));
        assert!(resolved_added.in_diff);

        // 2. Context line
        let ctx_loc = json!({
            "file": "fs/inode.c",
            "code_snippet": "int x = 0;",
            "line": 2
        });
        let resolved_ctx =
            resolve_single_location_two_sided(dir.path(), None, &parsed, &ctx_loc).unwrap();
        assert_eq!(resolved_ctx.line, 2);
        assert_eq!(resolved_ctx.side, Some("REVISION".to_string()));
        assert!(!resolved_ctx.in_diff);
    }

    #[test]
    fn test_resolve_finding_locations_decorates_side_in_json() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("net/core/dev.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(
            &file_path,
            "int dev_queue(void) {\n    new_call();\n    return 0;\n}\n",
        )
        .unwrap();

        let diff = r#"diff --git a/net/core/dev.c b/net/core/dev.c
--- a/net/core/dev.c
+++ b/net/core/dev.c
@@ -1,4 +1,4 @@
 int dev_queue(void) {
-    old_call();
+    new_call();
     return 0;
 }
"#;
        let mut findings = vec![
            json!({
                "problem": "Removal of old_call causes regression",
                "locations": [{
                    "file": "net/core/dev.c",
                    "code_snippet": "-    old_call();",
                    "line": 2
                }]
            }),
            json!({
                "problem": "new_call missing check",
                "locations": [{
                    "file": "net/core/dev.c",
                    "code_snippet": "new_call();",
                    "line": 2
                }]
            }),
        ];

        resolve_finding_locations(dir.path(), diff, &mut findings);

        assert_eq!(findings[0]["locations"][0]["side"], "PARENT");
        assert_eq!(findings[0]["locations"][0]["line"], 2);
        assert_eq!(findings[0]["locations"][0]["in_diff"], true);

        assert_eq!(findings[1]["locations"][0]["side"], "REVISION");
        assert_eq!(findings[1]["locations"][0]["line"], 2);
        assert_eq!(findings[1]["locations"][0]["in_diff"], true);
    }

    #[test]
    fn test_find_snippet_in_deleted_lines_with_blank_lines() {
        let deleted = vec![
            (10, "void cleanup(void)".to_string()),
            (11, "{".to_string()),
            (12, "".to_string()), // blank line
            (13, "    kfree(buf);".to_string()),
            (14, "}".to_string()),
        ];

        // Snippet that also contains a blank line
        let snip_with_blank = "void cleanup(void)\n{\n\n    kfree(buf);\n}";
        let res = find_snippet_in_deleted_lines(&deleted, snip_with_blank, None);
        assert_eq!(res, Some((10, 14)));

        // Snippet that omitted the blank line
        let snip_without_blank = "void cleanup(void)\n{\n    kfree(buf);\n}";
        let res2 = find_snippet_in_deleted_lines(&deleted, snip_without_blank, None);
        assert_eq!(res2, Some((10, 14)));
    }

    #[test]
    fn test_resolve_location_does_not_strip_unary_minus_when_matching_code() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("src/math.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(
            &file_path,
            "int negate(int y) {\n    int x = -y;\n    return x;\n}\n",
        )
        .unwrap();

        let diff = r#"diff --git a/src/math.c b/src/math.c
--- a/src/math.c
+++ b/src/math.c
@@ -1,4 +1,4 @@
 int negate(int y) {
-    int x = y;
+    int x = -y;
     return x;
 }
"#;
        let parsed = parse_two_sided_diff(diff);
        let loc = json!({
            "file": "src/math.c",
            "line": 2,
            "code_snippet": "    int x = -y;\n    return x;",
            "function_or_symbol": "negate"
        });

        let resolved = resolve_single_location_two_sided(dir.path(), None, &parsed, &loc).unwrap();
        assert_eq!(resolved.file, "src/math.c");
        assert_eq!(resolved.line, 2);
        assert_eq!(resolved.end_line, 3);
        assert_eq!(resolved.side, Some("REVISION".to_string()));
        assert!(resolved.in_diff);
    }

    #[test]
    fn test_resolve_location_unary_minus_verbatim_match_preserves_revision_margin() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("src/neg.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, "int x = 1;\nint z = -y;\n").unwrap();

        let diff = r#"diff --git a/src/neg.c b/src/neg.c
--- a/src/neg.c
+++ b/src/neg.c
@@ -1,2 +1,2 @@
 int x = 1;
-int z = 0;
+int z = -y;
"#;
        let parsed = parse_two_sided_diff(diff);
        let loc = json!({
            "file": "src/neg.c",
            "line": 2,
            "code_snippet": "-y;",
            "function_or_symbol": "z"
        });

        let resolved = resolve_single_location_two_sided(dir.path(), None, &parsed, &loc).unwrap();
        assert_eq!(resolved.file, "src/neg.c");
        assert_eq!(resolved.line, 2);
        assert_eq!(resolved.side, Some("REVISION".to_string()));
        assert!(resolved.in_diff);
    }

    #[test]
    fn test_find_snippet_in_deleted_lines_rejects_non_contiguous_lines() {
        let deleted = vec![
            (10, "int x = 1;".to_string()),
            // Separate hunk / deletion block across context lines
            (50, "int y = 2;".to_string()),
        ];
        let snip = "int x = 1;\nint y = 2;";
        assert_eq!(
            find_snippet_in_deleted_lines_exact(&deleted, snip, None),
            None
        );
        // Fallback should match the longest line ("int x = 1;" or "int y = 2;", both 10 chars)
        let fallback = find_snippet_in_deleted_lines_fallback(&deleted, snip, None);
        assert!(fallback.is_some());
        let (s, e) = fallback.unwrap();
        assert_eq!(s, e);
    }

    #[test]
    fn test_resolve_location_prefers_exact_multiline_match_over_partial_deleted() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("src/demo.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        // File on disk has a 3-line function at lines 20-22 outside the diff
        let mut content = String::new();
        for i in 1..=30 {
            if i == 20 {
                content.push_str("void existing_fn(void) {\n    kfree(ptr);\n}\n");
            } else if !(20..=22).contains(&i) {
                content.push_str(&format!("// line {i}\n"));
            }
        }
        std::fs::write(&file_path, &content).unwrap();

        // Diff has deleted line `kfree(ptr);` at line 5
        let diff = r#"diff --git a/src/demo.c b/src/demo.c
--- a/src/demo.c
+++ b/src/demo.c
@@ -4,3 +4,2 @@
 int a = 1;
-    kfree(ptr);
 int b = 2;
"#;
        let parsed = parse_two_sided_diff(diff);
        let loc = json!({
            "file": "src/demo.c",
            "code_snippet": "void existing_fn(void) {\n    kfree(ptr);\n}",
            "line": 20
        });

        let resolved = resolve_single_location_two_sided(dir.path(), None, &parsed, &loc).unwrap();
        // Must resolve to the exact 3-line match on REVISION at line 20,
        // NOT truncated to a 1-line match on PARENT at line 5!
        assert_eq!(resolved.file, "src/demo.c");
        assert_eq!(resolved.line, 20);
        assert_eq!(resolved.end_line, 22);
        assert_eq!(resolved.side, Some("REVISION".to_string()));
        assert!(!resolved.in_diff);
    }

    #[test]
    fn test_find_snippet_in_lines_fallback_tries_candidate_lines_in_descending_order() {
        let file_lines = vec![
            "void compute(void)",
            "{",
            "    int status = perform_calculation();",
            "    return status;",
            "}",
        ];
        // Snippet where the longest line diverged / is not in the file,
        // but the second-longest line is in the file.
        let snip = "    int very_long_diverged_variable_name_not_in_file = 0;\n    int status = perform_calculation();";
        let res = find_snippet_in_lines_fallback(&file_lines, snip, None, None, None);
        assert_eq!(res, Some((2, 2)));
    }

    #[test]
    fn test_resolve_location_renamed_file_resolves_deleted_and_added_lines() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("new_dir/worker.c");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(
            &file_path,
            "// header\nint process(void) {\n    new_logic();\n    return 0;\n}\n",
        )
        .unwrap();

        let diff = r#"diff --git a/old_dir/worker.c b/new_dir/worker.c
similarity index 85%
rename from old_dir/worker.c
rename to new_dir/worker.c
--- a/old_dir/worker.c
+++ b/new_dir/worker.c
@@ -2,3 +2,3 @@
 int process(void) {
-    old_logic();
+    new_logic();
     return 0;
"#;
        let parsed = parse_two_sided_diff(diff);

        // 1. Finding citing deleted code under post-rename path
        let deleted_loc = json!({
            "file": "new_dir/worker.c",
            "code_snippet": "-    old_logic();",
            "line": 3
        });
        let resolved_del =
            resolve_single_location_two_sided(dir.path(), None, &parsed, &deleted_loc).unwrap();
        assert_eq!(resolved_del.line, 3);
        assert_eq!(resolved_del.side, Some("PARENT".to_string()));
        assert!(resolved_del.in_diff);

        // 2. Finding citing added code under post-rename path
        let added_loc = json!({
            "file": "new_dir/worker.c",
            "code_snippet": "+    new_logic();",
            "line": 3
        });
        let resolved_added =
            resolve_single_location_two_sided(dir.path(), None, &parsed, &added_loc).unwrap();
        assert_eq!(resolved_added.line, 3);
        assert_eq!(resolved_added.side, Some("REVISION".to_string()));
        assert!(resolved_added.in_diff);
    }
}
