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
    pub author: Option<String>,
}

/// Returns true if the authenticated forge username belongs to Dependabot
/// (`dependabot[bot]` or `dependabot`).
pub fn is_dependabot_author(author: &str) -> bool {
    let lower = author.trim().to_ascii_lowercase();
    lower == "dependabot" || lower == "dependabot[bot]"
}

/// Classification of a webhook request that passed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeEvent {
    /// A pull or merge request carrying commits to review.
    ChangeRequest,
    /// A provider handshake sent to confirm the endpoint is reachable. It
    /// carries nothing to review and must be acknowledged without side
    /// effects.
    Handshake,
}

/// Longest caller supplied value echoed into a log line.
const MAX_LOGGED_LEN: usize = 32;

/// Echo a caller supplied string into a log line only when it is short and
/// printable. A webhook sender controls both the event header and the
/// provider path segment, and a raw value could carry newlines that forge
/// whole log entries.
pub fn loggable(value: &str) -> &str {
    let printable = !value.is_empty()
        && value.len() <= MAX_LOGGED_LEN
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' ' | '[' | ']'));
    if printable { value } else { "(invalid)" }
}

/// Name the event a webhook carried, for diagnostics only. Validation never
/// consults this: it reads the provider specific header directly.
pub fn event_label(headers: &HeaderMap) -> &str {
    ["x-github-event", "x-gitlab-event"]
        .into_iter()
        .find_map(|name| headers.get(name).and_then(|value| value.to_str().ok()))
        .map(loggable)
        .unwrap_or("(none)")
}

/// Whether an action reported for a change request asks for a review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewIntent {
    /// The request carries commits that have not been reviewed yet.
    Review,
    /// The forge is reporting a change that leaves the commits untouched,
    /// such as a label, an assignee or a title edit.
    Skip,
}

/// Trait for forge provider implementations
pub trait ForgeProvider: Send + Sync {
    /// Provider name (e.g., "GitHub", "GitLab")
    fn name(&self) -> &str;

    /// Validate webhook event type and verify signature when a secret is
    /// configured. When `secret` is `None`, only event-type validation is
    /// performed and the request is treated as unauthenticated — callers
    /// must enforce their own access control before calling this method.
    /// Returns the kind of event on success, `UNAUTHORIZED` if the signature
    /// is missing or invalid, `BAD_REQUEST` if the event type is wrong.
    fn validate_event(
        &self,
        headers: &HeaderMap,
        body: &Bytes,
        secret: Option<&str>,
    ) -> Result<ForgeEvent, StatusCode>;

    /// Parse webhook payload and extract metadata
    fn parse_payload(&self, body: &Bytes) -> Result<(String, ForgeMetadata), StatusCode>;

