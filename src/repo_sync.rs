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

//! Provisioning of the tree a daemon reviews (`sashiko repo sync`).
//!
//! A daemon reads its project's tree from `[git] repository_path` and never
//! clones it. This module is the one place that does: it clones the project's
//! upstream into an absent or empty directory, starting from the project's
//! seed when it has one, and fetches the upstream into a tree that already
//! exists. Every way of running a daemon (a build from source, the container
//! image, a deployment) provisions its tree through here, so that the
//! per-project facts live in [`ProjectId::upstream`] rather than in a shell
//! script per deployment.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Result, anyhow, bail};
use tracing::{info, warn};

use crate::git_ops::{FetchOutcome, GIT_PROTOCOL_RESTRICTIONS, refresh_remote};
use crate::project::{ProjectId, Seed, Upstream};

/// The remote a provisioned tree fetches its upstream through.
const ORIGIN: &str = "origin";

/// What a sync did to the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// The tree was missing and has been cloned. `seeded` says whether the
    /// clone was started from the project's seed rather than the upstream
    /// alone. A bundle git cannot download is skipped by git itself, with a
    /// warning of its own, so a seeded clone may still have taken everything
    /// from the upstream.
    Cloned { seeded: bool },
    /// The tree existed and its upstream has been fetched.
    Fetched,
    /// The tree existed but fetching its upstream failed. The tree is left as
    /// it was, which is still a tree a daemon can review against.
    FetchFailed(String),
}

impl std::fmt::Display for SyncOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncOutcome::Cloned { seeded: true } => f.write_str("cloned from its seed"),
            SyncOutcome::Cloned { seeded: false } => f.write_str("cloned"),
            SyncOutcome::Fetched => f.write_str("fetched"),
            SyncOutcome::FetchFailed(reason) => {
                write!(f, "fetch failed, existing tree kept: {reason}")
            }
        }
    }
}

/// What is at the path a tree is to be provisioned at.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TreeState {
    /// Nothing, or an empty directory: a clone can go straight there.
    Vacant,
    /// A git repository, working tree or bare, whose git directory is this.
    Repository(PathBuf),
    /// A directory holding nothing but a `.git` directory that has no refs,
    /// which is what a clone killed before it finished leaves behind.
    InterruptedClone,
    /// Anything else: not ours to clone over or delete.
    Occupied,
}

/// The directory `sashiko repo sync` provisions when no `--path` is given:
/// the daemon's own `[git] repository_path`, so that an entrypoint can sync
/// and then start the daemon without naming the path twice, else the
/// project's directory under `third_party/`.
pub fn resolve_path(
    explicit: Option<&Path>,
    configured: Option<&str>,
    project: ProjectId,
) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    match configured.map(str::trim).filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => Path::new("third_party").join(project.as_str()),
    }
}

/// Clones `upstream` into `path` if no tree is there, otherwise fetches it
/// into the tree that is.
pub async fn sync(path: &Path, upstream: Upstream) -> Result<SyncOutcome> {
    match inspect(path).await? {
        TreeState::Repository(_) => Ok(refresh_existing(path, upstream).await),
        TreeState::Vacant => clone(path, upstream).await,
        TreeState::InterruptedClone => {
            warn!(
                "{} holds an interrupted clone; removing it and cloning again",
                path.display()
            );
            remove_interrupted_clone(path)?;
            clone(path, upstream).await
        }
        TreeState::Occupied => bail!(
            "{} exists and is not a git repository; move it away or sync a different --path",
            path.display()
        ),
    }
}

/// Checks that `path` holds a tree a daemon reviewing `project` can start
/// against, and says how to get one when it does not.
///
/// An `origin` that does not point at the project's upstream is only
/// reported: a fork or a mirror is a legitimate tree to review against, but a
/// daemon pointed at another project's tree is worth a line in the log.
pub async fn check_daemon_tree(path: &Path, project: ProjectId) -> Result<(), String> {
    let state = inspect(path).await.map_err(|e| e.to_string())?;
    if !matches!(state, TreeState::Repository(_)) {
        return Err(format!(
            "[git] repository_path {:?} is not a git repository; run `sashiko repo sync` \
             to clone the {project} tree there",
            path.display().to_string()
        ));
    }
    let expected = project.upstream().url;
    match origin_url(path).await {
        Some(url) if url != expected => warn!(
            "The {project} tree at {} fetches origin from {}, not from {}",
            path.display(),
            crate::utils::redact_secret(&url),
            expected
        ),
        Some(_) => {}
        None => warn!(
            "The {project} tree at {} has no origin remote",
            path.display()
        ),
    }
    Ok(())
}

