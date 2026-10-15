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

//! Deterministic evidence check (design section 7.4): every piece of
//! evidence a model cites must name a file that exists at the pinned
//! revision, a line range inside it, and a snippet that really appears
//! there. Models misremember line numbers by a few lines, so the snippet is
//! searched in a small window around the cited range; it must still match
//! line by line, whitespace-normalized.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::git_cmd;
use crate::notes::facts::Evidence;

/// How many lines around the cited range a snippet may be found in.
const LINE_TOLERANCE: usize = 5;
/// Snippet lines that only elide code and are not matched.
const ELISIONS: &[&str] = &["...", "…", "// ...", "/* ... */"];

/// The outcome of checking one piece of evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EvidenceCheck {
    Ok,
    MissingFile,
    BadRange {
        file_lines: usize,
    },
    SnippetMismatch {
        first_unmatched_line: String,
    },
    EmptySnippet,
    /// The snippet is verbatim code of the file, but at these lines rather
    /// than the cited ones.
    Relocated {
        start_line: usize,
        end_line: usize,
    },
}

impl EvidenceCheck {
    pub fn is_ok(&self) -> bool {
        matches!(self, EvidenceCheck::Ok | EvidenceCheck::Relocated { .. })
    }

    /// One-line explanation for the verifier and the logs.
    pub fn describe(&self) -> String {
        match self {
            EvidenceCheck::Ok => "ok".to_string(),
            EvidenceCheck::MissingFile => "file does not exist at the pinned revision".to_string(),
            EvidenceCheck::BadRange { file_lines } => {
                format!("line range is outside the file ({file_lines} lines)")
            }
            EvidenceCheck::SnippetMismatch {
                first_unmatched_line,
            } => format!(
                "snippet does not appear near the cited lines; first unmatched line: {first_unmatched_line:?}"
            ),
            EvidenceCheck::EmptySnippet => "snippet is empty".to_string(),
            EvidenceCheck::Relocated {
                start_line,
                end_line,
            } => format!("snippet found at lines {start_line}-{end_line}, not at the cited lines"),
        }
    }
}

/// Reads files at a pinned revision, caching each once.
pub struct RevisionFiles<'a> {
    repo: &'a Path,
    sha: &'a str,
    cache: HashMap<String, Option<Vec<String>>>,
}

impl<'a> RevisionFiles<'a> {
    pub fn new(repo: &'a Path, sha: &'a str) -> Self {
        Self {
            repo,
            sha,
            cache: HashMap::new(),
        }
    }

    /// The normalized lines of `path` at the revision, or `None` if it
    /// does not exist.
    async fn lines(&mut self, path: &str) -> Result<Option<&Vec<String>>> {
        if !self.cache.contains_key(path) {
            let content = read_at_revision(self.repo, self.sha, path).await?;
            let lines = content.map(|text| text.lines().map(normalize).collect());
            self.cache.insert(path.to_string(), lines);
        }
        Ok(self.cache.get(path).and_then(Option::as_ref))
    }

    /// Checks one piece of evidence.
    pub async fn check(&mut self, evidence: &Evidence) -> Result<EvidenceCheck> {
        let wanted: Vec<String> = evidence
            .snippet
            .lines()
            .map(normalize)
            .filter(|l| !l.is_empty() && !ELISIONS.contains(&l.as_str()))
            .collect();
        if wanted.is_empty() {
            return Ok(EvidenceCheck::EmptySnippet);
        }
        let path = evidence.path.trim().trim_start_matches("./");
        let Some(lines) = self.lines(path).await? else {
            return Ok(EvidenceCheck::MissingFile);
        };
        let start = evidence.start_line.max(1);
        let end = evidence.end_line.max(start);
        if start > lines.len() {
            return Ok(EvidenceCheck::BadRange {
                file_lines: lines.len(),
            });
        }
        let window_start = start.saturating_sub(1 + LINE_TOLERANCE);
        let window_end = end.saturating_add(LINE_TOLERANCE).min(lines.len());
        let check = match_in_order(&wanted, &lines[window_start..window_end]);
        if check.is_ok() {
            return Ok(check);
        }
        // Explorers often copy code verbatim but misremember where it is.
        // Verbatim code elsewhere in the file still backs the claim.
        Ok(
            relocate(&wanted, lines, start).map_or(check, |(start_line, end_line)| {
                EvidenceCheck::Relocated {
                    start_line,
                    end_line,
                }
            }),
        )
    }
}

/// Every wanted line must occur in the window, in order. A snippet line may
/// be a fragment of a longer source line.
fn match_in_order(wanted: &[String], window: &[String]) -> EvidenceCheck {
    let mut pos = 0;
    for line in wanted {
        match window[pos..].iter().position(|w| w.contains(line.as_str())) {
            Some(offset) => pos += offset + 1,
            None => {
                return EvidenceCheck::SnippetMismatch {
                    first_unmatched_line: line.clone(),
                };
            }
        }
    }
    EvidenceCheck::Ok
}

