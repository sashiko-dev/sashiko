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

use crate::utils::redact_secret;
use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use tempfile::TempDir;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{error, info, warn};

pub const GIT_PROTOCOL_RESTRICTIONS: &[&str] = &[
    "-c",
    "protocol.allow=never",
    "-c",
    "protocol.http.allow=always",
    "-c",
    "protocol.https.allow=always",
    "-c",
    "protocol.git.allow=always",
    "-c",
    "protocol.ssh.allow=always",
    "-c",
    "protocol.file.allow=always",
];

#[allow(dead_code)]
pub struct GitWorktree {
    pub dir: Option<TempDir>,
    pub path: PathBuf,
    pub repo_path: PathBuf,
    pub is_managed: bool,
}

impl GitWorktree {
    #[allow(dead_code)]
    pub fn from_path(path: PathBuf, repo_path: PathBuf) -> Self {
        Self {
            dir: None,
            path,
            repo_path,
            is_managed: false,
        }
    }

    #[allow(dead_code)]
    pub async fn new(
        repo_path: &Path,
        commit_hash: &str,
        parent_dir: Option<&Path>,
    ) -> Result<Self> {
        let temp_dir = if let Some(parent) = parent_dir {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
            tempfile::Builder::new()
                .prefix("sashiko-worktree-")
                .tempdir_in(parent)?
        } else {
            TempDir::new()?
        };
        let path = temp_dir.path().to_path_buf();

        info!("Creating worktree at {:?}", path);

        // Split worktree creation into two phases:
        // 1) metadata update (under lock, fast)
        // 2) file checkout (no lock, parallelizable)
        let output = {
            let lock = get_worktree_lock();
            let _guard = lock.lock().await;
            Command::new("git")
                .current_dir(repo_path)
                .args(["-c", "safe.bareRepository=all"])
                .arg("worktree")
                .arg("add")
                .arg("--detach")
                .arg("--no-checkout")
                .arg(&path)
                .arg(commit_hash)
                .output()
                .await?
        };

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to create worktree: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        let output = Command::new("git")
            .current_dir(&path)
            .args(["-c", "safe.bareRepository=all"])
            .args(["reset", "--hard", commit_hash])
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to populate worktree: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        Ok(Self {
            dir: Some(temp_dir),
            path,
            repo_path: repo_path.to_path_buf(),
            is_managed: true,
        })
    }

    pub async fn new_scratch_clone(
        repo_path: &Path,
        commit_hash: &str,
        parent_dir: Option<&Path>,
    ) -> Result<Self> {
        let temp_dir = if let Some(parent) = parent_dir {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
            tempfile::Builder::new()
                .prefix("sashiko-review-")
                .tempdir_in(parent)?
        } else {
            tempfile::Builder::new()
                .prefix("sashiko-review-")
                .tempdir()?
        };
        let path = temp_dir.path().to_path_buf();

        info!("Creating scratch clone at {:?}", path);

        let output = Command::new("git")
            .args(["-c", "safe.bareRepository=all"])
            .arg("clone")
            .arg("--shared")
            .arg("--no-checkout")
            .arg(repo_path)
            .arg(&path)
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to create scratch clone: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        let output = Command::new("git")
            .current_dir(&path)
            .args(["-c", "safe.bareRepository=all"])
            .args(["reset", "--hard", commit_hash])
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to populate scratch clone: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        Ok(Self {
            dir: Some(temp_dir),
            path,
            repo_path: repo_path.to_path_buf(),
            is_managed: false,
        })
    }

    #[allow(dead_code)]
    pub async fn apply_patch(&self, patch_content: &str) -> Result<()> {
        info!("Applying patch in {:?}", self.path);

        let mut child = Command::new("git")
            .current_dir(&self.path)
            .env("GIT_AUTHOR_NAME", "Sashiko Bot")
            .env("GIT_AUTHOR_EMAIL", "sashiko@localhost")
            .env("GIT_COMMITTER_NAME", "Sashiko Bot")
            .env("GIT_COMMITTER_EMAIL", "sashiko@localhost")
            .args(["-c", "safe.bareRepository=all"])
            .arg("am")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(patch_content.as_bytes()).await?;
        }

