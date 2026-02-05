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

use crate::db::Database;
use crate::events::Event;
use crate::nntp::NntpClient;
use crate::settings::Settings;
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::Sender;
use tokio::time::{Duration, sleep};
use tracing::{error, info, warn};

pub struct Ingestor {
    settings: Settings,
    db: Arc<Database>,
    sender: Sender<Event>,
    download: Option<usize>,
    nntp_enabled: bool,
    message_ids: Option<Vec<String>>,
    thread_ids: Option<Vec<String>>,
    git_source: Option<String>,
    baseline: Option<String>,
}

impl Ingestor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        settings: Settings,
        db: Arc<Database>,
        sender: Sender<Event>,
        download: Option<usize>,
        nntp_enabled: bool,
        message_ids: Option<Vec<String>>,
        thread_ids: Option<Vec<String>>,
        git_source: Option<String>,
        baseline: Option<String>,
    ) -> Self {
        Self {
            settings,
            db,
            sender,
            download,
            nntp_enabled,
            message_ids,
            thread_ids,
            git_source,
            baseline,
        }
    }

    pub async fn run(&self) -> Result<()> {
        let mut work_done = false;

        if let Some(msg_ids) = &self.message_ids {
            // Try to connect to NNTP for batch ingestion
            let mut nntp_client = match NntpClient::connect(
                &self.settings.nntp.server,
                self.settings.nntp.port,
            )
            .await
            {
                Ok(c) => Some(c),
                Err(e) => {
                    warn!(
                        "Failed to connect to NNTP for batch ingestion: {}. Falling back to HTTP.",
                        e
                    );
                    None
                }
            };

            for msg_id in msg_ids {
                info!("Ingesting specific message: {}", msg_id);
                let mut ingested = false;

                if let Some(client) = &mut nntp_client {
                    let nntp_id = format!("<{}>", msg_id);
                    match client.article(&nntp_id).await {
                        Ok(lines) => {
                            self.sender
                                .send(Event::ArticleFetched {
                                    group: "manual".to_string(),
                                    article_id: msg_id.clone(),
                                    content: lines,
                                    raw: None,
                                    baseline: self.baseline.clone(),
                                })
                                .await?;
                            info!("Successfully ingested message {} via NNTP", msg_id);
                            ingested = true;
                        }
                        Err(e) => {
                            warn!("NNTP fetch failed for {}: {}", msg_id, e);
                        }
                    }
                }

                if !ingested {
                    if let Err(e) = self.ingest_message_by_id(msg_id).await {
                        error!("Failed to ingest message {}: {}", msg_id, e);
                    }
                }
            }

            if let Some(mut client) = nntp_client {
                let _ = client.quit().await;
            }

            work_done = true;
        }

        if let Some(thread_ids) = &self.thread_ids {
            for thread_id in thread_ids {
                info!("Ingesting specific thread: {}", thread_id);
                if let Err(e) = self.ingest_thread_by_id(thread_id).await {
                    error!("Failed to ingest thread {}: {}", thread_id, e);
                }
            }
            work_done = true;
        }

        if let Some(git_source) = &self.git_source {
            info!("Ingesting from git source: {}", git_source);
            if let Err(e) = self.run_git_ingestion(git_source).await {
                error!("Failed to ingest from git source {}: {}", git_source, e);
            }
            work_done = true;
        }

        if work_done {
            return Ok(());
        }

        if let Some(n) = self.download {
            info!(
                "Bootstrap requested: downloading/ingesting last {} messages from git archive",
                n
            );
            if let Err(e) = self.run_git_bootstrap(n).await {
                error!("Git bootstrap failed: {}", e);
            }
        }

        if self.nntp_enabled {
            self.run_nntp().await?;
        } else {
            info!("NNTP ingestor disabled (default). Use --track to enable.");
        }

        Ok(())
    }

    async fn ingest_message_by_id(&self, msg_id: &str) -> Result<()> {
        let url = format!("https://lore.kernel.org/all/{}/raw", msg_id);
        info!("Fetching raw message from {}", url);

        let output = Command::new("curl")
            .arg("-s")
            .arg("-L") // Follow redirects
            .arg(&url)
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!(
                "Failed to fetch message (status: {}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        let content = output.stdout;
        if content.is_empty() {
            return Err(anyhow!("Empty response from {}", url));
        }

        // We use "manual" as group name for manually ingested messages
        self.sender
            .send(Event::ArticleFetched {
                group: "manual".to_string(),
                article_id: msg_id.to_string(),
                content: Vec::new(),
                raw: Some(content),
                baseline: self.baseline.clone(),
            })
            .await?;

        info!("Successfully ingested message {}", msg_id);
        Ok(())
    }

    async fn ingest_thread_by_id(&self, msg_id: &str) -> Result<()> {
        let url = format!("https://lore.kernel.org/all/{}/t.mbox.gz", msg_id);
        info!("Fetching thread mbox from {}", url);

        // curl ... | gunzip
        let mut curl_cmd = Command::new("bash");
        curl_cmd
            .arg("-c")
            .arg(format!("curl -s -L {} | gunzip", url))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = curl_cmd.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to open stdout"))?;
        let reader = BufReader::new(stdout);

        self.ingest_from_mbox_reader(
            reader,
            self.baseline.clone(),
            &format!("thread {}", msg_id),
            "lore-mbox",
        )
        .await?;

        let status = child.wait().await?;
        if !status.success() {
            warn!("curl/gunzip process exited with error");
        }

        Ok(())
    }

    async fn ingest_from_mbox_reader<R: AsyncBufRead + Unpin>(
        &self,
        mut reader: R,
        baseline: Option<String>,
        source_desc: &str,
        group: &str,
    ) -> Result<usize> {
        let mut current_email = Vec::new();
        let mut count = 0;
        let mut line = Vec::new();

        loop {
            line.clear();
            let bytes_read = reader.read_until(b'\n', &mut line).await?;
            if bytes_read == 0 {
                break;
            }

            // check if line starts with "From "
            if is_mbox_separator(&line) {
                if !current_email.is_empty() {
                    // Process previous email
                    self.process_mbox_email(&current_email, baseline.clone(), group)
                        .await?;
                    count += 1;
                    current_email.clear();
                }
                // We don't include the "From " line
            } else {
                current_email.extend_from_slice(&line);
            }
        }

        // Process last email
        if !current_email.is_empty() {
            self.process_mbox_email(&current_email, baseline, group)
                .await?;
            count += 1;
        }

        info!(
            "Successfully ingested {} messages from {}",
            count, source_desc
        );
        Ok(count)
    }

    async fn run_git_ingestion(&self, range: &str) -> Result<()> {
        let repo_path = std::path::PathBuf::from(&self.settings.git.repository_path);
        if !repo_path.exists() {
            return Err(anyhow!("Git repository not found at {:?}", repo_path));
        }

        // Determine baseline
        let baseline = if let Some(b) = &self.baseline {
            Some(b.clone())
        } else {
            // Try to deduce baseline from range
            match crate::git_ops::get_range_base(&repo_path, range).await {
                Ok(b) => {
                    info!("Deduced baseline for range {}: {}", range, b);
                    Some(b)
                }
                Err(e) => {
                    warn!("Failed to deduce baseline for range {}: {}", range, e);
                    None
                }
            }
        };

        // Calculate commit count for the range to set total_parts correctly
        let count_output = Command::new("git")
            .current_dir(&repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .arg("rev-list")
            .arg("--count")
            .arg(range)
            .output()
            .await?;

        let total_count = if count_output.status.success() {
            String::from_utf8_lossy(&count_output.stdout)
                .trim()
                .parse::<usize>()
                .unwrap_or(0)
        } else {
            warn!(
                "Failed to count commits in range {}: {}",
                range,
                String::from_utf8_lossy(&count_output.stderr)
            );
            0
        };

        info!(
            "Generating patches for range {} from {:?} (count: {})",
            range, repo_path, total_count
        );

        let mut cmd = Command::new("git");
        cmd.current_dir(&repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .arg("format-patch")
            .arg("--stdout")
            .arg("--thread")
            // .arg("--base=auto") // Optional: useful if we want base info in the patch, but we are setting it manually
            .arg(range)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to open stdout"))?;
        let reader = BufReader::new(stdout);

        self.ingest_from_mbox_reader(
            reader,
            baseline,
            &format!("git range {}", range),
            &format!("git-import:{}:{}", total_count, range),
        )
        .await?;

        let status = child.wait().await?;
        if !status.success() {
            return Err(anyhow!("git format-patch failed"));
        }

        Ok(())
    }

    async fn process_mbox_email(
        &self,
        raw_bytes: &[u8],
        baseline: Option<String>,
        group: &str,
    ) -> Result<()> {
        // We don't have the Message-ID easily unless we parse it here,
        // but we can let the main parser handle it.
        // We use a placeholder ID or try to extract it.
        // Actually, ArticleFetched expects article_id.
        // Let's try to extract Message-ID quickly.
        let raw_str = String::from_utf8_lossy(raw_bytes);
        let msg_id = raw_str
            .lines()
            .find(|l| l.to_lowercase().starts_with("message-id:"))
            .map(|l| {
                l.trim_start_matches(|c| c != '<')
                    .trim_end_matches(|c| c != '>')
            })
            .unwrap_or("unknown")
            .trim_matches(|c| c == '<' || c == '>') // Remove brackets if present
            .to_string();

        // Skip if empty (e.g. mbox artifacts)
        if raw_bytes.iter().all(|b| b.is_ascii_whitespace()) {
            return Ok(());
        }

        self.sender
            .send(Event::ArticleFetched {
                group: group.to_string(),
                article_id: msg_id,
                content: Vec::new(),
                raw: Some(raw_bytes.to_vec()),
                baseline,
            })
            .await?;
        Ok(())
    }

    async fn run_git_bootstrap(&self, limit: usize) -> Result<()> {
        let mut remaining = limit;

        for group in &self.settings.nntp.groups {
            // Ensure the mailing list exists in the DB so messages can be linked to it
            if let Err(e) = self.db.ensure_mailing_list(group, group).await {
                error!("Failed to ensure mailing list {} exists: {}", group, e);
                // Continue anyway, maybe it exists? Or we just fail linking.
            }

            match self.resolve_git_info(group).await {
                Ok((epochs, base_path)) => {
                    for (epoch, url) in epochs {
                        if remaining == 0 {
                            break;
                        }

                        let epoch_path = base_path.join(epoch.to_string());
                        info!(
                            "Bootstrapping group {} epoch {} from {} to {:?}",
                            group, epoch, url, epoch_path
                        );

                        if let Err(e) = self.bootstrap_repo(&url, &epoch_path, remaining).await {
                            error!("Failed to bootstrap group {} epoch {}: {}", group, epoch, e);
                            continue;
                        } else {
                            match self
                                .ingest_git_objects(group, &epoch_path, Some(remaining))
                                .await
                            {
                                Ok(count) => {
                                    info!("Ingested {} messages from epoch {}", count, epoch);
                                    remaining = remaining.saturating_sub(count);
                                }
                                Err(e) => {
                                    error!(
                                        "Failed to ingest objects for group {} epoch {}: {}",
                                        group, epoch, e
                                    );
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to resolve git info for group {}: {}", group, e);
                }
            }
        }
        Ok(())
    }

    async fn resolve_git_info(
        &self,
        group: &str,
    ) -> Result<(Vec<(i32, String)>, std::path::PathBuf)> {
        // Dynamic path: archives/<group_name>
        let path = std::path::PathBuf::from("archives").join(group);

        // Dynamic URL heuristic
        // org.kernel.vger.linux-kernel -> lkml
        // org.kernel.vger.netdev -> netdev
        // etc.
        let list_id = if group == "org.kernel.vger.linux-kernel" {
            "lkml"
        } else {
            group.split('.').next_back().unwrap_or(group)
        };

        let epochs = self.find_epoch_urls(list_id).await?;

        Ok((epochs, path))
    }

    async fn find_epoch_urls(&self, list_id: &str) -> Result<Vec<(i32, String)>> {
        info!("Fetching manifest to find epochs for {}", list_id);

        let output = Command::new("bash")
            .arg("-c")
            .arg("curl -s https://lore.kernel.org/manifest.js.gz | gunzip")
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!("Failed to fetch manifest"));
        }

        let json: Value = serde_json::from_slice(&output.stdout)?;
        let map = json
            .as_object()
            .ok_or_else(|| anyhow!("Manifest is not a JSON object"))?;

        let mut epochs = Vec::new();
        let prefix = format!("/{}/git/", list_id);

        for (key, _val) in map {
            if key.starts_with(&prefix) && key.ends_with(".git") {
                let suffix = &key[prefix.len()..key.len() - 4];
                if let Ok(epoch) = suffix.parse::<i32>() {
                    epochs.push((epoch, format!("https://lore.kernel.org{}", key)));
                }
            }
        }

        epochs.sort_by(|a, b| b.0.cmp(&a.0)); // Descending order

        if epochs.is_empty() {
            warn!(
                "Could not find any epochs for {}, defaulting to 0.git",
                list_id
            );
            epochs.push((0, format!("https://lore.kernel.org/{}/0.git", list_id)));
        }

        info!("Found {} epochs for {}", epochs.len(), list_id);
        Ok(epochs)
    }

    async fn bootstrap_repo(&self, url: &str, path: &std::path::Path, n: usize) -> Result<()> {
        // 1. Ensure repo exists
        if !path.exists() {
            info!(
                "Cloning archive from {} to {:?} with depth {}",
                url, path, n
            );
            // Parent directory must exist
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            let output = Command::new("git")
                .arg("clone")
                .arg("--bare")
                .arg(format!("--depth={}", n))
                .arg(url)
                .arg(path)
                .output()
                .await?;

            if !output.status.success() {
                return Err(anyhow!(
                    "Git clone failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
        } else {
            // Repo exists, ensure remote is correct then fetch
            let remote_output = Command::new("git")
                .arg("-c")
                .arg("safe.bareRepository=all")
                .current_dir(path)
                .arg("remote")
                .arg("get-url")
                .arg("origin")
                .output()
                .await?;

            if remote_output.status.success() {
                let current_url = String::from_utf8_lossy(&remote_output.stdout)
                    .trim()
                    .to_string();
                if current_url != url {
                    info!("Updating remote origin from {} to {}", current_url, url);
                    let set_url_output = Command::new("git")
                        .arg("-c")
                        .arg("safe.bareRepository=all")
                        .current_dir(path)
                        .arg("remote")
                        .arg("set-url")
                        .arg("origin")
                        .arg(url)
                        .output()
                        .await?;

                    if !set_url_output.status.success() {
                        warn!(
                            "Failed to update remote url: {}",
                            String::from_utf8_lossy(&set_url_output.stderr)
                        );
                    }
                }
            }

            info!("Fetching latest changes in {:?} with depth {}", path, n);
            let output = Command::new("git")
                .arg("-c")
                .arg("safe.bareRepository=all")
                .current_dir(path)
                .arg("fetch")
                .arg(format!("--depth={}", n))
                .arg("origin")
                .arg("+refs/heads/*:refs/heads/*") // Fetch all heads
                .output()
                .await?;

            if !output.status.success() {
                // Warn but continue, maybe we are offline
                warn!(
                    "Git fetch failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
        Ok(())
    }
    async fn run_nntp(&self) -> Result<()> {
        info!(
            "Starting NNTP Ingestor for groups: {:?}",
            self.settings.nntp.groups
        );

        loop {
            if let Err(e) = self.process_nntp_cycle().await {
                error!("NNTP Ingestion cycle failed: {}", e);
            }
            sleep(Duration::from_secs(60)).await;
        }
    }

    async fn process_nntp_cycle(&self) -> Result<()> {
        let mut client =
            NntpClient::connect(&self.settings.nntp.server, self.settings.nntp.port).await?;

        for group_name in &self.settings.nntp.groups {
            self.db.ensure_mailing_list(group_name, group_name).await?;

            let info = client.group(group_name).await?;
            let last_known = self.db.get_last_article_num(group_name).await?;

            info!(
                "Group {}: estimated count={}, low={}, high={}, last_known={}",
                group_name, info.number, info.low, info.high, last_known
            );

            let mut current = last_known;
            if current == 0 && info.high > 0 {
                // If we have bootstrapped from git, we might want to be smarter here.
                // But git archives don't easily map to NNTP article numbers unless we query the Message-ID.
                // For now, if we are fresh, start from high - 5 (as per original logic).
                // TODO: Sync git message-ids to NNTP article numbers if possible?
                current = info.high.saturating_sub(5);
                self.db.update_last_article_num(group_name, current).await?;
                info!("Initialized high-water mark to {}", current);
            }

            if current < info.high {
                let next_id = current + 1;
                info!("Fetching article {}", next_id);
                match client.article(&next_id.to_string()).await {
                    Ok(lines) => {
                        self.sender
                            .send(Event::ArticleFetched {
                                group: group_name.clone(),
                                article_id: next_id.to_string(),
                                content: lines,
                                raw: None,
                                baseline: None,
                            })
                            .await?;
                        self.db.update_last_article_num(group_name, next_id).await?;
                        info!("Updated high-water mark to {}", next_id);
                    }
                    Err(e) => {
                        error!("Failed to fetch article {}: {}", next_id, e);
                    }
                }
            }
        }

        client.quit().await?;
        Ok(())
    }

    async fn ingest_git_objects(
        &self,
        group_name: &str,
        path: &std::path::Path,
        limit: Option<usize>,
    ) -> Result<usize> {
        info!("Starting Git Ingestion from {:?}", path);

        // 1. Start git rev-list (Producer)
        info!("Starting object enumeration...");
        let mut rev_list_cmd = Command::new("git");
        rev_list_cmd
            .arg("-c")
            .arg("safe.bareRepository=all")
            .current_dir(path)
            .arg("rev-list")
            .arg("--all")
            .arg("--objects");

        if let Some(n) = limit {
            rev_list_cmd.arg(format!("--max-count={}", n));
        }

        // IMPORTANT: kill_on_drop ensure process is killed if the future is cancelled (Ctrl-C)
        rev_list_cmd.kill_on_drop(true);
        rev_list_cmd.stdout(Stdio::piped());

        let mut rev_list_child = rev_list_cmd.spawn()?;
        let rev_list_stdout = rev_list_child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to open stdout for git rev-list"))?;
        let mut rev_list_reader = BufReader::new(rev_list_stdout).lines();

        // 2. Start git cat-file --batch (Consumer)
        let mut cat_file_cmd = Command::new("git");
        cat_file_cmd
            .arg("-c")
            .arg("safe.bareRepository=all")
            .current_dir(path)
            .arg("cat-file")
            .arg("--batch")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true); // Ensure cleanup

        let mut cat_file_child = cat_file_cmd.spawn()?;
        let mut cat_stdin = cat_file_child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to open stdin for git cat-file"))?;
        let cat_stdout = cat_file_child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to open stdout for git cat-file"))?;
        let mut cat_reader = BufReader::new(cat_stdout);

        let mut count = 0;
        let mut processed_blobs = 0;

        // 3. Stream: rev-list -> cat-file -> application
        while let Ok(Some(line)) = rev_list_reader.next_line().await {
            let hash = line
                .split_whitespace()
                .next()
                .ok_or_else(|| anyhow!("Invalid rev-list output: {}", line))?;

            // Write SHA to cat-file
            cat_stdin
                .write_all(format!("{}\n", hash).as_bytes())
                .await?;
            cat_stdin.flush().await?;

            // Read header: <sha> <type> <size>
            let mut header = String::new();
            if cat_reader.read_line(&mut header).await? == 0 {
                break; // Unexpected EOF from cat-file
            }

            let parts: Vec<&str> = header.split_whitespace().collect();
            if parts.len() < 3 {
                warn!("Invalid batch header for {}: {}", hash, header);
                continue;
            }

            let obj_type = parts[1];
            let size: usize = parts[2].parse().unwrap_or(0);

            // Read content + newline
            let mut content = vec![0u8; size];
            cat_reader.read_exact(&mut content).await?;

            // Consume the trailing newline that --batch outputs
            let mut newline = [0u8; 1];
            cat_reader.read_exact(&mut newline).await?;

            if obj_type == "blob" {
                // We provide raw content, so 'content' field is ignored by the parser.
                // We pass an empty vector to avoid expensive UTF-8 validation and allocation.
                self.sender
                    .send(Event::ArticleFetched {
                        group: group_name.to_string(),
                        article_id: hash.to_string(),
                        content: Vec::new(),
                        raw: Some(content),
                        baseline: None,
                    })
                    .await?;

                processed_blobs += 1;
                if processed_blobs % 1000 == 0 {
                    info!("Processed {} blobs", processed_blobs);
                }
            }

            count += 1;
        }

        info!(
            "Git ingestion completed. Scanned {} objects, processed {} blobs.",
            count, processed_blobs
        );
        Ok(processed_blobs)
    }
}

fn is_mbox_separator(line: &[u8]) -> bool {
    if !line.starts_with(b"From ") {
        return false;
    }
    // Heuristic: Mbox separator lines (From_ lines) usually contain a timestamp.
    // We look for at least two colons (HH:MM:SS) to distinguish from
    // "From " starting a sentence in the body.
    line.iter().filter(|&&b| b == b':').count() >= 2
}
