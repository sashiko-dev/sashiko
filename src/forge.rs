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

use anyhow::Result;
use axum::http::{HeaderMap, StatusCode};
use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Arc;

/// Metadata extracted from forge webhook
#[derive(Debug, Clone)]
pub struct ForgeMetadata {
    pub repo_url: Option<String>,
    pub base_sha: String,
    pub head_sha: String,
    pub pr_number: i64,
    pub pr_title: Option<String>,
    pub pr_url: Option<String>,
}

/// Trait for forge provider implementations
pub trait ForgeProvider: Send + Sync {
    /// Provider name (e.g., "GitHub", "GitLab")
    fn name(&self) -> &str;

    /// Validate webhook event from headers
    fn validate_event(&self, headers: &HeaderMap) -> Result<(), StatusCode>;

    /// Parse webhook payload and extract metadata
    fn parse_payload(&self, body: &Bytes) -> Result<(String, ForgeMetadata), StatusCode>;
}

/// GitHub forge provider
pub struct GitHubForge;

impl ForgeProvider for GitHubForge {
    fn name(&self) -> &str {
        "GitHub"
    }

    fn validate_event(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        let event = headers
            .get("x-github-event")
            .and_then(|v| v.to_str().ok())
            .ok_or(StatusCode::BAD_REQUEST)?;

        if event != "pull_request" {
            return Err(StatusCode::BAD_REQUEST);
        }

        Ok(())
    }

    fn parse_payload(&self, body: &Bytes) -> Result<(String, ForgeMetadata), StatusCode> {
        use serde_json::Value;

        let payload: Value = serde_json::from_slice(body).map_err(|_| StatusCode::BAD_REQUEST)?;

        let action = payload["action"]
            .as_str()
            .ok_or(StatusCode::BAD_REQUEST)?
            .to_string();

        let pr = &payload["pull_request"];
        if pr.is_null() {
            return Err(StatusCode::BAD_REQUEST);
        }

        let head_sha = pr["head"]["sha"]
            .as_str()
            .ok_or(StatusCode::BAD_REQUEST)?
            .to_string();

        let base_sha = pr["base"]["sha"]
            .as_str()
            .ok_or(StatusCode::BAD_REQUEST)?
            .to_string();

        let pr_number = pr["number"].as_i64().ok_or(StatusCode::BAD_REQUEST)?;

        let pr_title = pr["title"].as_str().map(|s| s.to_string());
        let pr_url = pr["html_url"].as_str().map(|s| s.to_string());

        let repo_url = payload["repository"]["clone_url"]
            .as_str()
            .map(|s| s.to_string());

        let metadata = ForgeMetadata {
            repo_url,
            base_sha,
            head_sha,
            pr_number,
            pr_title,
            pr_url,
        };

        Ok((action, metadata))
    }
}

/// GitLab forge provider
pub struct GitLabForge;

impl ForgeProvider for GitLabForge {
    fn name(&self) -> &str {
        "GitLab"
    }

    fn validate_event(&self, headers: &HeaderMap) -> Result<(), StatusCode> {
        let event = headers
            .get("x-gitlab-event")
            .and_then(|v| v.to_str().ok())
            .ok_or(StatusCode::BAD_REQUEST)?;

        if event != "Merge Request Hook" {
            return Err(StatusCode::BAD_REQUEST);
        }

        Ok(())
    }

    fn parse_payload(&self, body: &Bytes) -> Result<(String, ForgeMetadata), StatusCode> {
        use serde_json::Value;

        let payload: Value = serde_json::from_slice(body).map_err(|_| StatusCode::BAD_REQUEST)?;

        let action = payload["object_kind"]
            .as_str()
            .ok_or(StatusCode::BAD_REQUEST)?
            .to_string();

        let attrs = &payload["object_attributes"];
        if attrs.is_null() {
            return Err(StatusCode::BAD_REQUEST);
        }

        let head_sha = attrs["last_commit"]["id"]
            .as_str()
            .ok_or(StatusCode::BAD_REQUEST)?
            .to_string();

        // For GitLab, extract base_sha from diff_refs if available
        let base_sha = attrs["diff_refs"]["base_sha"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| head_sha.clone());

        let pr_number = attrs["iid"].as_i64().ok_or(StatusCode::BAD_REQUEST)?;

        let pr_title = attrs["title"].as_str().map(|s| s.to_string());
        let pr_url = attrs["url"].as_str().map(|s| s.to_string());

        let repo_url = payload["project"]["git_http_url"]
            .as_str()
            .map(|s| s.to_string());

        let metadata = ForgeMetadata {
            repo_url,
            base_sha,
            head_sha,
            pr_number,
            pr_title,
            pr_url,
        };

        Ok((action, metadata))
    }
}

/// Registry for forge providers
pub struct ForgeRegistry {
    providers: HashMap<String, Arc<dyn ForgeProvider>>,
}

impl ForgeRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            providers: HashMap::new(),
        };

        // Register built-in providers
        registry.register("github", Arc::new(GitHubForge));
        registry.register("gitlab", Arc::new(GitLabForge));

        registry
    }

    pub fn register(&mut self, name: &str, provider: Arc<dyn ForgeProvider>) {
        self.providers.insert(name.to_string(), provider);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn ForgeProvider>> {
        self.providers.get(name).cloned()
    }

    pub fn list_providers(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }
}

impl Default for ForgeRegistry {
    fn default() -> Self {
        Self::new()
    }
}