/// Classifies what is at `path`.
///
/// Git is always pointed at the candidate git directory explicitly. Run in a
/// directory that is not a repository, git searches the parent directories,
/// and a tree under `third_party/` of a Sashiko checkout would be mistaken
/// for the checkout itself.
async fn inspect(path: &Path) -> Result<TreeState> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(TreeState::Vacant),
        Err(e) => return Err(anyhow!("cannot inspect {}: {e}", path.display())),
    };
    // A symlink to a directory is followed, as git and the daemon follow it.
    if !metadata.is_dir() && !path.is_dir() {
        return Ok(TreeState::Occupied);
    }

    let entries = std::fs::read_dir(path)
        .map_err(|e| anyhow!("cannot read {}: {e}", path.display()))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| anyhow!("cannot read {}: {e}", path.display()))?;
    if entries.is_empty() {
        return Ok(TreeState::Vacant);
    }

    let dot_git = path.join(".git");
    let candidate = if dot_git.exists() {
        dot_git
    } else {
        // A bare repository is its own git directory.
        path.to_path_buf()
    };
    let valid = git_dir_is_valid(&candidate).await;
    if valid && has_refs(&candidate).await? {
        return Ok(TreeState::Repository(candidate));
    }

    let only_dot_git = entries.len() == 1 && entries[0] == ".git" && candidate.is_dir();
    if only_dot_git {
        return Ok(TreeState::InterruptedClone);
    }
    if valid {
        // A repository without refs yet, such as a fresh `git init` with files
        // next to it: fetching into it is how it gets some.
        return Ok(TreeState::Repository(candidate));
    }
    Ok(TreeState::Occupied)
}

/// Whether git accepts `git_dir` as a git directory, without searching any
/// directory above it.
async fn git_dir_is_valid(git_dir: &Path) -> bool {
    crate::git_cmd::detached_async()
        .arg("--git-dir")
        .arg(git_dir)
        .args(["rev-parse", "--git-dir"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

/// Whether the git directory holds at least one ref.
async fn has_refs(git_dir: &Path) -> Result<bool> {
    let output = crate::git_cmd::detached_async()
        .arg("--git-dir")
        .arg(git_dir)
        .args(["for-each-ref", "--count=1", "--format=%(refname)"])
        .output()
        .await?;
    if !output.status.success() {
        return Ok(false);
    }
    Ok(!output.stdout.trim_ascii().is_empty())
}

/// Removes the `.git` an interrupted clone left. [`inspect`] has established
/// that it is the only entry and holds no refs, so nothing is lost.
fn remove_interrupted_clone(path: &Path) -> Result<()> {
    let dot_git = path.join(".git");
    std::fs::remove_dir_all(&dot_git)
        .map_err(|e| anyhow!("cannot remove interrupted clone {}: {e}", dot_git.display()))
}

/// Fetches the upstream into a tree that already exists.
///
/// `origin` keeps the URL it has: a developer who points `repository_path`
/// at their own clone of a fork must not have it rewritten. Only a missing
/// `origin` is created, pointing at the upstream. A failed fetch leaves the
/// tree as it was, which a daemon can still start against.
async fn refresh_existing(path: &Path, upstream: Upstream) -> SyncOutcome {
    let url = match origin_url(path).await {
        Some(url) => {
            if url != upstream.url {
                warn!(
                    "origin of {} is {}, not the upstream {}; fetching from it unchanged",
                    path.display(),
                    crate::utils::redact_secret(&url),
                    upstream.url
                );
            }
            url
        }
        None => upstream.url.to_string(),
    };
    info!("Fetching origin into {}", path.display());
    fetch_origin(path, &url).await
}

/// Fetches `origin` through the daemon's own fetch path, forced, so the
/// remote's refs, `origin/HEAD` and the fetch timestamps the daemon's sync
/// worker reads all end up as that worker would leave them.
async fn fetch_origin(path: &Path, url: &str) -> SyncOutcome {
    match refresh_remote(path, ORIGIN, url, true).await {
        Ok(FetchOutcome::StaleLocalRef(reason)) => SyncOutcome::FetchFailed(reason),
        Ok(_) => SyncOutcome::Fetched,
        Err(e) => SyncOutcome::FetchFailed(e.to_string()),
    }
}

/// The URL `origin` points at, if the tree has an `origin`.
async fn origin_url(path: &Path) -> Option<String> {
    let output = crate::git_cmd::in_dir_async(path)
        .args(["remote", "get-url", ORIGIN])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!url.is_empty()).then_some(url)
}

/// Clones the upstream into a vacant `path`, starting from its seed.
///
/// A seed only saves time, so a seeded clone that fails is retried from the
/// upstream alone. Once cloned, `origin` is fetched from the upstream: after
/// a mirror that is what brings the tree up to date, and after any clone it
/// records the fetch the daemon would otherwise repeat at once. That fetch
/// failing does not fail the sync, since the clone is already a usable tree.
async fn clone(path: &Path, upstream: Upstream) -> Result<SyncOutcome> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("cannot create {}: {e}", parent.display()))?;
    }

    let seeded_clone = match upstream.seed {
        Seed::None => None,
        Seed::Bundle(bundle) => Some(
            run_clone(path, upstream.url, Some(bundle))
                .await
                .map_err(|e| (bundle, e)),
        ),
        Seed::Mirror(mirror) => Some(run_clone(path, mirror, None).await.map_err(|e| (mirror, e))),
    };
    let seeded = match seeded_clone {
        Some(Ok(())) => true,
        Some(Err((seed, e))) => {
            warn!(
                "Cloning from seed {seed} failed ({e}); cloning {} alone",
                upstream.url
            );
            clear_failed_clone(path).await?;
            run_clone(path, upstream.url, None).await?;
            false
        }
        None => {
            run_clone(path, upstream.url, None).await?;
            false
        }
    };

    // refresh_remote points origin at the upstream, which after a mirror
    // clone is the step that moves it off the mirror.
    if let SyncOutcome::FetchFailed(reason) = fetch_origin(path, upstream.url).await {
        warn!(
            "Fetching {} into the new clone failed ({reason}); the clone is usable as it is",
            upstream.url
        );
    }
    Ok(SyncOutcome::Cloned { seeded })
}

