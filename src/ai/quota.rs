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

use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{info, warn};

pub(crate) const MAX_RETRY_AFTER: Duration = Duration::from_secs(5 * 60);
const MAX_PACING_INTERVAL: Duration = Duration::from_secs(86_400);
const DEFAULT_TPM_WINDOW: Duration = Duration::from_secs(60);

#[derive(Default)]
struct RateLimitState {
    next_allowed_at: Option<Instant>,
    token_window: VecDeque<(Instant, usize)>,
    window_tokens: usize,
}

impl RateLimitState {
    fn prune_expired(&mut self, now: Instant, window: Duration) {
        while let Some(&(_ts, tokens)) = self.token_window.front() {
            if now.saturating_duration_since(_ts) >= window {
                self.token_window.pop_front();
                self.window_tokens = self.window_tokens.saturating_sub(tokens);
            } else {
                break;
            }
        }
    }

    fn required_wait(
        &mut self,
        now: Instant,
        min_interval: Option<Duration>,
        max_tpm: Option<usize>,
        tpm_window: Duration,
    ) -> Option<Duration> {
        self.prune_expired(now, tpm_window);

        let rps_wait = min_interval
            .and(self.next_allowed_at)
            .filter(|&until| until > now)
            .map(|until| until - now);

        let tpm_wait = max_tpm.and_then(|limit| {
            if self.window_tokens < limit {
                return None;
            }
            let mut remaining = self.window_tokens;
            for &(ts, tokens) in &self.token_window {
                remaining = remaining.saturating_sub(tokens);
                if remaining < limit {
                    let expires_at = ts + tpm_window;
                    return (expires_at > now).then(|| expires_at - now);
                }
            }
            None
        });

        match (rps_wait, tpm_wait) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }
}

pub struct QuotaManager {
    // Stores the time when we can resume making requests.
    // If None or in the past, we are free to go.
    blocked_until: Mutex<Option<Instant>>,
    // Serializes dispatch pacing so only one request waits and reserves the next slot.
    dispatch_gate: Mutex<()>,
    // Pacing interval derived from `max_rps` (1.0 / max_rps).
    min_interval: Option<Duration>,
    // Client-side tokens-per-minute ceiling across uncached input and output tokens.
    max_tpm: Option<usize>,
    // Sliding window duration for TPM tracking (default 60s).
    tpm_window: Duration,
    // Rate limit state protected by a synchronous mutex (never held across .await).
    rate_state: StdMutex<RateLimitState>,
}

impl Default for QuotaManager {
    fn default() -> Self {
        Self::new()
    }
}

impl QuotaManager {
    pub fn new() -> Self {
        Self::with_limits(None, None)
    }

    pub fn with_limits(max_rps: Option<f64>, max_tpm: Option<usize>) -> Self {
        Self::with_limits_and_window(max_rps, max_tpm, DEFAULT_TPM_WINDOW)
    }

    pub fn with_limits_and_window(
        max_rps: Option<f64>,
        max_tpm: Option<usize>,
        tpm_window: Duration,
    ) -> Self {
        let min_interval = match max_rps {
            Some(r) if r < 0.0 || !r.is_finite() => {
                warn!("Ignoring invalid max_rps value {r:?}; must be finite and non-negative");
                None
            }
            Some(0.0) => None,
            Some(r) => {
                Duration::try_from_secs_f64((1.0 / r).min(MAX_PACING_INTERVAL.as_secs_f64()))
                    .ok()
                    .filter(|d| !d.is_zero())
                    .map(|d| d.min(MAX_PACING_INTERVAL))
            }
            None => None,
        };
        Self {
            blocked_until: Mutex::new(None),
            dispatch_gate: Mutex::new(()),
            min_interval,
            max_tpm: max_tpm.filter(|&t| t > 0),
            tpm_window,
            rate_state: StdMutex::new(RateLimitState::default()),
        }
    }

