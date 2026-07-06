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

/// Validate a git commit SHA (40-char SHA-1 or 64-char SHA-256, lowercase hex).
pub fn is_valid_git_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Check that a URL uses an acceptable scheme for git operations.
pub fn is_valid_repo_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://") || url.starts_with("git@")
}

/// Check that a repository URL does not target known internal or metadata
/// endpoints. Returns false for cloud metadata services, loopback addresses,
/// and other destinations that should never be cloned.
pub fn is_safe_repo_url(url: &str) -> bool {
    if !is_valid_repo_url(url) {
        return false;
    }
    let lower = url.to_lowercase();
    !lower.contains("169.254.")
        && !lower.contains("metadata.google")
        && !lower.contains("localhost")
        && !lower.contains("127.0.0.1")
        && !lower.contains("[::1]")
        && !lower.contains("0.0.0.0")
}

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

        if !is_valid_git_sha(&head_sha) || !is_valid_git_sha(&base_sha) {
            return Err(StatusCode::BAD_REQUEST);
        }

        let pr_number = pr["number"].as_i64().ok_or(StatusCode::BAD_REQUEST)?;
        if pr_number <= 0 {
            return Err(StatusCode::BAD_REQUEST);
        }

        let pr_title = pr["title"].as_str().map(|s| s.to_string());
        let pr_url = pr["html_url"].as_str().map(|s| s.to_string());

        let repo_url = payload["repository"]["clone_url"]
            .as_str()
            .map(|s| s.to_string());

        if let Some(ref url) = repo_url
            && !is_safe_repo_url(url)
        {
            return Err(StatusCode::BAD_REQUEST);
        }

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

        let base_sha = attrs["diff_refs"]["base_sha"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| head_sha.clone());

        if !is_valid_git_sha(&head_sha) || !is_valid_git_sha(&base_sha) {
            return Err(StatusCode::BAD_REQUEST);
        }

        let pr_number = attrs["iid"].as_i64().ok_or(StatusCode::BAD_REQUEST)?;
        if pr_number <= 0 {
            return Err(StatusCode::BAD_REQUEST);
        }

        let pr_title = attrs["title"].as_str().map(|s| s.to_string());
        let pr_url = attrs["url"].as_str().map(|s| s.to_string());

        let repo_url = payload["project"]["git_http_url"]
            .as_str()
            .map(|s| s.to_string());

        if let Some(ref url) = repo_url
            && !is_safe_repo_url(url)
        {
            return Err(StatusCode::BAD_REQUEST);
        }

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

/// Extract repository name from a URL
pub fn extract_repo_name_from_url(url: &str) -> String {
    url.trim_end_matches('/')
        .split('/')
        .next_back()
        .map(|s| s.trim_end_matches(".git"))
        .unwrap_or("repo")
        .to_string()
}

