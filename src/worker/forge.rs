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
use crate::fetcher::FetchRequest;
use crate::settings::ForgeSettings;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio::time::sleep;
use tracing::{error, info, warn};

const GITHUB_API_BASE: &str = "https://api.github.com";
const MAX_GITHUB_POLL_PAGES: u32 = 5;
const GITHUB_POLL_PER_PAGE: u32 = 50;

pub struct ForgeWorker {
    db: Arc<Database>,
    settings: ForgeSettings,
    max_retries: u32,
    cached_token: Mutex<Option<(String, i64)>>,
    fetch_sender: Option<mpsc::Sender<FetchRequest>>,
    repo_path: Option<PathBuf>,
    api_base: String,
}

impl ForgeWorker {
    pub fn new(db: Arc<Database>, settings: ForgeSettings, max_retries: u32) -> Self {
        Self {
            db,
            settings,
            max_retries,
            cached_token: Mutex::new(None),
            fetch_sender: None,
            repo_path: None,
            api_base: GITHUB_API_BASE.to_string(),
        }
    }

    pub fn with_poller(
        mut self,
        fetch_sender: mpsc::Sender<FetchRequest>,
        repo_path: PathBuf,
    ) -> Self {
        self.fetch_sender = Some(fetch_sender);
        self.repo_path = Some(repo_path);
        self
    }

    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into();
        self
    }

    fn load_private_key(&self) -> Result<String, String> {
        if let Some(ref pem) = self.settings.app_private_key
            && !pem.trim().is_empty()
        {
            return Ok(pem.clone());
        }
        if let Some(ref path) = self.settings.app_private_key_path
            && !path.trim().is_empty()
        {
            return std::fs::read_to_string(path).map_err(|e| {
                format!("failed to read GitHub App private key from {}: {}", path, e)
            });
        }
        Err("no GitHub App private key configured".to_string())
    }

    async fn invalidate_cached_token(&self) {
        let mut guard = self.cached_token.lock().await;
        *guard = None;
    }

    async fn resolve_token(&self, client: &reqwest::Client) -> Result<String, String> {
        if let (Some(app_id), Some(installation_id)) =
            (self.settings.app_id, self.settings.installation_id)
        {
            let now = chrono::Utc::now().timestamp();
            {
                let guard = self.cached_token.lock().await;
                if let Some((ref token, expires_at)) = *guard
                    && now + 300 < expires_at
                {
                    return Ok(token.clone());
                }
            }

            let pem = self.load_private_key()?;
            let jwt = crate::forge::mint_github_app_jwt(app_id, &pem)?;
            let token = crate::forge::exchange_github_installation_token(
                client,
                &self.api_base,
                installation_id,
                &jwt,
            )
            .await?;

            let mut guard = self.cached_token.lock().await;
            *guard = Some((token.clone(), now + 3600));
            return Ok(token);
        }

        if let Some(ref token) = self.settings.api_token
            && !token.trim().is_empty()
        {
            return Ok(token.clone());
        }

        Err("neither GitHub App credentials nor api_token configured".to_string())
    }

    async fn resolve_optional_token(&self, client: &reqwest::Client) -> Option<String> {
        if self.settings.app_id.is_some() && self.settings.installation_id.is_some() {
            match self.resolve_token(client).await {
                Ok(token) => return Some(token),
                Err(e) => {
                    warn!(
                        "Failed to resolve GitHub App token for PR polling (falling back to api_token/unauthenticated): {}",
                        e
                    );
                }
            }
        }
        self.settings
            .api_token
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    pub async fn resolve_polled_github_repos(&self) -> Vec<String> {
        if self
            .settings
            .provider
            .as_deref()
            .is_some_and(|p| !p.trim().is_empty() && !p.trim().eq_ignore_ascii_case("github"))
        {
            return Vec::new();
        }

        if !self.settings.repos.is_empty() {
            let mut seen = HashSet::new();
            let mut out = Vec::with_capacity(self.settings.repos.len());
            for raw in &self.settings.repos {
                match crate::forge::normalize_github_repo_slug(raw) {
                    Some(slug) => {
                        if seen.insert(slug.to_ascii_lowercase()) {
                            out.push(slug);
                        }
                    }
                    None => {
                        warn!(
                            "Ignoring invalid forge.repos entry: {}",
                            crate::forge::loggable(raw)
                        );
                    }
                }
            }
            return out;
        }

        if let Some(ref repo_path) = self.repo_path
            && let Some(detected) = crate::forge::detect_github_repo_from_git_dir(repo_path).await
        {
            return vec![detected];
        }

        Vec::new()
    }

    pub async fn poll_github_repos_once(&self, client: &reqwest::Client) -> usize {
        let Some(ref fetch_sender) = self.fetch_sender else {
            return 0;
        };
        let repos = self.resolve_polled_github_repos().await;
        if repos.is_empty() {
            return 0;
        }

        let mut total_ingested = 0;
        for repo in repos {
            let now = chrono::Utc::now().timestamp();
            let watermark = match self
                .db
                .bootstrap_or_get_forge_poll_watermark(&repo, now)
                .await
            {
                Ok(ts) => ts,
                Err(e) => {
                    error!("Failed to load GitHub poll watermark for {}: {}", repo, e);
                    continue;
                }
            };

            let mut max_updated_at = watermark;
            let mut had_poll_error = false;
            let mut had_ingest_error = false;
            let mut had_skipped_item = false;
            let mut reached_watermark = false;
            let mut saw_last_page = false;

            for page in 1..=MAX_GITHUB_POLL_PAGES {
                let mut token = self.resolve_optional_token(client).await;
                let mut result = crate::forge::list_github_open_pull_requests(
                    client,
                    &self.api_base,
                    &repo,
                    token.as_deref(),
                    page,
                    GITHUB_POLL_PER_PAGE,
                )
                .await;

                if let Err(ref e) = result
                    && e.contains("401 Unauthorized")
                    && token.is_some()
                {
                    warn!(
                        "GitHub pulls API returned 401 for {}; invalidating cached token and retrying",
                        repo
                    );
                    self.invalidate_cached_token().await;
                    token = self.resolve_optional_token(client).await;
                    result = crate::forge::list_github_open_pull_requests(
                        client,
                        &self.api_base,
                        &repo,
                        token.as_deref(),
                        page,
                        GITHUB_POLL_PER_PAGE,
                    )
                    .await;
                    if let Err(ref e2) = result
                        && e2.contains("401 Unauthorized")
                        && token.is_some()
                    {
                        result = crate::forge::list_github_open_pull_requests(
                            client,
                            &self.api_base,
                            &repo,
                            None,
                            page,
                            GITHUB_POLL_PER_PAGE,
                        )
                        .await;
                    }
                }

                let pulls_page = match result {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(
                            "Failed to poll open GitHub pull requests for {} (page {}): {}",
                            repo, page, e
                        );
                        had_poll_error = true;
                        break;
                    }
                };

                had_skipped_item |= pulls_page.skipped_count > 0;
                if (pulls_page.raw_count as u32) < GITHUB_POLL_PER_PAGE {
                    saw_last_page = true;
                }
                for pr in pulls_page.items {
                    max_updated_at = max_updated_at.max(pr.updated_at);
                    if pr.updated_at < watermark {
                        reached_watermark = true;
                        break;
                    }

                    let pr_number = pr.metadata.pr_number;
                    if let Some(ref author) = pr.metadata.author
                        && crate::forge::is_dependabot_author(author)
                    {
                        continue;
                    }
                    match self
                        .db
                        .has_forge_pr_head_ingested(&repo, pr_number, &pr.metadata.head_sha)
                        .await
                    {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(e) => {
                            error!(
                                "Failed to check ingestion state for polled GitHub PR {}#{}: {}",
                                repo, pr_number, e
                            );
                            had_ingest_error = true;
                            continue;
                        }
                    }

                    let head_short = &pr.metadata.head_sha[..8.min(pr.metadata.head_sha.len())];
                    match crate::server::ingest_forge_pull_request(
                        &self.db,
                        fetch_sender,
                        "GitHub",
                        pr.metadata.clone(),
                    )
                    .await
                    {
                        Ok(true) => {
                            info!(
                                "Polled and queued GitHub PR {}#{} (head {}) for review",
                                repo, pr_number, head_short
                            );
                            total_ingested += 1;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            error!(
                                "Failed to ingest polled GitHub PR {}#{}: {}",
                                repo, pr_number, e
                            );
                            had_ingest_error = true;
                        }
                    }
                }

                if reached_watermark || saw_last_page {
                    break;
                }
            }

            if !had_poll_error
                && !had_ingest_error
                && !had_skipped_item
                && (reached_watermark || saw_last_page)
                && let Err(e) = self
                    .db
                    .set_forge_poll_watermark(&repo, max_updated_at.max(watermark))
                    .await
            {
                error!("Failed to update GitHub poll watermark for {}: {}", repo, e);
            }
        }

        total_ingested
    }

    pub async fn run_poller(&self) {
        if self.settings.poll_interval_secs == 0 {
            info!("Forge PR polling disabled (forge.poll_interval_secs = 0)");
            return;
        }
        let client = match crate::worker::outbox_client() {
            Ok(client) => client,
            Err(e) => {
                error!("Failed to build forge poller HTTP client: {}", e);
                return;
            }
        };
        let repos = self.resolve_polled_github_repos().await;
        info!(
            "Starting Forge PR Poller (interval: {}s, repos: {:?})...",
            self.settings.poll_interval_secs, repos
        );
        self.poll_github_repos_once(&client).await;
        let delay = Duration::from_secs(self.settings.poll_interval_secs.max(10));
        loop {
            sleep(delay).await;
            self.poll_github_repos_once(&client).await;
        }
    }

    fn backoff_seconds(retry_count: i64) -> i64 {
        match retry_count {
            0 => 5,
            1 => 30,
            _ => 180,
        }
    }

    fn is_permanent_failure(err: &str) -> bool {
        err.contains("404 Not Found") || err.contains("422 Unprocessable")
    }

    pub async fn run(&self) {
        info!("Starting Forge Worker...");
        // Every GitHub call below (token exchange and comment posting) is
        // awaited inline by this sequential loop, so the client must bound
        // each request or a silent remote stalls the whole outbox.
        let client = match crate::worker::outbox_client() {
            Ok(client) => client,
            Err(e) => {
                error!("Failed to build forge HTTP client: {}", e);
                return;
            }
        };
        loop {
            if let Err(e) = self.db.sweep_ghost_forge_outbox().await {
                error!("Failed to sweep ghost forge outbox entries: {}", e);
            }

            match self.db.lock_pending_forge_outbox().await {
                Ok(Some(entry)) => {
                    if entry.provider != "github" {
                        let msg = format!("unsupported forge provider: {}", entry.provider);
                        error!("Forge outbox ID {}: {}", entry.id, msg);
                        if let Err(db_err) = self.db.mark_forge_outbox_failed(entry.id, &msg).await
                        {
                            error!(
                                "Failed to mark forge outbox {} as failed: {}",
                                entry.id, db_err
                            );
                        }
                        continue;
                    }

                    info!(
                        "Processing forge outbox ID {} for {}#{}",
                        entry.id, entry.repo, entry.pr_number
                    );

                    let token = match self.resolve_token(&client).await {
                        Ok(t) => t,
                        Err(e) => {
                            error!(
                                "Failed to resolve GitHub token for outbox ID {}: {}",
                                entry.id, e
                            );
                            self.handle_failure(entry.id, entry.retry_count, &e).await;
                            continue;
                        }
                    };

                    match crate::forge::post_github_pr_comment(
                        &client,
                        &self.api_base,
                        &entry.repo,
                        entry.pr_number,
                        &token,
                        &entry.body,
                    )
                    .await
                    {
                        Ok(status) => {
                            info!(
                                "Posted GitHub PR comment for {}#{} (outbox ID {}, HTTP {})",
                                entry.repo, entry.pr_number, entry.id, status
                            );
                            if let Err(e) = self.db.mark_forge_outbox_sent(entry.id).await {
                                error!("Failed to mark forge outbox {} as sent: {}", entry.id, e);
                            }
                        }
                        Err(e) => {
                            if e.contains("401 Unauthorized") {
                                warn!(
                                    "GitHub returned 401; invalidating cached installation token"
                                );
                                self.invalidate_cached_token().await;
                            }
                            error!(
                                "Failed to post GitHub PR comment for outbox ID {}: {}",
                                entry.id, e
                            );
                            self.handle_failure(entry.id, entry.retry_count, &e).await;
                        }
                    }
                }
                Ok(None) => {
                    sleep(Duration::from_secs(5)).await;
                }
                Err(e) => {
                    error!("Database error while locking forge outbox entry: {}", e);
                    sleep(Duration::from_secs(10)).await;
                }
            }
        }
    }

    async fn handle_failure(&self, id: i64, retry_count: i64, err: &str) {
        if Self::is_permanent_failure(err) || retry_count + 1 >= self.max_retries as i64 {
            if let Err(db_err) = self.db.mark_forge_outbox_failed(id, err).await {
                error!("Failed to mark forge outbox {} as failed: {}", id, db_err);
            }
        } else {
            let delay = Self::backoff_seconds(retry_count);
            let retry_at = chrono::Utc::now().timestamp() + delay;
            if let Err(db_err) = self.db.set_forge_outbox_retry_at(id, retry_at, err).await {
                error!(
                    "Failed to schedule retry for forge outbox {}: {}",
                    id, db_err
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::get,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct MockGitHubState {
        pulls: Arc<Mutex<serde_json::Value>>,
        reject_bearer_once: Arc<AtomicUsize>,
    }

    async fn mock_pulls_handler(
        State(state): State<MockGitHubState>,
        headers: HeaderMap,
    ) -> Result<Json<serde_json::Value>, StatusCode> {
        if headers.contains_key("authorization")
            && state.reject_bearer_once.fetch_sub(1, Ordering::SeqCst) == 1
        {
            return Err(StatusCode::UNAUTHORIZED);
        }
        let body = state.pulls.lock().await.clone();
        Ok(Json(body))
    }

    #[tokio::test]
    async fn test_poll_github_repos_reconciles_missed_draft_and_force_pushed_prs() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let now_rfc3339 = chrono::Utc::now().to_rfc3339();
        let base1 = "1111111111111111111111111111111111111111";
        let base2 = "9999999999999999999999999999999999999999";
        let head1 = "2222222222222222222222222222222222222222";
        let head2 = "3333333333333333333333333333333333333333";
        let draft_head = "4444444444444444444444444444444444444444";

        let initial_pulls = serde_json::json!([
            {
                "number": 735,
                "title": "Missed webhook PR",
                "html_url": "https://github.com/sashiko-dev/sashiko/pull/735",
                "draft": false,
                "updated_at": now_rfc3339,
                "user": { "login": "octocat" },
                "head": { "sha": head1 },
                "base": {
                    "sha": base1,
                    "repo": { "clone_url": "https://github.com/sashiko-dev/sashiko.git" }
                }
            },
            {
                "number": 736,
                "title": "Draft PR should be reviewed",
                "html_url": "https://github.com/sashiko-dev/sashiko/pull/736",
                "draft": true,
                "updated_at": now_rfc3339,
                "user": { "login": "octocat" },
                "head": { "sha": draft_head },
                "base": {
                    "sha": base1,
                    "repo": { "clone_url": "https://github.com/sashiko-dev/sashiko.git" }
                }
            },
            {
                "number": 737,
                "title": "Bump deps",
                "html_url": "https://github.com/sashiko-dev/sashiko/pull/737",
                "draft": false,
                "updated_at": now_rfc3339,
                "user": { "login": "dependabot[bot]" },
                "head": { "sha": head1 },
                "base": {
                    "sha": base1,
                    "repo": { "clone_url": "https://github.com/sashiko-dev/sashiko.git" }
                }
            }
        ]);

        let mock_state = MockGitHubState {
            pulls: Arc::new(Mutex::new(initial_pulls)),
            reject_bearer_once: Arc::new(AtomicUsize::new(1)),
        };
        let app = Router::new()
            .route("/repos/sashiko-dev/sashiko/pulls", get(mock_pulls_handler))
            .with_state(mock_state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut settings = crate::settings::Settings::new().unwrap();
        settings.forge.enabled = true;
        settings.forge.api_token = Some("stale-token".to_string());
        settings.forge.repos = vec!["sashiko-dev/sashiko".to_string()];

        let (fetch_tx, _fetch_rx) = mpsc::channel(10);
        let worker = ForgeWorker::new(db.clone(), settings.forge.clone(), 3)
            .with_poller(fetch_tx, PathBuf::from("."))
            .with_api_base(format!("http://{}", addr));

        let client = crate::worker::outbox_client().unwrap();

        // 1. First poll recovers from initial 401 via unauthenticated fallback,
        // ingests #735 and draft #736, and skips Dependabot #737.
        let count1 = worker.poll_github_repos_once(&client).await;
        assert_eq!(count1, 2);

        let f1 = db.lock_pending_fetch_queue().await.unwrap().unwrap();
        assert_eq!(f1.mr_number, Some(735));
        db.mark_fetch_queue_done(f1.id, f1.locked_at).await.unwrap();
        db.conn
            .execute(
                "UPDATE patchsets SET status = 'Reviewed' WHERE mr_number = 735",
                (),
            )
            .await
            .unwrap();

        let f2 = db.lock_pending_fetch_queue().await.unwrap().unwrap();
        assert_eq!(f2.mr_number, Some(736));
        db.mark_fetch_queue_done(f2.id, f2.locked_at).await.unwrap();
        db.conn
            .execute(
                "UPDATE patchsets SET status = 'Reviewed' WHERE mr_number = 736",
                (),
            )
            .await
            .unwrap();

        // 2. When main advances (base_sha changes to base2) but head_sha is unchanged,
        // polling must NOT re-ingest or re-review #735.
        let later_rfc3339 = (chrono::Utc::now() + chrono::Duration::seconds(30)).to_rfc3339();
        *mock_state.pulls.lock().await = serde_json::json!([
            {
                "number": 735,
                "title": "Missed webhook PR",
                "html_url": "https://github.com/sashiko-dev/sashiko/pull/735",
                "draft": false,
                "updated_at": later_rfc3339,
                "user": { "login": "octocat" },
                "head": { "sha": head1 },
                "base": {
                    "sha": base2,
                    "repo": { "clone_url": "https://github.com/sashiko-dev/sashiko.git" }
                }
            }
        ]);
        let count2 = worker.poll_github_repos_once(&client).await;
        assert_eq!(count2, 0);
        assert!(db.lock_pending_fetch_queue().await.unwrap().is_none());

        // 3. Force-pushing a new head_sha (head2) on #735 queues a [v2] review.
        let v2_rfc3339 = (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339();
        *mock_state.pulls.lock().await = serde_json::json!([
            {
                "number": 735,
                "title": "Missed webhook PR",
                "html_url": "https://github.com/sashiko-dev/sashiko/pull/735",
                "draft": false,
                "updated_at": v2_rfc3339,
                "user": { "login": "octocat" },
                "head": { "sha": head2 },
                "base": {
                    "sha": base2,
                    "repo": { "clone_url": "https://github.com/sashiko-dev/sashiko.git" }
                }
            }
        ]);
        let count3 = worker.poll_github_repos_once(&client).await;
        assert_eq!(count3, 1);
        let f3 = db.lock_pending_fetch_queue().await.unwrap().unwrap();
        assert_eq!(f3.mr_number, Some(735));
        assert_eq!(f3.commit_range, format!("{base2}..{head2}"));

        let mut rows = db
            .conn
            .query(
                "SELECT subject FROM patchsets WHERE mr_number = 735 ORDER BY id DESC LIMIT 1",
                (),
            )
            .await
            .unwrap();
        let latest_subject: String = rows.next().await.unwrap().unwrap().get(0).unwrap();
        assert!(
            latest_subject.contains("#735 [v2]: Missed webhook PR"),
            "unexpected subject: {latest_subject}"
        );
        drop(rows);
        db.mark_fetch_queue_done(f3.id, f3.locked_at).await.unwrap();
        db.conn
            .execute(
                "UPDATE patchsets SET status = 'Reviewed' WHERE id = (SELECT MAX(id) FROM patchsets WHERE mr_number = 735)",
                (),
            )
            .await
            .unwrap();

        // 4. Force-pushing #735 back to head1 (head1 -> head2 -> head1) queues [v3],
        // and a malformed item on the same page prevents advancing the watermark.
        let wm_before_v3 = db
            .bootstrap_or_get_forge_poll_watermark("sashiko-dev/sashiko", 999_999)
            .await
            .unwrap();
        let v3_rfc3339 = (chrono::Utc::now() + chrono::Duration::seconds(120)).to_rfc3339();
        *mock_state.pulls.lock().await = serde_json::json!([
            {
                "number": 735,
                "title": "Missed webhook PR",
                "html_url": "https://github.com/sashiko-dev/sashiko/pull/735",
                "draft": false,
                "updated_at": v3_rfc3339,
                "user": { "login": "octocat" },
                "head": { "sha": head1 },
                "base": {
                    "sha": base2,
                    "repo": { "clone_url": "https://github.com/sashiko-dev/sashiko.git" }
                }
            },
            {
                "number": 999,
                "title": "Malformed PR",
                "updated_at": v3_rfc3339,
                "head": { "sha": "not-a-sha" },
                "base": { "sha": base2 }
            }
        ]);
        let count4 = worker.poll_github_repos_once(&client).await;
        assert_eq!(count4, 1);
        let wm_after_v3 = db
            .bootstrap_or_get_forge_poll_watermark("sashiko-dev/sashiko", 999_999)
            .await
            .unwrap();
        assert_eq!(
            wm_after_v3, wm_before_v3,
            "watermark must not advance when a page contains skipped malformed items"
        );
        let mut rows_v3 = db
            .conn
            .query(
                "SELECT subject, status FROM patchsets WHERE mr_number = 735 ORDER BY id DESC LIMIT 1",
                (),
            )
            .await
            .unwrap();
        let row_v3 = rows_v3.next().await.unwrap().unwrap();
        let v3_subject: String = row_v3.get(0).unwrap();
        let v3_status: String = row_v3.get(1).unwrap();
        assert!(
            v3_subject.contains("#735 [v3]: Missed webhook PR"),
            "unexpected v3 subject: {v3_subject}"
        );
        assert_eq!(v3_status, "Fetching");

        server_handle.abort();
    }
}
