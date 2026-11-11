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

use crate::project::ProjectId;
use crate::settings::CustomRemoteSettings;
use anyhow::Result;
use regex::Regex;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;
use std::sync::OnceLock;
use tracing::{debug, warn};

pub const LINUX_NEXT_URL: &str =
    "https://git.kernel.org/pub/scm/linux/kernel/git/next/linux-next.git";
const STABLE_LINUX_URL: &str = "https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git";
const MAX_ACTIVE_STABLE_BRANCHES: usize = 8;

/// Limit on waiting for each git command of the ancestry check; the check
/// runs two. With a commit-graph each takes milliseconds even on a kernel
/// repository.
const OFFLINE_GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// A git command for a local query. In a partial clone git otherwise
/// fetches a missing object from the promisor remote; GIT_NO_LAZY_FETCH
/// stops that in git versions that support it.  Versions without it,
/// such as 2.44.0, ignore it; 2.43.5 has it as a backport.
fn offline_git(repo_path: &Path, args: &[&str]) -> tokio::process::Command {
    let mut cmd = crate::git_cmd::in_dir_async(repo_path);
    cmd.env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    cmd
}

/// Runs offline_git(), giving up on the wait after OFFLINE_GIT_TIMEOUT;
/// kill_on_drop then asks for the child to be killed. None on spawn
/// failure or timeout.
async fn run_offline_git(repo_path: &Path, args: &[&str]) -> Option<std::process::Output> {
    tokio::time::timeout(OFFLINE_GIT_TIMEOUT, offline_git(repo_path, args).output())
        .await
        .ok()?
        .ok()
}

#[derive(Debug, Clone)]
pub struct MaintainersEntry {
    pub subsystem: String,
    pub trees: Vec<(String, Option<String>)>, // (URL, Branch)
    pub patterns: Vec<String>,
}

/// A git object ID naming a commit, in the form the trees Sashiko reviews
/// write one: 40 lowercase hexadecimal digits. A SHA-256 repository would
/// need the 64-digit form too; no such tree is reviewed today.
///
/// Every producer of a baseline commit emits that form, so parsing keeps a
/// value that came over the wire from reaching git as an option, a refspec or
/// a log line full of control characters.
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct CommitId(String);

impl CommitId {
    pub fn parse(value: &str) -> Option<Self> {
        if value.len() != 40
            || !value
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
            return None;
        }

        Some(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CommitId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum BaselineResolution {
    Commit(CommitId), // Explicit base-commit
    LocalRef(String), // e.g. "net-next/master" or "HEAD"
    RemoteTarget {
        url: String,
        name: String,
        branch: Option<String>,
    }, // e.g. url="git://...", name="net-next"
}

impl BaselineResolution {
    pub fn as_str(&self) -> String {
        match self {
            BaselineResolution::Commit(h) => h.to_string(),
            BaselineResolution::LocalRef(r) => r.clone(),
            BaselineResolution::RemoteTarget { name, branch, .. } => match branch {
                Some(b) => format!("{}/{}", name, b),
                None => format!("{}/HEAD", name),
            },
        }
    }
}

#[derive(Debug)]
pub struct BaselineRegistry {
    entries: Vec<MaintainersEntry>,
    remote_map: HashMap<String, String>, // URL -> Local Remote Name
    pub custom_remotes: Option<Vec<CustomRemoteSettings>>,
    pub repo_path: std::path::PathBuf,
    project: Option<ProjectId>,
    fetch_remotes: Vec<(String, String)>, // (fetch URL, remote name), including shared URLs
    mainline_remote: Option<(String, String)>, // (URL, local remote name)
}

impl BaselineRegistry {
    pub fn new(
        repo_path: &Path,
        custom_remotes: Option<Vec<CustomRemoteSettings>>,
        project: Option<ProjectId>,
    ) -> Result<Self> {
        let fetch_remotes = Self::load_git_remotes(repo_path).unwrap_or_default();
        let mut remote_map = HashMap::new();
        for (url, name) in &fetch_remotes {
            remote_map.insert(url.clone(), name.clone());
            if let Some(stripped) = url.strip_suffix(".git") {
                remote_map.insert(stripped.to_string(), name.clone());
            }
        }

        // Prefer the configured project's upstream, even when origin is a
        // contributor fork. Unconfigured repositories retain Linux-first
        // URL inference.
        let mainline_remote = ordered_mainline_remotes(&fetch_remotes, project)
            .into_iter()
            .next()
            .map(|(url, name)| (url.clone(), name.clone()));

        let mainline_name = mainline_remote
            .as_ref()
            .map(|(_, name)| name.as_str())
            .unwrap_or("origin");

        let branch = mainline_branch(project);
        let ref_name = format!("{}/{}", mainline_name, branch);
        debug!(
            "Attempting to load MAINTAINERS from {}:MAINTAINERS",
            ref_name
        );

        let entries = match Self::read_file_from_git(repo_path, &ref_name, "MAINTAINERS") {
            Ok(content) => {
                let reader = std::io::Cursor::new(content);
                Self::parse_maintainers(reader)?
            }
            Err(e) => {
                warn!(
                    "Failed to load MAINTAINERS from git {}: {}. Falling back to local file.",
                    ref_name,
                    e.to_string().trim()
                );
                let maintainers_path = repo_path.join("MAINTAINERS");
                if maintainers_path.exists() {
                    debug!("Loading MAINTAINERS from local file {:?}", maintainers_path);
                    let file = std::fs::File::open(&maintainers_path)?;
                    let reader = std::io::BufReader::new(file);
                    Self::parse_maintainers(reader)?
                } else {
                    warn!(
                        "MAINTAINERS file not found at {:?}, baseline detection will be limited",
                        maintainers_path
                    );
                    Vec::new()
                }
            }
        };

        Ok(Self {
            entries,
            remote_map,
            fetch_remotes,
            custom_remotes,
            repo_path: repo_path.to_path_buf(),
            project,
            mainline_remote,
        })
    }

    /// The remote-tracking ref of the mainline branch, such as `origin/master`.
    pub fn mainline_ref(&self) -> String {
        let branch = mainline_branch(self.project);
        format!("{}/{}", self.mainline_remote_name(), branch)
    }

    /// Name of the identified mainline remote (Linus tree or origin), for
    /// use as a git fetch target. Falls back to "origin" when none was
    /// identified.
    pub fn mainline_remote_name(&self) -> &str {
        self.mainline_remote
            .as_ref()
            .map(|(_, name)| name.as_str())
            .unwrap_or("origin")
    }

    /// Whether a candidate is a remote's default HEAD that resolved to a
    /// strict ancestor of the local mainline ref.
    ///
    /// A MAINTAINERS T: entry without a branch yields the remote's HEAD,
    /// a branch nobody chose for patch application. On some trees it
    /// points to a commit dated years ago (tytso/ext4.git, whose HEAD is a
    /// merge from 2020), and a small patch still applies there with an
    /// offset, so the review would run against old code.
    ///
    /// Such a HEAD has no commit that mainline lacks. The caller tries it
    /// after the other candidates rather than dropping it. This is not a
    /// test of whether the tree is abandoned: a live tree's branch can
    /// also sit on a mainline commit for a while after a merge window, and
    /// is then tried after linux-next and mainline too.
    ///
    /// Explicit branches, base-commit trailers, version tags and
    /// linux-next are left alone. Git runs with GIT_NO_LAZY_FETCH set and
    /// a timeout on each wait, and any error means "do not defer".
    pub async fn is_merged_default_head(&self, candidate: &BaselineResolution, sha: &str) -> bool {
        match candidate {
            BaselineResolution::RemoteTarget {
                url, branch: None, ..
            } if url != LINUX_NEXT_URL => {}
            _ => return false,
        }

        let mainline_ref = format!("{}^{{commit}}", self.mainline_ref());
        let tip = match run_offline_git(
            &self.repo_path,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &mainline_ref,
            ],
        )
        .await
        {
            Some(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).trim().to_string()
            }
            _ => return false,
        };
        if tip == sha {
            return false;
        }

        run_offline_git(&self.repo_path, &["merge-base", "--is-ancestor", sha, &tip])
            .await
            .is_some_and(|out| out.status.success())
    }

    fn read_file_from_git(repo_path: &Path, rev: &str, file_path: &str) -> Result<String> {
        let output = crate::git_cmd::in_dir(repo_path)
            .args(["show", &format!("{}:{}", rev, file_path)])
            .output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "git show failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn parse_maintainers<R: BufRead>(reader: R) -> Result<Vec<MaintainersEntry>> {
        let mut entries = Vec::new();
        let mut current_subsystem = String::new();
        let mut current_trees = Vec::new();
        let mut current_patterns = Vec::new();

        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                if !current_subsystem.is_empty()
                    && (!current_trees.is_empty() || !current_patterns.is_empty())
                {
                    entries.push(MaintainersEntry {
                        subsystem: current_subsystem.clone(),
                        trees: current_trees.clone(),
                        patterns: current_patterns.clone(),
                    });
                }
                current_subsystem.clear();
                current_trees.clear();
                current_patterns.clear();
                continue;
            }

