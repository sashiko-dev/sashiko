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

//! Deterministic history collection for `history` review stages.
//!
//! Reads the git history of the code a patch changes: the commits that last
//! wrote the modified and deleted lines, the commits that introduced
//! identifiers the patch removes, and the commits that later fixed or
//! reverted any of them. With the `server` feature it also fetches the
//! mailing list threads where the most relevant of those commits were
//! reviewed. What counts as code and where reviews happened differ per
//! project, so both come from the project's [`HistoryProfile`]. See
//! `designs/DESIGN_CODE_HISTORY_REVIEW.md`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::time::Duration;

use tokio::time::timeout;
use tracing::{debug, warn};

use crate::prior_context::sanitize_untrusted_discussion_text;
use crate::project::ProjectId;
use crate::worker::fixes_check::{HunkBlameTarget, extract_diff_blame_targets};

/// Where a project's patches were reviewed before they were merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscussionSource {
    /// Mailing lists archived on lore.kernel.org. Threads are fetched by
    /// Message-ID and by patch ID across all archives; `lists` names the
    /// archives a `Link:` trailer may point at.
    Lore { lists: &'static [&'static str] },
}

/// Per-project settings for history collection.
#[derive(Debug, Clone, Copy)]
pub struct HistoryProfile {
    /// Directory prefixes whose history is not collected.
    skipped_dirs: &'static [&'static str],
    /// File names whose history is not collected.
    skipped_files: &'static [&'static str],
    /// Where to look for review threads, or `None` to use git history only.
    discussions: Option<DiscussionSource>,
}

/// Tests, documentation, and build files change with nearly every QEMU
/// feature, so their history would crowd out the history of the code.
const QEMU_PROFILE: HistoryProfile = HistoryProfile {
    skipped_dirs: &["tests/", "docs/"],
    skipped_files: &["meson.build", "Kconfig", "MAINTAINERS", "meson_options.txt"],
    discussions: Some(DiscussionSource::Lore {
        lists: &[
            "qemu-devel",
            "qemu-riscv",
            "qemu-arm",
            "qemu-rust",
            "qemu-trivial",
        ],
    }),
};

impl HistoryProfile {
    /// The profile of a project whose review workflow has a `history` stage.
    /// Projects without one return `None`, and nothing is collected for them.
    pub fn for_project(project: ProjectId) -> Option<Self> {
        match project {
            ProjectId::Qemu => Some(QEMU_PROFILE),
            ProjectId::Linux | ProjectId::Sashiko | ProjectId::Gcc | ProjectId::Zephyr => None,
        }
    }

    /// Whether the history of `path` can constrain the code under review.
    fn is_code_path(&self, path: &str) -> bool {
        let name = path.rsplit('/').next().unwrap_or(path);
        !self.skipped_dirs.iter().any(|dir| path.starts_with(dir))
            && !self.skipped_files.contains(&name)
    }
}

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_GIT_STDOUT: usize = 512 * 1024;
const COMMIT_FORMAT: &str = "--format=%H%x00%h%x00%cs%x00%s";
/// Commits read per modified hunk.
const REGION_LOG_LIMIT: &str = "6";
/// Commits read per run of deleted lines.
const REMOVED_LOG_LIMIT: &str = "3";
/// Identifiers removed by the patch whose origin is looked up.
const MAX_REMOVED_IDENTIFIERS: usize = 3;
/// Removed identifiers checked against the post-image before giving up.
const MAX_IDENTIFIER_PROBES: usize = 12;
const MIN_IDENTIFIER_LEN: usize = 6;
const MAX_FOLLOW_UPS_PER_COMMIT: usize = 3;
/// Runs of deleted lines looked up across the whole patch.
const MAX_DELETED_RUNS: usize = 12;
/// Highest-ranked commits whose later fixes and reverts are looked up.
const MAX_FOLLOW_UP_SOURCES: usize = 12;
/// Wall-clock bound on commit discovery. Each git command has its own
/// timeout, but a patch with many hunks would otherwise chain them.
const DISCOVERY_BUDGET: Duration = Duration::from_secs(120);
const MAX_HISTORY_COMMITS: usize = 12;
const MAX_MESSAGE_BYTES: usize = 2048;
const MAX_EXCERPT_BYTES: usize = 1536;
const MAX_HISTORY_BYTES: usize = 24 * 1024;

/// A commit shown in `<history_context>`, as git reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedCommit {
    /// Abbreviated SHA (at least 12 characters), as printed in the block.
    pub sha: String,
    pub subject: String,
}

impl RenderedCommit {
    /// Whether `sha` (any case, at least 7 hex characters) names this
    /// commit.
    pub fn matches(&self, sha: &str) -> bool {
        let sha = sha.trim().to_ascii_lowercase();
        sha.len() >= 7 && (self.sha.starts_with(&sha) || sha.starts_with(&self.sha))
    }
}

/// History gathered for one target commit, rendered for the `history` stage.
#[derive(Debug, Clone, Default)]
pub struct CodeHistoryContext {
    /// Body of the `<history_context>` block.
    pub rendered: String,
    /// Body of the `<history_discussions>` block, or empty.
    pub discussions: String,
    /// The commits that `rendered` shows, in its order.
    pub commits: Vec<RenderedCommit>,
    /// Message-IDs of the mails in `discussions`.
    pub discussion_msgids: Vec<String>,
}

/// How a historical commit relates to the code the patch changes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Relation {
    IntroducedRemovedLines { file: String },
    IntroducedRemovedIdentifier { ident: String },
    FixedBy { sha: String },
    RevertedBy { sha: String },
    Fixes { sha: String },
    Reverts { sha: String },
    ModifiedRegion { file: String },
}

impl Relation {
    /// Lower ranks are more likely to explain why the patched code exists.
    fn rank(&self) -> u8 {
        match self {
            Relation::IntroducedRemovedLines { .. }
            | Relation::IntroducedRemovedIdentifier { .. } => 0,
            Relation::FixedBy { .. } | Relation::RevertedBy { .. } => 1,
            Relation::Fixes { .. } | Relation::Reverts { .. } => 2,
            Relation::ModifiedRegion { .. } => 3,
        }
    }

