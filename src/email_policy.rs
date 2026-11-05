use crate::db::SeverityFloor;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct PatchworkPolicy {
    #[serde(default)]
    pub enabled: bool,
    pub api_url: Option<String>,
    pub token: Option<String>,
    pub email: Option<String>,
    /// Minimum finding severity to include in patchwork checks.
    /// Findings below this threshold are excluded from the check
    /// count and description. Accepts: "Low", "Medium", "High",
    /// "Critical" (case-insensitive). Default: None (all findings).
    pub min_severity: Option<String>,
    /// Minimum severity of NEW findings that triggers the "fail"
    /// check state instead of "warning". Accepts: "Low", "Medium",
    /// "High", "Critical" (case-insensitive). Default: "High".
    /// New findings at or above this threshold produce "fail";
    /// below it produce "warning". Pre-existing findings never
    /// affect the check state.
    #[serde(default = "default_fail_severity")]
    pub fail_severity: String,
}

fn default_fail_severity() -> String {
    "High".to_string()
}

impl Default for PatchworkPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            api_url: None,
            token: None,
            email: None,
            min_severity: None,
            fail_severity: default_fail_severity(),
        }
    }
}

impl PatchworkPolicy {
    /// Normalize api_url: strip trailing slashes, validate scheme.
    /// Invalid schemes produce a warning and clear api_url.
    /// Non-localhost http:// URLs produce a security warning since
    /// the API token would be sent in plaintext.
    pub fn normalize(&mut self) {
        if let Some(url) = &self.api_url {
            let trimmed = url.trim_end_matches('/');
            if !trimmed.starts_with("https://") && !trimmed.starts_with("http://") {
                tracing::warn!("Patchwork api_url has invalid scheme: {}", url);
                self.api_url = None;
            } else {
                if trimmed.starts_with("http://") && !Self::is_localhost_url(trimmed) {
                    tracing::warn!(
                        "Patchwork api_url uses http:// for a non-localhost host. \
                         The API token will be sent in plaintext: {}",
                        trimmed
                    );
                }
                self.api_url = Some(trimmed.to_string());
            }
        }
    }

    /// Check whether a URL points to a localhost address.
    fn is_localhost_url(url: &str) -> bool {
        let after_scheme = url
            .strip_prefix("http://")
            .or_else(|| url.strip_prefix("https://"))
            .unwrap_or(url);
        // Extract host+port before first path separator
        let host_port = after_scheme.split('/').next().unwrap_or("");
        // Handle IPv6 bracket notation: [::1]:8000
        if host_port.starts_with('[') {
            host_port.starts_with("[::1]")
        } else {
            let host = host_port.split(':').next().unwrap_or("");
            matches!(host, "localhost" | "127.0.0.1")
        }
    }
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ReplyTarget {
    Author,
    List,
    Recipients,
}

#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "lowercase")]
pub enum PositiveReviewPolicy {
    #[default]
    None,
    Author,
    All,
}

fn default_true() -> bool {
    true
}

/// Whether reviews mention pre-existing bugs that relate to the patch, and
/// how severe such a bug must be. See
/// designs/DESIGN_RELATED_PREEXISTING_BUGS.md.
///
/// Configured via `report_preexisting_bugs = true | false | "none" | "low" |
/// "medium" | "high" | "critical"`, or via the legacy `related_preexisting`
/// table (`{ enabled = ..., min_severity = ... }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelatedPreexistingPolicy {
    pub enabled: bool,
    pub min_severity: SeverityFloor,
}

fn default_related_preexisting_min_severity() -> SeverityFloor {
    SeverityFloor::Medium
}

impl Default for RelatedPreexistingPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            min_severity: default_related_preexisting_min_severity(),
        }
    }
}

impl<'de> Deserialize<'de> for RelatedPreexistingPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let over = RelatedPreexistingOverride::deserialize(deserializer)?;
        let defaults = Self::default();
        Ok(Self {
            enabled: over.enabled.unwrap_or(defaults.enabled),
            min_severity: over.min_severity.unwrap_or(defaults.min_severity),
        })
    }
}

/// Per-list override of [`RelatedPreexistingPolicy`]; unset fields fall back
/// to `defaults.report_preexisting_bugs`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelatedPreexistingOverride {
    pub enabled: Option<bool>,
    pub min_severity: Option<SeverityFloor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRelatedPreexistingTable {
    enabled: Option<bool>,
    min_severity: Option<SeverityFloor>,
}

impl<'de> Deserialize<'de> for RelatedPreexistingOverride {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PreexistingVisitor;

