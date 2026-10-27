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

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::git_cmd;
use crate::notes::build::Usage;
use crate::notes::history::{git_text, truncate_chars};
use crate::workflow::{WorkflowEngine, WorkflowEnv};
use crate::workflows::code_notes::LessonKind;
use crate::workflows::notes_corpus::{
    CORPUS_BATCH_SIZE, CORPUS_STAGE_NAMES, CaseAnalysis, CorpusState, corpus_stage, corpus_workflow,
};

/// Characters of a fix's message, its function-context diff, and each
/// introducing change's diff given to the analysis.
const MESSAGE_CHARS: usize = 2000;
const FIX_DIFF_CHARS: usize = 12_000;
const INTRODUCED_DIFF_CHARS: usize = 4000;
/// Introducing changes shown per fix.
const MAX_INTRODUCED: usize = 2;
/// Length of the commit IDs cases are shown and cited by.
const FIX_ID_LEN: usize = 12;

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

/// Name of the file recording which corpus a directory holds.
const META_FILE: &str = "corpus-meta.json";

/// Which fixes a corpus directory holds. Every shard of one corpus must
/// agree, so shards of different corpora never mix in one directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusMeta {
    pub revision: String,
    pub fix_patterns: Vec<String>,
    pub limit: Option<usize>,
    pub sample: Option<usize>,
    pub seed: String,
}

impl CorpusMeta {
    /// The metadata in `dir`, if any.
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join(META_FILE);
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Ok(Some(serde_json::from_str(&text).with_context(|| {
            format!("failed to parse {}", path.display())
        })?))
    }

    /// Records this corpus in `dir`, or checks that `dir` already holds
    /// the same one.
    pub fn claim(&self, dir: &Path) -> Result<()> {
        if let Some(existing) = Self::load(dir)? {
            if &existing != self {
                bail!(
                    "{} holds another corpus ({existing:?}); use a different --out",
                    dir.display()
                );
            }
            return Ok(());
        }
        let tmp = dir.join(format!("{META_FILE}.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, dir.join(META_FILE))
            .with_context(|| format!("failed to record the corpus in {}", dir.display()))
    }
}

/// One analyzed case as stored in a corpus file (one JSON object per
/// line). The diff is not kept; it can be read again from the fix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusRecord {
    pub fix: String,
    pub subject: String,
    pub files: Vec<String>,
    pub introduced: Vec<String>,
    pub analysis: CaseAnalysis,
}

/// Settings of one corpus analysis run.
#[derive(Debug, Clone)]
pub struct CorpusConfig {
    pub project: String,
    pub out_file: PathBuf,
    pub temperature: f32,
    pub introducer: Option<Regex>,
}

/// What one corpus analysis run did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CorpusSummary {
    pub already_done: usize,
    pub analyzed: usize,
    pub code_bugs: usize,
    pub failed: usize,
    pub usage: Usage,
}

/// Fixes already analyzed in `path`.
pub fn read_done(path: &Path) -> Result<HashSet<String>> {
    if !path.exists() {
        return Ok(HashSet::new());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut done = HashSet::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        // A run killed mid-write leaves a torn last line; it is retried.
        match serde_json::from_str::<CorpusRecord>(line) {
            Ok(record) => {
                done.insert(record.fix);
            }
            Err(err) => warn!(
                "{}:{}: skipping unreadable record: {err}",
                path.display(),
                i + 1
            ),
        }
    }
    Ok(done)
}

/// Analyzes every fix in `fixes` not yet in the output file, appending
/// each round's records so an interrupted run resumes where it stopped.
/// Each case is attempted once per run; failed cases are retried by the
/// next run.
pub async fn analyze(
    repo: &Path,
    env: &WorkflowEnv<'_>,
    fixes: &[String],
    cfg: &CorpusConfig,
) -> Result<CorpusSummary> {
    let done = read_done(&cfg.out_file)?;
    let todo: Vec<&String> = fixes.iter().filter(|f| !done.contains(*f)).collect();
    let mut summary = CorpusSummary {
        already_done: fixes.len() - todo.len(),
        ..Default::default()
    };
    let per_round = CORPUS_BATCH_SIZE * CORPUS_STAGE_NAMES.len();
    let rounds = todo.len().div_ceil(per_round);
    for (round, chunk) in todo.chunks(per_round).enumerate() {
        let contexts = futures_util::future::join_all(
            chunk
                .iter()
                .map(|fix| case_context(repo, fix, cfg.introducer.as_ref())),
        )
        .await;
        let mut cases = Vec::new();
        for (fix, context) in chunk.iter().zip(contexts) {
            match context {
                Ok(case) => cases.push((fix_id(fix), case)),
                Err(err) => {
                    warn!("{fix}: cannot read the case: {err:#}");
                    summary.failed += 1;
                }
            }
        }
        let stages = cases
            .chunks(CORPUS_BATCH_SIZE)
            .zip(CORPUS_STAGE_NAMES)
            .map(|(batch, name)| corpus_stage(name, batch.to_vec(), cfg.temperature))
            .collect();
        let mut state = CorpusState {
            project: cfg.project.clone(),
            analyses: Vec::new(),
        };
        match WorkflowEngine::execute(&corpus_workflow(stages), env, &mut state, None).await {
            Ok(outcome) => summary.usage.add(&outcome),
            Err(err) => warn!("corpus round {} failed: {err:#}", round + 1),
        }
        let records = records_for(&cases, state.analyses);
        summary.failed += cases.len() - records.len();
        summary.analyzed += records.len();
        summary.code_bugs += records
            .iter()
            .filter(|r| r.analysis.kind == LessonKind::CodeBug)
            .count();
        append_records(&cfg.out_file, &records)?;
        info!(
            "corpus round {}/{rounds}: {} analyzed, {} failed so far; {} tokens in",
            round + 1,
            summary.analyzed,
            summary.failed,
            summary.usage.tokens_in
        );
    }
    Ok(summary)
}