    /// Decide whether the action reported by `parse_payload` describes new
    /// commits. The action vocabulary belongs to the provider, so each one
    /// answers for itself.
    fn review_intent(&self, action: &str) -> ReviewIntent;
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
    ) -> Result<ForgeEvent, StatusCode> {
        let event = headers
            .get("x-github-event")
            .and_then(|v| v.to_str().ok())
            .ok_or(StatusCode::BAD_REQUEST)?;

        // A ping is what GitHub sends the moment a webhook is saved, and its
        // delivery status is what an administrator checks to confirm the
        // endpoint works. Rejecting it reports a broken integration that is
        // in fact correctly configured.
        let kind = match event {
            "pull_request" => ForgeEvent::ChangeRequest,
            "ping" => ForgeEvent::Handshake,
            _ => return Err(StatusCode::BAD_REQUEST),
        };

        if let Some(secret) = secret {
            let sig = headers
                .get("x-hub-signature-256")
                .and_then(|v| v.to_str().ok())
                .ok_or(StatusCode::UNAUTHORIZED)?;
            if !verify_github_signature(secret, body, sig) {
                return Err(StatusCode::UNAUTHORIZED);
            }
        }

        Ok(kind)
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
        let author = pr["user"]["login"]
            .as_str()
            .or_else(|| payload["sender"]["login"].as_str())
            .map(|s| s.to_string());

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
            author,
        };

        Ok((action, metadata))
    }

    fn review_intent(&self, action: &str) -> ReviewIntent {
        // GitHub reports roughly twenty actions on a pull request and sends
        // every one of them to a subscriber. Only these four can leave the
        // head commit different from the last time it was seen.
        match action {
            "opened" | "reopened" | "synchronize" | "ready_for_review" => ReviewIntent::Review,
            _ => ReviewIntent::Skip,
        }
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
    ) -> Result<ForgeEvent, StatusCode> {
        let event = headers
            .get("x-gitlab-event")
            .and_then(|v| v.to_str().ok())
            .ok_or(StatusCode::BAD_REQUEST)?;

        // GitLab has no handshake event: its Test button replays a real hook,
        // so a merge request is the only thing worth accepting here.
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
                return Ok(ForgeEvent::ChangeRequest);
            }

            // Fallback: legacy secret token (X-Gitlab-Token)
            if let Some(token) = headers.get("x-gitlab-token").and_then(|v| v.to_str().ok()) {
                if !verify_secret_token(secret, token) {
                    return Err(StatusCode::UNAUTHORIZED);
                }
                return Ok(ForgeEvent::ChangeRequest);
            }

            // Secret configured but no auth header present
            return Err(StatusCode::UNAUTHORIZED);
        }

        Ok(ForgeEvent::ChangeRequest)
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
        let author = payload["user"]["username"].as_str().map(|s| s.to_string());

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
            author,
        };

        Ok((action, metadata))
    }

    fn review_intent(&self, _action: &str) -> ReviewIntent {
        // parse_payload reports the object kind here, not the action inside
        // object_attributes, so there is nothing to discriminate on yet and
        // every merge request hook is reviewed as it always was. Narrowing
        // this needs the action and oldrev fields, which is a change to
        // GitLab parsing rather than to the route.
        ReviewIntent::Review
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

/// Extract repository name from a GitLab MR or GitHub PR URL
pub fn extract_repo_name_from_mr_url(url: &str) -> Option<String> {
    let before_sep = url
        .split_once("/-/")
        .or_else(|| url.split_once("/pull/"))
        .map(|(prefix, _)| prefix)?;
    let name = before_sep
        .trim_end_matches('/')
        .split('/')
        .next_back()?
        .trim_end_matches(".git")
        .to_string();
    if name.is_empty() { None } else { Some(name) }
}

/// Extract the `"owner/repo"` slug from a GitHub PR or GitLab MR URL.
pub fn extract_owner_repo_from_mr_url(url: &str) -> Option<String> {
    let before_sep = url
        .split_once("/-/")
        .or_else(|| url.split_once("/pull/"))
        .map(|(prefix, _)| prefix)?;
    let after_scheme = before_sep
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(before_sep);
    let (_, path) = after_scheme.split_once('/')?;
    let slug = path.trim_matches('/').trim_end_matches(".git");
    if slug.is_empty() || !slug.contains('/') {
        None
    } else {
        Some(slug.to_string())
    }
}

fn is_valid_github_repo_segment(seg: &str) -> bool {
    !seg.is_empty()
        && seg != "."
        && seg != ".."
        && seg.len() <= 100
        && seg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Normalize a GitHub repository identifier (`"owner/repo"`, HTTPS clone/PR URL,
/// or SSH `git@github.com:owner/repo.git` URL) into a canonical `"owner/repo"`
/// slug. Returns `None` if the input names a non-GitHub host or contains path
/// traversal / invalid segment characters.
pub fn normalize_github_repo_slug(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    let raw_path = if let Some(rest) = trimmed.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        if !host.eq_ignore_ascii_case("github.com") {
            return None;
        }
        std::borrow::Cow::Borrowed(path)
    } else if trimmed.contains("://") {
        let parsed = url::Url::parse(trimmed).ok()?;
        if !matches!(parsed.scheme(), "http" | "https" | "ssh") {
            return None;
        }
        if !parsed.username().is_empty() && parsed.scheme() != "ssh" {
            return None;
        }
        if parsed.password().is_some() {
            return None;
        }
        let host = parsed.host_str()?;
        if !host.eq_ignore_ascii_case("github.com") {
            return None;
        }
        std::borrow::Cow::Owned(parsed.path().to_string())
    } else {
        std::borrow::Cow::Borrowed(trimmed)
    };

    let without_pull = raw_path
        .split_once("/pull/")
        .map(|(prefix, _)| prefix)
        .unwrap_or(raw_path.as_ref());
    let cleaned = without_pull.trim_matches('/').trim_end_matches(".git");
    let mut parts = cleaned.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    if !is_valid_github_repo_segment(owner) || !is_valid_github_repo_segment(repo) {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// Inspect the `origin` remote of `repo_path` and return its normalized GitHub
/// `"owner/repo"` slug when `origin` points to `github.com`.
pub async fn detect_github_repo_from_git_dir(repo_path: &std::path::Path) -> Option<String> {
    let mut cmd = crate::git_cmd::in_dir_async(repo_path);
    cmd.args([
        "-c",
        "safe.bareRepository=all",
        "remote",
        "get-url",
        "origin",
    ]);
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?;
    normalize_github_repo_slug(&url)
}

/// Open pull request metadata returned by the GitHub Pulls REST API.
#[derive(Debug, Clone)]
pub struct GitHubPolledPullRequest {
    pub metadata: ForgeMetadata,
    pub updated_at: i64,
}

/// One page of open pull requests returned by the GitHub Pulls REST API.
#[derive(Debug, Clone)]
pub struct GitHubPullsPage {
    pub items: Vec<GitHubPolledPullRequest>,
    pub raw_count: usize,
    pub skipped_count: usize,
}

/// Fetch one page of open pull requests from GitHub's Pulls REST API sorted by
/// `updated` descending. Malformed items are logged and skipped rather than
/// aborting the entire repository poll.
pub async fn list_github_open_pull_requests(
    client: &reqwest::Client,
    api_base: &str,
    repo: &str,
    token: Option<&str>,
    page: u32,
    per_page: u32,
) -> Result<GitHubPullsPage, String> {
    let repo = normalize_github_repo_slug(repo)
        .ok_or_else(|| format!("invalid GitHub repository slug: {}", loggable(repo)))?;
    let per_page = per_page.clamp(1, 100);
    let page = page.max(1);
    let url = format!(
        "{}/repos/{}/pulls?state=open&sort=updated&direction=desc&per_page={}&page={}",
        api_base.trim_end_matches('/'),
        repo,
        per_page,
        page
    );

    let mut req = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "sashiko")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if let Some(tok) = token.map(str::trim).filter(|s| !s.is_empty()) {
        req = req.header("Authorization", format!("Bearer {}", tok));
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("GitHub pulls GET failed for {}: {}", repo, e))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let redacted = crate::utils::redact_secret(&text);
        return Err(format!(
            "GitHub pulls API returned {} for {}: {}",
            status, repo, redacted
        ));
    }

    let items: Vec<serde_json::Value> = resp.json().await.map_err(|e| {
        format!(
            "invalid JSON array from GitHub pulls endpoint for {} ({}): {}",
            repo, status, e
        )
    })?;

    let raw_count = items.len();
    let mut skipped_count = 0usize;
    let mut out = Vec::with_capacity(raw_count);
    for pr in items {
        let Some(pr_number) = pr["number"].as_i64().filter(|&n| n > 0) else {
            tracing::warn!("Skipping GitHub PR item with invalid number in {}", repo);
            skipped_count += 1;
            continue;
        };
        let Some(head_sha) = pr["head"]["sha"]
            .as_str()
            .filter(|s| is_valid_git_sha(s))
            .map(str::to_string)
        else {
            tracing::warn!(
                "Skipping GitHub PR {}#{} with invalid head.sha",
                repo,
                pr_number
            );
            skipped_count += 1;
            continue;
        };
        let Some(base_sha) = pr["base"]["sha"]
            .as_str()
            .filter(|s| is_valid_git_sha(s))
            .map(str::to_string)
        else {
            tracing::warn!(
                "Skipping GitHub PR {}#{} with invalid base.sha",
                repo,
                pr_number
            );
            skipped_count += 1;
            continue;
        };
        let Some(updated_at) = pr["updated_at"]
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp())
        else {
            tracing::warn!(
                "Skipping GitHub PR {}#{} with missing or invalid updated_at",
                repo,
                pr_number
            );
            skipped_count += 1;
            continue;
        };

        let clone_url = pr["base"]["repo"]["clone_url"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("https://github.com/{}.git", repo));
        if !is_safe_repo_url(&clone_url)
            || crate::fetcher::url_has_embedded_credentials(Some(&clone_url))
        {
            tracing::warn!(
                "Skipping GitHub PR {}#{} with unsafe clone_url",
                repo,
                pr_number
            );
            skipped_count += 1;
            continue;
        }

        let pr_title = pr["title"].as_str().map(str::to_string);
        let pr_url = pr["html_url"]
            .as_str()
            .map(str::to_string)
            .or_else(|| Some(format!("https://github.com/{}/pull/{}", repo, pr_number)));
        let author = pr["user"]["login"].as_str().map(str::to_string);

        out.push(GitHubPolledPullRequest {
            metadata: ForgeMetadata {
                repo_url: Some(clone_url),
                base_sha,
                head_sha,
                pr_number,
                pr_title,
                pr_url,
                author,
            },
            updated_at,
        });
    }

    Ok(GitHubPullsPage {
        items: out,
        raw_count,
        skipped_count,
    })
}

/// Summary and findings for one commit in a pull request series.
#[derive(Debug, Clone)]
pub struct PatchReviewSummaryItem {
    pub part_index: usize,
    pub total_parts: usize,
    pub commit_id: Option<String>,
    pub subject: String,
    pub inline_review: Option<String>,
    pub failed: bool,
}

/// Banner prefix emitted in forge PR comments when one or more commits failed
/// to complete review.
pub const INCOMPLETE_REVIEW_MARKER: &str = "⚠ **Review incomplete**";

/// Return true when `body` was produced by `compose_pr_review_comment` for an
/// incomplete review (banner immediately following the `### Sashiko review` header).
pub fn is_incomplete_pr_review_comment(body: &str) -> bool {
    body.split_once("\n\n").is_some_and(|(header, rest)| {
        header.starts_with("### Sashiko review") && rest.starts_with(INCOMPLETE_REVIEW_MARKER)
    })
}

/// Maximum byte length of a GitHub issue/PR comment body (GitHub's hard limit
/// is 65 536 bytes; leave safety margin for UTF-8 boundary and trailing link).
const MAX_GITHUB_COMMENT_BYTES: usize = 60_000;

/// Compose the markdown comment posted to a pull request thread when a review
/// finishes. Clean pull requests receive a short confirmation; partially or
/// wholly failed pull requests receive an explicit incomplete-review banner
/// alongside any findings discovered in completed commits.
pub fn compose_pr_review_comment(
    version: Option<u32>,
    total_commits: usize,
    series_summary: Option<&str>,
    patches: &[PatchReviewSummaryItem],
    patchset_failed: bool,
    target_url: &str,
) -> String {
    let header = match version {
        Some(v) if v > 1 => format!("### Sashiko review — v{}", v),
        _ => "### Sashiko review".to_string(),
    };

    let link_host = target_url
        .split_once("://")
        .and_then(|(_, rest)| rest.split('/').next())
        .filter(|h| !h.is_empty())
        .unwrap_or("sashiko.sashiko.dev");

    let commit_word = if total_commits == 1 {
        "1 commit".to_string()
    } else {
        format!("{} commits", total_commits)
    };

    let failed_patches: Vec<&PatchReviewSummaryItem> =
        patches.iter().filter(|p| p.failed).collect();

    let patches_with_findings: Vec<&PatchReviewSummaryItem> = patches
        .iter()
        .filter(|p| {
            p.inline_review.as_deref().is_some_and(|text| {
                let t = text.trim();
                !t.is_empty() && t != "No issues found."
            })
        })
        .collect();

    if !patchset_failed && failed_patches.is_empty() && patches_with_findings.is_empty() {
        return format!(
            "{header}\n\n✓ **No issues found** across {commit_word}.\n\n[Full review log on {link_host}]({target_url})\n"
        );
    }

    let mut out = String::with_capacity(2048);
    out.push_str(&header);
    out.push_str("\n\n");

    if !failed_patches.is_empty() {
        let failed_count = failed_patches.len();
        let listed: Vec<String> = failed_patches
            .iter()
            .take(5)
            .map(|p| format!("`{}/{}`", p.part_index, p.total_parts))
            .collect();
        let listed_str = if failed_count > 5 {
            format!("{}, +{} more", listed.join(", "), failed_count - 5)
        } else {
            listed.join(", ")
        };
        out.push_str(&format!(
            "{INCOMPLETE_REVIEW_MARKER} — {failed_count} of {commit_word} failed to complete review ({listed_str}).\n\n"
        ));
    } else if patchset_failed {
        out.push_str(&format!(
            "{INCOMPLETE_REVIEW_MARKER} — review failed to complete.\n\n"
        ));
    }

    if let Some(summary) = series_summary.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("<details>\n<summary>Series summary</summary>\n\n");
        out.push_str(summary);
        out.push_str("\n\n</details>\n\n");
    }

    for patch in patches_with_findings {
        let short_sha = patch
            .commit_id
            .as_deref()
            .filter(|s| s.len() >= 8)
            .map(|s| crate::utils::utf8_prefix(s, 8));
        let title_line = match short_sha {
            Some(sha) => format!(
                "#### Commit {}/{} — `{}` {}\n\n",
                patch.part_index, patch.total_parts, sha, patch.subject
            ),
            None => format!(
                "#### Commit {}/{} — {}\n\n",
                patch.part_index, patch.total_parts, patch.subject
            ),
        };
        out.push_str(&title_line);

        if let Some(inline) = patch.inline_review.as_deref() {
            out.push_str(inline.trim());
            out.push_str("\n\n");
        }
    }

    let footer = format!("[Full review and stage logs on {link_host}]({target_url})\n");

    if out.len() + footer.len() > MAX_GITHUB_COMMENT_BYTES {
        let mut cut = MAX_GITHUB_COMMENT_BYTES.saturating_sub(footer.len() + 64);
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("\n\n*(comment truncated; see full report below)*\n\n");
    }

    out.push_str(&footer);
    out
}

