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

//! A pass-through [`AiProvider`] decorator that retries with backoff on
//! rate-limit and transient (e.g. 503 "overloaded") errors.
//!
//! The daemon classifies provider errors and backs off before retrying them.
//! A worker running reviews in-process calls the provider directly, so its only
//! recourse is the stage loop retrying immediately and blindly. This decorator
//! gives that path the same behaviour, reusing [`QuotaManager`] and the typed
//! [`AiErrorClass`] classification the providers already produce:
//!
//! - `RateLimit` (429 / quota): account-wide, so it is reported to a shared
//!   [`QuotaManager`] and every concurrent request waits out the window.
//! - `Transient` (503 overloaded, 500/502/504, 529, and transport failures
//!   such as a refused or reset connection, a timeout or a response body cut
//!   off): a momentary failure, so only this call backs off, exponentially
//!   and with jitter so concurrent stages do not resynchronise onto the same
//!   retry instant.
//! - `Fatal` (including requests that cannot be built or that hit a redirect
//!   loop): propagated immediately.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::time::sleep;
use tracing::warn;

use crate::ai::quota::QuotaManager;
use crate::ai::{
    AiErrorClass, AiProvider, AiProviderDecorator, AiRequest, AiResponse, CacheStats,
    ProviderCapabilities, classify_ai_error, get_log_prefix,
};

/// Maximum number of attempts (initial try plus retries) for a single call
/// before the last error is propagated. Bounds wall-clock so a sustained
/// outage cannot hang a review indefinitely; transient blips resolve well
/// within this.
const MAX_ATTEMPTS: u32 = 6;

/// Lets a caller account for the time spent waiting on the shared quota gate
/// and stop the retry loop early.
///
/// The daemon bounds a review by an activity deadline rather than by an attempt
/// count, and credits rate-limit pacing waits back only when no sibling request
/// is actively in flight, failing once the deadline passes.
pub trait RetryBudget: Send + Sync {
    /// Time spent blocked on the shared quota gate before an attempt.
    fn credit_wait(&self, slept: Duration);
    /// Return an error to abandon further retries.
    fn check(&self) -> Result<()>;
    /// Called when an underlying provider request starts.
    fn on_request_start(&self) {}
    /// Called when an underlying provider request finishes.
    fn on_request_end(&self) {}
    /// Called when waiting on a quota gate or semaphore starts.
    fn on_wait_start(&self) {}
    /// Called when waiting on a quota gate or semaphore ends.
    fn on_wait_end(&self) {}
    /// Returns the current deadline, flushing any pending idle wait.
    fn current_deadline(&self) -> Option<tokio::time::Instant> {
        None
    }
}

#[derive(Default)]
struct ActiveTracker {
    active_count: usize,
    waiting_count: usize,
    idle_wait_since: Option<tokio::time::Instant>,
    last_credit_end: Option<tokio::time::Instant>,
    last_active_end: Option<tokio::time::Instant>,
}

impl ActiveTracker {
    fn flush_idle_wait_at(
        &mut self,
        now: tokio::time::Instant,
        deadline: &mut tokio::time::Instant,
    ) {
        if self.active_count == 0
            && let Some(since) = self.idle_wait_since
            && now > since
        {
            let uncredited = now - since;
            if uncredited >= Duration::from_millis(1) {
                *deadline += uncredited;
                self.last_credit_end = Some(now);
                self.idle_wait_since = Some(now);
            }
        }
    }

    fn on_wait_start_at(&mut self, now: tokio::time::Instant, deadline: &mut tokio::time::Instant) {
        self.flush_idle_wait_at(now, deadline);
        if self.waiting_count == 0 && self.active_count == 0 {
            let since = self.last_credit_end.map_or(now, |end| now.max(end));
            self.idle_wait_since = Some(since);
        }
        self.waiting_count += 1;
    }

    fn on_wait_end_at(&mut self, now: tokio::time::Instant, deadline: &mut tokio::time::Instant) {
        self.flush_idle_wait_at(now, deadline);
        self.waiting_count = self.waiting_count.saturating_sub(1);
        if self.waiting_count == 0 {
            self.idle_wait_since = None;
        }
    }

    fn on_request_start_at(
        &mut self,
        now: tokio::time::Instant,
        deadline: &mut tokio::time::Instant,
    ) {
        self.flush_idle_wait_at(now, deadline);
        self.idle_wait_since = None;
        self.active_count += 1;
    }