    fn label(&self) -> String {
        match self {
            Relation::IntroducedRemovedLines { file } => {
                format!("last wrote lines this patch deletes in {file}")
            }
            Relation::IntroducedRemovedIdentifier { ident } => {
                format!("introduced `{ident}`, which this patch removes")
            }
            Relation::FixedBy { sha } => format!("later fixed by {sha}"),
            Relation::RevertedBy { sha } => format!("later reverted by {sha}"),
            Relation::Fixes { sha } => format!("fixes {sha}"),
            Relation::Reverts { sha } => format!("reverts {sha}"),
            Relation::ModifiedRegion { file } => {
                format!("modified the region this patch changes in {file}")
            }
        }
    }
}

#[derive(Debug, Clone)]
struct HistoryCommit {
    full_sha: String,
    abbrev_sha: String,
    date: String,
    subject: String,
    relations: BTreeSet<Relation>,
}

impl HistoryCommit {
    fn rank(&self) -> u8 {
        self.relations
            .iter()
            .map(Relation::rank)
            .min()
            .unwrap_or(u8::MAX)
    }
}

/// Commits keyed by full SHA, in the order they were first found.
#[derive(Default)]
struct CommitSet {
    order: Vec<String>,
    commits: BTreeMap<String, HistoryCommit>,
}

impl CommitSet {
    fn add(&mut self, info: CommitInfo, relation: Relation) {
        let entry = self
            .commits
            .entry(info.full_sha.clone())
            .or_insert_with(|| {
                self.order.push(info.full_sha.clone());
                HistoryCommit {
                    full_sha: info.full_sha,
                    abbrev_sha: info.abbrev_sha,
                    date: info.date,
                    subject: info.subject,
                    relations: BTreeSet::new(),
                }
            });
        entry.relations.insert(relation);
    }

    fn snapshot(&self) -> Vec<HistoryCommit> {
        self.order
            .iter()
            .filter_map(|sha| self.commits.get(sha).cloned())
            .collect()
    }

    /// Commits ordered by relation rank, then newest first, then discovery.
    fn ranked(&self) -> Vec<HistoryCommit> {
        let mut commits = self.snapshot();
        commits.sort_by(|a, b| a.rank().cmp(&b.rank()).then_with(|| b.date.cmp(&a.date)));
        commits
    }
}

#[derive(Debug, Clone)]
struct CommitInfo {
    full_sha: String,
    abbrev_sha: String,
    date: String,
    subject: String,
}

/// Collects the history of the code `diff_only` changes, as of the parent of
/// `target_commit_sha`, using the profile of `project`. Returns `None` when
/// the project has no profile, the patch modifies no existing code lines,
/// or the parent cannot be resolved.
pub async fn collect_code_history(
    project: ProjectId,
    worktree: &Path,
    target_commit_sha: &str,
    series_shas: &[String],
    diff_only: &str,
) -> Option<CodeHistoryContext> {
    let profile = HistoryProfile::for_project(project)?;
    // The target reaches git as a revision argument; accepting only an
    // object name keeps a value such as "--output=x" from being read as an
    // option. Every other revision passed to git comes from git output.
    if !is_object_name(target_commit_sha) {
        warn!(
            "code history: {:?} is not a commit object name",
            target_commit_sha
        );
        return None;
    }
    let (files, targets) = extract_diff_blame_targets(diff_only);
    let files: Vec<String> = files
        .into_iter()
        .filter(|f| profile.is_code_path(f))
        .collect();
    let targets: Vec<HunkBlameTarget> = targets
        .into_iter()
        .filter(|t| profile.is_code_path(&t.pre_file))
        .collect();
    if targets.is_empty() {
        debug!("code history: patch modifies no existing code lines");
        return None;
    }
    let parent = resolve_parent(worktree, target_commit_sha).await?;
    let mut set = CommitSet::default();
    let discovery = async {
        collect_region_history(worktree, &parent, &targets, series_shas, &mut set).await;
        collect_removed_identifiers(
            worktree,
            &parent,
            target_commit_sha,
            &files,
            diff_only,
            series_shas,
            &mut set,
        )
        .await;
        collect_follow_ups(worktree, &parent, series_shas, &mut set).await;
    };
    if timeout(DISCOVERY_BUDGET, discovery).await.is_err() {
        warn!(
            "code history: discovery exceeded {:?}; using the commits found so far",
            DISCOVERY_BUDGET
        );
    }
    if set.commits.is_empty() {
        return None;
    }

    let mut ranked = set.ranked();
    ranked.truncate(MAX_HISTORY_COMMITS);
    let (rendered, shown) = render_history(worktree, &files, &ranked).await;
    // Discussions and the commit list cover only commits the block shows.
    ranked.truncate(shown);
    let mut context = CodeHistoryContext {
        rendered,
        commits: ranked
            .iter()
            .map(|c| RenderedCommit {
                sha: c.abbrev_sha.clone(),
                subject: c.subject.clone(),
            })
            .collect(),
        ..Default::default()
    };
    if let Some(source) = profile.discussions {
        attach_discussions(project, source, worktree, &ranked, &mut context).await;
    }
    Some(context)
}

#[cfg(feature = "server")]
async fn attach_discussions(
    project: ProjectId,
    source: DiscussionSource,
    worktree: &Path,
    ranked: &[HistoryCommit],
    context: &mut CodeHistoryContext,
) {
    let (text, msgids) = match source {
        DiscussionSource::Lore { lists } => {
            discussions::collect(project, lists, worktree, ranked).await
        }
    };
    context.discussions = text;
    context.discussion_msgids = msgids;
}

/// Fetching review threads needs the lore client, which only the `server`
/// build has; the minimal build reviews with git history alone.
#[cfg(not(feature = "server"))]
async fn attach_discussions(
    _project: ProjectId,
    _source: DiscussionSource,
    _worktree: &Path,
    _ranked: &[HistoryCommit],
    _context: &mut CodeHistoryContext,
) {
}

