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

use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackoffType {
    Exponential,
    Flat,
}

pub struct QuotaManager {
    // Stores the time when we can resume making requests.
    // If None or in the past, we are free to go.
    blocked_until: Mutex<Option<Instant>>,
    // Track consecutive transient errors for exponential backoff.
    consecutive_transient_errors: Mutex<u32>,
    transient_backoff_type: BackoffType,
    transient_flat_delay: Duration,
}

impl Default for QuotaManager {
    fn default() -> Self {
        Self::new()
    }
}

impl QuotaManager {
    pub fn new() -> Self {
        Self::new_with_settings(BackoffType::Exponential, Duration::from_secs(30))
    }

    pub fn new_with_settings(
        transient_backoff_type: BackoffType,
        transient_flat_delay: Duration,
    ) -> Self {
        Self {
            blocked_until: Mutex::new(None),
            consecutive_transient_errors: Mutex::new(0),
            transient_backoff_type,
            transient_flat_delay: transient_flat_delay.max(Duration::from_secs(1)),
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
        let delay = retry_after.max(Duration::from_secs(1));
        let mut guard = self.blocked_until.lock().await;
        let resume_time = Instant::now() + delay;

        if let Some(current) = *guard {
            if resume_time > current {
                *guard = Some(resume_time);
            }
        } else {
            *guard = Some(resume_time);
        }

        warn!(
            "Quota exhausted! Blocking all LLM requests for {:.2}s",
            delay.as_secs_f64()
        );
    }

    pub async fn report_transient_error(&self) {
        let mut count_guard = self.consecutive_transient_errors.lock().await;
        *count_guard += 1;
        let count = *count_guard;

        let backoff = match self.transient_backoff_type {
            BackoffType::Exponential => {
                let backoff_secs = (1.0 * (2.0_f64.powi((count - 1) as i32))).min(60.0);
                Duration::from_secs_f64(backoff_secs).max(Duration::from_secs(1))
            }
            BackoffType::Flat => self.transient_flat_delay,
        };

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
            count,
            backoff.as_secs_f64()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_quota_manager_flat_backoff() {
        tokio::time::pause(); // Pause the real clock
        let manager = Arc::new(QuotaManager::new_with_settings(
            BackoffType::Flat,
            Duration::from_secs(5),
        ));

        // First transient error -> triggers 5s flat delay
        manager.report_transient_error().await;

        let manager_clone = manager.clone();
        let handle = tokio::spawn(async move { manager_clone.wait_for_access().await });

        // Yield execution to let the spawned task run and hit the sleep
        tokio::task::yield_now().await;
        // Sleep a tiny virtual duration to ensure the task has transitioned to sleeping
        tokio::time::sleep(Duration::from_millis(1)).await;

        // Fast-forward 5s instantly
        tokio::time::advance(Duration::from_secs(5)).await;
        let slept = handle.await.unwrap();

        // It should have slept for at least 5s (capped at the total advanced time)
        assert!(slept >= Duration::from_secs(5));
    }

    #[tokio::test]
    async fn test_quota_manager_exponential_backoff() {
        tokio::time::pause();
        let manager = Arc::new(QuotaManager::new_with_settings(
            BackoffType::Exponential,
            Duration::from_secs(30),
        ));

        // Attempt 1 -> 1s delay
        manager.report_transient_error().await;
        let manager_clone = manager.clone();
        let handle1 = tokio::spawn(async move { manager_clone.wait_for_access().await });
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(handle1.await.unwrap() >= Duration::from_secs(1));

        // Attempt 2 -> 2s delay
        manager.report_transient_error().await;
        let manager_clone = manager.clone();
        let handle2 = tokio::spawn(async move { manager_clone.wait_for_access().await });
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(handle2.await.unwrap() >= Duration::from_secs(2));

        // Attempt 3 -> 4s delay
        manager.report_transient_error().await;
        let manager_clone = manager.clone();
        let handle3 = tokio::spawn(async move { manager_clone.wait_for_access().await });
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(handle3.await.unwrap() >= Duration::from_secs(4));
    }

    #[tokio::test]
    async fn test_quota_manager_min_delay_cap() {
        tokio::time::pause();
        // Configure with 0s flat delay (invalid, should be capped at 1s)
        let manager = Arc::new(QuotaManager::new_with_settings(
            BackoffType::Flat,
            Duration::from_secs(0),
        ));

        manager.report_transient_error().await;

        let manager_clone = manager.clone();
        let handle = tokio::spawn(async move { manager_clone.wait_for_access().await });
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        let slept = handle.await.unwrap();

        assert!(slept >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn test_quota_manager_report_success_resets() {
        tokio::time::pause();
        let manager = Arc::new(QuotaManager::new_with_settings(
            BackoffType::Exponential,
            Duration::from_secs(30),
        ));

        // Attempt 1 -> 1s delay
        manager.report_transient_error().await;

        // Report success -> resets
        manager.report_success().await;

        // Attempt 1 (after reset) -> should be 1s again (not 2s)
        manager.report_transient_error().await;
        let manager_clone = manager.clone();
        let handle = tokio::spawn(async move { manager_clone.wait_for_access().await });
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(handle.await.unwrap() >= Duration::from_secs(1));
    }
}