    fn on_request_end_at(
        &mut self,
        now: tokio::time::Instant,
        _deadline: &mut tokio::time::Instant,
    ) {
        self.last_active_end = Some(now);
        self.active_count = self.active_count.saturating_sub(1);
        if self.active_count == 0 && self.waiting_count > 0 {
            let since = self.last_credit_end.map_or(now, |end| now.max(end));
            self.idle_wait_since = Some(since);
        }
    }

    fn credit_wait_at(
        &mut self,
        now: tokio::time::Instant,
        slept: Duration,
        deadline: &mut tokio::time::Instant,
    ) {
        self.flush_idle_wait_at(now, deadline);
        if slept.is_zero() || self.active_count > 0 {
            return;
        }
        let sleep_start = now.checked_sub(slept).unwrap_or(now);
        let mut effective_start = match self.last_credit_end {
            Some(end) if end > sleep_start => end,
            _ => sleep_start,
        };
        if let Some(active_end) = self.last_active_end
            && active_end > effective_start
        {
            effective_start = active_end;
        }
        if now > effective_start {
            let uncredited = now - effective_start;
            if uncredited >= Duration::from_millis(1) {
                *deadline += uncredited;
                self.last_credit_end = Some(now);
            }
        }
    }
}

/// A [`RetryBudget`] backed by a wall-clock deadline shared with the caller.
///
/// Time spent parked on the quota gate is credited back only when no sibling
/// request of the review is actively in flight, so waiting out a rate limit while
/// completely idle does not consume the caller's budget.
pub struct DeadlineBudget {
    deadline: Arc<std::sync::Mutex<tokio::time::Instant>>,
    tracker: std::sync::Mutex<ActiveTracker>,
}

impl DeadlineBudget {
    pub fn new(deadline: Arc<std::sync::Mutex<tokio::time::Instant>>) -> Self {
        Self {
            deadline,
            tracker: std::sync::Mutex::new(ActiveTracker::default()),
        }
    }

    pub fn current_deadline_at(&self, now: tokio::time::Instant) -> tokio::time::Instant {
        let mut deadline = self.deadline.lock().unwrap();
        let mut tracker = self.tracker.lock().unwrap();
        tracker.flush_idle_wait_at(now, &mut deadline);
        *deadline
    }

    pub fn current_deadline(&self) -> tokio::time::Instant {
        self.current_deadline_at(tokio::time::Instant::now())
    }

    pub fn active_requests(&self) -> usize {
        self.tracker.lock().unwrap().active_count
    }

    #[cfg(test)]
    pub fn on_wait_start_at(&self, now: tokio::time::Instant) {
        let mut d = self.deadline.lock().unwrap();
        self.tracker.lock().unwrap().on_wait_start_at(now, &mut d);
    }

    #[cfg(test)]
    pub fn on_wait_end_at(&self, now: tokio::time::Instant) {
        let mut d = self.deadline.lock().unwrap();
        self.tracker.lock().unwrap().on_wait_end_at(now, &mut d);
    }

    #[cfg(test)]
    pub fn on_request_start_at(&self, now: tokio::time::Instant) {
        let mut d = self.deadline.lock().unwrap();
        self.tracker
            .lock()
            .unwrap()
            .on_request_start_at(now, &mut d);
    }

    #[cfg(test)]
    pub fn on_request_end_at(&self, now: tokio::time::Instant) {
        let mut d = self.deadline.lock().unwrap();
        self.tracker.lock().unwrap().on_request_end_at(now, &mut d);
    }

    #[cfg(test)]
    pub fn credit_wait_at(&self, now: tokio::time::Instant, slept: Duration) {
        let mut d = self.deadline.lock().unwrap();
        self.tracker
            .lock()
            .unwrap()
            .credit_wait_at(now, slept, &mut d);
    }
}

impl RetryBudget for DeadlineBudget {
    fn on_wait_start(&self) {
        let now = tokio::time::Instant::now();
        let mut d = self.deadline.lock().unwrap();
        self.tracker.lock().unwrap().on_wait_start_at(now, &mut d);
    }

    fn on_wait_end(&self) {
        let now = tokio::time::Instant::now();
        let mut d = self.deadline.lock().unwrap();
        self.tracker.lock().unwrap().on_wait_end_at(now, &mut d);
    }

    fn on_request_start(&self) {
        let now = tokio::time::Instant::now();
        let mut d = self.deadline.lock().unwrap();
        self.tracker
            .lock()
            .unwrap()
            .on_request_start_at(now, &mut d);
    }

