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
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio::time::{Duration, interval, timeout};
use tracing::{error, info, warn};

const GIT_FETCH_TIMEOUT: Duration = Duration::from_secs(300);

/// Compute the ingestion `article_id` matching a remote fetch placeholder.
pub fn fetch_article_id(commit_or_range: &str, mr_number: Option<i64>) -> String {
    match mr_number {
        Some(number) => format!("mr-{}-{}", number, commit_or_range),
        None => commit_or_range.to_string(),
    }
}

/// Return true if a repository URL embeds HTTP credentials or secret query tokens
/// that must never be persisted to disk in `fetch_queue`.
pub fn url_has_embedded_credentials(url: Option<&str>) -> bool {
    let Some(u) = url else {
        return false;
    };
    if redact_secret(u) != u {
        return true;
    }
    url::Url::parse(u).is_ok_and(|parsed| {
        matches!(parsed.scheme(), "http" | "https")
            && (!parsed.username().is_empty() || parsed.password().is_some())
    })
}

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
    rx: Mutex<mpsc::Receiver<FetchRequest>>,
    main_tx: mpsc::Sender<Event>,
    #[allow(clippy::type_complexity)]
    mr_metadata: std::sync::Mutex<HashMap<String, (Option<String>, Option<String>, Option<i64>)>>,
    gitlab_token: Option<String>,
    db: Option<(Arc<Database>, u32)>,
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
                rx: Mutex::new(rx),
                main_tx,
                mr_metadata: std::sync::Mutex::new(HashMap::new()),
                gitlab_token,
                db: None,
            },
            tx,
        )
    }

    pub fn with_db(mut self, db: Arc<Database>, max_retries: u32) -> Self {
        self.db = Some((db, max_retries));
        self
    }

    fn record_in_memory_request(
        &self,
        queue: &mut HashMap<Option<String>, HashSet<String>>,
        req: FetchRequest,
    ) {
        let mut mr_metadata = self.mr_metadata.lock().unwrap_or_else(|e| e.into_inner());
        if req.mr_url.is_some() || req.mr_title.is_some() || req.mr_number.is_some() {
            mr_metadata.insert(
                req.commit_hash.clone(),
                (req.mr_url.clone(), req.mr_title.clone(), req.mr_number),
            );
        } else {
            mr_metadata.remove(&req.commit_hash);
        }
        drop(mr_metadata);
        queue
            .entry(req.repo_url)
            .or_default()
            .insert(req.commit_hash);
    }

    async fn flush_in_memory_channel(
        &self,
        rx: &mut mpsc::Receiver<FetchRequest>,
        queue: &mut HashMap<Option<String>, HashSet<String>>,
    ) {
        while let Ok(next_req) = rx.try_recv() {
            if url_has_embedded_credentials(next_req.repo_url.as_deref()) {
                self.record_in_memory_request(queue, next_req);
            }
        }
        if !queue.is_empty() {
            self.process_queue(queue).await;
            self.mr_metadata
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }
    }

    pub async fn run(&self) {
        info!("FetchAgent started");
        let mut rx = self.rx.lock().await;
        let mut queue: HashMap<Option<String>, HashSet<String>> = HashMap::new();

        if let Some((ref db, max_retries)) = self.db {
            self.drain_db_queue_with_channel(db, max_retries, Some((&mut rx, &mut queue)))
                .await;
            let mut ticker = interval(Duration::from_secs(5));

            loop {
                tokio::select! {
                    maybe_req = rx.recv() => {
                        let Some(first_req) = maybe_req else {
                            break;
                        };
                        if url_has_embedded_credentials(first_req.repo_url.as_deref()) {
                            self.record_in_memory_request(&mut queue, first_req);
                        }
                        self.flush_in_memory_channel(&mut rx, &mut queue).await;
                        self.drain_db_queue_with_channel(
                            db,
                            max_retries,
                            Some((&mut rx, &mut queue)),
                        )
                        .await;
                    }
                    _ = ticker.tick() => {
                        self.flush_in_memory_channel(&mut rx, &mut queue).await;
                        self.drain_db_queue_with_channel(
                            db,
                            max_retries,
                            Some((&mut rx, &mut queue)),
                        )
                        .await;
                    }
                }
            }
            return;
        }

        let mut ticker = interval(Duration::from_secs(10));
        loop {
            tokio::select! {
                Some(req) = rx.recv() => {
                    self.record_in_memory_request(&mut queue, req);
                }
                _ = ticker.tick() => {
                    if !queue.is_empty() {
                        self.process_queue(&mut queue).await;
                        self.mr_metadata
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clear();
                    }
                }
            }
        }
    }

    pub async fn drain_db_queue(&self, db: &Database, max_retries: u32) {
        self.drain_db_queue_with_channel(db, max_retries, None)
            .await;
    }

    #[allow(clippy::type_complexity)]
    async fn drain_db_queue_with_channel(
        &self,
        db: &Database,
        max_retries: u32,
        mut in_memory: Option<(
            &mut mpsc::Receiver<FetchRequest>,
            &mut HashMap<Option<String>, HashSet<String>>,
        )>,
    ) {
        if let Err(e) = db.sweep_stale_fetch_queue().await {
            error!("Failed to sweep stale fetch_queue entries: {}", e);
        }

        loop {
            if let Some((ref mut rx, ref mut queue)) = in_memory {
                self.flush_in_memory_channel(rx, queue).await;
            }
            match db.lock_pending_fetch_queue().await {
                Ok(Some(row)) => {
                    self.process_fetch_queue_row(db, row, max_retries).await;
                }
                Ok(None) => break,
                Err(e) => {
                    error!("Failed to lock pending fetch_queue entry: {}", e);
                    break;
                }
            }
        }
    }

    fn backoff_seconds(retry_count: i64) -> i64 {
        match retry_count {
            0 => 5,
            1 => 30,
            _ => 180,
        }
    }

    async fn handle_db_fetch_error(
        &self,
        db: &Database,
        row: &FetchQueueRow,
        max_retries: u32,
        err_msg: &str,
    ) {
        let cap = max_retries.max(1) as i64;
        if row.retry_count + 1 >= cap {
            self.fail_db_fetch_permanently(db, row, err_msg).await;
        } else {
            let delay = Self::backoff_seconds(row.retry_count);
            let retry_at = chrono::Utc::now().timestamp() + delay;
            warn!(
                "Transient fetch failure for {} (attempt {}/{}), retrying in {}s: {}",
                row.article_id,
                row.retry_count + 1,
                cap,
                delay,
                err_msg
            );
            if let Err(e) = db
                .set_fetch_queue_retry_at(row.id, row.locked_at, retry_at, err_msg)
                .await
            {
                error!("Failed to schedule retry for fetch_queue {}: {}", row.id, e);
            }
        }
    }

    async fn fail_db_fetch_permanently(&self, db: &Database, row: &FetchQueueRow, err_msg: &str) {
        let redacted = redact_secret(err_msg);
        match db
            .mark_fetch_queue_failed(row.id, row.locked_at, &redacted)
            .await
        {
            Ok(true) => {
                let _ = self
                    .main_tx
                    .send(Event::IngestionFailed {
                        article_id: row.article_id.clone(),
                        error: redacted,
                        source: MessageSource::GitFetch,
                    })
                    .await;
            }
            Ok(false) => {
                warn!(
                    "Fetch queue entry {} lease was already superseded; suppressing IngestionFailed",
                    row.id
                );
            }
            Err(e) => {
                error!("Failed to mark fetch_queue {} as Failed: {}", row.id, e);
            }
        }
    }

    fn collect_endpoints(commit_or_range: &str) -> Vec<String> {
        let mut endpoints = Vec::new();
        if commit_or_range.contains("..") {
            let parts: Vec<&str> = commit_or_range.split("..").collect();
            if parts.len() == 2 {
                let base = parts[0];
                let head = parts[1].trim_start_matches('.');
                if !base.is_empty() {
                    endpoints.push(base.to_string());
                }
                if !head.is_empty() {
                    endpoints.push(head.to_string());
                }
            }
        } else {
            endpoints.push(commit_or_range.to_string());
        }
        endpoints
    }

    async fn is_local_repo_url(&self, url: &str) -> bool {
        let url_path = PathBuf::from(url);
        let repo_path = self.repo_path.clone();
        tokio::task::spawn_blocking(move || {
            match (
                std::fs::canonicalize(&url_path),
                std::fs::canonicalize(&repo_path),
            ) {
                (Ok(canon_url), Ok(canon_repo)) => canon_url == canon_repo,
                _ => false,
            }
        })
        .await
        .unwrap_or(false)
    }

    async fn process_fetch_queue_row(&self, db: &Database, row: FetchQueueRow, max_retries: u32) {
        let commits_to_check = Self::collect_endpoints(&row.commit_range);
        let mut missing_commits = Vec::new();
        for commit in &commits_to_check {
            if !self.is_present(commit).await {
                missing_commits.push(commit.clone());
            }
        }

        if !missing_commits.is_empty() {
            if row.repo_url.is_empty() || self.is_local_repo_url(&row.repo_url).await {
                let err_msg = format!(
                    "Local repository missing commits for {}: {:?}",
                    row.article_id, missing_commits
                );
                warn!("{}", err_msg);
                self.fail_db_fetch_permanently(db, &row, &err_msg).await;
                return;
            }

            let url_display = redact_secret(&row.repo_url);
            let remote_name = self.get_remote_name(&row.repo_url);

            if let Err(e) = self.ensure_remote(&remote_name, &row.repo_url).await {
                let err_msg = format!(
                    "Failed to set up remote {}: {}",
                    url_display,
                    redact_secret(&e.to_string())
                );
                error!("{}", err_msg);
                self.handle_db_fetch_error(db, &row, max_retries, &err_msg)
                    .await;
                return;
            }

            if let Err(e) = self.fetch_commits(&remote_name, &missing_commits).await {
                warn!(
                    "Optimistic fetch failed for {}: {}. Falling back to full fetch.",
                    url_display,
                    redact_secret(&e.to_string())
                );
                if let Err(e) = self.fetch_all(&remote_name).await {
                    let err_msg = format!(
                        "Failed to fetch from {}: {}",
                        url_display,
                        redact_secret(&e.to_string())
                    );
                    error!("{}", err_msg);
                    self.handle_db_fetch_error(db, &row, max_retries, &err_msg)
                        .await;
                    return;
                }
            }
        }

        match db.has_fetch_queue_lease(row.id, row.locked_at).await {
            Ok(true) => {}
            Ok(false) => {
                warn!(
                    "Fetch queue entry {} lost its lease during git fetch; skipping submission",
                    row.id
                );
                return;
            }
            Err(e) => {
                let err_msg = format!(
                    "Failed to verify fetch_queue lease for {}: {}",
                    row.article_id,
                    redact_secret(&e.to_string())
                );
                error!("{}", err_msg);
                self.handle_db_fetch_error(db, &row, max_retries, &err_msg)
                    .await;
                return;
            }
        }

        let events = if row.commit_range.contains("..") {
            match self
                .extract_range_events(
                    &row.commit_range,
                    &row.article_id,
                    row.mr_url.clone(),
                    row.mr_title.clone(),
                    row.mr_number,
                )
                .await
            {
                Ok(events) => events,
                Err(err_msg) => {
                    self.fail_db_fetch_permanently(db, &row, &err_msg).await;
                    return;
                }
            }
        } else {
            match self
                .extract_single_commit_event(
                    &row.commit_range,
                    &row.article_id,
                    row.mr_url.clone(),
                    row.mr_title.clone(),
                    row.mr_number,
                )
                .await
            {
                Ok(event) => vec![event],
                Err(err_msg) => {
                    self.fail_db_fetch_permanently(db, &row, &err_msg).await;
                    return;
                }
            }
        };

        match db.mark_fetch_queue_done(row.id, row.locked_at).await {
            Ok(true) => {}
            Ok(false) => {
                warn!(
                    "Fetch queue entry {} lost its lease before submission; dropping extracted events",
                    row.id
                );
                return;
            }
            Err(e) => {
                let err_msg = format!(
                    "Failed to mark fetch_queue {} as Done: {}",
                    row.id,
                    redact_secret(&e.to_string())
                );
                error!("{}", err_msg);
                self.handle_db_fetch_error(db, &row, max_retries, &err_msg)
                    .await;
                return;
            }
        }

        for event in events {
            if let Err(e) = self.main_tx.send(event).await {
                error!("Failed to send PatchSubmitted event: {}", e);
                return;
            }
        }

        info!("Successfully submitted fetched request {}", row.article_id);
    }

    pub async fn fetch_and_submit_batch(&mut self, repo_url: Option<String>, commits: Vec<String>) {
        let mut queue: HashMap<Option<String>, HashSet<String>> = HashMap::new();
        queue.entry(repo_url).or_default().extend(commits);
        self.process_queue(&mut queue).await;
    }

    fn article_id_for(&self, commit_or_range: &str) -> String {
        let mr_number = self
            .mr_metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(commit_or_range)
            .and_then(|(_, _, mr_number)| *mr_number);
        fetch_article_id(commit_or_range, mr_number)
    }

    async fn send_ingestion_failed(&self, article_id: String, error: String) {
        let _ = self
            .main_tx
            .send(Event::IngestionFailed {
                article_id,
                error,
                source: MessageSource::GitFetch,
            })
            .await;
    }

    async fn process_queue(&self, queue: &mut HashMap<Option<String>, HashSet<String>>) {
        info!("Processing fetch queue with {} repos", queue.len());

        for (url_opt, commits) in queue.drain() {
            if commits.is_empty() {
                continue;
            }

            let commit_list: Vec<String> = commits.into_iter().collect();
            let url_display = url_opt
                .as_deref()
                .map(redact_secret)
                .unwrap_or_else(|| "local".to_string());

            info!(
                "Processing {} commits for remote {}",
                commit_list.len(),
                url_display
            );

            // For ranges (base..head), check both endpoints individually while
            // tracking which queued items depend on a remote fetch.
            let mut missing_commits = Vec::new();
            let mut missing_seen = HashSet::new();
            let mut items_needing_fetch = HashSet::new();
            for commit_or_range in &commit_list {
                let mut endpoints = Vec::new();
                if commit_or_range.contains("..") {
                    let parts: Vec<&str> = commit_or_range.split("..").collect();
                    if parts.len() == 2 {
                        let base = parts[0];
                        let head = parts[1].trim_start_matches('.');
                        if !base.is_empty() {
                            endpoints.push(base);
                        }
                        if !head.is_empty() {
                            endpoints.push(head);
                        }
                    }
                } else {
                    endpoints.push(commit_or_range.as_str());
                }

                for endpoint in endpoints {
                    if !self.is_present(endpoint).await {
                        items_needing_fetch.insert(commit_or_range.clone());
                        if missing_seen.insert(endpoint.to_string()) {
                            missing_commits.push(endpoint.to_string());
                        }
                    }
                }
            }

            let mut fetch_failed = false;

            if missing_commits.is_empty() {
                info!(
                    "All commits present locally, skipping fetch for {}",
                    url_display
                );
            } else if let Some(url) = url_opt {
                // Remote fetch logic
                let remote_name = self.get_remote_name(&url);

                // Check if repo is local (same as self.repo_path)
                let is_local = self.is_local_repo_url(&url).await;

                if is_local {
                    warn!(
                        "Repository is local but commits are missing: {:?}. Cannot fetch.",
                        missing_commits
                    );
                    // Do not continue here; let it fall through to Step 3 where it will fail individually
                } else if let Err(e) = self.ensure_remote(&remote_name, &url).await {
                    let err_msg = redact_secret(&e.to_string());
                    error!("Failed to ensure remote {}: {}", url_display, err_msg);
                    for item in &commit_list {
                        if items_needing_fetch.contains(item) {
                            self.send_ingestion_failed(
                                self.article_id_for(item),
                                format!("Failed to set up remote {}: {}", url_display, err_msg),
                            )
                            .await;
                        }
                    }
                    fetch_failed = true;
                } else if let Err(e) = self.fetch_commits(&remote_name, &missing_commits).await {
                    // 1. Optimistic fetch failed; fall back to fetching all heads.
                    warn!(
                        "Optimistic fetch failed for {}: {}. Falling back to full fetch.",
                        url_display,
                        redact_secret(&e.to_string())
                    );
                    if let Err(e) = self.fetch_all(&remote_name).await {
                        let err_msg = redact_secret(&e.to_string());
                        error!("Full fetch failed for {}: {}", url_display, err_msg);
                        for item in &commit_list {
                            if items_needing_fetch.contains(item) {
                                self.send_ingestion_failed(
                                    self.article_id_for(item),
                                    format!("Failed to fetch from {}: {}", url_display, err_msg),
                                )
                                .await;
                            }
                        }
                        fetch_failed = true;
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
                if fetch_failed && items_needing_fetch.contains(&commit_or_range) {
                    continue;
                }
                if commit_or_range.contains("..") {
                    self.process_range_item(&commit_or_range).await;
                } else {
                    self.process_single_commit_item(&commit_or_range).await;
                }
            }
        }
    }

    async fn extract_range_events(
        &self,
        range: &str,
        article_id: &str,
        mr_url: Option<String>,
        mr_title: Option<String>,
        mr_number: Option<i64>,
    ) -> std::result::Result<Vec<Event>, String> {
        let shas = match crate::git_ops::resolve_git_range(&self.repo_path, range).await {
            Ok(shas) => shas,
            Err(e) => {
                return Err(format!(
                    "Failed to resolve git range: {}",
                    redact_secret(&e.to_string())
                ));
            }
        };

        let mut extracted = Vec::with_capacity(shas.len());
        for sha in &shas {
            match crate::git_ops::extract_patch_metadata(&self.repo_path, sha).await {
                Ok(meta) => extracted.push((sha.clone(), meta)),
                Err(e) => {
                    let redacted = redact_secret(&e.to_string());
                    error!(
                        "Failed to extract patch {} from range {}: {}",
                        sha, range, redacted
                    );
                    return Err(format!(
                        "Failed to extract patch {} from range {}: {}",
                        sha, range, redacted
                    ));
                }
            }
        }

        let has_real_commits = extracted.iter().any(|(_, meta)| {
            crate::patch::parse_b4_submit_tracking(&meta.subject, &meta.message, &meta.diff)
                .is_none()
        });

        let mut tracker_cover: Option<(
            crate::patch::B4TrackerCommit,
            crate::git_ops::PatchMetadata,
        )> = None;
        let mut leading_tracker_base: Option<String> = None;
        let mut real_patches = Vec::with_capacity(extracted.len());

        if has_real_commits {
            for (sha, meta) in extracted {
                if let Some(t) =
                    crate::patch::parse_b4_submit_tracking(&meta.subject, &meta.message, &meta.diff)
                {
                    if real_patches.is_empty() && leading_tracker_base.is_none() {
                        leading_tracker_base = meta.base_commit.clone();
                    }
                    if tracker_cover.is_none() {
                        tracker_cover = Some((t, meta));
                    }
                } else {
                    real_patches.push((sha, meta));
                }
            }
            if let Some(ref base) = leading_tracker_base
                && let Some((_, first_meta)) = real_patches.first_mut()
            {
                first_meta.base_commit = Some(base.clone());
            }
        } else {
            real_patches = extracted;
        }

        if real_patches.is_empty() {
            return Err(format!("No commits found in range {}", range));
        }

        let real_count = real_patches.len() as u32;
        let mut events = Vec::with_capacity(real_patches.len() + 1);

        if let Some((tracker_info, tracker_meta)) = tracker_cover {
            if let Some(ref cid) = tracker_info.change_id
                && let Some((_, first_meta)) = real_patches.first_mut()
                && !first_meta
                    .message
                    .to_ascii_lowercase()
                    .contains("change-id:")
            {
                if !first_meta.message.is_empty() && !first_meta.message.ends_with('\n') {
                    first_meta.message.push('\n');
                }
                first_meta
                    .message
                    .push_str(&format!("\nchange-id: {}", cid));
            }

            if tracker_info.edited_subject.is_some() || tracker_info.edited_body.is_some() {
                let mut cover_body = tracker_info.edited_body.unwrap_or_default();
                if !cover_body.is_empty()
                    && let Some(ref cid) = tracker_info.change_id
                    && !cover_body.to_ascii_lowercase().contains("change-id:")
                {
                    cover_body.push_str(&format!("\n\nchange-id: {}", cid));
                }
                let cover_subject = tracker_info
                    .edited_subject
                    .or_else(|| mr_title.clone())
                    .or_else(|| real_patches.first().map(|(_, p)| p.subject.clone()))
                    .unwrap_or(tracker_meta.subject);
                events.push(Event::PatchSubmitted {
                    group: "git-fetch".to_string(),
                    article_id: article_id.to_string(),
                    message_id: format!("{}@sashiko.local", article_id),
                    subject: cover_subject,
                    author: tracker_meta.author,
                    message: cover_body,
                    diff: String::new(),
                    base_commit: leading_tracker_base.or(tracker_meta.base_commit),
                    timestamp: tracker_meta.timestamp,
                    index: 0,
                    total: real_count,
                    mr_url: mr_url.clone(),
                    mr_title: mr_title.clone(),
                    mr_number,
                });
            }
        }

        for (i, (sha, meta)) in real_patches.into_iter().enumerate() {
            events.push(Event::PatchSubmitted {
                group: "git-fetch".to_string(),
                article_id: article_id.to_string(),
                message_id: sha,
                subject: meta.subject,
                author: meta.author,
                message: meta.message,
                diff: meta.diff,
                base_commit: meta.base_commit,
                timestamp: meta.timestamp,
                index: (i + 1) as u32,
                total: real_count,
                mr_url: mr_url.clone(),
                mr_title: mr_title.clone(),
                mr_number,
            });
        }

        Ok(events)
    }

    async fn extract_single_commit_event(
        &self,
        commit: &str,
        article_id: &str,
        mr_url: Option<String>,
        mr_title: Option<String>,
        mr_number: Option<i64>,
    ) -> std::result::Result<Event, String> {
        let full_sha = match self.resolve_sha(commit).await {
            Ok(sha) => sha,
            Err(e) => {
                return Err(format!(
                    "Failed to resolve SHA: {}",
                    redact_secret(&e.to_string())
                ));
            }
        };

        match self
            .extract_patch(
                &full_sha,
                article_id,
                1,
                1,
                mr_url.as_ref(),
                mr_title.as_ref(),
                mr_number,
            )
            .await
        {
            Ok(mut event) => {
                if let Event::PatchSubmitted {
                    ref mut message_id, ..
                } = event
                {
                    *message_id = full_sha;
                }
                Ok(event)
            }
            Err(e) => {
                let err_msg = redact_secret(&e.to_string());
                error!("Failed to extract patch {}: {}", commit, err_msg);
                Err(format!("Failed to extract patch: {}", err_msg))
            }
        }
    }

    async fn process_range_item(&self, range: &str) {
        let (mr_url, mr_title, mr_number) = self
            .mr_metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(range)
            .cloned()
            .unwrap_or((None, None, None));
        let article_id = fetch_article_id(range, mr_number);

        match self
            .extract_range_events(range, &article_id, mr_url, mr_title, mr_number)
            .await
        {
            Ok(events) => {
                for event in events {
                    if let Err(e) = self.main_tx.send(event).await {
                        error!("Failed to send PatchSubmitted event: {}", e);
                    }
                }
                info!("Successfully submitted remote range {}", range);
            }
            Err(err_msg) => {
                self.send_ingestion_failed(article_id, err_msg).await;
            }
        }
    }

    async fn process_single_commit_item(&self, commit: &str) {
        let (mr_url, mr_title, mr_number) = self
            .mr_metadata
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(commit)
            .cloned()
            .unwrap_or((None, None, None));
        let article_id = fetch_article_id(commit, mr_number);

        match self
            .extract_single_commit_event(commit, &article_id, mr_url, mr_title, mr_number)
            .await
        {
            Ok(event) => {
                if let Err(e) = self.main_tx.send(event).await {
                    error!("Failed to send PatchSubmitted event: {}", e);
                } else {
                    info!("Successfully submitted remote patch {}", commit);
                }
            }
            Err(err_msg) => {
                self.send_ingestion_failed(article_id, err_msg).await;
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
        // Inject GitLab token via parsed url::Url to prevent parser differentials.
        let authenticated_url = if let Some(token) = &self.gitlab_token {
            if let Ok(mut parsed) = url::Url::parse(url)
                && parsed.scheme() == "https"
                && parsed
                    .host_str()
                    .is_some_and(|h| h.eq_ignore_ascii_case("gitlab.com"))
            {
                let _ = parsed.set_username("oauth2");
                let _ = parsed.set_password(Some(token));
                parsed.to_string()
            } else {
                url.to_string()
            }
        } else {
            url.to_string()
        };

        // Check if remote exists
        let status = crate::git_cmd::in_dir_async(&self.repo_path)
            .args(["-c", "safe.bareRepository=all"])
            .args(["remote", "get-url", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await?;

        if status.success() {
            let output = crate::git_cmd::in_dir_async(&self.repo_path)
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
                let output = crate::git_cmd::in_dir_async(&self.repo_path)
                    .args(["-c", "safe.bareRepository=all"])
                    .args(["remote", "set-url", name, &authenticated_url])
                    .output()
                    .await?;

                if !output.status.success() {
                    return Err(anyhow!(
                        "Failed to update remote: {}",
                        redact_secret(String::from_utf8_lossy(&output.stderr).trim())
                    ));
                }
            }
        } else {
            info!(
                "Adding remote {} -> {}",
                name,
                redact_secret(&authenticated_url)
            );
            let output = crate::git_cmd::in_dir_async(&self.repo_path)
                .args(["-c", "safe.bareRepository=all"])
                .args(["remote", "add", name, &authenticated_url])
                .output()
                .await?;

            if !output.status.success() {
                return Err(anyhow!(
                    "Failed to add remote: {}",
                    redact_secret(String::from_utf8_lossy(&output.stderr).trim())
                ));
            }
        }
        Ok(())
    }

    /// Runs one `git fetch`, dropping a stale commit-graph and trying
    /// again when that is what turned the fetch away.  Fetch-pack
    /// rejects the graph before it opens a connection, so that retry
    /// repeats no transfer; the wording the commit parse emits can
    /// come after one, and then the retry pays for it again.  Returns
    /// git's own output either way; the caller words the failure.
    async fn fetch_with_graph_retry(&self, args: &[&str]) -> Result<Output> {
        let mut dropped_graph = false;

        loop {
            let output = timeout(
                GIT_FETCH_TIMEOUT,
                crate::git_cmd::in_dir_async(&self.repo_path)
                    .args(crate::git_ops::GIT_PROTOCOL_RESTRICTIONS)
                    .arg("fetch")
                    .args(args)
                    .output(),
            )
            .await
            .map_err(|_| anyhow!("git fetch timed out after {}s", GIT_FETCH_TIMEOUT.as_secs()))??;

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

    async fn fetch_commits(&self, remote: &str, commits: &[String]) -> Result<()> {
        let mut args = vec![remote];
        for commit in commits {
            if commit.is_empty()
                || commit.chars().any(char::is_whitespace)
                || commit.starts_with('-')
                || commit.starts_with('+')
                || commit.contains(':')
            {
                warn!("Skipping invalid commit ref in fetch batch: {}", commit);
                continue;
            }
            args.push(commit.as_str());
        }
        if args.len() == 1 {
            return Err(anyhow!("No valid commit refs to fetch"));
        }

        let output = self.fetch_with_graph_retry(&args).await?;
        if !output.status.success() {
            return Err(anyhow!(
                "Fetch failed: {}",
                redact_secret(String::from_utf8_lossy(&output.stderr).trim())
            ));
        }
        Ok(())
    }

    async fn fetch_all(&self, remote: &str) -> Result<()> {
        let output = self.fetch_with_graph_retry(&[remote]).await?;

        if !output.status.success() {
            return Err(anyhow!(
                "Fetch all failed: {}",
                redact_secret(String::from_utf8_lossy(&output.stderr).trim())
            ));
        }
        Ok(())
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

        let output = crate::git_cmd::in_dir_async(&self.repo_path)
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
        let output = crate::git_cmd::in_dir_async(&self.repo_path)
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
        crate::git_cmd::in_dir_async(&repo_path)
            .arg("init")
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;

        let file_path = repo_path.join("file.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "content")?;

        crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-m", "Subject Line\n\nBody Line"])
            .output()
            .await?;

        let (tx, _rx) = mpsc::channel(1);
        let (agent, _) = FetchAgent::new(repo_path.clone(), tx, None);

        let output = crate::git_cmd::in_dir_async(&repo_path)
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
        crate::git_cmd::in_dir_async(&repo_path)
            .arg("init")
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;

        let file_path = repo_path.join("file.txt");
        let mut file = File::create(&file_path)?;
        writeln!(file, "content")?;

        crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-m", "Subject Line"])
            .output()
            .await?;

        let (tx, _rx) = mpsc::channel(1);
        let (agent, _) = FetchAgent::new(repo_path.clone(), tx, None);

        let output = crate::git_cmd::in_dir_async(&repo_path)
            .args(["rev-parse", "HEAD^{tree}"])
            .output()
            .await?;
        let tree_sha = String::from_utf8(output.stdout)?.trim().to_string();

        assert!(
            !agent.is_present(&tree_sha).await,
            "Tree SHA should not be considered a present commit"
        );

        let output = crate::git_cmd::in_dir_async(&repo_path)
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
    async fn test_process_queue_redacts_credentials_on_failure() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        crate::git_cmd::in_dir_async(&repo_path)
            .arg("init")
            .output()
            .await?;

        let (tx, mut rx) = mpsc::channel(10);
        let (agent, _) = FetchAgent::new(repo_path, tx, None);

        let req = FetchRequest {
            repo_url: Some("https://oauth2:glpat_secret123@127.0.0.1:0/group/repo.git".to_string()),
            commit_hash: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
            mr_url: None,
            mr_title: None,
            mr_number: None,
        };

        let mut queue: HashMap<Option<String>, HashSet<String>> = HashMap::new();
        queue
            .entry(req.repo_url)
            .or_default()
            .insert(req.commit_hash.clone());

        agent.process_queue(&mut queue).await;

        let event = rx
            .try_recv()
            .expect("Expected IngestionFailed event to be emitted");
        match event {
            Event::IngestionFailed {
                article_id,
                error,
                source,
            } => {
                assert_eq!(article_id, req.commit_hash);
                assert_eq!(source, MessageSource::GitFetch);
                assert!(
                    error.contains("[REDACTED]"),
                    "Expected error to contain [REDACTED], got: {}",
                    error
                );
                assert!(
                    !error.contains("glpat_secret123"),
                    "Error leaked credential: {}",
                    error
                );
            }
            other => panic!("Expected IngestionFailed, got {:?}", other),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_process_queue_filters_b4_tracker_commit() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        crate::git_cmd::in_dir_async(&repo_path)
            .arg("init")
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;

        std::fs::write(repo_path.join("file.txt"), "base\n")?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-m", "base commit"])
            .output()
            .await?;
        let base_out = crate::git_cmd::in_dir_async(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        let base_sha = String::from_utf8(base_out.stdout)?.trim().to_string();

        let tracker_msg = "Series cover title\n\n\
            Cover body paragraph.\n\n\
            --- b4-submit-tracking ---\n\
            # This section is used internally by b4 prep for tracking purposes.\n\
            {\n  \
              \"series\": {\n    \
                \"revision\": 1,\n    \
                \"change-id\": \"20260401-b4-range-112233445566\",\n    \
                \"prefixes\": []\n  \
              }\n\
            }";
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "--allow-empty", "-m", tracker_msg])
            .output()
            .await?;

        std::fs::write(repo_path.join("file.txt"), "base\nchange1\n")?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-m", "subsys: real patch"])
            .output()
            .await?;
        let head_out = crate::git_cmd::in_dir_async(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        let head_sha = String::from_utf8(head_out.stdout)?.trim().to_string();

        let range = format!("{}..{}", base_sha, head_sha);
        let (tx, mut rx) = mpsc::channel(10);
        let (agent, _) = FetchAgent::new(repo_path, tx, None);

        let mut queue: HashMap<Option<String>, HashSet<String>> = HashMap::new();
        queue.entry(None).or_default().insert(range.clone());
        agent.process_queue(&mut queue).await;

        let ev0 = rx.try_recv().expect("Expected index 0 cover event");
        match ev0 {
            Event::PatchSubmitted {
                index,
                total,
                subject,
                message,
                diff,
                base_commit,
                ..
            } => {
                assert_eq!(index, 0);
                assert_eq!(total, 1);
                assert_eq!(subject, "Series cover title");
                assert!(message.contains("Cover body paragraph."));
                assert!(message.contains("change-id: 20260401-b4-range-112233445566"));
                assert!(diff.is_empty());
                assert_eq!(base_commit.as_deref(), Some(base_sha.as_str()));
            }
            other => panic!("Expected PatchSubmitted, got {:?}", other),
        }

        let ev1 = rx.try_recv().expect("Expected index 1 patch event");
        match ev1 {
            Event::PatchSubmitted {
                index,
                total,
                subject,
                message_id,
                base_commit,
                ..
            } => {
                assert_eq!(index, 1);
                assert_eq!(total, 1);
                assert_eq!(subject, "subsys: real patch");
                assert_eq!(message_id, head_sha);
                assert_eq!(base_commit.as_deref(), Some(base_sha.as_str()));
            }
            other => panic!("Expected PatchSubmitted, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());

        Ok(())
    }

    async fn remove_loose_object(repo_path: &std::path::Path, rev: &str) -> Result<String> {
        let out = crate::git_cmd::in_dir_async(repo_path)
            .args(["rev-parse", rev])
            .output()
            .await?;
        assert!(out.status.success(), "rev-parse {} failed", rev);
        let oid = String::from_utf8(out.stdout)?.trim().to_string();
        let obj_path = repo_path
            .join(".git/objects")
            .join(&oid[..2])
            .join(&oid[2..]);
        let mut perms = std::fs::metadata(&obj_path)?.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&obj_path, perms)?;
        std::fs::remove_file(&obj_path)?;
        Ok(oid)
    }

    #[tokio::test]
    async fn test_process_queue_range_partial_extract_failure_emits_mr_ingestion_failed()
    -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        crate::git_cmd::in_dir_async(&repo_path)
            .arg("init")
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;

        std::fs::write(repo_path.join("file.txt"), "base\n")?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-m", "base commit"])
            .output()
            .await?;
        let base_out = crate::git_cmd::in_dir_async(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        let base_sha = String::from_utf8(base_out.stdout)?.trim().to_string();

        std::fs::write(repo_path.join("file1.txt"), "first commit content\n")?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-m", "subsys: first patch"])
            .output()
            .await?;

        std::fs::write(repo_path.join("file2.txt"), "second commit missing blob\n")?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-m", "subsys: second patch"])
            .output()
            .await?;
        let head_out = crate::git_cmd::in_dir_async(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        let head_sha = String::from_utf8(head_out.stdout)?.trim().to_string();

        // Corrupt only the second commit's blob so resolve_git_range and commit 1
        // extraction succeed while commit 2 extraction fails.
        remove_loose_object(&repo_path, "HEAD:file2.txt").await?;

        let range = format!("{}..{}", base_sha, head_sha);
        let (tx, mut rx) = mpsc::channel(10);
        let (agent, _) = FetchAgent::new(repo_path, tx, None);
        agent.mr_metadata.lock().unwrap().insert(
            range.clone(),
            (
                Some("https://github.com/org/repo/pull/42".to_string()),
                Some("PR title".to_string()),
                Some(42),
            ),
        );

        let mut queue: HashMap<Option<String>, HashSet<String>> = HashMap::new();
        queue.entry(None).or_default().insert(range.clone());
        agent.process_queue(&mut queue).await;

        let ev = rx
            .try_recv()
            .expect("Expected IngestionFailed event when subset of range fails extraction");
        match ev {
            Event::IngestionFailed {
                article_id,
                error,
                source,
            } => {
                assert_eq!(article_id, format!("mr-42-{}", range));
                assert_eq!(source, MessageSource::GitFetch);
                assert!(
                    error.contains(&head_sha),
                    "Expected failed commit SHA in error, got: {}",
                    error
                );
            }
            other => panic!(
                "Expected IngestionFailed without partial PatchSubmitted, got {:?}",
                other
            ),
        }
        assert!(
            rx.try_recv().is_err(),
            "No partial PatchSubmitted events should be emitted"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_process_queue_mr_range_all_fail_and_remote_fail_use_mr_article_id() -> Result<()>
    {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        crate::git_cmd::in_dir_async(&repo_path)
            .arg("init")
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;

        std::fs::write(repo_path.join("file.txt"), "base\n")?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-m", "base commit"])
            .output()
            .await?;
        let base_out = crate::git_cmd::in_dir_async(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        let base_sha = String::from_utf8(base_out.stdout)?.trim().to_string();

        std::fs::write(repo_path.join("only.txt"), "sole commit missing blob\n")?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-m", "subsys: sole patch"])
            .output()
            .await?;
        let head_out = crate::git_cmd::in_dir_async(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        let head_sha = String::from_utf8(head_out.stdout)?.trim().to_string();

        remove_loose_object(&repo_path, "HEAD:only.txt").await?;

        let local_range = format!("{}..{}", base_sha, head_sha);
        let remote_range = format!("{}..deadbeefdeadbeefdeadbeefdeadbeefdeadbeef", base_sha);

        let (tx, mut rx) = mpsc::channel(10);
        let (agent, _) = FetchAgent::new(repo_path, tx, None);
        agent.mr_metadata.lock().unwrap().insert(
            local_range.clone(),
            (
                Some("https://github.com/org/repo/pull/99".to_string()),
                Some("PR 99".to_string()),
                Some(99),
            ),
        );
        agent.mr_metadata.lock().unwrap().insert(
            remote_range.clone(),
            (
                Some("https://gitlab.com/group/repo/-/merge_requests/77".to_string()),
                Some("MR 77".to_string()),
                Some(77),
            ),
        );

        let mut queue: HashMap<Option<String>, HashSet<String>> = HashMap::new();
        queue.entry(None).or_default().insert(local_range.clone());
        agent.process_queue(&mut queue).await;

        match rx
            .try_recv()
            .expect("Expected IngestionFailed for local MR range")
        {
            Event::IngestionFailed {
                article_id, source, ..
            } => {
                assert_eq!(article_id, format!("mr-99-{}", local_range));
                assert_eq!(source, MessageSource::GitFetch);
            }
            other => panic!("Expected IngestionFailed, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());

        queue
            .entry(Some(
                "https://oauth2:glpat_secret123@127.0.0.1:0/group/repo.git".to_string(),
            ))
            .or_default()
            .insert(remote_range.clone());
        agent.process_queue(&mut queue).await;

        match rx
            .try_recv()
            .expect("Expected IngestionFailed for remote MR range")
        {
            Event::IngestionFailed {
                article_id,
                error,
                source,
            } => {
                assert_eq!(article_id, format!("mr-77-{}", remote_range));
                assert_eq!(source, MessageSource::GitFetch);
                assert!(error.contains("[REDACTED]"));
                assert!(!error.contains("glpat_secret123"));
            }
            other => panic!("Expected IngestionFailed, got {:?}", other),
        }
        assert!(rx.try_recv().is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_drain_db_queue_submits_range_and_retries_transient_failures() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        crate::git_cmd::in_dir_async(&repo_path)
            .arg("init")
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.name", "Test User"])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .await?;

        let file_path = repo_path.join("file.txt");
        {
            let mut file = File::create(&file_path)?;
            writeln!(file, "base")?;
        }
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-m", "Base commit"])
            .output()
            .await?;
        let base_out = crate::git_cmd::in_dir_async(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        let base_sha = String::from_utf8(base_out.stdout)?.trim().to_string();

        {
            let mut file = File::create(&file_path)?;
            writeln!(file, "head")?;
        }
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["add", "."])
            .output()
            .await?;
        crate::git_cmd::in_dir_async(&repo_path)
            .args(["commit", "-m", "PR commit"])
            .output()
            .await?;
        let head_out = crate::git_cmd::in_dir_async(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .await?;
        let head_sha = String::from_utf8(head_out.stdout)?.trim().to_string();

        let db = Arc::new(
            Database::new(&crate::settings::DatabaseSettings {
                url: ":memory:".to_string(),
                token: String::new(),
            })
            .await?,
        );
        db.migrate().await?;

        let range = format!("{base_sha}..{head_sha}");
        db.enqueue_fetch_request(
            Some("https://github.com/sashiko-dev/sashiko.git"),
            &range,
            Some("https://github.com/sashiko-dev/sashiko/pull/738"),
            Some("Durable fetch queue"),
            Some(738),
        )
        .await?;

        let (tx, mut rx) = mpsc::channel(10);
        let (agent, _) = FetchAgent::new(repo_path, tx, None);
        let agent = agent.with_db(db.clone(), 2);

        agent.drain_db_queue(&db, 2).await;

        let submitted = rx.try_recv().expect("expected PatchSubmitted event");
        match submitted {
            Event::PatchSubmitted {
                article_id,
                message_id,
                mr_number,
                index,
                total,
                ..
            } => {
                assert_eq!(article_id, format!("mr-738-{range}"));
                assert_eq!(message_id, head_sha);
                assert_eq!(mr_number, Some(738));
                assert_eq!(index, 1);
                assert_eq!(total, 1);
            }
            other => panic!("expected PatchSubmitted, got {other:?}"),
        }
        assert!(db.lock_pending_fetch_queue().await?.is_none());

        // Now enqueue an unreachable remote range with max_retries = 2:
        // attempt 1 schedules a retry without emitting IngestionFailed;
        // attempt 2 exhausts retries and emits IngestionFailed with the exact
        // mr-739-<range> article_id (not individual commit SHAs).
        let missing_range =
            "1111111111111111111111111111111111111111..2222222222222222222222222222222222222222";
        db.enqueue_fetch_request(
            Some("https://127.0.0.1:0/sashiko-dev/sashiko.git"),
            missing_range,
            Some("https://github.com/sashiko-dev/sashiko/pull/739"),
            Some("Unreachable PR"),
            Some(739),
        )
        .await?;

        agent.drain_db_queue(&db, 2).await;
        assert!(
            rx.try_recv().is_err(),
            "first transient failure must schedule backoff rather than failing immediately"
        );

        // Clear backoff via immediate re-enqueue and run attempt 2 (exhaustion).
        db.enqueue_fetch_request(
            Some("https://127.0.0.1:0/sashiko-dev/sashiko.git"),
            missing_range,
            Some("https://github.com/sashiko-dev/sashiko/pull/739"),
            Some("Unreachable PR"),
            Some(739),
        )
        .await?;
        agent.drain_db_queue(&db, 2).await;

        let failed = rx
            .try_recv()
            .expect("exhausted retry budget must emit IngestionFailed");
        match failed {
            Event::IngestionFailed {
                article_id, source, ..
            } => {
                assert_eq!(article_id, format!("mr-739-{missing_range}"));
                assert_eq!(source, MessageSource::GitFetch);
            }
            other => panic!("expected IngestionFailed, got {other:?}"),
        }
        assert!(
            rx.try_recv().is_err(),
            "must emit exactly one IngestionFailed per failed range, not one per missing SHA"
        );

        Ok(())
    }
}