        impl<'de> serde::de::Visitor<'de> for PreexistingVisitor {
            type Value = RelatedPreexistingOverride;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a boolean, \"none\", a severity floor (\"low\", \"medium\", \"high\", \"critical\"), or a table",
                )
            }

            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(RelatedPreexistingOverride {
                    enabled: Some(v),
                    min_severity: None,
                })
            }

            fn visit_str<E: serde::de::Error>(self, raw: &str) -> Result<Self::Value, E> {
                if raw.trim().eq_ignore_ascii_case("none") {
                    return Ok(RelatedPreexistingOverride {
                        enabled: Some(false),
                        min_severity: None,
                    });
                }
                let floor = raw.parse::<SeverityFloor>().map_err(|_| {
                    E::custom(format!(
                        "invalid severity {raw:?}: expected true, false, \"none\", \"low\", \"medium\", \"high\", or \"critical\""
                    ))
                })?;
                Ok(RelatedPreexistingOverride {
                    enabled: Some(true),
                    min_severity: Some(floor),
                })
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                map: M,
            ) -> Result<Self::Value, M::Error> {
                let raw = RawRelatedPreexistingTable::deserialize(
                    serde::de::value::MapAccessDeserializer::new(map),
                )?;
                Ok(RelatedPreexistingOverride {
                    enabled: raw.enabled,
                    min_severity: raw.min_severity,
                })
            }
        }

        deserializer.deserialize_any(PreexistingVisitor)
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct DefaultListPolicy {
    #[serde(default = "default_true")]
    pub track: bool,
    #[serde(default)]
    pub reply_to: Vec<ReplyTarget>,
    #[serde(default)]
    pub positive_review: PositiveReviewPolicy,
    #[serde(default)]
    pub mute_all: bool,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub ignored_emails: Vec<String>,
    #[serde(default)]
    pub patchwork: PatchworkPolicy,
    #[serde(default)]
    pub embargo_hours: u32,
    #[serde(default, alias = "related_preexisting")]
    pub report_preexisting_bugs: RelatedPreexistingPolicy,
}

impl Default for DefaultListPolicy {
    fn default() -> Self {
        Self {
            track: true,
            reply_to: Vec::new(),
            positive_review: PositiveReviewPolicy::None,
            mute_all: false,
            cc: Vec::new(),
            ignored_emails: Vec::new(),
            patchwork: PatchworkPolicy::default(),
            embargo_hours: 0,
            report_preexisting_bugs: RelatedPreexistingPolicy::default(),
        }
    }
}

/// Per-mailing-list filter applied during NNTP ingestion so shared lists
/// carrying patches for multiple trees only create patchsets for submissions
/// belonging to the configured project.
#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IngestFilterPolicy {
    #[serde(default)]
    pub allowed_subject_prefixes: Vec<String>,
    #[serde(default)]
    pub ignored_subject_prefixes: Vec<String>,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    #[serde(default)]
    pub ignored_paths: Vec<String>,
}

fn path_matches_pattern(path: &str, pattern: &str) -> bool {
    let norm_path = path.trim_start_matches("./").trim_start_matches('/');
    let norm_pat = pattern
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/');
    if norm_pat.is_empty() {
        return false;
    }
    if norm_pat.ends_with('/') {
        norm_path.starts_with(norm_pat)
    } else if let Some(stem) = norm_pat.strip_suffix('*') {
        norm_path.starts_with(stem)
    } else {
        norm_path == norm_pat
    }
}

impl IngestFilterPolicy {
    pub fn is_empty(&self) -> bool {
        self.allowed_subject_prefixes.is_empty()
            && self.ignored_subject_prefixes.is_empty()
            && self.allowed_paths.is_empty()
            && self.ignored_paths.is_empty()
    }

    pub fn validate(&self, list_key: &str) -> Result<(), String> {
        for (field, items) in [
            ("allowed_subject_prefixes", &self.allowed_subject_prefixes),
            ("ignored_subject_prefixes", &self.ignored_subject_prefixes),
            ("allowed_paths", &self.allowed_paths),
            ("ignored_paths", &self.ignored_paths),
        ] {
            if items.iter().any(|s| s.trim().is_empty()) {
                return Err(format!(
                    "Mailing list {:?} has an empty entry in `ingest_filter.{}`",
                    list_key, field
                ));
            }
        }
        Ok(())
    }

