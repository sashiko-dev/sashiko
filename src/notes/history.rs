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

//! A unit's bug history (design sections 7.2 and 9): the fixes that touched
//! its files, found through the profile's fix patterns, read from
//! `git log <revision>` so only ancestors of the build revision are visible.
//! That bound is what keeps historic replay free of leakage: notes built at
//! a revision can never learn from a fix made after it.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::git_cmd;

/// Paths passed to one `git log` call.
const PATHS_PER_CALL: usize = 400;
/// Characters of a fix's message and diff given to the classifier.
const MESSAGE_CHARS: usize = 1500;
const DIFF_CHARS: usize = 3000;

/// One historic fix of a unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryCase {
    pub fix: String,
    pub time: i64,
    pub subject: String,
    /// The unit's files the fix touched.
    pub files: Vec<String>,
    /// The message, trimmed for the classifier.
    pub message: String,
    /// The diff restricted to the unit's files, trimmed for the classifier.
    pub diff: String,
}

/// The most recent fixes (at most `max_cases`) reachable from `revision`
/// that touch `files` and whose message matches one of `fix_patterns`
/// (extended regular expressions, matched by `git log --grep`).
pub async fn unit_history(
    repo: &Path,
    revision: &str,
    files: &[String],
    fix_patterns: &[String],
    max_cases: usize,
) -> Result<Vec<HistoryCase>> {
    if fix_patterns.is_empty() || files.is_empty() || max_cases == 0 {
        return Ok(Vec::new());
    }
    let mut seen = HashSet::new();
    let mut found: Vec<(i64, String, String)> = Vec::new();
    for chunk in files.chunks(PATHS_PER_CALL) {
        let mut cmd = git_cmd::in_dir_async(repo);
        cmd.args(["log", "--no-merges", "-E", "--format=%H%x1f%ct%x1f%s"]);
        cmd.arg(format!("--max-count={max_cases}"));
        for pattern in fix_patterns {
            cmd.arg(format!("--grep={pattern}"));
        }
        cmd.arg(revision).arg("--").args(chunk);
        let output = cmd.output().await.context("failed to run git log")?;
        if !output.status.success() {
            bail!(
                "git log for unit history failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut parts = line.splitn(3, '\u{1f}');
            let (Some(sha), Some(time), Some(subject)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if seen.insert(sha.to_string()) {
                found.push((
                    time.parse().unwrap_or(0),
                    sha.to_string(),
                    subject.to_string(),
                ));
            }
        }
    }
    // Newest first across chunks, then keep the window.
    found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    found.truncate(max_cases);

    let unit_set: HashSet<&str> = files.iter().map(String::as_str).collect();
    let mut cases = Vec::new();
    for (time, sha, subject) in found {
        let message = git_text(repo, &["log", "-1", "--format=%B", &sha]).await?;
        let mut diff_args = vec!["show", "--format=", "--no-color", sha.as_str(), "--"];
        let unit_files: Vec<&str> = files.iter().map(String::as_str).collect();
        // The diff is restricted to the unit's files; very large units fall
        // back to the whole diff rather than exceeding argument limits.
        if unit_files.len() <= PATHS_PER_CALL {
            diff_args.extend(unit_files);
        }
        let diff = git_text(repo, &diff_args).await?;
        let touched = git_text(
            repo,
            &["show", "--format=", "--name-only", "--no-renames", &sha],
        )
        .await?
        .lines()
        .filter(|path| unit_set.contains(path))
        .map(str::to_string)
        .collect();
        cases.push(HistoryCase {
            fix: sha,
            time,
            subject,
            files: touched,
            message: truncate_chars(&message, MESSAGE_CHARS),
            diff: truncate_chars(&diff, DIFF_CHARS),
        });
    }
    Ok(cases)
}

async fn git_text(repo: &Path, args: &[&str]) -> Result<String> {
    let output = git_cmd::in_dir_async(repo)
        .args(args)
        .output()
        .await
        .context("failed to run git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str("\n[truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::scope::test_repo;

    #[tokio::test]
    async fn test_unit_history_respects_revision_files_and_patterns() {
        let dir = tempfile::tempdir().unwrap();
        test_repo::commit_files(
            dir.path(),
            &[("net/a.c", "int a;\n"), ("mm/b.c", "int b;\n")],
        );
        let git = |args: &[&str]| {
            let out = crate::git_cmd::in_dir(dir.path())
                .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        };
        std::fs::write(dir.path().join("net/a.c"), "int a = 1;\n").unwrap();
        git(&[
            "commit",
            "-qam",
            "net: fix a\n\nFixes: 1234567890ab (\"net: add a\")",
        ]);
        let fix_sha = git(&["rev-parse", "HEAD"]).trim().to_string();
        std::fs::write(dir.path().join("mm/b.c"), "int b = 1;\n").unwrap();
        git(&[
            "commit",
            "-qam",
            "mm: fix b\n\nFixes: 1234567890ab (\"mm: add b\")",
        ]);
        std::fs::write(dir.path().join("net/a.c"), "int a = 2;\n").unwrap();
        git(&["commit", "-qam", "net: cleanup a"]);
        let build_rev = git(&["rev-parse", "HEAD"]).trim().to_string();
        std::fs::write(dir.path().join("net/a.c"), "int a = 3;\n").unwrap();
        git(&[
            "commit",
            "-qam",
            "net: later fix\n\nFixes: abcdef123456 (\"x\")",
        ]);

        let patterns = vec!["^Fixes: [0-9a-f]{8,}".to_string()];
        let files = vec!["net/a.c".to_string()];
        let cases = unit_history(dir.path(), &build_rev, &files, &patterns, 10)
            .await
            .unwrap();
        // Only the net fix reachable from the build revision qualifies: not
        // the mm fix (other files), not the cleanup (no pattern), not the
        // later fix (after the revision).
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].fix, fix_sha);
        assert_eq!(cases[0].subject, "net: fix a");
        assert_eq!(cases[0].files, vec!["net/a.c".to_string()]);
        assert!(cases[0].diff.contains("int a = 1;"));
        assert!(!cases[0].diff.contains("mm/b.c"));

        assert!(
            unit_history(dir.path(), &build_rev, &files, &[], 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_truncate_chars() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("abcdef", 3), "abc\n[truncated]");
    }
}