/// Mint a short-lived RS256 JWT identifying a GitHub App (`iss = app_id`).
pub fn mint_github_app_jwt(app_id: u64, pem: &str) -> Result<String, String> {
    #[derive(serde::Serialize)]
    struct Claims {
        iat: i64,
        exp: i64,
        iss: String,
    }

    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        iat: now - 60,
        exp: now + 540,
        iss: app_id.to_string(),
    };

    let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes())
        .map_err(|e| format!("invalid GitHub App RSA private key: {}", e))?;
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    jsonwebtoken::encode(&header, &claims, &key)
        .map_err(|e| format!("failed to sign GitHub App JWT: {}", e))
}

/// Exchange a GitHub App JWT for an installation access token.
pub async fn exchange_github_installation_token(
    client: &reqwest::Client,
    api_base: &str,
    installation_id: u64,
    jwt: &str,
) -> Result<String, String> {
    let url = format!(
        "{}/app/installations/{}/access_tokens",
        api_base.trim_end_matches('/'),
        installation_id
    );
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", jwt))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "sashiko")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|e| format!("GitHub token request failed: {}", e))?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.map_err(|e| {
        format!(
            "invalid JSON from GitHub token endpoint ({}): {}",
            status, e
        )
    })?;

    if !status.is_success() {
        let msg = body["message"].as_str().unwrap_or("unknown error");
        return Err(format!(
            "GitHub token exchange returned {}: {}",
            status, msg
        ));
    }

    body["token"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "GitHub token response missing 'token' field".to_string())
}

