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

use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use tempfile::TempDir;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{info, warn};

#[allow(dead_code)]
pub struct GitWorktree {
    pub dir: TempDir,
    pub path: PathBuf,
    pub repo_path: PathBuf,
    pub is_managed: bool,
}

impl GitWorktree {
    #[allow(dead_code)]
    pub fn from_path(path: PathBuf, repo_path: PathBuf) -> Self {
        // Create a dummy tempdir to satisfy the struct (it won't be deleted or used).
        // Actually, we can't easily construct a TempDir that doesn't delete on drop unless we use into_path() but we need to keep it in struct.
        // Or we make dir: Option<TempDir>.
        // Let's change struct to Option<TempDir>.
        Self {
            dir: tempfile::Builder::new().prefix("dummy").tempdir().unwrap(), // Hack: create a dummy tempdir, but we won't use it.
            // If we drop this struct, the dummy tempdir is deleted, which is acceptable.
            // Do not delete the path.
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

        let output = Command::new("git")
            .current_dir(repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .arg("worktree")
            .arg("add")
            .arg("--detach")
            .arg(&path)
            .arg(commit_hash)
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to create worktree: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(Self {
            dir: temp_dir,
            path,
            repo_path: repo_path.to_path_buf(),
            is_managed: true,
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
            .arg("--3way")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
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
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        Ok(())
    }

    #[allow(dead_code)]
    pub async fn apply_raw_diff(&self, diff_content: &str) -> Result<std::process::Output> {
        info!("Applying raw diff in {:?}", self.path);

        let mut child = Command::new("git")
            .current_dir(&self.path)
            .args(["-c", "safe.bareRepository=all"])
            .arg("apply")
            .arg("--3way")
            .arg("-") // Read from stdin
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(diff_content.as_bytes()).await?;
        }

        let output = child.wait_with_output().await?;

        Ok(output)
    }

    pub async fn get_commit_show(&self, hash: &str) -> Result<String> {
        let output = Command::new("git")
            .current_dir(&self.path)
            .args(["show", hash])
            .output()
            .await?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Err(anyhow!(
                "git show failed: {}",
                String::from_utf8_lossy(&output.stderr)
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
            .args(["diff-tree", "--no-commit-id", "--name-only", "-r", "--root", hash])
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
                String::from_utf8_lossy(&output.stderr)
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
    pub async fn remove(self) -> Result<()> {
        if !self.is_managed {
            return Ok(());
        }
        info!("Removing worktree at {:?}", self.path);
        let output = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .arg("worktree")
            .arg("remove")
            .arg("-f")
            .arg(&self.path)
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to remove worktree: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
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
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(output.stdout)
}

#[allow(dead_code)]
pub async fn prune_worktrees(repo_path: &Path) -> Result<()> {
    info!("Pruning git worktrees in {:?}", repo_path);
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["-c", "safe.bareRepository=all"])
        .arg("worktree")
        .arg("prune")
        .output()
        .await?;

    if !output.status.success() {
        return Err(anyhow!(
            "git worktree prune failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
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
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

impl Drop for GitWorktree {
    fn drop(&mut self) {
        warn!(
            "Dropping worktree at {:?}. Use explicit .remove() for clean git state.",
            self.path
        );
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

pub async fn ensure_remote(
    repo_path: &Path,
    name: &str,
    url: &str,
    force_fetch: bool,
) -> Result<()> {
    // 1. Security Check (Skipped - trusting MAINTAINERS)
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

        if !check.status.success() {
            info!("Adding remote {} ({})", name, url);
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

    // 4. Fetch
    if should_fetch {
        info!("Fetching remote {}", name);
        let fetch = Command::new("git")
            .current_dir(repo_path)
            .args(["fetch", "--prune", name])
            .output()
            .await?;

        if !fetch.status.success() {
            warn!(
                "Failed to fetch remote {}: {}",
                name,
                String::from_utf8_lossy(&fetch.stderr)
            );
            // We continue even if fetch fails, attempting set-head might still work or fail later
        } else {
            // Update timestamp only on success
            if let Ok(file) = std::fs::File::create(&timestamp_file) {
                let _ = file.set_len(0);
            }
        }
    }

    // Ensure HEAD is set correctly (if we fetched OR if it was missing)
    if should_fetch || !head_exists {
        // Requires global config lock
        let global_lock = get_global_config_lock();
        let _global_guard = global_lock.lock().await;

        let set_head = Command::new("git")
            .current_dir(repo_path)
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
            String::from_utf8_lossy(&output.stderr)
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
            limit: None,
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
            String::from_utf8_lossy(&output.stderr)
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
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

pub async fn git_checkout(repo_path: &Path, target: &str) -> Result<()> {
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["checkout", target])
        .output()
        .await?;

    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "git checkout failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

pub async fn git_branch(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["branch", "--list", "--all"])
        .output()
        .await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(anyhow!(
            "git branch failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

pub async fn git_tag(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["tag", "--list"])
        .output()
        .await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(anyhow!(
            "git tag failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
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

        // Create a branch
        Command::new("git")
            .current_dir(&repo_path)
            .args(["branch", "feature"])
            .output()
            .await?;

        // Test git_branch
        let branches = git_branch(&repo_path).await?;
        assert!(branches.contains("feature"));
        assert!(branches.contains("master"));

        // Test git_checkout
        git_checkout(&repo_path, "feature").await?;
        let branches_after = git_branch(&repo_path).await?;
        assert!(branches_after.contains("* feature"));

        // Create a tag
        Command::new("git")
            .current_dir(&repo_path)
            .args(["tag", "v1.0"])
            .output()
            .await?;

        // Test git_tag
        let tags = git_tag(&repo_path).await?;
        assert!(tags.contains("v1.0"));

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
        Command::new("git").current_dir(&repo_path).args(["init"]).output().await?;
        let _ = Command::new("git").current_dir(&repo_path).args(["branch", "-m", "master"]).output().await;
        Command::new("git").current_dir(&repo_path).args(["config", "user.email", "test@example.com"]).output().await?;
        Command::new("git").current_dir(&repo_path).args(["config", "user.name", "Test User"]).output().await?;

        // 1. Initial commit
        let file_path = repo_path.join("test.txt");
        {
             let mut file = File::create(&file_path)?;
             writeln!(file, "Initial content")?;
        }
        Command::new("git").current_dir(&repo_path).args(["add", "."]).output().await?;
        Command::new("git").current_dir(&repo_path).args(["commit", "-m", "Initial commit"]).output().await?;
        
        let initial_hash = get_commit_hash(&repo_path, "HEAD").await?;

        // 2. Create a branch and add a commit
        Command::new("git").current_dir(&repo_path).args(["checkout", "-b", "feature"]).output().await?;
        {
             let mut file = std::fs::OpenOptions::new().append(true).open(&file_path)?;
             writeln!(file, "Feature content")?;
        }
        Command::new("git").current_dir(&repo_path).args(["commit", "-am", "Feature commit"]).output().await?;
        let feature_hash = get_commit_hash(&repo_path, "HEAD").await?;

        // 3. Back to master and add a commit
        Command::new("git").current_dir(&repo_path).args(["checkout", "master"]).output().await?;
        let other_file = repo_path.join("other.txt");
        {
             let mut file = File::create(&other_file)?;
             writeln!(file, "Other content")?;
        }
        Command::new("git").current_dir(&repo_path).args(["add", "."]).output().await?;
        Command::new("git").current_dir(&repo_path).args(["commit", "-m", "Master commit"]).output().await?;
        let master_hash = get_commit_hash(&repo_path, "HEAD").await?;

        // 4. Merge feature into master
        Command::new("git").current_dir(&repo_path).args(["merge", "feature", "--no-ff", "-m", "Merge commit"]).output().await?;
        let merge_hash = get_commit_hash(&repo_path, "HEAD").await?;

        // 5. Create an empty commit
        Command::new("git").current_dir(&repo_path).args(["commit", "--allow-empty", "-m", "Empty commit"]).output().await?;
        let empty_hash = get_commit_hash(&repo_path, "HEAD").await?;

        // Use GitWorktree to inspect
        let worktree = GitWorktree::new(&repo_path, &merge_hash, None).await?;

        // Check Merge Commit
        assert!(worktree.is_merge_commit(&merge_hash).await?, "Should be a merge commit");
        assert!(!worktree.is_merge_commit(&initial_hash).await?, "Initial should not be merge");
        assert!(!worktree.is_merge_commit(&feature_hash).await?, "Feature should not be merge");
        assert!(!worktree.is_merge_commit(&master_hash).await?, "Master should not be merge");
        
        // Check Empty Commit
        assert!(worktree.is_empty_commit(&empty_hash).await?, "Should be empty commit");
        assert!(!worktree.is_empty_commit(&initial_hash).await?, "Initial should not be empty");
        
        Ok(())
    }
}