/// Whether `rev` is an abbreviated or full object name: 7 to 64 hexadecimal
/// digits.
fn is_object_name(rev: &str) -> bool {
    (7..=64).contains(&rev.len()) && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

async fn run_git(worktree: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = crate::git_cmd::in_dir_async(worktree);
    cmd.args(args);
    cmd.kill_on_drop(true);
    let output = timeout(
        GIT_TIMEOUT,
        crate::toolbox::command::capped_output_with_limit(&mut cmd, MAX_GIT_STDOUT),
    )
    .await;
    match output {
        Ok(Ok(out)) if out.is_usable() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(Ok(out)) => {
            debug!(
                "code history: git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            None
        }
        Ok(Err(e)) => {
            warn!("code history: git {:?} could not run: {}", args, e);
            None
        }
        Err(_) => {
            warn!("code history: git {:?} timed out", args);
            None
        }
    }
}

async fn resolve_parent(worktree: &Path, target: &str) -> Option<String> {
    let rev = format!("{target}^");
    let out = run_git(worktree, &["rev-parse", "--verify", "--quiet", &rev]).await?;
    let sha = out.trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

fn parse_commit_lines(stdout: &str, series_shas: &[String]) -> Vec<CommitInfo> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\0');
            let full_sha = parts.next()?.trim().to_ascii_lowercase();
            let abbrev_sha = parts.next()?.trim().to_ascii_lowercase();
            let date = parts.next()?.trim().to_string();
            let subject = parts.next()?.trim().to_string();
            // SHA-1 object names have 40 hex digits, SHA-256 names 64.
            let well_formed = matches!(full_sha.len(), 40 | 64)
                && full_sha.bytes().all(|b| b.is_ascii_hexdigit());
            (well_formed && !series_shas.contains(&full_sha)).then_some(CommitInfo {
                full_sha,
                abbrev_sha,
                date,
                subject,
            })
        })
        .collect()
}

async fn log_line_range(
    worktree: &Path,
    parent: &str,
    file: &str,
    start: usize,
    end: usize,
    limit: &str,
    series_shas: &[String],
) -> Vec<CommitInfo> {
    let range = format!("{start},{end}:{file}");
    let args = [
        "log",
        "--no-patch",
        "--abbrev=12",
        COMMIT_FORMAT,
        "-n",
        limit,
        "-L",
        &range,
        parent,
    ];
    run_git(worktree, &args)
        .await
        .map(|out| parse_commit_lines(&out, series_shas))
        .unwrap_or_default()
}

/// Contiguous runs of deleted line numbers.
fn deleted_runs(deleted: &HashSet<usize>) -> Vec<(usize, usize)> {
    let mut lines: Vec<usize> = deleted.iter().copied().collect();
    lines.sort_unstable();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for line in lines {
        match runs.last_mut() {
            Some((_, end)) if *end + 1 == line => *end = line,
            _ => runs.push((line, line)),
        }
    }
    runs
}

async fn collect_region_history(
    worktree: &Path,
    parent: &str,
    targets: &[HunkBlameTarget],
    series_shas: &[String],
    set: &mut CommitSet,
) {
    let mut runs_left = MAX_DELETED_RUNS;
    for target in targets {
        let file = &target.pre_file;
        let runs = deleted_runs(&target.deleted_lines);
        let take = runs.len().min(runs_left);
        runs_left -= take;
        for (start, end) in runs.into_iter().take(take) {
            let found = log_line_range(
                worktree,
                parent,
                file,
                start,
                end,
                REMOVED_LOG_LIMIT,
                series_shas,
            )
            .await;
            for info in found {
                set.add(
                    info,
                    Relation::IntroducedRemovedLines { file: file.clone() },
                );
            }
        }
        let found = log_line_range(
            worktree,
            parent,
            file,
            target.start_line,
            target.end_line,
            REGION_LOG_LIMIT,
            series_shas,
        )
        .await;
        for info in found {
            set.add(info, Relation::ModifiedRegion { file: file.clone() });
        }
    }
}

/// Identifiers that appear on deleted lines and on no added line, longest
/// first. Short or all-lowercase words are skipped because they are mostly
/// comment prose or local variables.
fn removed_only_identifiers(diff_only: &str) -> Vec<String> {
    let mut removed = String::new();
    let mut added = String::new();
    for line in diff_only.lines() {
        if line.starts_with("---") || line.starts_with("+++") {
            continue;
        }
        if let Some(rest) = line.strip_prefix('-') {
            removed.push_str(rest);
            removed.push('\n');
        } else if let Some(rest) = line.strip_prefix('+') {
            added.push_str(rest);
            added.push('\n');
        }
    }
    let added_ids = crate::notes::maintainer_notes::source_identifiers_all(&added);
    let mut ids: Vec<String> = crate::notes::maintainer_notes::source_identifiers_all(&removed)
        .into_iter()
        .filter(|id| !added_ids.contains(id))
        .filter(|id| id.len() >= MIN_IDENTIFIER_LEN)
        .filter(|id| id.chars().any(|c| c == '_' || c.is_ascii_uppercase()))
        .collect();
    ids.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    ids
}

async fn collect_removed_identifiers(
    worktree: &Path,
    parent: &str,
    target: &str,
    files: &[String],
    diff_only: &str,
    series_shas: &[String],
    set: &mut CommitSet,
) {
    let mut found = 0;
    for ident in removed_only_identifiers(diff_only)
        .into_iter()
        .take(MAX_IDENTIFIER_PROBES)
    {
        if found == MAX_REMOVED_IDENTIFIERS {
            break;
        }
        if still_present(worktree, target, files, &ident).await {
            continue;
        }
        let pickaxe = format!("-S{ident}");
        let mut args = vec![
            "log",
            "--no-patch",
            "--abbrev=12",
            COMMIT_FORMAT,
            "--reverse",
            &pickaxe,
            parent,
            "--",
        ];
        args.extend(files.iter().map(String::as_str));
        let Some(out) = run_git(worktree, &args).await else {
            continue;
        };
        if let Some(info) = parse_commit_lines(&out, series_shas).into_iter().next() {
            set.add(
                info,
                Relation::IntroducedRemovedIdentifier {
                    ident: ident.clone(),
                },
            );
            found += 1;
        }
    }
}

/// Whether `ident` still occurs as a word in `files` at `target`.
async fn still_present(worktree: &Path, target: &str, files: &[String], ident: &str) -> bool {
    let mut args = vec!["grep", "-l", "-w", "-F", "-e", ident, target, "--"];
    args.extend(files.iter().map(String::as_str));
    run_git(worktree, &args)
        .await
        .is_some_and(|out| !out.trim().is_empty())
}

/// How a later commit message refers to an earlier commit.
#[derive(Debug, PartialEq, Eq)]
enum FollowUpKind {
    Fix,
    Revert,
}

