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

//! The project-wide fix corpus (design section 9.1): every fix reachable
//! from a revision, each with the code context a deep analysis needs. The
//! fix diff is shown with whole functions around every change, and the
//! change that introduced the bug is added when the profile can name it,
//! so the analysis reads code rather than only the commit message.

use std::path::Path;

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::git_cmd;
use crate::notes::history::{git_text, truncate_chars};

/// Characters of a fix's message, its function-context diff, and each
/// introducing change's diff given to the analysis.
const MESSAGE_CHARS: usize = 2000;
const FIX_DIFF_CHARS: usize = 12_000;
const INTRODUCED_DIFF_CHARS: usize = 4000;
/// Introducing changes shown per fix.
const MAX_INTRODUCED: usize = 2;

/// The change a fix names as having introduced its bug.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntroducedChange {
    pub commit: String,
    pub subject: String,
    /// The change restricted to the files the fix touched.
    pub diff: String,
}

/// One fix with its code context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusCase {
    pub fix: String,
    pub subject: String,
    pub message: String,
    /// The fix with whole functions around every change.
    pub diff: String,
    pub files: Vec<String>,
    #[serde(default)]
    pub introduced: Vec<IntroducedChange>,
}

/// Fixes reachable from `revision` whose message matches one of
/// `fix_patterns`, newest first, at most `limit` of them.
pub async fn list_fixes(
    repo: &Path,
    revision: &str,
    fix_patterns: &[String],
    limit: Option<usize>,
) -> Result<Vec<String>> {
    if fix_patterns.is_empty() {
        bail!("the profile has no history.fix_patterns, so fixes cannot be found");
    }
    let mut cmd = git_cmd::in_dir_async(repo);
    cmd.args(["log", "--no-merges", "-E", "--format=%H"]);
    if let Some(limit) = limit {
        cmd.arg(format!("--max-count={limit}"));
    }
    for pattern in fix_patterns {
        cmd.arg(format!("--grep={pattern}"));
    }
    cmd.arg(revision).arg("--");
    let output = cmd.output().await.context("failed to run git log")?;
    if !output.status.success() {
        bail!(
            "git log for the fix corpus failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

/// A deterministic pseudo-random subset of `fixes` of at most `size`,
/// ordered by a seeded hash so repeated runs pick the same cases.
pub fn sample(fixes: &[String], size: usize, seed: &str) -> Vec<String> {
    let mut keyed: Vec<([u8; 32], &String)> = fixes
        .iter()
        .map(|sha| {
            let mut hasher = Sha256::new();
            hasher.update(seed.as_bytes());
            hasher.update(sha.as_bytes());
            (hasher.finalize().into(), sha)
        })
        .collect();
    keyed.sort();
    keyed
        .into_iter()
        .take(size)
        .map(|(_, sha)| sha.clone())
        .collect()
}

/// The fixes of shard `index` out of `count`, by position.
pub fn shard(fixes: &[String], index: usize, count: usize) -> Vec<String> {
    fixes
        .iter()
        .enumerate()
        .filter(|(i, _)| count == 0 || i % count == index)
        .map(|(_, sha)| sha.clone())
        .collect()
}

/// Reads `fix` with its context. `introducer` is a multi-line regular
/// expression whose first capture group is the ID of an introducing
/// commit; IDs that do not resolve in `repo` are skipped.
pub async fn case_context(
    repo: &Path,
    fix: &str,
    introducer: Option<&Regex>,
) -> Result<CorpusCase> {
    let message = git_text(repo, &["log", "-1", "--format=%B", fix]).await?;
    let subject = message.lines().next().unwrap_or_default().to_string();
    let diff = git_text(
        repo,
        &["show", "--format=", "--no-color", "--function-context", fix],
    )
    .await?;
    let files: Vec<String> = git_text(
        repo,
        &["show", "--format=", "--name-only", "--no-renames", fix],
    )
    .await?
    .lines()
    .filter(|l| !l.is_empty())
    .map(str::to_string)
    .collect();

    let mut introduced = Vec::new();
    if let Some(re) = introducer {
        for id in introducer_ids(re, &message) {
            if introduced.len() == MAX_INTRODUCED {
                break;
            }
            let Some(commit) = resolve_commit(repo, &id).await else {
                continue;
            };
            if commit == fix
                || introduced
                    .iter()
                    .any(|c: &IntroducedChange| c.commit == commit)
            {
                continue;
            }
            let subject = git_text(repo, &["log", "-1", "--format=%s", &commit]).await?;
            let mut args = vec!["show", "--format=", "--no-color", "--function-context"];
            args.push(&commit);
            args.push("--");
            args.extend(files.iter().map(String::as_str));
            let diff = git_text(repo, &args).await?;
            introduced.push(IntroducedChange {
                commit,
                subject: subject.trim().to_string(),
                diff: truncate_chars(&diff, INTRODUCED_DIFF_CHARS),
            });
        }
    }
    Ok(CorpusCase {
        fix: fix.to_string(),
        subject,
        message: truncate_chars(&message, MESSAGE_CHARS),
        diff: truncate_chars(&diff, FIX_DIFF_CHARS),
        files,
        introduced,
    })
}

fn introducer_ids(re: &Regex, message: &str) -> Vec<String> {
    re.captures_iter(message)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect()
}

async fn resolve_commit(repo: &Path, id: &str) -> Option<String> {
    let output = git_cmd::in_dir_async(repo)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{id}^{{commit}}"),
        ])
        .output()
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::scope::test_repo;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = crate::git_cmd::in_dir(dir)
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
    }

    #[tokio::test]
    async fn test_case_context_reads_fix_and_introducer() {
        let dir = tempfile::tempdir().unwrap();
        test_repo::commit_files(dir.path(), &[("a.c", "int f(void)\n{\n\treturn 0;\n}\n")]);
        std::fs::write(
            dir.path().join("a.c"),
            "int f(void)\n{\n\tint x;\n\treturn x;\n}\n",
        )
        .unwrap();
        git(dir.path(), &["commit", "-qam", "a: add x"]);
        let bad = git(dir.path(), &["rev-parse", "HEAD"]).trim().to_string();
        std::fs::write(
            dir.path().join("a.c"),
            "int f(void)\n{\n\tint x = 0;\n\treturn x;\n}\n",
        )
        .unwrap();
        let msg = format!(
            "a: initialize x\n\nFixes: {} (\"a: add x\")\nFixes: deadbeefdeadbeef (\"gone\")",
            &bad[..12]
        );
        git(dir.path(), &["commit", "-qam", &msg]);
        let fix = git(dir.path(), &["rev-parse", "HEAD"]).trim().to_string();

        let re = Regex::new(r"(?m)^Fixes: ([0-9a-f]{8,})").unwrap();
        let case = case_context(dir.path(), &fix, Some(&re)).await.unwrap();
        assert_eq!(case.subject, "a: initialize x");
        assert_eq!(case.files, vec!["a.c"]);
        // Function context shows the whole function, not only the hunk.
        assert!(case.diff.contains("int f(void)"));
        assert_eq!(case.introduced.len(), 1);
        assert_eq!(case.introduced[0].commit, bad);
        assert_eq!(case.introduced[0].subject, "a: add x");

        let fixes = list_fixes(
            dir.path(),
            "HEAD",
            &["^Fixes: [0-9a-f]{8,}".to_string()],
            None,
        )
        .await
        .unwrap();
        assert_eq!(fixes, vec![fix]);
    }

    #[test]
    fn test_sample_and_shard_are_deterministic() {
        let fixes: Vec<String> = (0..20).map(|i| format!("{i:040}")).collect();
        let a = sample(&fixes, 5, "seed");
        assert_eq!(a.len(), 5);
        assert_eq!(a, sample(&fixes, 5, "seed"));
        assert_ne!(a, sample(&fixes, 5, "other"));
        assert_eq!(sample(&fixes, 50, "seed").len(), 20);

        let parts: Vec<Vec<String>> = (0..3).map(|i| shard(&fixes, i, 3)).collect();
        assert_eq!(parts.iter().map(Vec::len).sum::<usize>(), 20);
        assert_eq!(parts[1][0], fixes[1]);
        assert_eq!(shard(&fixes, 0, 1), fixes);
    }
}
