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

    // 2. Fall back to reading from the physical worktree disk
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

/// Resolves a single finding location against the worktree file and diff hunks.
pub fn resolve_single_location(
    worktree_path: &Path,
    revision: Option<&str>,
    diff_ranges: &HashMap<String, Vec<(usize, usize)>>,
    location: &Value,
) -> Option<ResolvedLocation> {
    let raw_file = location.get("file").and_then(Value::as_str)?;
    let norm_file =
        normalize_file_path_with_worktree_and_rev(Some(worktree_path), revision, raw_file)?;

    let content = read_file_content(worktree_path, revision, &norm_file)?;
    let file_lines: Vec<&str> = content.lines().collect();
    if file_lines.is_empty() {
        return None;
    }

    let file_diff = diff_ranges.get(&norm_file);

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

    let symbol_line = if !sym.is_empty() {
        file_lines
            .iter()
            .enumerate()
            .find(|(_, line)| line.contains(sym))
            .map(|(i, _)| i + 1)
    } else {
        None
    };

    let matched_range = if !raw_snippet.is_empty() {
        find_snippet_in_lines(&file_lines, raw_snippet, hint_line, file_diff, symbol_line)
    } else {
        None
    };

    let hint_range = hint_line.filter(|&l| l <= file_lines.len()).map(|l| {
        let idx = l.saturating_sub(1);
        (idx, idx)
    });

    let (start_idx, end_idx) = if let Some(range) = matched_range {
        range
    } else if let Some(range) = hint_range {
        range
    } else if !sym.is_empty() {
        let sym_match = file_lines
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

    let start_line_str = file_lines[start_idx];
    let end_line_str = file_lines[end_idx];

    let start_line_chars = start_line_str.chars().count();
    let end_line_chars = end_line_str.chars().count();

    let start_indent_chars = start_line_str
        .chars()
        .take_while(|c| c.is_whitespace())
        .count();
    let end_trimmed_chars = end_line_str.trim_end().chars().count();

    let (start_char, safe_end_char) = if start_line == end_line {
        // Single-line range: Gerrit requires start_character < end_character
        // when start_line == end_line. Omit characters on empty lines to avoid 400 Bad Request.
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
        // Multi-line range: start_character applies to start_line, end_character applies to end_line.
        let s = start_indent_chars.min(start_line_chars);
        let e = if end_line_chars == 0 {
            0
        } else {
            end_trimmed_chars.max(1).min(end_line_chars)
        };
        (Some(s), Some(e))
    };

    let line_range = if start_line == end_line {
        format!("{start_line}")
    } else {
        format!("{start_line}-{end_line}")
    };

    let in_diff = diff_ranges
        .get(&norm_file)
        .map(|ranges| ranges.iter().any(|&(s, e)| start_idx <= e && end_idx >= s))
        .unwrap_or(false);

    let snippet_out = if raw_snippet.is_empty() {
        Some(file_lines[start_idx..=end_idx].join("\n"))
    } else {
        None
    };

    Some(ResolvedLocation {
        file: norm_file,
        line: start_line,
        end_line,
        line_range,
        start_character: start_char,
        end_character: safe_end_char,
        in_diff,
        code_snippet: snippet_out,
    })
}

/// Resolves and enriches finding locations using pre-parsed diff ranges.
pub fn resolve_finding_locations_with_ranges(
    worktree_path: &Path,
    revision: Option<&str>,
    diff_ranges: &HashMap<String, Vec<(usize, usize)>>,
    findings: &mut [Value],
) {
    for finding in findings.iter_mut() {
        if let Some(locs) = finding.get_mut("locations").and_then(Value::as_array_mut) {
            for loc in locs.iter_mut() {
                if let Some(resolved) =
                    resolve_single_location(worktree_path, revision, diff_ranges, loc)
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
                    if let Some(snip) = resolved.code_snippet {
                        map.insert("code_snippet".to_string(), json!(snip));
                    }
                }
            }
        }
    }
}

/// Resolves and enriches all finding locations in place using the target worktree and diff.
pub fn resolve_finding_locations(
    worktree_path: &Path,
    target_commit_diff: &str,
    findings: &mut [Value],
) {
    let diff_ranges = parse_diff_ranges(target_commit_diff);
    resolve_finding_locations_with_ranges(worktree_path, None, &diff_ranges, findings);
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
}
