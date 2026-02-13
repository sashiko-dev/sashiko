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
use crate::git_ops::git_command;
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

    fn get_tracked_groups(&self) -> Vec<(String, String)> {
        self.settings
            .mailing_lists
            .track
            .iter()
            .map(|entry| {
                if let Some((name, group)) = entry.split_once(':') {
                    (name.to_string(), group.to_string())
                } else if entry.contains('.') {
                    (entry.clone(), entry.clone())
                } else {
                    // Heuristics for common lists
                    let group = match entry.as_str() {
                        "linux-mm" => "org.kvack.linux-mm".to_string(),
                        _ => format!("org.kernel.vger.{}", entry),
                    };
                    (entry.clone(), group)
                }
            })
            .collect()
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
        let count_output = git_command()
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

        let mut cmd = git_command();
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
        let msg_id = extract_message_id(raw_bytes);

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
        let groups = self.get_tracked_groups();
        if groups.is_empty() {
            return Ok(());
        }

        // Split the total limit evenly across groups (ceiling division)
        let limit_per_group = limit.div_ceil(groups.len());

        for (name, group) in groups {
            let mut group_remaining = limit_per_group;

            // Ensure the mailing list exists in the DB so messages can be linked to it
            // We use &group because ensure_mailing_list expects &str
            if let Err(e) = self.db.ensure_mailing_list(&name, &group).await {
                error!("Failed to ensure mailing list {} exists: {}", group, e);
                // Continue anyway; linking may fail if the list doesn't exist.
            }

            match self.resolve_git_info(&group).await {
                Ok((epochs, base_path)) => {
                    for (epoch, url) in epochs {
                        if group_remaining == 0 {
                            break;
                        }

                        let epoch_path = base_path.join(epoch.to_string());
                        info!(
                            "Bootstrapping group {} epoch {} from {} to {:?}",
                            group, epoch, url, epoch_path
                        );

                        if let Err(e) = self
                            .bootstrap_repo(&url, &epoch_path, group_remaining)
                            .await
                        {
                            error!("Failed to bootstrap group {} epoch {}: {}", group, epoch, e);
                            continue;
                        } else {
                            match self
                                .ingest_git_objects(&group, &epoch_path, Some(group_remaining))
                                .await
                            {
                                Ok(count) => {
                                    info!("Ingested {} messages from epoch {}", count, epoch);
                                    group_remaining = group_remaining.saturating_sub(count);
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

            let output = git_command()
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
            let remote_output = git_command()
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
                    let set_url_output = git_command()
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
            let output = git_command()
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
        let groups = self.get_tracked_groups();
        info!("Starting NNTP Ingestor for groups: {:?}", groups);

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

        for (name, group_name) in self.get_tracked_groups() {
            let group_name = &group_name;
            self.db.ensure_mailing_list(&name, group_name).await?;

            let info = client.group(group_name).await?;
            let last_known = self.db.get_last_article_num(group_name).await?;

            info!(
                "Group {}: estimated count={}, low={}, high={}, last_known={}",
                group_name, info.number, info.low, info.high, last_known
            );

            let mut current = last_known;
            if current == 0 && info.high > 0 {
                // Initialize with a safe overlap window (e.g. 4000 messages ~ 1 day for LKML)
                // This ensures we catch up if git archive is slightly stale.
                let overlap = if self.download.is_some() {
                    // If we just bootstrapped from git, we are likely close to the tip.
                    // We only need a small overlap to cover the lag between git mirror and NNTP.
                    100
                } else {
                    4000
                };
                current = info.high.saturating_sub(overlap);
                self.db.update_last_article_num(group_name, current).await?;
                info!(
                    "Initialized high-water mark to {} (overlap window: {})",
                    current, overlap
                );
            }

            // Fetch ALL pending messages
            while current < info.high {
                let next_id = current + 1;
                // info!("Fetching article {}", next_id);
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
                        current = next_id;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        // 423 is "No such article number in this group"
                        if msg.contains("423") {
                            warn!("Article {} missing (423), skipping", next_id);
                            self.db.update_last_article_num(group_name, next_id).await?;
                            current = next_id;
                        } else {
                            error!("Failed to fetch article {}: {}", next_id, e);
                            break; // Stop and retry later (transient error or connection lost)
                        }
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
        let mut rev_list_cmd = git_command();
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
        let mut cat_file_cmd = git_command();
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

pub fn split_mbox(raw: &[u8]) -> Vec<Vec<u8>> {
    let mut emails = Vec::new();
    let mut current_email = Vec::new();

    for line in raw.split_inclusive(|&b| b == b'\n') {
        if is_mbox_separator(line) {
            if !current_email.is_empty() {
                emails.push(std::mem::take(&mut current_email));
            }
            // Skip the "From " line
        } else {
            current_email.extend_from_slice(line);
        }
    }

    if !current_email.is_empty() {
        emails.push(current_email);
    }

    emails
}

pub fn is_mbox_separator(line: &[u8]) -> bool {
    if !line.starts_with(b"From ") {
        return false;
    }
    // Heuristic: Mbox separator lines (From_ lines) usually contain a timestamp.
    // We look for at least two colons (HH:MM:SS) to distinguish from
    // "From " starting a sentence in the body.
    line.iter().filter(|&&b| b == b':').count() >= 2
}

pub fn extract_message_id(raw_bytes: &[u8]) -> String {
    let raw_str = String::from_utf8_lossy(raw_bytes);
    for line in raw_str.lines() {
        if line.to_lowercase().starts_with("message-id:") {
            let val = line.split_once(':').map(|x| x.1).unwrap_or("").trim();
            // Remove brackets
            let clean = val.trim_start_matches('<').trim_end_matches('>');
            if !clean.is_empty() {
                return clean.to_string();
            }
        }
    }
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_mbox_separator() {
        assert!(is_mbox_separator(
            b"From user@example.com Mon Jan 1 00:00:00 2023\n"
        ));
        assert!(!is_mbox_separator(b"From: user@example.com\n"));
        assert!(!is_mbox_separator(b"Subject: Test\n"));
        assert!(!is_mbox_separator(b"Some body text\n"));
    }

    #[test]
    fn test_extract_message_id() {
        let email = b"Subject: Test\nMessage-ID: <12345@example.com>\n\nBody";
        assert_eq!(extract_message_id(email), "12345@example.com");

        let email_no_brackets = b"Subject: Test\nMessage-ID: 12345@example.com\n\nBody";
        assert_eq!(extract_message_id(email_no_brackets), "12345@example.com");

        let email_mixed_case = b"Subject: Test\nmessage-id: <12345@example.com>\n\nBody";
        assert_eq!(extract_message_id(email_mixed_case), "12345@example.com");

        let email_missing = b"Subject: Test\n\nBody";
        assert_eq!(extract_message_id(email_missing), "unknown");
    }

    #[test]
    fn test_split_mbox() {
        let mbox = b"From user@example.com Mon Jan 1 00:00:00 2023\n\
Subject: Patch 1\n\
Message-ID: <1@example.com>\n\
\n\
Body 1\n\
\n\
From user@example.com Mon Jan 1 00:00:01 2023\n\
Subject: Patch 2\n\
Message-ID: <2@example.com>\n\
\n\
Body 2\n";

        let messages = split_mbox(mbox);
        assert_eq!(messages.len(), 2);

        let msg1 = String::from_utf8_lossy(&messages[0]);
        assert!(msg1.contains("Subject: Patch 1"));
        assert!(msg1.contains("Body 1"));
        assert!(!msg1.contains("From user@example.com"));

        let msg2 = String::from_utf8_lossy(&messages[1]);
        assert!(msg2.contains("Subject: Patch 2"));
        assert!(msg2.contains("Body 2"));
        assert!(!msg2.contains("From user@example.com"));
    }

    #[test]
    fn test_split_mbox_single() {
        let mbox = b"From user@example.com Mon Jan 1 00:00:00 2023\n\
Subject: Patch 1\n\
Message-ID: <1@example.com>\n\
\n\
Body 1\n";

        let messages = split_mbox(mbox);
        assert_eq!(messages.len(), 1);
        let msg1 = String::from_utf8_lossy(&messages[0]);
        assert!(msg1.contains("Subject: Patch 1"));
    }

    #[test]
    fn test_split_git_format_patch() {
        let raw = b"From b99d70c0d1380f1368fd4a82271280c4fd28558b Mon Sep 17 00:00:00 2001
From: Tony Luck <tony.luck@intel.com>
Date: Wed, 25 Oct 2023 13:25:13 -0700
Subject: [PATCH 1/5] x86/cpu: Add model number for Intel Arrow Lake mobile
 processor

For \"reasons\" Intel has code-named this CPU with a \"_H\" suffix.

[ dhansen: As usual, apply this and send it upstream quickly to
	   make it easier for anyone who is doing work that
	   consumes this. ]

Signed-off-by: Tony Luck <tony.luck@intel.com>
Signed-off-by: Dave Hansen <dave.hansen@linux.intel.com>
Link: https://lore.kernel.org/all/20231025202513.12358-1-tony.luck%40intel.com
---
 arch/x86/include/asm/intel-family.h | 2 ++
 1 file changed, 2 insertions(+)

diff --git a/arch/x86/include/asm/intel-family.h b/arch/x86/include/asm/intel-family.h
index 5fcd85fd64fd..197316121f04 100644
--- a/arch/x86/include/asm/intel-family.h
+++ b/arch/x86/include/asm/intel-family.h
@@ -27,6 +27,7 @@
  *		_X	- regular server parts
  *		_D	- micro server parts
  *		_N,_P	- other mobile parts
+ *		_H	- premium mobile parts
  *		_S	- other client parts
  *
  *		Historical OPTDIFFs:
@@ -124,6 +125,7 @@
 #define INTEL_FAM6_METEORLAKE		0xAC
 #define INTEL_FAM6_METEORLAKE_L		0xAA
 
+#define INTEL_FAM6_ARROWLAKE_H		0xC5
 #define INTEL_FAM6_ARROWLAKE		0xC6
 
 #define INTEL_FAM6_LUNARLAKE_M		0xBD
-- 
2.53.0.rc2.204.g2597b5adb4-goog


From 128b0c9781c9f2651bea163cb85e52a6c7be0f9e Mon Sep 17 00:00:00 2001
From: Thomas Gleixner <tglx@linutronix.de>
Date: Wed, 25 Oct 2023 23:04:15 +0200
Subject: [PATCH 2/5] x86/i8259: Skip probing when ACPI/MADT advertises PCAT
 compatibility

David and a few others reported that on certain newer systems some legacy
interrupts fail to work correctly.

Debugging revealed that the BIOS of these systems leaves the legacy PIC in
uninitialized state which makes the PIC detection fail and the kernel
switches to a dummy implementation.

Unfortunately this fallback causes quite some code to fail as it depends on
checks for the number of legacy PIC interrupts or the availability of the
real PIC.

In theory there is no reason to use the PIC on any modern system when
IO/APIC is available, but the dependencies on the related checks cannot be
resolved trivially and on short notice. This needs lots of analysis and
rework.

The PIC detection has been added to avoid quirky checks and force selection
of the dummy implementation all over the place, especially in VM guest
scenarios. So it's not an option to revert the relevant commit as that
would break a lot of other scenarios.

One solution would be to try to initialize the PIC on detection fail and
retry the detection, but that puts the burden on everything which does not
have a PIC.

Fortunately the ACPI/MADT table header has a flag field, which advertises
in bit 0 that the system is PCAT compatible, which means it has a legacy
8259 PIC.

Evaluate that bit and if set avoid the detection routine and keep the real
PIC installed, which then gets initialized (for nothing) and makes the rest
of the code with all the dependencies work again.

Fixes: e179f6914152 (\"x86, irq, pic: Probe for legacy PIC and set legacy_pic appropriately\")
Reported-by: David Lazar <dlazar@gmail.com>
Signed-off-by: Thomas Gleixner <tglx@linutronix.de>
Tested-by: David Lazar <dlazar@gmail.com>
Reviewed-by: Hans de Goede <hdegoede@redhat.com>
Reviewed-by: Mario Limonciello <mario.limonciello@amd.com>
Cc: stable@vger.kernel.org
Closes: https://bugzilla.kernel.org/show_bug.cgi?id=218003
Link: https://lore.kernel.org/r/875y2u5s8g.ffs@tglx
---
 arch/x86/include/asm/i8259.h |  2 ++
 arch/x86/kernel/acpi/boot.c  |  3 +++
 arch/x86/kernel/i8259.c      | 38 ++++++++++++++++++++++++++++--------
 3 files changed, 35 insertions(+), 8 deletions(-)

diff --git a/arch/x86/include/asm/i8259.h b/arch/x86/include/asm/i8259.h
index 637fa1df3512..c715097e92fd 100644
--- a/arch/x86/include/asm/i8259.h
+++ b/arch/x86/include/asm/i8259.h
@@ -69,6 +69,8 @@ struct legacy_pic {
 	void (*make_irq)(unsigned int irq);
 };
 
+void legacy_pic_pcat_compat(void);
+
 extern struct legacy_pic *legacy_pic;
 extern struct legacy_pic null_legacy_pic;
 
diff --git a/arch/x86/kernel/acpi/boot.c b/arch/x86/kernel/acpi/boot.c
index 2a0ea38955df..c55c0ef47a18 100644
--- a/arch/x86/kernel/acpi/boot.c
+++ b/arch/x86/kernel/acpi/boot.c
@@ -148,6 +148,9 @@ static int __init acpi_parse_madt(struct acpi_table_header *table)
 		pr_debug(\"Local APIC address 0x%08x\\n\", madt->address);
 	}
 
+	if (madt->flags & ACPI_MADT_PCAT_COMPAT)
+		legacy_pic_pcat_compat();
+
 	/* ACPI 6.3 and newer support the online capable bit. */
 	if (acpi_gbl_FADT.header.revision > 6 ||
 	    (acpi_gbl_FADT.header.revision == 6 &&
diff --git a/arch/x86/kernel/i8259.c b/arch/x86/kernel/i8259.c
index 30a55207c000..c20d1832c481 100644
--- a/arch/x86/kernel/i8259.c
+++ b/arch/x86/kernel/i8259.c
@@ -32,6 +32,7 @@
  */
 static void init_8259A(int auto_eoi);
 
+static bool pcat_compat __ro_after_init;
 static int i8259A_auto_eoi;
 DEFINE_RAW_SPINLOCK(i8259A_lock);
 
@@ -299,15 +300,32 @@ static void unmask_8259A(void)
 
 static int probe_8259A(void)
 {
+	unsigned char new_val, probe_val = ~(1 << PIC_CASCADE_IR);
 	unsigned long flags;
-	unsigned char probe_val = ~(1 << PIC_CASCADE_IR);
-	unsigned char new_val;
+
+	/*
+	 * If MADT has the PCAT_COMPAT flag set, then do not bother probing
+	 * for the PIC. Some BIOSes leave the PIC uninitialized and probing
+	 * fails.
+	 *
+	 * Right now this causes problems as quite some code depends on
+	 * nr_legacy_irqs() > 0 or has_legacy_pic() == true. This is silly
+	 * when the system has an IO/APIC because then PIC is not required
+	 * at all, except for really old machines where the timer interrupt
+	 * must be routed through the PIC. So just pretend that the PIC is
+	 * there and let legacy_pic->init() initialize it for nothing.
+	 *
+	 * Alternatively this could just try to initialize the PIC and
+	 * repeat the probe, but for cases where there is no PIC that's
+	 * just pointless.
+	 */
+	if (pcat_compat)
+		return nr_legacy_irqs();
+
 	/*
-	 * Check to see if we have a PIC.
-	 * Mask all except the cascade and read
-	 * back the value we just wrote. If we don't
-	 * have a PIC, we will read 0xff as opposed to the
-	 * value we wrote.
+	 * Check to see if we have a PIC.  Mask all except the cascade and
+	 * read back the value we just wrote. If we don't have a PIC, we
+	 * will read 0xff as opposed to the value we wrote.
 	 */
 	raw_spin_lock_irqsave(&i8259A_lock, flags);
 
@@ -429,5 +447,9 @@ static int __init i8259A_init_ops(void)
 
 	return 0;
 }
-
 device_initcall(i8259A_init_ops);
+
+void __init legacy_pic_pcat_compat(void)
+{
+	pcat_compat = true;
+}
-- 
2.53.0.rc2.204.g2597b5adb4-goog


From bd94d86f490b70c58b3fc5739328a53ad4b18d86 Mon Sep 17 00:00:00 2001
From: Thomas Gleixner <tglx@linutronix.de>
Date: Wed, 25 Oct 2023 23:31:35 +0200
Subject: [PATCH 3/5] x86/tsc: Defer marking TSC unstable to a worker

Tetsuo reported the following lockdep splat when the TSC synchronization
fails during CPU hotplug:

   tsc: Marking TSC unstable due to check_tsc_sync_source failed

   WARNING: inconsistent lock state
   inconsistent {IN-HARDIRQ-W} -> {HARDIRQ-ON-W} usage.
   ffffffff8cfa1c78 (watchdog_lock){?.-.}-{2:2}, at: clocksource_watchdog+0x23/0x5a0
   {IN-HARDIRQ-W} state was registered at:
     _raw_spin_lock_irqsave+0x3f/0x60
     clocksource_mark_unstable+0x1b/0x90
     mark_tsc_unstable+0x41/0x50
     check_tsc_sync_source+0x14f/0x180
     sysvec_call_function_single+0x69/0x90

   Possible unsafe locking scenario:
     lock(watchdog_lock);
     <Interrupt>
       lock(watchdog_lock);

   stack backtrace:
    _raw_spin_lock+0x30/0x40
    clocksource_watchdog+0x23/0x5a0
    run_timer_softirq+0x2a/0x50
    sysvec_apic_timer_interrupt+0x6e/0x90

The reason is the recent conversion of the TSC synchronization function
during CPU hotplug on the control CPU to a SMP function call. In case
that the synchronization with the upcoming CPU fails, the TSC has to be
marked unstable via clocksource_mark_unstable().

clocksource_mark_unstable() acquires 'watchdog_lock', but that lock is
taken with interrupts enabled in the watchdog timer callback to minimize
interrupt disabled time. That's obviously a possible deadlock scenario,

Before that change the synchronization function was invoked in thread
context so this could not happen.

As it is not crucical whether the unstable marking happens slightly
delayed, defer the call to a worker thread which avoids the lock context
problem.

Fixes: 9d349d47f0e3 (\"x86/smpboot: Make TSC synchronization function call based\")
Reported-by: Tetsuo Handa <penguin-kernel@i-love.sakura.ne.jp>
Signed-off-by: Thomas Gleixner <tglx@linutronix.de>
Tested-by: Tetsuo Handa <penguin-kernel@i-love.sakura.ne.jp>
Cc: stable@vger.kernel.org
Link: https://lore.kernel.org/r/87zg064ceg.ffs@tglx
---
 arch/x86/kernel/tsc_sync.c | 10 +++++++++-
 1 file changed, 9 insertions(+), 1 deletion(-)

diff --git a/arch/x86/kernel/tsc_sync.c b/arch/x86/kernel/tsc_sync.c
index bbc440c93e08..1123ef3ccf90 100644
--- a/arch/x86/kernel/tsc_sync.c
+++ b/arch/x86/kernel/tsc_sync.c
@@ -15,6 +15,7 @@
  * ( The serial nature of the boot logic and the CPU hotplug lock
  *   protects against more than 2 CPUs entering this code. )
  */
+#include <linux/workqueue.h>
 #include <linux/topology.h>
 #include <linux/spinlock.h>
 #include <linux/kernel.h>
@@ -342,6 +343,13 @@ static inline unsigned int loop_timeout(int cpu)
 	return (cpumask_weight(topology_core_cpumask(cpu)) > 1) ? 2 : 20;
 }
 
+static void tsc_sync_mark_tsc_unstable(struct work_struct *work)
+{
+	mark_tsc_unstable(\"check_tsc_sync_source failed\");
+}
+
+static DECLARE_WORK(tsc_sync_work, tsc_sync_mark_tsc_unstable);
+
 /*
  * The freshly booted CPU initiates this via an async SMP function call.
  */
@@ -395,7 +403,7 @@ static void check_tsc_sync_source(void *__cpu)
 			\"turning off TSC clock.\\n\", max_warp);
 		if (random_warps)
 			pr_warn(\"TSC warped randomly betwe";

        let messages = split_mbox(raw);
        assert_eq!(messages.len(), 3);

        let msg1 = String::from_utf8_lossy(&messages[0]);
        assert!(msg1.contains(
            "Subject: [PATCH 1/5] x86/cpu: Add model number for Intel Arrow Lake mobile"
        ));
        assert!(msg1.contains("arch/x86/include/asm/intel-family.h | 2 ++"));

        let msg2 = String::from_utf8_lossy(&messages[1]);
        assert!(msg2.contains(
            "Subject: [PATCH 2/5] x86/i8259: Skip probing when ACPI/MADT advertises PCAT"
        ));
        assert!(
            msg2.contains("arch/x86/kernel/i8259.c      | 38 ++++++++++++++++++++++++++++--------")
        );

        let msg3 = String::from_utf8_lossy(&messages[2]);
        assert!(
            msg3.contains("Subject: [PATCH 3/5] x86/tsc: Defer marking TSC unstable to a worker")
        );
        assert!(msg3.contains("static DECLARE_WORK(tsc_sync_work, tsc_sync_mark_tsc_unstable);"));
    }

    #[test]
    fn test_extract_message_id_regression_no_brackets() {
        let raw = b"From: user\nMessage-ID: 12345@example.com\nSubject: Hi";
        assert_eq!(extract_message_id(raw), "12345@example.com");
    }
}