/// Post a comment to a GitHub pull request (via the issue comments endpoint).
pub async fn post_github_pr_comment(
    client: &reqwest::Client,
    api_base: &str,
    repo: &str,
    pr_number: i64,
    token: &str,
    body: &str,
) -> Result<u16, String> {
    let url = format!(
        "{}/repos/{}/issues/{}/comments",
        api_base.trim_end_matches('/'),
        repo,
        pr_number
    );
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "sashiko")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .json(&serde_json::json!({ "body": body }))
        .send()
        .await
        .map_err(|e| format!("GitHub comment POST failed: {}", e))?;

    let status = resp.status();
    if status.is_success() {
        Ok(status.as_u16())
    } else {
        let text = resp.text().await.unwrap_or_default();
        Err(format!("GitHub returned {}: {}", status, text))
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

/// Format a pull request / merge request subject line with `#` (or `!` for GitLab)
/// and an optional `[vN]` revision tag when `version >= 2`.
pub fn format_mr_subject(mr_url: Option<&str>, number: i64, version: u32, title: &str) -> String {
    let prefix = match mr_url {
        Some(url) if url.contains("gitlab") => "!",
        _ => "#",
    };
    let clean_title = strip_existing_mr_prefix(title, number);
    if version >= 2 {
        format!("{}{} [v{}]: {}", prefix, number, version, clean_title)
    } else {
        format!("{}{}: {}", prefix, number, clean_title)
    }
}

fn strip_existing_mr_prefix(title: &str, number: i64) -> &str {
    let trimmed = title.trim();
    for pfx in ['#', '!'] {
        let base = format!("{}{}", pfx, number);
        if let Some(rest) = trimmed.strip_prefix(&base) {
            let rest = rest.trim_start();
            if let Some(after_v) = rest.strip_prefix("[v")
                && let Some((_, after_bracket)) = after_v.split_once(']')
            {
                return after_bracket
                    .trim_start()
                    .strip_prefix(':')
                    .unwrap_or(after_bracket)
                    .trim_start();
            }
            if let Some(after_colon) = rest.strip_prefix(':') {
                return after_colon.trim_start();
            }
        }
    }
    trimmed
}

/// Extract the version number from a pull request subject formatted by `format_mr_subject`.
pub fn extract_mr_version_from_subject(subject: Option<&str>, number: i64) -> Option<u32> {
    let trimmed = subject?.trim();
    for pfx in ['#', '!'] {
        let base = format!("{}{}", pfx, number);
        if let Some(rest) = trimmed.strip_prefix(&base) {
            let rest = rest.trim_start();
            if let Some(after_v) = rest.strip_prefix("[v")
                && let Some((digits, _)) = after_v.split_once(']')
                && let Ok(v) = digits.parse::<u32>()
            {
                return Some(v);
            }
            return Some(1);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_mr_subject_versions() {
        assert_eq!(
            format_mr_subject(
                Some("https://github.com/sashiko-dev/sashiko/pull/513"),
                513,
                1,
                "baseline: route iwl-net and iwl-next series to dev-queue"
            ),
            "#513: baseline: route iwl-net and iwl-next series to dev-queue"
        );
        assert_eq!(
            format_mr_subject(
                Some("https://github.com/sashiko-dev/sashiko/pull/513"),
                513,
                5,
                "baseline: route iwl-net and iwl-next series to dev-queue"
            ),
            "#513 [v5]: baseline: route iwl-net and iwl-next series to dev-queue"
        );
        assert_eq!(
            format_mr_subject(
                Some("https://github.com/sashiko-dev/sashiko/pull/513"),
                513,
                2,
                "#513: baseline: route iwl-net and iwl-next series to dev-queue"
            ),
            "#513 [v2]: baseline: route iwl-net and iwl-next series to dev-queue"
        );
        assert_eq!(
            format_mr_subject(
                Some("https://gitlab.com/org/repo/-/merge_requests/42"),
                42,
                3,
                "fix race condition"
            ),
            "!42 [v3]: fix race condition"
        );
    }

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

    #[test]
    fn test_loggable_passes_plain_values() {
        assert_eq!(loggable("pull_request"), "pull_request");
        assert_eq!(loggable("Merge Request Hook"), "Merge Request Hook");
        assert_eq!(loggable("github"), "github");
    }

    #[test]
    fn test_loggable_rejects_forged_log_lines() {
        assert_eq!(loggable("ping\nRejected nothing"), "(invalid)");
        assert_eq!(loggable("ping\r\n"), "(invalid)");
        assert_eq!(loggable(""), "(invalid)");
        assert_eq!(loggable(&"a".repeat(MAX_LOGGED_LEN + 1)), "(invalid)");
    }

    #[test]
    fn test_event_label_reports_the_event_header() {
        let mut headers = HeaderMap::new();
        assert_eq!(event_label(&headers), "(none)");

        headers.insert("x-github-event", "ping".parse().unwrap());
        assert_eq!(event_label(&headers), "ping");

        let mut gitlab = HeaderMap::new();
        gitlab.insert("x-gitlab-event", "Merge Request Hook".parse().unwrap());
        assert_eq!(event_label(&gitlab), "Merge Request Hook");
    }

    /// Build the header value GitHub sends for a given body and secret.
    fn github_signature(secret: &str, body: &[u8]) -> String {
        use std::fmt::Write;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let mut hex = String::from("sha256=");
        for b in mac.finalize().into_bytes() {
            let _ = write!(hex, "{:02x}", b);
        }
        hex
    }

    #[test]
    fn test_github_validate_event_classifies_ping_as_handshake() {
        let forge = GitHubForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "ping".parse().unwrap());
        let body = Bytes::from(r#"{"zen":"Design for failure."}"#);
        assert_eq!(
            forge.validate_event(&headers, &body, None).unwrap(),
            ForgeEvent::Handshake
        );
    }

    #[test]
    fn test_github_validate_event_classifies_pull_request_as_change_request() {
        let forge = GitHubForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request".parse().unwrap());
        let body = Bytes::from("{}");
        assert_eq!(
            forge.validate_event(&headers, &body, None).unwrap(),
            ForgeEvent::ChangeRequest
        );
    }

    #[test]
    fn test_github_validate_event_accepts_signed_ping() {
        let forge = GitHubForge;
        let body = Bytes::from(r#"{"zen":"Design for failure."}"#);
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "ping".parse().unwrap());
        headers.insert(
            "x-hub-signature-256",
            github_signature("my-secret", &body).parse().unwrap(),
        );
        assert_eq!(
            forge
                .validate_event(&headers, &body, Some("my-secret"))
                .unwrap(),
            ForgeEvent::Handshake
        );
    }

    #[test]
    fn test_github_validate_event_rejects_unsigned_ping() {
        // A handshake is acknowledged with 200, so it must prove the sender
        // just like any other event when a secret is configured.
        let forge = GitHubForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "ping".parse().unwrap());
        let body = Bytes::from(r#"{"zen":"Design for failure."}"#);
        assert_eq!(
            forge
                .validate_event(&headers, &body, Some("my-secret"))
                .unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn test_github_validate_event_rejects_unrelated_event() {
        let forge = GitHubForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "push".parse().unwrap());
        let body = Bytes::from("{}");
        assert_eq!(
            forge.validate_event(&headers, &body, None).unwrap_err(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_gitlab_validate_event_rejects_ping() {
        let forge = GitLabForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-event", "ping".parse().unwrap());
        let body = Bytes::from("{}");
        assert_eq!(
            forge.validate_event(&headers, &body, None).unwrap_err(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_github_review_intent_accepts_commit_changing_actions() {
        let forge = GitHubForge;
        for action in ["opened", "reopened", "synchronize", "ready_for_review"] {
            assert_eq!(
                forge.review_intent(action),
                ReviewIntent::Review,
                "{action} should be reviewed"
            );
        }
    }

    #[test]
    fn test_github_review_intent_skips_metadata_actions() {
        let forge = GitHubForge;
        for action in [
            "labeled",
            "unlabeled",
            "edited",
            "assigned",
            "review_requested",
            "closed",
            "converted_to_draft",
            // An action GitHub has not invented yet is skipped rather than
            // charged a review on the guess that it moved the head commit.
            "some_future_action",
        ] {
            assert_eq!(
                forge.review_intent(action),
                ReviewIntent::Skip,
                "{action} should be skipped"
            );
        }
    }

    #[test]
    fn test_gitlab_review_intent_reviews_every_merge_request_hook() {
        let forge = GitLabForge;
        assert_eq!(forge.review_intent("merge_request"), ReviewIntent::Review);
    }

    #[test]
    fn test_extract_repo_name_from_mr_url() {
        assert_eq!(
            extract_repo_name_from_mr_url("https://gitlab.com/org/repo/-/merge_requests/10"),
            Some("repo".to_string())
        );
        assert_eq!(
            extract_repo_name_from_mr_url("https://github.com/sashiko-dev/sashiko/pull/501"),
            Some("sashiko".to_string())
        );
        assert_eq!(
            extract_repo_name_from_mr_url("https://example.com/not-a-pr"),
            None
        );
    }

    #[test]
    fn test_extract_owner_repo_from_mr_url() {
        assert_eq!(
            extract_owner_repo_from_mr_url("https://github.com/sashiko-dev/sashiko/pull/501"),
            Some("sashiko-dev/sashiko".to_string())
        );
        assert_eq!(
            extract_owner_repo_from_mr_url("https://gitlab.com/org/sub/repo/-/merge_requests/10"),
            Some("org/sub/repo".to_string())
        );
        assert_eq!(
            extract_owner_repo_from_mr_url("https://example.com/not-a-pr"),
            None
        );
    }

    #[test]
    fn test_compose_pr_review_comment_clean() {
        let patches = vec![
            PatchReviewSummaryItem {
                part_index: 1,
                total_parts: 2,
                commit_id: Some("1234567890abcdef1234567890abcdef12345678".to_string()),
                subject: "first clean patch".to_string(),
                inline_review: None,
                failed: false,
            },
            PatchReviewSummaryItem {
                part_index: 2,
                total_parts: 2,
                commit_id: Some("fedcba0987654321fedcba0987654321fedcba09".to_string()),
                subject: "second clean patch".to_string(),
                inline_review: Some("No issues found.".to_string()),
                failed: false,
            },
        ];
        let body = compose_pr_review_comment(
            Some(1),
            2,
            None,
            &patches,
            false,
            "https://sashiko.sashiko.dev/#/patchset/mr-501-abc..def",
        );
        assert!(body.starts_with("### Sashiko review\n\n"));
        assert!(body.contains("✓ **No issues found** across 2 commits."));
        assert!(!is_incomplete_pr_review_comment(&body));
        assert!(
            body.contains(
                "[Full review log on sashiko.sashiko.dev](https://sashiko.sashiko.dev/#/patchset/mr-501-abc..def)"
            )
        );
    }

    #[test]
    fn test_compose_pr_review_comment_with_findings_and_version() {
        let patches = vec![
            PatchReviewSummaryItem {
                part_index: 1,
                total_parts: 2,
                commit_id: Some("1234567890abcdef1234567890abcdef12345678".to_string()),
                subject: "clean commit".to_string(),
                inline_review: None,
                failed: false,
            },
            PatchReviewSummaryItem {
                part_index: 2,
                total_parts: 2,
                commit_id: Some("fedcba0987654321fedcba0987654321fedcba09".to_string()),
                subject: "commit with bug".to_string(),
                inline_review: Some("Severity: HIGH\nMissing check on input buffer.".to_string()),
                failed: false,
            },
        ];
        let body = compose_pr_review_comment(
            Some(3),
            2,
            Some("Series summary text here."),
            &patches,
            false,
            "https://sashiko.sashiko.dev/#/patchset/mr-501-v3",
        );
        assert!(body.starts_with("### Sashiko review — v3\n\n"));
        assert!(body.contains("<details>\n<summary>Series summary</summary>\n\nSeries summary text here.\n\n</details>"));
        assert!(!body.contains("clean commit"));
        assert!(body.contains("#### Commit 2/2 — `fedcba09` commit with bug"));
        assert!(body.contains("Severity: HIGH\nMissing check on input buffer."));
        assert!(!is_incomplete_pr_review_comment(&body));
        assert!(
            body.contains(
                "[Full review and stage logs on sashiko.sashiko.dev](https://sashiko.sashiko.dev/#/patchset/mr-501-v3)"
            )
        );
    }

    #[test]
    fn test_compose_pr_review_comment_partial_failure_without_findings() {
        let patches = vec![
            PatchReviewSummaryItem {
                part_index: 1,
                total_parts: 3,
                commit_id: Some("1234567890abcdef1234567890abcdef12345678".to_string()),
                subject: "first clean patch".to_string(),
                inline_review: None,
                failed: false,
            },
            PatchReviewSummaryItem {
                part_index: 2,
                total_parts: 3,
                commit_id: Some("abcdef1234567890abcdef1234567890abcdef12".to_string()),
                subject: "failed commit".to_string(),
                inline_review: None,
                failed: true,
            },
            PatchReviewSummaryItem {
                part_index: 3,
                total_parts: 3,
                commit_id: Some("fedcba0987654321fedcba0987654321fedcba09".to_string()),
                subject: "third clean patch".to_string(),
                inline_review: Some("No issues found.".to_string()),
                failed: false,
            },
        ];
        let body = compose_pr_review_comment(
            Some(1),
            3,
            None,
            &patches,
            false,
            "https://sashiko.sashiko.dev/#/patchset/sashiko-733",
        );
        assert!(is_incomplete_pr_review_comment(&body));
        assert!(!body.contains("No issues found"));
        assert!(body.contains(
            "⚠ **Review incomplete** — 1 of 3 commits failed to complete review (`2/3`)."
        ));
        assert!(
            body.contains(
                "[Full review and stage logs on sashiko.sashiko.dev](https://sashiko.sashiko.dev/#/patchset/sashiko-733)"
            )
        );
    }

    #[test]
    fn test_compose_pr_review_comment_partial_failure_preserves_findings() {
        let patches = vec![
            PatchReviewSummaryItem {
                part_index: 1,
                total_parts: 3,
                commit_id: Some("1111111122222222333333334444444455555555".to_string()),
                subject: "timed out commit".to_string(),
                inline_review: None,
                failed: true,
            },
            PatchReviewSummaryItem {
                part_index: 2,
                total_parts: 3,
                commit_id: Some("aaaaaaaa22222222333333334444444455555555".to_string()),
                subject: "commit with real bug".to_string(),
                inline_review: Some("Severity: MEDIUM\nUnbounded retry loop.".to_string()),
                failed: false,
            },
            PatchReviewSummaryItem {
                part_index: 3,
                total_parts: 3,
                commit_id: Some("bbbbbbbb22222222333333334444444455555555".to_string()),
                subject: "clean commit".to_string(),
                inline_review: None,
                failed: false,
            },
        ];
        let body = compose_pr_review_comment(
            Some(2),
            3,
            Some("Series overview."),
            &patches,
            false,
            "https://sashiko.sashiko.dev/#/patchset/sashiko-733",
        );
        assert!(body.starts_with("### Sashiko review — v2\n\n"));
        assert!(is_incomplete_pr_review_comment(&body));
        assert!(body.contains(
            "⚠ **Review incomplete** — 1 of 3 commits failed to complete review (`1/3`)."
        ));
        assert!(body.contains("<details>\n<summary>Series summary</summary>"));
        assert!(body.contains("#### Commit 2/3 — `aaaaaaaa` commit with real bug"));
        assert!(body.contains("Severity: MEDIUM\nUnbounded retry loop."));
    }

    #[test]
    fn test_compose_pr_review_comment_many_failures_and_self_quote() {
        let many_failed: Vec<PatchReviewSummaryItem> = (1..=7)
            .map(|idx| PatchReviewSummaryItem {
                part_index: idx,
                total_parts: 7,
                commit_id: None,
                subject: format!("patch {idx}"),
                inline_review: None,
                failed: true,
            })
            .collect();
        let body = compose_pr_review_comment(
            None,
            7,
            None,
            &many_failed,
            false,
            "https://sashiko.sashiko.dev/#/patchset/sashiko-700",
        );
        assert!(is_incomplete_pr_review_comment(&body));
        assert!(body.contains(
            "7 of 7 commits failed to complete review (`1/7`, `2/7`, `3/7`, `4/7`, `5/7`, +2 more)."
        ));

        // A completed review whose finding quotes INCOMPLETE_REVIEW_MARKER must not
        // be misclassified as an incomplete review comment.
        let completed_quoting_marker = compose_pr_review_comment(
            None,
            1,
            None,
            &[PatchReviewSummaryItem {
                part_index: 1,
                total_parts: 1,
                commit_id: None,
                subject: "forge: tweak banner".to_string(),
                inline_review: Some(format!(
                    "Check usage of `{INCOMPLETE_REVIEW_MARKER}` in src/forge.rs."
                )),
                failed: false,
            }],
            false,
            "https://sashiko.sashiko.dev/#/patchset/sashiko-701",
        );
        assert!(!is_incomplete_pr_review_comment(&completed_quoting_marker));
    }

    #[test]
    fn test_compose_pr_review_comment_failed_patchset_without_failed_commits() {
        let patches = vec![PatchReviewSummaryItem {
            part_index: 1,
            total_parts: 1,
            commit_id: None,
            subject: "clean commit".to_string(),
            inline_review: None,
            failed: false,
        }];
        let body = compose_pr_review_comment(
            None,
            1,
            None,
            &patches,
            true,
            "https://sashiko.sashiko.dev/#/patchset/sashiko-702",
        );
        assert!(is_incomplete_pr_review_comment(&body));
        assert!(body.contains("⚠ **Review incomplete** — review failed to complete."));
        assert!(!body.contains("No issues found"));
    }

    #[test]
    fn test_mint_github_app_jwt_valid_rsa_key() {
        let test_pem = "-----BEGIN PRIVATE KEY-----\n\
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDpmekVGfs9pOaJ\n\
Fwiha1czG7AKrJOV3vMY5TOTpUdS78e9K0x5aq7GiVesTQE+SWpL6Z+YJ34kfP7F\n\
IMYJdyWe2i8nZIunMsZixNr9ljiGC19FZZeUelgWf5YyocJ3JfJfuqYnif0gZuUL\n\
HevyBwEItFXMmj6ZGVCTvHHeo8g4eSqvwCcD7c1TfcRgHc/hy82+nALsuifyvxjo\n\
vkXNKsJCm+W/FEhsXNxQ0Ros4H8BcDpJw1HI2XHJHcX+ouonEjTzMX7lzxdvbNp6\n\
XbPf1ANJJPtq/KVjNFTUrrMeLdph0J0KDJtDK3bK/Uls+2VS12YKQHJs0ylq05l8\n\
Bavic5wZAgMBAAECggEAGDKYkaur+h9iFK1NeDsVlfZg7pogfOkn/ATcqjH4CL/3\n\
IWyiHVRPCm3LpnjLj4Isqp8RUxeJhN9rHKG1IeHfrxbMGk6F+38noa+L57IZ5M4P\n\
blwvBB2wO5Ride2KUVadnAufjn+dYtqFu028KnP4nXLguF2kl638ShwTx4uQ/+21\n\
M3SYoKl+FQCUs4kts1pYgzGa3NpV0LCJnfGz1eGwajO2saYLCBaimrX/hprexzmm\n\
Th2ITsZab75tDaMKbpyVZIfN3wOCxthV9byXOIFndVaDmzrghzPBL1iGKWcMCnMT\n\
onlB27jVXnrGlTGkVLH9qKQ7Z6Lyj9GV6xddx9oQSQKBgQD2ayKONlQGwzLUB8s5\n\
eKRT6d/uhTl9WeNX0KXAsiYjDH8vXZqDR2YUPs3hSqABpQuH+EEeF8h/5BMmq/da\n\
M9+sZyqsH/B27N5JbnhseOnYccd+4PsfRISEwkLa9DkKS2tQZy2A1lnLWo6TAe37\n\
8bq2LXAPZiOeEEc7GCt0yVDpfwKBgQDyrzEDrQPJr/6MvHWi4qDumSYK2+fXNP1B\n\
FqnP3L09C0TTvj1Ia5HLHJ/CeiHq6suC8D+QrDbiu01DbyACBeNq9y2fzv5cixZx\n\
V58aNZd7w45SqmSieYgnCi+z/YN6VDkTNVp6Pyn4ZK/I8oxWAkWm1q8ZjAoxBaua\n\
OQQKVAtWZwKBgQCSB8+Eo6GMGGWoza2bs2j+6ZxxR7ZYGMrnoZh455o+Lwu4UCpf\n\
HhLacJWlq4nDL8HzpCVC5ilF0S2gP0zowdEN5F2ff5YLhDf/IF5xOf6q7FKjWES5\n\
tOsrmcvw4cZj2WoRTfPjZCP2pQXVDNGx+wEBMVA1b/wvkcoEtUAbh6pRlQKBgQDe\n\
uK2xA+4AAYcJvkPv0zGDCAaD3MHvHfB29ceuvpTmGxt1gJhZiG9rCsAMCW5rXESd\n\
zMNpkMNmXiNQigHEGYdXObYjfiKu5+8W4iVgNmLp8NUDROHKwuKTgaO5+iXZ9MXU\n\
vRhmLOXl0vII56CnpropnclhFsabquqMRVtR50PobQKBgBhRj6Wi9QDQ/6/kk5VG\n\
2evCz2SRP0Mua25y3+gNDNcjfIVaiQdCd5lJMG3G9esdc3SJ2lbz+QQUbk1U5Jcz\n\
NS77VgBQLugIAhcS11DAtF4vd29/Jc1kDsQQ30Or5ONNGjMe0x0WN+uGRzrmLI6U\n\
bfBnKqGjJguuHd5ta5Vh5B51\n\
-----END PRIVATE KEY-----";

        let jwt = mint_github_app_jwt(4982337, test_pem).expect("should sign JWT");
        assert_eq!(jwt.split('.').count(), 3);
    }

    #[test]
    fn test_is_dependabot_author_and_github_payload() {
        assert!(is_dependabot_author("dependabot[bot]"));
        assert!(is_dependabot_author("dependabot"));
        assert_eq!(loggable("dependabot[bot]"), "dependabot[bot]");
        assert!(!is_dependabot_author("kfree"));
        assert!(!is_dependabot_author(
            "Roman Gushchin <roman.gushchin@linux.dev>"
        ));

        let body = Bytes::from(
            serde_json::json!({
                "action": "opened",
                "pull_request": {
                    "number": 42,
                    "title": "Bump tokio from 1.40.0 to 1.41.0",
                    "html_url": "https://github.com/sashiko-dev/sashiko/pull/42",
                    "user": { "login": "dependabot[bot]" },
                    "head": { "sha": "1111111111111111111111111111111111111111" },
                    "base": { "sha": "2222222222222222222222222222222222222222" }
                },
                "repository": {
                    "clone_url": "https://github.com/sashiko-dev/sashiko.git"
                }
            })
            .to_string(),
        );

        let forge = GitHubForge;
        let (_action, metadata) = forge.parse_payload(&body).expect("valid payload");
        assert_eq!(metadata.author.as_deref(), Some("dependabot[bot]"));
        assert!(metadata.author.as_deref().is_some_and(is_dependabot_author));
    }

    #[test]
    fn test_normalize_github_repo_slug() {
        assert_eq!(
            normalize_github_repo_slug("sashiko-dev/sashiko"),
            Some("sashiko-dev/sashiko".to_string())
        );
        assert_eq!(
            normalize_github_repo_slug(" https://github.com/sashiko-dev/sashiko.git "),
            Some("sashiko-dev/sashiko".to_string())
        );
        assert_eq!(
            normalize_github_repo_slug("https://github.com/sashiko-dev/sashiko/pull/735"),
            Some("sashiko-dev/sashiko".to_string())
        );
        assert_eq!(
            normalize_github_repo_slug("git@github.com:sashiko-dev/sashiko.git"),
            Some("sashiko-dev/sashiko".to_string())
        );
        assert_eq!(
            normalize_github_repo_slug("ssh://git@github.com/sashiko-dev/sashiko.git"),
            Some("sashiko-dev/sashiko".to_string())
        );

        for invalid in [
            "",
            "sashiko-dev",
            "sashiko-dev/sashiko/extra",
            "../sashiko",
            "sashiko-dev/..",
            "https://gitlab.com/sashiko-dev/sashiko.git",
            "git@gitlab.com:sashiko-dev/sashiko.git",
            "https://user:pass@github.com/sashiko-dev/sashiko.git",
            "sashiko-dev/sashiko?foo=bar",
        ] {
            assert_eq!(
                normalize_github_repo_slug(invalid),
                None,
                "{invalid} should be rejected"
            );
        }
    }

    #[tokio::test]
    async fn test_detect_github_repo_from_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let status = crate::git_cmd::in_dir_async(dir.path())
            .args(["init", "--bare"])
            .status()
            .await
            .unwrap();
        assert!(status.success());

        assert_eq!(detect_github_repo_from_git_dir(dir.path()).await, None);

        let status = crate::git_cmd::in_dir_async(dir.path())
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/sashiko-dev/sashiko.git",
            ])
            .status()
            .await
            .unwrap();
        assert!(status.success());

        assert_eq!(
            detect_github_repo_from_git_dir(dir.path()).await,
            Some("sashiko-dev/sashiko".to_string())
        );
    }
}
