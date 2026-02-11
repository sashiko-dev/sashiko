use axum::{
    Router,
    extract::{Path, Query, Request, State},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
};
use serde::Deserialize;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tracing::{error, info};

#[derive(Deserialize)]
struct Params {
    key: String,
}

#[derive(Clone)]
struct AppState {
    real_base_url: String,
    client: reqwest::Client,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let real_base_url = std::env::var("REAL_GEMINI_URL")
        .unwrap_or_else(|_| "https://generativelanguage.googleapis.com".to_string());

    let state = AppState {
        real_base_url,
        client: reqwest::Client::new(),
    };

    let app = Router::new()
        // Use wildcard to capture everything under models/
        .route("/v1beta/models/{*path}", post(handle_proxy))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();
    info!(
        "Gemini Proxy listening on {}",
        listener.local_addr().unwrap()
    );
    axum::serve(listener, app).await.unwrap();
}

async fn handle_proxy(
    State(state): State<AppState>,
    Path(path): Path<String>,
    Query(params): Query<Params>,
    req: Request,
) -> impl IntoResponse {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros();

    info!("Intercepted request for path: {}", path);

    // 1. Read Request Body
    let bytes = axum::body::to_bytes(req.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap(); // 10MB limit
    let req_json: Value = serde_json::from_slice(&bytes).unwrap();

    // 2. Save Request
    let req_filename = format!("tests/data/traces/trace_{}_req.json", timestamp);
    let mut file = File::create(&req_filename).await.unwrap();
    file.write_all(&bytes).await.unwrap();
    info!("Saved request to {}", req_filename);

    // 3. Forward to Real API
    let url = format!(
        "{}/v1beta/models/{}?key={}",
        state.real_base_url, path, params.key
    );

    let res = state.client.post(&url).json(&req_json).send().await;

    match res {
        Ok(api_resp) => {
            let status = api_resp.status();
            let resp_bytes = api_resp.bytes().await.unwrap();

            // 4. Save Response
            let resp_filename = format!("tests/data/traces/trace_{}_resp.json", timestamp);
            let mut file = File::create(&resp_filename).await.unwrap();
            file.write_all(&resp_bytes).await.unwrap();
            info!("Saved response to {}", resp_filename);

            // 5. Return Response
            (status, axum::body::Body::from(resp_bytes)).into_response()
        }
        Err(e) => {
            error!("Upstream error: {}", e);
            (StatusCode::BAD_GATEWAY, e.to_string()).into_response()
        }
    }
}