            if !line.contains(':') && current_subsystem.is_empty() {
                current_subsystem = line.trim().to_string();
            } else if let Some((tag, value)) = line.split_once(':') {
                let val = value.trim();
                match tag {
                    "T" => {
                        if let Some(rest) = val.strip_prefix("git ") {
                            let parts: Vec<&str> = rest.split_whitespace().collect();
                            if !parts.is_empty() {
                                let url = parts[0].to_string();
                                let branch = if parts.len() > 1 {
                                    Some(parts[1].to_string())
                                } else {
                                    None
                                };
                                current_trees.push((url, branch));
                            }
                        }
                    }
                    "F" => {
                        current_patterns.push(val.to_string());
                    }
                    _ => {}
                }
            }
        }

        if !current_subsystem.is_empty()
            && (!current_trees.is_empty() || !current_patterns.is_empty())
        {
            entries.push(MaintainersEntry {
                subsystem: current_subsystem,
                trees: current_trees,
                patterns: current_patterns,
            });
        }

        debug!("Parsed {} MAINTAINERS entries", entries.len());
        Ok(entries)
    }

    fn load_git_remotes(repo_path: &Path) -> Result<Vec<(String, String)>> {
        let output = crate::git_cmd::in_dir(repo_path)
            .args(["remote", "-v"])
            .output()?;
        let mut remotes = Vec::new();
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let mut parts = line.split_whitespace();
                // Fetch state and refspecs belong to the name, not the URL.
                if let (Some(name), Some(url), Some("(fetch)")) =
                    (parts.next(), parts.next(), parts.next())
                {
                    remotes.push((url.to_string(), name.to_string()));
                }
            }
        }
        Ok(remotes)
    }

    pub async fn resolve_candidates(
        &self,
        files: &[String],
        subject: &str,
        body: Option<&str>,
    ) -> Vec<BaselineResolution> {
        let mut candidates = Vec::new();

        // 1. Explicit Base Commit
        if let Some(body_text) = body
            && let Some(commit) = extract_base_commit(body_text)
        {
            candidates.push(BaselineResolution::Commit(commit));
        }

        // When a non-Linux mainline remote (such as GCC or Sashiko) was explicitly
        // identified, skip Linux-specific stable and linux-next candidate remotes.
        let has_non_linux_mainline = self.project.map_or_else(
            || {
                self.mainline_remote
                    .as_ref()
                    .is_some_and(|(url, _)| is_non_linux_project_url(url))
            },
            |project| project != ProjectId::Linux,
        );

        // 1.5 Version Tag from Subject
        let subject_candidates = subject_version_candidates(subject, has_non_linux_mainline);
        let has_subject_version = !subject_candidates.is_empty();
        candidates.extend(subject_candidates);

        // 1.6 Explicit baseline version tag stated in cover letter or changelog
        if let Some(body_text) = body
            && let Some(tag) = extract_body_version_tag(body_text)
        {
            candidates.push(BaselineResolution::LocalRef(format!("v{tag}")));
        }

        // 1.7 Unversioned Linux stable backports ([PATCH stable backport] or
        //     leading "commit <sha> upstream." without an explicit X.Y tag)
        if !has_non_linux_mainline
            && !has_subject_version
            && is_unversioned_stable_backport(subject, body)
        {
            candidates.extend(self.active_stable_branch_candidates().await);
        }

        // 2. Subsystem Heuristic
        let heuristic_candidates = self.resolve_subsystem_heuristic(files, subject);
        candidates.extend(heuristic_candidates);

        // 3. Custom Remotes
        // Ahead of linux-next because the first candidate that applies wins,
        // and a topic-branch series usually applies to linux-next too -- but
        // against a stale snapshot: a daily build behind, and sharing no SHAs
        // with the topic branch once the maintainer rebases.
        if let Some(custom_remotes) = &self.custom_remotes {
            for remote in custom_remotes {
                let refspecs = remote.refspecs();

                // Ensure remote is configured without blocking review dispatch on routine sync
                if let Err(e) = crate::git_ops::ensure_remote_if_missing(
                    &self.repo_path,
                    &remote.name,
                    &remote.url,
                    refspecs.as_deref(),
                )
                .await
                {
                    warn!(
                        "Failed to ensure custom remote {}: {}. Using local branches.",
                        remote.name, e
                    );
                }

                let branch_patterns = remote.branch_patterns.as_deref().filter(|p| !p.is_empty());
                let only_branches = remote.only_branches.as_deref().filter(|b| !b.is_empty());

                let mut matched_any = false;
                if let Some(patterns) = branch_patterns {
                    match crate::git_ops::get_remote_branches(&self.repo_path, &remote.name).await {
                        Ok(branches) => {
                            let filtered = branches
                                .into_iter()
                                .filter(|b| crate::git_ops::matches_branch_pattern(b, patterns));
                            for branch in filtered {
                                matched_any = true;
                                candidates.push(BaselineResolution::RemoteTarget {
                                    url: remote.url.clone(),
                                    name: remote.name.clone(),
                                    branch: Some(branch),
                                });
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Failed to get remote branches for {}: {}. Continuing.",
                                remote.name, e
                            );
                        }
                    }
                } else if remote.check_all_branches {
                    match crate::git_ops::get_remote_branches(&self.repo_path, &remote.name).await {
                        Ok(branches) => {
                            for branch in branches {
                                matched_any = true;
                                candidates.push(BaselineResolution::RemoteTarget {
                                    url: remote.url.clone(),
                                    name: remote.name.clone(),
                                    branch: Some(branch),
                                });
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Failed to get remote branches for {}: {}. Continuing.",
                                remote.name, e
                            );
                        }
                    }
                }

                // only_branches is additive: append explicitly configured branches
                if let Some(branches) = only_branches {
                    for branch in branches {
                        candidates.push(BaselineResolution::RemoteTarget {
                            url: remote.url.clone(),
                            name: remote.name.clone(),
                            branch: Some(branch.clone()),
                        });
                    }
                } else if !matched_any && !remote.check_all_branches && branch_patterns.is_none() {
                    candidates.push(BaselineResolution::RemoteTarget {
                        url: remote.url.clone(),
                        name: remote.name.clone(),
                        branch: None,
                    });
                }
            }
        }

        // 4. Linux Next (skip when a non-Linux mainline remote was identified)
        if !has_non_linux_mainline {
            candidates.push(self.resolve_url(LINUX_NEXT_URL, None));
        }

        // 5. Mainline
        // Use the identified mainline remote (Linus tree or origin) as a
        // RemoteTarget so that ensure_remote fetches it before use. A bare
        // LocalRef("HEAD") would resolve to the local checkout, which nothing
        // in Sashiko ever advances after fetching.
        if let Some((url, name)) = &self.mainline_remote {
            candidates.push(BaselineResolution::RemoteTarget {
                url: url.clone(),
                name: name.clone(),
                branch: Some(mainline_branch(self.project).to_string()),
            });
            // A failed fetch or missing ref must not hide a usable mirror.
            for (mirror_url, mirror_name) in
                ordered_mainline_remotes(&self.fetch_remotes, self.project)
            {
                if mirror_name != name {
                    candidates.push(BaselineResolution::RemoteTarget {
                        url: mirror_url.clone(),
                        name: mirror_name.clone(),
                        branch: Some(mainline_branch(self.project).to_string()),
                    });
                }
            }
        } else {
            candidates.push(BaselineResolution::LocalRef("HEAD".to_string()));
        }

        // Deduplicate
        // Simple deduplication based on string representation or enum equality
        // Since we implement PartialEq, dedup works if consecutive. We need unique.
        let mut unique_candidates = Vec::new();
        for c in candidates {
            if !unique_candidates.contains(&c) {
                unique_candidates.push(c);
            }
        }

        unique_candidates
    }

    fn resolve_subsystem_heuristic(
        &self,
        files: &[String],
        subject: &str,
    ) -> Vec<BaselineResolution> {
        let mut tree_counts: HashMap<(String, Option<String>), usize> = HashMap::new();
        let mut tree_subsystems: HashMap<(String, Option<String>), String> = HashMap::new();

        for file in files {
            for entry in &self.entries {
                let mut matched = false;
                for pattern in &entry.patterns {
                    if pattern.ends_with('/') {
                        if file.starts_with(pattern) {
                            matched = true;
                            break;
                        }
                    } else if pattern == file {
                        matched = true;
                        break;
                    }
                }

                if matched {
                    for tree in &entry.trees {
                        *tree_counts.entry(tree.clone()).or_insert(0) += 1;
                        // Associate this tree with the subsystem that maps to
                        // it directly, so special-case checks below key off
                        // the winning tree's own subsystem rather than
                        // whichever entry happened to match first overall.
                        tree_subsystems
                            .entry(tree.clone())
                            .or_insert_with(|| entry.subsystem.clone());
                    }
                }
            }
        }

        // OBJTOOL in Linux MAINTAINERS points at maintainer-tip.rst via P:
        // without a T: line, while patches land on tip.git objtool/core and
        // objtool/urgent.
        let objtool_count = files.iter().filter(|f| is_objtool_file(f)).count();
        if objtool_count > 0
            && !tree_counts
                .keys()
                .any(|(u, b)| is_tip_tree_url(u) && b.as_deref() == Some("objtool/core"))
            && let Some(tip_url) = self
                .entries
                .iter()
                .flat_map(|e| &e.trees)
                .map(|(url, _)| url)
                .find(|url| is_tip_tree_url(url))
                .cloned()
        {
            let key = (tip_url, Some("objtool/core".to_string()));
            *tree_counts.entry(key.clone()).or_insert(0) += objtool_count;
            tree_subsystems
                .entry(key)
                .or_insert_with(|| "OBJTOOL".to_string());
        }

        if tree_counts.is_empty() {
            return Vec::new();
        }

        // Intel Wired LAN series are queued on dev-queue of Tony's trees,
        // never on the trees' default branches.
        if let Some(url) = Self::iwl_queue_tree(subject, &tree_counts) {
            return vec![self.resolve_url(&url, Some("dev-queue".to_string()))];
        }

        let mut candidates: Vec<(&(String, Option<String>), &usize)> = tree_counts.iter().collect();
        candidates.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.0.cmp(&b.0.0)));

        // Check for Linux-MM special handling
        // If the top candidate is akpm/mm or linux-mm, OR the subsystem is MEMORY MANAGEMENT
        let top_tree = candidates[0].0;
        let (top_url, _top_branch) = top_tree;
        let top_subsystem = tree_subsystems.get(top_tree);
        let is_mm = top_url.contains("akpm/mm")
            || top_url.contains("linux-mm")
            || top_subsystem
                .map(|s| s.eq_ignore_ascii_case("MEMORY MANAGEMENT"))
                .unwrap_or(false);

        if is_mm {
            // For linux-mm, we prioritize specific branches: mm-new, mm-unstable, mm-stable
            // We use the discovered URL (likely akpm/mm)
            let mm_url = top_url;
            return vec![
                self.resolve_url(mm_url, Some("mm-new".to_string())),
                self.resolve_url(mm_url, Some("mm-unstable".to_string())),
                self.resolve_url(mm_url, Some("mm-stable".to_string())),
            ];
        }

        // Check for Staging special handling
        // gregkh/staging.git's HEAD points at "main", which tracks what has
        // already been merged upstream rather than the branch new patches
        // are expected to apply to. Prioritize the actual active branches,
        // same reasoning as the linux-mm case above.
        let is_staging = top_url.contains("gregkh/staging")
            || top_subsystem
                .map(|s| s.eq_ignore_ascii_case("STAGING SUBSYSTEM"))
                .unwrap_or(false);

        if is_staging {
            let staging_url = top_url;
            return vec![
                self.resolve_url(staging_url, Some("staging-testing".to_string())),
                self.resolve_url(staging_url, Some("staging-next".to_string())),
            ];
        }

        let subject_lower = subject.to_lowercase();
        let prefixes = crate::patch::get_subject_prefixes(subject);
        let is_next = subject_lower.contains("next");

        let mut base_prefixes = Vec::new();
        for prefix in &prefixes {
            let prefix_lower = prefix.to_lowercase();
            base_prefixes.push(prefix_lower.clone());
            if prefix_lower.contains('-') {
                for part in prefix_lower.split('-') {
                    if part != "next" && part != "net" {
                        base_prefixes.push(part.to_string());
                    }
                }
            }
        }

        let mut scored_candidates: Vec<(i32, BaselineResolution)> = Vec::new();

        for (candidate_tree, count) in candidates {
            let (url, branch) = candidate_tree;
            let mut score = (*count as i32) * 10;
            let url_lower = url.to_lowercase();
            let branch_lower = branch
                .as_ref()
                .map(|b| b.to_lowercase())
                .unwrap_or_default();

            let mut has_prefix_match = false;
            for prefix in &base_prefixes {
                if url_lower.ends_with(&format!("/{}.git", prefix)) || branch_lower == *prefix {
                    score += 50;
                }
                if url_lower.contains(prefix) || branch_lower.contains(prefix) {
                    has_prefix_match = true;
                }
            }
            if has_prefix_match {
                score += 100;
            }

            if url_lower.contains("mptcp") {
                if is_next {
                    if branch_lower == "export" {
                        score += 50;
                    }
                    if branch_lower == "export-net" {
                        score -= 50;
                    }
                } else {
                    if branch_lower == "export-net" {
                        score += 50;
                    }
                    if branch_lower == "export" {
                        score -= 50;
                    }
                }
            } else {
                let tree_is_next = url_lower.contains("next") || branch_lower.contains("next");
                if is_next {
                    if tree_is_next {
                        score += 50;
                    } else {
                        score -= 50;
                    }
                } else if tree_is_next {
                    score -= 50;
                } else {
                    score += 50;
                }
            }

            // Fallback keywords match (legacy heuristic)
            let keywords = [
                "net", "bpf", "drm", "mm", "sched", "x86", "arm", "arm64", "scsi", "usb", "perf",
            ];
            let cand_subsys = tree_subsystems.get(candidate_tree);
            for kw in keywords {
                let url_matches = url_lower.contains(kw);
                let branch_matches = branch_lower.contains(kw);
                let subject_or_subsys_matches = subject_lower.contains(kw)
                    || cand_subsys
                        .map(|s| s.to_lowercase().contains(kw))
                        .unwrap_or(false);

                if subject_or_subsys_matches && (url_matches || branch_matches) {
                    score += 10;
                }
            }

            scored_candidates.push((score, self.resolve_url(url, branch.clone())));
        }

        // Sort descending by score
        scored_candidates.sort_by_key(|b| std::cmp::Reverse(b.0));

        let prefer_urgent = subject_lower.contains("urgent");
        let mut unique_filtered = Vec::new();
        let mut tip_master_urls: Vec<String> = Vec::new();
        let push_unique = |list: &mut Vec<BaselineResolution>, item: BaselineResolution| {
            if !list.contains(&item) {
                list.push(item);
            }
        };

        for (_, res) in scored_candidates {
            if let BaselineResolution::RemoteTarget {
                ref url,
                branch: Some(ref branch),
                ..
            } = res
                && is_tip_tree_url(url)
            {
                if !tip_master_urls.contains(url) {
                    tip_master_urls.push(url.clone());
                }
                if let Some(subsys) = branch.strip_suffix("/core")
                    && !subsys.is_empty()
                {
                    let urgent = self.resolve_url(url, Some(format!("{subsys}/urgent")));
                    if prefer_urgent {
                        push_unique(&mut unique_filtered, urgent);
                        push_unique(&mut unique_filtered, res);
                    } else {
                        push_unique(&mut unique_filtered, res);
                        push_unique(&mut unique_filtered, urgent);
                    }
                    continue;
                }
            }
            push_unique(&mut unique_filtered, res);
        }

        for tip_url in tip_master_urls {
            push_unique(
                &mut unique_filtered,
                self.resolve_url(&tip_url, Some("master".to_string())),
            );
        }

        unique_filtered
    }

    /// URL of the Tony Nguyen queue tree matching an iwl-net/iwl-next tag in
    /// the subject, picked from the MAINTAINERS trees the touched files voted
    /// for. Returns None when the subject carries no iwl tag or none of those
    /// trees is the matching queue.
    fn iwl_queue_tree(
        subject: &str,
        tree_counts: &HashMap<(String, Option<String>), usize>,
    ) -> Option<String> {
        let subject_lower = subject.to_lowercase();
        let queue = if subject_lower.contains("iwl-next") {
            "next-queue"
        } else if subject_lower.contains("iwl-net") {
            "net-queue"
        } else {
            return None;
        };

        tree_counts
            .keys()
            .map(|(url, _)| url)
            .find(|url| url.contains(queue))
            .cloned()
    }

    fn resolve_url(&self, url: &str, branch: Option<String>) -> BaselineResolution {
        if let Some(remote_name) = self.remote_map.get(url) {
            BaselineResolution::RemoteTarget {
                url: url.to_string(),
                name: remote_name.clone(),
                branch,
            }
        } else {
            let name = self.suggest_remote_name(url);
            BaselineResolution::RemoteTarget {
                url: url.to_string(),
                name,
                branch,
            }
        }
    }

    fn suggest_remote_name(&self, url: &str) -> String {
        let path = url.trim_end_matches('/');
        let name = path.rsplit('/').next().unwrap_or("unknown");
        let name = name.strip_suffix(".git").unwrap_or(name);

        if name == "linux" {
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() >= 2 {
                return parts[parts.len() - 2].to_string();
            }
        }
        name.to_string()
    }

    async fn active_stable_branch_candidates(&self) -> Vec<BaselineResolution> {
        let stable_remote = self
            .remote_map
            .get(STABLE_LINUX_URL)
            .map(String::as_str)
            .unwrap_or("stable");
        if stable_remote.is_empty() || stable_remote.starts_with('-') {
            return Vec::new();
        }
        let count_arg = format!("--count={MAX_ACTIVE_STABLE_BRANCHES}");
        let pattern = format!("refs/remotes/{stable_remote}/linux-*.y");
        let Some(output) = run_offline_git(
            &self.repo_path,
            &[
                "for-each-ref",
                "--sort=-version:refname",
                "--sort=-committerdate",
                &count_arg,
                "--format=%(refname:short)",
                &pattern,
            ],
        )
        .await
        else {
            return Vec::new();
        };
        if !output.status.success() {
            return Vec::new();
        }
        let prefix = format!("{stable_remote}/");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().strip_prefix(&prefix))
            .filter(|branch| is_valid_stable_branch_name(branch))
            .map(|branch| BaselineResolution::RemoteTarget {
                url: STABLE_LINUX_URL.to_string(),
                name: stable_remote.to_string(),
                branch: Some(branch.to_string()),
            })
            .collect()
    }
}

