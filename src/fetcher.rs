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
    pub repo_url: String,
    pub commit_hash: String,
}

pub struct FetchAgent {
    repo_path: PathBuf,
    rx: mpsc::Receiver<FetchRequest>,
    main_tx: mpsc::Sender<Event>,
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
            },
            tx,
        )
    }

    pub async fn run(mut self) {
        info!("FetchAgent started");
        let mut queue: HashMap<String, HashSet<String>> = HashMap::new();
        let mut ticker = interval(Duration::from_secs(10));

        loop {
            tokio::select! {
                Some(req) = self.rx.recv() => {
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

    async fn process_queue(&self, queue: &mut HashMap<String, HashSet<String>>) {
        info!("Processing fetch queue with {} repos", queue.len());

        for (url, commits) in queue.drain() {
            if commits.is_empty() {
                continue;
            }

            let commit_list: Vec<String> = commits.into_iter().collect();
            let remote_name = self.get_remote_name(&url);

            info!(
                "Processing {} commits for remote {} ({})",
                commit_list.len(),
                remote_name,
                url
            );

            if let Err(e) = self.ensure_remote(&remote_name, &url).await {
                error!("Failed to ensure remote {}: {}", url, e);
                for commit in commit_list {
                    let _ = self
                        .main_tx
                        .send(Event::IngestionFailed {
                            article_id: commit,
                            error: format!("Failed to set up remote {}: {}", url, e),
                        })
                        .await;
                }
                continue;
            }

            // 1. Try optimistic fetch (fetch specific commits)
            // Many servers deny this (allowReachableSHA1InWant=false), but some support it.
            // We construct `git fetch remote sha1 sha2 ...`
            if let Err(e) = self.fetch_commits(&remote_name, &commit_list).await {
                warn!(
                    "Optimistic fetch failed for {}: {}. Falling back to full fetch.",
                    url, e
                );
                // 2. Fallback: Fetch everything (heads)
                if let Err(e) = self.fetch_all(&remote_name).await {
                    error!("Full fetch failed for {}: {}", url, e);
                    for commit in commit_list {
                        let _ = self
                            .main_tx
                            .send(Event::IngestionFailed {
                                article_id: commit,
                                error: format!("Failed to fetch from {}: {}", url, e),
                            })
                            .await;
                    }
                    continue;
                }
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

                    if count > 100 {
                        let _ = self
                            .main_tx
                            .send(Event::IngestionFailed {
                                article_id: range.clone(),
                                error: format!(
                                    "Git range contains {} commits, which exceeds the limit of 100",
                                    count
                                ),
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
                    info!("Successfully submitted remote range {}", range);
                } else {
                    // Single commit
                    match self
                        .extract_patch(&commit_or_range, &commit_or_range, 1, 1)
                        .await
                    {
                        Ok(mut event) => {
                            if let Event::PatchSubmitted {
                                ref mut message_id, ..
                            } = event
                            {
                                *message_id = commit_or_range.clone();
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
                info!("Updating remote {} from {} to {}", name, current_url, url);
                Command::new("git")
                    .current_dir(&self.repo_path)
                    .args(["remote", "set-url", name, url])
                    .output()
                    .await?;
            }
        } else {
            info!("Adding remote {} -> {}", name, url);
            let output = Command::new("git")
                .current_dir(&self.repo_path)
                .args(["remote", "add", name, url])
                .output()
                .await?;

            if !output.status.success() {
                return Err(anyhow!(
                    "Failed to add remote: {}",
                    String::from_utf8_lossy(&output.stderr)
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
                String::from_utf8_lossy(&output.stderr)
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
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(())
    }

    async fn extract_patch(
        &self,
        commit: &str,
        article_id: &str,
        index: u32,
        total: u32,
    ) -> Result<Event> {
        // Format: AuthorName%nAuthorEmail%nSubject%nBody...%n---SASHIKO-END-HEADER---%nDiff...
        let format = "format:%an%n%ae%n%s%n%b%n---SASHIKO-END-HEADER---";

        let output = Command::new("git")
            .current_dir(&self.repo_path)
            .args(["show", &format!("--format={}", format), commit])
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!(
                "git show failed: {}",
                String::from_utf8_lossy(&output.stderr)
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
        let author_name = lines.next().unwrap_or("Unknown").trim();
        let author_email = lines.next().unwrap_or("unknown@localhost").trim();
        let subject = lines.next().unwrap_or("No Subject").trim();

        // Body is the rest
        let body: Vec<&str> = lines.collect();
        let message = body.join("\n").trim().to_string();

        let author = format!("{} <{}>", author_name, author_email);

        Ok(Event::PatchSubmitted {
            group: "git-fetch".to_string(),
            article_id: article_id.to_string(),
            message_id: String::new(), // Set by caller
            subject: subject.to_string(),
            author,
            message,
            diff,
            base_commit: Some(commit.to_string()), // The commit itself is the point of reference
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64,
            index,
            total,
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
}
