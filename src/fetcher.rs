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

use crate::db::{Database, FetchQueueRow};
use crate::events::{Event, MessageSource};
use crate::utils::redact_secret;
use anyhow::{Result, anyhow};
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{Duration, interval};
use tracing::{error, info, warn};

/// How often the worker polls the durable queue for due work.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How often expired leases from dead workers are reclaimed.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// A claimed fetch whose worker stops updating it for this long is treated as a
/// ghost and reclaimed by the sweep.
const GHOST_LEASE_SECS: i64 = 600;
/// Total wall-clock window a fetch keeps retrying before it is failed.
const RETRY_WINDOW_SECS: i64 = 24 * 60 * 60;
/// First retry delay; subsequent delays double up to BACKOFF_CAP_SECS.
const BACKOFF_BASE_SECS: i64 = 30;
/// Maximum delay between retries.
const BACKOFF_CAP_SECS: i64 = 600;

/// Maximum number of fetch_queue rows to claim and pre-fetch in a single
/// `git fetch` call. Batching improves server-side pack negotiation.
const BATCH_SIZE: usize = 20;

/// Marker error for failures that can never succeed on retry (e.g. a commit
/// is missing and the row has no remote to fetch from). These bypass the
/// backoff retry window and fail the fetch immediately, mirroring the
/// fast-fail behaviour of the old in-memory FetchAgent.
#[derive(Debug)]
struct PermanentFetchError(String);

impl std::fmt::Display for PermanentFetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for PermanentFetchError {}

/// Derive the git commit or range to fetch from a placeholder message id.
///
/// Handles the two placeholder id shapes produced by the API:
///   - git-fetch:  "<sha>@sashiko.local"        -> "<sha>"
///   - PR/MR:      "mr-<number>-<base>..<head>"  -> "<base>..<head>"
pub fn commit_hash_from_placeholder(msgid: &str) -> String {
    // "mr-<number>-<rest>" -> "<rest>"; anything else passes through.
    let s = msgid
        .strip_prefix("mr-")
        .and_then(|rest| rest.find('-').map(|dash| &rest[dash + 1..]))
        .unwrap_or(msgid);
    // Strip any "@sashiko.local" suffix from git-fetch placeholder ids.
    s.split('@').next().unwrap_or(s).to_string()
}

/// Current unix time in seconds.
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Exponential backoff (in seconds) for a given attempt count. `attempts` is the
/// number of attempts already made (>= 1 after the first claim): 1 -> 30s,
/// 2 -> 60s, 3 -> 120s, ... capped at BACKOFF_CAP_SECS.
fn backoff_secs(attempts: i64) -> i64 {
    let exp = attempts.saturating_sub(1).clamp(0, 20) as u32;
    BACKOFF_BASE_SECS
        .saturating_mul(2_i64.saturating_pow(exp))
        .min(BACKOFF_CAP_SECS)
}

