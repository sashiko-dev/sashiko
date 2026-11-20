use crate::db::Severity;
use crate::email_policy::PatchworkPolicy;
use reqwest::{Client, header};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info, warn};

/// Result of evaluating a patchwork check from a policy and findings.
/// Separates config (PatchworkPolicy) from computed output.
#[derive(Debug, PartialEq)]
pub struct PatchworkCheckResult {
    /// Patchwork check state: "success", "warning", or "fail"
    pub state: String,
    /// Human-readable description with per-severity breakdown
    pub description: String,
}

/// Per-severity counts for new and pre-existing findings.
#[derive(Debug, Default)]
struct SeverityCounts {
    new: usize,
    preexisting: usize,
}

impl PatchworkCheckResult {
    /// The check for a patch whose clean verdict was inherited from an
    /// earlier review of the same diff rather than produced by a model run
    /// on this copy.
    pub fn inherited_clean() -> Self {
        Self {
            state: "success".to_string(),
            description:
                "Sashiko AI review found no regressions (unchanged since an earlier review)"
                    .to_string(),
        }
    }

    /// Build a check result from a patchwork policy and raw findings.
    ///
    /// Applies min_severity filtering, splits by preexisting flag,
    /// computes the check state from fail_severity threshold (new
    /// findings only), and formats the description with per-severity
    /// breakdown including pre-existing counts.
    pub fn from_policy(policy: &PatchworkPolicy, findings: &[Value]) -> Self {
        let min_threshold = policy
            .min_severity
            .as_ref()
            .map(|s| Severity::from_str(s) as i32)
            .unwrap_or(Severity::Low as i32);

        let fail_threshold = Severity::from_str(&policy.fail_severity) as i32;

        // Count findings per severity, split by new vs pre-existing
        let mut critical = SeverityCounts::default();
        let mut high = SeverityCounts::default();
        let mut medium = SeverityCounts::default();
        let mut low = SeverityCounts::default();

        for f in findings {
            let sev =
                Severity::from_str(f.get("severity").and_then(|v| v.as_str()).unwrap_or("Low"));

            // Apply min_severity filter
            if (sev as i32) < min_threshold {
                continue;
            }

            let is_preexisting = f
                .get("preexisting")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let bucket = match sev {
                Severity::Critical => &mut critical,
                Severity::High => &mut high,
                Severity::Medium => &mut medium,
                Severity::Low | Severity::Unknown => &mut low,
            };

            if is_preexisting {
                bucket.preexisting += 1;
            } else {
                bucket.new += 1;
            }
        }

        let total_new = critical.new + high.new + medium.new + low.new;
        let total_preexisting =
            critical.preexisting + high.preexisting + medium.preexisting + low.preexisting;

        // Determine state from NEW findings only
        let state = if total_new == 0 {
            "success"
        } else {
            // Check if any new finding meets the fail threshold
            let has_fail_level = [
                (Severity::Critical as i32, critical.new),
                (Severity::High as i32, high.new),
                (Severity::Medium as i32, medium.new),
                (Severity::Low as i32, low.new),
            ]
            .iter()
            .any(|(sev, count)| *count > 0 && *sev >= fail_threshold);

            if has_fail_level { "fail" } else { "warning" }
        };

        // Build description
        let description = if total_new == 0 && total_preexisting == 0 {
            "Sashiko AI review found no regressions".to_string()
        } else {
            Self::format_description(&critical, &high, &medium, &low)
        };

        Self {
            state: state.to_string(),
            description,
        }
    }

    /// Format per-severity description, dropping zero-count severities.
    /// New counts shown bare, pre-existing in parentheses.
    /// Example: "Critical: 1 · High: 2 (1 pre-existing)"
    fn format_description(
        critical: &SeverityCounts,
        high: &SeverityCounts,
        medium: &SeverityCounts,
        low: &SeverityCounts,
    ) -> String {
        let mut parts = Vec::new();

        for (label, counts) in [
            ("Critical", critical),
            ("High", high),
            ("Medium", medium),
            ("Low", low),
        ] {
            if counts.new == 0 && counts.preexisting == 0 {
                continue;
            }

            let part = if counts.new > 0 && counts.preexisting > 0 {
                format!(
                    "{}: {} ({} pre-existing)",
                    label, counts.new, counts.preexisting
                )
            } else if counts.new > 0 {
                format!("{}: {}", label, counts.new)
            } else {
                format!("{}: {} pre-existing", label, counts.preexisting)
            };

            parts.push(part);
        }

        parts.join(" \u{00b7} ") // middle dot separator
    }
}

