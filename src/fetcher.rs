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

use crate::events::Event;
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
    pub mr_url: Option<String>,
    pub mr_title: Option<String>,
    pub mr_number: Option<i64>,
}

pub struct FetchAgent {
    repo_path: PathBuf,
    rx: mpsc::Receiver<FetchRequest>,
    main_tx: mpsc::Sender<Event>,
    #[allow(clippy::type_complexity)]
    mr_metadata: HashMap<String, (Option<String>, Option<String>, Option<i64>)>, // commit_hash -> (mr_url, mr_title, mr_number)
    gitlab_token: Option<String>,
}

impl FetchAgent {
    pub fn new(
        repo_path: PathBuf,
        main_tx: mpsc::Sender<Event>,
        gitlab_token: Option<String>,
    ) -> (Self, mpsc::Sender<FetchRequest>) {
        let (tx, rx) = mpsc::channel(100);
        (
            Self {
                repo_path,
                rx,
                main_tx,
                mr_metadata: HashMap::new(),
                gitlab_token,
            },
            tx,
        )
    }

    pub async fn run(mut self) {
        info!("FetchAgent started");
        let mut queue: HashMap<Option<String>, HashSet<String>> = HashMap::new();
        let mut ticker = interval(Duration::from_secs(10));

        loop {
            tokio::select! {
                Some(req) = self.rx.recv() => {
                    // Store MR metadata if present
                    if req.mr_url.is_some() || req.mr_title.is_some() || req.mr_number.is_some() {
                        self.mr_metadata.insert(
                            req.commit_hash.clone(),
                            (req.mr_url.clone(), req.mr_title.clone(), req.mr_number)
                        );
                    }
                    queue.entry(req.repo_url)
                        .or_default()
                        .insert(req.commit_hash);
                }
                _ = ticker.tick() => {
                    if !queue.is_empty() {
                        self.process_queue(&mut queue).await;
                    }
                }
            }
        }
    }

