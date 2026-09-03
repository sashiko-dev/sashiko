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
//! - `Transient` (503 overloaded, 500/502/504, 529): a momentary server-side
//!   failure, so only this call backs off, exponentially and with jitter so
//!   concurrent stages do not resynchronise onto the same retry instant.
//! - `Fatal`: propagated immediately.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::time::sleep;
use tracing::warn;

use crate::ai::quota::QuotaManager;
use crate::ai::{
    AiErrorClass, AiProvider, AiRequest, AiResponse, CacheStats, ProviderCapabilities,
    classify_ai_error, get_log_prefix,
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
/// count, and does not want a rate-limit wait to consume that budget, so it
/// credits the wait back and fails once the deadline passes.
pub trait RetryBudget: Send + Sync {
    /// Time spent blocked on the shared quota gate before an attempt.
    fn credit_wait(&self, slept: Duration);
    /// Return an error to abandon further retries.
    fn check(&self) -> Result<()>;
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
            let slept = self.quota.wait_for_access().await;
            if let Some(budget) = &self.budget {
                budget.credit_wait(slept);
                budget.check()?;
            }

            match self.inner.generate_content(request.clone()).await {
                Ok(response) => {
                    self.quota.report_success().await;
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
                            warn!(
                                "{}Transient AI error (streak {}); backing off {:.1}s then retry {}/{}: {}",
                                get_log_prefix(),
                                transient_streak,
                                jittered.as_secs_f64(),
                                attempt,
                                MAX_ATTEMPTS,
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

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        self.inner.estimate_tokens(request)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        self.inner.get_capabilities()
    }

    fn cache_stats(&self) -> Option<CacheStats> {
        self.inner.cache_stats()
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

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            0
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
}
