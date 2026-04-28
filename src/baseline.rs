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

use crate::settings::CustomRemoteSettings;
use anyhow::Result;
use regex::Regex;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;
use std::sync::OnceLock;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct MaintainersEntry {
    pub subsystem: String,
    pub trees: Vec<(String, Option<String>)>, // (URL, Branch)
    pub patterns: Vec<String>,
}

#[derive(Debug, PartialEq, Clone)]
pub enum BaselineResolution {
    Commit(String),   // Explicit base-commit hash
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
            BaselineResolution::Commit(h) => h.clone(),
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
}

impl BaselineRegistry {
    pub fn new(
        repo_path: &Path,
        custom_remotes: Option<Vec<CustomRemoteSettings>>,
    ) -> Result<Self> {
        let remote_map = Self::load_git_remotes(repo_path).unwrap_or_default();

        // Identify Linus's tree
        let linus_remote = remote_map
            .iter()
            .find(|(url, _)| url.contains("torvalds/linux.git"))
            .map(|(_, name)| name.as_str())
            .unwrap_or("origin");

        let ref_name = format!("{}/master", linus_remote);
        info!(
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
                    info!("Loading MAINTAINERS from local file {:?}", maintainers_path);
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
            custom_remotes,
            repo_path: repo_path.to_path_buf(),
        })
    }

    fn read_file_from_git(repo_path: &Path, rev: &str, file_path: &str) -> Result<String> {
        use std::process::Command;
        let output = Command::new("git")
            .current_dir(repo_path)
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

        info!("Parsed {} MAINTAINERS entries", entries.len());
        Ok(entries)
    }

    fn load_git_remotes(repo_path: &Path) -> Result<HashMap<String, String>> {
        use std::process::Command;

        let output = Command::new("git")
            .current_dir(repo_path)
            .args(["remote", "-v"])
            .output()?;

        let mut map = HashMap::new();
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let name = parts[0];
                    let url = parts[1];
                    map.insert(url.to_string(), name.to_string());
                    if let Some(stripped) = url.strip_suffix(".git") {
                        map.insert(stripped.to_string(), name.to_string());
                    }
                }
            }
        }
        Ok(map)
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

        // 1.5 Version Tag from Subject
        if let Some(version) = extract_version_tag(subject) {
            if version.ends_with(".y") {
                candidates.push(BaselineResolution::RemoteTarget {
                    url: "https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git"
                        .to_string(),
                    name: "stable".to_string(),
                    branch: Some(format!("linux-{}", version)),
                });
                let base_version = version.strip_suffix(".y").unwrap();
                candidates.push(BaselineResolution::LocalRef(format!("v{}", base_version)));
            } else {
                candidates.push(BaselineResolution::LocalRef(format!("v{}", version)));
            }
        }

        // 2. Subsystem Heuristic
        let heuristic_candidates = self.resolve_subsystem_heuristic(files, subject);
        candidates.extend(heuristic_candidates);

        // 3. Linux Next
        // Hardcoded linux-next URL
        let linux_next_url = "https://git.kernel.org/pub/scm/linux/kernel/git/next/linux-next.git";
        candidates.push(self.resolve_url(linux_next_url, None));

        // 4. Mainline (Local Origin/Master or HEAD)
        // We assume 'origin' is mainline if available, or just HEAD.
        // Or if we can find 'torvalds/linux.git' in remote map.
        // For simplicity: HEAD.
        candidates.push(BaselineResolution::LocalRef("HEAD".to_string()));

        // 5. Custom Remotes
        if let Some(custom_remotes) = &self.custom_remotes {
            for remote in custom_remotes {
                // Fetch to ensure we have the latest branches (Issue 1)
                if let Err(e) =
                    crate::git_ops::ensure_remote(&self.repo_path, &remote.name, &remote.url, false)
                        .await
                {
                    warn!(
                        "Failed to ensure custom remote {}: {}. Using local branches.",
                        remote.name, e
                    );
                }

                if remote.check_all_branches {
                    match crate::git_ops::get_remote_branches(&self.repo_path, &remote.name).await {
                        Ok(branches) => {
                            for branch in branches {
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

                // Change else if to if (Issue 3)
                if let Some(branches) = &remote.only_branches {
                    for branch in branches {
                        candidates.push(BaselineResolution::RemoteTarget {
                            url: remote.url.clone(),
                            name: remote.name.clone(),
                            branch: Some(branch.clone()),
                        });
                    }
                }
            }
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
        let mut matched_subsystem_name = None;

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
                    // Capture the subsystem name of the first match (simplified heuristic)
                    if matched_subsystem_name.is_none() {
                        matched_subsystem_name = Some(entry.subsystem.clone());
                    }

                    for tree in &entry.trees {
                        *tree_counts.entry(tree.clone()).or_insert(0) += 1;
                    }
                }
            }
        }

        if tree_counts.is_empty() {
            return Vec::new();
        }

        let mut candidates: Vec<(&(String, Option<String>), &usize)> = tree_counts.iter().collect();
        candidates.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.0.cmp(&b.0.0)));

        // Check for Linux-MM special handling
        // If the top candidate is akpm/mm or linux-mm, OR the subsystem is MEMORY MANAGEMENT
        let (top_url, _top_branch) = candidates[0].0;
        let is_mm = top_url.contains("akpm/mm")
            || top_url.contains("linux-mm")
            || matched_subsystem_name
                .as_deref()
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

        for ((url, branch), count) in candidates {
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
            for kw in keywords {
                let url_matches = url_lower.contains(kw);
                let branch_matches = branch_lower.contains(kw);
                let subject_or_subsys_matches = subject_lower.contains(kw)
                    || matched_subsystem_name
                        .as_deref()
                        .map(|s| s.to_lowercase().contains(kw))
                        .unwrap_or(false);

                if subject_or_subsys_matches && (url_matches || branch_matches) {
                    score += 10;
                }
            }

            scored_candidates.push((score, self.resolve_url(url, branch.clone())));
        }

        // Sort descending by score
        scored_candidates.sort_by(|a, b| b.0.cmp(&a.0));

        let mut unique_filtered = Vec::new();
        for (_, res) in scored_candidates {
            if !unique_filtered.contains(&res) {
                unique_filtered.push(res);
            }
        }

        unique_filtered
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
}

