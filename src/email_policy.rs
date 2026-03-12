use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Deserialize, Debug, Clone)]
pub struct EmailPolicyConfig {
    #[serde(default)]
    pub defaults: SubsystemPolicy,
    #[serde(default)]
    pub subsystems: HashMap<String, SubsystemPolicy>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct SubsystemPolicy {
    #[serde(default)]
    pub lists: Vec<String>,
    #[serde(default)]
    pub allow_public_reply: bool,
    #[serde(default = "default_true")]
    pub send_to_author: bool,
    #[serde(default)]
    pub send_to_maintainers: bool,
    #[serde(default)]
    pub mute_all: bool,
    #[serde(default)]
    pub custom_recipients: Vec<String>,
}

impl Default for SubsystemPolicy {
    fn default() -> Self {
        Self {
            lists: Vec::new(),
            allow_public_reply: false,
            send_to_author: true,
            send_to_maintainers: false,
            mute_all: false,
            custom_recipients: Vec::new(),
        }
    }
}

fn default_true() -> bool {
    true
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
        let config: Self = toml::from_str(&content)?;
        Ok(config)
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
            allow_public_reply = false
            send_to_author = true
            send_to_maintainers = true
            mute_all = false
            custom_recipients = []

            [subsystems.mm]
            lists = ["linux-mm@kvack.org", "linux-mm@vger.kernel.org"]
            allow_public_reply = true
            send_to_author = true
            send_to_maintainers = true

            [subsystems.bpf]
            lists = ["bpf@vger.kernel.org"]
            allow_public_reply = false
            send_to_author = true
            send_to_maintainers = false

            [subsystems.net]
            lists = ["netdev@vger.kernel.org"]
            mute_all = true
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");

        assert_eq!(config.defaults.allow_public_reply, false);
        assert_eq!(config.defaults.send_to_author, true);

        let mm_policy = config.subsystems.get("mm").expect("mm subsystem missing");
        assert_eq!(
            mm_policy.lists,
            vec!["linux-mm@kvack.org", "linux-mm@vger.kernel.org"]
        );
        assert_eq!(mm_policy.allow_public_reply, true);

        let bpf_policy = config.subsystems.get("bpf").expect("bpf subsystem missing");
        assert_eq!(bpf_policy.allow_public_reply, false);
        assert_eq!(bpf_policy.send_to_author, true);
        assert_eq!(bpf_policy.send_to_maintainers, false);

        let net_policy = config.subsystems.get("net").expect("net subsystem missing");
        assert_eq!(net_policy.mute_all, true);
    }

    #[test]
    fn test_load_missing_policy() {
        let config = EmailPolicyConfig::load("non_existent_file.toml")
            .expect("Failed to load default policy");
        assert_eq!(config.defaults.send_to_author, true);
        assert!(config.subsystems.is_empty());
    }
}
