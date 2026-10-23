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

//! A pass-through [`AiProvider`] decorator that caps the number of concurrent
//! `generate_content` calls against a shared semaphore.
//!
//! A review fans its analysis stages out concurrently, so a single patch can
//! issue one model call per stage at once, and the worker reviews several
//! patches in parallel on top of that. The daemon bounds this globally with its
//! own LLM semaphore, but a worker running reviews in-process has nothing in
//! front of it. Sharing one semaphore across every provider the run creates
//! turns `[review] concurrency` into a real ceiling on in-flight model calls.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Semaphore;

use crate::ai::{AiProvider, AiRequest, AiResponse, CacheStats, ProviderCapabilities};

/// How many concurrent model calls a review `concurrency` allows.
///
/// Derived from the shape of a review workflow: its analysis stages run as one
/// parallel fan-out, whose width the planning stage chooses per patch, followed
/// by consolidation stages that run sequentially. An active review therefore
/// averages roughly three concurrent calls, so scaling to `concurrency * 3`
/// saturates model capacity while worktrees and worker processes stay gated at
/// `concurrency` itself. The factor is empirical rather than a bound the
/// workflow guarantees.
///
/// A configuration asking for no parallelism stays fully serial rather than
/// being widened.
pub fn llm_permits(concurrency: usize) -> usize {
    if concurrency < 2 { 1 } else { concurrency * 3 }
}

/// Limits concurrent model calls to the permits of a shared semaphore. All
/// other behaviour is delegated unchanged to the inner provider.
pub struct ConcurrencyLimitedProvider {
    inner: Arc<dyn AiProvider>,
    semaphore: Arc<Semaphore>,
    budget: Option<Arc<dyn crate::ai::backoff_provider::RetryBudget>>,
    quota: Option<Arc<crate::ai::quota::QuotaManager>>,
}

impl ConcurrencyLimitedProvider {
    /// The semaphore is shared, so every provider built from it draws on the
    /// same pool of permits.
    pub fn new(inner: Arc<dyn AiProvider>, semaphore: Arc<Semaphore>) -> Self {
        Self {
            inner,
            semaphore,
            budget: None,
            quota: None,
        }
    }

    pub fn with_budget(
        mut self,
        budget: Option<Arc<dyn crate::ai::backoff_provider::RetryBudget>>,
    ) -> Self {
        self.budget = budget;
        self
    }

    pub fn with_quota(mut self, quota: Option<Arc<crate::ai::quota::QuotaManager>>) -> Self {
        self.quota = quota;
        self
    }
}

#[async_trait]
impl AiProvider for ConcurrencyLimitedProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let _permit = {
            let _wait_guard = self.budget.as_ref().map(|b| {
                b.on_wait_start();
                crate::ai::backoff_provider::QuotaWaitGuard(b.as_ref())
            });