fn ordered_mainline_remotes(
    fetch_remotes: &[(String, String)],
    project: Option<ProjectId>,
) -> Vec<(&String, &String)> {
    let mut remotes: Vec<_> = fetch_remotes
        .iter()
        .map(|(url, name)| (url, name))
        .filter(|(url, _)| project.unwrap_or_default().matches_upstream_url(url))
        .collect();
    // Match the bug resolver's remote-name order without collapsing shared URLs.
    remotes.sort_by_key(|(_, name)| *name);
    remotes.dedup_by_key(|(_, name)| *name);
    if !remotes.iter().any(|(_, name)| name.as_str() == "origin")
        && let Some((url, name)) = fetch_remotes.iter().find(|(_, name)| name == "origin")
    {
        remotes.push((url, name));
    }
    remotes
}

fn is_non_linux_project_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.contains("gcc.gnu.org")
        || lower.contains("gnutools/gcc")
        || lower.contains("gcc-mirror/gcc")
        || lower.contains("qemu-project/qemu")
        || lower.contains("qemu.org")
        || lower.contains("qemu/qemu")
        || lower.contains("zephyrproject-rtos/zephyr")
        || lower.contains("sashiko")
}

/// The branch a mainline remote develops on.
fn mainline_branch(project: Option<ProjectId>) -> &'static str {
    project.map_or("master", ProjectId::mainline_branch)
}

fn is_tip_tree_url(url: &str) -> bool {
    let lower = url.trim_end_matches('/').to_ascii_lowercase();
    lower.ends_with("/tip/tip.git") || lower.ends_with("/tip.git")
}

fn is_objtool_file(file: &str) -> bool {
    file.starts_with("tools/objtool/")
        || file.starts_with("include/linux/objtool")
        || file == "Documentation/tools/objtool.txt"
}

fn sanitize_diff_path(raw: &str) -> Option<&str> {
    let trimmed = raw.trim().trim_start_matches("./");
    if trimmed.is_empty()
        || trimmed.starts_with('/')
        || trimmed.starts_with('-')
        || trimmed.chars().any(char::is_control)
        || trimmed
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return None;
    }
    Some(trimmed)
}

fn parse_unified_header_path<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(prefix)?;
    let path = rest.split('\t').next()?.trim();
    let path = path
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(path);
    Some(path)
}

fn strip_p1_single_side(raw: &str) -> &str {
    if let Some(rel) = raw.strip_prefix("b/").or_else(|| raw.strip_prefix("a/")) {
        return rel;
    }
    if let Some((root, rel)) = raw.split_once('/')
        && (root == "linux" || root.starts_with("linux-") || root.ends_with(".orig"))
    {
        return rel;
    }
    raw
}

fn extract_unified_pair_path<'a>(old_raw: &'a str, new_raw: &'a str) -> Option<&'a str> {
    if old_raw == "/dev/null" && new_raw == "/dev/null" {
        return None;
    }
    if old_raw == "/dev/null" {
        return sanitize_diff_path(strip_p1_single_side(new_raw));
    }
    if new_raw == "/dev/null" {
        return sanitize_diff_path(strip_p1_single_side(old_raw));
    }
    if let Some(rel) = new_raw
        .strip_prefix("b/")
        .or_else(|| old_raw.strip_prefix("a/"))
    {
        return sanitize_diff_path(rel);
    }
    if old_raw != new_raw
        && let (Some((_, old_rel)), Some((_, new_rel))) =
            (old_raw.split_once('/'), new_raw.split_once('/'))
        && old_rel == new_rel
    {
        return sanitize_diff_path(new_rel);
    }
    sanitize_diff_path(new_raw)
}

pub fn extract_files_from_diff(diff: &str) -> Vec<String> {
    let lines: Vec<&str> = diff.lines().collect();
    let mut files = Vec::new();
    let mut in_git_header = false;
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        if let Some(path) = line.strip_prefix("diff --git a/")
            && let Some((a, _)) = path.split_once(' ')
        {
            if let Some(clean) = sanitize_diff_path(a) {
                files.push(clean.to_string());
            }
            in_git_header = true;
            i += 1;
            continue;
        }
        if line.starts_with("@@ ") {
            in_git_header = false;
            i += 1;
            continue;
        }
        if !in_git_header
            && i + 2 < lines.len()
            && lines[i + 2].starts_with("@@ -")
            && let Some(old_raw) = parse_unified_header_path(line, "--- ")
            && let Some(new_raw) = parse_unified_header_path(lines[i + 1], "+++ ")
        {
            if let Some(clean) = extract_unified_pair_path(old_raw, new_raw) {
                files.push(clean.to_string());
            }
            i += 3;
            continue;
        }
        i += 1;
    }
    files
}

pub fn extract_base_commit(body: &str) -> Option<CommitId> {
    static BASE_COMMIT_RE: OnceLock<Regex> = OnceLock::new();
    let re = BASE_COMMIT_RE.get_or_init(|| Regex::new(r"(?m)^base-commit:[ \t]*(\S+)").unwrap());
    // The trailer only says where to look; what an object ID is stays with
    // CommitId, and a trailer that holds anything else is no trailer at all.
    re.captures_iter(body)
        .find_map(|caps| CommitId::parse(caps.get(1)?.as_str()))
}

pub fn extract_version_tag(subject: &str) -> Option<String> {
    static VERSION_TAG_RE: OnceLock<Regex> = OnceLock::new();
    let re = VERSION_TAG_RE
        .get_or_init(|| Regex::new(r"(?i)\bv?(\d+\.\d+(?:\.\d+)?(?:-rc\d+)?(?:\.y)?)\b").unwrap());
    for caps in re.captures_iter(subject) {
        let Some(full) = caps.get(0) else {
            continue;
        };
        let Some(ver_match) = caps.get(1) else {
            continue;
        };
        let ver = ver_match.as_str().to_lowercase();
        if is_bracketed_patch_revision_tag(subject, full.start(), full.end(), &ver) {
            continue;
        }
        return Some(ver);
    }
    None
}

/// Returns true when a matched `X.Y` token sits inside a non-stable `[...]`
/// subject prefix and represents a dotted patch revision (`[patch V1.1 02/38]`,
/// `[PATCH v2.1 1/3]`) rather than a baseline release tag.
fn is_bracketed_patch_revision_tag(subject: &str, start: usize, end: usize, ver: &str) -> bool {
    let Some(open) = subject[..start].rfind('[') else {
        return false;
    };
    if subject[open..start].contains(']') {
        return false;
    }
    let Some(rel_close) = subject[end..].find(']') else {
        return false;
    };
    let group = &subject[open + 1..end + rel_close];
    let mut has_patch_or_rfc = false;
    for tok in group.split(|c: char| !c.is_alphanumeric() && c != '-' && c != '.' && c != '_') {
        if tok.eq_ignore_ascii_case("stable") {
            return false;
        }
        if tok.eq_ignore_ascii_case("patch")
            || tok.eq_ignore_ascii_case("rfc")
            || tok.eq_ignore_ascii_case("resend")
        {
            has_patch_or_rfc = true;
        }
    }
    let Some((maj_s, min_s)) = ver.split_once('.') else {
        return false;
    };
    if !maj_s.bytes().all(|b| b.is_ascii_digit()) || !min_s.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let (Ok(major), Ok(minor)) = (maj_s.parse::<u32>(), min_s.parse::<u32>()) else {
        return false;
    };
    let starts_with_v = subject.as_bytes()[start].eq_ignore_ascii_case(&b'v');
    major < 3 && (major, minor) != (2, 6) && (starts_with_v || has_patch_or_rfc)
}