/// Whether a revision string is a full git object id (sha1 = 40 hex chars, or
/// sha256 = 64). Only full object ids can be requested directly via `want`
/// without a ref lookup; branch/tag names must be resolved by the server.
fn looks_like_object_id(rev: &str) -> bool {
    matches!(rev.len(), 40 | 64) && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Durable, restart-safe git fetch worker.
///
/// Replaces the previous in-memory `FetchAgent` channel. Fetch requests live in
/// the `fetch_queue` table; this worker claims them under a lease, performs the
/// git fetch and patch extraction, and either completes them, schedules a
/// backoff retry, or (after RETRY_WINDOW_SECS) fails them terminally. Because
/// all state lives in the database, requests survive restarts and crashed
/// workers are recovered by the ghost sweep.
pub struct FetchWorker {
    repo_path: PathBuf,
    db: Arc<Database>,
    main_tx: mpsc::Sender<Event>,
    gitlab_token: Option<String>,
}

impl FetchWorker {
    pub fn new(
        repo_path: PathBuf,
        db: Arc<Database>,
        main_tx: mpsc::Sender<Event>,
        gitlab_token: Option<String>,
    ) -> Self {
        Self {
            repo_path,
            db,
            main_tx,
            gitlab_token,
        }
    }

    pub async fn run(self) {
        info!("FetchWorker started");

        // Reclaim ghost leases on an independent task so that a long-running (or
        // stuck) fetch on the main loop can never prevent dead-worker recovery.
        {
            let db = self.db.clone();
            tokio::spawn(async move {
                let mut sweep = interval(SWEEP_INTERVAL);
                loop {
                    sweep.tick().await;
                    match db.sweep_ghost_fetches(GHOST_LEASE_SECS).await {
                        Ok(n) if n > 0 => {
                            warn!("Reclaimed {} ghost fetch(es) from dead workers", n);
                        }
                        Ok(_) => {}
                        Err(e) => error!("Ghost fetch sweep failed: {}", e),
                    }
                }
            });
        }

        let mut poll = interval(POLL_INTERVAL);
        loop {
            poll.tick().await;
            // Claim up to BATCH_SIZE rows and pre-fetch their objects in a
            // single `git fetch` call. Batching lets the server compute one
            // optimal pack for many wants, avoiding the pathological 11M-object
            // downloads that happen when unreachable SHAs are fetched one at a
            // time with poor negotiation.
            let mut batch = Vec::new();
            loop {
                if batch.len() >= BATCH_SIZE {
                    break;
                }
                match self.db.lock_pending_fetch().await {
                    Ok(Some(row)) => batch.push(row),
                    Ok(None) => break,
                    Err(e) => {
                        error!("Failed to claim pending fetch: {}", e);
                        break;
                    }
                }
            }
            if batch.is_empty() {
                continue;
            }

            // Pre-fetch all missing objects in one batch call.
            self.batch_prefetch(&batch).await;

            // Process each row individually for ingestion.
            for row in batch {
                self.process_row(row).await;
            }
        }
    }

    /// Process a single claimed fetch: ensure the commits are present locally
    /// (fetching from the remote if needed), extract the patch(es), and emit the
    /// corresponding events. On any failure the row is retried with backoff, or
    /// failed terminally once the retry window is exhausted.
    async fn process_row(&self, row: FetchQueueRow) {
        info!(
            "Processing fetch {} (commit {}, attempt {})",
            row.id, row.commit_hash, row.attempts
        );

        if let Err(e) = self.ensure_present(&row).await {
            self.handle_failure(&row, e.context("fetch failed")).await;
            return;
        }

        match self.ingest(&row).await {
            Ok(()) => {
                if let Err(e) = self.db.mark_fetch_done(row.id).await {
                    error!("Failed to mark fetch {} done: {}", row.id, e);
                } else {
                    info!("Fetch complete for {}", row.commit_hash);
                }
            }
            Err(e) => {
                self.handle_failure(&row, e.context("extract failed")).await;
            }
        }
    }

    /// Ensure every commit referenced by the row exists locally, fetching from
    /// the remote when necessary.
    async fn ensure_present(&self, row: &FetchQueueRow) -> Result<()> {
        // For ranges (base..head), both endpoints must be checked individually.
        let mut to_check = Vec::new();
        if let Some((start, end)) = row.commit_hash.split_once("..") {
            to_check.push(start.to_string());
            to_check.push(end.to_string());
        } else {
            to_check.push(row.commit_hash.clone());
        }
        for sha in &row.supporting_commits {
            if let Some((start, end)) = sha.split_once("..") {
                to_check.push(start.to_string());
                to_check.push(end.to_string());
            } else {
                to_check.push(sha.clone());
            }
        }

        let mut missing = Vec::new();
        for commit in &to_check {
            if !self.is_present(commit).await {
                missing.push(commit.clone());
            }
        }
        if missing.is_empty() {
            return Ok(());
        }

        let url = match row.repo_url.as_deref().map(str::trim) {
            Some(u) if !u.is_empty() => u,
            _ => {
                return Err(PermanentFetchError(format!(
                    "commits {:?} missing and no remote is configured",
                    missing
                ))
                .into());
            }
        };

        // A local repository cannot be fetched from; the commits simply are not
        // there.
        let is_local = {
            let url_path = PathBuf::from(url);
            match (
                std::fs::canonicalize(&url_path),
                std::fs::canonicalize(&self.repo_path),
            ) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            }
        };
        if is_local {
            return Err(PermanentFetchError(format!(
                "repository is local but commits are missing: {:?}",
                missing
            ))
            .into());
        }

        let remote_name = self.get_remote_name(url);

        // Acquire the per-remote lock so we never run concurrent fetches
        // against the same remote (shared with ensure_remote_with_refspecs
        // and batch_prefetch).
        let lock = crate::git_ops::get_remote_lock(&remote_name);
        let _guard = lock.lock().await;

        self.ensure_remote(&remote_name, url).await?;

        // Fetch exactly the missing revisions. We deliberately do NOT fall
        // back to a full `git fetch` of all refs: large mirrors (e.g. the kernel
        // repo with hundreds of thousands of refs) make that pathologically
        // slow, and it is unnecessary -- a still-missing object here is
        // genuinely unfetchable and should surface as an error.
        self.fetch_commits(&remote_name, &missing).await?;
        Ok(())
    }

    /// Extract patch(es) for the row and emit PatchSubmitted events.
    async fn ingest(&self, row: &FetchQueueRow) -> Result<()> {
        let article_id = Self::article_id_for(row);
        let mr_url = row.mr_url.clone();
        let mr_title = row.mr_title.clone();

        if row.commit_hash.contains("..") {
            let shas = crate::git_ops::resolve_git_range(&self.repo_path, &row.commit_hash).await?;
            let count = shas.len() as u32;
            for (i, sha) in shas.iter().enumerate() {
                let mut event = self
                    .extract_patch(
                        sha,
                        &article_id,
                        (i + 1) as u32,
                        count,
                        mr_url.as_ref(),
                        mr_title.as_ref(),
                        row.mr_number,
                    )
                    .await?;
                if let Event::PatchSubmitted {
                    ref mut message_id, ..
                } = event
                {
                    *message_id = sha.clone();
                }
                self.main_tx
                    .send(event)
                    .await
                    .map_err(|e| anyhow!("failed to send event: {}", e))?;
            }
            info!("Submitted range {}", row.commit_hash);
        } else {
            let full_sha = self.resolve_sha(&row.commit_hash).await?;
            let mut event = self
                .extract_patch(
                    &full_sha,
                    &article_id,
                    1,
                    1,
                    mr_url.as_ref(),
                    mr_title.as_ref(),
                    row.mr_number,
                )
                .await?;
            if let Event::PatchSubmitted {
                ref mut message_id, ..
            } = event
            {
                *message_id = full_sha.clone();
            }
            self.main_tx
                .send(event)
                .await
                .map_err(|e| anyhow!("failed to send event: {}", e))?;
            info!("Submitted patch {}", row.commit_hash);
        }
        Ok(())
    }

    /// Decide whether to retry (with backoff) or terminally fail a fetch.
    async fn handle_failure(&self, row: &FetchQueueRow, error: anyhow::Error) {
        let now = now_secs();
        let first = row.first_attempt_at.unwrap_or(now);
        let permanent = error.downcast_ref::<PermanentFetchError>().is_some();
        let msg = format!("{:#}", error);
        if permanent {
            error!(
                "Fetch {} for {} failed permanently (not retrying): {}",
                row.id, row.commit_hash, msg
            );
        } else if now - first >= RETRY_WINDOW_SECS {
            error!(
                "Fetch {} for {} exhausted its {}h retry window: {}",
                row.id,
                row.commit_hash,
                RETRY_WINDOW_SECS / 3600,
                msg
            );
        }
        if permanent || now - first >= RETRY_WINDOW_SECS {
            if let Err(e) = self.db.mark_fetch_failed(row.id, &msg).await {
                error!("Failed to mark fetch {} failed: {}", row.id, e);
            }
            // Surface the terminal failure so the placeholder patchset moves out
            // of 'Fetching' into 'Failed'.
            let _ = self
                .main_tx
                .send(Event::IngestionFailed {
                    article_id: Self::article_id_for(row),
                    error: msg.clone(),
                    source: MessageSource::GitFetch,
                })
                .await;
        } else {
            let backoff = backoff_secs(row.attempts);
            warn!(
                "Fetch {} for {} failed (attempt {}), retrying in {}s: {}",
                row.id, row.commit_hash, row.attempts, backoff, msg
            );
            if let Err(e) = self
                .db
                .set_fetch_retry_at(row.id, now + backoff, &msg)
                .await
            {
                error!("Failed to schedule retry for fetch {}: {}", row.id, e);
            }
        }
    }

    fn article_id_for(row: &FetchQueueRow) -> String {
        match row.mr_number {
            Some(number) => format!("mr-{}-{}", number, row.commit_hash),
            None => row.commit_hash.clone(),
        }
    }

    fn get_remote_name(&self, url: &str) -> String {
        // Use a hash of the URL to ensure safe and unique remote names
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        url.hash(&mut hasher);
        format!("fetcher-{:x}", hasher.finish())
    }

    async fn ensure_remote(&self, name: &str, url: &str) -> Result<()> {
        let global_lock = crate::git_ops::get_global_config_lock();
        let _global_guard = global_lock.lock().await;

        // Inject GitLab token if available
        let authenticated_url = if let Some(token) = &self.gitlab_token {
            if url.contains("gitlab.com") && url.starts_with("https://") {
                url.replace("https://", &format!("https://oauth2:{}@", token))
            } else {
                url.to_string()
            }
        } else {
            url.to_string()
        };

        // Check if remote exists
        let status = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .args(["remote", "get-url", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;

        if status.success() {
            let output = Command::new("git")
                .current_dir(&self.repo_path)
                .args(["-c", "safe.bareRepository=all"])
                .args(["remote", "get-url", name])
                .output()
                .await?;
            let current_url = String::from_utf8_lossy(&output.stdout).trim().to_string();

            if current_url != authenticated_url {
                info!(
                    "Updating remote {} from {} to {}",
                    name,
                    redact_secret(&current_url),
                    redact_secret(&authenticated_url)
                );
                Command::new("git")
                    .current_dir(&self.repo_path)
                    .args(["-c", "safe.bareRepository=all"])
                    .args(["remote", "set-url", name, &authenticated_url])
                    .output()
                    .await?;
            }
        } else {
            info!(
                "Adding remote {} -> {}",
                name,
                redact_secret(&authenticated_url)
            );
            let output = Command::new("git")
                .current_dir(&self.repo_path)
                .args(["-c", "safe.bareRepository=all"])
                .args(["remote", "add", name, &authenticated_url])
                .output()
                .await?;

            if !output.status.success() {
                return Err(anyhow!(
                    "Failed to add remote: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
        Ok(())
    }

    /// Base `git fetch` command shared by all fetches: no interactive prompt,
    /// the standard protocol restrictions, and `kill_on_drop` so a cancelled
    /// fetch is reaped. Callers append fetch flags, the remote, and revisions.
    ///
    /// Note: protocol v2 is deliberately NOT forced here. Protocol v1 sends
    /// all remote refs upfront, giving the client rich negotiation data that
    /// avoids pathological full-repo downloads for unreachable SHAs.
    fn fetch_base_cmd(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(crate::git_ops::GIT_PROTOCOL_RESTRICTIONS)
            .arg("fetch")
            .kill_on_drop(true);
        cmd
    }

    /// Runs one `git fetch`, dropping a stale commit-graph and trying
    /// again when that is what turned the fetch away.
    async fn fetch_with_graph_retry(&self, args: &[&str]) -> Result<Output> {
        let mut dropped_graph = false;

        loop {
            let mut cmd = self.fetch_base_cmd();
            cmd.args(args);
            let output = cmd.output().await?;

            if output.status.success() || dropped_graph {
                if dropped_graph {
                    crate::git_ops::schedule_commit_graph_rebuild(&self.repo_path);
                }
                return Ok(output);
            }

            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            if !crate::git_ops::is_stale_commit_graph(&stderr) {
                return Ok(output);
            }

            warn!("Fetch found a stale commit-graph; dropping it");
            if let Err(e) = crate::git_ops::drop_commit_graph(&self.repo_path).await {
                warn!("Failed to drop the commit-graph: {}", e);
                return Ok(output);
            }
            dropped_graph = true;
        }
    }

    /// Fetch the given revisions from `remote`, tuned for a very large remote.
    ///
    /// Full object ids are fetched directly (`want <oid>`) in a single batched
    /// call with no ref advertisement. Branch/tag names are resolved server-side
    /// via a protocol-v2, prefix-filtered `ls-refs` (cheap even against a remote
    /// with hundreds of thousands of refs) and fetched individually so one bad
    /// name does not fail the whole batch.
    async fn fetch_commits(&self, remote: &str, commits: &[String]) -> Result<()> {
        let mut object_ids: Vec<&str> = Vec::new();
        let mut ref_names: Vec<&str> = Vec::new();
        for c in commits {
            if looks_like_object_id(c) {
                object_ids.push(c.as_str());
            } else {
                ref_names.push(c.as_str());
            }
        }

        if !object_ids.is_empty() {
            let mut args = vec!["--no-tags", "--no-write-fetch-head", remote];
            args.extend(object_ids.iter().copied());
            let output = self.fetch_with_graph_retry(&args).await?;
            if !output.status.success() {
                return Err(anyhow!(
                    "fetch of {} object(s) failed: {}",
                    object_ids.len(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }

        for name in ref_names {
            let output = self.fetch_with_graph_retry(&[remote, name]).await?;
            if !output.status.success() {
                return Err(anyhow!(
                    "fetch of ref {} failed: {}",
                    name,
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
        }
        Ok(())
    }

    /// Pre-fetch objects for a batch of rows in a single `git fetch` call.
    /// Groups rows by remote URL and fetches all missing SHAs together so
    /// the server can compute one optimal pack.
    async fn batch_prefetch(&self, rows: &[FetchQueueRow]) {
        // Group SHAs by remote URL.
        let mut by_remote: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for row in rows {
            let mut commits = if let Some((start, end)) = row.commit_hash.split_once("..") {
                vec![start.to_string(), end.to_string()]
            } else {
                vec![row.commit_hash.clone()]
            };
            for sha in &row.supporting_commits {
                if let Some((start, end)) = sha.split_once("..") {
                    commits.push(start.to_string());
                    commits.push(end.to_string());
                } else {
                    commits.push(sha.clone());
                }
            }
            let url = row.repo_url.clone().unwrap_or_default();
            by_remote.entry(url).or_default().extend(commits);
        }

        for (url, commits) in &by_remote {
            if url.is_empty() {
                continue;
            }
            // Filter to only missing objects.
            let mut missing = Vec::new();
            for c in commits {
                if !self.is_present(c).await {
                    missing.push(c.clone());
                }
            }
            if missing.is_empty() {
                continue;
            }

            let remote_name = self.get_remote_name(url);
            if let Err(e) = self.ensure_remote(&remote_name, url).await {
                warn!("batch_prefetch: failed to ensure remote {}: {}", url, e);
                continue;
            }
            info!(
                "batch_prefetch: fetching {} SHAs from {} in one call",
                missing.len(),
                url
            );
            // Acquire the per-remote lock so we never run concurrent
            // fetches against the same remote (which causes duplicate
            // 11M-object downloads).
            let lock = crate::git_ops::get_remote_lock(&remote_name);
            let _guard = lock.lock().await;
            if let Err(e) = self.fetch_commits(&remote_name, &missing).await {
                warn!("batch_prefetch: batch fetch failed: {}", e);
                // Individual process_row calls will retry on their own.
            }
        }
    }

    async fn is_present(&self, commit_or_range: &str) -> bool {
        let mut args = vec!["-c", "safe.bareRepository=all"];
        let arg_str: String;

        if let Some((start, end)) = commit_or_range.split_once("..") {
            arg_str = format!("{}^{{commit}}..{}^{{commit}}", start, end);
            args.extend(["rev-list", "-n", "1", &arg_str]);
        } else {
            arg_str = format!("{}^{{commit}}", commit_or_range);
            args.extend(["rev-parse", "--verify", &arg_str]);
        };

        let output = Command::new("git")
            .current_dir(&self.repo_path)
            .args(&args)
            .output()
            .await;

        match output {
            Ok(s) => {
                let success = s.status.success();
                if success {
                    info!("is_present: {} is present", commit_or_range);
                } else {
                    info!(
                        "is_present: {} is missing or not a commit. stderr: {}",
                        commit_or_range,
                        String::from_utf8_lossy(&s.stderr)
                    );
                }
                success
            }
            Err(e) => {
                error!("is_present: {} check failed: {}", commit_or_range, e);
                false
            }
        }
    }

    async fn resolve_sha(&self, commit: &str) -> Result<String> {
        let output = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .args(["rev-parse", "--verify", commit])
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to resolve SHA for {}: {}",
                commit,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    #[allow(clippy::too_many_arguments)]
    async fn extract_patch(
        &self,
        commit: &str,
        article_id: &str,
        index: u32,
        total: u32,
        mr_url: Option<&String>,
        mr_title: Option<&String>,
        mr_number: Option<i64>,
    ) -> Result<Event> {
        let meta = crate::git_ops::extract_patch_metadata(&self.repo_path, commit).await?;

        Ok(Event::PatchSubmitted {
            group: "git-fetch".to_string(),
            article_id: article_id.to_string(),
            message_id: String::new(), // Set by caller
            subject: meta.subject,
            author: meta.author,
            message: meta.message,
            diff: meta.diff,
            base_commit: meta.base_commit,
            timestamp: meta.timestamp,
            index,
            total,
            mr_url: mr_url.cloned(),
            mr_title: mr_title.cloned(),
            mr_number,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    async fn test_worker(repo_path: PathBuf) -> FetchWorker {
        let settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&settings).await.unwrap());
        db.migrate().await.unwrap();
        let (tx, _rx) = mpsc::channel(1);
        FetchWorker::new(repo_path, db, tx, None)
    }

    #[test]
    fn test_commit_hash_from_placeholder() {
        assert_eq!(
            commit_hash_from_placeholder("abc123@sashiko.local"),
            "abc123"
        );
        assert_eq!(
            commit_hash_from_placeholder("mr-42-base..head"),
            "base..head"
        );
        assert_eq!(
            commit_hash_from_placeholder("mr-42-base..head@sashiko.local"),
            "base..head"
        );
        assert_eq!(commit_hash_from_placeholder("plainsha"), "plainsha");
    }

    #[test]
    fn test_backoff_secs_is_exponential_and_capped() {
        assert_eq!(backoff_secs(1), 30);
        assert_eq!(backoff_secs(2), 60);
        assert_eq!(backoff_secs(3), 120);
        assert_eq!(backoff_secs(4), 240);
        assert_eq!(backoff_secs(5), 480);
        assert_eq!(backoff_secs(6), 600);
        assert_eq!(backoff_secs(100), 600);
    }

    #[test]
    fn test_looks_like_object_id() {
        assert!(looks_like_object_id(&"a".repeat(40)));
        assert!(looks_like_object_id(&"0".repeat(64)));
        assert!(looks_like_object_id(
            "8a21aa5149b3f34f48a277d596d1ffe512b32a41"
        ));
        assert!(!looks_like_object_id("main"));
        assert!(!looks_like_object_id("v6.10"));
        assert!(!looks_like_object_id("refs/heads/main"));
        assert!(!looks_like_object_id(&"a".repeat(39)));
        assert!(!looks_like_object_id("zzzz"));
    }

    #[tokio::test]
    async fn test_fetch_worker_construction() {
        let _worker = test_worker(PathBuf::from("/tmp")).await;
    }

    #[tokio::test]
    async fn test_extract_patch_parsing() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        // Setup dummy repo
        Command::new("git")
            .current_dir(&repo_path)
            .arg("init")
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;

        let file_path = repo_path.join("file.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "content")?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Subject Line\n\nBody Line"])
            .output()
            .await?;

        let agent = test_worker(repo_path.clone()).await;

        let output = Command::new("git")
            .current_dir(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        let head = String::from_utf8(output.stdout)?.trim().to_string();

        let event = agent
            .extract_patch(&head, &head, 1, 1, None, None, None)
            .await?;

        match event {
            Event::PatchSubmitted {
                subject,
                author,
                message,
                diff,
                article_id,
                ..
            } => {
                assert_eq!(subject, "Subject Line");
                assert_eq!(author, "Test User <test@example.com>");
                assert!(message.contains("Body Line"));
                assert!(diff.contains("diff --git"));
                assert_eq!(article_id, head);
            }
            _ => panic!("Wrong event type"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_is_present_with_tree_sha() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        // Setup dummy repo
        Command::new("git")
            .current_dir(&repo_path)
            .arg("init")
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;

        let file_path = repo_path.join("file.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "content")?;

        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Subject Line"])
            .output()
            .await?;

        let agent = test_worker(repo_path.clone()).await;

        let output = Command::new("git")
            .current_dir(&repo_path)
            .args(["rev-parse", "HEAD^{tree}"])
            .output()
            .await?;
        let tree_sha = String::from_utf8(output.stdout)?.trim().to_string();

        assert!(
            !agent.is_present(&tree_sha).await,
            "Tree SHA should not be considered a present commit"
        );

        let output = Command::new("git")
            .current_dir(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        let commit_sha = String::from_utf8(output.stdout)?.trim().to_string();

        assert!(
            agent.is_present(&commit_sha).await,
            "Commit SHA should be considered present"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_ensure_present_checks_supporting_commits() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        Command::new("git")
            .current_dir(&repo_path)
            .arg("init")
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;

        let file_path = repo_path.join("file.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "content")?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        Command::new("git")
            .current_dir(&repo_path)
            .args(["commit", "-m", "Commit 1"])
            .output()
            .await?;

        let output = Command::new("git")
            .current_dir(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        let commit1 = String::from_utf8(output.stdout)?.trim().to_string();

        let agent = test_worker(repo_path.clone()).await;

        let row = FetchQueueRow {
            id: 1,
            patchset_id: None,
            cover_letter_message_id: None,
            repo_url: None,
            commit_hash: commit1.clone(),
            mr_url: None,
            mr_title: None,
            mr_number: None,
            status: crate::db::FetchStatus::Pending,
            attempts: 0,
            first_attempt_at: None,
            next_retry_at: None,
            locked_at: None,
            last_error: None,
            priority: 500,
            created_at: 0,
            supporting_commits: vec!["missing_sha_1234567890".to_string()],
        };

        // ensure_present must fail because supporting_commits contains a missing SHA
        assert!(agent.ensure_present(&row).await.is_err());

        // When supporting_commits is present locally, ensure_present must succeed
        let row_valid = FetchQueueRow {
            supporting_commits: vec![commit1.clone()],
            ..row
        };
        assert!(agent.ensure_present(&row_valid).await.is_ok());

        Ok(())
    }
}