    /// Evaluates whether an NNTP submission with `subject_prefixes` and
    /// `touched_paths` (from a unified diff or cover-letter diffstat) belongs
    /// to this project.
    pub fn allows_submission(&self, subject_prefixes: &[String], touched_paths: &[String]) -> bool {
        if self.is_empty() {
            return true;
        }
        if subject_prefixes.iter().any(|prefix| {
            self.ignored_subject_prefixes
                .iter()
                .any(|ignored| prefix.eq_ignore_ascii_case(ignored.trim()))
        }) {
            return false;
        }
        if touched_paths.iter().any(|path| {
            self.ignored_paths
                .iter()
                .any(|pat| path_matches_pattern(path, pat))
        }) {
            return false;
        }
        if subject_prefixes.iter().any(|prefix| {
            self.allowed_subject_prefixes
                .iter()
                .any(|allowed| prefix.eq_ignore_ascii_case(allowed.trim()))
        }) {
            return true;
        }
        if !self.allowed_paths.is_empty() {
            return !touched_paths.is_empty()
                && touched_paths.iter().any(|path| {
                    self.allowed_paths
                        .iter()
                        .any(|pat| path_matches_pattern(path, pat))
                });
        }
        self.allowed_subject_prefixes.is_empty()
    }
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct MailingListPolicy {
    pub track: Option<bool>,
    pub nntp_group: Option<String>,
    pub reply_to: Option<Vec<ReplyTarget>>,
    pub positive_review: Option<PositiveReviewPolicy>,
    pub mute_all: Option<bool>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub ignored_emails: Vec<String>,
    #[serde(default)]
    pub subject_prefixes: Vec<String>,
    #[serde(default)]
    pub patchwork: PatchworkPolicy,
    pub embargo_hours: Option<u32>,
    #[serde(default, alias = "related_preexisting")]
    pub report_preexisting_bugs: RelatedPreexistingOverride,
    #[serde(default)]
    pub ingest_filter: IngestFilterPolicy,
}

pub type SubsystemPolicy = MailingListPolicy;

#[derive(Deserialize, Debug, Clone, Default)]
pub struct EmailPolicyConfig {
    #[serde(default)]
    pub defaults: DefaultListPolicy,
    #[serde(flatten)]
    pub lists: HashMap<String, MailingListPolicy>,
}

pub type MailingListsConfig = EmailPolicyConfig;

/// Extracts the normalized lowercase email address from either a formatted
/// `"Display Name <user@domain>"` string or a bare `"user@domain"` string.
pub fn extract_bare_email(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(start) = trimmed.find('<')
        && let Some(end) = trimmed[start + 1..].find('>')
    {
        return trimmed[start + 1..start + 1 + end]
            .trim()
            .to_ascii_lowercase();
    }
    trimmed.to_ascii_lowercase()
}

/// Derives the standard lore.kernel.org NNTP group name for a mailing list
/// email address by reversing its domain components and appending the local
/// part (`local@d1.d2...dn` -> `dn...d2.d1.local`).
pub fn derive_nntp_group(email: &str) -> String {
    let bare = extract_bare_email(email);
    match bare.as_str() {
        "devicetree@vger.kernel.org" => return "org.kernel.vger.linux-devicetree".to_string(),
        "b.a.t.m.a.n@lists.open-mesh.org" => return "org.open-mesh.lists.batman".to_string(),
        "intel-wired-lan@lists.osuosl.org" => return "org.osuosl.intel-wired-lan".to_string(),
        _ => {}
    }
    if let Some((local, domain)) = bare.split_once('@') {
        let mut parts: Vec<&str> = domain.split('.').collect();
        parts.reverse();
        parts.push(local);
        parts.join(".")
    } else {
        bare
    }
}

impl EmailPolicyConfig {
    /// Parses and validates a mailing list & email policy configuration from a
    /// TOML string, applying environment overrides and URL normalization.
    pub fn parse_content(content: &str) -> anyhow::Result<Self> {
        let mut config: Self = toml::from_str(content)?;
        config.validate().map_err(anyhow::Error::msg)?;

        let env_token = std::env::var("SASHIKO_PATCHWORK_TOKEN").ok();
        config.apply_token_override(env_token.as_deref());
        config.normalize_patchwork_urls();

        Ok(config)
    }

