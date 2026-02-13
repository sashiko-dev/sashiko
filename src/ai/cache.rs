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

use crate::ai::{AiMessage, AiProvider, AiRequest, AiRole, AiTool};
use crate::worker::prompts::PromptRegistry;
use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

pub struct CacheManager {
    prompts: PromptRegistry,
    provider: Arc<dyn AiProvider>,
    model: String,
    ttl: String,
    tools: Option<Vec<AiTool>>,
}

impl CacheManager {
    pub fn new(
        base_dir: PathBuf,
        provider: Arc<dyn AiProvider>,
        model: String,
        ttl: String,
        tools: Option<Vec<AiTool>>,
    ) -> Self {
        Self {
            prompts: PromptRegistry::new(base_dir),
            provider,
            model,
            ttl,
            tools,
        }
    }

    /// Builds the full context string from prompts directory.
    /// Delegates to PromptRegistry.
    async fn build_context(&self) -> Result<String> {
        self.prompts.build_context().await
    }

    /// Calculates hash of content and tools for cache key.
    /// Delegates to PromptRegistry.
    fn calculate_hash(&self, content: &str) -> String {
        let mut full_content = content.to_string();
        full_content.push_str(&self.model);
        self.prompts
            .calculate_content_hash(&full_content, self.tools.as_deref())
    }

    /// Ensures a valid cache exists for the current content.
    /// Returns the cache resource name (e.g., "cachedContents/123...").
    /// If `ignore_cache_name` is provided, any existing cache with that name will be deleted and ignored.
    pub async fn ensure_cache(&self, ignore_cache_name: Option<&str>) -> Result<String> {
        let context_str = self.build_context().await?;
        let hash = self.calculate_hash(&context_str);
        // Short hash for readability
        let short_hash = &hash[..8];
        let expected_display_name = format!("sashiko-reviewer-v1-{}", short_hash);

        // List existing caches
        let existing = self.provider.list_context_caches().await?;
        let mut valid_candidate = None;

        if let Some(ignore) = ignore_cache_name {
            tracing::info!("EnsureCache: Requested to ignore/delete: '{}'", ignore);
        } else {
            tracing::info!("EnsureCache: No ignore target specified.");
        }

        for (display_name, name) in existing {
            if display_name == expected_display_name {
                if let Some(ignore) = ignore_cache_name {
                    if name == ignore {
                        tracing::warn!(
                            "Deleting/Ignoring cache '{}' (MATCHED ignore target)",
                            name
                        );
                        if let Err(e) = self.provider.delete_context_cache(&name).await {
                            tracing::warn!("Failed to delete ignored cache {}: {}", name, e);
                        }
                        continue;
                    }
                }

                if valid_candidate.is_none() {
                    valid_candidate = Some(name.clone());
                } else {
                    tracing::debug!("Found duplicate valid cache candidate: {}", name);
                }
            }
        }

        if let Some(name) = valid_candidate {
            tracing::info!("Found existing cache: {} ({})", name, expected_display_name,);
            return Ok(name);
        }

        tracing::info!("Creating new cache: {}", expected_display_name);

        // Create new cache
        let request = AiRequest {
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some(context_str),
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: self.tools.clone(),
            temperature: None,
            preloaded_context: None,
        };

        self.provider
            .create_context_cache(request, self.ttl.clone(), Some(expected_display_name))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiProvider, AiRequest, AiResponse, ProviderCapabilities};
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    struct MockProvider {
        created_request: Arc<Mutex<Option<AiRequest>>>,
        existing: Vec<(String, String)>,
        deleted: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl AiProvider for MockProvider {
        async fn generate_content(&self, _request: AiRequest) -> Result<AiResponse> {
            unimplemented!()
        }

        fn estimate_tokens(&self, _request: &AiRequest) -> usize {
            0
        }

        fn get_capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                model_name: "test".to_string(),
                context_window_size: 100,
            }
        }

