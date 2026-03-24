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

//! Gerrit review backend — posts sashiko findings as inline comments.
//!
//! When `[gerrit]` is configured in Settings.toml, completed reviews are
//! automatically posted back to the originating Gerrit change as inline
//! comments with a cover message summarizing the findings.
//!
//! # Configuration
//!
//! ```toml
//! [gerrit]
//! url = "https://review.example.com"
//! username = "sashiko"
//! password = "http-password"       # Gerrit HTTP password (not SSH key)
//! # project = "lustre-release"     # optional: restrict to one project
//! # dry_run = false                # optional: log but don't post (default: false)
//! ```

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tracing::{debug, error, info, warn};

use crate::db::Severity;
use crate::settings::GerritSettings;

/// A single inline comment to post on a Gerrit change.
#[derive(Debug, Clone, Serialize)]
struct InlineComment {
    path: String,
    line: i64,
    message: String,
    unresolved: bool,
}

/// Gerrit "set review" request body.
/// See: https://gerrit-review.googlesource.com/Documentation/rest-api-changes.html#set-review
#[derive(Debug, Serialize)]
struct SetReviewInput {
    message: String,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    comments: HashMap<String, Vec<CommentInput>>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    labels: HashMap<String, i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
}

#[derive(Debug, Serialize)]
struct CommentInput {
    line: i64,
    message: String,
    unresolved: Option<bool>,
}

/// A structured finding ready for Gerrit posting.
#[derive(Debug, Clone)]
pub struct GerritFinding {
    pub severity: Severity,
    pub problem: String,
    pub severity_explanation: Option<String>,
    pub file_path: Option<String>,
    pub line_number: Option<i64>,
}

/// Severity -> Code-Review vote mapping.
fn severity_vote(severity: &Severity) -> i32 {
    match severity {
        Severity::Critical => -2,
        Severity::High => -1,
        Severity::Medium | Severity::Low => 0,
    }
}

/// Details about a Gerrit change needed for fetching and posting back.
#[derive(Debug, Clone, Deserialize)]
pub struct ChangeInfo {
    pub project: String,
    pub subject: String,
    pub current_revision: Option<String>,
    #[serde(default)]
    pub revisions: HashMap<String, RevisionInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RevisionInfo {
    #[serde(rename = "_number")]
    pub number: Option<u32>,
    #[serde(default)]
    pub fetch: HashMap<String, FetchInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FetchInfo {
    pub url: String,
    #[serde(rename = "ref")]
    pub fetch_ref: String,
}

/// Gerrit REST client for fetching changes and posting review results.
pub struct GerritClient {
    client: Client,
    settings: GerritSettings,
}

impl GerritClient {
    pub fn new(settings: GerritSettings) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("Failed to build HTTP client for Gerrit")?;

        info!(
            "Gerrit backend configured: {} (dry_run={})",
            settings.url, settings.dry_run
        );

        Ok(Self { client, settings })
    }

    /// Get the URL configured for this Gerrit instance.
    pub fn url(&self) -> &str {
        &self.settings.url
    }

    /// Fetch change details from Gerrit REST API.
    ///
    /// Returns the change info including the current revision's fetch ref,
    /// which is needed to `git fetch` the change into a local repo.
    pub async fn get_change_detail(&self, change_number: u64) -> Result<ChangeInfo> {
        let url = format!(
            "{}/a/changes/{}?o=CURRENT_REVISION&o=DOWNLOAD_COMMANDS",
            self.settings.url.trim_end_matches('/'),
            change_number
        );

        debug!("Fetching Gerrit change detail: {}", url);

        let resp = self
            .client
            .get(&url)
            .basic_auth(&self.settings.username, Some(&self.settings.password))
            .send()
            .await
            .context("Failed to fetch change detail from Gerrit")?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!(
                "Gerrit returned HTTP {} for change {}: {}",
                status,
                change_number,
                body.chars().take(300).collect::<String>()
            );
        }

        // Gerrit prefixes JSON responses with )]}'
        let body = resp.text().await?;
        let json_body = body
            .strip_prefix(")]}'")
            .map(|s| s.trim_start())
            .unwrap_or(&body);

        let change: ChangeInfo =
            serde_json::from_str(json_body).context("Failed to parse Gerrit change detail")?;

        info!(
            "Gerrit change {}: project={}, subject={}",
            change_number, change.project, change.subject
        );

        Ok(change)
    }

