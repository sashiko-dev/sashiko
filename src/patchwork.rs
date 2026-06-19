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
                Severity::Low => &mut low,
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

/// Post a check result to the Patchwork REST API for a given patch.
///
/// Looks up the patch by message-ID, then POSTs the check. Returns Ok
/// on success or Err with a description on failure so the caller can
/// decide whether to retry.
pub async fn post_patchwork_check(
    client: &Client,
    api_url: &str,
    token: Option<&str>,
    msgid: &str,
    status: &str,
    description: &str,
    target_url: &str,
) -> Result<(), String> {
    let api_url = api_url.trim_end_matches('/');

    // Strip angle brackets from the message-ID.
    let clean_msgid = msgid.trim_matches(|c| c == '<' || c == '>');

    // Build the lookup URL with proper URL-encoding for the msgid.
    let base_url = format!("{}/patches/", api_url);
    let patches_url = reqwest::Url::parse_with_params(&base_url, &[("msgid", clean_msgid)])
        .map_err(|e| format!("failed to build patchwork URL: {}", e))?;
    debug!("Fetching Patchwork patch by msgid: {}", clean_msgid);

    let mut get_req = client.get(patches_url);
    if let Some(token) = token {
        get_req = get_req.header(header::AUTHORIZATION, format!("Token {}", token));
    }

    let resp = get_req
        .send()
        .await
        .map_err(|e| format!("failed to fetch patchwork patch list: {}", e))?;

    if !resp.status().is_success() {
        let status_code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("patchwork API returned {}: {}", status_code, body));
    }

    let patches: Vec<PatchworkListResponse> = resp
        .json()
        .await
        .map_err(|e| format!("failed to parse patchwork list response: {}", e))?;

    if patches.is_empty() {
        debug!("Patchwork returned no patches for msgid {}", msgid);
        return Ok(());
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
        .map_err(|e| format!("failed to post patchwork check: {}", e))?;

    if post_resp.status().is_success() {
        info!("Successfully posted check to Patchwork for msgid {}", msgid);
        Ok(())
    } else {
        let status_code = post_resp.status();
        let body = post_resp.text().await.unwrap_or_default();
        Err(format!(
            "patchwork check post failed with status {}: {}",
            status_code, body
        ))
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
    fn test_no_findings_success() {
        let result = PatchworkCheckResult::from_policy(&default_policy(), &[]);
        assert_eq!(result.state, "success");
        assert_eq!(result.description, "Sashiko AI review found no regressions");
    }

    #[test]
    fn test_new_high_produces_fail() {
        let findings = vec![finding_new("High")];
        let result = PatchworkCheckResult::from_policy(&default_policy(), &findings);
        assert_eq!(result.state, "fail");
        assert!(result.description.contains("High: 1"));
    }

    #[test]
    fn test_new_critical_produces_fail() {
        let findings = vec![finding_new("Critical")];
        let result = PatchworkCheckResult::from_policy(&default_policy(), &findings);
        assert_eq!(result.state, "fail");
        assert!(result.description.contains("Critical: 1"));
    }

    #[test]
    fn test_new_medium_produces_warning() {
        let findings = vec![finding_new("Medium")];
        let result = PatchworkCheckResult::from_policy(&default_policy(), &findings);
        assert_eq!(result.state, "warning");
        assert!(result.description.contains("Medium: 1"));
    }

    #[test]
    fn test_new_low_produces_warning() {
        let findings = vec![finding_new("Low")];
        let result = PatchworkCheckResult::from_policy(&default_policy(), &findings);
        assert_eq!(result.state, "warning");
        assert!(result.description.contains("Low: 1"));
    }

    #[test]
    fn test_only_preexisting_produces_success() {
        let findings = vec![finding_preexisting("Critical"), finding_preexisting("High")];
        let result = PatchworkCheckResult::from_policy(&default_policy(), &findings);
        assert_eq!(result.state, "success");
        assert!(result.description.contains("pre-existing"));
    }

    #[test]
    fn test_mixed_new_and_preexisting() {
        let findings = vec![
            finding_new("Critical"),
            finding_preexisting("High"),
            finding_preexisting("High"),
        ];
        let result = PatchworkCheckResult::from_policy(&default_policy(), &findings);
        assert_eq!(result.state, "fail");
        assert!(result.description.contains("Critical: 1"));
        assert!(result.description.contains("High: 2 pre-existing"));
    }

    #[test]
    fn test_mixed_new_and_preexisting_same_severity() {
        let findings = vec![
            finding_new("High"),
            finding_new("High"),
            finding_new("High"),
            finding_preexisting("High"),
        ];
        let result = PatchworkCheckResult::from_policy(&default_policy(), &findings);
        assert_eq!(result.state, "fail");
        assert!(result.description.contains("High: 3 (1 pre-existing)"));
    }

    #[test]
    fn test_zero_counts_dropped_from_description() {
        let findings = vec![finding_new("Critical")];
        let result = PatchworkCheckResult::from_policy(&default_policy(), &findings);
        assert!(!result.description.contains("High"));
        assert!(!result.description.contains("Medium"));
        assert!(!result.description.contains("Low"));
    }

    #[test]
    fn test_min_severity_filters_both_new_and_preexisting() {
        let policy = PatchworkPolicy {
            min_severity: Some("Medium".to_string()),
            ..Default::default()
        };
        let findings = vec![
            finding_new("Low"),
            finding_preexisting("Low"),
            finding_new("Medium"),
            finding_preexisting("High"),
        ];
        let result = PatchworkCheckResult::from_policy(&policy, &findings);
        assert_eq!(result.state, "warning");
        assert!(result.description.contains("Medium: 1"));
        assert!(result.description.contains("High: 1 pre-existing"));
        assert!(!result.description.contains("Low"));
    }

    #[test]
    fn test_min_severity_filters_all_to_success() {
        let policy = PatchworkPolicy {
            min_severity: Some("Critical".to_string()),
            ..Default::default()
        };
        let findings = vec![
            finding_new("Low"),
            finding_new("Medium"),
            finding_new("High"),
        ];
        let result = PatchworkCheckResult::from_policy(&policy, &findings);
        assert_eq!(result.state, "success");
        assert_eq!(result.description, "Sashiko AI review found no regressions");
    }

    #[test]
    fn test_custom_fail_severity_critical() {
        let policy = PatchworkPolicy {
            fail_severity: "Critical".to_string(),
            ..Default::default()
        };
        // High is below Critical threshold, so warning not fail
        let findings = vec![finding_new("High")];
        let result = PatchworkCheckResult::from_policy(&policy, &findings);
        assert_eq!(result.state, "warning");
    }

    #[test]
    fn test_custom_fail_severity_low() {
        let policy = PatchworkPolicy {
            fail_severity: "Low".to_string(),
            ..Default::default()
        };
        // Any new finding triggers fail
        let findings = vec![finding_new("Low")];
        let result = PatchworkCheckResult::from_policy(&policy, &findings);
        assert_eq!(result.state, "fail");
    }

    #[test]
    fn test_missing_preexisting_treated_as_new() {
        // findings without preexisting field default to new
        let findings = vec![serde_json::json!({"severity": "High", "problem": "test"})];
        let result = PatchworkCheckResult::from_policy(&default_policy(), &findings);
        assert_eq!(result.state, "fail");
        assert!(result.description.contains("High: 1"));
        assert!(!result.description.contains("pre-existing"));
    }

    #[test]
    fn test_null_preexisting_treated_as_new() {
        let findings =
            vec![serde_json::json!({"severity": "High", "problem": "test", "preexisting": null})];
        let result = PatchworkCheckResult::from_policy(&default_policy(), &findings);
        assert_eq!(result.state, "fail");
    }

    #[test]
    fn test_description_dot_separator() {
        let findings = vec![finding_new("Critical"), finding_new("Medium")];
        let result = PatchworkCheckResult::from_policy(&default_policy(), &findings);
        assert!(result.description.contains("\u{00b7}")); // middle dot
    }

    // -- compose_patchwork_email tests --

    #[test]
    fn test_compose_patchwork_email_warning() {
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
        assert!(body.contains("msgid: <12345@kernel.org>"));
        assert!(body.contains("status: warning"));
        assert!(body.contains("description: Sashiko AI review found 2 potential issue(s)"));
        assert!(body.contains("target_url: https://sashiko.dev/#/patchset/abc?part=1"));
        assert!(body.contains("context: sashiko"));
    }

    #[test]
    fn test_compose_patchwork_email_success() {
        let (subject, body) = compose_patchwork_email(
            "<99@kernel.org>",
            "success",
            "Sashiko AI review found no regressions",
            "https://sashiko.dev/#/patchset/xyz?part=0",
            "[PATCH v2] improve error handling",
        );

        assert_eq!(
            subject,
            "[sashiko-check] success - [PATCH v2] improve error handling"
        );
        assert!(body.contains("status: success"));
    }

    #[test]
    fn test_compose_patchwork_email_line_format() {
        let (_, body) = compose_patchwork_email(
            "<id@host>",
            "warning",
            "desc",
            "https://example.com",
            "subj",
        );

        // Each key-value pair must be on its own line for downstream parsing
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 5);
        assert!(lines[0].starts_with("msgid: "));
        assert!(lines[1].starts_with("status: "));
        assert!(lines[2].starts_with("description: "));
        assert!(lines[3].starts_with("target_url: "));
        assert!(lines[4].starts_with("context: "));
    }
}