    pub fn record_tokens(&self, tokens: usize) {
        if self.max_tpm.is_none() || tokens == 0 {
            return;
        }
        if let Ok(mut state) = self.rate_state.lock() {
            let now = Instant::now();
            state.prune_expired(now, self.tpm_window);
            state.token_window.push_back((now, tokens));
            state.window_tokens = state.window_tokens.saturating_add(tokens);
        }
    }

    pub async fn is_in_cooldown(&self) -> bool {
        let guard = self.blocked_until.lock().await;
        if let Some(until) = *guard {
            until > Instant::now()
        } else {
            false
        }
    }

    pub async fn wait_for_cooldown(&self) -> Duration {
        let wait_start = Instant::now();
        let mut slept_any = false;

        loop {
            let quota_sleep = {
                let guard = self.blocked_until.lock().await;
                if let Some(until) = *guard {
                    let now = Instant::now();
                    if until > now { Some(until - now) } else { None }
                } else {
                    None
                }
            };

            if let Some(duration) = quota_sleep {
                info!(
                    "{}Global AI rate limit/quota active. Waiting for {:.2}s...",
                    crate::ai::get_log_prefix(),
                    duration.as_secs_f64()
                );
                tokio::time::sleep(duration).await;
                slept_any = true;
                continue;
            }

            break;
        }

        if slept_any {
            wait_start.elapsed()
        } else {
            Duration::ZERO
        }
    }

    pub async fn wait_for_pacing_checked<F>(&self, check: F) -> anyhow::Result<Option<Duration>>
    where
        F: Fn() -> anyhow::Result<()>,
    {
        if self.min_interval.is_none() && self.max_tpm.is_none() {
            check()?;
            return Ok(Some(Duration::ZERO));
        }

        let _gate = self.dispatch_gate.lock().await;
        check()?;

        if self.is_in_cooldown().await {
            return Ok(None);
        }

        let wait_start = Instant::now();
        let mut slept_any = false;

        loop {
            if self.is_in_cooldown().await {
                return Ok(None);
            }

            let rate_wait = {
                let mut state = self.rate_state.lock().unwrap();
                state.required_wait(
                    Instant::now(),
                    self.min_interval,
                    self.max_tpm,
                    self.tpm_window,
                )
            };

            if let Some(duration) = rate_wait {
                tracing::debug!(
                    "{}AI rate-limit pacing active. Waiting for {:.2}s...",
                    crate::ai::get_log_prefix(),
                    duration.as_secs_f64()
                );
                tokio::time::sleep(duration).await;
                slept_any = true;
                check()?;
                continue;
            }

            break;
        }

        {
            let mut state = self.rate_state.lock().unwrap();
            if let Some(interval) = self.min_interval {
                state.next_allowed_at = Instant::now().checked_add(interval);
            }
        }

        let slept = if slept_any {
            Some(wait_start.elapsed())
        } else {
            Some(Duration::ZERO)
        };
        Ok(slept)
    }

    pub async fn wait_for_pacing(&self) -> Option<Duration> {
        self.wait_for_pacing_checked(|| Ok(()))
            .await
            .unwrap_or(None)
    }

    pub async fn wait_for_access(&self) -> Duration {
        let start = Instant::now();
        let mut slept_any = false;
        loop {
            let cooldown_slept = self.wait_for_cooldown().await;
            if !cooldown_slept.is_zero() {
                slept_any = true;
            }
            match self.wait_for_pacing().await {
                Some(pacing_slept) => {
                    if !pacing_slept.is_zero() {
                        slept_any = true;
                    }
                    break;
                }
                None => {
                    slept_any = true;
                    continue;
                }
            }
        }
        if slept_any {
            start.elapsed()
        } else {
            Duration::ZERO
        }
    }

