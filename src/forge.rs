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
use base64::Engine;
use bytes::Bytes;
use hmac::{Hmac, Mac, digest::KeyInit};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Validate a git commit SHA (40-char SHA-1 or 64-char SHA-256, hex digits).
pub fn is_valid_git_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Check that a URL uses an acceptable scheme for git operations.
pub fn is_valid_repo_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://") || url.starts_with("git@")
}

/// Check that a repository URL does not target known internal or metadata
/// endpoints. Parses the URL and checks the host component only, avoiding
/// false positives from blocklist patterns appearing in usernames or paths.
///
/// This is a best-effort blocklist, not a complete SSRF mitigation. DNS
/// rebinding can bypass host-based checks. The primary access control
/// is webhook signature verification.
pub fn is_safe_repo_url(url: &str) -> bool {
    if !is_valid_repo_url(url) {
        return false;
    }
    // For git@ URLs, extract the host portion. SSH interprets the LAST
    // '@' as the user/host separator, so we must do the same to prevent
    // injection via URLs like "git@github.com@127.0.0.1:repo.git".
    let host = if let Some(rest) = url.strip_prefix("git@") {
        let before_colon = rest.split(':').next().unwrap_or("");
        // Use the portion after the last '@' — this is what SSH resolves
        let ssh_host = before_colon.rsplit('@').next().unwrap_or(before_colon);
        ssh_host.to_ascii_lowercase()
    } else if let Ok(parsed) = url::Url::parse(url) {
        parsed.host_str().unwrap_or("").to_ascii_lowercase()
    } else {
        return false;
    };

    // Blocklist applied to the host component only
    !host.starts_with("169.254.")
        && host != "metadata.google.internal"
        && !host.starts_with("localhost")
        && !host.starts_with("127.")
        && host != "[::1]"
        && host != "0.0.0.0"
        // Decimal representations of loopback (127.0.0.0/8)
        && !(2130706432..=2130706687).contains(&host.parse::<u64>().unwrap_or(0))
        // Decimal representation of 169.254.169.254
        && host != "2852039166"
        // Hex and octal IP representations
        && !host.starts_with("0x7f")
        && !host.starts_with("0xa9fe")
        && !host.starts_with("0177")
}

/// Decode a webhook secret. If prefixed with "whsec_", strip the prefix
/// and base64-decode the remainder (Standard Webhooks convention). Otherwise
/// return the raw string bytes.
fn decode_webhook_secret(secret: &str) -> Vec<u8> {
    if let Some(encoded) = secret.strip_prefix("whsec_") {
        match base64::engine::general_purpose::STANDARD.decode(encoded) {
            Ok(key) => key,
            Err(e) => {
                tracing::warn!(
                    "webhook_secret has whsec_ prefix but base64 decode failed: {}. \
                     Check that the token was copied correctly from GitLab. \
                     Falling back to raw string bytes.",
                    e
                );
                secret.as_bytes().to_vec()
            }
        }
    } else {
        secret.as_bytes().to_vec()
    }
}

/// Verify a Standard Webhooks HMAC-SHA256 signature (GitLab 19.0+ signing
/// token). The signature header may contain multiple space-separated entries,
/// each in the format "v1,{base64(hmac)}". The HMAC is computed over
/// "{message_id}.{timestamp}.{body}".
fn verify_standard_webhook_signature(
    secret: &str,
    msg_id: &str,
    timestamp: &str,
    body: &[u8],
    signatures: &str,
) -> bool {
    let key = decode_webhook_secret(secret);
    let mut mac = match HmacSha256::new_from_slice(&key) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let preamble = format!("{}.{}.", msg_id, timestamp);
    mac.update(preamble.as_bytes());
    mac.update(body);
    let result = mac.finalize().into_bytes();
    let expected = format!(
        "v1,{}",
        base64::engine::general_purpose::STANDARD.encode(result)
    );
    signatures
        .split(' ')
        .any(|sig| expected.as_bytes().ct_eq(sig.as_bytes()).into())
}

