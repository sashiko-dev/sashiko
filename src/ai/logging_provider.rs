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

//! A pass-through [`AiProvider`] decorator that logs each model turn — the
//! request that is sent and the response that comes back — at INFO level.
//!
//! This is the single, shared implementation of per-turn logging. The review
//! worker (`worker::prompts`) drives the same stage loop in both local-CLI and
//! daemon modes, and every model call funnels through `generate_content`, so
//! wrapping the provider here logs turns identically for both paths. It is
//! enabled by the `[ai] log_turns` setting and wired in at `bin/review.rs`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use async_trait::async_trait;
use tracing::info;

use crate::ai::{AiProvider, AiRequest, AiResponse, CacheStats, ProviderCapabilities};

/// Wraps any [`AiProvider`], logging each request/response turn. All other
/// behaviour is delegated unchanged to the inner provider.
pub struct LoggingProvider {
    inner: Arc<dyn AiProvider>,
    turn: AtomicU64,
}

impl LoggingProvider {
    pub fn new(inner: Arc<dyn AiProvider>) -> Self {
        Self {
            inner,
            turn: AtomicU64::new(0),
        }
    }
}

#[async_trait]
impl AiProvider for LoggingProvider {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let turn = self.turn.fetch_add(1, Ordering::SeqCst) + 1;
        // The worker tags requests with their patch context (e.g. "[ps:0 p:1] ").
        let tag = request.context_tag.clone().unwrap_or_default();

        // Log the outgoing request (its most recent message).
        let n_msgs = request.messages.len();
        if let Some(last) = request.messages.last() {
            let role = format!("{:?}", last.role).to_lowercase();
            if let Some(tool_calls) = &last.tool_calls {
                let names: Vec<&str> = tool_calls
                    .iter()
                    .map(|t| t.function_name.as_str())
                    .collect();
                info!("{tag}→ Turn {turn} ({n_msgs} msgs): [{role}] tool_calls={names:?}");
            } else {
                let content = last.content.as_deref().unwrap_or("(no text content)");
                let preview: String = content.chars().take(300).collect();
                let ellipsis = if content.chars().count() > 300 { "…" } else { "" };
                info!("{tag}→ Turn {turn} ({n_msgs} msgs): [{role}] {preview}{ellipsis}");
            }
        }

        let response = self.inner.generate_content(request).await?;

        // Log the response: text, any tool calls, and token usage.
        if let Some(content) = &response.content {
            let preview: String = content.chars().take(500).collect();
            let ellipsis = if content.chars().count() > 500 { "…" } else { "" };
            info!("{tag}← Turn {turn} text: {preview}{ellipsis}");
        }
        if let Some(tool_calls) = &response.tool_calls {
            for call in tool_calls {
                let args = call.arguments.to_string();
                let preview: String = args.chars().take(200).collect();
                let ellipsis = if args.chars().count() > 200 { "…" } else { "" };
                info!(
                    "{tag}← Turn {turn} tool_call: {}({preview}{ellipsis})",
                    call.function_name
                );
            }
        }
        if let Some(usage) = &response.usage {
            info!(
                "{tag}← Turn {turn} tokens: in={} out={} cached={}",
                usage.prompt_tokens,
                usage.completion_tokens,
                usage.cached_tokens.unwrap_or(0)
            );
        }

        Ok(response)
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