pub fn extract_gcc_release_branch(subject: &str) -> Option<u32> {
    static GCC_BRANCH_RE: OnceLock<Regex> = OnceLock::new();
    let re = GCC_BRANCH_RE.get_or_init(|| {
        Regex::new(r"(?i)(?:\breleases/gcc-(\d{1,2})\b|\[[^\]]*\bgcc[- ]?(\d{1,2})\b[^\]]*\])")
            .unwrap()
    });
    let caps = re.captures(subject)?;
    let num = caps.get(1).or_else(|| caps.get(2))?.as_str();
    num.parse().ok()
}

fn subject_version_candidates(
    subject: &str,
    has_non_linux_mainline: bool,
) -> Vec<BaselineResolution> {
    let version = extract_version_tag(subject);
    let stable_versions = explicit_stable_versions(subject).or_else(|| {
        let version = version.as_deref()?;
        (version.ends_with(".y") || is_stable_series_prefix(subject, version))
            .then(|| (version.strip_suffix(".y").unwrap_or(version), None))
    });
    let mut candidates = Vec::new();
    if let Some(gcc_major) = extract_gcc_release_branch(subject) {
        candidates.push(BaselineResolution::LocalRef(format!(
            "origin/releases/gcc-{gcc_major}"
        )));
        candidates.push(BaselineResolution::LocalRef(format!(
            "releases/gcc-{gcc_major}"
        )));
    }
    if !has_non_linux_mainline && let Some((first, last)) = stable_versions {
        for version in std::iter::once(first).chain(last) {
            candidates.push(BaselineResolution::RemoteTarget {
                url: STABLE_LINUX_URL.to_string(),
                name: "stable".to_string(),
                branch: Some(format!("linux-{version}.y")),
            });
        }
    }
    if let Some(version) = version {
        let version = version.strip_suffix(".y").unwrap_or(&version);
        candidates.push(BaselineResolution::LocalRef(format!("v{version}")));
    }
    candidates
}

/// Extract an explicit `vX.Y[.Z][-rcN]` baseline tag declared in non-quoted
/// pre-diff cover-letter or changelog prose (for example `"The series applies
/// on top of v7.0-rc3"` or `"- Rebase on v7.2-rc1"`).
pub fn extract_body_version_tag(body: &str) -> Option<String> {
    static BODY_TAG_RE: OnceLock<Regex> = OnceLock::new();
    let re = BODY_TAG_RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:applies(?:\s+cleanly)?\s+(?:on(?:\s+top\s+of)?|to|against)|(?:re)?based?\s+(?:on(?:\s+top\s+of)?|onto|against))\s+v(\d+\.\d+(?:\.\d+)?(?:-rc\d+)?)\b",
        )
        .unwrap()
    });

    for line in body.lines() {
        if line.starts_with("diff --git ")
            || line.starts_with("--- a/")
            || line.starts_with("--- /dev/null")
            || line.starts_with("@@ -")
        {
            break;
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with('>') {
            continue;
        }
        if let Some(caps) = re.captures(trimmed)
            && let Some(m) = caps.get(1)
        {
            let ver = m.as_str().to_lowercase();
            let base = ver.split('-').next().unwrap_or(&ver);
            let mut parts = base.split('.');
            if let (Some(maj_s), Some(min_s)) = (parts.next(), parts.next())
                && let (Ok(major), Ok(minor)) = (maj_s.parse::<u32>(), min_s.parse::<u32>())
                && (major >= 3 || (major == 2 && minor == 6))
            {
                return Some(ver);
            }
        }
    }
    None
}

fn is_valid_stable_branch_name(branch: &str) -> bool {
    let Some(ver) = branch
        .strip_prefix("linux-")
        .and_then(|s| s.strip_suffix(".y"))
    else {
        return false;
    };
    let Some((major, minor)) = ver.split_once('.') else {
        return false;
    };
    !major.is_empty()
        && !minor.is_empty()
        && major.bytes().all(|b| b.is_ascii_digit())
        && minor.bytes().all(|b| b.is_ascii_digit())
}

fn has_upstream_backport_marker(body: &str) -> bool {
    for line in body.lines().take(40) {
        let trimmed = line.trim();
        if trimmed == "---"
            || trimmed.starts_with("diff --git ")
            || trimmed.starts_with("--- a/")
            || trimmed.starts_with("@@ -")
        {
            break;
        }
        if trimmed.starts_with('>') {
            continue;
        }
        if let Some(rest) = trimmed
            .strip_prefix("commit ")
            .or_else(|| trimmed.strip_prefix("Commit "))
        {
            let rest = rest.strip_suffix('.').unwrap_or(rest);
            if let Some(sha) = rest
                .strip_suffix(" upstream")
                .or_else(|| rest.strip_suffix(" UPSTREAM"))
                && CommitId::parse(sha.trim()).is_some()
            {
                return true;
            }
        }
        if let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            let inner = inner.trim();
            if let Some(sha) = inner
                .strip_prefix("Upstream commit ")
                .or_else(|| inner.strip_prefix("upstream commit "))
                && CommitId::parse(sha.trim()).is_some()
            {
                return true;
            }
        }
    }
    false
}

fn is_unversioned_stable_backport(subject: &str, body: Option<&str>) -> bool {
    crate::patch::subject_prefix_tokens(subject).any(|tok| tok.eq_ignore_ascii_case("stable"))
        || body.is_some_and(has_upstream_backport_marker)
}

fn explicit_stable_versions(subject: &str) -> Option<(&str, Option<&str>)> {
    if !crate::patch::subject_prefix_tokens(subject)
        .any(|token| token.eq_ignore_ascii_case("stable"))
    {
        return None;
    }

    // Only the first complete token supplies hints, with at most two named
    // endpoints. Expanding a numeric range would invent unrequested branches.
    crate::patch::subject_prefix_tokens(subject).find_map(|token| {
        if let Some((first, last)) = token.split_once('-') {
            Some((
                stable_series_version(first)?,
                Some(stable_series_version(last)?),
            ))
        } else {
            Some((stable_series_version(token)?, None))
        }
    })
}

fn stable_series_version(token: &str) -> Option<&str> {
    let version = token.strip_prefix(['v', 'V']).unwrap_or(token);
    let version = version
        .strip_suffix(".y")
        .or_else(|| version.strip_suffix(".Y"))
        .unwrap_or(version);
    let (major, minor) = version.split_once('.')?;
    [major, minor]
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        .then_some(version)
}