/// Verify a GitHub HMAC-SHA256 signature. The header value has the format
/// "sha256={hex_digest}". GitHub sends lowercase hex per their documentation.
/// The received hex is normalized to lowercase before comparison for
/// robustness against forges that may use uppercase.
fn verify_github_signature(secret: &str, body: &[u8], signature_header: &str) -> bool {
    let hex_sig = match signature_header.strip_prefix("sha256=") {
        Some(s) => s.to_ascii_lowercase(),
        None => return false,
    };
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    let result = mac.finalize().into_bytes();
    let mut computed = String::with_capacity(64);
    for b in result {
        use std::fmt::Write;
        let _ = write!(computed, "{:02x}", b);
    }
    computed.as_bytes().ct_eq(hex_sig.as_bytes()).into()
}

/// Verify a legacy GitLab secret token via constant-time comparison.
/// Note: ct_eq reveals whether the lengths differ (but not the content).
/// This is acceptable for webhook secrets with sufficient entropy.
fn verify_secret_token(secret: &str, token_header: &str) -> bool {
    secret.as_bytes().ct_eq(token_header.as_bytes()).into()
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

    /// Validate webhook event type and verify signature when a secret is
    /// configured. When `secret` is `None`, only event-type validation is
    /// performed and the request is treated as unauthenticated — callers
    /// must enforce their own access control before calling this method.
    /// Returns `UNAUTHORIZED` if the signature is missing or invalid,
    /// `BAD_REQUEST` if the event type is wrong.
    fn validate_event(
        &self,
        headers: &HeaderMap,
        body: &Bytes,
        secret: Option<&str>,
    ) -> Result<(), StatusCode>;

    /// Parse webhook payload and extract metadata
    fn parse_payload(&self, body: &Bytes) -> Result<(String, ForgeMetadata), StatusCode>;
}

/// GitHub forge provider
pub struct GitHubForge;

impl ForgeProvider for GitHubForge {
    fn name(&self) -> &str {
        "GitHub"
    }