    /// Extract the fetch ref and URL from a ChangeInfo.
    ///
    /// Returns (fetch_url, fetch_ref, patchset_number, commit_sha).
    pub fn extract_fetch_info(
        &self,
        change: &ChangeInfo,
        change_number: u64,
    ) -> Result<(String, String, u32, String)> {
        let current_rev = change
            .current_revision
            .as_deref()
            .context("Change has no current revision")?;

        let rev_data = change
            .revisions
            .get(current_rev)
            .context("Current revision not found in revisions map")?;

        let patchset_number = rev_data.number.unwrap_or(1);

        // Try fetch protocols in preference order
        let (fetch_url, fetch_ref) = ["anonymous http", "http", "ssh"]
            .iter()
            .find_map(|protocol| {
                rev_data.fetch.get(*protocol).map(|fi| {
                    (fi.url.clone(), fi.fetch_ref.clone())
                })
            })
            .unwrap_or_else(|| {
                // Construct manually
                let change_str = change_number.to_string();
                let suffix = if change_str.len() >= 2 {
                    &change_str[change_str.len() - 2..]
                } else {
                    &change_str
                };
                let fetch_ref = format!(
                    "refs/changes/{}/{}/{}",
                    suffix, change_number, patchset_number
                );
                let fetch_url = format!(
                    "{}/{}",
                    self.settings.url.trim_end_matches('/'),
                    change.project
                );
                (fetch_url, fetch_ref)
            });

        info!(
            "Gerrit change {} ps{}: ref={}, sha={}",
            change_number,
            patchset_number,
            fetch_ref,
            &current_rev[..12.min(current_rev.len())]
        );

        Ok((fetch_url, fetch_ref, patchset_number, current_rev.to_string()))
    }

    /// Fetch a Gerrit change into a local git repository.
    ///
    /// Runs `git fetch <url> <ref>` and returns the FETCH_HEAD SHA.
    pub async fn fetch_into_repo(
        &self,
        repo_path: &str,
        fetch_url: &str,
        fetch_ref: &str,
    ) -> Result<String> {
        info!("Fetching {} {} into {}", fetch_url, fetch_ref, repo_path);

        let output = tokio::process::Command::new("git")
            .args(["fetch", fetch_url, fetch_ref])
            .current_dir(repo_path)
            .output()
            .await
            .context("Failed to run git fetch")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git fetch failed: {}", stderr);
        }

        let sha_output = tokio::process::Command::new("git")
            .args(["rev-parse", "FETCH_HEAD"])
            .current_dir(repo_path)
            .output()
            .await
            .context("Failed to run git rev-parse FETCH_HEAD")?;

        if !sha_output.status.success() {
            anyhow::bail!("git rev-parse FETCH_HEAD failed");
        }