        let output = child.wait_with_output().await?;

        if !output.status.success() {
            let _ = Command::new("git")
                .current_dir(&self.path)
                .args(["-c", "safe.bareRepository=all"])
                .arg("am")
                .arg("--abort")
                .output()
                .await;

            return Err(anyhow!(
                "git am failed. stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        Ok(())
    }

    pub async fn get_commit_show(&self, hash: &str) -> Result<String> {
        let output = Command::new("git")
            .current_dir(&self.path)
            .args(["show", "--patch", hash])
            .output()
            .await?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Err(anyhow!(
                "git show failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    pub async fn get_commit_message(&self, hash: &str) -> Result<String> {
        let output = Command::new("git")
            .current_dir(&self.path)
            .args(["show", "--no-patch", hash])
            .output()
            .await?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Err(anyhow!(
                "git show --no-patch failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    pub async fn is_merge_commit(&self, hash: &str) -> Result<bool> {
        let output = Command::new("git")
            .current_dir(&self.path)
            .args(["rev-list", "--parents", "-n", "1", hash])
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!("git rev-list failed"));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Output: commit parent1 parent2 ...
        let parts: Vec<&str> = stdout.split_whitespace().collect();
        Ok(parts.len() > 2)
    }

    pub async fn is_empty_commit(&self, hash: &str) -> Result<bool> {
        let output = Command::new("git")
            .current_dir(&self.path)
            .args([
                "diff-tree",
                "--no-commit-id",
                "--name-only",
                "-r",
                "--root",
                hash,
            ])
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!("git diff-tree failed"));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.trim().is_empty())
    }

    pub async fn reset_hard(&self, ref_name: &str) -> Result<()> {
        info!("Resetting worktree to {}", ref_name);
        let output = Command::new("git")
            .current_dir(&self.path)
            .args(["reset", "--hard", ref_name])
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!(
                "git reset --hard failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        // Also clean untracked files to be safe
        let clean_output = Command::new("git")
            .current_dir(&self.path)
            .args(["clean", "-fdx"])
            .output()
            .await?;

        if !clean_output.status.success() {
            return Err(anyhow!(
                "git clean failed: {}",
                String::from_utf8_lossy(&clean_output.stderr)
            ));
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub async fn remove(mut self) -> Result<()> {
        if !self.is_managed {
            return Ok(());
        }
        info!("Removing worktree at {:?}", self.path);

        let output = {
            let lock = get_worktree_lock();
            let _guard = lock.lock().await;
            Command::new("git")
                .current_dir(&self.repo_path)
                .args(["-c", "safe.bareRepository=all"])
                .arg("worktree")
                .arg("remove")
                .arg("-f")
                .arg(&self.path)
                .output()
                .await?
        };

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to remove worktree: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        self.is_managed = false;
        Ok(())
    }
}

#[allow(dead_code)]
pub async fn read_blob(repo_path: &Path, hash: &str) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["-c", "safe.bareRepository=all"])
        .arg("cat-file")
        .arg("-p")
        .arg(hash)
        .output()
        .await?;

    if !output.status.success() {
        return Err(anyhow!(
            "git cat-file failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

#[allow(dead_code)]
pub async fn prune_worktrees(repo_path: &Path) -> Result<()> {
    info!("Pruning git worktrees in {:?}", repo_path);
    let output = {
        let lock = get_worktree_lock();
        let _guard = lock.lock().await;
        Command::new("git")
            .current_dir(repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .arg("worktree")
            .arg("prune")
            .output()
            .await?
    };

    if !output.status.success() {
        return Err(anyhow!(
            "git worktree prune failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

#[allow(dead_code)]
pub async fn ensure_submodule_config_compat(repo_path: &Path) -> Result<()> {
    info!("Ensuring submodule config compatibility in {:?}", repo_path);
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["-c", "safe.bareRepository=all"])
        .args(["config", "--unset", "core.worktree"])
        .output()
        .await?;

    if !output.status.success() {
        let status_code = output.status.code().unwrap_or(-1);
        if status_code != 5 {
            return Err(anyhow!(
                "git config --unset core.worktree failed with exit code {}: {}",
                status_code,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    } else {
        info!("Successfully unset core.worktree in {:?}", repo_path);
    }
    Ok(())
}

#[allow(dead_code)]
pub async fn cleanup_worktree_dir(worktree_dir: &Path) -> Result<()> {
    if !worktree_dir.exists() {
        return Ok(());
    }
    info!(
        "Cleaning up stale worktree directories in {:?}",
        worktree_dir
    );
    let mut entries = tokio::fs::read_dir(worktree_dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            let is_sashiko = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|name| name.starts_with("sashiko-worktree-"))
                .unwrap_or(false);
            if is_sashiko {
                info!("Deleting stale worktree directory: {:?}", path);
                if let Err(e) = tokio::fs::remove_dir_all(&path).await {
                    error!("Failed to delete stale worktree dir {:?}: {}", path, e);
                }
            }
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub async fn check_disk_usage(path: &Path) -> Result<String> {
    let output = Command::new("du").arg("-sh").arg(path).output().await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(anyhow!(
            "Failed to check disk usage: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

impl Drop for GitWorktree {
    fn drop(&mut self) {
        if self.is_managed {
            warn!(
                "Dropping worktree at {:?}. Use explicit .remove() for clean git state.",
                self.path
            );
        }
    }
}

fn get_remote_lock(name: &str) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let map_mutex = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = map_mutex.lock().unwrap();
    map.entry(name.to_string())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

fn get_global_config_lock() -> Arc<AsyncMutex<()>> {
    static GLOBAL_LOCK: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();
    GLOBAL_LOCK
        .get_or_init(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

fn get_worktree_lock() -> Arc<AsyncMutex<()>> {
    static WORKTREE_LOCK: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();
    WORKTREE_LOCK
        .get_or_init(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

pub async fn ensure_remote(
    repo_path: &Path,
    name: &str,
    url: &str,
    force_fetch: bool,
) -> Result<()> {
    // 1. Validate repo_path to prevent git from traversing up to parent repos
    if !repo_path.join(".git").exists() && !repo_path.join("HEAD").exists() {
        return Err(anyhow::anyhow!(
            "{} is not a valid git repository. Did you forget to initialize submodules?",
            repo_path.display()
        ));
    }

    // acquire remote-specific lock
    let lock = get_remote_lock(name);
    let _guard = lock.lock().await;

    let mut just_added = false;

    // 2. Check if exists (requires global config lock)
    {
        let global_lock = get_global_config_lock();
        let _global_guard = global_lock.lock().await;

        let check = Command::new("git")
            .current_dir(repo_path)
            .args(["remote", "get-url", name])
            .output()
            .await?;

        if check.status.success() {
            // Check if URL matches, if not update it
            let current_url = String::from_utf8_lossy(&check.stdout).trim().to_string();
            if current_url != url {
                info!(
                    "Updating remote URL for {} from {} to {}",
                    name,
                    redact_secret(&current_url),
                    redact_secret(url)
                );
                let update = Command::new("git")
                    .current_dir(repo_path)
                    .args(["remote", "set-url", name, url])
                    .output()
                    .await?;
                if !update.status.success() {
                    return Err(anyhow!(
                        "Failed to update remote URL: {}",
                        String::from_utf8_lossy(&update.stderr)
                    ));
                }
            }
        } else {
            info!("Adding remote {} ({})", name, redact_secret(url));
            let add = Command::new("git")
                .current_dir(repo_path)
                .args(["remote", "add", name, url])
                .output()
                .await?;
            if !add.status.success() {
                let stderr = String::from_utf8_lossy(&add.stderr);
                if !stderr.contains("already exists") {
                    return Err(anyhow!("Failed to add remote: {}", stderr));
                }
            }
            just_added = true;
        }
    } // Release global lock

    // 3. Lazy Fetch Check
    let timestamp_dir = repo_path.join(".sashiko/fetch_timestamps");
    if !timestamp_dir.exists() {
        std::fs::create_dir_all(&timestamp_dir)?;
    }
    let timestamp_file = timestamp_dir.join(name);

    let age = std::fs::metadata(&timestamp_file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| std::time::SystemTime::now().duration_since(m).ok());

    // Check if HEAD exists
    let head_ref = format!("refs/remotes/{}/HEAD", name);
    let head_exists = Command::new("git")
        .current_dir(repo_path)
        .args(["show-ref", "--verify", "-q", &head_ref])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);

    let should_fetch = if just_added || !head_exists || force_fetch {
        true
    } else {
        let fetch_interval = if url.contains("akpm/mm") || url.contains("linux-next") {
            std::time::Duration::from_secs(300)
        } else {
            std::time::Duration::from_secs(3600)
        };
        match age {
            Some(a) => a > fetch_interval,
            None => true,
        }
    };

    if !should_fetch {
        let reason = if force_fetch {
            "forced but recently fetched"
        } else {
            "fresh"
        };
        info!("Skipping fetch for {} ({})", name, reason);
        return Ok(());
    }

    let mut fetch_ok = false;
    let mut error_msg = String::new();

    // 4. Fetch
    if should_fetch {
        info!("Fetching remote {}", name);

        let fetch_future = Command::new("git")
            .current_dir(repo_path)
            .args(GIT_PROTOCOL_RESTRICTIONS)
            .args(["fetch", "--prune", "--no-tags", name])
            .kill_on_drop(true)
            .output();

        // Dynamically scale timeout: 30 minutes for heavy initial fetches, 5 minutes for routine updates
        let timeout_duration = if just_added || !head_exists {
            std::time::Duration::from_secs(1800)
        } else {
            std::time::Duration::from_secs(300)
        };

        match tokio::time::timeout(timeout_duration, fetch_future).await {
            Ok(Ok(mut fetch)) => {
                if !fetch.status.success() {
                    let stderr = String::from_utf8_lossy(&fetch.stderr);

                    // Auto-recover from bad tags
                    if stderr.contains("fatal: bad object refs/tags/")
                        && let Some(start) = stderr.find("refs/tags/")
                    {
                        let tag_path = &stderr[start..];
                        let tag_path = tag_path.split_whitespace().next().unwrap_or("");
                        if !tag_path.is_empty() {
                            warn!(
                                "Detected bad tag '{}'. Attempting to delete and retry fetch.",
                                tag_path
                            );
                            let _ = Command::new("git")
                                .current_dir(repo_path)
                                .args(["update-ref", "-d", tag_path])
                                .output()
                                .await;

                            // Retry the fetch once
                            let retry_future = Command::new("git")
                                .current_dir(repo_path)
                                .args(GIT_PROTOCOL_RESTRICTIONS)
                                .args(["fetch", "--prune", "--no-tags", name])
                                .kill_on_drop(true)
                                .output();

                            if let Ok(Ok(retry_fetch)) =
                                tokio::time::timeout(timeout_duration, retry_future).await
                            {
                                fetch = retry_fetch;
                            }
                        }
                    }
                }

                if fetch.status.success() {
                    fetch_ok = true;
                } else {
                    error_msg = format!(
                        "Failed to fetch remote {}: {}",
                        name,
                        String::from_utf8_lossy(&fetch.stderr).trim()
                    );
                }
            }
            Ok(Err(e)) => {
                error_msg = format!("Failed to execute git fetch for {}: {}", name, e);
            }
            Err(_) => {
                error_msg = format!(
                    "Git fetch for {} timed out after {} seconds",
                    name,
                    timeout_duration.as_secs()
                );
            }
        }

        if !fetch_ok {
            let max_stale_age = std::time::Duration::from_secs(86400); // 24 hours
            let can_fallback = head_exists
                && match age {
                    Some(a) => a < max_stale_age,
                    None => false,
                };

            if can_fallback {
                warn!(
                    "Fetch failed for {} ({}), but local ref exists and is fresh enough (age: {:?}). Falling back to local ref.",
                    name, error_msg, age
                );
                // We do NOT update the timestamp file, so we will try to fetch again next time.
            } else {
                error!("{}", error_msg);
                return Err(anyhow::anyhow!(error_msg));
            }
        } else {
            // Update timestamp only on success
            if let Ok(file) = std::fs::File::create(&timestamp_file) {
                let _ = file.set_len(0);
            }
        }
    }

    // Ensure HEAD is set correctly (if we fetched successfully)
    if should_fetch && fetch_ok {
        // Requires global config lock
        let global_lock = get_global_config_lock();
        let _global_guard = global_lock.lock().await;

        let set_head = Command::new("git")
            .current_dir(repo_path)
            .args(GIT_PROTOCOL_RESTRICTIONS)
            .args(["remote", "set-head", name, "--auto"])
            .output()
            .await?;

        if !set_head.status.success() {
            warn!(
                "Failed to set-head for remote {}: {}",
                name,
                String::from_utf8_lossy(&set_head.stderr)
            );
        }
    }

    Ok(())
}

pub async fn get_remote_branches(repo_path: &Path, remote_name: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["branch", "-r", "--list", &format!("{}/*", remote_name)])
        .output()
        .await?;

    if !output.status.success() {
        return Err(anyhow!(
            "Failed to list remote branches: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let branches = stdout
        .lines()
        .map(|line| line.trim())
        .filter_map(|line| line.strip_prefix(&format!("{}/", remote_name)))
        .filter(|s| !s.contains("->")) // Filter out symbolic references like HEAD -> origin/main
        .map(|s| s.to_string())
        .collect();
    Ok(branches)
}

pub async fn get_commit_hash(path: &Path, ref_name: &str) -> Result<String> {
    let output = Command::new("git")
        .current_dir(path)
        .args(["rev-parse", ref_name])
        .output()
        .await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(anyhow!(
            "Failed to resolve {}: {}",
            ref_name,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[derive(Debug, Clone)]
pub struct GitLogParams {
    pub repo_path: PathBuf,
    pub limit: Option<usize>,
    pub rev_range: Option<String>,
    pub paths: Vec<String>,

    // Output toggle flags
    pub show_hash: bool,
    pub show_author: bool,
    pub show_date: bool,
    pub show_subject: bool,
    pub show_body: bool,
    pub show_stat: bool,
}

impl Default for GitLogParams {
    fn default() -> Self {
        Self {
            repo_path: PathBuf::new(),
            limit: Some(100),
            rev_range: None,
            paths: Vec::new(),
            show_hash: true,
            show_author: false,
            show_date: false,
            show_subject: true,
            show_body: false,
            show_stat: false,
        }
    }
}

pub async fn get_git_log(params: GitLogParams) -> Result<String> {
    let mut args = vec!["log".to_string()];

    // Format string construction
    let mut format_parts = Vec::new();
    if params.show_hash {
        format_parts.push("Hash: %h");
    }
    if params.show_author {
        format_parts.push("Author: %an");
    }
    if params.show_date {
        format_parts.push("Date: %ad");
        args.push("--date=short".to_string());
    }
    if params.show_subject {
        format_parts.push("Subject: %s");
    }
    if params.show_body {
        format_parts.push("Body:%n%b");
    }

    let format_string = if format_parts.is_empty() {
        "%h %s".to_string()
    } else {
        format_parts.join("%n") + "%n---"
    };

    args.push(format!("--pretty=format:{}", format_string));

    if let Some(limit) = params.limit {
        args.push(format!("-n{}", limit));
    }

    if params.show_stat {
        args.push("--stat".to_string());
    }

    if let Some(range) = &params.rev_range {
        args.push(range.clone());
    }

    if !params.paths.is_empty() {
        args.push("--".to_string());
        args.extend(params.paths.clone());
    }

    let output = Command::new("git")
        .current_dir(&params.repo_path)
        .args(&args)
        .output()
        .await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(anyhow!(
            "git log failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

pub async fn git_status(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["status"])
        .output()
        .await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(anyhow!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Metadata extracted from a single git commit.
pub struct PatchMetadata {
    pub author: String,
    pub subject: String,
    pub message: String,
    pub diff: String,
    pub base_commit: Option<String>,
    pub timestamp: i64,
}

/// Resolve a git range (e.g. "HEAD~3..HEAD") to an ordered list of commit SHAs.
pub async fn resolve_git_range(repo_path: &Path, range: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["-c", "safe.bareRepository=all"])
        .args(["rev-list", "--reverse", range])
        .output()
        .await?;

    if !output.status.success() {
        return Err(anyhow!(
            "Failed to resolve git range '{}': {}",
            range,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let shas: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect();

    if shas.is_empty() {
        return Err(anyhow!("Git range '{}' is empty", range));
    }

    Ok(shas)
}

/// Extract patch metadata from a commit using `git show`.
pub async fn extract_patch_metadata(repo_path: &Path, commit: &str) -> Result<PatchMetadata> {
    // Resolve parent to use as base_commit
    let parent_output = Command::new("git")
        .current_dir(repo_path)
        .args(["rev-parse", &format!("{}^", commit)])
        .output()
        .await?;

    let base_commit = if parent_output.status.success() {
        Some(
            String::from_utf8_lossy(&parent_output.stdout)
                .trim()
                .to_string(),
        )
    } else {
        warn!(
            "Failed to resolve parent for {}, using commit as base",
            commit
        );
        Some(commit.to_string())
    };

    let format = "format:%an%n%ae%n%s%n%b%n---SASHIKO-END-HEADER---%n";

    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["show", &format!("--format={}", format), commit])
        .output()
        .await?;

    if !output.status.success() {
        return Err(anyhow!(
            "git show failed for {}: {}",
            commit,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let raw = String::from_utf8_lossy(&output.stdout).to_string();
    let parts: Vec<&str> = raw.split("---SASHIKO-END-HEADER---\n").collect();

    if parts.len() < 2 {
        return Err(anyhow!("Failed to parse git show output for {}", commit));
    }

    let header_part = parts[0];
    let diff = parts[1..].join("---SASHIKO-END-HEADER---\n");

    let mut lines = header_part.lines();
    let author_name = lines.next().unwrap_or_default().trim();
    let author_email = lines.next().unwrap_or("unknown@localhost").trim();
    let subject = lines.next().unwrap_or("No Subject").trim();

    let body: Vec<&str> = lines.collect();
    let message = body.join("\n").trim().to_string();

    let author = if author_name.is_empty() || author_name.to_lowercase() == "unknown" {
        author_email.to_string()
    } else {
        format!("{} <{}>", author_name, author_email)
    };

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;

    Ok(PatchMetadata {
        author,
        subject: subject.to_string(),
        message,
        diff,
        base_commit,
        timestamp,
    })
}

pub async fn is_dirty(repo_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["-c", "safe.bareRepository=all"])
        .args(["status", "--porcelain"])
        .output()
        .await?;

    if !output.status.success() {
        return Err(anyhow!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(!stdout.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    #[tokio::test]
    async fn test_git_ops_extensions() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        // Init git repo
        Command::new("git")
            .current_dir(&repo_path)
            .args(["init"])
            .output()
            .await?;

        // Ensure we are on master
        let _ = Command::new("git")
            .current_dir(&repo_path)
            .args(["branch", "-m", "master"])
            .output()
            .await;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;

        // Commit 1
        let file_path = repo_path.join("test.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "Hello World")?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()
            .await?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Initial commit"])
            .output()
            .await?;

        // Test git_status
        let status = git_status(&repo_path).await?;
        assert!(status.contains("nothing to commit, working tree clean"));

        Ok(())
    }

    #[tokio::test]
    async fn test_git_log() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        // Init git repo
        Command::new("git")
            .current_dir(&repo_path)
            .args(["init"])
            .output()
            .await?;

        // Configure user
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;

        // Commit 1
        let file_path = repo_path.join("test.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "Hello World")?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()
            .await?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Initial commit"])
            .output()
            .await?;

        // Commit 2
        let mut file = std::fs::OpenOptions::new().append(true).open(&file_path)?;
        writeln!(file, "Change 1")?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-am", "Second commit"])
            .output()
            .await?;

        // Test get_git_log
        let params = GitLogParams {
            repo_path: repo_path.clone(),
            limit: Some(1),
            show_subject: true,
            show_hash: true,
            ..Default::default()
        };

        let log = get_git_log(params).await?;
        assert!(log.contains("Second commit"));
        assert!(!log.contains("Initial commit")); // Limited to 1

        // Test with author
        let params = GitLogParams {
            repo_path: repo_path.clone(),
            show_author: true,
            ..Default::default()
        };
        let log = get_git_log(params).await?;
        assert!(log.contains("Author: Test User"));

        Ok(())
    }

    #[tokio::test]
    async fn test_apply_patch_failure() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        // Init git repo
        Command::new("git")
            .current_dir(&repo_path)
            .args(["init"])
            .output()
            .await?;

        // Ensure we are on master
        let _ = Command::new("git")
            .current_dir(&repo_path)
            .args(["branch", "-m", "master"])
            .output()
            .await;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;

        // Create a dummy file
        let file_path = repo_path.join("test.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "Hello World")?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Initial commit"])
            .output()
            .await?;
        let head_hash = get_commit_hash(&repo_path, "HEAD").await?;

        // Create a worktree
        let worktree = GitWorktree::new(&repo_path, &head_hash, None).await?;

        // Try to apply a bad patch
        let bad_patch = "Invalid patch content";
        let result = worktree.apply_patch(bad_patch).await;

        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();

        // Check if stdout and stderr are mentioned in the error message
        assert!(err_msg.contains("stdout:"));
        assert!(err_msg.contains("stderr:"));
        assert!(err_msg.contains("git am failed"));

        Ok(())
    }

    #[tokio::test]
    async fn test_merge_and_empty_commit_detection() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        // Init git repo
        Command::new("git")
            .current_dir(&repo_path)
            .args(["init"])
            .output()
            .await?;
        let _ = Command::new("git")
            .current_dir(&repo_path)
            .args(["branch", "-m", "master"])
            .output()
            .await;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;

        // 1. Initial commit
        let file_path = repo_path.join("test.txt");
        {
            let mut file = File::create(&file_path)?;
            writeln!(file, "Initial content")?;
        }
        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Initial commit"])
            .output()
            .await?;

        let initial_hash = get_commit_hash(&repo_path, "HEAD").await?;

        // 2. Create a branch and add a commit
        Command::new("git")
            .current_dir(&repo_path)
            .args(["checkout", "-b", "feature"])
            .output()
            .await?;
        {
            let mut file = std::fs::OpenOptions::new().append(true).open(&file_path)?;
            writeln!(file, "Feature content")?;
        }
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-am", "Feature commit"])
            .output()
            .await?;
        let feature_hash = get_commit_hash(&repo_path, "HEAD").await?;

        // 3. Back to master and add a commit
        Command::new("git")
            .current_dir(&repo_path)
            .args(["checkout", "master"])
            .output()
            .await?;
        let other_file = repo_path.join("other.txt");
        {
            let mut file = File::create(&other_file)?;
            writeln!(file, "Other content")?;
        }
        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Master commit"])
            .output()
            .await?;
        let master_hash = get_commit_hash(&repo_path, "HEAD").await?;

        // 4. Merge feature into master
        Command::new("git")
            .current_dir(&repo_path)
            .args(["merge", "feature", "--no-ff", "-m", "Merge commit"])
            .output()
            .await?;
        let merge_hash = get_commit_hash(&repo_path, "HEAD").await?;

        // 5. Create an empty commit
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "--allow-empty", "-m", "Empty commit"])
            .output()
            .await?;
        let empty_hash = get_commit_hash(&repo_path, "HEAD").await?;

        // Use GitWorktree to inspect
        let worktree = GitWorktree::new(&repo_path, &merge_hash, None).await?;

        // Check Merge Commit
        assert!(
            worktree.is_merge_commit(&merge_hash).await?,
            "Should be a merge commit"
        );
        assert!(
            !worktree.is_merge_commit(&initial_hash).await?,
            "Initial should not be merge"
        );
        assert!(
            !worktree.is_merge_commit(&feature_hash).await?,
            "Feature should not be merge"
        );
        assert!(
            !worktree.is_merge_commit(&master_hash).await?,
            "Master should not be merge"
        );

        // Check Empty Commit
        assert!(
            worktree.is_empty_commit(&empty_hash).await?,
            "Should be empty commit"
        );
        assert!(
            !worktree.is_empty_commit(&initial_hash).await?,
            "Initial should not be empty"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_remote_bad_tag_recovery() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let local_repo_path = temp_dir.path().join("local");
        let remote_repo_path = temp_dir.path().join("remote");

        std::fs::create_dir(&local_repo_path)?;
        std::fs::create_dir(&remote_repo_path)?;

        // Init remote repo
        Command::new("git")
            .current_dir(&remote_repo_path)
            .args(["init"])
            .output()
            .await?;

        Command::new("git")
            .current_dir(&remote_repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&remote_repo_path)
            .args(["config", "user.name", "Test"])
            .output()
            .await?;
        let mut file = File::create(remote_repo_path.join("file.txt"))?;
        writeln!(file, "test")?;
        Command::new("git")
            .current_dir(&remote_repo_path)
            .args(["add", "file.txt"])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&remote_repo_path)
            .args(["commit", "-m", "init"])
            .output()
            .await?;

        // Init local repo
        Command::new("git")
            .current_dir(&local_repo_path)
            .args(["init"])
            .output()
            .await?;

        // Use ensure_remote to add remote and fetch
        ensure_remote(
            &local_repo_path,
            "origin",
            remote_repo_path.to_str().unwrap(),
            true,
        )
        .await?;

        // Create bad tag in local repo
        let tags_dir = local_repo_path.join(".git").join("refs").join("tags");
        std::fs::create_dir_all(&tags_dir)?;
        let bad_tag_path = tags_dir.join("bad-tag");
        let mut bad_tag_file = File::create(&bad_tag_path)?;
        writeln!(bad_tag_file, "0000000000000000000000000000000000000000")?;

        // Fetch again, should auto-recover and delete the bad tag
        ensure_remote(
            &local_repo_path,
            "origin",
            remote_repo_path.to_str().unwrap(),
            true,
        )
        .await?;

        assert!(
            !bad_tag_path.exists(),
            "Bad tag should have been deleted by recovery logic"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_remote_protocol_security() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo_path = dir.path().to_path_buf();

        // Init local repo
        Command::new("git")
            .current_dir(&repo_path)
            .args(["init"])
            .output()
            .await?;

        let proof_file = dir.path().join("vandalized_protocol.txt");
        let script_path = dir.path().join("trigger.sh");
        std::fs::write(
            &script_path,
            format!("#!/bin/sh\ntouch \"{}\"", proof_file.display()),
        )?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script_path)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script_path, perms)?;
        }

        let url = format!("ext::{}", script_path.to_str().unwrap());

        // ensure_remote might return Ok(()) even if fetch fails because it ignores fetch failures.
        // However, the key security guarantee is that the command in the ext:: URL is NOT executed.
        let _ = ensure_remote(&repo_path, "malicious", &url, true).await;

        assert!(
            !proof_file.exists(),
            "Command should NOT have been executed!"
        );

        Ok(())
    }
}
