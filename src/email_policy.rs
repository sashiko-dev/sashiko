use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Deserialize, Debug, Clone)]
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

#[derive(Deserialize, Debug, Clone, Default)]
pub struct EmailPolicyConfig {
    #[serde(default)]
    pub defaults: SubsystemPolicy,
    #[serde(default)]
    pub subsystems: HashMap<String, SubsystemPolicy>,
}

#[derive(Deserialize, Debug, Clone, Default)]
pub struct SubsystemPolicy {
    #[serde(default)]
    pub lists: Vec<String>,
    #[serde(default)]
    pub reply_all: bool,
    #[serde(default)]
    pub reply_to_author: bool,
    #[serde(default)]
    pub cc_individuals: bool,
    #[serde(default)]
    pub mute_all: bool,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub ignored_emails: Vec<String>,
    #[serde(default)]
    pub subject_prefixes: Vec<String>,
    #[serde(default)]
    pub patchwork: PatchworkPolicy,
    #[serde(default)]
    pub embargo_hours: Option<u32>,
    #[serde(default)]
    pub send_positive_review: bool,
}

impl EmailPolicyConfig {
    /// Loads the email policy configuration from a TOML file.
    /// Returns a default configuration if the file does not exist.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self {
                defaults: SubsystemPolicy::default(),
                subsystems: HashMap::new(),
            });
        }

        let content = fs::read_to_string(path)?;
        let mut config: Self = toml::from_str(&content)?;

        let env_token = std::env::var("SASHIKO_PATCHWORK_TOKEN").ok();
        config.apply_token_override(env_token.as_deref());
        config.normalize_patchwork_urls();

        Ok(config)
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
        for sub in self.subsystems.values_mut() {
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
        for sub in self.subsystems.values_mut() {
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
            reply_all = false
            reply_to_author = true
            cc_individuals = true
            mute_all = false
            cc = []

            [subsystems.mm]
            lists = ["linux-mm@kvack.org", "linux-mm@vger.kernel.org"]
            reply_all = true
            reply_to_author = true
            cc_individuals = true
            send_positive_review = true

            [subsystems.bpf]
            lists = ["bpf@vger.kernel.org"]
            reply_all = false
            reply_to_author = true
            cc_individuals = false
            send_positive_review = false

            [subsystems.net]
            lists = ["netdev@vger.kernel.org"]
            mute_all = true

            [subsystems.net.patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.2"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");

        assert!(!config.defaults.reply_all);
        assert!(config.defaults.reply_to_author);
        assert!(!config.defaults.patchwork.enabled);
        assert!(!config.defaults.send_positive_review);

        let mm_policy = config.subsystems.get("mm").expect("mm subsystem missing");
        assert_eq!(
            mm_policy.lists,
            vec!["linux-mm@kvack.org", "linux-mm@vger.kernel.org"]
        );
        assert!(mm_policy.reply_all);
        assert!(!mm_policy.patchwork.enabled);
        assert!(mm_policy.send_positive_review);

        let bpf_policy = config.subsystems.get("bpf").expect("bpf subsystem missing");
        assert!(!bpf_policy.reply_all);
        assert!(bpf_policy.reply_to_author);
        assert!(!bpf_policy.cc_individuals);
        assert!(!bpf_policy.send_positive_review);

        let net_policy = config.subsystems.get("net").expect("net subsystem missing");
        assert!(net_policy.mute_all);
        assert!(net_policy.patchwork.enabled);
        assert_eq!(
            net_policy.patchwork.api_url.as_deref(),
            Some("https://patchwork.kernel.org/api/1.2")
        );
    }

    #[test]
    fn test_load_missing_policy() {
        let config = EmailPolicyConfig::load("non_existent_file.toml")
            .expect("Failed to load default policy");
        assert!(!config.defaults.reply_to_author);
        assert!(config.subsystems.is_empty());
    }

    #[test]
    fn test_patchwork_email_field() {
        let toml_content = r#"
            [defaults]

            [subsystems.media]
            lists = ["linux-media@vger.kernel.org"]

            [subsystems.media.patchwork]
            enabled = true
            email = "pw-bot@lists.example.org"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let media = config.subsystems.get("media").expect("media missing");
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

            [subsystems.net.patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.3"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config.subsystems.get("net").expect("net missing");
        assert!(net.patchwork.enabled);
        assert!(net.patchwork.email.is_none());
        assert!(net.patchwork.api_url.is_some());
    }

    #[test]
    fn test_url_normalization_trailing_slash() {
        let toml_content = r#"
            [defaults]

            [subsystems.net.patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.3/"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config.subsystems.get("net").expect("net missing");
        assert_eq!(
            net.patchwork.api_url.as_deref(),
            Some("https://patchwork.kernel.org/api/1.3")
        );
    }

    #[test]
    fn test_url_normalization_multiple_trailing_slashes() {
        let toml_content = r#"
            [defaults]

            [subsystems.net.patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.3///"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config.subsystems.get("net").expect("net missing");
        assert_eq!(
            net.patchwork.api_url.as_deref(),
            Some("https://patchwork.kernel.org/api/1.3")
        );
    }

    #[test]
    fn test_url_normalization_invalid_scheme() {
        let toml_content = r#"
            [defaults]

            [subsystems.net.patchwork]
            enabled = true
            api_url = "ftp://patchwork.kernel.org/api/1.3"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config.subsystems.get("net").expect("net missing");
        // Invalid scheme should be cleared
        assert!(net.patchwork.api_url.is_none());
    }

    #[test]
    fn test_url_normalization_valid_http() {
        let toml_content = r#"
            [defaults]

            [subsystems.net.patchwork]
            enabled = true
            api_url = "http://localhost:8000/api/1.3"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config.subsystems.get("net").expect("net missing");
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
        let mut subsystems = HashMap::new();
        subsystems.insert(
            "net".to_string(),
            SubsystemPolicy {
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
            defaults: SubsystemPolicy::default(),
            subsystems,
        }
    }

    #[test]
    fn test_token_override_fills_gap() {
        let mut config =
            make_config_with_patchwork(true, Some("https://patchwork.kernel.org/api/1.3"), None);
        config.apply_token_override(Some("injected-token"));

        let net = config.subsystems.get("net").unwrap();
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

        let net = config.subsystems.get("net").unwrap();
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

        let net = config.subsystems.get("net").unwrap();
        assert!(
            net.patchwork.token.is_none(),
            "disabled patchwork should not get override token"
        );
    }

    #[test]
    fn test_token_override_skips_no_api_url() {
        let mut config = make_config_with_patchwork(true, None, None);
        config.apply_token_override(Some("injected-token"));

        let net = config.subsystems.get("net").unwrap();
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

        let net = config.subsystems.get("net").unwrap();
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

            [subsystems.net.patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.3"
            min_severity = "Medium"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config.subsystems.get("net").expect("net missing");
        assert_eq!(net.patchwork.min_severity.as_deref(), Some("Medium"));
    }

    #[test]
    fn test_min_severity_absent_is_none() {
        let toml_content = r#"
            [defaults]

            [subsystems.net.patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.3"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config.subsystems.get("net").expect("net missing");
        assert!(net.patchwork.min_severity.is_none());
    }

    #[test]
    fn test_fail_severity_default_is_high() {
        let toml_content = r#"
            [defaults]

            [subsystems.net.patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.3"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config.subsystems.get("net").expect("net missing");
        assert_eq!(net.patchwork.fail_severity, "High");
    }

    #[test]
    fn test_fail_severity_custom() {
        let toml_content = r#"
            [defaults]

            [subsystems.net.patchwork]
            enabled = true
            api_url = "https://patchwork.kernel.org/api/1.3"
            fail_severity = "Critical"
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");
        let net = config.subsystems.get("net").expect("net missing");
        assert_eq!(net.patchwork.fail_severity, "Critical");
    }
}