        let sha = String::from_utf8_lossy(&sha_output.stdout).trim().to_string();
        info!("Fetched commit: {}", &sha[..12.min(sha.len())]);
        Ok(sha)
    }

    /// Post review findings to a Gerrit change.
    ///
    /// `change_id` can be a change number, Change-Id, or "project~branch~Change-Id".
    /// `revision_id` is the patchset commit SHA or patchset number.
    pub async fn post_review(
        &self,
        change_id: &str,
        revision_id: &str,
        findings: &[GerritFinding],
        include_vote: bool,
    ) -> Result<()> {
        let (comments, summary) = self.findings_to_review(findings);

        let mut labels = HashMap::new();
        if include_vote && !findings.is_empty() {
            let worst_vote = findings
                .iter()
                .map(|f| severity_vote(&f.severity))
                .min()
                .unwrap_or(0);
            if worst_vote != 0 {
                labels.insert("Code-Review".to_string(), worst_vote);
            }
        }

        let input = SetReviewInput {
            message: summary,
            comments,
            labels,
            tag: Some("autogenerated:sashiko".to_string()),
        };

        if self.settings.dry_run {
            let comment_count: usize = input.comments.values().map(|v| v.len()).sum();
            info!(
                "Gerrit dry-run: would post {} inline comment(s) to {}/{}",
                comment_count, change_id, revision_id
            );
            debug!("Gerrit dry-run payload: {}", serde_json::to_string_pretty(&input)?);
            return Ok(());
        }

        let url = format!(
            "{}/a/changes/{}/revisions/{}/review",
            self.settings.url.trim_end_matches('/'),
            change_id,
            revision_id
        );

        let comment_count: usize = input.comments.values().map(|v| v.len()).sum();
        info!(
            "Posting {} inline comment(s) to Gerrit: {}",
            comment_count, url
        );

        let resp = self
            .client
            .post(&url)
            .basic_auth(&self.settings.username, Some(&self.settings.password))
            .json(&input)
            .send()
            .await
            .context("Failed to send review to Gerrit")?;

        let status = resp.status();
        if status.is_success() {
            info!(
                "Successfully posted review to Gerrit change {} (HTTP {})",
                change_id, status
            );
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            error!(
                "Gerrit returned HTTP {}: {}",
                status,
                body.chars().take(500).collect::<String>()
            );
            anyhow::bail!("Gerrit review post failed: HTTP {}", status)
        }
    }

    /// Convert findings into Gerrit comment format + summary message.
    fn findings_to_review(
        &self,
        findings: &[GerritFinding],
    ) -> (HashMap<String, Vec<CommentInput>>, String) {
        let mut comments: HashMap<String, Vec<CommentInput>> = HashMap::new();
        let mut unlocated: Vec<String> = Vec::new();

        for f in findings {
            let mut msg_parts = vec![format!("[{}] {}", f.severity, f.problem)];
            if let Some(ref explanation) = f.severity_explanation {
                msg_parts.push(format!("\nReasoning: {}", explanation));
            }
            let message = msg_parts.join("");

            if let (Some(path), Some(line)) = (&f.file_path, f.line_number) {
                comments
                    .entry(path.clone())
                    .or_default()
                    .push(CommentInput {
                        line,
                        message,
                        unresolved: Some(matches!(f.severity, Severity::High | Severity::Critical)),
                    });
            } else {
                unlocated.push(format!(
                    "- [{}] {}",
                    f.severity,
                    f.problem.chars().take(200).collect::<String>()
                ));
            }
        }

        // Build summary
        let mut summary_parts = vec![
            "Sashiko Automated Review".to_string(),
            "=".repeat(25),
        ];

        if findings.is_empty() {
            summary_parts.push("No issues found.".to_string());
        } else {
            let mut counts: HashMap<String, usize> = HashMap::new();
            for f in findings {
                *counts.entry(format!("{}", f.severity)).or_default() += 1;
            }
            let count_str: Vec<String> = counts
                .iter()
                .map(|(sev, count)| format!("{} {}", count, sev))
                .collect();
            summary_parts.push(format!(
                "Found {} issue(s): {}",
                findings.len(),
                count_str.join(", ")
            ));
        }

        if !unlocated.is_empty() {
            summary_parts.push(String::new());
            summary_parts.push("General findings (no specific file location):".to_string());
            summary_parts.extend(unlocated);
        }

        (comments, summary_parts.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_severity_votes() {
        assert_eq!(severity_vote(&Severity::Critical), -2);
        assert_eq!(severity_vote(&Severity::High), -1);
        assert_eq!(severity_vote(&Severity::Medium), 0);
        assert_eq!(severity_vote(&Severity::Low), 0);
    }

    #[test]
    fn test_findings_to_review_empty() {
        let settings = GerritSettings {
            url: "https://review.example.com".to_string(),
            username: "test".to_string(),
            password: "test".to_string(),
            project: None,
            dry_run: false,
            vote: false,
        };
        let client = GerritClient::new(settings).unwrap();
        let (comments, summary) = client.findings_to_review(&[]);
        assert!(comments.is_empty());
        assert!(summary.contains("No issues found"));
    }

    #[test]
    fn test_findings_to_review_with_location() {
        let settings = GerritSettings {
            url: "https://review.example.com".to_string(),
            username: "test".to_string(),
            password: "test".to_string(),
            project: None,
            dry_run: false,
            vote: false,
        };
        let client = GerritClient::new(settings).unwrap();
        let findings = vec![GerritFinding {
            severity: Severity::High,
            problem: "Use after free in foo()".to_string(),
            severity_explanation: Some("Buffer freed on line 10, used on line 15".to_string()),
            file_path: Some("fs/lustre/llite/file.c".to_string()),
            line_number: Some(15),
        }];
        let (comments, summary) = client.findings_to_review(&findings);
        assert_eq!(comments.len(), 1);
        assert!(comments.contains_key("fs/lustre/llite/file.c"));
        let file_comments = &comments["fs/lustre/llite/file.c"];
        assert_eq!(file_comments.len(), 1);
        assert_eq!(file_comments[0].line, 15);
        assert!(file_comments[0].message.contains("Use after free"));
        assert_eq!(file_comments[0].unresolved, Some(true)); // High severity
        assert!(summary.contains("1 issue(s)"));
    }

    #[test]
    fn test_findings_to_review_unlocated() {
        let settings = GerritSettings {
            url: "https://review.example.com".to_string(),
            username: "test".to_string(),
            password: "test".to_string(),
            project: None,
            dry_run: false,
            vote: false,
        };
        let client = GerritClient::new(settings).unwrap();
        let findings = vec![GerritFinding {
            severity: Severity::Medium,
            problem: "Missing error handling".to_string(),
            severity_explanation: None,
            file_path: None,
            line_number: None,
        }];
        let (comments, summary) = client.findings_to_review(&findings);
        assert!(comments.is_empty()); // No location = no inline comment
        assert!(summary.contains("General findings"));
        assert!(summary.contains("Missing error handling"));
    }
}