    async fn process_queue(&self, queue: &mut HashMap<Option<String>, HashSet<String>>) {
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
            // For ranges (base..head), we need to fetch both endpoints, not the range itself.
            // Git ranges are NOT valid refspecs - we must fetch the individual SHAs.
            let mut commits_to_check = Vec::new();
            for commit_or_range in &commit_list {
                if commit_or_range.contains("..") {
                    // It's a range - split and check both base and head
                    let parts: Vec<&str> = commit_or_range.split("..").collect();
                    if parts.len() == 2 {
                        commits_to_check.push(parts[0].to_string());
                        commits_to_check.push(parts[1].to_string());
                    }
                } else {
                    commits_to_check.push(commit_or_range.clone());
                }
            }

            let mut missing_commits = Vec::new();
            for commit in &commits_to_check {
                if !self.is_present(commit).await {
                    missing_commits.push(commit.clone());
                }
            }

            if missing_commits.is_empty() {
                info!(
                    "All commits present locally, skipping fetch for {}",
                    url_display
                );
            } else if let Some(url) = url_opt {
                // Remote fetch logic
                let remote_name = self.get_remote_name(&url);

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
                    if let Err(e) = self.ensure_remote(&remote_name, &url).await {
                        error!("Failed to ensure remote {}: {}", url, e);
                        for commit_or_range in &commit_list {
                            let _ = self
                                .main_tx
                                .send(Event::IngestionFailed {
                                    article_id: commit_or_range.clone(),
                                    error: format!("Failed to set up remote {}: {}", url, e),
                                })
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
                            for commit_or_range in &commit_list {
                                let _ = self
                                    .main_tx
                                    .send(Event::IngestionFailed {
                                        article_id: commit_or_range.clone(),
                                        error: format!("Failed to fetch from {}: {}", url, e),
                                    })
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
                    commits_to_check
                );
            }

            // 3. Process each commit or range
            for commit_or_range in commit_list {
                if commit_or_range.contains("..") {
                    // It's a range
                    let range = &commit_or_range;

                    // 1. Count commits
                    let count_output = Command::new("git")
                        .current_dir(&self.repo_path)
                        .args(["-c", "safe.bareRepository=all"])
                        .args(["rev-list", "--count", range])
                        .output()
                        .await;

                    let count = match count_output {
                        Ok(output) if output.status.success() => {
                            String::from_utf8_lossy(&output.stdout)
                                .trim()
                                .parse::<u32>()
                                .unwrap_or(0)
                        }
                        _ => {
                            let _ = self
                                .main_tx
                                .send(Event::IngestionFailed {
                                    article_id: range.clone(),
                                    error: "Failed to resolve git range count".to_string(),
                                })
                                .await;
                            continue;
                        }
                    };

                    if count == 0 {
                        let _ = self
                            .main_tx
                            .send(Event::IngestionFailed {
                                article_id: range.clone(),
                                error: "Git range is empty".to_string(),
                            })
                            .await;
                        continue;
                    }

                    // 2. Get list of SHAs
                    let list_output = Command::new("git")
                        .current_dir(&self.repo_path)
                        .args(["-c", "safe.bareRepository=all"])
                        .args(["rev-list", "--reverse", range])
                        .output()
                        .await;

                    let shas = match list_output {
                        Ok(output) if output.status.success() => {
                            String::from_utf8_lossy(&output.stdout)
                                .lines()
                                .map(|s| s.to_string())
                                .collect::<Vec<_>>()
                        }
                        _ => {
                            let _ = self
                                .main_tx
                                .send(Event::IngestionFailed {
                                    article_id: range.clone(),
                                    error: "Failed to resolve git range SHAs".to_string(),
                                })
                                .await;
                            continue;
                        }
                    };

                    // 3. Process each SHA
                    for (i, sha) in shas.iter().enumerate() {
                        // Get MR metadata for this commit range
                        let (mr_url, mr_title, mr_number) =
                            self.mr_metadata.get(range).unwrap_or(&(None, None, None));

                        // Use MR-prefixed article_id to match placeholder created in api.rs
                        let article_id = if let Some(number) = mr_number {
                            format!("mr-{}-{}", number, range)
                        } else {
                            range.to_string()
                        };

                        match self
                            .extract_patch(
                                sha,
                                &article_id,
                                (i + 1) as u32,
                                count,
                                mr_url.as_ref(),
                                mr_title.as_ref(),
                                *mr_number,
                            )
                            .await
                        {
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
                    info!("Successfully submitted remote range {}", range);
                } else {
                    // Single commit
                    let full_sha = match self.resolve_sha(&commit_or_range).await {
                        Ok(sha) => sha,
                        Err(e) => {
                            let _ = self
                                .main_tx
                                .send(Event::IngestionFailed {
                                    article_id: commit_or_range.clone(),
                                    error: format!("Failed to resolve SHA: {}", e),
                                })
                                .await;
                            continue;
                        }
                    };

                    // Get MR metadata for this commit
                    let (mr_url, mr_title, mr_number) = self
                        .mr_metadata
                        .get(&commit_or_range)
                        .unwrap_or(&(None, None, None));

                    // Use MR-prefixed article_id to match placeholder created in api.rs
                    let article_id = if let Some(number) = mr_number {
                        format!("mr-{}-{}", number, &commit_or_range)
                    } else {
                        commit_or_range.clone()
                    };

                    match self
                        .extract_patch(
                            &full_sha,
                            &article_id,
                            1,
                            1,
                            mr_url.as_ref(),
                            mr_title.as_ref(),
                            *mr_number,
                        )
                        .await
                    {
                        Ok(mut event) => {
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
                            let _ = self
                                .main_tx
                                .send(Event::IngestionFailed {
                                    article_id: commit_or_range,
                                    error: format!("Failed to extract patch: {}", e),
                                })
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
        // Inject GitLab token if available and URL is from gitlab.com
        let authenticated_url = if let Some(token) = &self.gitlab_token {
            if url.contains("gitlab.com") && url.starts_with("https://") {
                // Embed token using oauth2 format: https://oauth2:TOKEN@gitlab.com/...
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
            // Check if URL matches, if not update it
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

    async fn fetch_commits(&self, remote: &str, commits: &[String]) -> Result<()> {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .arg("fetch")
            .arg(remote);

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
            .args(["-c", "safe.bareRepository=all"])
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
        let mut args = vec!["-c", "safe.bareRepository=all"];
        let arg_str: String;

        if let Some((start, end)) = commit_or_range.split_once("..") {
            // For ranges, ensure both endpoints are commits
            arg_str = format!(
                "{Start}^{{commit}}..{End}^{{commit}}",
                Start = start,
                End = end
            );
            args.extend(["rev-list", "-n", "1", &arg_str]);
        } else {
            // For single commits, ensure it is a valid commit object
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
                        String::from_utf8_lossy(&s.stderr).trim()
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
        // Resolve parent to use as base_commit
        let parent_output = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["-c", "safe.bareRepository=all"])
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

        // Format: AuthorName%nAuthorEmail%nSubject%nBody...%n---SASHIKO-END-HEADER---%nDiff...
        let format = "format:%an%n%ae%n%s%n%b%n---SASHIKO-END-HEADER---%n";

        let output = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .args(["show", &format!("--format={}", format), commit])
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!(
                "git show failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        let raw = String::from_utf8_lossy(&output.stdout).to_string();
        let parts: Vec<&str> = raw.split("---SASHIKO-END-HEADER---\n").collect();

        if parts.len() < 2 {
            return Err(anyhow!("Failed to parse git show output structure"));
        }

        let header_part = parts[0];
        let diff = parts[1..].join("---SASHIKO-END-HEADER---\n"); // Rejoin just in case

        let mut lines = header_part.lines();
        let author_name = lines.next().unwrap_or_default().trim();
        let author_email = lines.next().unwrap_or("unknown@localhost").trim();
        let commit_subject = lines.next().unwrap_or("No Subject").trim();

        // Always use commit subject for patch records
        // Patchset title override happens in main.rs when MR metadata is available
        let subject = commit_subject.to_string();

        // Body is the rest
        let body: Vec<&str> = lines.collect();
        let message = body.join("\n").trim().to_string();

        let author = if author_name.is_empty() || author_name.to_lowercase() == "unknown" {
            author_email.to_string()
        } else {
            format!("{} <{}>", author_name, author_email)
        };

        Ok(Event::PatchSubmitted {
            group: "git-fetch".to_string(),
            article_id: article_id.to_string(),
            message_id: String::new(), // Set by caller
            subject,
            author,
            message,
            diff,
            base_commit,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64,
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

    #[tokio::test]
    async fn test_fetch_agent_lifecycle() {
        let (tx, _rx) = mpsc::channel(1);
        let repo_path = PathBuf::from("/tmp");
        let (_agent, _sender) = FetchAgent::new(repo_path, tx, None);
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
        let (agent, _) = FetchAgent::new(repo_path.clone(), tx, None);

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

        let (tx, _rx) = mpsc::channel(1);
        let (agent, _) = FetchAgent::new(repo_path.clone(), tx, None);

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
}