/// Classifies `body` as a fix or revert of the commit `full_sha`. Any
/// abbreviation of at least 8 characters counts, as trailer styles vary.
fn follow_up_kind(body: &str, full_sha: &str) -> Option<FollowUpKind> {
    let names_commit = |text: &str| {
        text.split(|c: char| !c.is_ascii_hexdigit())
            .any(|tok| tok.len() >= 8 && full_sha.starts_with(&tok.to_ascii_lowercase()))
    };
    for line in body.lines().map(str::trim) {
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("this reverts commit") && names_commit(line) {
            return Some(FollowUpKind::Revert);
        }
        if lower.starts_with("fixes:") && names_commit(line) {
            return Some(FollowUpKind::Fix);
        }
    }
    None
}

async fn collect_follow_ups(
    worktree: &Path,
    parent: &str,
    series_shas: &[String],
    set: &mut CommitSet,
) {
    for commit in set.ranked().into_iter().take(MAX_FOLLOW_UP_SOURCES) {
        let range = format!("{}..{}", commit.full_sha, parent);
        let grep = format!("--grep={}", &commit.full_sha[..8]);
        let args = [
            "log",
            "--no-patch",
            "--abbrev=12",
            "--format=%H%x00%h%x00%cs%x00%s%x00%B%x1e",
            "-F",
            &grep,
            &range,
        ];
        let Some(out) = run_git(worktree, &args).await else {
            continue;
        };
        let mut added = 0;
        for record in out.split('\x1e') {
            if added == MAX_FOLLOW_UPS_PER_COMMIT {
                break;
            }
            let Some((info, body)) = parse_follow_up_record(record, series_shas) else {
                continue;
            };
            let Some(kind) = follow_up_kind(&body, &commit.full_sha) else {
                continue;
            };
            let (forward, backward) = match kind {
                FollowUpKind::Fix => (
                    Relation::FixedBy {
                        sha: info.abbrev_sha.clone(),
                    },
                    Relation::Fixes {
                        sha: commit.abbrev_sha.clone(),
                    },
                ),
                FollowUpKind::Revert => (
                    Relation::RevertedBy {
                        sha: info.abbrev_sha.clone(),
                    },
                    Relation::Reverts {
                        sha: commit.abbrev_sha.clone(),
                    },
                ),
            };
            set.add(info, backward);
            if let Some(original) = set.commits.get_mut(&commit.full_sha) {
                original.relations.insert(forward);
            }
            added += 1;
        }
    }
}

fn parse_follow_up_record(record: &str, series_shas: &[String]) -> Option<(CommitInfo, String)> {
    let record = record.trim_start_matches('\n');
    let mut parts = record.splitn(5, '\0');
    let head = [parts.next()?, parts.next()?, parts.next()?, parts.next()?].join("\0");
    let body = parts.next().unwrap_or_default().to_string();
    let info = parse_commit_lines(&head, series_shas).into_iter().next()?;
    Some((info, body))
}

fn truncated(text: &str, max_bytes: usize) -> String {
    let text = text.trim_end();
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let prefix = crate::utils::utf8_prefix(text, max_bytes);
    format!(
        "{prefix}\n[... {} bytes truncated]",
        text.len() - prefix.len()
    )
}

async fn render_commit(worktree: &Path, files: &[String], commit: &HistoryCommit) -> String {
    let message = run_git(worktree, &["log", "-1", "--format=%B", &commit.full_sha])
        .await
        .unwrap_or_default();
    let mut show_args = vec![
        "show",
        "--format=",
        "--no-color",
        "-U3",
        &commit.full_sha,
        "--",
    ];
    show_args.extend(files.iter().map(String::as_str));
    let excerpt = run_git(worktree, &show_args).await.unwrap_or_default();
    let relations: Vec<String> = commit.relations.iter().map(Relation::label).collect();
    let excerpt = if excerpt.trim().is_empty() {
        "(does not touch the files this patch modifies)".to_string()
    } else {
        truncated(&excerpt, MAX_EXCERPT_BYTES)
    };
    // Commit messages and diffs are author-controlled text.
    sanitize_untrusted_discussion_text(&format!(
        "=== commit {} (\"{}\") {} ===\nRelation: {}\n\n{}\n\nDiff in the files this patch modifies:\n{}\n",
        commit.abbrev_sha,
        commit.subject,
        commit.date,
        relations.join("; "),
        truncated(&message, MAX_MESSAGE_BYTES),
        excerpt
    ))
}

/// Renders `ranked` in order until the size limit, returning the text and
/// how many leading commits it shows.
async fn render_history(
    worktree: &Path,
    files: &[String],
    ranked: &[HistoryCommit],
) -> (String, usize) {
    let mut out = String::new();
    for (idx, commit) in ranked.iter().enumerate() {
        let section = render_commit(worktree, files, commit).await;
        if out.len() + section.len() > MAX_HISTORY_BYTES {
            out.push_str(&format!(
                "[... {} more commits omitted to stay within the size limit]\n",
                ranked.len() - idx
            ));
            return (out, idx);
        }
        out.push_str(&section);
        out.push('\n');
    }
    (out, ranked.len())
}

#[cfg(feature = "server")]
mod discussions {
    //! Lore threads where the most relevant historical commits were reviewed.

    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::time::Duration;

    use regex::Regex;
    use serde::{Deserialize, Serialize};
    use tokio::io::AsyncWriteExt;
    use tracing::{debug, warn};

    use super::{HistoryCommit, run_git, truncated};
    use crate::mbox::{LoreMboxClient, split_mbox};
    use crate::patch::{clean_subject, extract_changelog_from_body, get_subject_prefixes};
    use crate::prior_context::sanitize_untrusted_discussion_text;
    use crate::project::ProjectId;

    /// Commits whose review threads are fetched.
    const MAX_DISCUSSED_COMMITS: usize = 4;
    const MAX_THREAD_BYTES: usize = 6 * 1024;
    const MAX_DISCUSSION_BYTES: usize = 16 * 1024;
    const MAX_COVER_BYTES: usize = 1536;
    /// Quoted lines kept before each reply paragraph.
    const QUOTE_CONTEXT_LINES: usize = 3;
    const LORE_REQUEST_GAP: Duration = Duration::from_secs(1);
    const CACHE_VERSION: &str = "v1";

    #[derive(Serialize, Deserialize)]
    struct CachedThread {
        text: String,
        msgids: Vec<String>,
    }