pub fn extract_files_from_diff(diff: &str) -> Vec<String> {
    let mut files = Vec::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("diff --git a/")
            && let Some((a, _)) = path.split_once(' ')
        {
            files.push(a.to_string());
        }
    }
    files
}

pub fn extract_base_commit(body: &str) -> Option<String> {
    static BASE_COMMIT_RE: OnceLock<Regex> = OnceLock::new();
    let re =
        BASE_COMMIT_RE.get_or_init(|| Regex::new(r"(?m)^base-commit: ([0-9a-f]{40})").unwrap());
    re.captures(body)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
}

pub fn extract_version_tag(subject: &str) -> Option<String> {
    static VERSION_TAG_RE: OnceLock<Regex> = OnceLock::new();
    let re = VERSION_TAG_RE
        .get_or_init(|| Regex::new(r"(?i)\bv?(\d+\.\d+(?:\.\d+)?(?:-rc\d+)?(?:\.y)?)\b").unwrap());
    re.captures(subject)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            BaselineResolution::Commit("1234567890123456789012345678901234567890".to_string())
        );

        match &candidates[1] {
            BaselineResolution::RemoteTarget { name, .. } => assert_eq!(name, "net-next"),
            _ => panic!("Expected RemoteTarget net-next"),
        }
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
    async fn test_resolve_custom_remotes() {
        use crate::settings::CustomRemoteSettings;
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path();

        // Init git repo to avoid ensure_remote failing too hard
        std::process::Command::new("git")
            .current_dir(repo_path)
            .arg("init")
            .output()
            .unwrap();

        let mut remote_map = HashMap::new();
        let dummy_url = repo_path.join("dummy_remote").to_str().unwrap().to_string();
        remote_map.insert(
            dummy_url.clone(),
            "dummy-repo".to_string(),
        );

        let registry = BaselineRegistry {
            entries: Vec::new(),
            remote_map,
            custom_remotes: Some(vec![CustomRemoteSettings {
                name: "dummy-repo".to_string(),
                url: dummy_url,
                check_all_branches: false,
                only_branches: Some(vec!["master".to_string()]),
            }]),
            repo_path: repo_path.to_path_buf(),
        };

        let candidates = registry.resolve_candidates(&[], "Subject", None).await;

        let candidate_names: Vec<String> = candidates
            .iter()
            .filter_map(|c| match c {
                BaselineResolution::RemoteTarget { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();

        assert!(candidate_names.contains(&"dummy-repo".to_string()));
    }
}
