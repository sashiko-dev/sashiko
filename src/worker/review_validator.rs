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

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tokio::fs;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RawIssue {
    pub file: String,
    pub compromised_line: String,
    pub approx_line: Option<usize>,
    pub issue: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedIssue {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub issue: String,
    pub snippet: String,
}

/// Resolves a list of raw issues against the worktree and validates they intersect with the diff.
pub async fn resolve_and_validate_snippets(
    issues: Vec<RawIssue>,
    worktree_path: &Path,
    diff_ranges: &HashMap<String, Vec<(usize, usize)>>,
) -> (Vec<ResolvedIssue>, Vec<String>) {
    let mut resolved = Vec::new();
    let mut errors = Vec::new();

    for raw in issues {
        match resolve_single_issue(&raw, worktree_path, diff_ranges).await {
            Ok(res) => resolved.push(res),
            Err(e) => errors.push(format!("Issue in {}: {}", raw.file, e)),
        }
    }

    (resolved, errors)
}

async fn resolve_single_issue(
    raw: &RawIssue,
    worktree_path: &Path,
    diff_ranges: &HashMap<String, Vec<(usize, usize)>>,
) -> Result<ResolvedIssue> {
    let file_path = worktree_path.join(&raw.file);
    if !file_path.exists() {
        anyhow::bail!("File '{}' not found in worktree.", raw.file);
    }

    let content = fs::read_to_string(&file_path)
        .await
        .context(format!("Failed to read file '{}'", raw.file))?;
    let lines: Vec<&str> = content.lines().collect();

    let snippet_lines: Vec<String> = raw
        .compromised_line
        .lines()
        .map(normalize_line)
        .filter(|l| !l.is_empty())
        .collect();

    if snippet_lines.is_empty() {
        anyhow::bail!("The provided code snippet for file '{}' is empty.", raw.file);
    }

    // 1. Fuzzy match snippet to find line numbers
    let mut found_range = None;

    // Search around approx_line first if provided
    let mut indices: Vec<usize> = (0..lines.len()).collect();
    if let Some(approx) = raw.approx_line {
        let approx_0 = approx.saturating_sub(1);
        indices.sort_by_key(|&i| (i as isize - approx_0 as isize).abs());
    }

    for &i in &indices {
        if match_at(&lines, i, &snippet_lines) {
            found_range = Some((i, i + snippet_lines.len() - 1));
            break;
        }
    }

    let (start_0, end_0) = found_range.ok_or_else(|| {
        anyhow::anyhow!(
            "Could not find the exact code snippet in file '{}'. Ensure 'compromised_line' matches the source code exactly.",
            raw.file
        )
    })?;

    // 2. Check if it intersects with diff ranges
    let ranges = diff_ranges.get(&raw.file).ok_or_else(|| {
        anyhow::anyhow!(
            "The file '{}' was not modified by this patch. Comments must only address modified code.",
            raw.file
        )
    })?;

    let intersects = ranges.iter().any(|(r_start, r_end)| {
        let max_start = std::cmp::max(start_0, *r_start);
        let min_end = std::cmp::min(end_0, *r_end);
        max_start <= min_end
    });

    if !intersects {
        anyhow::bail!(
            "The snippet in '{}' was found at lines {}-{}, but this range was not modified by the patch. Please only comment on code changed in this patch.",
            raw.file,
            start_0 + 1,
            end_0 + 1
        );
    }

    Ok(ResolvedIssue {
        file: raw.file.clone(),
        start_line: start_0 + 1,
        end_line: end_0 + 1,
        issue: raw.issue.clone(),
        snippet: raw.compromised_line.clone(),
    })
}

fn normalize_line(line: &str) -> String {
    // Aggressive normalization: remove all whitespace and leading +/- diff markers
    let mut s = line.trim();
    if (s.starts_with('+') || s.starts_with('-')) && (s.len() == 1 || s.chars().nth(1).unwrap().is_whitespace()) {
        s = s[1..].trim();
    }
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn match_at(lines: &[&str], start_idx: usize, snippet_lines: &[String]) -> bool {
    if start_idx + snippet_lines.len() > lines.len() {
        return false;
    }

    for (i, snippet_line) in snippet_lines.iter().enumerate() {
        if normalize_line(lines[start_idx + i]) != *snippet_line {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use std::fs;

    #[tokio::test]
    async fn test_resolve_and_validate() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test.c");
        fs::write(&file_path, "int main() {\n    int x = 1;\n    return 0;\n}\n").unwrap();

        let mut diff_ranges = HashMap::new();
        diff_ranges.insert("test.c".to_string(), vec![(1, 1)]); // Only line 2 (0-indexed 1) modified

        let issues = vec![
            RawIssue {
                file: "test.c".to_string(),
                compromised_line: "int x = 1;".to_string(),
                approx_line: Some(2),
                issue: "Problem here".to_string(),
            },
            RawIssue {
                file: "test.c".to_string(),
                compromised_line: "return 0;".to_string(),
                approx_line: Some(3),
                issue: "This should fail because not in diff".to_string(),
            }
        ];

        let (resolved, errors) = resolve_and_validate_snippets(issues, dir.path(), &diff_ranges).await;

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].start_line, 2);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("not modified by the patch"));
    }
}
