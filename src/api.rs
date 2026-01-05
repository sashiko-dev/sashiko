use crate::db::Database;
use crate::settings::ServerSettings;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, get_service},
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::services::{ServeDir, ServeFile};
use tracing::info;

pub struct AppState {
    pub db: Arc<Database>,
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

pub async fn run_server(
    settings: ServerSettings,
    db: Arc<Database>,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(AppState { db });

    let app = Router::new()
        .route("/api/patchsets", get(list_patchsets))
        .route("/api/messages", get(list_messages))
        .route("/api/patch", get(get_patchset))
        .route("/api/message", get(get_message))
        .route("/api/stats", get(get_stats))
        .route("/", get_service(ServeFile::new("static/index.html")))
        .nest_service("/static", ServeDir::new("static"))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], settings.port));
    info!("Web API listening on {}", addr);

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
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
    let counts = state.db.get_patchset_counts_by_status().await.unwrap_or_default();

    let pending = *counts.get("Pending").unwrap_or(&0);
    let reviewing = *counts.get("Reviewing").unwrap_or(&0);
    let applied = *counts.get("Applied").unwrap_or(&0);
    let failed = *counts.get("Failed").unwrap_or(&0);
    let reviewed = applied + failed;

    Json(serde_json::json!({
        "status": "ok",
        "version": "0.1.0",
        "messages": messages,
        "patchsets": patchsets,
        "breakdown": {
            "pending": pending,
            "reviewing": reviewing,
            "reviewed": reviewed,
            "applied": applied,
            "failed": failed
        }
    }))
}
