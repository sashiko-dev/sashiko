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
use crate::fetcher::FetchRequest;
use crate::settings::ServerSettings;
use axum::{
    Json, Router,
    extract::{ConnectInfo, Query, State},
    http::StatusCode,
    routing::{get, get_service, post},
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{error, info};

use std::time::{Duration, Instant};
use tokio::sync::RwLock;

struct CachedValue<T> {
    value: T,
    timestamp: Instant,
}

struct AsyncCache<T> {
    inner: RwLock<Option<CachedValue<T>>>,
    ttl: Duration,
}

impl<T: Clone> AsyncCache<T> {
    fn new(ttl: Duration) -> Self {
        Self {
            inner: RwLock::new(None),
            ttl,
        }
    }

    async fn get_or_fetch<F, Fut, E>(&self, fetch: F) -> Result<T, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
    {
        if let Some(cached) = self.inner.read().await.as_ref()
            && cached.timestamp.elapsed() < self.ttl
        {
            return Ok(cached.value.clone());
        }

        let mut write_guard = self.inner.write().await;
        if let Some(cached) = write_guard.as_ref()
            && cached.timestamp.elapsed() < self.ttl
        {
            return Ok(cached.value.clone());
        }

        let value = fetch().await?;
        *write_guard = Some(CachedValue {
            value: value.clone(),
            timestamp: Instant::now(),
        });
        Ok(value)
    }
}

struct AsyncMapCache<K, V> {
    inner: RwLock<std::collections::HashMap<K, CachedValue<V>>>,
    ttl: Duration,
}

impl<K: std::hash::Hash + Eq + Clone, V: Clone> AsyncMapCache<K, V> {
    fn new(ttl: Duration) -> Self {
        Self {
            inner: RwLock::new(std::collections::HashMap::new()),
            ttl,
        }
    }

    async fn get_or_fetch<F, Fut, E>(&self, key: K, fetch: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<V, E>>,
    {
        if let Some(cached) = self.inner.read().await.get(&key)
            && cached.timestamp.elapsed() < self.ttl
        {
            return Ok(cached.value.clone());
        }

        let mut write_guard = self.inner.write().await;
        if let Some(cached) = write_guard.get(&key)
            && cached.timestamp.elapsed() < self.ttl
        {
            return Ok(cached.value.clone());
        }

        let value = fetch().await?;
        write_guard.insert(
            key,
            CachedValue {
                value: value.clone(),
                timestamp: Instant::now(),
            },
        );
        Ok(value)
    }
}

pub struct AppState {
    pub db: Arc<Database>,
    pub sender: mpsc::Sender<Event>,
    pub fetch_sender: mpsc::Sender<FetchRequest>,
    pub read_only: bool,
    pub allow_all_submit: bool,
    pub smtp_enabled: bool,
    pub dry_run: bool,
    stats_cache: AsyncCache<serde_json::Value>,
    stats_timeline_cache: AsyncMapCache<Option<i64>, serde_json::Value>,
    stats_reviews_cache: AsyncCache<serde_json::Value>,
    stats_tools_cache: AsyncCache<serde_json::Value>,
    messages_count_cache: AsyncCache<usize>,
    patchsets_count_cache: AsyncCache<usize>,
    patchsets_homepage_cache: AsyncCache<Vec<crate::db::PatchsetRow>>,
    messages_homepage_cache: AsyncCache<Vec<crate::db::MessageRow>>,
}

#[derive(Deserialize)]
pub struct Pagination {
    pub page: Option<usize>,
    pub per_page: Option<usize>,
    pub q: Option<String>,
    pub mailing_list: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct PatchsetsResponse {
    pub items: Vec<crate::db::PatchsetRow>,
    pub total: usize,
    pub page: usize,
    pub per_page: usize,
}

#[derive(Serialize, Deserialize)]
pub struct MessagesResponse {
    pub items: Vec<crate::db::MessageRow>,
    pub total: usize,
    pub page: usize,
    pub per_page: usize,
}

#[derive(Deserialize)]
pub struct PatchQuery {
    pub id: String,
    pub page: Option<u32>,
    pub per_page: Option<u32>,
}

#[derive(Deserialize)]
pub struct ReviewQuery {
    pub id: Option<i64>,
    pub patchset_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct RerunPatchQuery {
    pub patchset_id: i64,
    pub patch_id: i64,
}

#[derive(Deserialize)]
pub struct SubsystemQuery {
    pub subsystem_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct InjectRequest {
    pub raw: String,
    pub group: Option<String>,
    pub baseline: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SubmitRequest {
    Inject {
        raw: String,
        base_commit: Option<String>,
        skip_subjects: Option<Vec<String>>,
        only_subjects: Option<Vec<String>>,
    },
    Remote {
        sha: String,
        repo: Option<String>,
        skip_subjects: Option<Vec<String>>,
        only_subjects: Option<Vec<String>>,
    },
    #[serde(rename = "remote-range")]
    RemoteRange {
        sha: String,
        repo: Option<String>,
        skip_subjects: Option<Vec<String>>,
        only_subjects: Option<Vec<String>>,
    },
    Thread {
        msgid: String,
    },
}

#[derive(Serialize, Deserialize)]
pub struct SubmitResponse {
    pub status: String,
    pub id: String,
}

pub async fn run_server(
    settings: ServerSettings,
    db: Arc<Database>,
    sender: mpsc::Sender<Event>,
    fetch_sender: mpsc::Sender<FetchRequest>,
    allow_all_submit: bool,
    smtp_enabled: bool,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(AppState {
        db,
        sender,
        fetch_sender,
        read_only: settings.read_only,
        allow_all_submit,
        smtp_enabled,
        dry_run,
        stats_cache: AsyncCache::new(Duration::from_secs(60)),
        stats_timeline_cache: AsyncMapCache::new(Duration::from_secs(60)),
        stats_reviews_cache: AsyncCache::new(Duration::from_secs(60)),
        stats_tools_cache: AsyncCache::new(Duration::from_secs(60)),
        messages_count_cache: AsyncCache::new(Duration::from_secs(30)),
        patchsets_count_cache: AsyncCache::new(Duration::from_secs(30)),
        patchsets_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
        messages_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
    });

    let app = Router::new()
        .route("/api/lists", get(list_mailing_lists))
        .route("/api/patchsets", get(list_patchsets))
        .route("/api/messages", get(list_messages))
        .route("/api/patch", get(get_patchset))
        .route("/api/message", get(get_message))
        .route("/api/review", get(get_review))
        .route("/api/stats", get(get_stats))
        .route("/api/stats/timeline", get(stats_timeline))
        .route("/api/stats/reviews", get(stats_reviews))
        .route("/api/stats/tools", get(stats_tools))
        .route("/api/submit", post(submit_patch))
        .route("/api/patchset/rerun", post(rerun_patchset))
        .route("/api/patch/rerun", post(rerun_patch))
        .route("/", get_service(ServeFile::new("static/index.html")))
        .nest_service("/static", ServeDir::new("static"))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], settings.port));
    info!("Web API listening on {}", addr);

    let listener = TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

fn generate_synthetic_id(prefix: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let start = SystemTime::now();
    let since_the_epoch = start
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards");
    // e.g. sashiko-local-1715890000-12345
    format!(
        "sashiko-{}-{}-{}",
        prefix,
        since_the_epoch.as_secs(),
        fastrand::u32(..)
    )
}

async fn submit_patch(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<SubmitRequest>,
) -> Result<Json<SubmitResponse>, StatusCode> {
    if state.read_only {
        return Err(StatusCode::FORBIDDEN);
    }

    if !state.allow_all_submit && !addr.ip().is_loopback() {
        info!("Refused patch submission from non-localhost: {}", addr);
        return Err(StatusCode::FORBIDDEN);
    }

    match payload {
        SubmitRequest::Inject {
            raw,
            base_commit,
            skip_subjects,
            only_subjects,
        } => {
            if raw.trim().is_empty() {
                return Err(StatusCode::BAD_REQUEST);
            }
            // Basic guardrail
            if !raw.contains("From ") && !raw.contains("Subject:") {
                return Err(StatusCode::BAD_REQUEST);
            }

            let id = generate_synthetic_id("inject");
            info!("Received raw mbox injection: {} (len: {})", id, raw.len());

            let event = Event::RawMboxSubmitted {
                raw,
                group: "api-submit".to_string(),
                baseline: base_commit,
                skip_subjects,
                only_subjects,
            };

            if let Err(e) = state.sender.send(event).await {
                error!("Failed to send raw mbox to queue: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }

            Ok(Json(SubmitResponse {
                status: "accepted".to_string(),
                id,
            }))
        }
        SubmitRequest::Remote {
            sha,
            repo,
            skip_subjects,
            only_subjects,
        }
        | SubmitRequest::RemoteRange {
            sha,
            repo,
            skip_subjects,
            only_subjects,
        } => {
            let id = sha.clone();
            let repo_display = repo.as_deref().unwrap_or("local");
            info!(
                "Received remote fetch request: {} from {}",
                sha, repo_display
            );

            // Create a placeholder record in the DB so the user can track status
            if let Err(e) = state
                .db
                .create_fetching_patchset(
                    &id,
                    &format!("Fetching {} from {}...", &sha, repo_display),
                    skip_subjects.as_ref(),
                    only_subjects.as_ref(),
                )
                .await
            {
                error!("Failed to create placeholder patchset: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }

            let req = FetchRequest {
                repo_url: repo,
                commit_hash: sha,
            };

            if let Err(e) = state.fetch_sender.send(req).await {
                error!("Failed to send fetch request to queue: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }

            Ok(Json(SubmitResponse {
                status: "accepted".to_string(),
                id,
            }))
        }
        SubmitRequest::Thread { msgid } => {
            let id = generate_synthetic_id("thread");
            info!("Received thread fetch request: {} (msgid: {})", id, msgid);

            // Create a placeholder record in the DB so the user can track status
            if let Err(e) = state
                .db
                .create_fetching_patchset(
                    &msgid, // Or &id? msgid is cleaner for display
                    &format!("Fetching thread {}...", msgid),
                    None,
                    None,
                )
                .await
            {
                tracing::error!("Failed to create placeholder patchset: {}", e);
                // Non-fatal, just continue
            }

            let msgid_clone = msgid.clone();
            let sender = state.sender.clone();

            tokio::spawn(async move {
                if let Err(e) = fetch_and_inject_thread(&msgid_clone, sender).await {
                    tracing::error!("Failed to fetch thread {}: {}", msgid_clone, e);
                }
            });

            Ok(Json(SubmitResponse {
                status: "accepted".to_string(),
                id, // The client might expect this ID
            }))
        }
    }
}

async fn fetch_and_inject_thread(
    msgid: &str,
    sender: tokio::sync::mpsc::Sender<Event>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("https://lore.kernel.org/all/{}/t.mbox.gz", msgid);
    let response = reqwest::get(&url).await?;

    if !response.status().is_success() {
        return Err(format!(
            "Failed to fetch thread {}: HTTP {}",
            msgid,
            response.status()
        )
        .into());
    }

    let bytes = response.bytes().await?;

    // Decompress the gzip data using a blocking task to avoid blocking the async runtime
    let raw = tokio::task::spawn_blocking(move || -> Result<String, std::io::Error> {
        use std::io::Read;
        let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
        let mut raw = String::new();
        decoder.read_to_string(&mut raw)?;
        Ok(raw)
    })
    .await??;

    let event = Event::RawMboxSubmitted {
        raw,
        group: "api-submit".to_string(),
        baseline: None,
        skip_subjects: None,
        only_subjects: None,
    };

    sender.send(event).await?;
    Ok(())
}

async fn list_mailing_lists(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    let lists = state.db.get_mailing_lists().await.map_err(|e| {
        error!("Failed to get mailing lists: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let result = lists
        .into_iter()
        .map(|(name, group)| {
            serde_json::json!({
                "name": name,
                "group": group
            })
        })
        .collect();

    Ok(Json(result))
}

async fn list_patchsets(
    State(state): State<Arc<AppState>>,
    Query(pagination): Query<Pagination>,
) -> Result<Json<PatchsetsResponse>, StatusCode> {
    let page = pagination.page.unwrap_or(1).max(1);
    let per_page = pagination.per_page.unwrap_or(50).clamp(1, 100);
    let offset = (page - 1) * per_page;

    let items = if pagination.q.is_none()
        && pagination.mailing_list.is_none()
        && page == 1
        && per_page == 50
    {
        state
            .patchsets_homepage_cache
            .get_or_fetch(|| async {
                state
                    .db
                    .get_patchsets(per_page, offset, None, None)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
            })
            .await?
    } else {
        state
            .db
            .get_patchsets(
                per_page,
                offset,
                pagination.q.clone(),
                pagination.mailing_list.clone(),
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };
    let total = if pagination.q.is_none() && pagination.mailing_list.is_none() {
        state
            .patchsets_count_cache
            .get_or_fetch(|| async {
                state
                    .db
                    .count_patchsets(None, None)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
            })
            .await?
    } else {
        state
            .db
            .count_patchsets(pagination.q.clone(), pagination.mailing_list.clone())
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };

    Ok(Json(PatchsetsResponse {
        items,
        total,
        page,
        per_page,
    }))
}

async fn list_messages(
    State(state): State<Arc<AppState>>,
    Query(pagination): Query<Pagination>,
) -> Result<Json<MessagesResponse>, StatusCode> {
    let page = pagination.page.unwrap_or(1).max(1);
    let per_page = pagination.per_page.unwrap_or(50).clamp(1, 100);
    let offset = (page - 1) * per_page;

    let items = if pagination.q.is_none()
        && pagination.mailing_list.is_none()
        && page == 1
        && per_page == 50
    {
        state
            .messages_homepage_cache
            .get_or_fetch(|| async {
                state
                    .db
                    .get_messages(per_page, offset, None, None)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
            })
            .await?
    } else {
        state
            .db
            .get_messages(
                per_page,
                offset,
                pagination.q.clone(),
                pagination.mailing_list.clone(),
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };
    let total = if pagination.q.is_none() && pagination.mailing_list.is_none() {
        state
            .messages_count_cache
            .get_or_fetch(|| async {
                state
                    .db
                    .count_messages(None, None)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
            })
            .await?
    } else {
        state
            .db
            .count_messages(pagination.q.clone(), pagination.mailing_list.clone())
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };

    Ok(Json(MessagesResponse {
        items,
        total,
        page,
        per_page,
    }))
}

async fn get_patchset(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PatchQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let result = if let Ok(id_val) = query.id.parse::<i64>() {
        info!("Fetching details for patchset id: {}", id_val);
        state
            .db
            .get_patchset_details(id_val, query.page, query.per_page)
            .await
    } else {
        info!("Fetching details for patchset msgid: {}", query.id);
        state
            .db
            .get_patchset_details_by_msgid(&query.id, query.page, query.per_page)
            .await
    };

    match result {
        Ok(Some(mut details)) => {
            if let Some(obj) = details.as_object_mut() {
                obj.insert(
                    "smtp_enabled".to_string(),
                    serde_json::Value::Bool(state.smtp_enabled),
                );
                obj.insert(
                    "dry_run".to_string(),
                    serde_json::Value::Bool(state.dry_run),
                );
            }
            Ok(Json(details))
        }
        Ok(None) => {
            info!("Patchset not found: {}", query.id);
            Err(StatusCode::NOT_FOUND)
        }
        Err(e) => {
            info!("Database error: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_review(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ReviewQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let result = if let Some(ps_id) = query.patchset_id {
        info!("Fetching latest review for patchset id: {}", ps_id);
        state.db.get_latest_review_for_patchset(ps_id).await
    } else if let Some(id) = query.id {
        info!("Fetching details for review id: {}", id);
        state.db.get_review_details(id).await
    } else {
        return Err(StatusCode::BAD_REQUEST);
    };

    match result {
        Ok(Some(details)) => Ok(Json(details)),
        Ok(None) => {
            info!("Review not found");
            Err(StatusCode::NOT_FOUND)
        }
        Err(e) => {
            info!("Database error: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_message(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PatchQuery>,
) -> Result<Json<crate::db::MessageRow>, StatusCode> {
    let result = if let Ok(id_val) = query.id.parse::<i64>() {
        info!("Fetching details for message id: {}", id_val);
        state.db.get_message_details(id_val).await
    } else {
        info!("Fetching details for message msgid: {}", query.id);
        state.db.get_message_details_by_msgid(&query.id).await
    };

    match result {
        Ok(Some(mut details)) => {
            if (details.body.is_none() || details.body.as_deref() == Some(""))
                && let (Some(hash), Some(group)) = (&details.git_blob_hash, &details.mailing_list)
            {
                let repo_root = std::path::PathBuf::from("archives").join(group);

                // 1. Find all potential repo paths (root + epochs)
                let mut candidate_paths = Vec::new();

                // Check epochs first (most likely for recent messages)
                if let Ok(mut entries) = tokio::fs::read_dir(&repo_root).await {
                    let mut epochs = Vec::new();
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        if let Ok(ft) = entry.file_type().await
                            && ft.is_dir()
                            && let Ok(name) = entry.file_name().into_string()
                            && let Ok(num) = name.parse::<i32>()
                        {
                            epochs.push(num);
                        }
                    }
                    epochs.sort_by(|a, b| b.cmp(a)); // Descending

                    for epoch in epochs {
                        candidate_paths.push(repo_root.join(epoch.to_string()));
                    }
                }

                // Add root as fallback
                candidate_paths.push(repo_root.clone());

                // 2. Search for blob
                for path in candidate_paths {
                    if let Ok(raw) = crate::git_ops::read_blob(&path, hash).await
                        && let Ok((metadata, _)) = crate::patch::parse_email(&raw)
                    {
                        details.body = Some(metadata.body);
                        break;
                    }
                }
            }
            Ok(Json(details))
        }
        Ok(None) => {
            info!("Message not found: {}", query.id);
            Err(StatusCode::NOT_FOUND)
        }
        Err(e) => {
            info!("Database error: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_stats(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let val = state
        .stats_cache
        .get_or_fetch(|| async {
            let messages = state.db.count_messages(None, None).await.unwrap_or(0);
            let patchsets = state.db.count_patchsets(None, None).await.unwrap_or(0);
            let counts = state
                .db
                .get_patchset_counts_by_status()
                .await
                .unwrap_or_default();

            let pending = *counts.get("Pending").unwrap_or(&0);
            let reviewing =
                *counts.get("In Review").unwrap_or(&0) + *counts.get("Applying").unwrap_or(&0);
            let reviewed = *counts.get("Reviewed").unwrap_or(&0);
            let failed = *counts.get("Failed").unwrap_or(&0);
            let failed_to_apply = *counts.get("Failed To Apply").unwrap_or(&0);
            let incomplete = *counts.get("Incomplete").unwrap_or(&0);
            let cancelled = *counts.get("Cancelled").unwrap_or(&0);

            Ok::<serde_json::Value, StatusCode>(serde_json::json!({
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
                "messages": messages,
                "patchsets": patchsets,
                "breakdown": {
                    "pending": pending,
                    "reviewing": reviewing,
                    "reviewed": reviewed,
                    "failed": failed,
                    "failed_to_apply": failed_to_apply,
                    "incomplete": incomplete,
                    "cancelled": cancelled
                }
            }))
        })
        .await?;

    Ok(Json(val))
}

async fn stats_timeline(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SubsystemQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let data = state
        .stats_timeline_cache
        .get_or_fetch(params.subsystem_id, || async {
            state
                .db
                .get_timeline_stats(params.subsystem_id)
                .await
                .map_err(|e| {
                    info!("Error getting timeline stats: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })
        })
        .await?;
    Ok(Json(data))
}

async fn stats_reviews(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let data = state
        .stats_reviews_cache
        .get_or_fetch(|| async {
            state.db.get_review_stats().await.map_err(|e| {
                info!("Error getting review stats: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })
        })
        .await?;
    Ok(Json(data))
}

async fn stats_tools(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let data = state
        .stats_tools_cache
        .get_or_fetch(|| async {
            state.db.get_tool_usage_stats().await.map_err(|e| {
                info!("Error getting tool stats: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })
        })
        .await?;
    Ok(Json(data))
}

async fn rerun_patchset(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Query(query): Query<PatchQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if state.read_only {
        return Err(StatusCode::FORBIDDEN);
    }

    if !state.allow_all_submit && !addr.ip().is_loopback() {
        return Err(StatusCode::FORBIDDEN);
    }

    let id = query
        .id
        .parse::<i64>()
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    state.db.rerun_patchset(id).await.map_err(|e| {
        error!("Failed to rerun patchset {}: {}", id, e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(serde_json::json!({ "status": "accepted" })))
}

async fn rerun_patch(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Query(query): Query<RerunPatchQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if state.read_only {
        return Err(StatusCode::FORBIDDEN);
    }

    if !state.allow_all_submit && !addr.ip().is_loopback() {
        return Err(StatusCode::FORBIDDEN);
    }

    state
        .db
        .rerun_patch(query.patchset_id, query.patch_id)
        .await
        .map_err(|e| {
            error!(
                "Failed to rerun patch {} in patchset {}: {}",
                query.patch_id, query.patchset_id, e
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(serde_json::json!({ "status": "accepted" })))
}
