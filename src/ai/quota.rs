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

pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(5 * 60);

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
        let mut guard = self.blocked_until.lock().await;
        // An older request can succeed while another call's quota window
        // is still active. Its success must not admit new requests early.
        if guard.is_some_and(|until| until <= Instant::now()) {
            *guard = None;
            info!("AI request succeeded, resetting quota backoff.");
        }
    }

    pub async fn report_quota_error(&self, retry_after: Duration) {
        let retry_after = retry_after.min(MAX_RETRY_AFTER);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn success_preserves_active_quota_cooldown() {
        let quota = QuotaManager::new();
        quota.report_quota_error(Duration::from_secs(60)).await;
        quota.report_success().await;

        let wait = quota.wait_for_access();
        tokio::pin!(wait);
        assert!(futures::poll!(wait.as_mut()).is_pending());
    }

    #[tokio::test]
    async fn success_leaves_access_open_without_an_active_cooldown() {
        for deadline in [None, Some(Instant::now() - Duration::from_secs(1))] {
            let quota = QuotaManager::new();
            *quota.blocked_until.lock().await = deadline;
            quota.report_success().await;

            assert!(quota.blocked_until.lock().await.is_none());
            let wait = quota.wait_for_access();
            tokio::pin!(wait);
            assert_eq!(
                futures::poll!(wait.as_mut()),
                std::task::Poll::Ready(Duration::ZERO)
            );
        }
    }
}