        async fn create_context_cache(
            &self,
            request: AiRequest,
            _ttl: String,
            _display_name: Option<String>,
        ) -> Result<String> {
            *self.created_request.lock().unwrap() = Some(request);
            Ok("cachedContents/test".to_string())
        }

        async fn list_context_caches(&self) -> Result<Vec<(String, String)>> {
            Ok(self.existing.clone())
        }

        async fn delete_context_cache(&self, name: &str) -> Result<()> {
            self.deleted.lock().unwrap().push(name.to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_ensure_cache_creates_with_correct_content() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_dir = temp_dir.path().to_path_buf();

        let captured = Arc::new(Mutex::new(None));
        let mock_provider = Arc::new(MockProvider {
            created_request: captured.clone(),
            existing: vec![],
            deleted: Arc::new(Mutex::new(vec![])),
        });

        let manager = CacheManager::new(
            base_dir,
            mock_provider,
            "test-model".to_string(),
            "60s".to_string(),
            None,
        );

        let res = manager.ensure_cache(None).await;
        assert!(res.is_ok());

        let request = captured
            .lock()
            .unwrap()
            .take()
            .expect("create_context_cache not called");
        assert!(request.messages.len() > 0);
    }

    #[tokio::test]
    async fn test_ensure_cache_finds_existing() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_dir = temp_dir.path().to_path_buf();

        let captured = Arc::new(Mutex::new(None));
        let mock_provider = Arc::new(MockProvider {
            created_request: captured.clone(),
            existing: vec![], // Will populate below
            deleted: Arc::new(Mutex::new(vec![])),
        });

        let manager = CacheManager::new(
            base_dir,
            mock_provider.clone(),
            "test-model".to_string(),
            "60s".to_string(),
            None,
        );

        let context_str = manager.build_context().await.unwrap();
        let hash = manager.calculate_hash(&context_str);
        let short_hash = &hash[..8];
        let expected_dn = format!("sashiko-reviewer-v1-{}", short_hash);

        // Now update the mock with existing cache
        let mock_provider_final = Arc::new(MockProvider {
            created_request: captured.clone(),
            existing: vec![(expected_dn, "cachedContents/existing".to_string())],
            deleted: Arc::new(Mutex::new(vec![])),
        });

        let manager = CacheManager::new(
            temp_dir.path().to_path_buf(),
            mock_provider_final,
            "test-model".to_string(),
            "60s".to_string(),
            None,
        );

        let res = manager.ensure_cache(None).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "cachedContents/existing");

        // Should NOT have created a new one
        assert!(captured.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_ensure_cache_deletes_ignored() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_dir = temp_dir.path().to_path_buf();

        let deleted = Arc::new(Mutex::new(vec![]));
        let captured = Arc::new(Mutex::new(None));

        let manager = CacheManager::new(
            base_dir,
            Arc::new(MockProvider {
                created_request: captured.clone(),
                existing: vec![],
                deleted: deleted.clone(),
            }),
            "test-model".to_string(),
            "60s".to_string(),
            None,
        );

        let context_str = manager.build_context().await.unwrap();
        let hash = manager.calculate_hash(&context_str);
        let short_hash = &hash[..8];
        let expected_dn = format!("sashiko-reviewer-v1-{}", short_hash);

        let mock_provider = Arc::new(MockProvider {
            created_request: captured.clone(),
            existing: vec![
                (expected_dn.clone(), "cachedContents/bad".to_string()),
                (expected_dn, "cachedContents/good".to_string()),
            ],
            deleted: deleted.clone(),
        });

        let manager = CacheManager::new(
            temp_dir.path().to_path_buf(),
            mock_provider,
            "test-model".to_string(),
            "60s".to_string(),
            None,
        );

        let res = manager.ensure_cache(Some("cachedContents/bad")).await;
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), "cachedContents/good");

        let deleted_list = deleted.lock().unwrap();
        assert_eq!(deleted_list.len(), 1);
        assert_eq!(deleted_list[0], "cachedContents/bad");
    }
}
