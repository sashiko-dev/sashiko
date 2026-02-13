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

use crate::git_ops::sync_git_command;
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
}

impl BaselineRegistry {
    pub fn new(repo_path: &Path) -> Result<Self> {
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
                    ref_name, e
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
        })
    }

    fn read_file_from_git(repo_path: &Path, rev: &str, file_path: &str) -> Result<String> {
        let output = sync_git_command()
            .current_dir(repo_path)
            .args(["show", &format!("{}:{}", rev, file_path)])
            .output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "git show failed: {}",
                String::from_utf8_lossy(&output.stderr)
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
        let output = sync_git_command()
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

    pub fn resolve_candidates(
        &self,
        files: &[String],
        subject: &str,
        body: Option<&str>,
    ) -> Vec<BaselineResolution> {
        let mut candidates = Vec::new();

        // 1. Explicit Base Commit
        if let Some(body_text) = body {
            if let Some(commit) = extract_base_commit(body_text) {
                candidates.push(BaselineResolution::Commit(commit));
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
        candidates.sort_by(|a, b| b.1.cmp(a.1));

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
        let keywords = [
            "net", "bpf", "drm", "mm", "sched", "x86", "arm", "arm64", "scsi", "usb",
        ];
        let is_next = subject_lower.contains("next");

        for (tree, _) in &candidates {
            let (url, branch) = tree;
            for kw in keywords {
                if subject_lower.contains(kw) && url.contains(kw) {
                    if is_next && url.contains("next") {
                        return vec![self.resolve_url(url, branch.clone())];
                    }
                    if !is_next && !url.contains("next") {
                        // Prefer non-next trees if the subject does not explicitly mention 'next'.
                    }
                }
            }
            if is_next && url.contains("next") {
                return vec![self.resolve_url(url, branch.clone())];
            }
        }

        let (url, branch) = candidates[0].0;
        vec![self.resolve_url(url, branch.clone())]
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
        if let Some(path) = line.strip_prefix("diff --git a/") {
            if let Some((a, _)) = path.split_once(' ') {
                files.push(a.to_string());
            }
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
        }
    }

    #[test]
    fn test_resolve_candidates() {
        let registry = create_registry();
        let files = vec!["net/core.c".to_string()];
        let body = "Some text\nbase-commit: 1234567890123456789012345678901234567890\n";

        let candidates = registry.resolve_candidates(&files, "Subject", Some(body));

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

    #[test]
    fn test_resolve_linux_mm() {
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
        };

        let files = vec!["mm/memory.c".to_string()];
        let candidates = registry.resolve_candidates(&files, "Subject", None);

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
}