    /// Fetches, filters, and renders the review threads of the commits most
    /// likely to explain the patched code. Returns the rendered block body
    /// and the Message-IDs of the mails in it.
    pub(super) async fn collect(
        project: ProjectId,
        lists: &[&str],
        worktree: &Path,
        ranked: &[HistoryCommit],
    ) -> (String, Vec<String>) {
        let mut out = String::new();
        let mut msgids = Vec::new();
        let selected = ranked.iter().filter(|c| c.rank() <= 1);
        for commit in selected.take(MAX_DISCUSSED_COMMITS) {
            let Some(thread) = thread_for_commit(project, lists, worktree, commit).await else {
                continue;
            };
            if thread.text.is_empty() {
                continue;
            }
            if out.len() + thread.text.len() > MAX_DISCUSSION_BYTES {
                break;
            }
            out.push_str(&thread.text);
            out.push('\n');
            msgids.extend(thread.msgids);
        }
        (out, msgids)
    }

    fn cache_path(project: ProjectId, full_sha: &str) -> Option<PathBuf> {
        let dir = crate::utils::data_home()
            .ok()?
            .join("sashiko")
            .join("code-history")
            .join(CACHE_VERSION)
            .join(project.as_str());
        Some(dir.join(format!("{full_sha}.json")))
    }

    async fn thread_for_commit(
        project: ProjectId,
        lists: &[&str],
        worktree: &Path,
        commit: &HistoryCommit,
    ) -> Option<CachedThread> {
        let path = cache_path(project, &commit.full_sha);
        if let Some(path) = &path
            && let Ok(raw) = tokio::fs::read(path).await
            && let Ok(cached) = serde_json::from_slice::<CachedThread>(&raw)
        {
            return Some(cached);
        }
        let fetched = fetch_thread_for_commit(lists, worktree, commit).await?;
        if fetched.complete
            && let Some(path) = path
        {
            store_cache(path, &fetched.thread).await;
        }
        Some(fetched.thread)
    }

    async fn store_cache(path: PathBuf, thread: &CachedThread) {
        let Ok(json) = serde_json::to_vec(thread) else {
            return;
        };
        let shown = path.clone();
        match tokio::task::spawn_blocking(move || write_atomically(&path, &json)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("code history: cannot write cache {:?}: {}", shown, e),
            Err(e) => warn!("code history: cache write for {:?} failed: {}", shown, e),
        }
    }