            loop {
                if let Some(quota) = &self.quota {
                    quota.wait_for_cooldown().await;
                }
                if let Some(budget) = &self.budget {
                    budget.check()?;
                }

                let permit = self
                    .semaphore
                    .acquire()
                    .await
                    .map_err(|e| anyhow::anyhow!("concurrency semaphore closed: {e}"))?;

                if let Some(quota) = &self.quota {
                    let check = || {
                        if let Some(budget) = &self.budget {
                            budget.check()?;
                        }
                        Ok(())
                    };
                    match quota.wait_for_pacing_checked(check).await? {
                        Some(_) => break permit,
                        None => {
                            drop(permit);
                            continue;
                        }
                    }
                } else {
                    break permit;
                }
            }
        };

        if let Some(budget) = &self.budget {
            budget.check()?;
        }
        let _active_guard = self.budget.as_ref().map(|b| {
            b.on_request_start();
            crate::ai::backoff_provider::RequestActiveGuard(b.as_ref())
        });
        self.inner.generate_content(request).await
    }

    fn handles_active_budget(&self) -> bool {
        self.budget.is_some() || self.inner.handles_active_budget()
    }

    fn handles_dispatch_gate(&self) -> bool {
        self.quota.is_some() || self.inner.handles_dispatch_gate()
    }

    fn map_cached_inner(
        &self,
        decorator: &mut Option<crate::ai::AiProviderDecorator>,
    ) -> Option<Arc<dyn AiProvider>> {
        let rewrapped = self.inner.map_cached_inner(decorator)?;
        Some(Arc::new(Self {
            inner: rewrapped,
            semaphore: self.semaphore.clone(),
            budget: self.budget.clone(),
            quota: self.quota.clone(),
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

    #[test]
    fn test_llm_permits_keeps_a_serial_configuration_serial() {
        // Widening these would defeat the point of asking for no parallelism,
        // which is what someone does to stay under a provider's limits.
        assert_eq!(llm_permits(0), 1);
        assert_eq!(llm_permits(1), 1);

        // Above that, calls are allowed to run wider than the worktrees are.
        assert_eq!(llm_permits(2), 6);
        assert_eq!(llm_permits(16), 48);
    }

    #[tokio::test]
    async fn test_concurrency_limited_provider_delegates_map_cached_inner() {
        use anyhow::Result;
        use async_trait::async_trait;

        struct MockCache {
            inner: Arc<dyn AiProvider>,
        }
        #[async_trait]
        impl AiProvider for MockCache {
            fn map_cached_inner(
                &self,
                decorator: &mut Option<crate::ai::AiProviderDecorator>,
            ) -> Option<Arc<dyn AiProvider>> {
                let f = decorator.take()?;
                Some(Arc::new(Self {
                    inner: f(self.inner.clone()),
                }))
            }
            async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
                Ok(AiResponse {
                    content: Some("from-cache".into()),
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
                    context_window_size: 100,
                }
            }
        }

        struct MockNet;
        #[async_trait]
        impl AiProvider for MockNet {
            async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
                panic!("network called");
            }
            fn get_capabilities(&self) -> ProviderCapabilities {
                ProviderCapabilities {
                    model_name: "mock".into(),
                    context_window_size: 100,
                }
            }
        }

        let cache: Arc<dyn AiProvider> = Arc::new(MockCache {
            inner: Arc::new(MockNet),
        });
        let sem = Arc::new(Semaphore::new(1));
        let limited = Arc::new(ConcurrencyLimitedProvider::new(cache, sem));
        let decorated = crate::ai::decorate_uncached(limited, |p| {
            Arc::new(ConcurrencyLimitedProvider::new(
                p,
                Arc::new(Semaphore::new(0)),
            ))
        });
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            decorated.generate_content(AiRequest::default()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(resp.content.as_deref(), Some("from-cache"));
    }

    #[tokio::test(start_paused = true)]
    async fn test_pacing_occurs_after_semaphore_acquire() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        struct CountingMock {
            calls: AtomicUsize,
        }
        #[async_trait]
        impl AiProvider for CountingMock {
            async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
                self.calls.fetch_add(1, Ordering::SeqCst);
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

        let inner = Arc::new(CountingMock {
            calls: AtomicUsize::new(0),
        });
        let sem = Arc::new(Semaphore::new(1));
        // max_rps = 2.0 -> 500ms pacing interval
        let quota = Arc::new(crate::ai::quota::QuotaManager::with_limits(Some(2.0), None));
        let provider = ConcurrencyLimitedProvider::new(inner.clone(), sem).with_quota(Some(quota));

        // First call goes through immediately
        let resp = provider
            .generate_content(AiRequest::default())
            .await
            .unwrap();
        assert_eq!(resp.content.as_deref(), Some("ok"));
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);

        // Second call must pace for 500ms
        let start = tokio::time::Instant::now();
        let resp2 = provider
            .generate_content(AiRequest::default())
            .await
            .unwrap();
        assert_eq!(resp2.content.as_deref(), Some("ok"));
        assert_eq!(inner.calls.load(Ordering::SeqCst), 2);
        assert!(tokio::time::Instant::now().duration_since(start) >= Duration::from_millis(500));
    }
}