    fn on_request_end(&self) {
        let now = tokio::time::Instant::now();
        let mut d = self.deadline.lock().unwrap();
        self.tracker.lock().unwrap().on_request_end_at(now, &mut d);
    }

    fn credit_wait(&self, slept: Duration) {
        let now = tokio::time::Instant::now();
        let mut d = self.deadline.lock().unwrap();
        self.tracker
            .lock()
            .unwrap()
            .credit_wait_at(now, slept, &mut d);
    }

    fn check(&self) -> Result<()> {
        let current = self.current_deadline();
        if tokio::time::Instant::now() > current {
            return Err(anyhow::anyhow!(
                "Review tool timed out (active time exceeded)"
            ));
        }
        Ok(())
    }

    fn current_deadline(&self) -> Option<tokio::time::Instant> {
        Some(self.current_deadline())
    }
}

pub(crate) struct QuotaWaitGuard<'a>(pub(crate) &'a dyn RetryBudget);

impl<'a> Drop for QuotaWaitGuard<'a> {
    fn drop(&mut self) {
        self.0.on_wait_end();
    }
}

pub(crate) struct RequestActiveGuard<'a>(pub(crate) &'a dyn RetryBudget);

impl<'a> Drop for RequestActiveGuard<'a> {
    fn drop(&mut self) {
        self.0.on_request_end();
    }
}

/// Adds rate-limit and transient retry with backoff around an inner provider.
pub struct BackoffProvider {
    inner: Arc<dyn AiProvider>,
    /// Shared across the run, so one rate-limit response backs every
    /// concurrent request off together.
    quota: Arc<QuotaManager>,
    /// Base unit of the exponential transient backoff. Tests use a tiny value
    /// so they run in real time.
    base_delay: Duration,
    /// Attempt ceiling, or None to retry until `budget` says to stop.
    max_attempts: Option<u32>,
    budget: Option<Arc<dyn RetryBudget>>,
}

impl BackoffProvider {
    /// With a `budget`, retries until it reports the caller's deadline has
    /// passed. Without one, falls back to a fixed attempt ceiling, which bounds
    /// wall-clock for callers that have no deadline of their own.
    pub fn new(
        inner: Arc<dyn AiProvider>,
        quota: Arc<QuotaManager>,
        budget: Option<Arc<dyn RetryBudget>>,
    ) -> Self {
        Self {
            inner,
            quota,
            base_delay: Duration::from_secs(1),
            max_attempts: budget.is_none().then_some(MAX_ATTEMPTS),
            budget,
        }
    }
}