/// Runs one `git clone` of `url` into `path`, starting from `bundle` when
/// given. Git's progress goes to this process's stderr: a first clone of a
/// large tree takes long enough that whoever runs it wants to see it move.
async fn run_clone(path: &Path, url: &str, bundle: Option<&str>) -> Result<()> {
    info!("Cloning {url} into {}", path.display());
    let mut command = crate::git_cmd::detached_async();
    command.args(GIT_PROTOCOL_RESTRICTIONS).arg("clone");
    if let Some(bundle) = bundle {
        command.arg(format!("--bundle-uri={bundle}"));
    }
    let status = command
        .arg("--")
        .arg(url)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .await?;
    if !status.success() {
        bail!("git clone {url} exited with {status}");
    }
    Ok(())
}

/// Makes `path` vacant again after a failed clone. Git removes what it
/// created when a clone fails; this covers a clone that died without
/// cleaning up, and refuses to touch anything it did not leave.
async fn clear_failed_clone(path: &Path) -> Result<()> {
    match inspect(path).await? {
        TreeState::Vacant => Ok(()),
        TreeState::InterruptedClone => remove_interrupted_clone(path),
        TreeState::Repository(_) | TreeState::Occupied => bail!(
            "a failed clone left {} in a state that is not safe to clear",
            path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Runs git in `dir` and returns its trimmed stdout, failing the test on
    /// a non-zero exit.
    async fn git(dir: &Path, args: &[&str]) -> String {
        let output = crate::git_cmd::in_dir_async(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(["-c", "protocol.file.allow=always"])
            .args(args)
            .output()
            .await
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// An upstream repository with one commit on master.
    async fn upstream_repo(root: &Path) -> PathBuf {
        let dir = root.join("upstream");
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "--quiet", "-b", "master"]).await;
        commit(&dir, "initial").await;
        dir
    }

    async fn commit(dir: &Path, message: &str) -> String {
        git(dir, &["commit", "--quiet", "--allow-empty", "-m", message]).await;
        git(dir, &["rev-parse", "HEAD"]).await
    }

    /// An upstream whose strings live as long as the test binary, as the
    /// per-project table's do.
    fn upstream(url: &Path, seed: Seed) -> Upstream {
        Upstream {
            url: Box::leak(url.to_string_lossy().into_owned().into_boxed_str()),
            seed,
        }
    }

    fn leak(path: &Path) -> &'static str {
        Box::leak(path.to_string_lossy().into_owned().into_boxed_str())
    }

    #[test]
    fn test_resolve_path_prefers_flag_then_settings_then_project_dir() {
        let flag = Path::new("/flag");
        assert_eq!(
            resolve_path(Some(flag), Some("/configured"), ProjectId::Linux),
            PathBuf::from("/flag")
        );
        assert_eq!(
            resolve_path(None, Some("/configured"), ProjectId::Linux),
            PathBuf::from("/configured")
        );
        assert_eq!(
            resolve_path(None, Some("  "), ProjectId::Sashiko),
            PathBuf::from("third_party/sashiko")
        );
        assert_eq!(
            resolve_path(None, None, ProjectId::Linux),
            PathBuf::from("third_party/linux")
        );
    }

    #[tokio::test]
    async fn test_sync_clones_into_a_missing_directory() {
        let root = TempDir::new().unwrap();
        let up = upstream_repo(root.path()).await;
        let tip = git(&up, &["rev-parse", "HEAD"]).await;
        let tree = root.path().join("nested/tree");

        let outcome = sync(&tree, upstream(&up, Seed::None)).await.unwrap();

        assert_eq!(outcome, SyncOutcome::Cloned { seeded: false });
        assert_eq!(git(&tree, &["rev-parse", "origin/master"]).await, tip);
        assert_eq!(
            git(&tree, &["remote", "get-url", "origin"]).await,
            up.to_string_lossy()
        );
        // The daemon's sync worker reads this to decide when to fetch next.
        assert!(tree.join(".sashiko/fetch_timestamps/origin").exists());
    }

    #[tokio::test]
    async fn test_sync_clones_into_an_empty_directory() {
        let root = TempDir::new().unwrap();
        let up = upstream_repo(root.path()).await;
        let tree = root.path().join("tree");
        std::fs::create_dir(&tree).unwrap();

        let outcome = sync(&tree, upstream(&up, Seed::None)).await.unwrap();

        assert_eq!(outcome, SyncOutcome::Cloned { seeded: false });
    }

    #[tokio::test]
    async fn test_sync_fetches_into_an_existing_tree() {
        let root = TempDir::new().unwrap();
        let up = upstream_repo(root.path()).await;
        let tree = root.path().join("tree");
        sync(&tree, upstream(&up, Seed::None)).await.unwrap();
        let tip = commit(&up, "second").await;

        let outcome = sync(&tree, upstream(&up, Seed::None)).await.unwrap();

        assert_eq!(outcome, SyncOutcome::Fetched);
        assert_eq!(git(&tree, &["rev-parse", "origin/master"]).await, tip);
    }

    #[tokio::test]
    async fn test_sync_keeps_an_origin_pointing_elsewhere() {
        let root = TempDir::new().unwrap();
        let up = upstream_repo(root.path()).await;
        let fork = root.path().join("fork");
        git(
            root.path(),
            &["clone", "--quiet", &up.to_string_lossy(), "fork"],
        )
        .await;
        let fork_tip = commit(&fork, "fork only").await;
        let tree = root.path().join("tree");
        sync(&tree, upstream(&fork, Seed::None)).await.unwrap();

        let outcome = sync(&tree, upstream(&up, Seed::None)).await.unwrap();

        assert_eq!(outcome, SyncOutcome::Fetched);
        assert_eq!(
            git(&tree, &["remote", "get-url", "origin"]).await,
            fork.to_string_lossy()
        );
        assert_eq!(git(&tree, &["rev-parse", "origin/master"]).await, fork_tip);
    }

    #[tokio::test]
    async fn test_sync_reports_a_failed_fetch_and_keeps_the_tree() {
        let root = TempDir::new().unwrap();
        let up = upstream_repo(root.path()).await;
        let tree = root.path().join("tree");
        sync(&tree, upstream(&up, Seed::None)).await.unwrap();
        std::fs::remove_dir_all(&up).unwrap();

        let outcome = sync(&tree, upstream(&up, Seed::None)).await.unwrap();

        assert!(
            matches!(outcome, SyncOutcome::FetchFailed(_)),
            "{outcome:?}"
        );
        assert!(tree.join(".git").exists());
    }

    #[tokio::test]
    async fn test_sync_moves_a_mirror_clone_onto_the_upstream() {
        let root = TempDir::new().unwrap();
        let up = upstream_repo(root.path()).await;
        git(
            root.path(),
            &[
                "clone",
                "--quiet",
                "--bare",
                &up.to_string_lossy(),
                "mirror",
            ],
        )
        .await;
        let mirror = root.path().join("mirror");
        let tip = commit(&up, "not mirrored yet").await;
        let tree = root.path().join("tree");

        let outcome = sync(&tree, upstream(&up, Seed::Mirror(leak(&mirror))))
            .await
            .unwrap();

        assert_eq!(outcome, SyncOutcome::Cloned { seeded: true });
        assert_eq!(
            git(&tree, &["remote", "get-url", "origin"]).await,
            up.to_string_lossy()
        );
        assert_eq!(git(&tree, &["rev-parse", "origin/master"]).await, tip);
    }

    #[tokio::test]
    async fn test_sync_falls_back_to_the_upstream_when_a_mirror_fails() {
        let root = TempDir::new().unwrap();
        let up = upstream_repo(root.path()).await;
        let missing = root.path().join("no-such-mirror");
        let tree = root.path().join("tree");

        let outcome = sync(&tree, upstream(&up, Seed::Mirror(leak(&missing))))
            .await
            .unwrap();

        assert_eq!(outcome, SyncOutcome::Cloned { seeded: false });
        assert_eq!(
            git(&tree, &["remote", "get-url", "origin"]).await,
            up.to_string_lossy()
        );
    }

    #[tokio::test]
    async fn test_sync_clones_with_a_bundle_seed() {
        let root = TempDir::new().unwrap();
        let up = upstream_repo(root.path()).await;
        let bundle = root.path().join("clone.bundle");
        git(
            &up,
            &["bundle", "create", &bundle.to_string_lossy(), "--all"],
        )
        .await;
        let tip = commit(&up, "after the bundle").await;
        let tree = root.path().join("tree");

        let outcome = sync(&tree, upstream(&up, Seed::Bundle(leak(&bundle))))
            .await
            .unwrap();

        assert!(matches!(outcome, SyncOutcome::Cloned { .. }), "{outcome:?}");
        assert_eq!(git(&tree, &["rev-parse", "origin/master"]).await, tip);
    }

    #[tokio::test]
    async fn test_sync_survives_a_missing_bundle() {
        let root = TempDir::new().unwrap();
        let up = upstream_repo(root.path()).await;
        let missing = root.path().join("no-such.bundle");
        let tree = root.path().join("tree");

        let outcome = sync(&tree, upstream(&up, Seed::Bundle(leak(&missing))))
            .await
            .unwrap();

        assert!(matches!(outcome, SyncOutcome::Cloned { .. }), "{outcome:?}");
        assert!(git(&tree, &["rev-parse", "origin/master"]).await.len() >= 40);
    }

    #[tokio::test]
    async fn test_sync_replaces_an_interrupted_clone() {
        let root = TempDir::new().unwrap();
        let up = upstream_repo(root.path()).await;
        let tree = root.path().join("tree");
        std::fs::create_dir_all(tree.join(".git/objects/pack")).unwrap();

        let outcome = sync(&tree, upstream(&up, Seed::None)).await.unwrap();

        assert_eq!(outcome, SyncOutcome::Cloned { seeded: false });
    }

    #[tokio::test]
    async fn test_sync_refuses_a_directory_that_is_not_a_repository() {
        let root = TempDir::new().unwrap();
        let up = upstream_repo(root.path()).await;
        let tree = root.path().join("tree");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("precious"), "keep me").unwrap();

        let error = sync(&tree, upstream(&up, Seed::None)).await.unwrap_err();

        assert!(
            error.to_string().contains("not a git repository"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(tree.join("precious")).unwrap(),
            "keep me"
        );
    }

    #[tokio::test]
    async fn test_check_daemon_tree_does_not_mistake_a_parent_repository() {
        // An empty third_party/<project> inside a checkout must not pass as a
        // tree because git finds the checkout above it.
        let root = TempDir::new().unwrap();
        let up = upstream_repo(root.path()).await;
        let nested = up.join("third_party/linux");
        std::fs::create_dir_all(&nested).unwrap();

        let error = check_daemon_tree(&nested, ProjectId::Linux)
            .await
            .unwrap_err();

        assert!(error.contains("sashiko repo sync"), "{error}");
        assert!(error.contains("linux"), "{error}");
    }

    #[tokio::test]
    async fn test_check_daemon_tree_accepts_a_repository() {
        let root = TempDir::new().unwrap();
        let up = upstream_repo(root.path()).await;

        check_daemon_tree(&up, ProjectId::Sashiko).await.unwrap();
    }

    #[tokio::test]
    async fn test_check_daemon_tree_rejects_a_missing_path() {
        let root = TempDir::new().unwrap();

        let error = check_daemon_tree(&root.path().join("absent"), ProjectId::Linux)
            .await
            .unwrap_err();

        assert!(error.contains("is not a git repository"), "{error}");
    }
}