    pub async fn report_success(&self) {
        let mut guard = self.blocked_until.lock().await;
        if guard.is_some() {
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

    #[test]
    fn test_quota_manager_zero_limits_disabled() {
        let quota = QuotaManager::with_limits(Some(0.0), Some(0));
        assert!(quota.min_interval.is_none());
        assert!(quota.max_tpm.is_none());
    }

    #[tokio::test]
    async fn test_max_rps_paces_requests() {
        let quota = QuotaManager::with_limits(Some(20.0), None);
        let t0 = Instant::now();

        let s1 = tokio::time::timeout(Duration::from_secs(5), quota.wait_for_access())
            .await
            .unwrap();
        assert_eq!(s1, Duration::ZERO);

        let _ = tokio::time::timeout(Duration::from_secs(5), quota.wait_for_access())
            .await
            .unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(45));
    }

    #[tokio::test]
    async fn test_max_rps_is_cancellation_safe() {
        let quota = std::sync::Arc::new(QuotaManager::with_limits(Some(2.0), None));
        let t0 = Instant::now();

        let s1 = tokio::time::timeout(Duration::from_secs(5), quota.wait_for_access())
            .await
            .unwrap();
        assert_eq!(s1, Duration::ZERO);

        let q2 = quota.clone();
        let _ = tokio::time::timeout(Duration::from_millis(50), async move {
            q2.wait_for_access().await;
        })
        .await;

        let _ = tokio::time::timeout(Duration::from_secs(5), quota.wait_for_access())
            .await
            .unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(450));
        assert!(t0.elapsed() < Duration::from_millis(850));
    }

    #[tokio::test]
    async fn test_max_rps_honours_concurrent_quota_error() {
        let quota = QuotaManager::with_limits(Some(25.0), None);

        let s1 = tokio::time::timeout(Duration::from_secs(5), quota.wait_for_access())
            .await
            .unwrap();
        assert_eq!(s1, Duration::ZERO);

        let t0 = Instant::now();
        quota.report_quota_error(Duration::from_millis(100)).await;

        let _ = tokio::time::timeout(Duration::from_secs(5), quota.wait_for_access())
            .await
            .unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(90));
    }

    #[tokio::test]
    async fn test_max_tpm_throttles_when_window_exceeded() {
        let quota =
            QuotaManager::with_limits_and_window(None, Some(1000), Duration::from_millis(150));
        let t_rec = Instant::now();
        quota.record_tokens(1000);

        let _ = tokio::time::timeout(Duration::from_secs(5), quota.wait_for_access())
            .await
            .unwrap();

        assert!(t_rec.elapsed() >= Duration::from_millis(140));
    }

    #[tokio::test]
    async fn test_max_tpm_sliding_window_multi_entry_expiry() {
        let quota =
            QuotaManager::with_limits_and_window(None, Some(1000), Duration::from_millis(150));

        let t0 = Instant::now();
        quota.record_tokens(600);
        tokio::time::sleep(Duration::from_millis(30)).await;
        quota.record_tokens(600);

        let _ = tokio::time::timeout(Duration::from_secs(5), quota.wait_for_access())
            .await
            .unwrap();

        assert!(t0.elapsed() >= Duration::from_millis(140));
    }

    #[tokio::test]
    async fn test_combined_max_rps_and_max_tpm() {
        let quota = QuotaManager::with_limits_and_window(
            Some(20.0),
            Some(1000),
            Duration::from_millis(150),
        );

        let s1 = quota.wait_for_access().await;
        assert_eq!(s1, Duration::ZERO);
        let t0 = Instant::now();
        quota.record_tokens(1000);

        let _ = tokio::time::timeout(Duration::from_secs(5), quota.wait_for_access())
            .await
            .unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(140));
    }

    #[test]
    fn test_tiny_positive_max_rps_does_not_drop_pacing() {
        let quota = QuotaManager::with_limits(Some(1e-300), None);
        assert_eq!(quota.min_interval, Some(MAX_PACING_INTERVAL));
    }
}
