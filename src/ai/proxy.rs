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

use crate::ai::gemini::{GeminiClient, GeminiError, GenerateContentRequest};
use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::IntoResponse,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

pub struct ProxyState {
    pub client: Arc<GeminiClient>,
    pub quota_manager: Arc<QuotaManager>,
}

pub struct QuotaManager {
    // Stores the time when we can resume making requests.
    // If None or in the past, we are free to go.
    blocked_until: Mutex<Option<Instant>>,
}

impl Default for QuotaManager {
    fn default() -> Self {
        Self::new()
    }
}

impl QuotaManager {
    pub fn new() -> Self {
        Self {
            blocked_until: Mutex::new(None),
        }
    }

    pub async fn wait_for_access(&self) {
        loop {
            let sleep_duration = {
                let guard = self.blocked_until.lock().await;
                if let Some(until) = *guard {
                    let now = Instant::now();
                    if until > now { Some(until - now) } else { None }
                } else {
                    None
                }
            };

            if let Some(duration) = sleep_duration {
                info!(
                    "Global quota exhausted. Waiting for {:.2}s...",
                    duration.as_secs_f64()
                );
                tokio::time::sleep(duration).await;
            } else {
                break;
            }
        }
    }

    pub async fn report_quota_error(&self, retry_after: Duration) {
        let mut guard = self.blocked_until.lock().await;
        let resume_time = Instant::now() + retry_after;

        // Only update if the new time is further in the future than existing
        if let Some(current) = *guard {
            if resume_time > current {
                *guard = Some(resume_time);
            }
        } else {
            *guard = Some(resume_time);
        }

        warn!(
            "Quota exhausted! Blocking all LLM requests for {:.2}s",
            retry_after.as_secs_f64()
        );
    }
}

pub async fn handle_generate(
    State(state): State<Arc<ProxyState>>,
    Json(request): Json<GenerateContentRequest>,
) -> impl IntoResponse {
    loop {
        // 1. Wait if globally blocked
        state.quota_manager.wait_for_access().await;

        // 2. Try request
        match state.client.generate_content_single(&request).await {
            Ok(response) => {
                return (StatusCode::OK, Json(response)).into_response();
            }
            Err(e) => {
                if let Some(GeminiError::QuotaExceeded(duration)) = e.downcast_ref::<GeminiError>()
                {
                    state.quota_manager.report_quota_error(*duration).await;
                    continue;
                }

                error!("Gemini Proxy Error: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response();
            }
        }
    }
}
