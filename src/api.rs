use crate::db::Database;
use crate::settings::ServerSettings;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, get_service},
};
use serde::Deserialize;
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
}

pub async fn run_server(
    settings: ServerSettings,
    db: Arc<Database>,
) -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(AppState { db });

    let app = Router::new()
        .route("/api/patchsets", get(list_patchsets))
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
) -> Result<Json<Vec<crate::db::PatchsetRow>>, StatusCode> {
    let page = pagination.page.unwrap_or(1).max(1);
    let per_page = pagination.per_page.unwrap_or(50).clamp(1, 100);
    let offset = (page - 1) * per_page;

    match state.db.get_patchsets(per_page, offset).await {
        Ok(patchsets) => Ok(Json(patchsets)),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn get_stats() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": "0.1.0"
    }))
}
