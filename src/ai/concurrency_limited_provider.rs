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
}

impl ConcurrencyLimitedProvider {
    /// The semaphore is shared, so every provider built from it draws on the
    /// same pool of permits.
    pub fn new(inner: Arc<dyn AiProvider>, semaphore: Arc<Semaphore>) -> Self {
        Self { inner, semaphore }
    }
}

#[async_trait]
impl AiProvider for ConcurrencyLimitedProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|e| anyhow::anyhow!("concurrency semaphore closed: {e}"))?;
        self.inner.generate_content(request).await
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
}