    fn validate_event(
        &self,
        headers: &HeaderMap,
        body: &Bytes,
        secret: Option<&str>,
    ) -> Result<(), StatusCode> {
        let event = headers
            .get("x-github-event")
            .and_then(|v| v.to_str().ok())
            .ok_or(StatusCode::BAD_REQUEST)?;

        if event != "pull_request" {
            return Err(StatusCode::BAD_REQUEST);
        }

        if let Some(secret) = secret {
            let sig = headers
                .get("x-hub-signature-256")
                .and_then(|v| v.to_str().ok())
                .ok_or(StatusCode::UNAUTHORIZED)?;
            if !verify_github_signature(secret, body, sig) {
                return Err(StatusCode::UNAUTHORIZED);
            }
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

    fn validate_event(
        &self,
        headers: &HeaderMap,
        body: &Bytes,
        secret: Option<&str>,
    ) -> Result<(), StatusCode> {
        let event = headers
            .get("x-gitlab-event")
            .and_then(|v| v.to_str().ok())
            .ok_or(StatusCode::BAD_REQUEST)?;

        if event != "Merge Request Hook" {
            return Err(StatusCode::BAD_REQUEST);
        }

        if let Some(secret) = secret {
            // Try Standard Webhooks signature first (GitLab 19.0+)
            if let (Some(msg_id), Some(timestamp), Some(sig)) = (
                headers.get("webhook-id").and_then(|v| v.to_str().ok()),
                headers
                    .get("webhook-timestamp")
                    .and_then(|v| v.to_str().ok()),
                headers
                    .get("webhook-signature")
                    .and_then(|v| v.to_str().ok()),
            ) {
                if !verify_standard_webhook_signature(secret, msg_id, timestamp, body, sig) {
                    return Err(StatusCode::UNAUTHORIZED);
                }
                return Ok(());
            }

            // Fallback: legacy secret token (X-Gitlab-Token)
            if let Some(token) = headers.get("x-gitlab-token").and_then(|v| v.to_str().ok()) {
                if !verify_secret_token(secret, token) {
                    return Err(StatusCode::UNAUTHORIZED);
                }
                return Ok(());
            }

            // Secret configured but no auth header present
            return Err(StatusCode::UNAUTHORIZED);
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
        assert!(!is_safe_repo_url("http://localhost.localdomain/repo"));
        assert!(!is_safe_repo_url("http://127.0.0.1:8080/repo"));
        assert!(!is_safe_repo_url("http://127.1/repo"));
        assert!(!is_safe_repo_url("http://[::1]:8080/repo"));
        assert!(!is_safe_repo_url("http://0.0.0.0/repo"));
        // Decimal and hex IP representations of 127.0.0.1
        assert!(!is_safe_repo_url("http://2130706433/repo"));
        assert!(!is_safe_repo_url("http://0x7f000001/repo"));
        // Octal representation
        assert!(!is_safe_repo_url("http://0177.0.0.1/repo"));
        // Decimal representation of 169.254.169.254
        assert!(!is_safe_repo_url("http://2852039166/latest/"));
        // Hex representation of 169.254.x.x
        assert!(!is_safe_repo_url("http://0xa9fea9fe/latest/"));
        // Decimal for 127.0.0.2 (other loopback addresses in 127.0.0.0/8)
        assert!(!is_safe_repo_url("http://2130706434/repo"));
    }

    #[test]
    fn test_is_safe_repo_url_blocks_ssh_injection() {
        // SSH resolves the LAST '@' as user/host separator
        assert!(!is_safe_repo_url("git@github.com@127.0.0.1:repo.git"));
        assert!(!is_safe_repo_url("git@github.com@localhost:repo.git"));
        assert!(!is_safe_repo_url("git@legit.com@169.254.169.254:repo.git"));
    }

    #[test]
    fn test_is_safe_repo_url_no_false_positives_on_path() {
        // Blocklist patterns in username or path should NOT trigger rejection
        assert!(is_safe_repo_url(
            "https://github.com/user-127.0.0.1/repo.git"
        ));
        assert!(is_safe_repo_url(
            "https://github.com/org/localhost-tools.git"
        ));
        assert!(is_safe_repo_url("git@github.com:0x7f-labs/project.git"));
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

    // --- HMAC verification tests ---

    #[test]
    fn test_verify_github_signature_known_vector() {
        // Test vector from GitHub docs:
        // secret: "It's a Secret to Everybody"
        // payload: "Hello, World!"
        // expected: sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17
        let secret = "It's a Secret to Everybody";
        let payload = b"Hello, World!";
        let sig = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(verify_github_signature(secret, payload, sig));
    }

    #[test]
    fn test_verify_github_signature_rejects_invalid() {
        let secret = "my-secret";
        let payload = b"test body";
        let sig = "sha256=0000000000000000000000000000000000000000000000000000000000000000";
        assert!(!verify_github_signature(secret, payload, sig));
    }

    #[test]
    fn test_verify_github_signature_rejects_missing_prefix() {
        let secret = "my-secret";
        let payload = b"test body";
        let sig = "md5=abcdef";
        assert!(!verify_github_signature(secret, payload, sig));
    }

    #[test]
    fn test_verify_standard_webhook_signature() {
        let secret = "test-secret-key";
        let msg_id = "msg-123";
        let timestamp = "1720000000";
        let body = b"test body";

        // Compute the expected signature manually
        let key = decode_webhook_secret(secret);
        let mut mac = HmacSha256::new_from_slice(&key).unwrap();
        let preamble = format!("{}.{}.", msg_id, timestamp);
        mac.update(preamble.as_bytes());
        mac.update(body);
        let result = mac.finalize().into_bytes();
        let sig = format!(
            "v1,{}",
            base64::engine::general_purpose::STANDARD.encode(result)
        );

        assert!(verify_standard_webhook_signature(
            secret, msg_id, timestamp, body, &sig
        ));
    }

    #[test]
    fn test_verify_standard_webhook_signature_rejects_tampered() {
        let secret = "test-secret";
        let msg_id = "msg-456";
        let timestamp = "1720000000";
        let body = b"original body";
        let sig = "v1,AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        assert!(!verify_standard_webhook_signature(
            secret, msg_id, timestamp, body, sig
        ));
    }

    #[test]
    fn test_verify_standard_webhook_multiple_signatures() {
        let secret = "multi-test";
        let msg_id = "msg-789";
        let timestamp = "1720000000";
        let body = b"multi sig body";

        let key = decode_webhook_secret(secret);
        let mut mac = HmacSha256::new_from_slice(&key).unwrap();
        mac.update(format!("{}.{}.", msg_id, timestamp).as_bytes());
        mac.update(body);
        let result = mac.finalize().into_bytes();
        let valid_sig = format!(
            "v1,{}",
            base64::engine::general_purpose::STANDARD.encode(result)
        );

        // Multiple signatures separated by space — one valid, one garbage
        let header = format!("v1,garbage_signature {}", valid_sig);
        assert!(verify_standard_webhook_signature(
            secret, msg_id, timestamp, body, &header
        ));
    }

    #[test]
    fn test_verify_secret_token() {
        assert!(verify_secret_token("my-secret", "my-secret"));
        assert!(!verify_secret_token("my-secret", "wrong-secret"));
        assert!(!verify_secret_token("my-secret", "my-secre")); // length differs
    }

    #[test]
    fn test_decode_webhook_secret_whsec_prefix() {
        // "whsec_" + base64("test-key") = "whsec_dGVzdC1rZXk="
        let decoded = decode_webhook_secret("whsec_dGVzdC1rZXk=");
        assert_eq!(decoded, b"test-key");
    }

    #[test]
    fn test_decode_webhook_secret_plain() {
        let decoded = decode_webhook_secret("my-plain-secret");
        assert_eq!(decoded, b"my-plain-secret");
    }

    #[test]
    fn test_decode_webhook_secret_invalid_base64_falls_back() {
        // Invalid base64 after whsec_ prefix — falls back to raw bytes
        let decoded = decode_webhook_secret("whsec_!!!invalid!!!");
        assert_eq!(decoded, b"whsec_!!!invalid!!!");
    }

    #[test]
    fn test_github_validate_event_accepts_without_secret() {
        let forge = GitHubForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request".parse().unwrap());
        let body = Bytes::from("{}");
        assert!(forge.validate_event(&headers, &body, None).is_ok());
    }

    #[test]
    fn test_github_validate_event_rejects_missing_signature() {
        let forge = GitHubForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request".parse().unwrap());
        let body = Bytes::from("{}");
        assert_eq!(
            forge
                .validate_event(&headers, &body, Some("my-secret"))
                .unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn test_gitlab_validate_event_accepts_legacy_token() {
        let forge = GitLabForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-event", "Merge Request Hook".parse().unwrap());
        headers.insert("x-gitlab-token", "shared-secret".parse().unwrap());
        let body = Bytes::from("{}");
        assert!(
            forge
                .validate_event(&headers, &body, Some("shared-secret"))
                .is_ok()
        );
    }

    #[test]
    fn test_gitlab_validate_event_rejects_wrong_token() {
        let forge = GitLabForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-event", "Merge Request Hook".parse().unwrap());
        headers.insert("x-gitlab-token", "wrong-token".parse().unwrap());
        let body = Bytes::from("{}");
        assert_eq!(
            forge
                .validate_event(&headers, &body, Some("correct-token"))
                .unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn test_gitlab_validate_event_rejects_no_auth_headers() {
        let forge = GitLabForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-event", "Merge Request Hook".parse().unwrap());
        let body = Bytes::from("{}");
        assert_eq!(
            forge
                .validate_event(&headers, &body, Some("my-secret"))
                .unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn test_gitlab_validate_event_accepts_without_secret() {
        let forge = GitLabForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-event", "Merge Request Hook".parse().unwrap());
        let body = Bytes::from("{}");
        assert!(forge.validate_event(&headers, &body, None).is_ok());
    }
}