    /// Loads the mailing list & email policy configuration from a TOML file.
    /// Returns a default configuration if the file does not exist.
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        match fs::read_to_string(path) {
            Ok(content) => Self::parse_content(&content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Asynchronously loads the mailing list & email policy configuration from
    /// a TOML file without blocking Tokio worker threads.
    /// Returns a default configuration if the file does not exist.
    pub async fn load_async(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        match tokio::fs::read_to_string(path).await {
            Ok(content) => Self::parse_content(&content),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Validates that every top-level table key (other than `[defaults]`) is a
    /// well-formed email address and that no list repeats its own address in
    /// its static `cc` list.
    pub fn validate(&self) -> Result<(), String> {
        for (key, policy) in &self.lists {
            let trimmed = key.trim();
            let Some((local, domain)) = trimmed.split_once('@') else {
                return Err(format!(
                    "Invalid mailing list key {:?}: expected an email address \
                     (e.g. [\"linux-kbuild@vger.kernel.org\"])",
                    key
                ));
            };
            if local.is_empty()
                || domain.is_empty()
                || !domain.contains('.')
                || trimmed.contains(char::is_whitespace)
            {
                return Err(format!(
                    "Invalid mailing list email address {:?}: must be of the form \"list@domain.tld\"",
                    key
                ));
            }

            let list_bare = extract_bare_email(trimmed);
            for cc_entry in &policy.cc {
                if extract_bare_email(cc_entry) == list_bare {
                    return Err(format!(
                        "Mailing list {:?} repeats its own address in `cc` ({:?}); \
                         include \"list\" in `reply_to` instead",
                        key, cc_entry
                    ));
                }
            }
            policy.ingest_filter.validate(key)?;
        }
        Ok(())
    }

    /// Returns the NNTP ingestion filter configured for `nntp_group`, when the
    /// group maps to a list with a non-empty `ingest_filter`.
    pub fn ingest_filter_for_nntp_group(&self, nntp_group: &str) -> Option<&IngestFilterPolicy> {
        self.lists.iter().find_map(|(email, policy)| {
            if policy.ingest_filter.is_empty() {
                return None;
            }
            let effective_group = policy
                .nntp_group
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(|| derive_nntp_group(email));
            (effective_group == nntp_group).then_some(&policy.ingest_filter)
        })
    }

    /// Returns `(short_name, nntp_group)` pairs for all lists in this config
    /// where `track` is enabled (`policy.track.unwrap_or(self.defaults.track)`).
    pub fn tracked_nntp_groups(&self) -> Vec<(String, String)> {
        let mut groups: Vec<(String, String)> = self
            .lists
            .iter()
            .filter(|(_, policy)| policy.track.unwrap_or(self.defaults.track))
            .map(|(email, policy)| {
                let group = policy
                    .nntp_group
                    .clone()
                    .unwrap_or_else(|| derive_nntp_group(email));
                let short_name = group.split('.').next_back().unwrap_or(email).to_string();
                (short_name, group)
            })
            .collect();
        groups.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        groups
    }

    /// Resolves the related pre-existing bug policy for a patch addressed to
    /// `addresses` (its To and Cc entries, bare or display-name form).
    ///
    /// When no configured list matches, the defaults apply. Otherwise each
    /// matched list contributes its override (falling back to the defaults
    /// field by field): any list that disables the feature disables it, and
    /// the strictest severity floor wins.
    pub fn related_preexisting_for<'a>(
        &self,
        addresses: impl IntoIterator<Item = &'a str>,
    ) -> RelatedPreexistingPolicy {
        let defaults = self.defaults.report_preexisting_bugs;
        let addresses: Vec<String> = addresses.into_iter().map(extract_bare_email).collect();
        let mut resolved: Option<RelatedPreexistingPolicy> = None;
        for (list_key, list_policy) in &self.lists {
            let list_bare = extract_bare_email(list_key);
            if !addresses.contains(&list_bare) {
                continue;
            }
            let over = list_policy.report_preexisting_bugs;
            let enabled = over.enabled.unwrap_or(defaults.enabled);
            let min_severity = over.min_severity.unwrap_or(defaults.min_severity);
            resolved = Some(match resolved {
                None => RelatedPreexistingPolicy {
                    enabled,
                    min_severity,
                },
                Some(acc) => RelatedPreexistingPolicy {
                    enabled: acc.enabled && enabled,
                    min_severity: acc.min_severity.max(min_severity),
                },
            });
        }
        resolved.unwrap_or(defaults)
    }

    /// Apply a fallback patchwork token to any enabled patchwork policy
    /// that has an api_url but no explicit token. Explicit TOML tokens
    /// are never overwritten.
    pub fn apply_token_override(&mut self, token: Option<&str>) {
        let Some(token) = token else { return };

        if self.defaults.patchwork.enabled
            && self.defaults.patchwork.api_url.is_some()
            && self.defaults.patchwork.token.is_none()
        {
            self.defaults.patchwork.token = Some(token.to_string());
        }
        for sub in self.lists.values_mut() {
            if sub.patchwork.enabled
                && sub.patchwork.api_url.is_some()
                && sub.patchwork.token.is_none()
            {
                sub.patchwork.token = Some(token.to_string());
            }
        }
    }

    /// Normalize patchwork URLs across all policies.
    fn normalize_patchwork_urls(&mut self) {
        self.defaults.patchwork.normalize();
        for sub in self.lists.values_mut() {
            sub.patchwork.normalize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_load_policy() {
        let toml_content = r#"
            [defaults]
            track = true
            reply_to = ["author", "recipients"]
            mute_all = false
            cc = []

            ["linux-mm@kvack.org"]
            reply_to = ["author", "list", "recipients"]
            positive_review = "all"

            ["bpf@vger.kernel.org"]
            reply_to = ["author"]
            positive_review = "none"

            ["netdev@vger.kernel.org"]
            mute_all = true

            ["netdev@vger.kernel.org".patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.2"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");

        assert!(config.defaults.track);
        assert_eq!(
            config.defaults.reply_to,
            vec![ReplyTarget::Author, ReplyTarget::Recipients]
        );
        assert!(!config.defaults.patchwork.enabled);
        assert_eq!(config.defaults.positive_review, PositiveReviewPolicy::None);

        let mm_policy = config
            .lists
            .get("linux-mm@kvack.org")
            .expect("mm list missing");
        assert_eq!(
            mm_policy.reply_to.as_deref(),
            Some(
                &[
                    ReplyTarget::Author,
                    ReplyTarget::List,
                    ReplyTarget::Recipients
                ][..]
            )
        );
        assert!(!mm_policy.patchwork.enabled);
        assert_eq!(mm_policy.positive_review, Some(PositiveReviewPolicy::All));

        let bpf_policy = config
            .lists
            .get("bpf@vger.kernel.org")
            .expect("bpf list missing");
        assert_eq!(
            bpf_policy.reply_to.as_deref(),
            Some(&[ReplyTarget::Author][..])
        );
        assert_eq!(bpf_policy.positive_review, Some(PositiveReviewPolicy::None));

        let net_policy = config
            .lists
            .get("netdev@vger.kernel.org")
            .expect("netdev list missing");
        assert_eq!(net_policy.mute_all, Some(true));
        assert!(net_policy.patchwork.enabled);
        assert_eq!(
            net_policy.patchwork.api_url.as_deref(),
            Some("https://patchwork.kernel.org/api/1.2")
        );
    }

    #[test]
    fn test_rejects_unknown_fields_and_invalid_keys() {
        // Unknown field inside a list block
        let bad_field = r#"
            ["bpf@vger.kernel.org"]
            reply_all = true
        "#;
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", bad_field).unwrap();
        assert!(
            EmailPolicyConfig::load(file.path()).is_err(),
            "unknown field reply_all must be rejected"
        );

        // Non-email top-level section key (e.g. old [subsystems.bpf] or typo [default])
        let bad_key = r#"
            [default]
            mute_all = true
        "#;
        let mut file2 = NamedTempFile::new().unwrap();
        write!(file2, "{}", bad_key).unwrap();
        let err = EmailPolicyConfig::load(file2.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("Invalid mailing list key"), "{err}");

        // Self-CC duplication
        let self_cc = r#"
            ["bpf@vger.kernel.org"]
            reply_to = ["author"]
            cc = ["bpf@vger.kernel.org"]
        "#;
        let mut file3 = NamedTempFile::new().unwrap();
        write!(file3, "{}", self_cc).unwrap();
        let err = EmailPolicyConfig::load(file3.path())
            .unwrap_err()
            .to_string();
        assert!(err.contains("repeats its own address in `cc`"), "{err}");
    }

    #[test]
    fn test_related_preexisting_defaults_when_unconfigured() {
        let config = EmailPolicyConfig::parse_content("").unwrap();
        let resolved = config.related_preexisting_for(["Dev <dev@example.org>"]);
        assert_eq!(resolved, RelatedPreexistingPolicy::default());
        assert!(resolved.enabled);
        assert_eq!(resolved.min_severity, SeverityFloor::Medium);
    }

    #[test]
    fn test_related_preexisting_resolution_across_lists() {
        let config = EmailPolicyConfig::parse_content(
            r#"
            [defaults.related_preexisting]
            min_severity = "Medium"

            ["linux-mm@kvack.org".related_preexisting]
            min_severity = "critical"

            ["bpf@vger.kernel.org"]
            reply_to = ["author"]

            ["netdev@vger.kernel.org".related_preexisting]
            enabled = false
            "#,
        )
        .unwrap();

        // No configured list matches: defaults.
        let none = config.related_preexisting_for(["other@example.org"]);
        assert!(none.enabled);
        assert_eq!(none.min_severity, SeverityFloor::Medium);

        // A list without an override inherits the defaults field by field.
        let bpf = config.related_preexisting_for(["bpf@vger.kernel.org"]);
        assert!(bpf.enabled);
        assert_eq!(bpf.min_severity, SeverityFloor::Medium);

        // The strictest floor wins, matched in display-name form.
        let mm_bpf =
            config.related_preexisting_for(["<bpf@vger.kernel.org>", "MM <LINUX-MM@kvack.org>"]);
        assert!(mm_bpf.enabled);
        assert_eq!(mm_bpf.min_severity, SeverityFloor::Critical);

        // Any matched list that disables the feature disables it.
        let net_bpf =
            config.related_preexisting_for(["bpf@vger.kernel.org", "netdev@vger.kernel.org"]);
        assert!(!net_bpf.enabled);
    }

    #[test]
    fn test_report_preexisting_bugs_flat_key() {
        let config = EmailPolicyConfig::parse_content(
            r#"
            [defaults]
            report_preexisting_bugs = "high"

            ["linux-mm@kvack.org"]
            report_preexisting_bugs = false

            ["netdev@vger.kernel.org"]
            report_preexisting_bugs = "none"

            ["bpf@vger.kernel.org"]
            report_preexisting_bugs = "critical"

            ["damon@lists.linux.dev"]
            report_preexisting_bugs = true
            "#,
        )
        .unwrap();

        let other = config.related_preexisting_for(["other@example.org"]);
        assert!(other.enabled);
        assert_eq!(other.min_severity, SeverityFloor::High);

        let mm = config.related_preexisting_for(["linux-mm@kvack.org"]);
        assert!(!mm.enabled);

        let net = config.related_preexisting_for(["netdev@vger.kernel.org"]);
        assert!(!net.enabled);

        // `true` enables reporting while preserving the configured default floor.
        let damon = config.related_preexisting_for(["damon@lists.linux.dev"]);
        assert!(damon.enabled);
        assert_eq!(damon.min_severity, SeverityFloor::High);

        let bpf_damon =
            config.related_preexisting_for(["bpf@vger.kernel.org", "damon@lists.linux.dev"]);
        assert!(bpf_damon.enabled);
        assert_eq!(bpf_damon.min_severity, SeverityFloor::Critical);

        let mm_bpf = config.related_preexisting_for(["linux-mm@kvack.org", "bpf@vger.kernel.org"]);
        assert!(!mm_bpf.enabled);

        // Opt-in list when defaults disable reporting.
        let opt_in = EmailPolicyConfig::parse_content(
            r#"
            [defaults]
            report_preexisting_bugs = false

            ["bpf@vger.kernel.org"]
            report_preexisting_bugs = true
            "#,
        )
        .unwrap();
        assert!(
            !opt_in
                .related_preexisting_for(["other@example.org"])
                .enabled
        );
        let bpf_opt_in = opt_in.related_preexisting_for(["bpf@vger.kernel.org"]);
        assert!(bpf_opt_in.enabled);
        assert_eq!(bpf_opt_in.min_severity, SeverityFloor::Medium);
    }

    #[test]
    fn test_related_preexisting_rejects_invalid_config() {
        let err = EmailPolicyConfig::parse_content(
            r#"
            [defaults.related_preexisting]
            min_severity = "unknown"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("invalid severity"), "{err}");

        let err = EmailPolicyConfig::parse_content(
            r#"
            ["bpf@vger.kernel.org"]
            report_preexisting_bugs = "no"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("invalid severity"), "{err}");

        let err = EmailPolicyConfig::parse_content(
            r#"
            ["bpf@vger.kernel.org"]
            report_preexisting_bugs = 42
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("boolean"), "{err}");

        let err = EmailPolicyConfig::parse_content(
            r#"
            ["bpf@vger.kernel.org"]
            report_preexisting_bugs = false
            ["bpf@vger.kernel.org".related_preexisting]
            enabled = true
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("duplicate field"), "{err}");

        let err = EmailPolicyConfig::parse_content(
            r#"
            ["bpf@vger.kernel.org".related_preexisting]
            enable = false
            "#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn test_derive_nntp_group_and_tracked_groups() {
        assert_eq!(
            derive_nntp_group("linux-kbuild@vger.kernel.org"),
            "org.kernel.vger.linux-kbuild"
        );
        assert_eq!(
            derive_nntp_group("linux-mm@kvack.org"),
            "org.kvack.linux-mm"
        );
        assert_eq!(
            derive_nntp_group("mptcp@lists.linux.dev"),
            "dev.linux.lists.mptcp"
        );

        let toml_content = r#"
            [defaults]
            track = true

            ["bpf@vger.kernel.org"]
            ["devicetree@vger.kernel.org"]
            nntp_group = "org.kernel.vger.linux-devicetree"
            ["untracked@vger.kernel.org"]
            track = false
        "#;
        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();
        let config = EmailPolicyConfig::load(file.path()).unwrap();
        assert_eq!(
            config.tracked_nntp_groups(),
            vec![
                ("bpf".to_string(), "org.kernel.vger.bpf".to_string()),
                (
                    "linux-devicetree".to_string(),
                    "org.kernel.vger.linux-devicetree".to_string()
                ),
            ]
        );
    }

    #[test]
    fn test_load_missing_policy() {
        let config = EmailPolicyConfig::load("non_existent_file.toml")
            .expect("Failed to load default policy");
        assert!(config.defaults.reply_to.is_empty());
        assert!(config.lists.is_empty());
    }

    fn make_config_with_patchwork(
        enabled: bool,
        api_url: Option<&str>,
        token: Option<&str>,
    ) -> EmailPolicyConfig {
        let mut lists = HashMap::new();
        lists.insert(
            "netdev@vger.kernel.org".to_string(),
            MailingListPolicy {
                patchwork: PatchworkPolicy {
                    enabled,
                    api_url: api_url.map(String::from),
                    token: token.map(String::from),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        EmailPolicyConfig {
            defaults: DefaultListPolicy::default(),
            lists,
        }
    }

    #[test]
    fn test_patchwork_policy_deserialization_and_url_normalization() {
        let toml_content = r#"
            [defaults]

            ["linux-media@vger.kernel.org".patchwork]
            enabled = true
            email = "pw-bot@lists.example.org"

            ["netdev@vger.kernel.org".patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.3///"
            min_severity = "Medium"
            fail_severity = "Critical"

            ["local@vger.kernel.org".patchwork]
            enabled = true
            api_url = "http://localhost:8000/api/1.3/"

            ["remote-http@vger.kernel.org".patchwork]
            enabled = true
            api_url = "http://patchwork.example.org/api/1.3"

            ["bad-scheme@vger.kernel.org".patchwork]
            enabled = true
            api_url = "ftp://patchwork.kernel.org/api/1.3"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");

        let media = &config.lists["linux-media@vger.kernel.org"].patchwork;
        assert!(media.enabled);
        assert_eq!(media.email.as_deref(), Some("pw-bot@lists.example.org"));
        assert!(media.api_url.is_none());
        assert!(media.token.is_none());
        assert!(media.min_severity.is_none());
        assert_eq!(media.fail_severity, "High");

        let net = &config.lists["netdev@vger.kernel.org"].patchwork;
        assert!(net.enabled);
        assert!(net.email.is_none());
        assert_eq!(
            net.api_url.as_deref(),
            Some("https://patchwork.kernel.org/api/1.3")
        );
        assert_eq!(net.min_severity.as_deref(), Some("Medium"));
        assert_eq!(net.fail_severity, "Critical");

        let local = &config.lists["local@vger.kernel.org"].patchwork;
        assert_eq!(
            local.api_url.as_deref(),
            Some("http://localhost:8000/api/1.3")
        );

        let remote_http = &config.lists["remote-http@vger.kernel.org"].patchwork;
        assert_eq!(
            remote_http.api_url.as_deref(),
            Some("http://patchwork.example.org/api/1.3")
        );

        let bad_scheme = &config.lists["bad-scheme@vger.kernel.org"].patchwork;
        assert!(bad_scheme.api_url.is_none());
    }

    #[test]
    fn test_apply_token_override() {
        let cases = [
            (
                true,
                Some("https://patchwork.kernel.org/api/1.3"),
                None,
                Some("injected-token"),
                Some("injected-token"),
            ),
            (
                true,
                Some("https://patchwork.kernel.org/api/1.3"),
                Some("toml-explicit-token"),
                Some("injected-token"),
                Some("toml-explicit-token"),
            ),
            (
                false,
                Some("https://patchwork.kernel.org/api/1.3"),
                None,
                Some("injected-token"),
                None,
            ),
            (true, None, None, Some("injected-token"), None),
            (
                true,
                Some("https://patchwork.kernel.org/api/1.3"),
                None,
                None,
                None,
            ),
        ];

        for (enabled, api_url, initial_token, override_token, expected) in cases {
            let mut config = make_config_with_patchwork(enabled, api_url, initial_token);
            config.apply_token_override(override_token);
            let net = config.lists.get("netdev@vger.kernel.org").unwrap();
            assert_eq!(net.patchwork.token.as_deref(), expected);
        }
    }

    #[test]
    fn test_is_localhost_url() {
        assert!(PatchworkPolicy::is_localhost_url(
            "http://localhost:8000/api"
        ));
        assert!(PatchworkPolicy::is_localhost_url(
            "http://127.0.0.1:8000/api"
        ));
        assert!(PatchworkPolicy::is_localhost_url("http://[::1]:8000/api"));
        assert!(PatchworkPolicy::is_localhost_url("http://localhost/api"));
        assert!(!PatchworkPolicy::is_localhost_url(
            "http://patchwork.kernel.org/api"
        ));
        assert!(!PatchworkPolicy::is_localhost_url("http://10.0.0.1/api"));
    }

    #[test]
    fn test_production_policy_is_valid() {
        let linux_config = EmailPolicyConfig::load("projects/linux/mailing_lists.toml")
            .expect("Production projects/linux/mailing_lists.toml failed to parse");
        assert!(!linux_config.lists.is_empty());
        assert!(!linux_config.tracked_nntp_groups().is_empty());
        assert!(linux_config.lists.contains_key("bpf@vger.kernel.org"));
        assert!(
            linux_config
                .lists
                .contains_key("devicetree@vger.kernel.org")
        );
        assert!(crate::email_router::EmailRouter::is_ignored_author(
            &linux_config,
            "\"tip-bot2 for Peter Zijlstra\" <tip-bot2@linutronix.de>"
        ));

        let sashiko_config = EmailPolicyConfig::load("projects/sashiko/mailing_lists.toml")
            .expect("Production projects/sashiko/mailing_lists.toml failed to parse");
        assert!(sashiko_config.tracked_nntp_groups().is_empty());

        let e2fsprogs_config = EmailPolicyConfig::load("projects/e2fsprogs/mailing_lists.toml")
            .expect("projects/e2fsprogs/mailing_lists.toml failed to parse");
        assert_eq!(
            e2fsprogs_config.tracked_nntp_groups(),
            vec![(
                "linux-ext4".to_string(),
                "org.kernel.vger.linux-ext4".to_string(),
            )]
        );
        assert!(
            e2fsprogs_config
                .ingest_filter_for_nntp_group("org.kernel.vger.linux-ext4")
                .is_some()
        );
    }

    /// The documented example parses, and so does every commented-out
    /// section in it once uncommented, so copying any part of it works.
    #[test]
    fn test_example_policy_is_valid() {
        let path = "docs/examples/mailing_lists.example.toml";
        let text = std::fs::read_to_string(path).expect("example policy is readable");
        EmailPolicyConfig::parse_content(&text).expect("example policy parses");

        let uncommented = crate::settings::uncomment_examples(&text);
        let config =
            EmailPolicyConfig::parse_content(&uncommented).expect("uncommented example parses");
        assert!(config.lists.contains_key("bpf@vger.kernel.org"));
        assert!(config.lists["netdev@vger.kernel.org"].patchwork.enabled);
    }

    #[tokio::test]
    async fn test_load_async() {
        let linux_config = EmailPolicyConfig::load_async("projects/linux/mailing_lists.toml")
            .await
            .expect("Async load of projects/linux/mailing_lists.toml failed");
        assert!(linux_config.lists.contains_key("bpf@vger.kernel.org"));

        let missing = EmailPolicyConfig::load_async("non_existent_file.toml")
            .await
            .expect("Async load of missing policy file should return default");
        assert!(missing.lists.is_empty());
    }

    #[test]
    fn test_ingest_filter_allows_and_rejects_shared_list_traffic() {
        let toml_content = r#"
            [defaults]
            track = true

            ["linux-ext4@vger.kernel.org"]
            reply_to = ["author", "list"]
            positive_review = "all"

            ["linux-ext4@vger.kernel.org".ingest_filter]
            allowed_subject_prefixes = ["e2fsprogs"]
            ignored_subject_prefixes = ["fstests", "xfstests", "kernel", "linux"]
            allowed_paths = [
                "lib/ext2fs/",
                "lib/e2p/",
                "lib/Makefile*",
                "e2fsck/",
                "misc/",
                "tests/",
                "configure.ac",
            ]
            ignored_paths = [
                "fs/",
                "include/",
                "tests/ext4/",
                "tests/generic/",
                "common/",
            ]
        "#;

        let config = EmailPolicyConfig::parse_content(toml_content).unwrap();
        let filter = config
            .ingest_filter_for_nntp_group("org.kernel.vger.linux-ext4")
            .expect("filter resolved for linux-ext4 NNTP group");
        let bpf_filter = config.ingest_filter_for_nntp_group("org.kernel.vger.bpf");
        assert!(bpf_filter.is_none());

        let allows = |prefs: &[&str], paths: &[&str]| {
            let pref_vec: Vec<String> = prefs.iter().map(|s| (*s).to_string()).collect();
            let path_vec: Vec<String> = paths.iter().map(|s| (*s).to_string()).collect();
            filter.allows_submission(&pref_vec, &path_vec)
        };

        // e2fsprogs patch touching e2fsck/ and e2fsprogs test dir is accepted.
        assert!(allows(
            &[],
            &["e2fsck/pass1.c", "tests/f_bad_extent/expect.1"],
        ));
        // Glob pattern lib/Makefile* matches e2fsprogs shared-lib makefiles
        // without matching Linux kernel lib/crc32.c.
        assert!(allows(&[], &["lib/Makefile.elf-lib"]));
        assert!(!allows(&[], &["lib/crc32.c"]));

        // Linux kernel ext4 patch is rejected.
        assert!(!allows(&[], &["fs/ext4/mballoc.c"]));

        // xfstests patches (tagged [PATCH fstests] or touching tests/ext4/) are rejected.
        assert!(!allows(&["fstests"], &["tests/f_something"]));
        assert!(!allows(&[], &["tests/ext4/059"]));

        // Cover letter with [PATCH e2fsprogs 0/3] and no diffstat is accepted,
        // whereas an untagged cover letter with no diffstat is rejected.
        assert!(allows(&["e2fsprogs"], &[]));
        assert!(!allows(&[], &[]));

        // Empty filter entry fails validation.
        let invalid = r#"
            ["linux-ext4@vger.kernel.org".ingest_filter]
            allowed_paths = [""]
        "#;
        assert!(EmailPolicyConfig::parse_content(invalid).is_err());
    }
}
