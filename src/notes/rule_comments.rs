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

//! Comments that state rules ("must", "never", "only", "so that"), found
//! deterministically at the build revision. Authors write the rules that
//! matter most into such comments, and explorers given the list up front
//! miss fewer of them. The match is language-agnostic: any line that
//! starts like a comment in a common language and contains a rule word.

use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use regex::Regex;

use crate::git_cmd;

/// Paths passed to one `git grep` call.
const PATHS_PER_CALL: usize = 400;
/// Characters kept of one comment line.
const LINE_CHARS: usize = 200;

/// A cheap prefilter for `git grep`; the precise match is RULE_COMMENT.
const GREP_PATTERN: &str = "must|never|only|always|invariant|do not|don't|so that|caller";

/// A comment line (C, C++, Rust, Go, Java, shell, Python, SQL, Lisp and
/// assembler styles) containing a rule word.
static RULE_COMMENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)^\s*(//|/\*|\*|#|--|;).*\b(must|never|only|always|invariant|do not|don't|so that|callers?)\b",
    )
    .expect("valid regex")
});

/// Rule comments in `files` at `revision`, as "path:line: text", in file
/// order, at most `max` of them.
pub async fn rule_comments(
    repo: &Path,
    revision: &str,
    files: &[String],
    max: usize,
) -> Result<Vec<String>> {
    let mut found = Vec::new();
    for chunk in files.chunks(PATHS_PER_CALL) {
        if found.len() >= max {
            break;
        }
        let output = git_cmd::in_dir_async(repo)
            .args(["grep", "-n", "-I", "-i", "-E", GREP_PATTERN, revision, "--"])
            .args(chunk)
            .output()
            .await
            .context("failed to run git grep")?;
        // Exit status 1 means no match.
        if !output.status.success() && output.status.code() != Some(1) {
            bail!(
                "git grep for rule comments failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let prefix = format!("{revision}:");
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let line = line.strip_prefix(&prefix).unwrap_or(line);
            let mut parts = line.splitn(3, ':');
            let (Some(path), Some(lineno), Some(text)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if !RULE_COMMENT.is_match(text) {
                continue;
            }
            let text: String = text.trim().chars().take(LINE_CHARS).collect();
            found.push(format!("{path}:{lineno}: {text}"));
            if found.len() >= max {
                break;
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::scope::test_repo;

    #[tokio::test]
    async fn test_rule_comments_finds_comment_rules_only() {
        let dir = tempfile::tempdir().unwrap();
        let sha = test_repo::commit_files(
            dir.path(),
            &[
                (
                    "a.c",
                    "/* Callers must hold the lock. */\nint only = 1; /* not a leading comment */\n * never sleep here\nint x; // plain\n",
                ),
                (
                    "b.rs",
                    "// The cache is always flushed first.\nfn must() {}\n",
                ),
            ],
        );
        let files = vec!["a.c".to_string(), "b.rs".to_string()];
        let found = rule_comments(dir.path(), &sha, &files, 10).await.unwrap();
        assert_eq!(
            found,
            vec![
                "a.c:1: /* Callers must hold the lock. */",
                "a.c:3: * never sleep here",
                "b.rs:1: // The cache is always flushed first.",
            ]
        );
        assert_eq!(
            rule_comments(dir.path(), &sha, &files, 1)
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