/// Extract repository name from a GitLab MR URL
pub fn extract_repo_name_from_mr_url(url: &str) -> Option<String> {
    if let Some(before_sep) = url.split("/-/").next() {
        let name = before_sep
            .trim_end_matches('/')
            .split('/')
            .next_back()?
            .to_string();
        Some(name)
    } else {
        None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_git_sha_40_char() {
        assert!(is_valid_git_sha("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"));
        assert!(is_valid_git_sha("0000000000000000000000000000000000000000"));
        assert!(is_valid_git_sha("abcdef0123456789abcdef0123456789abcdef01"));
    }

    #[test]
    fn test_is_valid_git_sha_rejects_non_hex() {
        assert!(!is_valid_git_sha(
            "g1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
        ));
        assert!(!is_valid_git_sha("../../etc/passwd/../../../../etc/shadow"));
        // Uppercase hex is valid — git accepts both cases
        assert!(is_valid_git_sha("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
    }

    #[test]
    fn test_is_valid_git_sha_64_char() {
        let sha256 = "a".repeat(64);
        assert!(is_valid_git_sha(&sha256));
    }

    #[test]
    fn test_is_valid_git_sha_rejects_wrong_length() {
        assert!(!is_valid_git_sha("abc123"));
        assert!(!is_valid_git_sha("a".repeat(39).as_str()));
        assert!(!is_valid_git_sha("a".repeat(41).as_str()));
        assert!(!is_valid_git_sha(""));
    }

    #[test]
    fn test_is_valid_repo_url_accepts_valid_schemes() {
        assert!(is_valid_repo_url("https://gitlab.com/org/repo.git"));
        assert!(is_valid_repo_url("http://gitlab.internal/org/repo.git"));
        assert!(is_valid_repo_url("git@github.com:org/repo.git"));
    }

    #[test]
    fn test_is_valid_repo_url_rejects_invalid_schemes() {
        assert!(!is_valid_repo_url("ftp://files.example.com/repo.tar"));
        assert!(!is_valid_repo_url("file:///etc/passwd"));
        assert!(!is_valid_repo_url("javascript:alert(1)"));
        assert!(!is_valid_repo_url(""));
    }

    #[test]
    fn test_is_safe_repo_url_blocks_ssrf() {
        assert!(!is_safe_repo_url(
            "http://169.254.169.254/latest/meta-data/"
        ));
        assert!(!is_safe_repo_url("http://metadata.google.internal/"));
        assert!(!is_safe_repo_url("http://localhost:5432/"));
        assert!(!is_safe_repo_url("http://127.0.0.1:8080/repo"));
        assert!(!is_safe_repo_url("http://[::1]:8080/repo"));
        assert!(!is_safe_repo_url("http://0.0.0.0/repo"));
    }

    #[test]
    fn test_is_safe_repo_url_accepts_legitimate() {
        assert!(is_safe_repo_url("https://gitlab.com/org/repo.git"));
        assert!(is_safe_repo_url("https://github.com/org/repo.git"));
        assert!(is_safe_repo_url("git@gitlab.example.com:org/repo.git"));
        assert!(is_safe_repo_url(
            "http://gitlab.internal:8929/group/project.git"
        ));
    }

    #[test]
    fn test_github_parse_payload_rejects_invalid_sha() {
        let forge = GitHubForge;
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "head": {"sha": "not-a-valid-sha"},
                "base": {"sha": "also-not-valid"},
                "number": 1,
                "title": "test"
            },
            "repository": {"clone_url": "https://github.com/org/repo.git"}
        });
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());
        assert_eq!(
            forge.parse_payload(&body).unwrap_err(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_github_parse_payload_rejects_negative_pr() {
        let forge = GitHubForge;
        let valid_sha = "a".repeat(40);
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "head": {"sha": &valid_sha},
                "base": {"sha": &valid_sha},
                "number": -1,
                "title": "test"
            },
            "repository": {"clone_url": "https://github.com/org/repo.git"}
        });
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());
        assert_eq!(
            forge.parse_payload(&body).unwrap_err(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_github_parse_payload_rejects_ssrf_url() {
        let forge = GitHubForge;
        let valid_sha = "a".repeat(40);
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "head": {"sha": &valid_sha},
                "base": {"sha": &valid_sha},
                "number": 1,
                "title": "test"
            },
            "repository": {"clone_url": "http://169.254.169.254/latest/meta-data/"}
        });
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());
        assert_eq!(
            forge.parse_payload(&body).unwrap_err(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_github_parse_payload_accepts_valid() {
        let forge = GitHubForge;
        let valid_sha = "a".repeat(40);
        let base_sha = "b".repeat(40);
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "head": {"sha": &valid_sha},
                "base": {"sha": &base_sha},
                "number": 42,
                "title": "Fix something",
                "html_url": "https://github.com/org/repo/pull/42"
            },
            "repository": {"clone_url": "https://github.com/org/repo.git"}
        });
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());
        let (action, metadata) = forge.parse_payload(&body).unwrap();
        assert_eq!(action, "opened");
        assert_eq!(metadata.pr_number, 42);
        assert_eq!(metadata.head_sha, valid_sha);
    }

    #[test]
    fn test_gitlab_parse_payload_rejects_invalid_sha() {
        let forge = GitLabForge;
        let payload = serde_json::json!({
            "object_kind": "merge_request",
            "object_attributes": {
                "last_commit": {"id": "../../etc/passwd"},
                "diff_refs": {"base_sha": "invalid"},
                "iid": 1,
                "title": "test"
            },
            "project": {"git_http_url": "https://gitlab.com/org/repo.git"}
        });
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());
        assert_eq!(
            forge.parse_payload(&body).unwrap_err(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_gitlab_parse_payload_rejects_zero_iid() {
        let forge = GitLabForge;
        let valid_sha = "a".repeat(40);
        let payload = serde_json::json!({
            "object_kind": "merge_request",
            "object_attributes": {
                "last_commit": {"id": &valid_sha},
                "diff_refs": {"base_sha": &valid_sha},
                "iid": 0,
                "title": "test"
            },
            "project": {"git_http_url": "https://gitlab.com/org/repo.git"}
        });
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());
        assert_eq!(
            forge.parse_payload(&body).unwrap_err(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_gitlab_parse_payload_accepts_valid() {
        let forge = GitLabForge;
        let valid_sha = "c".repeat(40);
        let payload = serde_json::json!({
            "object_kind": "merge_request",
            "object_attributes": {
                "last_commit": {"id": &valid_sha},
                "diff_refs": {"base_sha": &valid_sha},
                "iid": 10,
                "title": "Fix bug",
                "url": "https://gitlab.com/org/repo/-/merge_requests/10"
            },
            "project": {"git_http_url": "https://gitlab.com/org/repo.git"}
        });
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());
        let (action, metadata) = forge.parse_payload(&body).unwrap();
        assert_eq!(action, "merge_request");
        assert_eq!(metadata.pr_number, 10);
    }
}
