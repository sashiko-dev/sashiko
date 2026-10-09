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
        }
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
        }
        Ok(())
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

    #[test]
    fn test_patchwork_email_field() {
        let toml_content = r#"
            [defaults]

            ["linux-media@vger.kernel.org"]

            ["linux-media@vger.kernel.org".patchwork]
            enabled = true
            email = "pw-bot@lists.example.org"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let media = config
            .lists
            .get("linux-media@vger.kernel.org")
            .expect("media missing");
        assert!(media.patchwork.enabled);
        assert_eq!(
            media.patchwork.email.as_deref(),
            Some("pw-bot@lists.example.org")
        );
        assert!(media.patchwork.api_url.is_none());
        assert!(media.patchwork.token.is_none());
    }

    #[test]
    fn test_patchwork_email_field_absent() {
        let toml_content = r#"
            [defaults]

            ["netdev@vger.kernel.org".patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.3"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config
            .lists
            .get("netdev@vger.kernel.org")
            .expect("net missing");
        assert!(net.patchwork.enabled);
        assert!(net.patchwork.email.is_none());
        assert!(net.patchwork.api_url.is_some());
    }

    #[test]
    fn test_url_normalization_trailing_slash() {
        let toml_content = r#"
            [defaults]

            ["netdev@vger.kernel.org".patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.3/"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config
            .lists
            .get("netdev@vger.kernel.org")
            .expect("net missing");
        assert_eq!(
            net.patchwork.api_url.as_deref(),
            Some("https://patchwork.kernel.org/api/1.3")
        );
    }

    #[test]
    fn test_url_normalization_multiple_trailing_slashes() {
        let toml_content = r#"
            [defaults]

            ["netdev@vger.kernel.org".patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.3///"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config
            .lists
            .get("netdev@vger.kernel.org")
            .expect("net missing");
        assert_eq!(
            net.patchwork.api_url.as_deref(),
            Some("https://patchwork.kernel.org/api/1.3")
        );
    }

    #[test]
    fn test_url_normalization_invalid_scheme() {
        let toml_content = r#"
            [defaults]

            ["netdev@vger.kernel.org".patchwork]
            enabled = true
            api_url = "ftp://patchwork.kernel.org/api/1.3"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config
            .lists
            .get("netdev@vger.kernel.org")
            .expect("net missing");
        // Invalid scheme should be cleared
        assert!(net.patchwork.api_url.is_none());
    }

    #[test]
    fn test_url_normalization_valid_http() {
        let toml_content = r#"
            [defaults]

            ["netdev@vger.kernel.org".patchwork]
            enabled = true
            api_url = "http://localhost:8000/api/1.3"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config
            .lists
            .get("netdev@vger.kernel.org")
            .expect("net missing");
        assert_eq!(
            net.patchwork.api_url.as_deref(),
            Some("http://localhost:8000/api/1.3")
        );
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
    fn test_token_override_fills_gap() {
        let mut config =
            make_config_with_patchwork(true, Some("https://patchwork.kernel.org/api/1.3"), None);
        config.apply_token_override(Some("injected-token"));

        let net = config.lists.get("netdev@vger.kernel.org").unwrap();
        assert_eq!(net.patchwork.token.as_deref(), Some("injected-token"));
    }

    #[test]
    fn test_token_override_no_overwrite_explicit() {
        let mut config = make_config_with_patchwork(
            true,
            Some("https://patchwork.kernel.org/api/1.3"),
            Some("toml-explicit-token"),
        );
        config.apply_token_override(Some("injected-token"));

        let net = config.lists.get("netdev@vger.kernel.org").unwrap();
        assert_eq!(
            net.patchwork.token.as_deref(),
            Some("toml-explicit-token"),
            "explicit TOML token should not be overwritten"
        );
    }

    #[test]
    fn test_token_override_skips_disabled() {
        let mut config =
            make_config_with_patchwork(false, Some("https://patchwork.kernel.org/api/1.3"), None);
        config.apply_token_override(Some("injected-token"));

        let net = config.lists.get("netdev@vger.kernel.org").unwrap();
        assert!(
            net.patchwork.token.is_none(),
            "disabled patchwork should not get override token"
        );
    }

    #[test]
    fn test_token_override_skips_no_api_url() {
        let mut config = make_config_with_patchwork(true, None, None);
        config.apply_token_override(Some("injected-token"));

        let net = config.lists.get("netdev@vger.kernel.org").unwrap();
        assert!(
            net.patchwork.token.is_none(),
            "patchwork without api_url should not get override token"
        );
    }

    #[test]
    fn test_token_override_none_is_noop() {
        let mut config =
            make_config_with_patchwork(true, Some("https://patchwork.kernel.org/api/1.3"), None);
        config.apply_token_override(None);

        let net = config.lists.get("netdev@vger.kernel.org").unwrap();
        assert!(net.patchwork.token.is_none());
    }

    #[test]
    fn test_patchwork_normalize_direct() {
        let mut policy = PatchworkPolicy {
            enabled: true,
            api_url: Some("https://example.org/api/1.3/".to_string()),
            ..Default::default()
        };
        policy.normalize();
        assert_eq!(
            policy.api_url.as_deref(),
            Some("https://example.org/api/1.3")
        );

        let mut bad = PatchworkPolicy {
            enabled: true,
            api_url: Some("ftp://example.org".to_string()),
            ..Default::default()
        };
        bad.normalize();
        assert!(bad.api_url.is_none());
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
    fn test_normalize_http_non_localhost_still_accepted() {
        // http:// for non-localhost is accepted (with a warning) not rejected
        let mut policy = PatchworkPolicy {
            enabled: true,
            api_url: Some("http://patchwork.example.org/api/1.3".to_string()),
            ..Default::default()
        };
        policy.normalize();
        assert_eq!(
            policy.api_url.as_deref(),
            Some("http://patchwork.example.org/api/1.3"),
            "http:// non-localhost should be accepted with warning, not rejected"
        );
    }

    #[test]
    fn test_min_severity_deserialization() {
        let toml_content = r#"
            [defaults]

            ["netdev@vger.kernel.org".patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.3"
            min_severity = "Medium"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config
            .lists
            .get("netdev@vger.kernel.org")
            .expect("net missing");
        assert_eq!(net.patchwork.min_severity.as_deref(), Some("Medium"));
    }

    #[test]
    fn test_min_severity_absent_is_none() {
        let toml_content = r#"
            [defaults]

            ["netdev@vger.kernel.org".patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.3"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config
            .lists
            .get("netdev@vger.kernel.org")
            .expect("net missing");
        assert!(net.patchwork.min_severity.is_none());
    }

    #[test]
    fn test_fail_severity_default_is_high() {
        let toml_content = r#"
            [defaults]

            ["netdev@vger.kernel.org".patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.3"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config
            .lists
            .get("netdev@vger.kernel.org")
            .expect("net missing");
        assert_eq!(net.patchwork.fail_severity, "High");
    }

    #[test]
    fn test_fail_severity_custom() {
        let toml_content = r#"
            [defaults]

            ["netdev@vger.kernel.org".patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.3"
            fail_severity = "Critical"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config
            .lists
            .get("netdev@vger.kernel.org")
            .expect("net missing");
        assert_eq!(net.patchwork.fail_severity, "Critical");
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

        let sashiko_config = EmailPolicyConfig::load("projects/sashiko/mailing_lists.toml")
            .expect("Production projects/sashiko/mailing_lists.toml failed to parse");
        assert!(sashiko_config.tracked_nntp_groups().is_empty());
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
}
