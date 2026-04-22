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

use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{info, warn};

pub struct QuotaManager {
    // Stores the time when we can resume making requests.
    // If None or in the past, we are free to go.
    blocked_until: Mutex<Option<Instant>>,
    // Track consecutive transient errors for exponential backoff.
    consecutive_transient_errors: Mutex<u32>,
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
            consecutive_transient_errors: Mutex::new(0),
        }
    }

    pub async fn wait_for_access(&self) -> Duration {
        let mut total_slept = Duration::ZERO;
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
                    "{}Global AI rate limit/quota active. Waiting for {:.2}s...",
                    crate::ai::get_log_prefix(),
                    duration.as_secs_f64()
                );
                tokio::time::sleep(duration).await;
                total_slept += duration;
            } else {
                break;
            }
        }
        total_slept
    }

    pub async fn report_success(&self) {
        let mut count = self.consecutive_transient_errors.lock().await;
        if *count > 0 {
            *count = 0;
            *self.blocked_until.lock().await = None;
            info!("AI request succeeded, resetting transient error backoff.");
        }
    }

    pub async fn report_quota_error(&self, retry_after: Duration) {
        let mut guard = self.blocked_until.lock().await;
        let resume_time = Instant::now() + retry_after;

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

    pub async fn report_transient_error(&self) {
        let mut count_guard = self.consecutive_transient_errors.lock().await;
        *count_guard += 1;
        let count = *count_guard;

        // Exponential backoff: 1s, 2s, 4s, 8s... capped at 60s
        let backoff_secs = (1.0 * (2.0_f64.powi((count - 1) as i32))).min(60.0);
        let backoff = Duration::from_secs_f64(backoff_secs);

        let mut block_guard = self.blocked_until.lock().await;
        let resume_time = Instant::now() + backoff;

        if let Some(current) = *block_guard {
            if resume_time > current {
                *block_guard = Some(resume_time);
            }
        } else {
            *block_guard = Some(resume_time);
        }

        warn!(
            "AI provider transient error (streak: {}). Globally backing off for {:.2}s",
            count, backoff_secs
        );
    }
}
