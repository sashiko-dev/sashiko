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

use crate::events::{Event, MessageSource};
use crate::utils::redact_secret;
use anyhow::{Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{Duration, interval};
use tracing::{error, info, warn};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FetchRequest {
    pub repo_url: Option<String>,
    pub commit_hash: String,
}

#[derive(Clone)]
struct DelayedRequest {
    repo_url: Option<String>,
    commit_hash: String,
    run_at: std::time::Instant,
}

pub struct FetchAgent {
    repo_path: PathBuf,
    rx: mpsc::Receiver<FetchRequest>,
    main_tx: mpsc::Sender<Event>,
    tick_interval: Duration,
    backoff_base: Duration,
}

impl FetchAgent {
    pub fn new(
        repo_path: PathBuf,
        main_tx: mpsc::Sender<Event>,
    ) -> (Self, mpsc::Sender<FetchRequest>) {
        let (tx, rx) = mpsc::channel(100);
        (
            Self {
                repo_path,
                rx,
                main_tx,
                tick_interval: Duration::from_secs(10),
                backoff_base: Duration::from_secs(10),
            },
            tx,
        )
    }

    pub fn with_intervals(mut self, tick_interval: Duration, backoff_base: Duration) -> Self {
        self.tick_interval = tick_interval;
        self.backoff_base = backoff_base;
        self
    }

    pub async fn run(mut self) {
        info!("FetchAgent started");
        let mut queue: HashMap<Option<String>, HashSet<String>> = HashMap::new();
        let mut delayed_retries: Vec<DelayedRequest> = Vec::new(); // In-memory delay vector
        let mut attempts_tracker: HashMap<String, u32> = HashMap::new(); // Local attempt tracker
        let mut ticker = interval(self.tick_interval); // Configurable batch timer

        loop {
            tokio::select! {
                Some(req) = self.rx.recv() => {
                    queue.entry(req.repo_url)
                        .or_default()
                        .insert(req.commit_hash);
                }
                _ = ticker.tick() => {
                    let now = std::time::Instant::now();
                    let mut expired_retries = Vec::new();

                    // Filter out expired retries
                    delayed_retries.retain(|item| {
                        if now >= item.run_at {
                            expired_retries.push(item.clone());
                            false // Remove from delayed list (promoted)
                        } else {
                            true // Keep in delayed list
                        }
                    });

                    // Promote expired retries into the active batch queue
                    for retry in expired_retries {
                        queue.entry(retry.repo_url)
                            .or_default()
                            .insert(retry.commit_hash);
                    }

                    // Process active batch queue
                    if !queue.is_empty() {
                        self.process_queue(&mut queue, &mut delayed_retries, &mut attempts_tracker).await;
                    }
                }
            }
        }
    }

    async fn process_queue(
        &self,
        queue: &mut HashMap<Option<String>, HashSet<String>>,
        delayed_retries: &mut Vec<DelayedRequest>,
        attempts_tracker: &mut HashMap<String, u32>,
    ) {
        info!("Processing fetch queue with {} repos", queue.len());

        for (url_opt, commits) in queue.drain() {
            if commits.is_empty() {
                continue;
            }

            let commit_list: Vec<String> = commits.into_iter().collect();
            let url_display = url_opt.as_deref().unwrap_or("local");

            info!(
                "Processing {} commits for remote {}",
                commit_list.len(),
                url_display
            );

            // Check existence first
            let mut missing_commits = Vec::new();
            for commit in &commit_list {
                if !self.is_present(commit).await {
                    missing_commits.push(commit.clone());
                }
            }

            if missing_commits.is_empty() {
                info!(
                    "All commits present locally, skipping fetch for {}",
                    url_display
                );
            } else if let Some(ref url) = url_opt {
                // Remote fetch logic
                let remote_name = self.get_remote_name(url);

                // Check if repo is local (same as self.repo_path)
                let is_local = {
                    let url_path = PathBuf::from(&url);
                    if let (Ok(canon_url), Ok(canon_repo)) = (
                        std::fs::canonicalize(&url_path),
                        std::fs::canonicalize(&self.repo_path),
                    ) {
                        canon_url == canon_repo
                    } else {
                        false
                    }
                };

                if is_local {
                    warn!(
                        "Repository is local but commits are missing: {:?}. Cannot fetch.",
                        missing_commits
                    );
                    // Do not continue here; let it fall through to Step 3 where it will fail individually
                } else {
                    if let Err(e) = self.ensure_remote(&remote_name, url).await {
                        error!("Failed to ensure remote {}: {}", url, e);
                        for commit in &missing_commits {
                            self.handle_fetch_failure(
                                &url_opt,
                                commit,
                                &format!("Failed to set up remote {}: {}", url, e),
                                delayed_retries,
                                attempts_tracker,
                            )
                            .await;
                        }
                        continue;
                    }

                    // 1. Try optimistic fetch (fetch specific commits)
                    if let Err(e) = self.fetch_commits(&remote_name, &missing_commits).await {
                        warn!(
                            "Optimistic fetch failed for {}: {}. Falling back to full fetch.",
                            url, e
                        );
                        // 2. Fallback: Fetch everything (heads)
                        if let Err(e) = self.fetch_all(&remote_name).await {
                            error!("Full fetch failed for {}: {}", url, e);
                            for commit in &missing_commits {
                                self.handle_fetch_failure(
                                    &url_opt,
                                    commit,
                                    &format!("Failed to fetch from {}: {}", url, e),
                                    delayed_retries,
                                    attempts_tracker,
                                )
                                .await;
                            }
                            continue;
                        }
                    }
                }
            } else {
                // Local repo, but commits are missing
                warn!(
                    "Local repository missing commits: {:?}. Cannot fetch.",
                    missing_commits
                );
            }

            // 3. Process each commit or range
            for commit_or_range in commit_list {
                if commit_or_range.contains("..") {
                    // It's a range
                    let range = &commit_or_range;

                    let shas = match crate::git_ops::resolve_git_range(&self.repo_path, range).await
                    {
                        Ok(shas) => shas,
                        Err(e) => {
                            self.handle_fetch_failure(
                                &url_opt,
                                &commit_or_range,
                                &format!("Failed to resolve git range: {}", e),
                                delayed_retries,
                                attempts_tracker,
                            )
                            .await;
                            continue;
                        }
                    };
                    let count = shas.len() as u32;

                    // Process each SHA
                    for (i, sha) in shas.iter().enumerate() {
                        match self.extract_patch(sha, range, (i + 1) as u32, count).await {
                            Ok(mut event) => {
                                if let Event::PatchSubmitted {
                                    ref mut message_id, ..
                                } = event
                                {
                                    *message_id = sha.clone();
                                }
                                if let Err(e) = self.main_tx.send(event).await {
                                    error!("Failed to send PatchSubmitted event: {}", e);
                                }
                            }
                            Err(e) => {
                                error!(
                                    "Failed to extract patch {} from range {}: {}",
                                    sha, range, e
                                );
                            }
                        }
                    }
                    // Range Success! Clean up attempts tracker and delay queue for the range key
                    attempts_tracker.remove(&commit_or_range);
                    delayed_retries.retain(|item| item.commit_hash != commit_or_range);
                    info!("Successfully submitted remote range {}", range);
                } else {
                    // Single commit
                    let full_sha = match self.resolve_sha(&commit_or_range).await {
                        Ok(sha) => sha,
                        Err(e) => {
                            self.handle_fetch_failure(
                                &url_opt,
                                &commit_or_range,
                                &format!("Failed to resolve SHA: {}", e),
                                delayed_retries,
                                attempts_tracker,
                            )
                            .await;
                            continue;
                        }
                    };

                    match self.extract_patch(&full_sha, &commit_or_range, 1, 1).await {
                        Ok(mut event) => {
                            // Success! Clean up attempts tracker and the parked delay queue
                            attempts_tracker.remove(&commit_or_range);
                            delayed_retries.retain(|item| item.commit_hash != commit_or_range);

                            if let Event::PatchSubmitted {
                                ref mut message_id, ..
                            } = event
                            {
                                *message_id = full_sha.clone();
                            }
                            if let Err(e) = self.main_tx.send(event).await {
                                error!("Failed to send PatchSubmitted event: {}", e);
                            } else {
                                info!("Successfully submitted remote patch {}", commit_or_range);
                            }
                        }
                        Err(e) => {
                            error!("Failed to extract patch {}: {}", commit_or_range, e);
                            self.handle_fetch_failure(
                                &url_opt,
                                &commit_or_range,
                                &format!("Failed to extract patch: {}", e),
                                delayed_retries,
                                attempts_tracker,
                            )
                            .await;
                        }
                    }
                }
            }
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
        // Check if remote exists
        let status = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["remote", "get-url", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;

        if status.success() {
            // Check if URL matches, if not update it
            let output = Command::new("git")
                .current_dir(&self.repo_path)
                .args(["remote", "get-url", name])
                .output()
                .await?;
            let current_url = String::from_utf8_lossy(&output.stdout).trim().to_string();

            if current_url != url {
                info!(
                    "Updating remote {} from {} to {}",
                    name,
                    redact_secret(&current_url),
                    redact_secret(url)
                );
                Command::new("git")
                    .current_dir(&self.repo_path)
                    .args(["remote", "set-url", name, url])
                    .output()
                    .await?;
            }
        } else {
            info!("Adding remote {} -> {}", name, redact_secret(url));
            let output = Command::new("git")
                .current_dir(&self.repo_path)
                .args(["remote", "add", name, url])
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

    async fn fetch_commits(&self, remote: &str, commits: &[String]) -> Result<()> {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.repo_path).arg("fetch").arg(remote);

        for commit in commits {
            cmd.arg(commit);
        }

        let output = cmd.output().await?;
        if !output.status.success() {
            return Err(anyhow!(
                "Fetch failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }

    async fn fetch_all(&self, remote: &str) -> Result<()> {
        let output = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["fetch", remote])
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!(
                "Fetch all failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }

    async fn is_present(&self, commit_or_range: &str) -> bool {
        let arg_str: String;
        let args = if let Some((start, end)) = commit_or_range.split_once("..") {
            // For ranges, ensure both endpoints are commits
            arg_str = format!("{}^{{commit}}..{}^{{commit}}", start, end);
            vec!["rev-list", "-n", "1", &arg_str]
        } else {
            // For single commits, ensure it is a valid commit object
            arg_str = format!("{}^{{commit}}", commit_or_range);
            vec!["rev-parse", "--verify", &arg_str]
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

    async fn extract_patch(
        &self,
        commit: &str,
        article_id: &str,
        index: u32,
        total: u32,
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
        })
    }

    async fn handle_fetch_failure(
        &self,
        url_opt: &Option<String>,
        commit: &str,
        error_msg: &str,
        delayed_retries: &mut Vec<DelayedRequest>,
        attempts_tracker: &mut HashMap<String, u32>,
    ) {
        let attempts = attempts_tracker.get(commit).cloned().unwrap_or(0);
        let max_retries = 3;

        if attempts < max_retries {
            let next_attempt = attempts + 1;
            attempts_tracker.insert(commit.to_string(), next_attempt);

            // Exponential backoff using configurable base: base * (2 ^ attempts)
            let delay = self.backoff_base * (2u32.pow(next_attempt));
            let run_at = std::time::Instant::now() + delay;

            warn!(
                "Fetch/extract failed for {} (attempts: {}/{}). Parking in delay queue for {}s.",
                commit,
                next_attempt,
                max_retries,
                delay.as_secs()
            );

            // Deduplicate inside delayed_retries first to prevent duplicate parked entries!
            delayed_retries.retain(|item| item.commit_hash != commit);

            delayed_retries.push(DelayedRequest {
                repo_url: url_opt.clone(),
                commit_hash: commit.to_string(),
                run_at,
            });
        } else {
            // Retries exhausted, clean up attempts tracker and the parked delay queue
            attempts_tracker.remove(commit);
            delayed_retries.retain(|item| item.commit_hash != commit);
            error!(
                "Fetch/extract failed for {} after {} retries. Marking as Failed. Error: {}",
                commit, max_retries, error_msg
            );

            let _ = self
                .main_tx
                .send(Event::IngestionFailed {
                    article_id: commit.to_string(),
                    error: error_msg.to_string(),
                    source: MessageSource::GitFetch,
                })
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    #[tokio::test]
    async fn test_fetch_agent_lifecycle() {
        let (tx, _rx) = mpsc::channel(1);
        let repo_path = PathBuf::from("/tmp");
        let (_agent, _sender) = FetchAgent::new(repo_path, tx);
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

        let (tx, _rx) = mpsc::channel(1);
        let (agent, _) = FetchAgent::new(repo_path.clone(), tx);

        let output = Command::new("git")
            .current_dir(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        let head = String::from_utf8(output.stdout)?.trim().to_string();

        let event = agent.extract_patch(&head, &head, 1, 1).await?;

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

        let (tx, _rx) = mpsc::channel(1);
        let (agent, _) = FetchAgent::new(repo_path.clone(), tx);

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
    async fn test_fetch_agent_delay_queue_retry() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        // Setup empty dummy repo
        Command::new("git")
            .current_dir(&repo_path)
            .arg("init")
            .output()
            .await?;

        let (event_tx, mut event_rx) = mpsc::channel(10);

        // Create agent with tiny sub-second intervals for fast real-time testing!
        let (agent, fetch_tx) = {
            let (a, tx) = FetchAgent::new(repo_path.clone(), event_tx);
            (
                a.with_intervals(Duration::from_millis(50), Duration::from_millis(50)),
                tx,
            )
        };

        // Spawn the agent in the background
        let agent_handle = tokio::spawn(agent.run());

        // Enqueue a FetchRequest for a missing commit (which will fail resolve_sha!)
        let req = FetchRequest {
            repo_url: None,
            commit_hash: "0123456789abcdef0123456789abcdef01234567".to_string(),
        };
        fetch_tx.send(req).await?;

        // Let the first tick (50ms) process. Wait 100ms to be safe.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The first tick ran process_queue, failed, and parked it in delayed_retries (Attempts = 1, delay = 100ms)
        // Verify no event is received yet.
        tokio::select! {
            Some(event) = event_rx.recv() => {
                panic!("Received unexpected event: {:?}", event);
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                // Expected: no event sent!
            }
        }

        // Wait for retries to exhaust:
        // Attempt 1: delay 100ms.
        // Attempt 2: delay 200ms.
        // Attempt 3: delay 400ms.
        // Total delay is 700ms. Plus ticks processing time, we sleep 1.2 seconds to be completely safe.
        tokio::time::sleep(Duration::from_millis(1200)).await;

        // Verify that Event::IngestionFailed is finally received!
        tokio::select! {
            Some(Event::IngestionFailed { article_id, error, .. }) = event_rx.recv() => {
                assert_eq!(article_id, "0123456789abcdef0123456789abcdef01234567");
                assert!(
                    error.contains("Failed to resolve SHA") || error.contains("Failed to extract patch"),
                    "Actual error was: {}",
                    error
                );
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                panic!("Timed out waiting for IngestionFailed event after retries!");
            }
        }

        // Clean up the agent
        agent_handle.abort();

        Ok(())
    }
}