#[derive(Debug, Deserialize)]
struct PatchworkListResponse {
    id: u64,
}

#[derive(Debug, Serialize)]
struct PatchworkCheckRequest {
    state: String,
    target_url: String,
    description: String,
    context: String,
}

/// Why a Patchwork check attempt did not succeed.
///
/// Distinguished from a plain string so the caller can tell a transient,
/// self-resolving condition apart from an outright API failure without
/// pattern-matching on message text.
#[derive(Debug)]
pub enum PatchworkCheckError {
    /// Patchwork's own mailing list mirror has not indexed this message
    /// yet. Sashiko can complete a review and attempt the check before
    /// Patchwork's mirror catches up; retrying once it does succeeds.
    NotIndexedYet,
    /// The API rejected the request or the network call itself failed.
    Api(String),
}

impl std::fmt::Display for PatchworkCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotIndexedYet => write!(f, "Patchwork has not indexed this message yet"),
            Self::Api(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for PatchworkCheckError {}

/// Post a check result to the Patchwork REST API for a given patch.
///
/// Looks up the patch by message-ID, then POSTs the check. Returns Ok
/// on success or Err describing the failure so the caller can decide
/// whether and how to retry.
pub async fn post_patchwork_check(
    client: &Client,
    api_url: &str,
    token: Option<&str>,
    msgid: &str,
    status: &str,
    description: &str,
    target_url: &str,
) -> Result<(), PatchworkCheckError> {
    let api_url = api_url.trim_end_matches('/');

    // Strip angle brackets from the message-ID.
    let clean_msgid = msgid.trim_matches(|c| c == '<' || c == '>');

    // Build the lookup URL with proper URL-encoding for the msgid.
    let base_url = format!("{}/patches/", api_url);
    let patches_url = reqwest::Url::parse_with_params(&base_url, &[("msgid", clean_msgid)])
        .map_err(|e| PatchworkCheckError::Api(format!("failed to build patchwork URL: {}", e)))?;
    debug!("Fetching Patchwork patch by msgid: {}", clean_msgid);

    let mut get_req = client.get(patches_url);
    if let Some(token) = token {
        get_req = get_req.header(header::AUTHORIZATION, format!("Token {}", token));
    }

    let resp = get_req.send().await.map_err(|e| {
        PatchworkCheckError::Api(format!("failed to fetch patchwork patch list: {}", e))
    })?;

    if !resp.status().is_success() {
        let status_code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(PatchworkCheckError::Api(format!(
            "patchwork API returned {}: {}",
            status_code, body
        )));
    }

    let patches: Vec<PatchworkListResponse> = resp.json().await.map_err(|e| {
        PatchworkCheckError::Api(format!("failed to parse patchwork list response: {}", e))
    })?;

    if patches.is_empty() {
        debug!("Patchwork has not indexed msgid {} yet", msgid);
        return Err(PatchworkCheckError::NotIndexedYet);
    }

    if patches.len() > 1 {
        warn!(
            "Patchwork returned {} patches for msgid {}, using first (id={})",
            patches.len(),
            msgid,
            patches[0].id
        );
    }

    let patch_id = patches[0].id;
    let check_url = format!("{}/patches/{}/checks/", api_url, patch_id);

    let payload = PatchworkCheckRequest {
        state: status.to_string(),
        target_url: target_url.to_string(),
        description: description.to_string(),
        context: "sashiko".to_string(),
    };

    debug!("Posting check to Patchwork: {} {:?}", check_url, payload);

    let mut post_req = client.post(&check_url).json(&payload);
    if let Some(token) = token {
        post_req = post_req.header(header::AUTHORIZATION, format!("Token {}", token));
    }

    let post_resp = post_req
        .send()
        .await
        .map_err(|e| PatchworkCheckError::Api(format!("failed to post patchwork check: {}", e)))?;

    if post_resp.status().is_success() {
        info!("Successfully posted check to Patchwork for msgid {}", msgid);
        Ok(())
    } else {
        let status_code = post_resp.status();
        let body = post_resp.text().await.unwrap_or_default();
        Err(PatchworkCheckError::Api(format!(
            "patchwork check post failed with status {}: {}",
            status_code, body
        )))
    }
}

/// Compose a structured patchwork notification email for email-based mode.
///
/// Returns (subject, body). The body uses a simple key-value format
/// parseable by downstream tools such as pw_tools.
pub fn compose_patchwork_email(
    msgid: &str,
    status: &str,
    description: &str,
    target_url: &str,
    patch_subject: &str,
) -> (String, String) {
    let subject = format!("[sashiko-check] {} - {}", status, patch_subject);
    let body = format!(
        "msgid: {}\nstatus: {}\ndescription: {}\ntarget_url: {}\ncontext: sashiko\n",
        msgid, status, description, target_url
    );
    (subject, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::email_policy::PatchworkPolicy;

    fn finding(severity: &str, preexisting: bool) -> Value {
        serde_json::json!({"severity": severity, "problem": "test", "preexisting": preexisting})
    }

    fn finding_new(severity: &str) -> Value {
        finding(severity, false)
    }

    fn finding_preexisting(severity: &str) -> Value {
        finding(severity, true)
    }

    fn default_policy() -> PatchworkPolicy {
        PatchworkPolicy::default()
    }

    // -- PatchworkCheckResult tests --

    #[test]
    fn test_patchwork_check_result_from_policy() {
        let policy = default_policy();

        let empty = PatchworkCheckResult::from_policy(&policy, &[]);
        assert_eq!(empty.state, "success");
        assert_eq!(empty.description, "Sashiko AI review found no regressions");

        for (severity, expected_state, expected_label) in [
            ("Critical", "fail", "Critical: 1"),
            ("High", "fail", "High: 1"),
            ("Medium", "warning", "Medium: 1"),
            ("Low", "warning", "Low: 1"),
        ] {
            let result = PatchworkCheckResult::from_policy(&policy, &[finding_new(severity)]);
            assert_eq!(result.state, expected_state, "{severity}");
            assert!(result.description.contains(expected_label), "{severity}");
            for other in ["Critical", "High", "Medium", "Low"] {
                if other != severity {
                    assert!(!result.description.contains(other));
                }
            }
        }

        // Pre-existing only produces success.
        let pre_only = PatchworkCheckResult::from_policy(
            &policy,
            &[finding_preexisting("Critical"), finding_preexisting("High")],
        );
        assert_eq!(pre_only.state, "success");
        assert!(pre_only.description.contains("pre-existing"));

        // Mixed new and pre-existing across and within severities, with middle-dot separator.
        let mixed = PatchworkCheckResult::from_policy(
            &policy,
            &[
                finding_new("Critical"),
                finding_new("High"),
                finding_new("High"),
                finding_new("High"),
                finding_preexisting("High"),
            ],
        );
        assert_eq!(mixed.state, "fail");
        assert!(mixed.description.contains("Critical: 1"));
        assert!(mixed.description.contains("\u{00b7}"));
        assert!(mixed.description.contains("High: 3 (1 pre-existing)"));

        // Missing or null preexisting field defaults to new.
        for finding_val in [
            serde_json::json!({"severity": "High", "problem": "test"}),
            serde_json::json!({"severity": "High", "problem": "test", "preexisting": null}),
        ] {
            let result = PatchworkCheckResult::from_policy(&policy, &[finding_val]);
            assert_eq!(result.state, "fail");
            assert!(result.description.contains("High: 1"));
            assert!(!result.description.contains("pre-existing"));
        }
    }

    #[test]
    fn test_patchwork_check_result_min_and_fail_severity() {
        let min_medium = PatchworkPolicy {
            min_severity: Some("Medium".to_string()),
            ..Default::default()
        };
        let result = PatchworkCheckResult::from_policy(
            &min_medium,
            &[
                finding_new("Low"),
                finding_preexisting("Low"),
                finding_new("Medium"),
                finding_preexisting("High"),
            ],
        );
        assert_eq!(result.state, "warning");
        assert!(result.description.contains("Medium: 1"));
        assert!(result.description.contains("High: 1 pre-existing"));
        assert!(!result.description.contains("Low"));

        let min_critical = PatchworkPolicy {
            min_severity: Some("Critical".to_string()),
            ..Default::default()
        };
        let result = PatchworkCheckResult::from_policy(
            &min_critical,
            &[
                finding_new("Low"),
                finding_new("Medium"),
                finding_new("High"),
            ],
        );
        assert_eq!(result.state, "success");
        assert_eq!(result.description, "Sashiko AI review found no regressions");

        let fail_critical = PatchworkPolicy {
            fail_severity: "Critical".to_string(),
            ..Default::default()
        };
        assert_eq!(
            PatchworkCheckResult::from_policy(&fail_critical, &[finding_new("High")]).state,
            "warning"
        );

        let fail_low = PatchworkPolicy {
            fail_severity: "Low".to_string(),
            ..Default::default()
        };
        assert_eq!(
            PatchworkCheckResult::from_policy(&fail_low, &[finding_new("Low")]).state,
            "fail"
        );
    }

    // -- compose_patchwork_email tests --

    #[test]
    fn test_compose_patchwork_email() {
        let (subject, body) = compose_patchwork_email(
            "<12345@kernel.org>",
            "warning",
            "Sashiko AI review found 2 potential issue(s)",
            "https://sashiko.dev/#/patchset/abc?part=1",
            "[PATCH] fix null deref in foo",
        );

        assert_eq!(
            subject,
            "[sashiko-check] warning - [PATCH] fix null deref in foo"
        );
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(
            lines,
            [
                "msgid: <12345@kernel.org>",
                "status: warning",
                "description: Sashiko AI review found 2 potential issue(s)",
                "target_url: https://sashiko.dev/#/patchset/abc?part=1",
                "context: sashiko",
            ]
        );
    }

    // -- post_patchwork_check tests --

    #[test]
    fn test_patchwork_check_error_display() {
        assert_eq!(
            PatchworkCheckError::NotIndexedYet.to_string(),
            "Patchwork has not indexed this message yet"
        );
        assert_eq!(
            PatchworkCheckError::Api("boom".to_string()).to_string(),
            "boom"
        );
    }

    /// Spawn a one-shot HTTP server that replies to the first request on
    /// its single connection with a canned status line and body, then
    /// stops. Mirrors the raw-socket test server already used for NNTP in
    /// `src/ingestor.rs`; there is no HTTP mocking crate in this codebase.
    async fn one_shot_http_server(status_line: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);

            // Drain the request line and headers up to the blank line.
            loop {
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }

            let response = format!(
                "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
            stream.get_mut().flush().await.unwrap();
        });

        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn test_empty_patch_list_is_retryable_not_success() {
        // Patchwork's mailing list mirror has not indexed this message yet:
        // a 200 with an empty list, not an HTTP error.
        let api_url = one_shot_http_server("HTTP/1.1 200 OK", "[]").await;
        let client = Client::new();

        let result = post_patchwork_check(
            &client,
            &api_url,
            None,
            "<no-such-patch@kernel.org>",
            "success",
            "desc",
            "https://example.com",
        )
        .await;

        assert!(
            matches!(result, Err(PatchworkCheckError::NotIndexedYet)),
            "expected NotIndexedYet, got {result:?}"
        );
    }

    #[tokio::test]
    async fn test_api_error_status_is_not_mistaken_for_not_indexed() {
        let api_url = one_shot_http_server("HTTP/1.1 500 Internal Server Error", "boom").await;
        let client = Client::new();

        let result = post_patchwork_check(
            &client,
            &api_url,
            None,
            "<any@kernel.org>",
            "success",
            "desc",
            "https://example.com",
        )
        .await;

        assert!(
            matches!(result, Err(PatchworkCheckError::Api(_))),
            "expected Api error, got {result:?}"
        );
    }
}