    /// Writes `data` through a temporary file in the same directory and
    /// renames it into place, so a concurrent review never reads a partial
    /// cache entry.
    fn write_atomically(path: &Path, data: &[u8]) -> std::io::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| std::io::Error::other("cache path has no parent directory"))?;
        std::fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        std::io::Write::write_all(&mut tmp, data)?;
        tmp.persist(path).map_err(|e| e.error)?;
        Ok(())
    }

    /// A thread rendered from lore and whether every request behind it
    /// succeeded. Lore answers a patch ID search that has no match with an
    /// error status the client does not tell apart from a failure, so only
    /// a thread whose requests all succeeded is known to be final and may
    /// be cached.
    struct FetchedThread {
        thread: CachedThread,
        complete: bool,
    }

    /// Fetches the merged version's thread by Message-ID and every version
    /// carrying the same diff by patch ID. `None` means the lore client
    /// could not be built.
    async fn fetch_thread_for_commit(
        lists: &[&str],
        worktree: &Path,
        commit: &HistoryCommit,
    ) -> Option<FetchedThread> {
        let client = match LoreMboxClient::new() {
            Ok(client) => client,
            Err(e) => {
                warn!("code history: cannot build lore client: {:#}", e);
                return None;
            }
        };
        let mut mboxes: Vec<Vec<u8>> = Vec::new();
        let mut complete = true;
        if let Some(msgid) = commit_message_id(lists, worktree, &commit.full_sha).await {
            match client.fetch_thread(&msgid).await {
                Ok(raw) => mboxes.push(raw),
                Err(e) => {
                    warn!("code history: lore thread {} failed: {:#}", msgid, e);
                    complete = false;
                }
            }
            tokio::time::sleep(LORE_REQUEST_GAP).await;
        }
        match commit_patch_id(worktree, &commit.full_sha).await {
            Some(patch_id) => {
                match client.search_patch_id(&patch_id).await {
                    Ok(raw) => mboxes.push(raw),
                    Err(e) => {
                        debug!(
                            "code history: lore patchid:{} returned nothing: {:#}",
                            patch_id, e
                        );
                        complete = false;
                    }
                }
                tokio::time::sleep(LORE_REQUEST_GAP).await;
            }
            None => complete = false,
        }
        // Splitting and parsing up to several MiB of mail is CPU-bound.
        let owned = commit.clone();
        let rendered = tokio::task::spawn_blocking(move || {
            let raw_mails: Vec<Vec<u8>> = mboxes.iter().flat_map(|m| split_mbox(m)).collect();
            render_thread(&owned, &raw_mails)
        })
        .await;
        match rendered {
            Ok(thread) => Some(FetchedThread { thread, complete }),
            Err(e) => {
                warn!(
                    "code history: parsing lore mail for {} failed: {}",
                    commit.abbrev_sha, e
                );
                None
            }
        }
    }

    /// The Message-ID from a `Message-ID` trailer, or from a `Link:` into
    /// one of the project's lore archives.
    async fn commit_message_id(lists: &[&str], worktree: &Path, full_sha: &str) -> Option<String> {
        let ids = run_git(
            worktree,
            &[
                "log",
                "-1",
                "--format=%(trailers:key=Message-ID,valueonly,separator=%x00)",
                full_sha,
            ],
        )
        .await
        .unwrap_or_default();
        if let Some(id) = ids.split('\0').map(normalize_msgid).find(|s| !s.is_empty()) {
            return Some(id);
        }
        let links = run_git(
            worktree,
            &[
                "log",
                "-1",
                "--format=%(trailers:key=Link,valueonly,separator=%x00)",
                full_sha,
            ],
        )
        .await?;
        links
            .split('\0')
            .find_map(|link| lore_link_msgid(link, lists))
    }

    fn normalize_msgid(raw: &str) -> String {
        raw.trim()
            .trim_start_matches('<')
            .trim_end_matches('>')
            .trim()
            .to_string()
    }

    /// The Message-ID in a lore URL whose archive is one of `lists` (or the
    /// archive-neutral `r` and `all`). Segments with percent escapes are
    /// skipped: the URL parser leaves them encoded, and the patch ID search
    /// still finds the thread.
    fn lore_link_msgid(link: &str, lists: &[&str]) -> Option<String> {
        let url = reqwest::Url::parse(link.trim()).ok()?;
        if url.host_str() != Some("lore.kernel.org") {
            return None;
        }
        let mut segments = url.path_segments()?;
        let archive = segments.next()?;
        if !(archive == "r" || archive == "all" || lists.contains(&archive)) {
            return None;
        }
        segments
            .find(|seg| seg.contains('@') && !seg.contains('%'))
            .map(normalize_msgid)
    }

    /// The stable patch ID of the commit's diff.
    async fn commit_patch_id(worktree: &Path, full_sha: &str) -> Option<String> {
        let diff = run_git(worktree, &["show", "--format=", "--no-color", full_sha]).await?;
        // run_git stops reading at MAX_GIT_STDOUT; a cut diff has a
        // different patch ID, which would find nothing or the wrong thread.
        if diff.len() >= super::MAX_GIT_STDOUT {
            debug!(
                "code history: diff of {} too large for a patch ID",
                full_sha
            );
            return None;
        }
        let mut cmd = crate::git_cmd::in_dir_async(worktree);
        cmd.args(["patch-id", "--stable"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().ok()?;
        let mut stdin = child.stdin.take()?;
        let writer = tokio::spawn(async move {
            let _ = stdin.write_all(diff.as_bytes()).await;
        });
        let output = tokio::time::timeout(super::GIT_TIMEOUT, child.wait_with_output())
            .await
            .ok()?
            .ok()?;
        let _ = writer.await;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let id = stdout.split_whitespace().next()?.to_ascii_lowercase();
        (id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
    }

    /// The role of a mail in a review thread.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum MailKind {
        /// A posting of a patch, carrying its diff.
        Posting,
        /// A series cover letter, top level or sent in reply to an earlier
        /// version.
        Cover,
        /// A reply, which may quote or suggest diffs.
        Reply,
    }

    struct ThreadMail {
        msgid: String,
        author: String,
        subject: String,
        in_reply_to: Option<String>,
        body: String,
        kind: MailKind,
    }

    fn parse_mails(raw_mails: &[Vec<u8>]) -> Vec<ThreadMail> {
        let mut seen = HashSet::new();
        raw_mails
            .iter()
            .filter_map(|raw| crate::patch::parse_email(raw).ok())
            .filter(|(meta, _)| seen.insert(meta.message_id.clone()))
            .map(|(meta, _)| {
                // parse_email marks replies (including replies that carry
                // an inline diff) as neither patch nor cover letter.
                let kind = if !meta.is_patch_or_cover {
                    MailKind::Reply
                } else if crate::patch::has_unquoted_diff(&meta.body) {
                    MailKind::Posting
                } else {
                    MailKind::Cover
                };
                ThreadMail {
                    msgid: meta.message_id,
                    author: meta.author,
                    subject: meta.subject,
                    in_reply_to: meta.in_reply_to,
                    body: meta.body,
                    kind,
                }
            })
            .collect()
    }

    fn is_pull(subject: &str) -> bool {
        get_subject_prefixes(subject)
            .iter()
            .any(|p| p.eq_ignore_ascii_case("pull"))
    }

    /// Mails about this commit: postings of the patch and replies to them
    /// (same cleaned subject, never a pull request), plus the cover letters
    /// those postings reply to.
    fn select_mails(commit_subject: &str, mails: Vec<ThreadMail>) -> Vec<ThreadMail> {
        let wanted = clean_subject(commit_subject);
        let (matching, rest): (Vec<_>, Vec<_>) = mails
            .into_iter()
            .partition(|m| clean_subject(&m.subject) == wanted && !is_pull(&m.subject));
        let parents: HashSet<String> = matching
            .iter()
            .filter(|m| m.kind == MailKind::Posting)
            .filter_map(|m| m.in_reply_to.clone())
            .collect();
        let mut selected: Vec<ThreadMail> = rest
            .into_iter()
            .filter(|m| {
                m.kind == MailKind::Cover && parents.contains(&m.msgid) && !is_pull(&m.subject)
            })
            .collect();
        selected.extend(matching);
        let mut bodies = HashSet::new();
        selected.retain(|m| bodies.insert(m.body.trim().to_string()));
        selected
    }

    fn is_tag_line(line: &str) -> bool {
        let lower = line.trim().to_ascii_lowercase();
        ["reviewed-by:", "acked-by:", "tested-by:", "signed-off-by:"]
            .iter()
            .any(|tag| lower.starts_with(tag))
    }

    /// Whether line `idx` is a quote attribution ("On <date>, X wrote:"),
    /// which mail clients often wrap onto a second line.
    fn is_attribution(lines: &[&str], idx: usize) -> bool {
        let trimmed = lines[idx].trim();
        if trimmed.ends_with("wrote:") {
            return true;
        }
        trimmed.starts_with("On ")
            && lines
                .get(idx + 1)
                .is_some_and(|next| next.trim().ends_with("wrote:"))
    }

    /// Reply prose with the signature and attribution lines removed and only
    /// `QUOTE_CONTEXT_LINES` non-empty quoted lines kept before each reply
    /// paragraph.
    fn reply_text(body: &str) -> (String, Vec<String>) {
        let mut out = String::new();
        let mut tags = Vec::new();
        let mut quoted: Vec<&str> = Vec::new();
        let lines: Vec<&str> = body.lines().collect();
        for (idx, line) in lines.iter().copied().enumerate() {
            if line == "-- " {
                break;
            }
            let trimmed = line.trim();
            if trimmed.starts_with('>') {
                if !trimmed.trim_start_matches(['>', ' ']).is_empty() {
                    quoted.push(line);
                }
                continue;
            }
            if is_attribution(&lines, idx) {
                continue;
            }
            if is_tag_line(trimmed) {
                tags.push(trimmed.to_string());
                continue;
            }
            if trimmed.is_empty() {
                // Blank lines keep pending quotes for the next paragraph.
                if quoted.is_empty() && !out.is_empty() {
                    out.push('\n');
                }
                continue;
            }
            let skip = quoted.len().saturating_sub(QUOTE_CONTEXT_LINES);
            for q in &quoted[skip..] {
                out.push_str(q);
                out.push('\n');
            }
            quoted.clear();
            out.push_str(line);
            out.push('\n');
        }
        (out.trim().to_string(), tags)
    }

    /// Diffstat lines (" path | 12 ++--", " 3 files changed, ...").
    fn is_diffstat_line(line: &str) -> bool {
        static DIFFSTAT: OnceLock<Regex> = OnceLock::new();
        let re = DIFFSTAT.get_or_init(|| {
            // Safety invariant: static regex literal is syntactically valid.
            Regex::new(r"^\s*(\S.*\|\s+(\d+\s*[+-]*|Bin .*)|\d+ files? changed.*)\s*$")
                .expect("valid diffstat regex")
        });
        re.is_match(line)
    }

    /// The notes a poster wrote below the `---` line, without the diffstat.
    fn posting_notes(body: &str) -> Option<String> {
        let notes = extract_changelog_from_body(body)?;
        let kept: Vec<&str> = notes.lines().filter(|l| !is_diffstat_line(l)).collect();
        let kept = kept.join("\n").trim().to_string();
        (!kept.is_empty()).then_some(kept)
    }

    /// The cover letter prose, cut at the shortlog ("Name (N):") that
    /// git format-patch appends.
    fn cover_text(body: &str) -> String {
        static SHORTLOG: OnceLock<Regex> = OnceLock::new();
        let re = SHORTLOG.get_or_init(|| {
            // Safety invariant: static regex literal is syntactically valid.
            Regex::new(r"^\S.* \(\d+\):$").expect("valid shortlog regex")
        });
        let prose: Vec<&str> = body.lines().take_while(|l| !re.is_match(l)).collect();
        truncated(&prose.join("\n"), MAX_COVER_BYTES)
    }

    fn mail_text(mail: &ThreadMail) -> String {
        match mail.kind {
            MailKind::Posting => match posting_notes(&mail.body) {
                Some(notes) => format!("(posting of the patch) Notes below the --- line:\n{notes}"),
                None => "(posting of the patch, no notes below the --- line)".to_string(),
            },
            MailKind::Cover => format!("(cover letter)\n{}", cover_text(&mail.body)),
            MailKind::Reply => {
                let (prose, tags) = reply_text(&mail.body);
                match (prose.is_empty(), tags.is_empty()) {
                    (true, true) => "(empty reply)".to_string(),
                    (true, false) => format!("(tags only) {}", tags.join(", ")),
                    (false, true) => prose,
                    (false, false) => format!("{prose}\n{}", tags.join("\n")),
                }
            }
        }
    }

    fn render_thread(commit: &HistoryCommit, raw_mails: &[Vec<u8>]) -> CachedThread {
        let mails = select_mails(&commit.subject, parse_mails(raw_mails));
        if mails.is_empty() {
            return CachedThread {
                text: String::new(),
                msgids: Vec::new(),
            };
        }
        let mut text = format!(
            "=== review of commit {} (\"{}\") ===\n",
            commit.abbrev_sha, commit.subject
        );
        for mail in &mails {
            let section = format!(
                "--- From: {} | Subject: {} | Message-ID: {}\n{}\n",
                mail.author,
                mail.subject,
                mail.msgid,
                mail_text(mail)
            );
            text.push_str(&sanitize_untrusted_discussion_text(&section));
        }
        let text = truncated(&text, MAX_THREAD_BYTES);
        // Only mails whose header survived truncation can be cited.
        let msgids = mails
            .iter()
            .filter(|m| text.contains(&format!("| Message-ID: {}\n", m.msgid)))
            .map(|m| m.msgid.clone())
            .collect();
        CachedThread { text, msgids }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn reply_text_keeps_context_and_folds_tags() {
            let body = "On Mon, Jan 1, X wrote:\n> a\n> b\n> c\n> d\n\nThis breaks VS mode.\n\nReviewed-by: A <a@b>\n-- \nsig\n";
            let (prose, tags) = reply_text(body);
            assert_eq!(prose, "> b\n> c\n> d\nThis breaks VS mode.");
            assert_eq!(tags, vec!["Reviewed-by: A <a@b>"]);
        }

        fn mail(msgid: &str, subject: &str, in_reply_to: Option<&str>, body: &str) -> Vec<u8> {
            let reply = in_reply_to
                .map(|id| format!("In-Reply-To: <{id}>\nReferences: <{id}>\n"))
                .unwrap_or_default();
            format!(
                "From: Dev <dev@example.org>\nDate: Mon, 2 Mar 2026 10:00:00 +0000\n\
                 Subject: {subject}\nMessage-ID: <{msgid}>\n{reply}\n{body}"
            )
            .into_bytes()
        }

        const DIFF: &str = "diff --git a/target/riscv/csr.c b/target/riscv/csr.c\n\
            --- a/target/riscv/csr.c\n+++ b/target/riscv/csr.c\n@@ -1,1 +1,1 @@\n-a\n+b\n";

        #[test]
        fn render_thread_classifies_postings_covers_and_replies() {
            let subject = "target/riscv: keep the mask";
            let raw = vec![
                mail(
                    "cover-v2@x",
                    "[PATCH v2 0/2] riscv mask fixes",
                    Some("cover-v1@x"),
                    "v2: keep bit 13 after review.\n\nDev (2):\n  a\n  b\n",
                ),
                mail(
                    "patch-v2@x",
                    &format!("[PATCH v2 1/2] {subject}"),
                    Some("cover-v2@x"),
                    &format!(
                        "Keep it.\n\nSigned-off-by: Dev <dev@example.org>\n---\nv2: rebased\n\n{DIFF}"
                    ),
                ),
                mail(
                    "reply@x",
                    &format!("Re: [PATCH v2 1/2] {subject}"),
                    Some("patch-v2@x"),
                    &format!("> -a\n> +b\n\nThis drops LCOFI; try instead:\n\n{DIFF}"),
                ),
                mail(
                    "pull@x",
                    &format!("[PULL 3/9] {subject}"),
                    Some("pull-cover@x"),
                    &format!("Keep it.\n---\n{DIFF}"),
                ),
                mail(
                    "other@x",
                    "[PATCH v2 2/2] target/riscv: something else",
                    Some("cover-v2@x"),
                    &format!("Other.\n---\n{DIFF}"),
                ),
            ];
            let commit = HistoryCommit {
                full_sha: "0".repeat(40),
                abbrev_sha: "000000000000".to_string(),
                date: "2026-03-02".to_string(),
                subject: subject.to_string(),
                relations: std::collections::BTreeSet::new(),
            };
            let thread = render_thread(&commit, &raw);
            assert_eq!(thread.msgids, vec!["cover-v2@x", "patch-v2@x", "reply@x"]);
            assert!(
                thread
                    .text
                    .contains("(cover letter)\nv2: keep bit 13 after review.")
            );
            assert!(
                thread
                    .text
                    .contains("(posting of the patch) Notes below the --- line:\nv2: rebased")
            );
            assert!(thread.text.contains("This drops LCOFI; try instead:"));
            assert!(!thread.text.contains("pull@x"));
            assert!(!thread.text.contains("other@x"));
        }

        #[test]
        fn reply_text_drops_wrapped_attribution_and_empty_quotes() {
            let body = "On Sat, Oct 24, 2026 at 2:19 PM Someone\n<s@x.com> wrote:\n> int x;\n>\n> 2.43.0\n>\nThis is my way\n";
            let (prose, _) = reply_text(body);
            assert_eq!(prose, "> int x;\n> 2.43.0\nThis is my way");
        }

        #[test]
        fn posting_notes_drop_diffstat() {
            let body = "msg\n---\nv2: keep the check\n\n target/riscv/csr.c | 4 ++--\n 1 file changed, 2 insertions(+), 2 deletions(-)\n\ndiff --git a/x b/x\n";
            assert_eq!(posting_notes(body), Some("v2: keep the check".to_string()));
            let only_stat =
                "msg\n---\n target/riscv/csr.c | 4 ++--\n 1 file changed\n\ndiff --git a/x b/x\n";
            assert_eq!(posting_notes(only_stat), None);
        }

        #[test]
        fn cover_text_stops_at_shortlog() {
            let body = "Why this series.\n\nv2: rebased\n\nA Person (2):\n  x: y\n";
            assert_eq!(cover_text(body), "Why this series.\n\nv2: rebased");
        }

        #[test]
        fn lore_link_msgid_accepts_only_project_archives() {
            let lists = ["qemu-devel", "qemu-riscv"];
            assert_eq!(
                lore_link_msgid(
                    "https://lore.kernel.org/qemu-devel/20261024061616.27381-8-p@q.com",
                    &lists
                ),
                Some("20261024061616.27381-8-p@q.com".to_string())
            );
            assert_eq!(
                lore_link_msgid("https://lore.kernel.org/r/a@b", &lists),
                Some("a@b".to_string())
            );
            assert_eq!(
                lore_link_msgid("https://lore.kernel.org/linux-mm/a@b", &lists),
                None
            );
            assert_eq!(
                lore_link_msgid("https://example.com/qemu-devel/a@b", &lists),
                None
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_object_name_rejects_options_and_names() {
        assert!(is_object_name("0123456"));
        assert!(is_object_name(&"a".repeat(40)));
        assert!(is_object_name(&"a".repeat(64)));
        assert!(!is_object_name("012345"));
        assert!(!is_object_name("--output=x"));
        assert!(!is_object_name("HEAD"));
        assert!(!is_object_name(&"a".repeat(65)));
    }

    #[test]
    fn parse_commit_lines_accepts_sha1_and_sha256_and_skips_series() {
        let sha1 = "a".repeat(40);
        let sha256 = "b".repeat(64);
        let series = "c".repeat(40);
        let stdout = format!(
            "{sha1}\0aaaaaaaaaaaa\02026-01-01\0x: one\n\
             {sha256}\0bbbbbbbbbbbb\02026-01-02\0x: two\n\
             {series}\0cccccccccccc\02026-01-03\0x: series\n\
             {}\0dddddddddddd\02026-01-04\0x: short\n\
             not a record\n",
            "d".repeat(39)
        );
        let parsed = parse_commit_lines(&stdout, &[series]);
        let subjects: Vec<&str> = parsed.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, vec!["x: one", "x: two"]);
        assert_eq!(parsed[1].abbrev_sha, "bbbbbbbbbbbb");
    }

    #[test]
    fn rendered_commit_matches_abbreviations_either_way() {
        let commit = RenderedCommit {
            sha: "0123456789ab".to_string(),
            subject: "x: y".to_string(),
        };
        assert!(commit.matches("0123456"));
        assert!(commit.matches("0123456789AB"));
        assert!(commit.matches("0123456789abcdef0123"));
        assert!(!commit.matches("012345"));
        assert!(!commit.matches("0123456789ac"));
    }

    #[test]
    fn qemu_profile_skips_tests_docs_and_build_files() {
        let Some(profile) = HistoryProfile::for_project(ProjectId::Qemu) else {
            panic!("QEMU has a history profile");
        };
        assert!(profile.is_code_path("target/riscv/tcg/csr.c"));
        assert!(!profile.is_code_path("tests/tcg/riscv64/system/meson.build"));
        assert!(!profile.is_code_path("docs/system/riscv/virt.rst"));
        assert!(!profile.is_code_path("hw/riscv/meson.build"));
        assert!(!profile.is_code_path("MAINTAINERS"));
    }

    #[test]
    fn projects_without_history_stage_have_no_profile() {
        for project in [
            ProjectId::Linux,
            ProjectId::Sashiko,
            ProjectId::Gcc,
            ProjectId::Zephyr,
        ] {
            assert!(HistoryProfile::for_project(project).is_none());
        }
    }

    #[test]
    fn deleted_runs_groups_contiguous_lines() {
        let lines: HashSet<usize> = [3, 4, 5, 9, 11, 12].into_iter().collect();
        assert_eq!(deleted_runs(&lines), vec![(3, 5), (9, 9), (11, 12)]);
    }

    #[test]
    fn follow_up_kind_reads_fixes_and_reverts() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            follow_up_kind("x\n\nFixes: 0123456789ab (\"y\")\n", sha),
            Some(FollowUpKind::Fix)
        );
        assert_eq!(
            follow_up_kind(
                "This reverts commit 0123456789abcdef0123456789abcdef01234567.\n",
                sha
            ),
            Some(FollowUpKind::Revert)
        );
        assert_eq!(follow_up_kind("See commit 0123456789ab.\n", sha), None);
    }

    #[test]
    fn removed_only_identifiers_skips_kept_and_prose_words() {
        let diff = "--- a/f.c\n+++ b/f.c\n-    riscv_cpu_cfg(env)->ext_zicbom = true;\n-    /* explain things */\n+    riscv_cpu_cfg(env)->ext_other = true;\n";
        assert_eq!(
            removed_only_identifiers(diff),
            vec!["ext_zicbom".to_string()]
        );
    }
}