fn fix_id(sha: &str) -> String {
    sha.chars().take(FIX_ID_LEN).collect()
}

/// Joins analyses to their cases by ID; analyses for unknown IDs and
/// cases without an analysis are dropped.
fn records_for(cases: &[(String, CorpusCase)], analyses: Vec<CaseAnalysis>) -> Vec<CorpusRecord> {
    let mut by_id: HashMap<String, CaseAnalysis> =
        analyses.into_iter().map(|a| (a.fix.clone(), a)).collect();
    cases
        .iter()
        .filter_map(|(id, case)| {
            by_id.remove(id).map(|analysis| CorpusRecord {
                fix: case.fix.clone(),
                subject: case.subject.clone(),
                files: case.files.clone(),
                introduced: case.introduced.iter().map(|c| c.commit.clone()).collect(),
                analysis,
            })
        })
        .collect()
}

fn append_records(path: &Path, records: &[CorpusRecord]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    // A torn last line from an interrupted run must not swallow the
    // first new record.
    let torn = std::fs::read(path).is_ok_and(|bytes| bytes.last().is_some_and(|b| *b != b'\n'));
    let mut text = if torn {
        "\n".to_string()
    } else {
        String::new()
    };
    for record in records {
        text.push_str(&serde_json::to_string(record)?);
        text.push('\n');
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("failed to append to {}", path.display()))
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
    fn test_records_join_by_id_and_resume() {
        let case = |sha: &str| CorpusCase {
            fix: sha.to_string(),
            subject: format!("fix {sha}"),
            message: String::new(),
            diff: String::new(),
            files: vec!["a.c".to_string()],
            introduced: vec![],
        };
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let cases = vec![(fix_id(&a), case(&a)), (fix_id(&b), case(&b))];
        let analysis = |id: &str| CaseAnalysis {
            fix: id.to_string(),
            kind: LessonKind::NotCodeBug,
            category: String::new(),
            violated_rule: String::new(),
            why_missed: String::new(),
            knowledge_needed: String::new(),
        };
        let records = records_for(&cases, vec![analysis(&fix_id(&b)), analysis("zzzz")]);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].fix, b);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corpus.jsonl");
        assert!(read_done(&path).unwrap().is_empty());
        append_records(&path, &records).unwrap();
        // A torn line from an interrupted run is skipped, not fatal.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"fix\": \"tor")
            .unwrap();
        assert_eq!(read_done(&path).unwrap(), HashSet::from([b.clone()]));
        let mut again = records.clone();
        again[0].fix = a.clone();
        append_records(&path, &again).unwrap();
        assert_eq!(read_done(&path).unwrap(), HashSet::from([a, b]));
    }

    #[test]
    fn test_corpus_meta_claim() {
        let dir = tempfile::tempdir().unwrap();
        let meta = CorpusMeta {
            revision: "abc".to_string(),
            fix_patterns: vec!["^Fixes: ".to_string()],
            limit: Some(10),
            sample: None,
            seed: "s".to_string(),
        };
        assert!(CorpusMeta::load(dir.path()).unwrap().is_none());
        meta.claim(dir.path()).unwrap();
        meta.claim(dir.path()).unwrap();
        assert_eq!(CorpusMeta::load(dir.path()).unwrap(), Some(meta.clone()));
        let other = CorpusMeta {
            revision: "def".to_string(),
            ..meta
        };
        assert!(format!("{:#}", other.claim(dir.path()).unwrap_err()).contains("another corpus"));
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