fn is_stable_series_prefix(subject: &str, version: &str) -> bool {
    // A bare X.Y prefix names a backport series, unlike an exact vX.Y tag
    // or a hardware/protocol version mentioned in the subject text.
    stable_series_version(version) == Some(version)
        && crate::patch::subject_prefix_tokens(subject).any(|prefix| prefix == version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_id_takes_the_form_git_writes_and_nothing_else() {
        let sha1 = "1234567890abcdef1234567890abcdef12345678";
        assert_eq!(CommitId::parse(sha1).unwrap().as_str(), sha1);

        // Abbreviated, over-long, upper case, and anything git would read as
        // an option, a refspec or a log line of its own.
        assert!(CommitId::parse("").is_none());
        assert!(CommitId::parse("1234567").is_none());
        assert!(CommitId::parse(&sha1[..39]).is_none());
        assert!(CommitId::parse(&format!("{sha1}0")).is_none());
        assert!(CommitId::parse(&sha1.to_uppercase()).is_none());
        assert!(CommitId::parse("v6.7-rc2").is_none());
        assert!(CommitId::parse("--upload-pack=sh").is_none());
        assert!(CommitId::parse("refs/heads/main:refs/heads/main").is_none());
        assert!(CommitId::parse("dead\u{1b}[2K\nApplied: beef").is_none());
    }

    #[test]
    fn extract_base_commit_keeps_the_first_trailer_naming_an_object_id() {
        let sha1 = "1234567890abcdef1234567890abcdef12345678";
        assert_eq!(
            extract_base_commit(&format!("text\nbase-commit: {sha1}\n")).as_ref(),
            CommitId::parse(sha1).as_ref()
        );

        // A trailer that names something else is not a base-commit, and does
        // not hide the one that follows it.
        assert!(extract_base_commit("base-commit: v6.7-rc2\n").is_none());
        assert!(extract_base_commit(&format!("base-commit: {sha1}-dirty\n")).is_none());
        assert_eq!(
            extract_base_commit(&format!("base-commit: HEAD\nbase-commit: {sha1}\n")).as_ref(),
            CommitId::parse(sha1).as_ref()
        );
        assert!(extract_base_commit(&format!("prefix base-commit: {sha1}\n")).is_none());
    }

    fn create_registry() -> BaselineRegistry {
        let entries = vec![MaintainersEntry {
            subsystem: "NETWORKING".to_string(),
            trees: vec![("git://net-next.git".to_string(), None)],
            patterns: vec!["net/".to_string()],
        }];
        let mut remote_map = HashMap::new();
        remote_map.insert("git://net-next.git".to_string(), "net-next".to_string());
        BaselineRegistry {
            entries,
            remote_map,
            custom_remotes: None,
            repo_path: std::path::PathBuf::from("."),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: None,
        }
    }

    #[tokio::test]
    async fn test_resolve_candidates() {
        let registry = create_registry();
        let files = vec!["net/core.c".to_string()];
        let body = "Some text\nbase-commit: 1234567890123456789012345678901234567890\n";

        let candidates = registry
            .resolve_candidates(&files, "Subject", Some(body))
            .await;

        assert_eq!(candidates.len(), 4); // Base, Subsystem, Next, Head

        assert_eq!(
            candidates[0],
            BaselineResolution::Commit(
                CommitId::parse("1234567890123456789012345678901234567890").unwrap()
            )
        );

        match &candidates[1] {
            BaselineResolution::RemoteTarget { name, .. } => assert_eq!(name, "net-next"),
            _ => panic!("Expected RemoteTarget net-next"),
        }
    }

    #[tokio::test]
    async fn test_resolve_stable_series_prefixes() {
        let registry = create_registry();
        let files = vec!["net/core.c".to_string()];
        let fallback = registry
            .resolve_candidates(&files, "[PATCH] fix", None)
            .await;
        let body = "base-commit: 1234567890123456789012345678901234567890\n";

        for (subject, version) in [
            ("[PATCH 5.10] net: fix", "5.10"),
            ("[PATCH 5.15] net: fix", "5.15"),
            ("[PATCH 6.1 0/2] net: fixes", "6.1"),
            ("[PATCH v2 6.1 1/2] net: fix", "6.1"),
            ("[RFC PATCH RESEND 6.1 01/02] net: fix", "6.1"),
            ("[PATCH][5.15] net: fix", "5.15"),
            ("[PATCH,5.15,v2,1/2] net: fix", "5.15"),
            ("[patch 5.15] net: fix", "5.15"),
            ("[PATCH 5.10.y] net: fix", "5.10"),
            ("[PATCH v5.10.y] net: fix", "5.10"),
            ("[PATCH 5.10.Y] net: fix", "5.10"),
            ("[PATCH 6.18 000/430] 6.18.3-rc1 review", "6.18"),
            ("[STABLE v6.6 1/5] net: fix", "6.6"),
            ("[STABLE v6.12 0/4] net: fixes", "6.12"),
            ("[STABLE v6.18 1/4] net: fix", "6.18"),
            ("[PATCH STABLE V6.6] net: fix", "6.6"),
            ("[stable v6.6] net: fix", "6.6"),
            ("[STABLE 6.6] net: fix", "6.6"),
            ("[STABLE v6.6.Y] net: fix", "6.6"),
            ("[RESEND PATCH v2][STABLE][v6.6] net: fix", "6.6"),
            ("[PATCH,STABLE,v6.6,1/2] net: fix", "6.6"),
            ("[v6.6][STABLE] net: fix", "6.6"),
            ("[STABLE v6.6 v6.1] net: fix", "6.6"),
            ("[STABLE v6.6-v6.6] net: fix", "6.6"),
        ] {
            for body in [None, Some(body)] {
                let mut expected = Vec::new();
                if body.is_some() {
                    expected.push(BaselineResolution::Commit(
                        CommitId::parse("1234567890123456789012345678901234567890").unwrap(),
                    ));
                }
                expected.extend([
                    stable_branch(version),
                    BaselineResolution::LocalRef(format!("v{version}")),
                ]);
                expected.extend(fallback.clone());

                assert_eq!(
                    registry.resolve_candidates(&files, subject, body).await,
                    expected,
                    "{subject}, body: {body:?}"
                );
            }
        }
    }

    fn stable_branch(version: &str) -> BaselineResolution {
        BaselineResolution::RemoteTarget {
            url: "https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git".to_string(),
            name: "stable".to_string(),
            branch: Some(format!("linux-{version}.y")),
        }
    }

    #[test]
    fn test_stable_series_version_requires_complete_ascii_version() {
        for token in ["6.6", "v6.6", "V6.6", "6.6.y", "v6.6.Y"] {
            assert_eq!(stable_series_version(token), Some("6.6"), "{token}");
        }
        for token in [
            "",
            "6",
            "6.",
            ".6",
            "v",
            "v6",
            "6.6.1",
            "6.6-rc1",
            "6.6-custom",
            "6.6.y.y",
            "vv6.6",
            "6.６",
            "６.6",
            "-6.6",
            "6.6_foo",
        ] {
            assert_eq!(stable_series_version(token), None, "{token}");
        }
        assert_eq!(
            explicit_stable_versions("[STABLE v999999999999999999999999.1-v6.6]"),
            Some(("999999999999999999999999.1", Some("6.6")))
        );
        assert_eq!(explicit_stable_versions("[STABLE v6.6.Y-invalid]"), None);
        assert_eq!(explicit_stable_versions("[STABLE]"), None);
    }

    #[tokio::test]
    async fn test_resolve_stable_ranges() {
        let registry = create_registry();
        let files = vec!["net/core.c".to_string()];
        let fallback = registry
            .resolve_candidates(&files, "[PATCH] fix", None)
            .await;
        let body = "base-commit: 1234567890123456789012345678901234567890\n";

        for (subject, first, last) in [
            ("[STABLE v6.6-v6.1 1/2] net: fix", "6.6", "6.1"),
            ("[STABLE v5.15-v5.10 0/2] net: fixes", "5.15", "5.10"),
            ("[STABLE v6.1-v6.6] net: fix", "6.1", "6.6"),
            ("[STABLE v6.1-v5.15] net: fix", "6.1", "5.15"),
            ("[STABLE 6.6-6.1] net: fix", "6.6", "6.1"),
            ("[STABLE v6.6-6.1] net: fix", "6.6", "6.1"),
            ("[STABLE V6.6.Y-V6.1.Y] net: fix", "6.6", "6.1"),
            ("[PATCH v2][stable][v6.6-v6.1] net: fix", "6.6", "6.1"),
            ("[STABLE v6.6-v6.1 v5.15-v5.10] net: fix", "6.6", "6.1"),
        ] {
            for body in [None, Some(body)] {
                let mut expected = Vec::new();
                if body.is_some() {
                    expected.push(BaselineResolution::Commit(
                        CommitId::parse("1234567890123456789012345678901234567890").unwrap(),
                    ));
                }
                expected.extend([
                    stable_branch(first),
                    stable_branch(last),
                    BaselineResolution::LocalRef(format!("v{first}")),
                ]);
                expected.extend(fallback.clone());
                assert_eq!(
                    registry.resolve_candidates(&files, subject, body).await,
                    expected,
                    "{subject}, body: {body:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_resolve_exact_tags_and_non_stable_numbers() {
        let registry = create_registry();
        let files = vec!["net/core.c".to_string()];
        let fallback = registry
            .resolve_candidates(&files, "[PATCH] fix", None)
            .await;

        for (subject, version) in [
            ("[PATCH v5.10] net: fix", "5.10"),
            ("[PATCH V5.10] net: fix", "5.10"),
            ("[PATCH 5.10.123] net: fix", "5.10.123"),
            ("[PATCH v5.10.123] net: fix", "5.10.123"),
            ("[PATCH 5.10-rc1] net: fix", "5.10-rc1"),
            ("[PATCH v5.10-rc1] net: fix", "5.10-rc1"),
            ("[PATCH] fix USB 3.0 support", "3.0"),
            ("[PATCH] net: backport to 5.10", "5.10"),
            ("[PATCH device-5.10] net: fix", "5.10"),
            ("[PATCH 5.10-custom] net: fix", "5.10"),
            ("[PATCH 5.10.1.2] net: fix", "5.10.1"),
            ("[STABLE v6.6.10] net: fix", "6.6.10"),
            ("[STABLE 6.6.10] net: fix", "6.6.10"),
            ("[STABLE v6.6-rc1] net: fix", "6.6-rc1"),
            ("[STABLE 6.6-rc1] net: fix", "6.6-rc1"),
            ("[PATCH v6.6-v6.1] net: fix", "6.6"),
            ("[PATCH v6.6] stable: fix", "6.6"),
            ("[PATCH stable-next v6.6] net: fix", "6.6"),
            ("[PATCH unstable v6.6] net: fix", "6.6"),
            ("[STABLE] net: fix USB 3.0", "3.0"),
            ("[STABLE device-v6.6] net: fix", "6.6"),
            ("[STABLE v6.6-custom] net: fix", "6.6"),
            ("[STABLE v6.6-] net: fix", "6.6"),
            ("[STABLE -v6.6] net: fix", "6.6"),
            ("[STABLE v6.6--v6.1] net: fix", "6.6"),
            ("[STABLE v6.6-v6.1-v5.15] net: fix", "6.6"),
            ("[STABLE v6.6-v6.1.10] net: fix", "6.6"),
            ("[STABLE v6.6.10-v6.1] net: fix", "6.6.10"),
            ("[STABLE v6.6-v6.1-rc1] net: fix", "6.6"),
            ("[STABLE v6.6-rc1-v6.1] net: fix", "6.6-rc1"),
            ("[STABLE v6.6-v6.1_custom] net: fix", "6.6"),
        ] {
            let mut expected = vec![BaselineResolution::LocalRef(format!("v{version}"))];
            expected.extend(fallback.clone());
            assert_eq!(
                registry.resolve_candidates(&files, subject, None).await,
                expected,
                "{subject}"
            );
        }

        // Dotted patch revision tags (such as [patch V1.1 02/38]) must not
        // synthesize bogus LocalRef("v1.1") baseline candidates.
        for subject in [
            "[patch V1.1 02/38] x86/paravirt: cleanup",
            "[PATCH v2.1 1/3] sched/fair: fix",
            "[RFC PATCH v1.2] timers: prototype",
        ] {
            assert_eq!(extract_version_tag(subject), None, "{subject}");
            assert_eq!(
                registry.resolve_candidates(&files, subject, None).await,
                fallback,
                "{subject}"
            );
        }
        assert_eq!(
            extract_version_tag("[patch V1.1 02/38] x86: fix regression in v6.12"),
            Some("6.12".to_string())
        );
    }

    #[tokio::test]
    async fn test_mainline_remote_used_instead_of_head() {
        let mut remote_map = HashMap::new();
        remote_map.insert(
            "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git".to_string(),
            "origin".to_string(),
        );
        let registry = BaselineRegistry {
            entries: Vec::new(),
            remote_map,
            custom_remotes: None,
            repo_path: std::path::PathBuf::from("."),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: Some((
                "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git".to_string(),
                "origin".to_string(),
            )),
        };

        let candidates = registry.resolve_candidates(&[], "Subject", None).await;

        // The mainline candidate should be a RemoteTarget for origin/master,
        // not a LocalRef("HEAD").
        let mainline = candidates.iter().find(|c| match c {
            BaselineResolution::RemoteTarget { name, branch, .. } => {
                name == "origin" && branch.as_deref() == Some("master")
            }
            _ => false,
        });
        assert!(mainline.is_some(), "Expected RemoteTarget origin/master");

        // No LocalRef("HEAD") should be present.
        let has_head = candidates
            .iter()
            .any(|c| matches!(c, BaselineResolution::LocalRef(r) if r == "HEAD"));
        assert!(
            !has_head,
            "LocalRef(HEAD) should not be emitted when mainline_remote is set"
        );
    }

    #[tokio::test]
    async fn test_head_fallback_without_mainline_remote() {
        let registry = BaselineRegistry {
            entries: Vec::new(),
            remote_map: HashMap::new(),
            custom_remotes: None,
            repo_path: std::path::PathBuf::from("."),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: None,
        };

        let candidates = registry.resolve_candidates(&[], "Subject", None).await;

        let has_head = candidates
            .iter()
            .any(|c| matches!(c, BaselineResolution::LocalRef(r) if r == "HEAD"));
        assert!(has_head, "LocalRef(HEAD) should be present as fallback");
    }

    #[test]
    fn test_parse_maintainers_with_comments() {
        let content = "
SUBSYSTEM
T: git git://example.com/repo.git branch (comment)
F: patterns/
";
        let reader = std::io::Cursor::new(content);
        let entries = BaselineRegistry::parse_maintainers(reader).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].subsystem, "SUBSYSTEM");
        assert_eq!(entries[0].trees.len(), 1);
        assert_eq!(entries[0].trees[0].0, "git://example.com/repo.git");
        assert_eq!(entries[0].trees[0].1, Some("branch".to_string()));
    }

    #[tokio::test]
    async fn test_resolve_linux_mm() {
        let entries = vec![MaintainersEntry {
            subsystem: "MEMORY MANAGEMENT".to_string(),
            trees: vec![(
                "git://git.kernel.org/pub/scm/linux/kernel/git/akpm/mm.git".to_string(),
                None,
            )],
            patterns: vec!["mm/".to_string()],
        }];
        let mut remote_map = HashMap::new();
        remote_map.insert(
            "git://git.kernel.org/pub/scm/linux/kernel/git/akpm/mm.git".to_string(),
            "akpm-mm".to_string(),
        );

        let registry = BaselineRegistry {
            entries,
            remote_map,
            custom_remotes: None,
            repo_path: std::path::PathBuf::from("."),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: None,
        };

        let files = vec!["mm/memory.c".to_string()];
        let candidates = registry.resolve_candidates(&files, "Subject", None).await;

        // Expected order:
        // 1. mm-new (Subsystem Heuristic 1)
        // 2. mm-unstable (Subsystem Heuristic 2)
        // 3. mm-stable (Subsystem Heuristic 3)
        // 4. linux-next (Hardcoded step 3)
        // 5. HEAD (Hardcoded step 4)

        assert!(candidates.len() >= 4);

        // Helper to check candidate
        let check_branch = |c: &BaselineResolution, expected_branch: &str| {
            if let BaselineResolution::RemoteTarget { branch, .. } = c {
                assert_eq!(branch.as_deref(), Some(expected_branch));
            } else {
                panic!("Expected RemoteTarget with branch {}", expected_branch);
            }
        };

        check_branch(&candidates[0], "mm-new");
        check_branch(&candidates[1], "mm-unstable");
        check_branch(&candidates[2], "mm-stable");

        // candidates[3] is linux-next (no branch name checked here, usually None -> HEAD, but let's check name)
        if let BaselineResolution::RemoteTarget { url, .. } = &candidates[3] {
            assert!(url.contains("linux-next"));
        } else {
            panic!("Expected linux-next");
        }
    }

    fn iwl_registry() -> BaselineRegistry {
        let entries = vec![MaintainersEntry {
            subsystem: "INTEL ETHERNET DRIVERS".to_string(),
            trees: vec![
                (
                    "https://git.kernel.org/pub/scm/linux/kernel/git/tnguy/net-queue.git"
                        .to_string(),
                    None,
                ),
                (
                    "https://git.kernel.org/pub/scm/linux/kernel/git/tnguy/next-queue.git"
                        .to_string(),
                    None,
                ),
            ],
            patterns: vec!["drivers/net/ethernet/intel/".to_string()],
        }];

        BaselineRegistry {
            entries,
            remote_map: HashMap::new(),
            custom_remotes: None,
            repo_path: std::path::PathBuf::from("."),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: None,
        }
    }

    #[tokio::test]
    async fn test_resolve_iwl_tags_pick_dev_queue() {
        let registry = iwl_registry();
        let files = vec!["drivers/net/ethernet/intel/ice/ice_main.c".to_string()];

        for (subject, expected_repo) in [
            ("[PATCH iwl-net v2 1/3] ice: fix something", "net-queue"),
            ("[PATCH IWL-NEXT] ice: add something", "next-queue"),
        ] {
            let candidates = registry.resolve_candidates(&files, subject, None).await;

            match &candidates[0] {
                BaselineResolution::RemoteTarget { url, branch, .. } => {
                    assert!(url.contains(expected_repo), "{} -> {}", subject, url);
                    assert_eq!(branch.as_deref(), Some("dev-queue"));
                }
                other => panic!("Expected dev-queue RemoteTarget, got {:?}", other),
            }
        }
    }

    #[tokio::test]
    async fn test_resolve_intel_without_iwl_tag_keeps_heuristic() {
        let registry = iwl_registry();
        let files = vec!["drivers/net/ethernet/intel/ice/ice_main.c".to_string()];

        let candidates = registry
            .resolve_candidates(&files, "[PATCH net] ice: fix something", None)
            .await;

        let has_dev_queue = candidates.iter().any(|c| {
            matches!(c, BaselineResolution::RemoteTarget { branch, .. }
                if branch.as_deref() == Some("dev-queue"))
        });
        assert!(!has_dev_queue, "dev-queue requires an iwl tag");
    }

    #[tokio::test]
    async fn test_resolve_staging() {
        // gregkh/staging.git's HEAD points at "main", which tracks what has
        // already landed upstream, not the branch new patches apply to
        // (staging-testing). The MAINTAINERS T: line carries no branch, so
        // without the special case this would resolve to HEAD and fail to
        // apply cleanly against the active tree (issue #268).
        let entries = vec![MaintainersEntry {
            subsystem: "STAGING SUBSYSTEM".to_string(),
            trees: vec![(
                "git://git.kernel.org/pub/scm/linux/kernel/git/gregkh/staging.git".to_string(),
                None,
            )],
            patterns: vec!["drivers/staging/".to_string()],
        }];
        let mut remote_map = HashMap::new();
        remote_map.insert(
            "git://git.kernel.org/pub/scm/linux/kernel/git/gregkh/staging.git".to_string(),
            "gregkh-staging".to_string(),
        );

        let registry = BaselineRegistry {
            entries,
            remote_map,
            custom_remotes: None,
            repo_path: std::path::PathBuf::from("."),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: None,
        };

        let files = vec!["drivers/staging/foo/bar.c".to_string()];
        let candidates = registry.resolve_candidates(&files, "Subject", None).await;

        assert!(candidates.len() >= 2);

        let check_branch = |c: &BaselineResolution, expected_branch: &str| {
            if let BaselineResolution::RemoteTarget { branch, .. } = c {
                assert_eq!(branch.as_deref(), Some(expected_branch));
            } else {
                panic!("Expected RemoteTarget with branch {}", expected_branch);
            }
        };

        check_branch(&candidates[0], "staging-testing");
        check_branch(&candidates[1], "staging-next");
    }

    #[tokio::test]
    async fn test_resolve_staging_does_not_hijack_other_subsystem() {
        // A series can touch a Staging file alongside files from an
        // unrelated subsystem. The Staging special case must only fire
        // when the winning tree (by file count) actually belongs to
        // Staging -- not merely because a Staging file happened to be the
        // first one matched while iterating `files`.
        let staging_url =
            "git://git.kernel.org/pub/scm/linux/kernel/git/gregkh/staging.git".to_string();
        let net_url = "git://git.kernel.org/pub/scm/linux/kernel/git/netdev/net.git".to_string();

        let entries = vec![
            MaintainersEntry {
                subsystem: "STAGING SUBSYSTEM".to_string(),
                trees: vec![(staging_url.clone(), None)],
                patterns: vec!["drivers/staging/".to_string()],
            },
            MaintainersEntry {
                subsystem: "NETWORKING [GENERAL]".to_string(),
                trees: vec![(net_url.clone(), None)],
                patterns: vec!["net/".to_string()],
            },
        ];

        let registry = BaselineRegistry {
            entries,
            remote_map: HashMap::new(),
            custom_remotes: None,
            repo_path: std::path::PathBuf::from("."),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: None,
        };

        // The Staging file is listed first, but three Networking files
        // outnumber it, so net_url must be the tree that wins.
        let files = vec![
            "drivers/staging/foo/bar.c".to_string(),
            "net/core/a.c".to_string(),
            "net/core/b.c".to_string(),
            "net/core/c.c".to_string(),
        ];
        let candidates = registry.resolve_candidates(&files, "Subject", None).await;

        for candidate in &candidates {
            if let BaselineResolution::RemoteTarget { url, branch, .. } = candidate
                && url == &net_url
            {
                assert_ne!(
                    branch.as_deref(),
                    Some("staging-testing"),
                    "net_url must not be resolved with a staging branch"
                );
                assert_ne!(
                    branch.as_deref(),
                    Some("staging-next"),
                    "net_url must not be resolved with a staging branch"
                );
            }
        }
    }

    #[tokio::test]
    async fn test_resolve_multiple_trees() {
        let entries = vec![MaintainersEntry {
            subsystem: "PERFORMANCE EVENTS SUBSYSTEM".to_string(),
            trees: vec![
                (
                    "git://git.kernel.org/pub/scm/linux/kernel/git/tip/tip.git".to_string(),
                    Some("perf/core".to_string()),
                ),
                (
                    "git://git.kernel.org/pub/scm/linux/kernel/git/perf/perf-tools.git".to_string(),
                    Some("perf-tools".to_string()),
                ),
                (
                    "git://git.kernel.org/pub/scm/linux/kernel/git/perf/perf-tools-next.git"
                        .to_string(),
                    Some("perf-tools-next".to_string()),
                ),
            ],
            patterns: vec!["tools/perf/".to_string()],
        }];
        let mut remote_map = HashMap::new();
        remote_map.insert(
            "git://git.kernel.org/pub/scm/linux/kernel/git/tip/tip.git".to_string(),
            "tip".to_string(),
        );

        let registry = BaselineRegistry {
            entries,
            remote_map,
            custom_remotes: None,
            repo_path: std::path::PathBuf::from("."),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: None,
        };

        let files = vec!["tools/perf/builtin-report.c".to_string()];
        let candidates = registry.resolve_candidates(&files, "Subject", None).await;

        // Current implementation likely only returns ONE of the trees (arbitrarily or first)
        // plus linux-next and HEAD.
        // We want ALL three trees to be present.

        let candidate_names: Vec<String> = candidates
            .iter()
            .filter_map(|c| match c {
                BaselineResolution::RemoteTarget { url, .. } => Some(url.clone()),
                _ => None,
            })
            .collect();

        assert!(candidate_names.iter().any(|n| n.contains("tip.git")));
        assert!(candidate_names.iter().any(|n| n.contains("perf-tools.git")));
        assert!(
            candidate_names
                .iter()
                .any(|n| n.contains("perf-tools-next.git"))
        );
    }

    #[tokio::test]
    async fn test_resolve_next_prioritization() {
        let entries = vec![MaintainersEntry {
            subsystem: "NETWORKING".to_string(),
            trees: vec![
                ("git://net.git".to_string(), Some("master".to_string())),
                ("git://net-next.git".to_string(), Some("master".to_string())),
            ],
            patterns: vec!["net/".to_string()],
        }];
        let mut remote_map = HashMap::new();
        remote_map.insert("git://net.git".to_string(), "net".to_string());
        remote_map.insert("git://net-next.git".to_string(), "net-next".to_string());

        let registry = BaselineRegistry {
            entries,
            remote_map,
            custom_remotes: None,
            repo_path: std::path::PathBuf::from("."),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: None,
        };

        let files = vec!["net/core.c".to_string()];

        // With "next" in subject
        let candidates_next = registry
            .resolve_candidates(&files, "[PATCH net-next] something", None)
            .await;
        let names_next: Vec<String> = candidates_next
            .iter()
            .filter_map(|c| match c {
                BaselineResolution::RemoteTarget { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();

        // Should prioritize net-next over net
        let pos_net_next = names_next.iter().position(|x| x == "net-next").unwrap();
        let pos_net = names_next.iter().position(|x| x == "net").unwrap();
        assert!(pos_net_next < pos_net);

        // Without "next" in subject
        let candidates_nonext = registry
            .resolve_candidates(&files, "[PATCH net] something", None)
            .await;
        let names_nonext: Vec<String> = candidates_nonext
            .iter()
            .filter_map(|c| match c {
                BaselineResolution::RemoteTarget { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();

        // Should prioritize net over net-next
        let pos_net_nonext = names_nonext.iter().position(|x| x == "net").unwrap();
        let pos_net_next_nonext = names_nonext.iter().position(|x| x == "net-next").unwrap();
        assert!(pos_net_nonext < pos_net_next_nonext);
    }

    #[tokio::test]
    async fn test_resolve_custom_remotes_precede_linux_next() {
        use crate::settings::CustomRemoteSettings;
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        let mut remote_map = HashMap::new();
        let dummy_url = repo_path.join("dummy_remote").to_str().unwrap().to_string();
        remote_map.insert(dummy_url.clone(), "dummy-repo".to_string());

        let registry = BaselineRegistry {
            entries: Vec::new(),
            remote_map,
            custom_remotes: Some(vec![CustomRemoteSettings {
                name: "dummy-repo".to_string(),
                url: dummy_url,
                check_all_branches: false,
                only_branches: Some(vec!["master".to_string()]),
                branch_patterns: None,
            }]),
            repo_path: repo_path.to_path_buf(),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: None,
        };

        let candidates = registry.resolve_candidates(&[], "Subject", None).await;

        let candidate_names: Vec<String> = candidates
            .iter()
            .filter_map(|c| match c {
                BaselineResolution::RemoteTarget { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();

        let pos_custom = candidate_names
            .iter()
            .position(|n| n == "dummy-repo")
            .unwrap();
        let pos_next = candidate_names
            .iter()
            .position(|n| n == "linux-next")
            .unwrap();
        assert!(
            pos_custom < pos_next,
            "custom remote at {} should precede linux-next at {}: {:?}",
            pos_custom,
            pos_next,
            candidate_names
        );
    }

    #[tokio::test]
    async fn test_resolve_custom_remotes_branch_patterns() {
        use crate::settings::CustomRemoteSettings;
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        crate::git_cmd::in_dir(repo_path)
            .args(["-c", "safe.bareRepository=all", "init", "-b", "main"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(repo_path)
            .args(["config", "user.name", "Test"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .unwrap();
        std::fs::write(repo_path.join("file.txt"), "hello").unwrap();
        crate::git_cmd::in_dir(repo_path)
            .args(["add", "file.txt"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(repo_path)
            .args(["commit", "-m", "init"])
            .output()
            .unwrap();

        for branch in &["6.18/dev", "6.18/prod", "other/feature"] {
            crate::git_cmd::in_dir(repo_path)
                .args(["branch", branch])
                .output()
                .unwrap();
        }

        let client_dir = tempfile::tempdir().unwrap();
        let client_path = client_dir.path();
        crate::git_cmd::in_dir(client_path)
            .args(["-c", "safe.bareRepository=all", "init", "-b", "main"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(client_path)
            .args(["config", "user.name", "Test"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(client_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .unwrap();

        let dummy_url = repo_path.to_str().unwrap().to_string();
        let mut remote_map = HashMap::new();
        remote_map.insert(dummy_url.clone(), "custom".to_string());

        let registry = BaselineRegistry {
            entries: Vec::new(),
            remote_map,
            custom_remotes: Some(vec![CustomRemoteSettings {
                name: "custom".to_string(),
                url: dummy_url,
                check_all_branches: true,
                only_branches: None,
                branch_patterns: Some(vec!["6.18/*".to_string()]),
            }]),
            repo_path: client_path.to_path_buf(),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: None,
        };

        let candidates = registry.resolve_candidates(&[], "Subject", None).await;
        let candidate_branches: Vec<String> = candidates
            .iter()
            .filter_map(|c| match c {
                BaselineResolution::RemoteTarget { name, branch, .. } if name == "custom" => {
                    branch.clone()
                }
                _ => None,
            })
            .collect();

        assert!(candidate_branches.contains(&"6.18/dev".to_string()));
        assert!(candidate_branches.contains(&"6.18/prod".to_string()));
        assert!(!candidate_branches.contains(&"other/feature".to_string()));
        assert!(!candidate_branches.contains(&"main".to_string()));
    }

    #[tokio::test]
    async fn test_resolve_custom_remotes_additive_only_branches() {
        use crate::settings::CustomRemoteSettings;
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        crate::git_cmd::in_dir(repo_path)
            .args(["-c", "safe.bareRepository=all", "init", "-b", "main"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(repo_path)
            .args(["config", "user.name", "Test"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .unwrap();
        std::fs::write(repo_path.join("file.txt"), "hello").unwrap();
        crate::git_cmd::in_dir(repo_path)
            .args(["add", "file.txt"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(repo_path)
            .args(["commit", "-m", "init"])
            .output()
            .unwrap();

        for branch in &["6.18/dev", "explicit-topic", "other/feature"] {
            crate::git_cmd::in_dir(repo_path)
                .args(["branch", branch])
                .output()
                .unwrap();
        }

        let client_dir = tempfile::tempdir().unwrap();
        let client_path = client_dir.path();
        crate::git_cmd::in_dir(client_path)
            .args(["-c", "safe.bareRepository=all", "init", "-b", "main"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(client_path)
            .args(["config", "user.name", "Test"])
            .output()
            .unwrap();
        crate::git_cmd::in_dir(client_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .unwrap();

        let dummy_url = repo_path.to_str().unwrap().to_string();
        let mut remote_map = HashMap::new();
        remote_map.insert(dummy_url.clone(), "custom".to_string());

        // branch_patterns matches 6.18/dev, only_branches adds explicit-topic
        let registry = BaselineRegistry {
            entries: Vec::new(),
            remote_map,
            custom_remotes: Some(vec![CustomRemoteSettings {
                name: "custom".to_string(),
                url: dummy_url,
                check_all_branches: false,
                only_branches: Some(vec!["explicit-topic".to_string()]),
                branch_patterns: Some(vec!["6.18/*".to_string()]),
            }]),
            repo_path: client_path.to_path_buf(),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: None,
        };

        let candidates = registry.resolve_candidates(&[], "Subject", None).await;
        let candidate_branches: Vec<String> = candidates
            .iter()
            .filter_map(|c| match c {
                BaselineResolution::RemoteTarget { name, branch, .. } if name == "custom" => {
                    branch.clone()
                }
                _ => None,
            })
            .collect();

        assert!(candidate_branches.contains(&"6.18/dev".to_string()));
        assert!(candidate_branches.contains(&"explicit-topic".to_string()));
        assert!(!candidate_branches.contains(&"other/feature".to_string()));
    }

    #[tokio::test]
    async fn test_resolve_custom_remotes_empty_lists_fallback_to_default() {
        use crate::settings::CustomRemoteSettings;
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        let registry = BaselineRegistry {
            entries: Vec::new(),
            remote_map: HashMap::new(),
            custom_remotes: Some(vec![CustomRemoteSettings {
                name: "custom".to_string(),
                url: "https://example.com/repo.git".to_string(),
                check_all_branches: false,
                only_branches: Some(vec![]),
                branch_patterns: Some(vec![]),
            }]),
            repo_path: repo_path.to_path_buf(),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: None,
        };

        let candidates = registry.resolve_candidates(&[], "Subject", None).await;
        let has_default = candidates.iter().any(|c| match c {
            BaselineResolution::RemoteTarget { name, branch, .. } => {
                name == "custom" && branch.is_none()
            }
            _ => false,
        });

        assert!(has_default, "empty lists must fall back to default branch");
    }

    /// Runs git in the test repository and returns its trimmed stdout.
    async fn git(repo: &Path, args: &[&str]) -> String {
        let out = crate::git_cmd::in_dir_async(repo)
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(out.status.success(), "git {:?} failed: {:?}", args, out);
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository whose mainline remote "origin" has the history
    /// old -> mid -> tip, plus a commit "side" that branches off mid and
    /// is not in mainline. Returns the registry and the four SHAs.
    async fn merged_head_fixture(dir: &Path) -> (BaselineRegistry, String, String, String, String) {
        git(dir, &["init", "--quiet"]).await;
        git(dir, &["config", "user.email", "test@example.com"]).await;
        git(dir, &["config", "user.name", "Test User"]).await;
        let mut shas = Vec::new();
        for msg in ["old", "mid", "tip"] {
            git(dir, &["commit", "--quiet", "--allow-empty", "-m", msg]).await;
            shas.push(git(dir, &["rev-parse", "HEAD"]).await);
        }
        git(dir, &["update-ref", "refs/remotes/origin/master", &shas[2]]).await;
        git(dir, &["checkout", "--quiet", "--detach", &shas[1]]).await;
        git(dir, &["commit", "--quiet", "--allow-empty", "-m", "side"]).await;
        let side = git(dir, &["rev-parse", "HEAD"]).await;

        let url = "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git";
        let registry = BaselineRegistry {
            entries: Vec::new(),
            remote_map: HashMap::new(),
            custom_remotes: None,
            repo_path: dir.to_path_buf(),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: Some((url.to_string(), "origin".to_string())),
        };
        let [old, mid, tip]: [String; 3] = shas.try_into().unwrap();
        (registry, old, mid, tip, side)
    }

    fn default_head(name: &str) -> BaselineResolution {
        BaselineResolution::RemoteTarget {
            url: format!("git://git.kernel.org/pub/scm/linux/kernel/git/{}.git", name),
            name: name.to_string(),
            branch: None,
        }
    }

    #[tokio::test]
    async fn test_merged_default_head_detection() {
        let dir = tempfile::tempdir().unwrap();
        let (mut registry, old, mid, tip, side) = merged_head_fixture(dir.path()).await;

        // tytso/ext4.git: the T: entry names no branch and HEAD is a
        // master that mainline passed years ago.
        let ext4 = default_head("tytso/ext4");
        assert!(registry.is_merged_default_head(&ext4, &old).await);
        assert!(registry.is_merged_default_head(&ext4, &mid).await);

        let tree = default_head("some/tree");
        // Level with mainline: nothing is stale yet.
        assert!(!registry.is_merged_default_head(&tree, &tip).await);
        // Carries work mainline does not have.
        assert!(!registry.is_merged_default_head(&tree, &side).await);

        let explicit_branch = BaselineResolution::RemoteTarget {
            url: "git://git.kernel.org/pub/scm/linux/kernel/git/tytso/ext4.git".to_string(),
            name: "ext4".to_string(),
            branch: Some("dev".to_string()),
        };
        let base_commit = BaselineResolution::Commit(CommitId::parse(&old).unwrap());
        let version_tag = BaselineResolution::LocalRef("v5.10".to_string());
        for candidate in [explicit_branch, base_commit, version_tag] {
            assert!(
                !registry.is_merged_default_head(&candidate, &old).await,
                "{:?} was chosen by someone and must not be deferred",
                candidate
            );
        }

        // linux-next is also a candidate without a branch.
        let next = BaselineResolution::RemoteTarget {
            url: LINUX_NEXT_URL.to_string(),
            name: "linux-next".to_string(),
            branch: None,
        };
        assert!(!registry.is_merged_default_head(&next, &old).await);

        // No mainline ref to compare against: keep the old behaviour.
        registry.mainline_remote = Some(("unused".to_string(), "nosuch".to_string()));
        assert!(
            !registry
                .is_merged_default_head(&default_head("tytso/ext4"), &old)
                .await
        );
    }

    #[test]
    fn test_offline_git_disables_lazy_fetch() {
        let cmd = offline_git(Path::new("."), &["rev-parse", "HEAD"]);
        let envs: Vec<_> = cmd.as_std().get_envs().collect();
        assert!(envs.contains(&(
            std::ffi::OsStr::new("GIT_NO_LAZY_FETCH"),
            Some(std::ffi::OsStr::new("1"))
        )));
    }

    #[tokio::test]
    async fn test_fork_origin_uses_configured_project() {
        for (project, upstream) in [
            (
                ProjectId::Sashiko,
                "https://github.com/sashiko-dev/sashiko.git",
            ),
            (
                ProjectId::Zephyr,
                "https://github.com/zephyrproject-rtos/zephyr.git",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let repo = dir.path();
            git(repo, &["init", "--quiet"]).await;
            let fork = "https://github.com/contributor/renamed-tree.git";
            git(repo, &["remote", "add", "origin", fork]).await;
            git(repo, &["remote", "add", "z-fork", fork]).await;
            git(repo, &["remote", "set-url", "--push", "origin", upstream]).await;
            git(
                repo,
                &[
                    "remote",
                    "add",
                    "linux",
                    "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git",
                ],
            )
            .await;

            let registry = BaselineRegistry::new(repo, None, Some(project)).unwrap();
            assert_eq!(registry.mainline_ref(), "origin/main");
            assert!(registry.entries.is_empty());
            assert_eq!(
                registry
                    .resolve_candidates(&["src/main.c".to_string()], "[PATCH] fix cleanup", None)
                    .await,
                vec![BaselineResolution::RemoteTarget {
                    url: fork.to_string(),
                    name: "origin".to_string(),
                    branch: Some("main".to_string()),
                }]
            );

            git(repo, &["remote", "add", "upstream", upstream]).await;
            let registry = BaselineRegistry::new(repo, None, Some(project)).unwrap();
            assert_eq!(registry.mainline_ref(), "upstream/main");
            assert_eq!(
                registry
                    .resolve_candidates(&[], "[PATCH] fix cleanup", None)
                    .await,
                vec![
                    BaselineResolution::RemoteTarget {
                        url: upstream.to_string(),
                        name: "upstream".to_string(),
                        branch: Some("main".to_string()),
                    },
                    BaselineResolution::RemoteTarget {
                        url: fork.to_string(),
                        name: "origin".to_string(),
                        branch: Some("main".to_string()),
                    },
                ]
            );

            for project in [None, Some(ProjectId::Linux)] {
                let registry = BaselineRegistry::new(repo, None, project).unwrap();
                assert_eq!(registry.mainline_ref(), "linux/master");
                assert!(
                    registry
                        .resolve_candidates(&[], "[PATCH] fix cleanup", None)
                        .await
                        .iter()
                        .any(|candidate| matches!(
                            candidate,
                            BaselineResolution::RemoteTarget { url, .. } if url == LINUX_NEXT_URL
                        ))
                );
            }
        }
    }

    #[tokio::test]
    async fn configured_linux_overrides_fork_url_hint() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "--quiet"]).await;
        git(
            repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/sashiko-dev/linux.git",
            ],
        )
        .await;
        for (project, expect_linux_next) in [(Some(ProjectId::Linux), true), (None, false)] {
            let registry = BaselineRegistry::new(repo, None, project).unwrap();
            let candidates = registry.resolve_candidates(&[], "fix cleanup", None).await;
            assert_eq!(
                candidates.iter().any(|candidate| matches!(candidate,
                    BaselineResolution::RemoteTarget { url, .. } if url == LINUX_NEXT_URL
                )),
                expect_linux_next
            );
        }
    }

    #[tokio::test]
    async fn test_non_linux_mainline_skips_linux_next() {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        let registry = BaselineRegistry {
            entries: Vec::new(),
            remote_map: HashMap::new(),
            custom_remotes: None,
            repo_path: repo_path.to_path_buf(),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: Some((
                "https://gcc.gnu.org/git/gcc.git".to_string(),
                "origin".to_string(),
            )),
        };

        let candidates = registry
            .resolve_candidates(&[], "[PATCH] tree-optimization: fix vrp", None)
            .await;
        assert_eq!(
            candidates,
            vec![BaselineResolution::RemoteTarget {
                url: "https://gcc.gnu.org/git/gcc.git".to_string(),
                name: "origin".to_string(),
                branch: Some("master".to_string()),
            }]
        );

        let qemu_registry = BaselineRegistry {
            entries: Vec::new(),
            remote_map: HashMap::new(),
            custom_remotes: None,
            repo_path: repo_path.to_path_buf(),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: Some((
                "https://gitlab.com/qemu-project/qemu.git".to_string(),
                "origin".to_string(),
            )),
        };

        let qemu_candidates = qemu_registry
            .resolve_candidates(&[], "[PATCH] target/riscv: fix CSR write", None)
            .await;
        assert_eq!(
            qemu_candidates,
            vec![BaselineResolution::RemoteTarget {
                url: "https://gitlab.com/qemu-project/qemu.git".to_string(),
                name: "origin".to_string(),
                branch: Some("master".to_string()),
            }]
        );
    }

    #[tokio::test]
    async fn test_gcc_release_branch_subject_candidates() {
        let temp_dir = tempfile::tempdir().unwrap();
        let registry = BaselineRegistry {
            entries: Vec::new(),
            remote_map: HashMap::new(),
            custom_remotes: None,
            repo_path: temp_dir.path().to_path_buf(),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: Some((
                "git://gcc.gnu.org/git/gcc.git".to_string(),
                "origin".to_string(),
            )),
        };

        let candidates = registry
            .resolve_candidates(&[], "[PATCH gcc-15] c++: backport fix", None)
            .await;
        assert_eq!(
            candidates,
            vec![
                BaselineResolution::LocalRef("origin/releases/gcc-15".to_string()),
                BaselineResolution::LocalRef("releases/gcc-15".to_string()),
                BaselineResolution::RemoteTarget {
                    url: "git://gcc.gnu.org/git/gcc.git".to_string(),
                    name: "origin".to_string(),
                    branch: Some("master".to_string()),
                },
            ]
        );
    }

    #[test]
    fn test_extract_files_from_diff_supports_quilt_and_git_formats() {
        let quilt_diff = "\
Commit description.

---
 kernel/sched/fair.c  | 4 ++--
 kernel/sched/new.c   | 3 +++
 kernel/sched/old.c   | 3 ---
 kernel/time/timer.c  | 2 +-
 4 files changed, 6 insertions(+), 6 deletions(-)

--- a/kernel/sched/fair.c
+++ b/kernel/sched/fair.c
@@ -10,4 +10,4 @@
--- inside_hunk_old
+++ inside_hunk_new
 context
--- /dev/null
+++ b/kernel/sched/new.c
@@ -0,0 +1,3 @@
+int x;
--- a/kernel/sched/old.c
+++ /dev/null
@@ -1,3 +0,0 @@
-int y;
Index: linux/kernel/time/timer.c
===================================================================
--- linux.orig/kernel/time/timer.c\t2026-11-05 12:00:00.000000000 +0100
+++ linux/kernel/time/timer.c\t2026-11-05 12:01:00.000000000 +0100
@@ -5,2 +5,2 @@
-old
+new
";
        assert_eq!(
            extract_files_from_diff(quilt_diff),
            vec![
                "kernel/sched/fair.c",
                "kernel/sched/new.c",
                "kernel/sched/old.c",
                "kernel/time/timer.c",
            ]
        );

        // Standard git format-patch headers must not double-count their --- / +++ pair,
        // and path traversal or option-like paths are rejected.
        let mixed = "\
diff --git a/kernel/irq/manage.c b/kernel/irq/manage.c
index 1111111..2222222 100644
--- a/kernel/irq/manage.c
+++ b/kernel/irq/manage.c
@@ -1,2 +1,2 @@
-a
+b
--- a/../etc/passwd
+++ b/../etc/passwd
@@ -1 +1 @@
-x
+y
--- a/--option
+++ b/--option
@@ -1 +1 @@
-x
+y
";
        assert_eq!(extract_files_from_diff(mixed), vec!["kernel/irq/manage.c"]);
    }

    #[tokio::test]
    async fn test_resolve_tip_urgent_master_and_objtool() {
        let tip_url = "git://git.kernel.org/pub/scm/linux/kernel/git/tip/tip.git".to_string();
        let entries = vec![
            MaintainersEntry {
                subsystem: "SCHEDULER".to_string(),
                trees: vec![(tip_url.clone(), Some("sched/core".to_string()))],
                patterns: vec!["kernel/sched/".to_string()],
            },
            MaintainersEntry {
                subsystem: "OBJTOOL".to_string(),
                trees: Vec::new(),
                patterns: vec!["tools/objtool/".to_string()],
            },
        ];
        let mut remote_map = HashMap::new();
        remote_map.insert(tip_url.clone(), "tip".to_string());
        let registry = BaselineRegistry {
            entries,
            remote_map,
            custom_remotes: None,
            repo_path: std::path::PathBuf::from("."),
            project: None,
            fetch_remotes: Vec::new(),
            mainline_remote: None,
        };

        let sched_files = vec!["kernel/sched/fair.c".to_string()];
        let tip_branches = |cands: &[BaselineResolution]| -> Vec<String> {
            cands
                .iter()
                .filter_map(|c| match c {
                    BaselineResolution::RemoteTarget {
                        name,
                        branch: Some(b),
                        ..
                    } if name == "tip" => Some(b.clone()),
                    _ => None,
                })
                .collect()
        };

        let normal = registry
            .resolve_candidates(
                &sched_files,
                "[PATCH 0/3] sched/fair: Fix cgroup::idle preemption",
                None,
            )
            .await;
        assert_eq!(
            tip_branches(&normal),
            vec!["sched/core", "sched/urgent", "master"]
        );

        let urgent = registry
            .resolve_candidates(&sched_files, "[PATCH sched/urgent] sched/fair: Fix", None)
            .await;
        assert_eq!(
            tip_branches(&urgent),
            vec!["sched/urgent", "sched/core", "master"]
        );

        let objtool = registry
            .resolve_candidates(
                &["tools/objtool/check.c".to_string()],
                "[PATCH] objtool: Fix jump table handling",
                None,
            )
            .await;
        assert_eq!(
            tip_branches(&objtool),
            vec!["objtool/core", "objtool/urgent", "master"]
        );
    }

    #[tokio::test]
    async fn test_resolve_body_version_tag_and_unversioned_stable_backport() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "--quiet"]).await;
        git(repo, &["config", "user.email", "test@example.com"]).await;
        git(repo, &["config", "user.name", "Test User"]).await;

        let commit_at = |msg: &'static str, date: &'static str| async move {
            let out = crate::git_cmd::in_dir_async(repo)
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date)
                .args(["commit", "--quiet", "--allow-empty", "-m", msg])
                .output()
                .await
                .unwrap();
            assert!(out.status.success(), "git commit failed: {:?}", out);
            git(repo, &["rev-parse", "HEAD"]).await
        };

        let old_sha = commit_at("old eol", "2025-01-01T00:00:00Z").await;
        git(
            repo,
            &["update-ref", "refs/remotes/stable/linux-6.17.y", &old_sha],
        )
        .await;

        let lts_sha = commit_at("lts", "2026-10-01T00:00:00Z").await;
        git(
            repo,
            &["update-ref", "refs/remotes/stable/linux-6.12.y", &lts_sha],
        )
        .await;
        git(
            repo,
            &["update-ref", "refs/remotes/stable/linux-6.18.y", &lts_sha],
        )
        .await;

        let mut remote_map = HashMap::new();
        remote_map.insert(STABLE_LINUX_URL.to_string(), "stable".to_string());
        let registry = BaselineRegistry {
            entries: Vec::new(),
            remote_map,
            custom_remotes: None,
            repo_path: repo.to_path_buf(),
            project: Some(ProjectId::Linux),
            fetch_remotes: Vec::new(),
            mainline_remote: None,
        };

        // 1. Cover-letter baseline tag ("applies on top of v7.0-rc3")
        let cover_body = "Hi!\n\nThe series applies on top of v7.0-rc3 and reworks /proc/interrupts.\n> applies on top of v6.1\n";
        let cands = registry
            .resolve_candidates(
                &[],
                "[patch V5 00/15] Improve /proc/interrupts further",
                Some(cover_body),
            )
            .await;
        assert_eq!(
            cands[0],
            BaselineResolution::LocalRef("v7.0-rc3".to_string())
        );
        assert_eq!(
            extract_body_version_tag("Commit msg\n---\nChanges in v4:\n- Rebase on v7.2-rc1\n"),
            Some("7.2-rc1".to_string())
        );
        assert_eq!(
            extract_body_version_tag("Based on v1.1 of the RFC series\n"),
            None
        );

        // 2. Unversioned [PATCH stable backport] with "commit <sha> upstream."
        let backport_body =
            "commit 4096fd0e8eaea13ebe5206700b33f49635ae18e5 upstream.\n\nFix leak.\n";
        let backport_cands = registry
            .resolve_candidates(
                &[],
                "[PATCH stable backport] clockevents: Fix resource leak",
                Some(backport_body),
            )
            .await;
        let stable_branches: Vec<String> = backport_cands
            .iter()
            .filter_map(|c| match c {
                BaselineResolution::RemoteTarget {
                    name,
                    branch: Some(b),
                    ..
                } if name == "stable" => Some(b.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            stable_branches,
            vec!["linux-6.18.y", "linux-6.12.y", "linux-6.17.y"]
        );

        // 3. Normal mainline patch carrying Cc: stable@vger.kernel.org must NOT
        //    trigger stable branch candidates.
        let mainline_body =
            "Fix race.\n\nCc: stable@vger.kernel.org\nSigned-off-by: Dev <d@k.org>\n";
        let mainline_cands = registry
            .resolve_candidates(&[], "[PATCH] clockevents: Fix race", Some(mainline_body))
            .await;
        assert!(!mainline_cands.iter().any(|c| matches!(
            c,
            BaselineResolution::RemoteTarget { name, .. } if name == "stable"
        )));
    }
}
