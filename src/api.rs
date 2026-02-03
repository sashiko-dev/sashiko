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

pub struct AppState {
    pub db: Arc<Database>,
    pub sender: mpsc::Sender<Event>,
    pub fetch_sender: mpsc::Sender<FetchRequest>,
}

#[derive(Deserialize)]
pub struct Pagination {
    pub page: Option<usize>,
    pub per_page: Option<usize>,
    pub q: Option<String>,
}

#[derive(Serialize)]
pub struct PatchsetsResponse {
    pub items: Vec<crate::db::PatchsetRow>,
    pub total: usize,
    pub page: usize,
    pub per_page: usize,
}

#[derive(Serialize)]
pub struct MessagesResponse {
    pub items: Vec<crate::db::MessageRow>,
    pub total: usize,
    pub page: usize,
    pub per_page: usize,
}

#[derive(Deserialize)]
pub struct PatchQuery {
    pub id: String,
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

#[derive(Deserialize)]
pub struct LocalPatch {
    pub subject: String,
    pub message: String,
    pub diff: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SubmitRequest {
    Local {
        author: String,
        subject: String,
        message: String,
        diff: String,
        base_commit: Option<String>,
    },
    #[serde(rename = "local-multiple")]
    LocalMultiple {
        author: String,
        base_commit: Option<String>,
        patches: Vec<LocalPatch>,
    },
    Remote {
        sha: String,
        repo: String,
    },
    #[serde(rename = "remote-range")]
    RemoteRange {
        sha: String,
        repo: String,
    },
}

#[derive(Serialize)]
pub struct SubmitResponse {
    pub status: String,
    pub id: String,
}

pub async fn run_server(
    settings: ServerSettings,
    db: Arc<Database>,
    sender: mpsc::Sender<Event>,
    fetch_sender: mpsc::Sender<FetchRequest>,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(AppState {
        db,
        sender,
        fetch_sender,
    });

    let app = Router::new()
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
    if !addr.ip().is_loopback() {
        info!("Refused patch submission from non-localhost: {}", addr);
        return Err(StatusCode::FORBIDDEN);
    }

    match payload {
        SubmitRequest::Local {
            author,
            subject,
            message,
            diff,
            base_commit,
        } => {
            let id = generate_synthetic_id("local");
            info!("Received local patch submission: {}", id);

            // Create a placeholder record so the user can track status immediately
            if let Err(e) = state.db.create_fetching_patchset(&id, &subject).await {
                error!("Failed to create placeholder for local patch: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }

            let event = Event::PatchSubmitted {
                group: "api-submit".to_string(),
                article_id: id.clone(),
                message_id: id.clone(),
                subject,
                author,
                message,
                diff,
                base_commit,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64,
                index: 1,
                total: 1,
            };

            if let Err(e) = state.sender.send(event).await {
                error!("Failed to send local patch to queue: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }

            Ok(Json(SubmitResponse {
                status: "accepted".to_string(),
                id,
            }))
        }
        SubmitRequest::LocalMultiple {
            author,
            base_commit,
            patches,
        } => {
            let total = patches.len();
            if total == 0 {
                return Err(StatusCode::BAD_REQUEST);
            }
            if total > 100 {
                return Err(StatusCode::PAYLOAD_TOO_LARGE);
            }

            let series_id = generate_synthetic_id("series");
            info!(
                "Received local series submission: {} ({} patches)",
                series_id, total
            );

            // Create a placeholder for the entire series
            let first_subject = &patches[0].subject;
            if let Err(e) = state
                .db
                .create_fetching_patchset(&series_id, first_subject)
                .await
            {
                error!("Failed to create placeholder for local series: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64;

            for (i, patch) in patches.into_iter().enumerate() {
                let patch_id = format!("{}-{}", series_id, i + 1);
                let event = Event::PatchSubmitted {
                    group: "api-submit".to_string(),
                    article_id: series_id.clone(),
                    message_id: patch_id,
                    subject: patch.subject,
                    author: author.clone(),
                    message: patch.message,
                    diff: patch.diff,
                    base_commit: base_commit.clone(),
                    timestamp: now,
                    index: (i + 1) as u32,
                    total: total as u32,
                };

                if let Err(e) = state.sender.send(event).await {
                    error!("Failed to send series patch {} to queue: {}", i + 1, e);
                    // We continue anyway, some might have succeeded
                }
            }

            Ok(Json(SubmitResponse {
                status: "accepted".to_string(),
                id: series_id,
            }))
        }
        SubmitRequest::Remote { sha, repo } | SubmitRequest::RemoteRange { sha, repo } => {
            let id = sha.clone();
            info!("Received remote fetch request: {} from {}", sha, repo);

            // Create a placeholder record in the DB so the user can track status
            if let Err(e) = state
                .db
                .create_fetching_patchset(&id, &format!("Fetching {} from {}...", &sha, &repo))
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
    }
}

async fn list_patchsets(
    State(state): State<Arc<AppState>>,
    Query(pagination): Query<Pagination>,
) -> Result<Json<PatchsetsResponse>, StatusCode> {
    let page = pagination.page.unwrap_or(1).max(1);
    let per_page = pagination.per_page.unwrap_or(50).clamp(1, 100);
    let offset = (page - 1) * per_page;

    let items = state
        .db
        .get_patchsets(per_page, offset, pagination.q.clone())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let total = state
        .db
        .count_patchsets(pagination.q.clone())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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

    let items = state
        .db
        .get_messages(per_page, offset, pagination.q.clone())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let total = state
        .db
        .count_messages(pagination.q.clone())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

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
        state.db.get_patchset_details(id_val).await
    } else {
        info!("Fetching details for patchset msgid: {}", query.id);
        state.db.get_patchset_details_by_msgid(&query.id).await
    };

    match result {
        Ok(Some(details)) => Ok(Json(details)),
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
    Query(query): Query<PatchQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if let Ok(id_val) = query.id.parse::<i64>() {
        info!("Fetching details for review id: {}", id_val);
        match state.db.get_review_details(id_val).await {
            Ok(Some(details)) => Ok(Json(details)),
            Ok(None) => {
                info!("Review not found: {}", id_val);
                Err(StatusCode::NOT_FOUND)
            }
            Err(e) => {
                info!("Database error: {}", e);
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    } else {
        Err(StatusCode::BAD_REQUEST)
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
            if details.body.is_none() || details.body.as_deref() == Some("") {
                if let (Some(hash), Some(group)) = (&details.git_blob_hash, &details.mailing_list) {
                    let repo_root = std::path::PathBuf::from("archives").join(group);

                    // 1. Find all potential repo paths (root + epochs)
                    let mut candidate_paths = Vec::new();

                    // Check epochs first (most likely for recent messages)
                    if let Ok(mut entries) = tokio::fs::read_dir(&repo_root).await {
                        let mut epochs = Vec::new();
                        while let Ok(Some(entry)) = entries.next_entry().await {
                            if let Ok(ft) = entry.file_type().await {
                                if ft.is_dir() {
                                    if let Ok(name) = entry.file_name().into_string() {
                                        if let Ok(num) = name.parse::<i32>() {
                                            epochs.push(num);
                                        }
                                    }
                                }
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
                        if let Ok(raw) = crate::git_ops::read_blob(&path, hash).await {
                            if let Ok((metadata, _)) = crate::patch::parse_email(&raw) {
                                details.body = Some(metadata.body);
                                break;
                            }
                        }
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

async fn get_stats(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let messages = state.db.count_messages(None).await.unwrap_or(0);
    let patchsets = state.db.count_patchsets(None).await.unwrap_or(0);
    let counts = state
        .db
        .get_patchset_counts_by_status()
        .await
        .unwrap_or_default();

    let pending = *counts.get("Pending").unwrap_or(&0);
    let applying = *counts.get("Applying").unwrap_or(&0);
    let reviewing = *counts.get("In Review").unwrap_or(&0); // DB uses "In Review"
    let reviewed = *counts.get("Reviewed").unwrap_or(&0);
    let failed = *counts.get("Failed").unwrap_or(&0);
    let failed_to_apply = *counts.get("Failed To Apply").unwrap_or(&0);
    let incomplete = *counts.get("Incomplete").unwrap_or(&0);
    let cancelled = *counts.get("Cancelled").unwrap_or(&0);

    Json(serde_json::json!({
        "status": "ok",
        "version": "0.1.0",
        "messages": messages,
        "patchsets": patchsets,
        "breakdown": {
            "pending": pending,
            "applying": applying,
            "reviewing": reviewing,
            "reviewed": reviewed,
            "failed": failed,
            "failed_to_apply": failed_to_apply,
            "incomplete": incomplete,
            "cancelled": cancelled
        }
    }))
}

async fn stats_timeline(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SubsystemQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let data = state
        .db
        .get_timeline_stats(params.subsystem_id)
        .await
        .map_err(|e| {
            info!("Error getting timeline stats: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(data))
}

async fn stats_reviews(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let data = state.db.get_review_stats().await.map_err(|e| {
        info!("Error getting review stats: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(data))
}

async fn stats_tools(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let data = state.db.get_tool_usage_stats().await.map_err(|e| {
        info!("Error getting tool stats: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(data))
}

async fn rerun_patchset(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Query(query): Query<PatchQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !addr.ip().is_loopback() {
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
    if !addr.ip().is_loopback() {
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