/// The 1-based line range of the compact in-order match of `wanted` in
/// `lines` that starts closest to `cited_start`. A match may span at most
/// twice the snippet's length plus a little, so scattered common lines do
/// not count as a match.
fn relocate(wanted: &[String], lines: &[String], cited_start: usize) -> Option<(usize, usize)> {
    let first = wanted.first()?;
    let max_span = wanted.len() * 2 + 10;
    let mut best: Option<(usize, usize)> = None;
    for (i, line) in lines.iter().enumerate() {
        if !line.contains(first.as_str()) {
            continue;
        }
        let limit = (i + max_span).min(lines.len());
        let mut pos = i + 1;
        let mut matched = true;
        for want in &wanted[1..] {
            match lines[pos..limit]
                .iter()
                .position(|l| l.contains(want.as_str()))
            {
                Some(offset) => pos += offset + 1,
                None => {
                    matched = false;
                    break;
                }
            }
        }
        if !matched {
            continue;
        }
        let candidate = (i + 1, pos);
        let closer = best.is_none_or(|(start, _)| {
            candidate.0.abs_diff(cited_start) < start.abs_diff(cited_start)
        });
        if closer {
            best = Some(candidate);
        }
    }
    best
}

/// Collapses runs of whitespace and trims the line.
fn normalize(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

async fn read_at_revision(repo: &Path, sha: &str, path: &str) -> Result<Option<String>> {
    if path.is_empty() || path.starts_with('-') || path.split('/').any(|p| p == "..") {
        return Ok(None);
    }
    let output = git_cmd::in_dir_async(repo)
        .args(["show", &format!("{sha}:{path}")])
        .output()
        .await
        .context("failed to run git show")?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::scope::test_repo;

    fn ev(path: &str, start: usize, end: usize, snippet: &str) -> Evidence {
        Evidence {
            path: path.to_string(),
            start_line: start,
            end_line: end,
            snippet: snippet.to_string(),
        }
    }

    #[tokio::test]
    async fn test_check_evidence_against_revision() {
        let dir = tempfile::tempdir().unwrap();
        let long: String = (0..20)
            .map(|i| format!("// line {i}\n"))
            .collect::<String>()
            + "fn twenty() {\n    20\n}\n";
        let sha = test_repo::commit_files(
            dir.path(),
            &[
                (
                    "src/cache.rs",
                    "use std::sync::Mutex;\n\npub fn get(m: &Mutex<u8>) -> u8 {\n    let guard = m.lock().unwrap();\n    *guard\n}\n",
                ),
                ("src/long.rs", &long),
            ],
        );
        let mut files = RevisionFiles::new(dir.path(), &sha);

        let exact = ev(
            "src/cache.rs",
            3,
            5,
            "pub fn get(m: &Mutex<u8>) -> u8 {\n  let guard =   m.lock().unwrap();",
        );
        assert_eq!(files.check(&exact).await.unwrap(), EvidenceCheck::Ok);

        // Off by a few lines, with an elision line.
        let shifted = ev(
            "src/cache.rs",
            1,
            1,
            "let guard = m.lock().unwrap();\n...\n*guard",
        );
        assert_eq!(files.check(&shifted).await.unwrap(), EvidenceCheck::Ok);

        let invented = ev("src/cache.rs", 3, 5, "let guard = m.try_lock();");
        assert!(matches!(
            files.check(&invented).await.unwrap(),
            EvidenceCheck::SnippetMismatch { .. }
        ));

        // Verbatim code far from the cited lines is found where it is.
        let far = ev("src/long.rs", 1, 2, "fn twenty() {\n    20\n}");
        assert_eq!(
            files.check(&far).await.unwrap(),
            EvidenceCheck::Relocated {
                start_line: 21,
                end_line: 23
            }
        );
        assert!(
            EvidenceCheck::Relocated {
                start_line: 5,
                end_line: 6
            }
            .is_ok()
        );

        // Lines in the wrong order do not match.
        let reordered = ev(
            "src/cache.rs",
            3,
            5,
            "*guard\nlet guard = m.lock().unwrap();",
        );
        assert!(!files.check(&reordered).await.unwrap().is_ok());

        assert_eq!(
            files.check(&ev("src/missing.rs", 1, 1, "x")).await.unwrap(),
            EvidenceCheck::MissingFile
        );
        assert_eq!(
            files.check(&ev("src/cache.rs", 90, 95, "x")).await.unwrap(),
            EvidenceCheck::BadRange { file_lines: 6 }
        );
        assert_eq!(
            files
                .check(&ev("src/cache.rs", 1, 1, "  \n...\n"))
                .await
                .unwrap(),
            EvidenceCheck::EmptySnippet
        );
        assert_eq!(
            files
                .check(&ev("../etc/passwd", 1, 1, "root"))
                .await
                .unwrap(),
            EvidenceCheck::MissingFile
        );
    }
}