#[async_trait]
impl AiProvider for BackoffProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let mut attempt: u32 = 0;
        let mut transient_streak: i32 = 0;
        loop {
            // Honour any active global rate-limit window before trying. The
            // wait is reported so a caller can keep it off its own deadline.
            let slept = {
                let _wait_guard = self.budget.as_ref().map(|b| {
                    b.on_wait_start();
                    QuotaWaitGuard(b.as_ref())
                });
                if self.inner.handles_dispatch_gate() {
                    self.quota.wait_for_cooldown().await
                } else {
                    self.quota.wait_for_access().await
                }
            };
            if let Some(budget) = &self.budget {
                budget.credit_wait(slept);
                budget.check()?;
            }

            let response_res = {
                let _active_guard = (!self.inner.handles_active_budget())
                    .then(|| {
                        self.budget.as_ref().map(|b| {
                            b.on_request_start();
                            RequestActiveGuard(b.as_ref())
                        })
                    })
                    .flatten();
                self.inner.generate_content(request.clone()).await
            };

            match response_res {
                Ok(response) => {
                    self.quota.report_success().await;
                    let actual_tokens = response.usage.as_ref().map_or(0, |u| u.uncached_tokens());
                    self.quota.record_tokens(actual_tokens);
                    return Ok(response);
                }
                Err(e) => {
                    attempt += 1;
                    if let Some(max) = self.max_attempts
                        && attempt >= max
                    {
                        return Err(e);
                    }
                    match classify_ai_error(&e) {
                        AiErrorClass::RateLimit { retry_after } => {
                            // Account-wide: block every concurrent request
                            // until it clears. The next iteration's
                            // wait_for_access() performs the sleep.
                            self.quota.report_quota_error(retry_after).await;
                        }
                        AiErrorClass::Transient { retry_after } => {
                            // Server-side blip. Exponential backoff with
                            // jitter, floored by any server-suggested delay.
                            // Per-call, not global.
                            transient_streak += 1;
                            let mult = 2.0_f64.powi(transient_streak - 1).min(60.0);
                            let backoff = self.base_delay.mul_f64(mult).max(retry_after);
                            let jittered = backoff + backoff.mul_f64(0.25 * fastrand::f64());
                            let retry_target = match self.max_attempts {
                                Some(max) => format!("{attempt}/{max}"),
                                None => format!("{attempt}"),
                            };
                            warn!(
                                "{}Transient AI error (streak {}); backing off {:.1}s then retry {}: {}",
                                get_log_prefix(),
                                transient_streak,
                                jittered.as_secs_f64(),
                                retry_target,
                                e
                            );
                            sleep(jittered).await;
                        }
                        AiErrorClass::Fatal => return Err(e),
                    }
                }
            }
        }
    }

    fn map_cached_inner(
        &self,
        decorator: &mut Option<AiProviderDecorator>,
    ) -> Option<Arc<dyn AiProvider>> {
        let rewrapped = self.inner.map_cached_inner(decorator)?;
        Some(Arc::new(Self {
            inner: rewrapped,
            quota: self.quota.clone(),
            base_delay: self.base_delay,
            max_attempts: self.max_attempts,
            budget: self.budget.clone(),
        }))
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        self.inner.get_capabilities()
    }

    fn cache_identity(&self) -> String {
        self.inner.cache_identity()
    }

    fn cache_stats(&self) -> Option<CacheStats> {
        self.inner.cache_stats()
    }

    async fn forget(&self, request: &AiRequest) {
        self.inner.forget(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::gemini::GeminiError;
    use crate::ai::{AiMessage, AiRole};
    use std::sync::atomic::{AtomicU32, Ordering};

    enum Behaviour {
        Transient,
        RateLimit,
        Fatal,
    }

    struct MockProvider {
        calls: AtomicU32,
        fail_times: u32,
        behaviour: Behaviour,
        rate_limit_after: Duration,
    }

    #[async_trait]
    impl AiProvider for MockProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_times {
                let err = match self.behaviour {
                    Behaviour::Transient => {
                        GeminiError::TransientError(Duration::from_secs(0), "503 overloaded".into())
                    }
                    Behaviour::RateLimit => GeminiError::QuotaExceeded(self.rate_limit_after),
                    Behaviour::Fatal => GeminiError::PermissionDenied("nope".into()),
                };
                return Err(err.into());
            }
            Ok(AiResponse {
                content: Some("ok".into()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                usage: None,
                truncated: false,
            })
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "mock".into(),
                context_window_size: 1000,
            }
        }
    }

    fn mock(
        behaviour: Behaviour,
        fail_times: u32,
        rate_limit_after: Duration,
    ) -> Arc<MockProvider> {
        Arc::new(MockProvider {
            calls: AtomicU32::new(0),
            fail_times,
            behaviour,
            rate_limit_after,
        })
    }

    /// A BackoffProvider with a tiny base delay so transient backoff runs in
    /// real time without needing the tokio virtual clock.
    fn fast(inner: Arc<dyn AiProvider>) -> BackoffProvider {
        BackoffProvider {
            inner,
            quota: Arc::new(QuotaManager::new()),
            base_delay: Duration::from_millis(1),
            max_attempts: Some(MAX_ATTEMPTS),
            budget: None,
        }
    }

    fn dummy_request() -> AiRequest {
        AiRequest {
            system: None,
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("hi".into()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
            reasoning_effort: None,
        }
    }

    #[tokio::test]
    async fn transient_backs_off_then_succeeds() {
        let m = mock(Behaviour::Transient, 3, Duration::ZERO);
        let resp = fast(m.clone())
            .generate_content(dummy_request())
            .await
            .unwrap();
        assert_eq!(resp.content.as_deref(), Some("ok"));
        assert_eq!(m.calls.load(Ordering::SeqCst), 4); // 3 transient failures + success
    }

    #[tokio::test]
    async fn transient_gives_up_after_max_attempts() {
        let m = mock(Behaviour::Transient, u32::MAX, Duration::ZERO);
        let err = fast(m.clone()).generate_content(dummy_request()).await;
        assert!(err.is_err());
        assert_eq!(m.calls.load(Ordering::SeqCst), MAX_ATTEMPTS); // bounded
    }

    #[tokio::test]
    async fn fatal_propagates_without_retry() {
        let m = mock(Behaviour::Fatal, u32::MAX, Duration::ZERO);
        let err = fast(m.clone()).generate_content(dummy_request()).await;
        assert!(err.is_err());
        assert_eq!(m.calls.load(Ordering::SeqCst), 1); // no retry on fatal
    }

    #[tokio::test]
    async fn expired_budget_stops_before_calling() {
        struct Expired;
        impl RetryBudget for Expired {
            fn credit_wait(&self, _slept: Duration) {}
            fn check(&self) -> Result<()> {
                Err(anyhow::anyhow!("deadline exceeded"))
            }
        }
        let m = mock(Behaviour::Transient, u32::MAX, Duration::ZERO);
        let provider = BackoffProvider::new(
            m.clone(),
            Arc::new(QuotaManager::new()),
            Some(Arc::new(Expired)),
        );
        assert!(provider.generate_content(dummy_request()).await.is_err());
        // The budget gates the loop, so the inner provider is never reached.
        assert_eq!(m.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rate_limit_waits_then_succeeds() {
        // QuotaManager uses std::Instant, so use a tiny real delay here.
        let m = mock(Behaviour::RateLimit, 2, Duration::from_millis(5));
        let resp = fast(m.clone())
            .generate_content(dummy_request())
            .await
            .unwrap();
        assert_eq!(resp.content.as_deref(), Some("ok"));
        assert_eq!(m.calls.load(Ordering::SeqCst), 3); // 2 rate-limit failures + success
    }

    #[tokio::test]
    async fn deadline_budget_credit_wait_deduplicates_overlapping_sleeps() {
        let deadline = Arc::new(std::sync::Mutex::new(
            tokio::time::Instant::now() + Duration::from_secs(60),
        ));
        let budget = DeadlineBudget::new(deadline.clone());

        // First sleep of 10s credits approximately 10s.
        budget.credit_wait(Duration::from_secs(10));
        let d1 = *deadline.lock().unwrap();

        // An immediately following call reporting the same 10s sleep should not add another 10s.
        budget.credit_wait(Duration::from_secs(10));
        let d2 = *deadline.lock().unwrap();
        assert_eq!(d1, d2);

        // Zero sleep should not affect deadline.
        budget.credit_wait(Duration::ZERO);
        let d3 = *deadline.lock().unwrap();
        assert_eq!(d2, d3);
    }

    #[test]
    fn test_credit_wait_does_not_extend_deadline_during_active_requests() {
        let deadline = Arc::new(std::sync::Mutex::new(
            tokio::time::Instant::now() + Duration::from_secs(10),
        ));
        let budget = DeadlineBudget::new(deadline.clone());

        // When a sibling request is active
        budget.on_request_start();
        let initial_deadline = *deadline.lock().unwrap();

        // Calling credit_wait while active_requests > 0 must NOT extend the deadline
        budget.credit_wait(Duration::from_secs(5));
        assert_eq!(*deadline.lock().unwrap(), initial_deadline);

        budget.on_request_end();
    }

    #[test]
    fn test_credit_wait_clamps_to_last_active_end() {
        let base = tokio::time::Instant::now();
        let deadline = Arc::new(std::sync::Mutex::new(base + Duration::from_secs(10)));
        let budget = DeadlineBudget::new(deadline.clone());

        budget.on_request_start();
        budget.on_request_end_at(base + Duration::from_millis(500));

        let initial_deadline = *deadline.lock().unwrap();
        // Calling credit_wait_at with a sleep duration that predates active_end
        // gives no credit because effective_start is clamped to active_end.
        budget.credit_wait_at(
            base + Duration::from_millis(500),
            Duration::from_millis(500),
        );
        assert_eq!(*deadline.lock().unwrap(), initial_deadline);

        // Calling credit_wait_at at base + 600ms for a sleep of 500ms credits only the 100ms
        // elapsed after active_end.
        budget.credit_wait_at(
            base + Duration::from_millis(600),
            Duration::from_millis(500),
        );
        let advanced = *deadline.lock().unwrap();
        assert_eq!(advanced, initial_deadline + Duration::from_millis(100));
    }

    #[test]
    fn test_staggered_waiters_credit_full_idle_interval() {
        let base = tokio::time::Instant::now();
        let deadline = Arc::new(std::sync::Mutex::new(base + Duration::from_secs(10)));
        let budget = DeadlineBudget::new(deadline.clone());

        // Waiter 1 starts waiting at t1 = base
        budget.on_wait_start_at(base);
        // Waiter 2 starts waiting at t2 = base + 200ms
        budget.on_wait_start_at(base + Duration::from_millis(200));

        // Waiter 2 wakes at t3 = base + 500ms and starts request
        budget.on_wait_end_at(base + Duration::from_millis(500));
        budget.on_request_start_at(base + Duration::from_millis(500));

        // Waiter 1 wakes at t3 = base + 500ms and ends wait
        budget.on_wait_end_at(base + Duration::from_millis(500));

        // The entire [base, base + 500ms] idle window should be credited (500ms)
        let advanced = *deadline.lock().unwrap();
        assert_eq!(
            advanced,
            base + Duration::from_secs(10) + Duration::from_millis(500)
        );
    }

    #[tokio::test]
    async fn test_concurrency_limited_waiter_does_not_increment_active_until_permit() {
        use crate::ai::concurrency_limited_provider::ConcurrencyLimitedProvider;
        use tokio::sync::Semaphore;

        let base = tokio::time::Instant::now();
        let deadline = Arc::new(std::sync::Mutex::new(base + Duration::from_secs(10)));
        let budget = Arc::new(DeadlineBudget::new(deadline.clone()));
        let sem = Arc::new(Semaphore::new(0));

        let m = mock(Behaviour::Fatal, 0, Duration::ZERO);
        let limited = Arc::new(
            ConcurrencyLimitedProvider::new(m, sem.clone()).with_budget(Some(budget.clone())),
        );

        let lim = limited.clone();
        let handle = tokio::spawn(async move { lim.generate_content(dummy_request()).await });

        tokio::task::yield_now().await;
        assert_eq!(budget.active_requests(), 0);

        sem.add_permits(1);
        let _ = handle.await;
        assert_eq!(budget.active_requests(), 0);
    }

    #[tokio::test]
    async fn test_backoff_provider_records_tokens_and_tracks_active_requests() {
        struct UsageMock {
            budget: Arc<DeadlineBudget>,
            fail: bool,
        }

        #[async_trait]
        impl AiProvider for UsageMock {
            async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
                // While inside generate_content, active_requests must be 1.
                assert_eq!(self.budget.active_requests(), 1);
                if self.fail {
                    return Err(GeminiError::PermissionDenied("fatal".into()).into());
                }
                Ok(AiResponse {
                    content: Some("ok".into()),
                    thought: None,
                    thought_signature: None,
                    tool_calls: None,
                    usage: Some(crate::ai::AiUsage {
                        prompt_tokens: 100,
                        completion_tokens: 50,
                        total_tokens: 150,
                        cached_tokens: None,
                    }),
                    truncated: false,
                })
            }
            fn get_capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities {
                    model_name: "mock".into(),
                    context_window_size: 1000,
                }
            }
        }

        let deadline = Arc::new(std::sync::Mutex::new(
            tokio::time::Instant::now() + Duration::from_secs(60),
        ));
        let budget = Arc::new(DeadlineBudget::new(deadline));
        // Configure quota with max_tpm = 100 and a 150ms window.
        let quota = Arc::new(QuotaManager::with_limits_and_window(
            None,
            Some(100),
            Duration::from_millis(150),
        ));
        let provider = BackoffProvider::new(
            Arc::new(UsageMock {
                budget: budget.clone(),
                fail: false,
            }),
            quota.clone(),
            Some(budget.clone()),
        );

        // Before call, active_requests must be 0.
        assert_eq!(budget.active_requests(), 0);
        let t0 = std::time::Instant::now();
        let resp = provider.generate_content(dummy_request()).await.unwrap();
        assert_eq!(resp.content.as_deref(), Some("ok"));
        // After call, active_requests must be back to 0.
        assert_eq!(budget.active_requests(), 0);

        // 150 tokens were recorded, exceeding the 100 TPM limit.
        // The next wait_for_access() must throttle until the window passes.
        let _ = tokio::time::timeout(Duration::from_secs(5), quota.wait_for_access())
            .await
            .unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(140));

        // Test error case: active_requests is decremented even on failure.
        let err_provider = BackoffProvider::new(
            Arc::new(UsageMock {
                budget: budget.clone(),
                fail: true,
            }),
            quota.clone(),
            Some(budget.clone()),
        );
        let err = err_provider.generate_content(dummy_request()).await;
        assert!(err.is_err());
        assert_eq!(budget.active_requests(), 0);
    }
}
