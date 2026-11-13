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

use crate::git_cmd::GitCommandExt;
use crate::utils::redact_secret;
use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use tempfile::TempDir;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, error, info, warn};

mod apply_error;
use apply_error::GitAmError;
pub use apply_error::PatchApplyFailure;
mod remote_writes;
#[cfg(any(feature = "server", test))]
pub(crate) use remote_writes::PendingWrites;
use remote_writes::finish_write;

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
        if commit_hash.is_empty() || commit_hash.starts_with('-') {
            return Err(anyhow!("Invalid commit revision: {}", commit_hash));
        }
        let mut worktree = Self::allocate(repo_path, parent_dir).await?;
        if let Err(error) = worktree.populate(commit_hash).await {
            if let Err(cleanup_error) = worktree.remove().await {
                warn!("Failed to clean up unpopulated worktree: {}", cleanup_error);
            }
            return Err(error);
        }
        Ok(worktree)
    }

    /// Allocate before starting cancellable Git operations so the caller can
    /// retain ownership and explicitly clean up an interrupted checkout.
    pub(crate) async fn allocate(repo_path: &Path, parent_dir: Option<&Path>) -> Result<Self> {
        let parent_owned = parent_dir.map(|p| p.to_path_buf());
        let temp_dir = tokio::task::spawn_blocking(move || -> Result<TempDir> {
            if let Some(parent) = parent_owned {
                if !parent.exists() {
                    std::fs::create_dir_all(&parent)?;
                }
                Ok(tempfile::Builder::new()
                    .prefix("sashiko-worktree-")
                    .tempdir_in(parent)?)
            } else {
                Ok(TempDir::new()?)
            }
        })
        .await??;
        let path = temp_dir.path().to_path_buf();
        Ok(Self {
            dir: Some(temp_dir),
            path,
            repo_path: repo_path.to_path_buf(),
            is_managed: false,
        })
    }

    /// Register and populate a newly allocated worktree. On error or future
    /// cancellation, the owner must call remove before reusing its resources.
    pub(crate) async fn populate(&mut self, commit_hash: &str) -> Result<()> {
        if commit_hash.is_empty() || commit_hash.starts_with('-') {
            return Err(anyhow!("Invalid commit revision: {}", commit_hash));
        }
        debug!("Creating worktree at {:?}", self.path);

        // Split worktree creation into two phases:
        // 1) metadata update (under lock, fast)
        // 2) file checkout (no lock, parallelizable)
        let output = {
            let lock = get_worktree_lock();
            let _guard = lock.lock().await;
            self.is_managed = true;
            crate::git_cmd::in_dir_async(&self.repo_path)
                .args(["-c", "safe.bareRepository=all"])
                .arg("worktree")
                .arg("add")
                .arg("--detach")
                .arg("--no-checkout")
                .arg("--")
                .arg(&self.path)
                .arg(commit_hash)
                .output_cancellable()
                .await?
        };

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to create worktree: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        // `commit_hash` is verified not to start with `-` above, and trailing
        // `--` prevents it from being interpreted as a pathspec on both
        // Git 2.39 and newer releases (`git reset` on Git 2.39 rejects
        // `--end-of-options`).
        let output = crate::git_cmd::in_dir_async(&self.path)
            .args(["-c", "safe.bareRepository=all"])
            .args(["reset", "--hard", commit_hash, "--"])
            .output_cancellable()
            .await?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to populate worktree: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        Ok(())
    }

    pub async fn new_scratch_clone(
        repo_path: &Path,
        commit_hash: &str,
        parent_dir: Option<&Path>,
    ) -> Result<Self> {
        if commit_hash.is_empty() || commit_hash.starts_with('-') {
            return Err(anyhow!("Invalid commit revision: {}", commit_hash));
        }
        let parent_owned = parent_dir.map(|p| p.to_path_buf());
        let temp_dir = tokio::task::spawn_blocking(move || -> Result<TempDir> {
            if let Some(parent) = parent_owned {
                if !parent.exists() {
                    std::fs::create_dir_all(&parent)?;
                }
                Ok(tempfile::Builder::new()
                    .prefix("sashiko-review-")
                    .tempdir_in(parent)?)
            } else {
                Ok(tempfile::Builder::new()
                    .prefix("sashiko-review-")
                    .tempdir()?)
            }
        })
        .await??;
        let path = temp_dir.path().to_path_buf();
        // Wrap temp_dir in GitWorktree immediately so any error or cancellation
        // during clone or checkout offloads TempDir removal via remove/Drop.
        let worktree = Self {
            dir: Some(temp_dir),
            path: path.clone(),
            repo_path: repo_path.to_path_buf(),
            is_managed: false,
        };

        debug!("Creating scratch clone at {:?}", path);

        let output = match crate::git_cmd::detached_async()
            .args(["-c", "safe.bareRepository=all"])
            .arg("clone")
            .arg("--shared")
            .arg("--no-checkout")
            .arg("--")
            .arg(repo_path)
            .arg(&path)
            .output()
            .await
        {
            Ok(out) => out,
            Err(e) => {
                if let Err(cleanup_err) = worktree.remove().await {
                    warn!(
                        "Failed to clean up scratch clone {:?} after clone I/O error: {}",
                        path, cleanup_err
                    );
                }
                return Err(e.into());
            }
        };

        if !output.status.success() {
            let err = anyhow!(
                "Failed to create scratch clone: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            if let Err(cleanup_err) = worktree.remove().await {
                warn!(
                    "Failed to clean up scratch clone {:?} after failed clone: {}",
                    path, cleanup_err
                );
            }
            return Err(err);
        }

        // `commit_hash` is verified not to start with `-` above, and trailing
        // `--` prevents it from being interpreted as a pathspec on both
        // Git 2.39 and newer releases (`git reset` on Git 2.39 rejects
        // `--end-of-options`).
        let output = match crate::git_cmd::in_dir_async(&path)
            .args(["-c", "safe.bareRepository=all"])
            .args(["reset", "--hard", commit_hash, "--"])
            .output()
            .await
        {
            Ok(out) => out,
            Err(e) => {
                if let Err(cleanup_err) = worktree.remove().await {
                    warn!(
                        "Failed to clean up scratch clone {:?} after reset I/O error: {}",
                        path, cleanup_err
                    );
                }
                return Err(e.into());
            }
        };

        if !output.status.success() {
            let err = anyhow!(
                "Failed to populate scratch clone: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            if let Err(cleanup_err) = worktree.remove().await {
                warn!(
                    "Failed to clean up scratch clone {:?} after failed reset: {}",
                    path, cleanup_err
                );
            }
            return Err(err);
        }

        Ok(worktree)
    }

    async fn run_git_am(
        &self,
        patch_content: &str,
        three_way: bool,
    ) -> Result<std::process::Output> {
        let mut cmd = crate::git_cmd::in_dir_async(&self.path);
        cmd.env("LC_ALL", "C")
            .env("GIT_AUTHOR_NAME", "Sashiko Bot")
            .env("GIT_AUTHOR_EMAIL", "sashiko@localhost")
            .env("GIT_COMMITTER_NAME", "Sashiko Bot")
            .env("GIT_COMMITTER_EMAIL", "sashiko@localhost")
            .args(["-c", "safe.bareRepository=all"])
            .arg("am");
        if three_way {
            cmd.arg("--3way");
        }
        Ok(crate::git_cmd::output(&mut cmd, Some(patch_content.as_bytes())).await?)
    }

    async fn abort_git_am(&self) {
        let _ = crate::git_cmd::in_dir_async(&self.path)
            .args(["-c", "safe.bareRepository=all"])
            .arg("am")
            .arg("--abort")
            .output_cancellable()
            .await;
    }

    #[allow(dead_code)]
    pub async fn apply_patch(&self, patch_content: &str) -> Result<()> {
        debug!("Applying patch in {:?}", self.path);

        let before_head = get_commit_hash(&self.path, "HEAD").await?;
        let output = self.run_git_am(patch_content, false).await?;
        if output.status.success() {
            if get_commit_hash(&self.path, "HEAD").await? != before_head {
                return Ok(());
            }
            self.abort_git_am().await;
            return Err(GitAmError::new(&output.stdout, &output.stderr).into());
        }

        self.abort_git_am().await;
        let first_err = GitAmError::new(&output.stdout, &output.stderr);

        if first_err.kind == PatchApplyFailure::ContextMismatch
            && let Ok(retry_output) = self.run_git_am(patch_content, true).await
        {
            if retry_output.status.success()
                && get_commit_hash(&self.path, "HEAD")
                    .await
                    .is_ok_and(|after_head| after_head != before_head)
            {
                return Ok(());
            }
            self.abort_git_am().await;
        }

        Err(first_err.into())
    }

    pub async fn get_commit_show(&self, hash: &str) -> Result<String> {
        let output = crate::git_cmd::in_dir_async(&self.path)
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
        let output = crate::git_cmd::in_dir_async(&self.path)
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
        let output = crate::git_cmd::in_dir_async(&self.path)
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
        let output = crate::git_cmd::in_dir_async(&self.path)
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
        debug!("Resetting worktree to {}", ref_name);
        let output = crate::git_cmd::in_dir_async(&self.path)
            .args(["reset", "--hard", ref_name])
            .output_cancellable()
            .await?;

        if !output.status.success() {
            return Err(anyhow!(
                "git reset --hard failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        // Also clean untracked files to be safe
        let clean_output = crate::git_cmd::in_dir_async(&self.path)
            .args(["clean", "-fdx"])
            .output_cancellable()
            .await?;

        if !clean_output.status.success() {
            return Err(anyhow!(
                "git clean failed: {}",
                String::from_utf8_lossy(&clean_output.stderr)
            ));
        }

        Ok(())
    }

    pub(crate) async fn remove_directory(&mut self) -> Result<()> {
        // Delete the checkout directory on the blocking pool before acquiring
        // `get_worktree_lock()` so recursive filesystem deletion runs
        // concurrently without holding the global worktree mutex. Because
        // `git worktree remove -f <path>` matches `<path>` against
        // `$GIT_DIR/worktrees/*/gitdir` metadata without requiring `<path>` to
        // exist on disk, the locked `git worktree remove -f` below succeeds
        // after `dir.close()` and only needs to unlink the small
        // `$GIT_DIR/worktrees/<name>` administrative entry.
        if let Some(dir) = self.dir.take() {
            match tokio::task::spawn_blocking(move || dir.close()).await? {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                // Pruning metadata cannot make failed filesystem cleanup a
                // success. Callers must not start another attempt after this.
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn remove(mut self) -> Result<()> {
        let directory = self.remove_directory().await;
        // Even a failed checkout deletion must not skip unregistering it.
        // Preserve that failure so callers do not retry into another checkout.
        let registration = self.remove_registration().await;
        directory.and(registration)
    }

    async fn remove_registration(&mut self) -> Result<()> {
        if !self.is_managed {
            return Ok(());
        }
        debug!("Removing worktree at {:?}", self.path);
        let lock = get_worktree_lock();
        let output = {
            let _guard = lock.lock().await;
            let rm_out = crate::git_cmd::in_dir_async(&self.repo_path)
                .args(["-c", "safe.bareRepository=all"])
                .arg("worktree")
                .arg("remove")
                .arg("-f")
                .arg("--")
                .arg(&self.path)
                .output_cancellable()
                .await?;
            if rm_out.status.success() {
                rm_out
            } else {
                // If `git worktree add` failed before completing registration
                // (or only wrote a partial `$GIT_DIR/worktrees/<name>` entry),
                // `git worktree remove` reports "is not a working tree". Since
                // `self.dir` has already been deleted above, `git worktree prune`
                // reclaims any half-registered administrative entry.
                crate::git_cmd::in_dir_async(&self.repo_path)
                    .args(["-c", "safe.bareRepository=all"])
                    .args(["worktree", "prune"])
                    .output_cancellable()
                    .await?
            }
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

/// Matches a branch name against a list of branch patterns.
/// Each pattern is either an exact branch name or contains at most one
/// '*' wildcard (e.g. "linux-*.y", "v6.18/*").
pub fn matches_branch_pattern(branch: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        if pattern == branch {
            return true;
        }
        if let Some((prefix, suffix)) = pattern.split_once('*') {
            branch.starts_with(prefix)
                && branch.len() >= prefix.len() + suffix.len()
                && branch[prefix.len()..].ends_with(suffix)
        } else {
            false
        }
    })
}

#[allow(dead_code)]
pub async fn read_blob(repo_path: &Path, hash: &str) -> Result<Vec<u8>> {
    let output = crate::git_cmd::in_dir_async(repo_path)
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

fn is_safe_repo_relative_path(file_path: &str) -> bool {
    let trimmed = file_path.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('-')
        || trimmed.starts_with('/')
        || trimmed.contains('\0')
        || trimmed.chars().any(char::is_control)
    {
        return false;
    }
    Path::new(trimmed)
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Checks whether a specific file path exists in git at a specific commit (or HEAD).
pub async fn git_file_exists_at(repo_path: &Path, file_path: &str, commit: Option<&str>) -> bool {
    let file_path = file_path.trim();
    if !is_safe_repo_relative_path(file_path) {
        return false;
    }
    let target = commit.unwrap_or("HEAD").trim();
    if target.is_empty() || target.starts_with('-') || target.contains('\0') {
        return false;
    }
    let obj = format!("{}:{}", target, file_path);
    let output = crate::git_cmd::in_dir_async(repo_path)
        .args(["-c", "safe.bareRepository=all"])
        .args(["cat-file", "-e", "--end-of-options", &obj])
        .output()
        .await;

    matches!(output, Ok(out) if out.status.success())
}

/// Checks whether a commit or object exists in git using `cat-file -e`.
pub async fn git_commit_exists(repo_path: &Path, commit: &str) -> bool {
    let commit = commit.trim();
    if commit.is_empty() || commit.starts_with('-') {
        return false;
    }
    let obj = format!("{}^{{commit}}", commit);
    let output = crate::git_cmd::in_dir_async(repo_path)
        .args(["-c", "safe.bareRepository=all"])
        .args(["cat-file", "-e", &obj])
        .output()
        .await;

    matches!(output, Ok(out) if out.status.success())
}

/// Checks whether all endpoints in a commit range exist in git using `cat-file -e`.
pub async fn git_range_exists(repo_path: &Path, commit_or_range: &str) -> bool {
    if let Some((start, end)) = commit_or_range.split_once("..") {
        let start = start.trim();
        let end = end.trim_start_matches('.');
        (start.is_empty() || git_commit_exists(repo_path, start).await)
            && (end.is_empty() || git_commit_exists(repo_path, end).await)
    } else {
        git_commit_exists(repo_path, commit_or_range).await
    }
}

/// Traces Git history to check if a file was renamed or moved across commits,
/// returning its latest path at the given commit or top-of-trunk.
pub async fn git_find_file_rename(
    repo_path: &Path,
    file_path: &str,
    target_commit: Option<&str>,
) -> Option<String> {
    let file_path = file_path.trim();
    if !repo_path.exists() || !is_safe_repo_relative_path(file_path) {
        return None;
    }

    // 1. If file already exists at target, return as-is
    if git_file_exists_at(repo_path, file_path, target_commit).await {
        return Some(file_path.to_string());
    }

    // 2. Trace forward renames (up to 5 steps) via git show -M on the commit modifying the file
    let mut current_file = file_path.to_string();
    for _ in 0..5 {
        let last_commit_output = crate::git_cmd::in_dir_async(repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .args(["log", "-n", "1", "--format=%H", "--", &current_file])
            .output()
            .await
            .ok()?;

        if !last_commit_output.status.success() {
            break;
        }

        let commit_sha = String::from_utf8_lossy(&last_commit_output.stdout)
            .trim()
            .to_string();
        if commit_sha.is_empty() || commit_sha.starts_with('-') {
            break;
        }

        let diff_output = crate::git_cmd::in_dir_async(repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .args([
                "show",
                "-M",
                "--name-status",
                "--format=",
                "--end-of-options",
                &commit_sha,
            ])
            .output()
            .await
            .ok()?;

        if !diff_output.status.success() {
            break;
        }

        let diff_str = String::from_utf8_lossy(&diff_output.stdout);
        let mut renamed_to = None;
        for line in diff_str.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 3 && parts[0].starts_with('R') && parts[1] == current_file {
                renamed_to = Some(parts[2].to_string());
                break;
            }
        }

        if let Some(next_path) = renamed_to {
            if !is_safe_repo_relative_path(&next_path) {
                break;
            }
            current_file = next_path;
            if git_file_exists_at(repo_path, &current_file, target_commit).await {
                return Some(current_file);
            }
        } else {
            break;
        }
    }

    // 3. Fallback: check git log --follow backwards
    let mut cmd = crate::git_cmd::in_dir_async(repo_path);
    cmd.args(["-c", "safe.bareRepository=all"])
        .arg("log")
        .arg("--follow")
        .arg("--name-only")
        .arg("--format=format:")
        .arg("--")
        .arg(file_path);

    let Ok(output) = cmd.output().await else {
        return None;
    };
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if git_file_exists_at(repo_path, trimmed, target_commit).await {
                return Some(trimmed.to_string());
            }
        }
    }

    None
}

#[allow(dead_code)]
pub async fn prune_worktrees(repo_path: &Path) -> Result<()> {
    debug!("Pruning git worktrees in {:?}", repo_path);
    let output = {
        let lock = get_worktree_lock();
        let _guard = lock.lock().await;
        crate::git_cmd::in_dir_async(repo_path)
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
    debug!("Ensuring submodule config compatibility in {:?}", repo_path);
    let output = crate::git_cmd::in_dir_async(repo_path)
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
        debug!("Successfully unset core.worktree in {:?}", repo_path);
    }
    Ok(())
}

/// Turns off git's own maintenance of the shared repository.
///
/// Auto-maintenance detaches from the fetch that triggers it and
/// repacks while the sync worker keeps fetching, so it can discard
/// objects a later fetch still names.  Nothing runs gc in its place,
/// so the packs the repository accumulates are the cost of this.
///
/// Every key is attempted whatever the ones before it did, since a
/// key left at its default is a piece of maintenance still running,
/// and the error names all of them.  The caller logs and carries on
/// either way, so the rest of the daemon runs whether or not this
/// took.
pub async fn ensure_gc_disabled(repo_path: &Path) -> Result<()> {
    const KEYS: &[(&str, &str)] = &[
        ("gc.auto", "0"),
        ("maintenance.auto", "false"),
        ("gc.writeCommitGraph", "false"),
        ("fetch.writeCommitGraph", "false"),
    ];

    let mut failures = Vec::new();
    for (key, value) in KEYS {
        let output = crate::git_cmd::in_dir_async(repo_path)
            .args(["config", key, value])
            .output()
            .await?;

        if !output.status.success() {
            failures.push(format!(
                "{} {}: {}",
                key,
                value,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }

    if !failures.is_empty() {
        return Err(anyhow!("git config failed for {}", failures.join("; ")));
    }

    Ok(())
}

/// Locates a directory under the object store.  It follows the object
/// directory rather than $GIT_DIR, which is why git resolves it
/// instead of this function joining the two.
async fn object_dir(repo_path: &Path, name: &str) -> Result<PathBuf> {
    let relative = format!("objects/{}", name);
    let output = crate::git_cmd::in_dir_async(repo_path)
        .args(["rev-parse", "--git-path", &relative])
        .output()
        .await?;

    if !output.status.success() {
        return Err(anyhow!(
            "git rev-parse --git-path {} failed: {}",
            relative,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(repo_path.join(path))
    }
}

/// Removes the commit-graph, whole or chained.  The graph is built
/// from the object database and holds nothing else, so removing it
/// costs slower revision walks and loses no history.
///
/// This takes no object-store lock.  A write ends in a rename, so a
/// removal racing one leaves either a fresh graph or none, and both
/// are states the repository is already prepared for.  A fetch
/// recovering from a stale graph calls here holding a remote lock,
/// and waiting out a walk under that lock costs more than the race
/// does.  Callers already holding the lock, such as
/// write_commit_graph, depend on this staying out.
pub async fn drop_commit_graph(repo_path: &Path) -> Result<()> {
    let info_dir = object_dir(repo_path, "info").await?;

    let graph = info_dir.join("commit-graph");
    match tokio::fs::remove_file(&graph).await {
        Ok(()) => info!("Removed commit-graph {:?}", graph),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow!("Failed to remove {:?}: {}", graph, e)),
    }

    let chain = info_dir.join("commit-graphs");
    match tokio::fs::remove_dir_all(&chain).await {
        Ok(()) => info!("Removed commit-graph chain {:?}", chain),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(anyhow!("Failed to remove {:?}: {}", chain, e)),
    }

    Ok(())
}

/// Runs one `git commit-graph write` with incremental changed-path Bloom
/// filters bounded per pass so startup and maintenance walks stay fast on
/// repositories with over a million commits.
async fn run_commit_graph_write(repo_path: &Path) -> Result<std::process::Output> {
    Ok(crate::git_cmd::in_dir_async(repo_path)
        .args([
            "-c",
            "core.commitGraph=true",
            "commit-graph",
            "write",
            "--reachable",
            "--changed-paths",
            "--max-new-filters=50000",
        ])
        .output()
        .await?)
}

/// Counts the pack files in the object store and the bytes they take
/// up.  Reads the pack directory rather than asking git, so the cost
/// does not follow the number of loose objects.
pub async fn pack_stats(repo_path: &Path) -> Result<(usize, u64)> {
    let pack_dir = object_dir(repo_path, "pack").await?;

    let mut entries = match tokio::fs::read_dir(&pack_dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(e) => return Err(anyhow!("Failed to read {:?}: {}", pack_dir, e)),
    };

    let mut packs = 0usize;
    let mut bytes = 0u64;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "pack") {
            packs += 1;
            if let Ok(meta) = entry.metadata().await {
                bytes += meta.len();
            }
        }
    }

    Ok((packs, bytes))
}

async fn write_commit_graph_locked(repo_path: &Path) -> Result<()> {
    debug!("Writing commit-graph for {:?}", repo_path);
    let started = std::time::Instant::now();

    let mut output = run_commit_graph_write(repo_path).await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if is_stale_commit_graph(&stderr) {
            warn!(
                "Commit-graph in {:?} outlived the objects it names; dropping it",
                repo_path
            );
            drop_commit_graph(repo_path).await?;
            output = run_commit_graph_write(repo_path).await?;
        }
    }

    if !output.status.success() {
        return Err(anyhow!(
            "git commit-graph write failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    debug!(
        "Wrote commit-graph for {:?} in {:.1}s",
        repo_path,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Rebuilds the commit-graph from the current object database,
/// dropping a graph that turns the write away and walking again.
///
/// The graph records what the object database holds at the moment of
/// the walk.  A fetch running beside it costs the graph only the
/// commits that fetch installs.  A repack is what leaves an entry
/// with no object behind it, and the object-store lock this takes is
/// what keeps one out.
///
/// The walk consults the graph already in place, so one naming lost
/// objects fails the write the way it fails a fetch.  Nothing else
/// here needs that graph, so drop it and rebuild from the object
/// database alone.  This is the only pass that writes a graph, and
/// git's verify passes a graph that is merely behind the refs, so
/// the write cannot be made conditional on one.
pub async fn write_commit_graph(repo_path: &Path) -> Result<()> {
    let lock = get_object_store_lock();
    let _guard = lock.lock().await;
    write_commit_graph_locked(repo_path).await
}

/// Rebuilds the commit-graph in the background once a recovery has
/// removed it.
///
/// A recovery leaves the repository walking history with no graph
/// until something writes one back.  The walk takes minutes and every
/// caller holds a fetch lock, so none of them can wait for it.  A
/// rebuild already under way stands in for a later one, since it
/// reads the object database as it finds it.  Call from a tokio
/// runtime.
pub fn schedule_commit_graph_rebuild(repo_path: &Path) {
    static REBUILD_LOCK: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();
    let lock = REBUILD_LOCK
        .get_or_init(|| Arc::new(AsyncMutex::new(())))
        .clone();
    let repo_path = repo_path.to_path_buf();

    tokio::spawn(async move {
        let Ok(_guard) = lock.try_lock() else {
            debug!("A commit-graph rebuild is already running; skipping this one");
            return;
        };
        if let Err(e) = write_commit_graph(&repo_path).await {
            error!("Failed to rebuild the commit-graph: {}", e);
        }
    });
}

/// Rolls the pack directory up into a geometric progression and
/// writes a multi-pack index over the result.
///
/// Loose objects join the rollup.  A fetch carrying fewer objects
/// than transfer.unpackLimit writes them loose rather than as a pack,
/// so they are most of what accumulates here.
///
/// The pass takes no reachability walk, so it removes no object.  A
/// commit-graph entry, a worktree, or a scratch clone borrowing from
/// this store still finds what it named.  Disk goes unreclaimed for
/// the same reason: an object no ref can reach is rolled up with the
/// rest.
pub async fn repack_repository(repo_path: &Path) -> Result<()> {
    let lock = get_object_store_lock();
    let _guard = lock.lock().await;

    info!("Repacking {:?}", repo_path);
    let started = std::time::Instant::now();

    let output = crate::git_cmd::in_dir_async(repo_path)
        .args(["repack", "--geometric=2", "-d", "--write-midx"])
        .output()
        .await?;

    if !output.status.success() {
        return Err(anyhow!(
            "git repack failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    write_commit_graph_locked(repo_path).await?;

    info!(
        "Repacked {:?} in {:.1}s",
        repo_path,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

#[allow(dead_code)]
pub async fn cleanup_worktree_dir(worktree_dir: &Path) -> Result<()> {
    if !worktree_dir.exists() {
        return Ok(());
    }
    debug!(
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
    /// Runs when a worktree or scratch clone is dropped. If `remove()` was
    /// not called on a managed worktree (`is_managed` is true), logs a
    /// warning and leaves the `$GIT_DIR/worktrees` entry for
    /// `prune_worktrees` at the next start, since `git worktree remove` is
    /// async and cannot run here. Whenever `self.dir` is still `Some`
    /// (either a managed worktree dropped without `remove()` or an
    /// unmanaged `new_scratch_clone` whose `TempDir` owns a full checkout),
    /// `dir.close()` is offloaded to `Handle::spawn_blocking` so
    /// `remove_dir_all` never blocks an async worker thread and any
    /// filesystem cleanup error is logged (`Handle::spawn_blocking` never
    /// panics during runtime shutdown; if the blocking pool is already shut
    /// down, Tokio drops the closure and `TempDir::drop` still removes the
    /// directory).
    fn drop(&mut self) {
        if self.is_managed {
            warn!(
                "Dropping worktree at {:?}. Use explicit .remove() for clean git state.",
                self.path
            );
        }
        if let Some(dir) = self.dir.take() {
            let path = self.path.clone();
            let close_dir = move || {
                if let Err(e) = dir.close()
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    warn!(
                        "Failed to remove temporary worktree directory {:?} on drop: {}",
                        path, e
                    );
                }
            };
            match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.spawn_blocking(close_dir);
                }
                Err(_) => close_dir(),
            }
        }
    }
}

fn get_remote_lock(name: &str) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let map_mutex = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = map_mutex.lock().unwrap();
    if map.len() >= 64 {
        map.retain(|_, lock| Arc::strong_count(lock) > 1);
    }
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

/// A cancelled caller must not kill a configuration writer while it owns
/// config.lock. The task retains the mutex until Git finishes its atomic write.
async fn finish_config_write<F, T>(write: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let guard = get_global_config_lock().lock_owned().await;
    finish_write(async move {
        let _guard = guard;
        write.await
    })
    .await
}

struct RemoteConfigChanges {
    just_added: bool,
    refspecs_changed: bool,
    url_changed: bool,
}

async fn configure_remote(
    repo_path: &Path,
    name: &str,
    url: &str,
    refspecs: Option<&[String]>,
) -> Result<RemoteConfigChanges> {
    let mut just_added = false;
    let mut refspecs_changed = false;
    let mut url_changed = false;

    let config_url_key = format!("remote.{}.url", name);
    let check = crate::git_cmd::in_dir_async(repo_path)
        .args(["config", "--get", &config_url_key])
        .output_cancellable()
        .await?;

    let old_url = if check.status.success() {
        let current_url = String::from_utf8_lossy(&check.stdout).trim().to_string();
        if !are_git_urls_equivalent(&current_url, url) {
            url_changed = true;
        }
        Some(current_url)
    } else {
        just_added = true;
        None
    };

    // Configure fetch refspecs in .git/config. If selective refspecs are
    // provided, configure them; otherwise ensure the default wildcard refspec
    // is restored. Compare against current configuration to avoid unnecessary
    // writes across await points.
    let config_key = format!("remote.{}.fetch", name);
    let current_out = crate::git_cmd::in_dir_async(repo_path)
        .args(["config", "--get-all", &config_key])
        .output_cancellable()
        .await?;

    let current_refspecs: Vec<String> = if current_out.status.success() {
        String::from_utf8_lossy(&current_out.stdout)
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    } else {
        Vec::new()
    };

    let default_refspec = format!("+refs/heads/*:refs/remotes/{}/*", name);
    let target_refspecs: Option<Vec<&str>> = match refspecs {
        Some(specs) if !specs.is_empty() => Some(specs.iter().map(String::as_str).collect()),
        Some(_) => Some(vec![default_refspec.as_str()]),
        None => {
            if current_refspecs.is_empty() {
                Some(vec![default_refspec.as_str()])
            } else {
                None
            }
        }
    };

    if let Some(target) = target_refspecs.as_ref() {
        let target_refspecs_owned: Vec<String> = target.iter().map(|s| (*s).to_string()).collect();

        if current_refspecs != target_refspecs_owned {
            refspecs_changed = true;
        }
    }

    // Delete old tracking refs and clear timestamps BEFORE updating config,
    // so that a failure during deletion or task cancellation leaves the old config
    // intact rather than leaving stale refs paired with new config.
    // For 'origin', never delete tracking refs (e.g. mirror clone transitioning to upstream).
    if (url_changed || refspecs_changed) && name != "origin" {
        delete_remote_tracking_refs(repo_path, name).await?;
    }
    if url_changed || refspecs_changed {
        let timestamp_dir = repo_path.join(".sashiko/fetch_timestamps");
        let _ = tokio::fs::remove_file(timestamp_dir.join(name)).await;
        let _ = tokio::fs::remove_file(timestamp_dir.join(format!("{}.fail", name))).await;
    }

    if just_added {
        info!("Adding remote {} ({})", name, redact_secret(url));
        let add = crate::git_cmd::in_dir_async(repo_path)
            .args(["remote", "add", name, url])
            .output_cancellable()
            .await?;
        if !add.status.success() {
            let stderr = String::from_utf8_lossy(&add.stderr);
            if !stderr.contains("already exists") {
                return Err(anyhow!(
                    "Failed to add remote: {}",
                    redact_secret(stderr.trim())
                ));
            }
        }
    } else if url_changed {
        if let Some(ref current_url) = old_url {
            info!(
                "Updating remote URL for {} from {} to {}",
                name,
                redact_secret(current_url),
                redact_secret(url)
            );
        }
        let update = crate::git_cmd::in_dir_async(repo_path)
            .args(["remote", "set-url", name, url])
            .output_cancellable()
            .await?;
        if !update.status.success() {
            return Err(anyhow!(
                "Failed to update remote URL: {}",
                redact_secret(String::from_utf8_lossy(&update.stderr).trim())
            ));
        }
    }

    if refspecs_changed && let Some(target) = target_refspecs {
        let unset = crate::git_cmd::in_dir_async(repo_path)
            .args(["config", "--unset-all", &config_key])
            .output_cancellable()
            .await?;
        if !unset.status.success()
            && unset.status.code() != Some(5)
            && unset.status.code() != Some(1)
        {
            let stderr = String::from_utf8_lossy(&unset.stderr);
            return Err(anyhow!(
                "Failed to reset fetch refspecs for {}: {}",
                name,
                redact_secret(stderr.trim())
            ));
        }
        for spec in &target {
            let add_spec = crate::git_cmd::in_dir_async(repo_path)
                .args(["config", "--add", &config_key, spec])
                .output_cancellable()
                .await?;
            if !add_spec.status.success() {
                let stderr = String::from_utf8_lossy(&add_spec.stderr);
                return Err(anyhow!(
                    "Failed to configure fetch refspec for {}: {}",
                    name,
                    redact_secret(stderr.trim())
                ));
            }
        }
    }

    Ok(RemoteConfigChanges {
        just_added,
        refspecs_changed,
        url_changed,
    })
}

pub(crate) fn get_worktree_lock() -> Arc<AsyncMutex<()>> {
    static WORKTREE_LOCK: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();
    WORKTREE_LOCK
        .get_or_init(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

/// The lock every pass that walks or rewrites the shared object
/// store takes: the commit-graph verify, the commit-graph write, and
/// the repack.  Two writes collide on
/// objects/info/commit-graph.lock and one of them dies.  A verify
/// beside a write reads a graph the write is halfway through
/// replacing.  A walk beside a repack reads the pack directory the
/// repack is replacing.
///
/// Each pass runs to minutes on a tree the size of Linux, so a
/// caller can wait that long for the lock.  None of them holds a
/// fetch lock.
fn get_object_store_lock() -> Arc<AsyncMutex<()>> {
    static OBJECT_STORE_LOCK: OnceLock<Arc<AsyncMutex<()>>> = OnceLock::new();
    OBJECT_STORE_LOCK
        .get_or_init(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

/// Git's two wordings for a commit-graph that names a commit the
/// object database does not hold.  Fetch-pack's negotiation emits
/// the first.  The second comes from the generic commit parse, which
/// ref negotiation, a pruning fetch, and the connectivity check all
/// reach instead.
const STALE_COMMIT_GRAPH: &[&str] = &[
    "in the commit graph file but not in the object database",
    "exists in commit-graph but not in the object database",
];

/// True when git turned an operation away over a commit-graph that
/// outlived the objects it names.  Every path that reads the graph
/// fails this way until the graph is dropped, so a caller that
/// fetches has a retry worth making.
pub fn is_stale_commit_graph(message: &str) -> bool {
    STALE_COMMIT_GRAPH
        .iter()
        .any(|wording| message.contains(wording))
}

/// The least the retry after a graph drop is given, whatever the
/// first attempt spent.  Fetch-pack rejects the graph before it opens
/// a connection, but the wording the commit parse emits can arrive
/// after a long transfer, which leaves nothing of the shared budget.
/// A retry handed that reports a timeout instead of the recovery it
/// was, and the graph outlives the cycle.
const GRAPH_RETRY_FLOOR: std::time::Duration = std::time::Duration::from_secs(300);

/// Runs one fetch, returning git's complaint rather than an error
/// value, since the caller decides which failures are worth a retry.
async fn fetch_remote(
    repo_path: &Path,
    name: &str,
    args: &[&str],
    timeout: std::time::Duration,
) -> std::result::Result<(), String> {
    let mut command = crate::git_cmd::in_dir_async(repo_path);
    let fetch_future = command
        .args(GIT_PROTOCOL_RESTRICTIONS)
        .args(args)
        .output_cancellable();

    match tokio::time::timeout(timeout, fetch_future).await {
        Ok(Ok(fetch)) => {
            if fetch.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "Failed to fetch remote {}: {}",
                    name,
                    crate::utils::redact_secret(String::from_utf8_lossy(&fetch.stderr).trim())
                ))
            }
        }
        Ok(Err(e)) => Err(format!("Failed to execute git fetch for {}: {}", name, e)),
        Err(_) => Err(format!(
            "Git fetch for {} timed out after {} seconds",
            name,
            timeout.as_secs()
        )),
    }
}

/// What a remote's refs rest on once refresh_remote() returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    /// The remote answered.
    Fetched,
    /// The fetch interval had not elapsed, so nothing was asked of
    /// the remote and the refs are the ones already on disk.
    Skipped,
    /// A fetch failed within the last hour, so nothing was asked of
    /// the remote and the refs are the ones the previous fetch
    /// wrote.  A remote whose refs are missing or older than the
    /// fallback window returns a BackoffError instead.
    BackedOff,
    /// The fetch failed and the refs left behind are the ones the
    /// previous fetch wrote.  Carries what git said about the
    /// failure.
    StaleLocalRef(String),
}

/// A fetch failed within the last hour and the local refs cannot
/// stand in for it, so backing off leaves the caller nothing to
/// use.  Nothing was asked of the remote.
#[derive(Debug)]
pub struct BackoffError {
    name: String,
}

impl std::fmt::Display for BackoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Remote {} failed recently and has no usable local ref, backing off",
            self.name
        )
    }
}

impl std::error::Error for BackoffError {}

/// Brings a remote's refs up to date for a caller about to read
/// them. A remote in backoff is an error whether or not a local ref
/// could stand in, so a review does not proceed against refs it did
/// not ask to be refreshed. A fetch that fails against a recent
/// enough local ref succeeds, as it always has.
pub async fn ensure_remote(
    repo_path: &Path,
    name: &str,
    url: &str,
    force_fetch: bool,
) -> Result<()> {
    ensure_remote_with_refspecs(repo_path, name, url, force_fetch, None).await
}

/// Like ensure_remote, but optionally limits the fetch to specific
/// refspecs. When refspecs is Some, only the listed refs are fetched
/// instead of all remote branches.
pub async fn ensure_remote_with_refspecs(
    repo_path: &Path,
    name: &str,
    url: &str,
    force_fetch: bool,
    refspecs: Option<&[String]>,
) -> Result<()> {
    match refresh_remote_with_refspecs(repo_path, name, url, force_fetch, refspecs).await? {
        FetchOutcome::BackedOff => Err(anyhow::anyhow!(
            "Remote {} failed recently, backing off",
            name
        )),
        _ => Ok(()),
    }
}

/// Brings a remote's refs up to date and reports what they rest on.
/// A remote in backoff with a usable local ref is an Ok outcome
/// here, for a caller that wants to count it apart from a failure.
/// A caller that is about to read the refs wants ensure_remote(),
/// which treats every backoff as the error it was before the
/// outcome existed.
pub async fn refresh_remote(
    repo_path: &Path,
    name: &str,
    url: &str,
    force_fetch: bool,
) -> Result<FetchOutcome> {
    refresh_remote_with_refspecs(repo_path, name, url, force_fetch, None).await
}

/// Returns true if a branch pattern conforms to valid Git refspec syntax.
///
/// Refspec patterns must contain exactly one asterisk wildcard. Question mark
/// wildcards, spaces, multiple asterisks, and invalid ref characters are rejected.
pub fn is_valid_refspec_pattern(pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if pattern.contains('?') {
        return false;
    }
    if pattern.chars().filter(|&c| c == '*').count() != 1 {
        return false;
    }
    if pattern.contains("..")
        || pattern.contains('@')
        || pattern.contains('\\')
        || pattern.contains(':')
        || pattern.contains('^')
        || pattern.contains('~')
        || pattern.contains('[')
        || pattern.starts_with('/')
        || pattern.ends_with('/')
        || pattern.contains(".lock")
    {
        return false;
    }
    if pattern
        .chars()
        .any(|c| c.is_ascii_control() || c.is_ascii_whitespace())
    {
        return false;
    }
    true
}

/// Returns true if a branch name conforms to valid Git branch naming rules.
///
/// Literal branch names must not contain wildcards ('*', '?'), spaces, or invalid ref characters.
pub fn is_valid_branch_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.contains('*') || name.contains('?') {
        return false;
    }
    if name.contains("..")
        || name.contains('@')
        || name.contains('\\')
        || name.contains(':')
        || name.contains('^')
        || name.contains('~')
        || name.contains('[')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.contains(".lock")
    {
        return false;
    }
    if name
        .chars()
        .any(|c| c.is_ascii_control() || c.is_ascii_whitespace())
    {
        return false;
    }
    true
}

/// Checks whether any ref exists under refs/remotes/<name>/.
async fn has_remote_refs(repo_path: &Path, name: &str) -> bool {
    let head_ref = format!("refs/remotes/{}/HEAD", name);
    if let Ok(status) = crate::git_cmd::in_dir_async(repo_path)
        .args(["show-ref", "--verify", "-q", &head_ref])
        .status()
        .await
        && status.success()
    {
        return true;
    }
    let remote_pattern = format!("refs/remotes/{}/", name);
    if let Ok(output) = crate::git_cmd::in_dir_async(repo_path)
        .args(["for-each-ref", "--count=1", &remote_pattern])
        .output()
        .await
    {
        return output.status.success() && !output.stdout.is_empty();
    }
    false
}

fn normalize_git_url(url: &str) -> &str {
    let mut u = url.trim();
    if let Some(stripped) = u.strip_prefix("git://") {
        u = stripped;
    } else if let Some(stripped) = u.strip_prefix("https://") {
        u = stripped;
    } else if let Some(stripped) = u.strip_prefix("http://") {
        u = stripped;
    } else if let Some(stripped) = u.strip_prefix("ssh://") {
        u = stripped;
    }
    u = u.trim_end_matches('/');
    u = u.strip_suffix(".git").unwrap_or(u);
    u
}

/// Compares two git remote URLs for semantic equivalence, ignoring schemes
/// (git:// vs https:// vs http://), trailing slashes, and trailing .git suffixes.
pub fn are_git_urls_equivalent(a: &str, b: &str) -> bool {
    let a_trim = a.trim();
    let b_trim = b.trim();
    a_trim == b_trim || normalize_git_url(a_trim) == normalize_git_url(b_trim)
}

/// Deletes all local tracking refs under refs/remotes/<name>/.
async fn delete_remote_tracking_refs(repo_path: &Path, name: &str) -> Result<()> {
    let prefix = format!("refs/remotes/{}/", name);
    let out = crate::git_cmd::in_dir_async(repo_path)
        .args(["for-each-ref", "--format=%(refname)", &prefix])
        .output_cancellable()
        .await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!(
            "Failed to list remote refs for {}: {}",
            name,
            stderr.trim()
        ));
    }
    let refs = String::from_utf8_lossy(&out.stdout);
    let mut stdin_input = String::new();
    for refname in refs.lines() {
        let refname = refname.trim();
        if !refname.is_empty() {
            stdin_input.push_str(&format!("delete {}\n", refname));
        }
    }
    if !stdin_input.is_empty() {
        let mut command = crate::git_cmd::in_dir_async(repo_path);
        command.args(["update-ref", "--stdin", "--no-deref"]);
        let output = crate::git_cmd::output(&mut command, Some(stdin_input.as_bytes())).await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "Failed to delete old remote refs for {}: {}",
                name,
                stderr.trim()
            ));
        }
    }
    Ok(())
}

/// Policy controlling when remote refresh initiates a network fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteFetchPolicy {
    /// Only fetch if the remote is missing, its URL changed, or it has no local refs.
    /// If the remote is already present locally with valid refs, skip network I/O.
    IfMissing,
    /// Standard sync policy: fetch if interval elapsed, remote is missing, or URL changed.
    IfIntervalElapsed,
    /// Unconditionally fetch from the remote.
    Force,
}

/// Like refresh_remote, but optionally passes explicit refspecs to git fetch.
pub async fn refresh_remote_with_refspecs(
    repo_path: &Path,
    name: &str,
    url: &str,
    force_fetch: bool,
    refspecs: Option<&[String]>,
) -> Result<FetchOutcome> {
    let policy = if force_fetch {
        RemoteFetchPolicy::Force
    } else {
        RemoteFetchPolicy::IfIntervalElapsed
    };
    refresh_remote_with_policy(repo_path, name, url, policy, refspecs).await
}

/// Ensures a remote is configured locally and has refs available, fetching
/// only if the remote is new, its URL changed, or no local refs exist yet.
/// When refs already exist locally, skips network I/O completely.
pub async fn ensure_remote_if_missing(
    repo_path: &Path,
    name: &str,
    url: &str,
    refspecs: Option<&[String]>,
) -> Result<()> {
    match refresh_remote_with_policy(repo_path, name, url, RemoteFetchPolicy::IfMissing, refspecs)
        .await?
    {
        FetchOutcome::BackedOff => Err(anyhow::anyhow!(
            "Remote {} failed recently, backing off",
            name
        )),
        _ => Ok(()),
    }
}

/// Refreshes a remote according to the specified fetch policy.
pub async fn refresh_remote_with_policy(
    repo_path: &Path,
    name: &str,
    url: &str,
    policy: RemoteFetchPolicy,
    refspecs: Option<&[String]>,
) -> Result<FetchOutcome> {
    // 1. Validate repo_path to prevent git from traversing up to parent repos
    if !tokio::fs::try_exists(repo_path.join(".git"))
        .await
        .unwrap_or(false)
        && !tokio::fs::try_exists(repo_path.join("HEAD"))
            .await
            .unwrap_or(false)
    {
        return Err(anyhow::anyhow!(
            "{} is not a valid git repository. Run `sashiko repo sync` to clone it.",
            repo_path.display()
        ));
    }

    // acquire remote-specific lock
    let remote_guard = Arc::new(get_remote_lock(name).lock_owned().await);

    // 2. Finish configuration changes even if the preparation is cancelled.
    let (config_repo, config_name, config_url) =
        (repo_path.to_owned(), name.to_owned(), url.to_owned());
    let config_refspecs = refspecs.map(<[String]>::to_vec);
    let config_guard = remote_guard.clone();
    let RemoteConfigChanges {
        just_added,
        refspecs_changed,
        url_changed,
    } = finish_config_write(async move {
        let _guard = config_guard;
        configure_remote(
            &config_repo,
            &config_name,
            &config_url,
            config_refspecs.as_deref(),
        )
        .await
    })
    .await?;

    // 3. Lazy Fetch Check
    let timestamp_dir = repo_path.join(".sashiko/fetch_timestamps");
    tokio::fs::create_dir_all(&timestamp_dir).await?;
    let timestamp_file = timestamp_dir.join(name);
    let fail_file = timestamp_dir.join(format!("{}.fail", name));

    let age = tokio::fs::metadata(&timestamp_file)
        .await
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| std::time::SystemTime::now().duration_since(m).ok());

    let fail_age = tokio::fs::metadata(&fail_file)
        .await
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| std::time::SystemTime::now().duration_since(m).ok());

    let failed_recently = match fail_age {
        Some(a) => a < std::time::Duration::from_secs(3600),
        None => false,
    };

    // Check if any remote refs exist locally
    let has_refs = has_remote_refs(repo_path, name).await;

    // The local ref stands in for a fetch that fails, or is backed
    // off, only while it is recent enough, and neither refspecs nor URL have changed.
    // For 'origin', URL change from a seed mirror to upstream does not invalidate
    // existing mirror-seeded refs, which remain usable if upstream is unreachable.
    let max_stale_age = std::time::Duration::from_secs(86400); // 24 hours
    let can_fallback = has_refs
        && !refspecs_changed
        && (!url_changed || name == "origin")
        && match age {
            Some(a) => a < max_stale_age,
            None => name == "origin",
        };

    let force_fetch = policy == RemoteFetchPolicy::Force;

    let should_fetch = match policy {
        RemoteFetchPolicy::Force => true,
        RemoteFetchPolicy::IfMissing => just_added || url_changed || refspecs_changed || !has_refs,
        RemoteFetchPolicy::IfIntervalElapsed => {
            if just_added || url_changed || refspecs_changed || !has_refs {
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
            }
        }
    };

    if !should_fetch {
        let reason = if policy == RemoteFetchPolicy::IfMissing {
            "local refs available"
        } else if force_fetch {
            "forced but recently fetched"
        } else {
            "fresh"
        };
        debug!("Skipping fetch for {} ({})", name, reason);
        return Ok(FetchOutcome::Skipped);
    }

    if failed_recently && !force_fetch {
        if !can_fallback {
            return Err(BackoffError {
                name: name.to_string(),
            }
            .into());
        }
        let reason = "failed recently, backing off";
        debug!("Skipping fetch for {} ({})", name, reason);
        return Ok(FetchOutcome::BackedOff);
    }

    let mut fetch_ok = false;
    let mut error_msg = String::new();

    // 4. Fetch
    if should_fetch {
        debug!("Fetching remote {}", name);

        let mut fetch_args = vec!["fetch", "--prune"];
        if !url.contains("torvalds/linux.git") && !url.contains("stable/linux.git") {
            fetch_args.push("--no-tags");
        }
        fetch_args.push(name);
        if let Some(specs) = refspecs {
            for spec in specs {
                fetch_args.push(spec.as_str());
            }
        }

        // Dynamically scale timeout: 30 minutes for heavy initial fetches, 5 minutes for routine updates
        let timeout_duration = if just_added || !has_refs {
            std::time::Duration::from_secs(1800)
        } else {
            std::time::Duration::from_secs(300)
        };

        let started = std::time::Instant::now();
        let mut attempt = fetch_remote(repo_path, name, &fetch_args, timeout_duration).await;

        // Git reads the commit-graph before it opens a connection, so
        // a graph naming lost objects fails every remote in the same
        // way.  Drop it and let this fetch rebuild the answer from the
        // object database.
        let stale_graph = attempt
            .as_ref()
            .err()
            .is_some_and(|message| is_stale_commit_graph(message));
        if stale_graph {
            warn!("Fetch for {} found a stale commit-graph; dropping it", name);
            match drop_commit_graph(repo_path).await {
                Ok(()) => {
                    // Both attempts come out of the one budget.  This
                    // holds the remote's lock, and the sync cycle
                    // walks the remotes one at a time.  The floor is
                    // what a first attempt that spent the budget
                    // before it tripped leaves the retry.
                    let remaining = timeout_duration
                        .saturating_sub(started.elapsed())
                        .max(GRAPH_RETRY_FLOOR);
                    attempt = fetch_remote(repo_path, name, &fetch_args, remaining).await;
                    match &attempt {
                        Ok(()) => info!(
                            "Fetch for {} succeeded once the commit-graph was gone",
                            name
                        ),
                        Err(message) => warn!(
                            "Fetch for {} fails with no commit-graph in the way: {}",
                            name, message
                        ),
                    }
                    schedule_commit_graph_rebuild(repo_path);
                }
                Err(e) => warn!("Failed to drop the commit-graph: {}", e),
            }
        }

        match attempt {
            Ok(()) => fetch_ok = true,
            Err(message) => error_msg = message,
        }

        // A retry can skip fetching as soon as refs exist. Own all post-fetch
        // finalization so cancellation cannot skip HEAD initialization. Keep
        // bounded network discovery outside the global configuration lock,
        // and retain remote serialization through the filesystem writes.
        let (completion_repo, completion_name) = (repo_path.to_owned(), name.to_owned());
        finish_write(async move {
            let _guard = remote_guard.clone();
            if fetch_ok {
                let head = remote_writes::discover_head(&completion_repo, &completion_name).await;
                finish_config_write(async move {
                    let _guard = remote_guard;
                    remote_writes::update_refs(&completion_repo, &completion_name, head.as_deref())
                        .await
                })
                .await?;
                let _ = tokio::fs::remove_file(&fail_file).await;
                let _ = tokio::fs::write(&timestamp_file, []).await;
            } else {
                let _ = tokio::fs::write(&fail_file, []).await;
            }
            Ok(())
        })
        .await?;

        if !fetch_ok {
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
        }
    }

    if fetch_ok {
        Ok(FetchOutcome::Fetched)
    } else {
        Ok(FetchOutcome::StaleLocalRef(error_msg))
    }
}

pub async fn get_remote_branches(repo_path: &Path, remote_name: &str) -> Result<Vec<String>> {
    let output = crate::git_cmd::in_dir_async(repo_path)
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

/// Resolve a revision to an existing commit, peeling annotated tags.
pub async fn get_commit_hash(path: &Path, ref_name: &str) -> Result<String> {
    let output = crate::git_cmd::in_dir_async(path)
        .args(["rev-parse", "--verify", "--end-of-options"])
        .arg(format!("{ref_name}^{{commit}}"))
        .output_cancellable()
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

    let output = crate::git_cmd::in_dir_async(&params.repo_path)
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
    // Force --untracked-files=normal so the output does not depend on a
    // user's global status.showUntrackedFiles setting.
    let output = crate::git_cmd::in_dir_async(repo_path)
        .args(["status", "--untracked-files=normal"])
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
    let output = crate::git_cmd::in_dir_async(repo_path)
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
    let parent_output = crate::git_cmd::in_dir_async(repo_path)
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

    let output = crate::git_cmd::in_dir_async(repo_path)
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
    let output = crate::git_cmd::in_dir_async(repo_path)
        .args(["-c", "safe.bareRepository=all"])
        .args(["status", "--porcelain", "--untracked-files=no"])
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

    async fn run_test_git(repo: &Path, args: &[&str]) -> Result<String> {
        let output = crate::git_cmd::in_dir_async(repo)
            .args(args)
            .output()
            .await?;
        anyhow::ensure!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    async fn commit_resolution_repo() -> Result<TempDir> {
        let repo = tempfile::tempdir()?;
        run_test_git(repo.path(), &["init", "-q", "-b", "main"]).await?;
        run_test_git(repo.path(), &["config", "user.name", "Test User"]).await?;
        run_test_git(repo.path(), &["config", "user.email", "test@example.com"]).await?;
        std::fs::write(repo.path().join("value.txt"), "base\n")?;
        run_test_git(repo.path(), &["add", "value.txt"]).await?;
        run_test_git(repo.path(), &["commit", "-q", "-s", "-m", "base"]).await?;
        Ok(repo)
    }

    #[tokio::test]
    async fn allocated_worktree_survives_cancelled_population() -> Result<()> {
        let repo = commit_resolution_repo().await?;
        let parent = tempfile::tempdir()?;
        let mut worktree = GitWorktree::allocate(repo.path(), Some(parent.path())).await?;
        let path = worktree.path.clone();
        let lock = get_worktree_lock();
        let guard = lock.lock().await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                worktree.populate("HEAD"),
            )
            .await
            .is_err()
        );
        drop(guard);
        assert!(path.exists(), "the caller still owns the checkout");
        worktree.remove().await?;
        assert!(!path.exists());

        let mut next = GitWorktree::allocate(repo.path(), Some(parent.path())).await?;
        assert_ne!(path, next.path);
        next.populate("HEAD").await?;
        assert_eq!(
            std::fs::read_to_string(next.path.join("value.txt"))?,
            "base\n"
        );
        next.remove().await?;
        assert_eq!(std::fs::read_dir(parent.path())?.count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn allocated_worktree_cleans_up_population_error() -> Result<()> {
        let repo = commit_resolution_repo().await?;
        let parent = tempfile::tempdir()?;
        let mut worktree = GitWorktree::allocate(repo.path(), Some(parent.path())).await?;
        let path = worktree.path.clone();
        assert!(worktree.populate("missing-ref").await.is_err());
        worktree.remove().await?;
        assert!(!path.exists());
        let registered = run_test_git(repo.path(), &["worktree", "list", "--porcelain"]).await?;
        assert!(!registered.contains(&path.to_string_lossy().to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn failed_directory_cleanup_is_not_reported_as_success() -> Result<()> {
        let repo = commit_resolution_repo().await?;
        let parent = tempfile::tempdir()?;
        for managed in [false, true] {
            let mut worktree = GitWorktree::allocate(repo.path(), Some(parent.path())).await?;
            if managed {
                worktree.populate("HEAD").await?;
            }
            let path = worktree.path.clone();
            // A non-directory deterministically makes TempDir::close fail,
            // including when the test is run with elevated filesystem access.
            std::fs::remove_dir_all(&path)?;
            std::fs::write(&path, b"not a directory")?;
            assert!(worktree.remove().await.is_err());
            assert!(path.is_file());
            let registered =
                run_test_git(repo.path(), &["worktree", "list", "--porcelain"]).await?;
            assert!(!registered.contains(&path.to_string_lossy().to_string()));
        }
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_config_writer_retains_its_lock_until_finished() -> Result<()> {
        let repo = tempfile::tempdir()?;
        let path = repo.path().join("config.lock");
        let writer_path = path.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (finish, released) = tokio::sync::oneshot::channel();
        let writer = tokio::spawn(finish_config_write(async move {
            tokio::fs::write(&writer_path, b"in progress").await?;
            started.send(()).unwrap();
            released.await?;
            tokio::fs::remove_file(writer_path).await?;
            Ok(())
        }));
        ready.await?;
        writer.abort();
        assert!(writer.await.unwrap_err().is_cancelled());
        assert!(path.exists());
        let lock = get_global_config_lock();
        assert!(lock.try_lock().is_err());
        finish.send(()).unwrap();
        let _guard = tokio::time::timeout(std::time::Duration::from_secs(60), lock.lock()).await?;
        assert!(!path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_remote_reconfiguration_finishes_under_owned_locks() -> Result<()> {
        let repo = commit_resolution_repo().await?;
        let name = "cancelled-reconfiguration";
        let reference = format!("refs/remotes/{name}/main");
        run_test_git(repo.path(), &["remote", "add", name, "./old-upstream"]).await?;
        run_test_git(repo.path(), &["update-ref", &reference, "HEAD"]).await?;
        let timestamp_dir = repo.path().join(".sashiko/fetch_timestamps");
        std::fs::create_dir_all(&timestamp_dir)?;
        std::fs::write(timestamp_dir.join(name), [])?;
        std::fs::write(timestamp_dir.join(format!("{name}.fail")), [])?;

        let ready = repo.path().join("writer-ready");
        let release = repo.path().join("writer-release");
        crate::test_support::write_executable_script(
            &repo.path().join(".git/hooks/reference-transaction"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = prepared ]; then\n  touch '{}'\n  while [ ! -f '{}' ]; do sleep .01; done\nfi\nexit 0\n",
                ready.display(),
                release.display()
            ),
        );

        let pending = PendingWrites::default();
        let caller_pending = pending.clone();
        let remote_lock = get_remote_lock(name);
        let path = repo.path().to_owned();
        let refspec = format!("+refs/heads/topic:refs/remotes/{name}/topic");
        let caller_refspec = refspec.clone();
        let caller = tokio::spawn(async move {
            caller_pending
                .scope(crate::git_cmd::with_isolated_processes(
                    refresh_remote_with_policy(
                        &path,
                        name,
                        "./new-upstream",
                        RemoteFetchPolicy::IfMissing,
                        Some(&[caller_refspec]),
                    ),
                ))
                .await
        });
        let started = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while !ready.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        caller.abort();
        let result = caller.await;
        let remote_still_locked = remote_lock.try_lock().is_err();
        std::fs::write(release, [])?;
        tokio::time::timeout(std::time::Duration::from_secs(30), pending.finish()).await??;
        started?;
        assert!(result.unwrap_err().is_cancelled());
        assert!(remote_still_locked);
        assert!(remote_lock.try_lock().is_ok());
        assert!(get_commit_hash(repo.path(), &reference).await.is_err());
        assert_eq!(
            run_test_git(
                repo.path(),
                &["config", "--get", &format!("remote.{name}.url")]
            )
            .await?,
            "./new-upstream"
        );
        assert_eq!(
            run_test_git(
                repo.path(),
                &["config", "--get-all", &format!("remote.{name}.fetch")]
            )
            .await?,
            refspec
        );
        assert!(!timestamp_dir.join(name).exists());
        assert!(!timestamp_dir.join(format!("{name}.fail")).exists());
        assert!(!repo.path().join(".git/config.lock").exists());
        assert!(!repo.path().join(format!(".git/{reference}.lock")).exists());
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_fetch_finishes_head_initialization_before_retry() -> Result<()> {
        let upstream = commit_resolution_repo().await?;
        run_test_git(upstream.path(), &["branch", "-m", "topic"]).await?;
        let repo = commit_resolution_repo().await?;
        let name = "cancelled-head-finalization";
        let url = upstream.path().to_str().unwrap().to_owned();
        run_test_git(repo.path(), &["remote", "add", name, &url]).await?;
        run_test_git(
            repo.path(),
            &[
                "config",
                &format!("remote.{name}.followRemoteHEAD"),
                "never",
            ],
        )
        .await?;
        let upload_pack = repo.path().join("upload-pack");
        let fetched = repo.path().join("upload-started");
        let ready = repo.path().join("discovery-ready");
        let release = repo.path().join("discovery-release");
        crate::test_support::write_executable_script(
            &upload_pack,
            &format!(
                "#!/bin/sh\nif [ -f '{}' ]; then\n  touch '{}'\n  while [ ! -f '{}' ]; do sleep .01; done\nelse\n  touch '{}'\nfi\nexec git-upload-pack \"$@\"\n",
                fetched.display(),
                ready.display(),
                release.display(),
                fetched.display()
            ),
        );
        run_test_git(
            repo.path(),
            &[
                "config",
                &format!("remote.{name}.uploadpack"),
                upload_pack.to_str().unwrap(),
            ],
        )
        .await?;

        let pending = PendingWrites::default();
        let caller_pending = pending.clone();
        let remote_lock = get_remote_lock(name);
        let path = repo.path().to_owned();
        let caller_url = url.clone();
        let caller = tokio::spawn(async move {
            caller_pending
                .scope(crate::git_cmd::with_isolated_processes(
                    ensure_remote_if_missing(&path, name, &caller_url, None),
                ))
                .await
        });
        let started = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while !ready.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        caller.abort();
        let result = caller.await;
        let remote_still_locked = remote_lock.try_lock().is_err();
        std::fs::write(release, [])?;
        tokio::time::timeout(std::time::Duration::from_secs(30), pending.finish()).await??;
        started?;
        assert!(result.unwrap_err().is_cancelled());
        assert!(remote_still_locked);
        assert_eq!(
            run_test_git(
                repo.path(),
                &["symbolic-ref", &format!("refs/remotes/{name}/HEAD")]
            )
            .await?,
            format!("refs/remotes/{name}/topic")
        );
        // The completed initialization lets the retry use local refs without
        // another fetch or HEAD advertisement, even with no working transport.
        std::fs::remove_file(upload_pack)?;
        assert!(matches!(
            refresh_remote_with_policy(repo.path(), name, &url, RemoteFetchPolicy::IfMissing, None)
                .await?,
            FetchOutcome::Skipped
        ));
        Ok(())
    }

    #[tokio::test]
    async fn remote_discovery_preserves_custom_heads_and_common_fallbacks() -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let upstream = commit_resolution_repo().await?;
        run_test_git(upstream.path(), &["branch", "-m", "topic"]).await?;
        run_test_git(upstream.path(), &["branch", "main"]).await?;
        let branch = crate::git_cmd::in_dir_async(upstream.path())
            .arg("branch")
            .arg(std::ffi::OsStr::from_bytes(b"unrelated-\xff"))
            .output()
            .await?;
        assert!(branch.status.success());
        let repo = commit_resolution_repo().await?;
        ensure_remote(
            repo.path(),
            "custom-head",
            upstream.path().to_str().unwrap(),
            true,
        )
        .await?;
        assert_eq!(
            run_test_git(
                repo.path(),
                &["symbolic-ref", "refs/remotes/custom-head/HEAD"]
            )
            .await?,
            "refs/remotes/custom-head/topic"
        );
        let changed = crate::git_cmd::in_dir_async(upstream.path())
            .args(["symbolic-ref", "HEAD"])
            .arg(std::ffi::OsStr::from_bytes(b"refs/heads/unrelated-\xff"))
            .output()
            .await?;
        assert!(changed.status.success());
        ensure_remote(
            repo.path(),
            "custom-head",
            upstream.path().to_str().unwrap(),
            true,
        )
        .await?;
        let head = crate::git_cmd::in_dir_async(repo.path())
            .args(["symbolic-ref", "refs/remotes/custom-head/HEAD"])
            .output()
            .await?;
        assert!(head.status.success());
        assert_eq!(head.stdout, b"refs/remotes/custom-head/unrelated-\xff\n");
        run_test_git(
            repo.path(),
            &["update-ref", "refs/remotes/custom-head/main", "HEAD"],
        )
        .await?;
        // Missing advertised branches retain the existing common-name fallback.
        remote_writes::update_refs(
            repo.path(),
            "custom-head",
            Some(std::ffi::OsStr::new("missing")),
        )
        .await?;
        assert_eq!(
            run_test_git(
                repo.path(),
                &["symbolic-ref", "refs/remotes/custom-head/HEAD"]
            )
            .await?,
            "refs/remotes/custom-head/main"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_shared_ref_writes_finish_without_stranding_locks() -> Result<()> {
        for reference in ["refs/heads/master", "refs/heads/main"] {
            let repo = commit_resolution_repo().await?;
            run_test_git(repo.path(), &["remote", "add", "origin", "."]).await?;
            run_test_git(repo.path(), &["branch", "master"]).await?;
            run_test_git(
                repo.path(),
                &["config", "remote.origin.followRemoteHEAD", "always"],
            )
            .await?;
            run_test_git(
                repo.path(),
                &["update-ref", "refs/remotes/origin/main", "HEAD"],
            )
            .await?;
            run_test_git(
                repo.path(),
                &["update-ref", "refs/remotes/origin/master", "HEAD"],
            )
            .await?;
            let ready = repo.path().join("writer-ready");
            let release = repo.path().join("writer-release");
            crate::test_support::write_executable_script(
                &repo.path().join(".git/hooks/reference-transaction"),
                &format!(
                    "#!/bin/sh\nif [ \"$1\" = prepared ] && grep -q ' {reference}$'; then\n  printf '%s' \"$PPID\" > '{}'\n  while [ ! -f '{}' ]; do sleep .01; done\nfi\nexit 0\n",
                    ready.display(),
                    release.display()
                ),
            );
            let pending = PendingWrites::default();
            let caller_pending = pending.clone();
            let remote_lock = get_remote_lock("cancelled-shared-ref-test");
            let guard = remote_lock.clone().lock_owned().await;
            let path = repo.path().to_owned();
            let caller = tokio::spawn(async move {
                caller_pending
                    .scope(crate::git_cmd::with_isolated_processes(
                        finish_config_write(async move {
                            let _guard = guard;
                            remote_writes::update_refs(
                                &path,
                                "origin",
                                Some(std::ffi::OsStr::new("main")),
                            )
                            .await
                        }),
                    ))
                    .await
            });
            let pgid = tokio::time::timeout(std::time::Duration::from_secs(30), async {
                loop {
                    if let Some(pgid) = std::fs::read_to_string(&ready)
                        .ok()
                        .and_then(|value| value.parse::<i32>().ok())
                    {
                        break pgid;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await?;
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
            let lockfile = repo.path().join(format!(".git/{reference}.lock"));
            assert!(lockfile.exists());
            assert!(remote_lock.try_lock().is_err());
            assert!(
                crate::process_group::SHARED_WRITER_GROUPS
                    .lock()
                    .unwrap()
                    .contains(&pgid)
            );
            if reference == "refs/heads/main" {
                // Shutdown must let Git's signal handlers remove its locks,
                // including when a reference hook prevents normal completion.
                crate::process_group::terminate_group(pgid);
            } else {
                std::fs::write(release, [])?;
            }
            tokio::time::timeout(std::time::Duration::from_secs(30), pending.finish()).await??;
            assert!(!lockfile.exists());
            assert!(!repo.path().join(".git/config.lock").exists());
            assert!(remote_lock.try_lock().is_ok());
            assert_eq!(
                run_test_git(repo.path(), &["symbolic-ref", "refs/remotes/origin/HEAD"]).await?,
                "refs/remotes/origin/main"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_get_commit_hash_resolution_and_rejection() -> Result<()> {
        let repo = commit_resolution_repo().await?;
        let head = run_test_git(repo.path(), &["rev-parse", "HEAD"]).await?;
        run_test_git(repo.path(), &["tag", "lightweight"]).await?;
        run_test_git(repo.path(), &["tag", "-a", "annotated", "-m", "release"]).await?;
        let tag = run_test_git(repo.path(), &["rev-parse", "annotated"]).await?;
        assert_ne!(tag, head);

        for ref_name in [
            "HEAD",
            "refs/heads/main",
            head.as_str(),
            &head[..12],
            "lightweight",
            "annotated",
            tag.as_str(),
        ] {
            assert_eq!(
                get_commit_hash(repo.path(), ref_name).await?,
                head,
                "failed to resolve {ref_name} to its commit"
            );
        }

        for ref_name in ["1111111111111111111111111111111111111111", "missing-branch"] {
            assert!(
                get_commit_hash(repo.path(), ref_name).await.is_err(),
                "accepted missing revision {ref_name}"
            );
        }

        for ref_name in ["HEAD^{tree}", "HEAD:value.txt"] {
            let hash = run_test_git(repo.path(), &["rev-parse", ref_name]).await?;
            assert!(
                get_commit_hash(repo.path(), &hash).await.is_err(),
                "accepted non-commit object {ref_name} ({hash})"
            );
        }

        for ref_name in ["--all", "--default=HEAD", "HEAD..HEAD", ""] {
            assert!(
                get_commit_hash(repo.path(), ref_name).await.is_err(),
                "accepted invalid revision {ref_name:?}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_git_commit_exists_and_range_exists() -> Result<()> {
        let repo = commit_resolution_repo().await?;
        let head = run_test_git(repo.path(), &["rev-parse", "HEAD"]).await?;

        assert!(git_commit_exists(repo.path(), &head).await);
        assert!(git_commit_exists(repo.path(), "HEAD").await);
        assert!(git_commit_exists(repo.path(), &head[..8]).await);

        assert!(!git_commit_exists(repo.path(), "1111111111111111111111111111111111111111").await);
        assert!(!git_commit_exists(repo.path(), "missing-branch").await);
        assert!(!git_commit_exists(repo.path(), "HEAD^{tree}").await);
        assert!(!git_commit_exists(repo.path(), "").await);
        assert!(!git_commit_exists(repo.path(), "--all").await);

        assert!(git_range_exists(repo.path(), &head).await);
        assert!(git_range_exists(repo.path(), "HEAD..HEAD").await);
        assert!(git_range_exists(repo.path(), &format!("{}..HEAD", head)).await);
        assert!(git_range_exists(repo.path(), "..HEAD").await);
        assert!(git_range_exists(repo.path(), "HEAD..").await);
        assert!(!git_range_exists(repo.path(), "missing..HEAD").await);
        assert!(!git_range_exists(repo.path(), "HEAD..missing").await);
        assert!(!git_range_exists(repo.path(), "-n..HEAD").await);

        Ok(())
    }

    #[test]
    fn test_is_stale_commit_graph_matches_both_wordings() {
        assert!(is_stale_commit_graph(
            "error: You are attempting to fetch 1a2b3c, which is in the \
             commit graph file but not in the object database."
        ));
        assert!(is_stale_commit_graph(
            "fatal: commit 1a2b3c exists in commit-graph but not in the \
             object database"
        ));
        assert!(!is_stale_commit_graph("fatal: couldn't find remote ref"));
    }

    #[tokio::test]
    async fn test_pack_stats_counts_only_packs() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        crate::git_cmd::in_dir_async(&repo_path)
            .args(["init"])
            .output()
            .await?;

        // A repository that has never packed reports nothing.
        assert_eq!(pack_stats(&repo_path).await?, (0, 0));

        let pack_dir = object_dir(&repo_path, "pack").await?;
        std::fs::create_dir_all(&pack_dir)?;
        let mut pack = File::create(pack_dir.join("pack-abc.pack"))?;
        pack.write_all(b"0123456789")?;
        File::create(pack_dir.join("pack-abc.idx"))?;

        let (packs, bytes) = pack_stats(&repo_path).await?;
        assert_eq!(packs, 1);
        assert_eq!(bytes, 10);

        Ok(())
    }

    #[tokio::test]
    async fn test_git_ops_extensions() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        // Init git repo
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["init"])
            .output()
            .await?;

        // Ensure we are on master
        let _ = crate::git_cmd::in_dir_async(&repo_path)
            .args(["branch", "-m", "master"])
            .output()
            .await;

        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;

        // Commit 1
        let file_path = repo_path.join("test.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "Hello World")?;

        crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "."])
            .output()
            .await?;

        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-m", "Initial commit"])
            .output()
            .await?;

        // Test git_status
        let status = git_status(&repo_path).await?;
        assert!(status.contains("nothing to commit, working tree clean"));

        Ok(())
    }

    #[tokio::test]
    async fn test_is_dirty_ignores_untracked_files() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        for args in [
            vec!["init"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test User"],
        ] {
            crate::git_cmd::in_dir_async(&repo_path)
                .args(&args)
                .output()
                .await?;
        }
        let tracked = repo_path.join("tracked.txt");
        writeln!(File::create(&tracked)?, "one")?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "tracked.txt"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-m", "Initial commit"])
            .output()
            .await?;
        assert!(!is_dirty(&repo_path).await?);

        // A build log or a stray patch directory is not a change to the
        // files under review.
        writeln!(File::create(repo_path.join("build.log"))?, "noise")?;
        assert!(!is_dirty(&repo_path).await?);

        writeln!(File::create(&tracked)?, "two")?;
        assert!(is_dirty(&repo_path).await?);

        Ok(())
    }

    #[tokio::test]
    async fn test_git_log() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        // Init git repo
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["init"])
            .output()
            .await?;

        // Configure user
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;

        // Commit 1
        let file_path = repo_path.join("test.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "Hello World")?;

        crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "."])
            .output()
            .await?;

        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-m", "Initial commit"])
            .output()
            .await?;

        // Commit 2
        let mut file = std::fs::OpenOptions::new().append(true).open(&file_path)?;
        writeln!(file, "Change 1")?;

        crate::git_cmd::in_dir_async(&repo_path)
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
    async fn test_apply_patch_classifies_failures_and_aborts() -> Result<()> {
        let repo = commit_resolution_repo().await?;
        let head = get_commit_hash(repo.path(), "HEAD").await?;
        let worktree = GitWorktree::new(repo.path(), &head, None).await?;

        for (diff, expected) in [
            (
                "diff --git a/missing.txt b/missing.txt\n--- a/missing.txt\n+++ b/missing.txt\n@@ -1 +1 @@\n-old\n+new\n",
                PatchApplyFailure::MissingFile,
            ),
            (
                "diff --git a/value.txt b/value.txt\nnew file mode 100644\n--- /dev/null\n+++ b/value.txt\n@@ -0,0 +1 @@\n+new\n",
                PatchApplyFailure::ExistingFile,
            ),
            (
                "diff --git a/value.txt b/value.txt\n--- a/value.txt\n+++ b/value.txt\n@@ -1 +1 @@\n-other\n+new\n",
                PatchApplyFailure::ContextMismatch,
            ),
            (
                "diff --git a/value.txt b/value.txt\n--- a/value.txt\n+++ b/value.txt\n@@ -1 +1 @@\n",
                PatchApplyFailure::MalformedPatch,
            ),
            (
                "---\n@@ -1 +1 @@\n-base\n+new\n",
                PatchApplyFailure::MalformedPatch,
            ),
            ("", PatchApplyFailure::EmptyPatch),
            ("Invalid patch content", PatchApplyFailure::EmptyPatch),
        ] {
            let mbox = format!(
                "From: Test User <test@example.com>\n\
                 Date: Tue, 14 Nov 2023 22:13:20 +0000\n\
                 Subject: [PATCH] example\n\n{diff}"
            );
            let error = worktree.apply_patch(&mbox).await.unwrap_err();
            assert_eq!(PatchApplyFailure::from_error(&error), expected, "{error:#}");
            assert!(error.to_string().contains("git am failed. stdout:"));
            assert!(error.to_string().contains("stderr:"));
            assert_eq!(get_commit_hash(&worktree.path, "HEAD").await?, head);
            assert_eq!(
                std::fs::read_to_string(worktree.path.join("value.txt"))?,
                "base\n"
            );
            let am_state =
                run_test_git(&worktree.path, &["rev-parse", "--git-path", "rebase-apply"]).await?;
            assert!(!worktree.path.join(am_state).exists());
        }
        worktree.remove().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_apply_patch_falls_back_to_three_way_merge() -> Result<()> {
        let repo = tempfile::tempdir()?;
        run_test_git(repo.path(), &["init", "-q", "-b", "main"]).await?;
        run_test_git(repo.path(), &["config", "user.name", "Test User"]).await?;
        run_test_git(repo.path(), &["config", "user.email", "test@example.com"]).await?;

        std::fs::write(
            repo.path().join("value.txt"),
            "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n",
        )?;
        run_test_git(repo.path(), &["add", "value.txt"]).await?;
        run_test_git(repo.path(), &["commit", "-q", "-s", "-m", "base"]).await?;
        let base_sha = get_commit_hash(repo.path(), "HEAD").await?;

        std::fs::write(
            repo.path().join("value.txt"),
            "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8_modified\n",
        )?;
        run_test_git(repo.path(), &["commit", "-q", "-a", "-s", "-m", "patch"]).await?;
        let mbox = run_test_git(repo.path(), &["format-patch", "--stdout", "-1", "HEAD"]).await?;

        run_test_git(repo.path(), &["reset", "--hard", &base_sha]).await?;
        std::fs::write(
            repo.path().join("value.txt"),
            "prepended\nline1\nline2\nline3\nline4\nline5\nline6_changed_context\nline7\nline8\n",
        )?;
        run_test_git(
            repo.path(),
            &["commit", "-q", "-a", "-s", "-m", "advance main"],
        )
        .await?;
        let head = get_commit_hash(repo.path(), "HEAD").await?;

        let worktree = GitWorktree::new(repo.path(), &head, None).await?;
        worktree.apply_patch(&mbox).await?;
        let applied_head = get_commit_hash(&worktree.path, "HEAD").await?;
        assert_ne!(applied_head, head);
        assert_eq!(
            std::fs::read_to_string(worktree.path.join("value.txt"))?,
            "prepended\nline1\nline2\nline3\nline4\nline5\nline6_changed_context\nline7\nline8_modified\n"
        );

        let noop_err = worktree.apply_patch(&mbox).await.unwrap_err();
        assert_eq!(
            PatchApplyFailure::from_error(&noop_err),
            PatchApplyFailure::ContextMismatch
        );
        assert_eq!(get_commit_hash(&worktree.path, "HEAD").await?, applied_head);
        worktree.remove().await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_merge_and_empty_commit_detection() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        // Init git repo
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["init"])
            .output()
            .await?;
        let _ = crate::git_cmd::in_dir_async(&repo_path)
            .args(["branch", "-m", "master"])
            .output()
            .await;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;

        // 1. Initial commit
        let file_path = repo_path.join("test.txt");
        {
            let mut file = File::create(&file_path)?;
            writeln!(file, "Initial content")?;
        }
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-m", "Initial commit"])
            .output()
            .await?;

        let initial_hash = get_commit_hash(&repo_path, "HEAD").await?;

        // 2. Create a branch and add a commit
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["checkout", "-b", "feature"])
            .output()
            .await?;
        {
            let mut file = std::fs::OpenOptions::new().append(true).open(&file_path)?;
            writeln!(file, "Feature content")?;
        }
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-am", "Feature commit"])
            .output()
            .await?;
        let feature_hash = get_commit_hash(&repo_path, "HEAD").await?;

        // 3. Back to master and add a commit
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["checkout", "master"])
            .output()
            .await?;
        let other_file = repo_path.join("other.txt");
        {
            let mut file = File::create(&other_file)?;
            writeln!(file, "Other content")?;
        }
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-m", "Master commit"])
            .output()
            .await?;
        let master_hash = get_commit_hash(&repo_path, "HEAD").await?;

        // 4. Merge feature into master
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["merge", "feature", "--no-ff", "-m", "Merge commit"])
            .output()
            .await?;
        let merge_hash = get_commit_hash(&repo_path, "HEAD").await?;

        // 5. Create an empty commit
        crate::git_cmd::in_dir_async(&repo_path)
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
    async fn test_ensure_remote_protocol_security() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo_path = dir.path().to_path_buf();

        // Init local repo
        crate::git_cmd::in_dir_async(&repo_path)
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

        // The refused fetch has no local ref to fall back on, so it
        // surfaces as an error.  The guarantee under test is that the
        // command in the ext:: URL never runs.
        let res = ensure_remote(&repo_path, "malicious", &url, true).await;
        assert!(res.is_err());

        assert!(
            !proof_file.exists(),
            "Command should NOT have been executed!"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_remote_failure_backoff() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo_path = dir.path().to_path_buf();

        // Init local repo
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["init"])
            .output()
            .await?;

        let url = "https://127.0.0.1:9/invalid-repo.git";

        // Initial fetch should attempt and fail
        let res1 = ensure_remote(&repo_path, "invalid", url, false).await;
        assert!(res1.is_err());

        let fail_file = repo_path.join(".sashiko/fetch_timestamps/invalid.fail");
        assert!(fail_file.exists(), "Failure timestamp file should exist");

        // Subsequent non-forced fetch backs off, and with no local
        // ref to fall back on that is an error
        let res2 = ensure_remote(&repo_path, "invalid", url, false).await;
        assert!(res2.is_err());
        assert!(res2.unwrap_err().to_string().contains("backing off"));

        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_remote_syncs_local_master_branch() -> Result<()> {
        let remote_dir = tempfile::tempdir()?;
        let local_dir = tempfile::tempdir()?;

        // Init upstream remote repo with an initial commit on master
        crate::git_cmd::in_dir_async(remote_dir.path())
            .args(["init", "-b", "master"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(remote_dir.path())
            .args(["config", "user.name", "Test"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(remote_dir.path())
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;
        std::fs::write(remote_dir.path().join("file.txt"), "v1")?;
        crate::git_cmd::in_dir_async(remote_dir.path())
            .args(["add", "file.txt"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(remote_dir.path())
            .args(["commit", "-m", "commit 1"])
            .output()
            .await?;

        // Clone local repo from remote
        crate::git_cmd::detached_async()
            .args([
                "clone",
                remote_dir.path().to_str().unwrap(),
                local_dir.path().to_str().unwrap(),
            ])
            .output()
            .await?;

        // Add a second commit to upstream
        std::fs::write(remote_dir.path().join("file.txt"), "v2")?;
        crate::git_cmd::in_dir_async(remote_dir.path())
            .args(["commit", "-am", "commit 2"])
            .output()
            .await?;
        let upstream_sha = String::from_utf8_lossy(
            &crate::git_cmd::in_dir_async(remote_dir.path())
                .args(["rev-parse", "master"])
                .output()
                .await?
                .stdout,
        )
        .trim()
        .to_string();

        // Run ensure_remote on local repo for 'origin'
        ensure_remote(
            local_dir.path(),
            "origin",
            remote_dir.path().to_str().unwrap(),
            true,
        )
        .await?;

        // Verify local branch 'master' was updated to upstream_sha
        let local_master_sha = String::from_utf8_lossy(
            &crate::git_cmd::in_dir_async(local_dir.path())
                .args(["rev-parse", "refs/heads/master"])
                .output()
                .await?
                .stdout,
        )
        .trim()
        .to_string();
        assert_eq!(local_master_sha, upstream_sha);

        Ok(())
    }

    #[tokio::test]
    async fn test_git_find_file_rename() -> Result<()> {
        let repo_dir = TempDir::new()?;
        crate::git_cmd::in_dir_async(repo_dir.path())
            .args(["init"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(repo_dir.path())
            .args(["config", "user.name", "Test"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(repo_dir.path())
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;

        // 1. Commit initial file
        std::fs::write(
            repo_dir.path().join("old_driver.c"),
            "int old_fn() { return 0; }\n",
        )?;
        crate::git_cmd::in_dir_async(repo_dir.path())
            .args(["add", "old_driver.c"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(repo_dir.path())
            .args(["commit", "-m", "add old_driver.c"])
            .output()
            .await?;

        // 2. Rename file via git mv and commit
        crate::git_cmd::in_dir_async(repo_dir.path())
            .args(["mv", "old_driver.c", "new_driver.c"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(repo_dir.path())
            .args(["commit", "-m", "rename to new_driver.c"])
            .output()
            .await?;

        // 3. Verify git_find_file_rename traces old_driver.c to new_driver.c
        let resolved = git_find_file_rename(repo_dir.path(), "old_driver.c", None).await;
        assert_eq!(resolved, Some("new_driver.c".to_string()));

        // 4. Non-existent file returns None
        let nonexistent = git_find_file_rename(repo_dir.path(), "does_not_exist.c", None).await;
        assert_eq!(nonexistent, None);

        // 5. Dash-prefixed flag injection attempt is rejected and creates no file
        let injected_file = repo_dir.path().join("pwned.txt");
        let flag_arg = format!("--output={}", injected_file.display());
        let injected = git_find_file_rename(repo_dir.path(), &flag_arg, None).await;
        assert_eq!(injected, None);
        assert!(!injected_file.exists());

        Ok(())
    }

    /// Ages a fetch timestamp so the next refresh_remote() sees the
    /// interval as elapsed or not.
    fn set_fetch_age(repo_path: &Path, remote: &str, age: std::time::Duration) -> Result<()> {
        let dir = repo_path.join(".sashiko/fetch_timestamps");
        std::fs::create_dir_all(&dir)?;
        let file = std::fs::File::create(dir.join(remote))?;
        file.set_modified(std::time::SystemTime::now() - age)?;
        Ok(())
    }

    #[tokio::test]
    async fn test_refresh_remote_reports_local_ref_outcomes() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo_path = dir.path().to_path_buf();

        crate::git_cmd::in_dir_async(&repo_path)
            .args(["init"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "--allow-empty", "-m", "base"])
            .output()
            .await?;

        // Stand in for a previous successful fetch: the remote's HEAD
        // exists locally and a timestamp records when it was written.
        let remote = "unreachable";
        let url = "https://127.0.0.1:9/invalid-repo.git";
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["remote", "add", remote, url])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args([
                "update-ref",
                &format!("refs/remotes/{}/HEAD", remote),
                "HEAD",
            ])
            .output()
            .await?;

        // Within the interval: nothing goes out.
        set_fetch_age(&repo_path, remote, std::time::Duration::from_secs(60))?;
        let res = refresh_remote(&repo_path, remote, url, false).await?;
        assert_eq!(res, FetchOutcome::Skipped);

        // Past the interval but within the fallback window: the fetch
        // fails and the local ref is what remains.
        set_fetch_age(&repo_path, remote, std::time::Duration::from_secs(2 * 3600))?;
        let res = refresh_remote(&repo_path, remote, url, false).await?;
        assert!(
            matches!(res, FetchOutcome::StaleLocalRef(ref msg) if !msg.is_empty()),
            "expected StaleLocalRef, got {:?}",
            res
        );

        // The failure is now backed off.
        let res = refresh_remote(&repo_path, remote, url, false).await?;
        assert_eq!(res, FetchOutcome::BackedOff);

        // Backed off, but the local ref is past the fallback window.
        set_fetch_age(
            &repo_path,
            remote,
            std::time::Duration::from_secs(30 * 3600),
        )?;
        let err = refresh_remote(&repo_path, remote, url, false)
            .await
            .expect_err("expected a backoff error");
        assert!(
            err.downcast_ref::<BackoffError>().is_some(),
            "expected BackoffError, got {:?}",
            err
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_remote_redacts_stderr_on_failure() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let repo_path = dir.path().to_path_buf();

        crate::git_cmd::in_dir_async(&repo_path)
            .args(["init"])
            .output()
            .await?;

        crate::git_cmd::in_dir_async(&repo_path)
            .args(["remote", "add", "existing", "https://example.com/old.git"])
            .output()
            .await?;

        // Lock .git/config so git remote add and git remote set-url fail and echo the target URL
        File::create(repo_path.join(".git/config.lock"))?;

        let cred_url = "https://user:secret_token_123@example.com/repo.git";

        // 1. git remote add failure when .git/config.lock exists
        let add_err = ensure_remote(&repo_path, "new_remote", cred_url, false)
            .await
            .expect_err("expected remote add to fail when config.lock exists")
            .to_string();
        assert!(
            add_err.contains("Failed to add remote:"),
            "unexpected error: {add_err}"
        );
        assert!(
            add_err.contains("[REDACTED]"),
            "expected redacted credentials in error: {add_err}"
        );
        assert!(
            !add_err.contains("secret_token_123"),
            "leaked secret token in error: {add_err}"
        );

        // 2. git remote set-url failure when .git/config.lock exists
        let set_url_err = ensure_remote(&repo_path, "existing", cred_url, false)
            .await
            .expect_err("expected remote set-url to fail when config.lock exists")
            .to_string();
        assert!(
            set_url_err.contains("Failed to update remote URL:"),
            "unexpected error: {set_url_err}"
        );
        assert!(
            set_url_err.contains("[REDACTED]"),
            "expected redacted credentials in error: {set_url_err}"
        );
        assert!(
            !set_url_err.contains("secret_token_123"),
            "leaked secret token in error: {set_url_err}"
        );

        Ok(())
    }

    #[test]
    fn test_is_valid_refspec_pattern() {
        // Valid patterns (must contain exactly one '*')
        assert!(is_valid_refspec_pattern("linux-*.y"));
        assert!(is_valid_refspec_pattern("v6.18/*"));
        assert!(is_valid_refspec_pattern("*-next"));
        assert!(is_valid_refspec_pattern("net-*"));

        // Invalid patterns: no asterisk (exact branch names belong in only_branches)
        assert!(!is_valid_refspec_pattern("main"));
        assert!(!is_valid_refspec_pattern("for-next"));

        // Invalid patterns: multiple asterisks
        assert!(!is_valid_refspec_pattern("linux-*-*.y"));
        assert!(!is_valid_refspec_pattern("*/*"));

        // Invalid patterns: question mark
        assert!(!is_valid_refspec_pattern("feat-?"));

        // Invalid patterns: spaces and invalid git ref characters
        assert!(!is_valid_refspec_pattern("feat branch*"));
        assert!(!is_valid_refspec_pattern("feat..branch*"));
        assert!(!is_valid_refspec_pattern("feat~1*"));
        assert!(!is_valid_refspec_pattern("feat:branch*"));
        assert!(!is_valid_refspec_pattern("/start-slash*"));
        assert!(!is_valid_refspec_pattern("end-slash/*/"));
        assert!(!is_valid_refspec_pattern("branch.lock*"));
    }

    #[test]
    fn test_is_valid_branch_name() {
        // Valid literal branch names
        assert!(is_valid_branch_name("main"));
        assert!(is_valid_branch_name("master"));
        assert!(is_valid_branch_name("for-next"));
        assert!(is_valid_branch_name("v6.18/dev"));

        // Invalid branch names: wildcards
        assert!(!is_valid_branch_name("linux-*.y"));
        assert!(!is_valid_branch_name("feat-?"));

        // Invalid branch names: spaces and invalid git ref characters
        assert!(!is_valid_branch_name("feat branch"));
        assert!(!is_valid_branch_name("feat..branch"));
        assert!(!is_valid_branch_name("feat~1"));
        assert!(!is_valid_branch_name("feat:branch"));
        assert!(!is_valid_branch_name("/start-slash"));
        assert!(!is_valid_branch_name("end-slash/"));
        assert!(!is_valid_branch_name("branch.lock"));
    }

    #[test]
    fn test_matches_branch_pattern() {
        let patterns = vec![
            "main".to_string(),
            "linux-*.y".to_string(),
            "v6.18/*".to_string(),
            "*-next".to_string(),
        ];

        // Exact match
        assert!(matches_branch_pattern("main", &patterns));
        assert!(!matches_branch_pattern("master", &patterns));

        // Wildcard prefix/suffix
        assert!(matches_branch_pattern("linux-6.12.y", &patterns));
        assert!(matches_branch_pattern("linux-6.6.y", &patterns));
        assert!(!matches_branch_pattern("linux-6.12", &patterns));

        // Prefix wildcard
        assert!(matches_branch_pattern("v6.18/dev", &patterns));
        assert!(matches_branch_pattern("v6.18/prod", &patterns));
        assert!(!matches_branch_pattern("v6.19/dev", &patterns));

        // Suffix wildcard
        assert!(matches_branch_pattern("net-next", &patterns));
        assert!(matches_branch_pattern("bpf-next", &patterns));
        assert!(!matches_branch_pattern("net-fixes", &patterns));
    }

    #[tokio::test]
    async fn test_ensure_remote_with_refspecs_limits_fetch() -> Result<()> {
        let source_dir = tempfile::tempdir()?;
        let source_path = source_dir.path();

        crate::git_cmd::in_dir(source_path)
            .args(["-c", "safe.bareRepository=all", "init", "-b", "main"])
            .output()?;
        crate::git_cmd::in_dir(source_path)
            .args(["config", "user.name", "Test"])
            .output()?;
        crate::git_cmd::in_dir(source_path)
            .args(["config", "user.email", "test@example.com"])
            .output()?;
        std::fs::write(source_path.join("file.txt"), "hello")?;
        crate::git_cmd::in_dir(source_path)
            .args(["add", "file.txt"])
            .output()?;
        crate::git_cmd::in_dir(source_path)
            .args(["commit", "-m", "init"])
            .output()?;

        for b in &["v6.18/dev", "v6.18/prod", "other/feature"] {
            crate::git_cmd::in_dir(source_path)
                .args(["branch", b])
                .output()?;
        }

        let consumer_dir = tempfile::tempdir()?;
        let consumer_path = consumer_dir.path();
        crate::git_cmd::in_dir(consumer_path)
            .args(["-c", "safe.bareRepository=all", "init", "-b", "main"])
            .output()?;

        let source_url = source_path.to_str().unwrap();
        let refspecs = vec!["+refs/heads/v6.18/*:refs/remotes/test_remote/v6.18/*".to_string()];

        ensure_remote_with_refspecs(
            consumer_path,
            "test_remote",
            source_url,
            true,
            Some(&refspecs),
        )
        .await?;

        let branches = get_remote_branches(consumer_path, "test_remote").await?;
        assert!(branches.contains(&"v6.18/dev".to_string()));
        assert!(branches.contains(&"v6.18/prod".to_string()));
        assert!(!branches.contains(&"other/feature".to_string()));
        assert!(!branches.contains(&"main".to_string()));

        let config_out = crate::git_cmd::in_dir(consumer_path)
            .args(["config", "--get-all", "remote.test_remote.fetch"])
            .output()?;
        let config_str = String::from_utf8_lossy(&config_out.stdout);
        assert!(config_str.contains("+refs/heads/v6.18/*:refs/remotes/test_remote/v6.18/*"));

        // Updating refspecs forces a fetch and updates .git/config
        let updated_refspecs =
            vec!["+refs/heads/other/*:refs/remotes/test_remote/other/*".to_string()];
        ensure_remote_with_refspecs(
            consumer_path,
            "test_remote",
            source_url,
            false,
            Some(&updated_refspecs),
        )
        .await?;

        let branches = get_remote_branches(consumer_path, "test_remote").await?;
        assert!(branches.contains(&"other/feature".to_string()));

        let config_out = crate::git_cmd::in_dir(consumer_path)
            .args(["config", "--get-all", "remote.test_remote.fetch"])
            .output()?;
        let config_str = String::from_utf8_lossy(&config_out.stdout);
        assert!(config_str.contains("+refs/heads/other/*:refs/remotes/test_remote/other/*"));
        assert!(!config_str.contains("+refs/heads/v6.18/*"));

        // Calling with refspecs = None preserves existing selective refspecs
        ensure_remote_with_refspecs(consumer_path, "test_remote", source_url, false, None).await?;
        let config_out = crate::git_cmd::in_dir(consumer_path)
            .args(["config", "--get-all", "remote.test_remote.fetch"])
            .output()?;
        let config_str = String::from_utf8_lossy(&config_out.stdout);
        assert!(config_str.contains("+refs/heads/other/*:refs/remotes/test_remote/other/*"));

        // Setting refspecs to Some(&[]) restores default wildcard refspec
        ensure_remote_with_refspecs(consumer_path, "test_remote", source_url, false, Some(&[]))
            .await?;

        let config_out = crate::git_cmd::in_dir(consumer_path)
            .args(["config", "--get-all", "remote.test_remote.fetch"])
            .output()?;
        let config_str = String::from_utf8_lossy(&config_out.stdout);
        assert_eq!(
            config_str.trim(),
            "+refs/heads/*:refs/remotes/test_remote/*"
        );

        let branches = get_remote_branches(consumer_path, "test_remote").await?;
        assert!(branches.contains(&"main".to_string()));
        assert!(branches.contains(&"v6.18/dev".to_string()));
        assert!(branches.contains(&"other/feature".to_string()));

        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_remote_if_missing_skips_fetch_when_refs_exist() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path();

        crate::git_cmd::in_dir(repo_path)
            .args(["-c", "safe.bareRepository=all", "init", "-b", "main"])
            .output()?;
        crate::git_cmd::in_dir(repo_path)
            .args(["config", "user.name", "Test"])
            .output()?;
        crate::git_cmd::in_dir(repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()?;
        std::fs::write(repo_path.join("file.txt"), "hello")?;
        crate::git_cmd::in_dir(repo_path)
            .args(["add", "file.txt"])
            .output()?;
        crate::git_cmd::in_dir(repo_path)
            .args(["commit", "-m", "init"])
            .output()?;

        // Create a local remote ref
        let remote_ref = "refs/remotes/myremote/main";
        crate::git_cmd::in_dir(repo_path)
            .args(["update-ref", remote_ref, "HEAD"])
            .output()?;
        crate::git_cmd::in_dir(repo_path)
            .args([
                "remote",
                "add",
                "myremote",
                "https://invalid.domain.test/repo.git",
            ])
            .output()?;

        // ensure_remote_if_missing should succeed without network I/O because local ref exists
        let res = ensure_remote_if_missing(
            repo_path,
            "myremote",
            "https://invalid.domain.test/repo.git",
            None,
        )
        .await;
        assert!(res.is_ok());

        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_remote_if_missing_succeeds_when_backed_off_with_local_refs() -> Result<()>
    {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path();

        crate::git_cmd::in_dir(repo_path)
            .args(["-c", "safe.bareRepository=all", "init", "-b", "main"])
            .output()?;
        crate::git_cmd::in_dir(repo_path)
            .args(["config", "user.name", "Test"])
            .output()?;
        crate::git_cmd::in_dir(repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()?;
        std::fs::write(repo_path.join("file.txt"), "hello")?;
        crate::git_cmd::in_dir(repo_path)
            .args(["add", "file.txt"])
            .output()?;
        crate::git_cmd::in_dir(repo_path)
            .args(["commit", "-m", "init"])
            .output()?;

        // Create local remote ref
        let remote_ref = "refs/remotes/backed_off_remote/main";
        crate::git_cmd::in_dir(repo_path)
            .args(["update-ref", remote_ref, "HEAD"])
            .output()?;
        crate::git_cmd::in_dir(repo_path)
            .args([
                "remote",
                "add",
                "backed_off_remote",
                "https://invalid.domain.test/repo.git",
            ])
            .output()?;

        // Create a recent failure timestamp file to simulate a background sync failure
        let timestamp_dir = repo_path.join(".sashiko/fetch_timestamps");
        std::fs::create_dir_all(&timestamp_dir)?;
        std::fs::write(timestamp_dir.join("backed_off_remote.fail"), "failed")?;

        // ensure_remote_if_missing must skip fetch and succeed because local refs exist
        let res = ensure_remote_if_missing(
            repo_path,
            "backed_off_remote",
            "https://invalid.domain.test/repo.git",
            None,
        )
        .await;
        assert!(
            res.is_ok(),
            "expected success when local refs exist despite recent failure backoff"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_remote_url_change_clears_old_refs_and_timestamps() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path();

        crate::git_cmd::in_dir(repo_path)
            .args(["-c", "safe.bareRepository=all", "init", "-b", "main"])
            .output()?;
        crate::git_cmd::in_dir(repo_path)
            .args(["config", "user.name", "Test"])
            .output()?;
        crate::git_cmd::in_dir(repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()?;
        std::fs::write(repo_path.join("file.txt"), "hello")?;
        crate::git_cmd::in_dir(repo_path)
            .args(["add", "file.txt"])
            .output()?;
        crate::git_cmd::in_dir(repo_path)
            .args(["commit", "-m", "init"])
            .output()?;

        // Create local remote ref and add remote
        crate::git_cmd::in_dir(repo_path)
            .args([
                "remote",
                "add",
                "changing_remote",
                "https://old.domain.test/repo.git",
            ])
            .output()?;
        let remote_ref = "refs/remotes/changing_remote/old_branch";
        crate::git_cmd::in_dir(repo_path)
            .args(["update-ref", remote_ref, "HEAD"])
            .output()?;

        // Create timestamp files
        let timestamp_dir = repo_path.join(".sashiko/fetch_timestamps");
        std::fs::create_dir_all(&timestamp_dir)?;
        let ts_file = timestamp_dir.join("changing_remote");
        let fail_file = timestamp_dir.join("changing_remote.fail");
        std::fs::write(&ts_file, "ok")?;
        std::fs::write(&fail_file, "fail")?;

        // Create a second local temporary repository to serve as the new URL
        let temp_dir2 = tempfile::tempdir()?;
        let repo2_path = temp_dir2.path();
        crate::git_cmd::in_dir(repo2_path)
            .args(["-c", "safe.bareRepository=all", "init", "-b", "main"])
            .output()?;
        crate::git_cmd::in_dir(repo2_path)
            .args(["config", "user.name", "Test 2"])
            .output()?;
        crate::git_cmd::in_dir(repo2_path)
            .args(["config", "user.email", "test2@example.com"])
            .output()?;
        std::fs::write(repo2_path.join("file2.txt"), "hello2")?;
        crate::git_cmd::in_dir(repo2_path)
            .args(["add", "file2.txt"])
            .output()?;
        crate::git_cmd::in_dir(repo2_path)
            .args(["commit", "-m", "init 2"])
            .output()?;

        let new_url = repo2_path.to_str().unwrap();

        // Calling refresh_remote_with_policy with a new URL
        let _ = refresh_remote_with_policy(
            repo_path,
            "changing_remote",
            new_url,
            RemoteFetchPolicy::IfMissing,
            None,
        )
        .await;

        // Verify that old ref from the previous URL was cleared
        let old_ref_out = crate::git_cmd::in_dir(repo_path)
            .args([
                "rev-parse",
                "--verify",
                "refs/remotes/changing_remote/old_branch",
            ])
            .output()?;
        assert!(!old_ref_out.status.success());

        // Verify that the new URL's ref was fetched
        let new_ref_out = crate::git_cmd::in_dir(repo_path)
            .args(["rev-parse", "--verify", "refs/remotes/changing_remote/main"])
            .output()?;
        assert!(new_ref_out.status.success());

        // Verify that old timestamp and fail files were cleared
        assert!(!fail_file.exists());

        Ok(())
    }

    #[tokio::test]
    async fn test_origin_url_change_preserves_tracking_refs_on_failure() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path();

        crate::git_cmd::in_dir(repo_path)
            .args(["-c", "safe.bareRepository=all", "init", "-b", "main"])
            .output()?;
        crate::git_cmd::in_dir(repo_path)
            .args(["config", "user.name", "Test"])
            .output()?;
        crate::git_cmd::in_dir(repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()?;
        std::fs::write(repo_path.join("file.txt"), "hello")?;
        crate::git_cmd::in_dir(repo_path)
            .args(["add", "file.txt"])
            .output()?;
        crate::git_cmd::in_dir(repo_path)
            .args(["commit", "-m", "init"])
            .output()?;

        // Simulate a mirror clone of origin
        crate::git_cmd::in_dir(repo_path)
            .args([
                "remote",
                "add",
                "origin",
                "https://mirror.domain.test/gcc.git",
            ])
            .output()?;
        let origin_ref = "refs/remotes/origin/master";
        crate::git_cmd::in_dir(repo_path)
            .args(["update-ref", origin_ref, "HEAD"])
            .output()?;

        // Repoint origin to an unreachable upstream URL with force_fetch
        let upstream_url = "https://unreachable.domain.invalid/gcc.git";
        let res = refresh_remote_with_policy(
            repo_path,
            "origin",
            upstream_url,
            RemoteFetchPolicy::Force,
            None,
        )
        .await;

        // Force fetch from unreachable URL returns StaleLocalRef due to can_fallback
        assert!(matches!(res, Ok(FetchOutcome::StaleLocalRef(_))));

        // Mirror tracking refs should NOT have been deleted
        let ref_check = crate::git_cmd::in_dir(repo_path)
            .args(["rev-parse", "--verify", origin_ref])
            .output()?;
        assert!(
            ref_check.status.success(),
            "origin tracking ref must be preserved when repointing from seed mirror"
        );

        // ensure_remote_if_missing should succeed using preserved refs
        let ensure_res = ensure_remote_if_missing(repo_path, "origin", upstream_url, None).await;
        assert!(
            ensure_res.is_ok(),
            "ensure_remote_if_missing must succeed when origin refs are preserved"
        );

        Ok(())
    }

    #[test]
    fn test_are_git_urls_equivalent() {
        assert!(are_git_urls_equivalent(
            "git://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git",
            "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git"
        ));
        assert!(are_git_urls_equivalent(
            "https://github.com/torvalds/linux.git",
            "https://github.com/torvalds/linux"
        ));
        assert!(are_git_urls_equivalent(
            "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git",
            "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux/"
        ));
        assert!(!are_git_urls_equivalent(
            "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git",
            "https://git.kernel.org/pub/scm/linux/kernel/git/netdev/net-next.git"
        ));
    }

    #[tokio::test]
    async fn test_repack_repository_writes_commit_graph_with_bloom_filters() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        crate::git_cmd::in_dir_async(&repo_path)
            .args(["init"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;

        std::fs::write(repo_path.join("a.c"), "int a(void) { return 1; }\n")?;
        let add_out = crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "a.c"])
            .output()
            .await?;
        assert!(add_out.status.success());
        let commit_out = crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "--no-gpg-sign", "-m", "initial"])
            .output()
            .await?;
        assert!(
            commit_out.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&commit_out.stderr)
        );

        repack_repository(&repo_path).await?;

        let info_dir = object_dir(&repo_path, "info").await?;
        let single_graph = info_dir.join("commit-graph");
        let graph_bytes = if single_graph.exists() {
            std::fs::read(&single_graph)?
        } else {
            let split_dir = info_dir.join("commit-graphs");
            let mut found = None;
            for entry in std::fs::read_dir(&split_dir)? {
                let p = entry?.path();
                if p.extension().is_some_and(|e| e == "graph") {
                    found = Some(std::fs::read(&p)?);
                    break;
                }
            }
            found.expect("expected single or split commit-graph file")
        };
        // Git commit-graph format stores 4-byte chunk IDs BIDX and BDAT when
        // changed-path Bloom filters are written.
        assert!(
            graph_bytes.windows(4).any(|w| w == b"BIDX")
                && graph_bytes.windows(4).any(|w| w == b"BDAT"),
            "expected BIDX/BDAT changed-path Bloom filter chunks in commit-graph"
        );

        let verify = crate::git_cmd::in_dir_async(&repo_path)
            .args(["-c", "core.commitGraph=true", "commit-graph", "verify"])
            .output()
            .await?;
        assert!(
            verify.status.success(),
            "git commit-graph verify failed: {}",
            String::from_utf8_lossy(&verify.stderr)
        );
        Ok(())
    }
}
