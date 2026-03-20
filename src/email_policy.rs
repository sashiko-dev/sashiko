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

#[derive(Deserialize, Debug, Clone, Default)]
pub struct SubsystemPolicy {
    #[serde(default)]
    pub lists: Vec<String>,
    #[serde(default)]
    pub reply_all: bool,
    #[serde(default)]
    pub reply_to_author: bool,
    #[serde(default)]
    pub cc_maintainers: bool,
    #[serde(default)]
    pub mute_all: bool,
    #[serde(default)]
    pub cc: Vec<String>,
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
            reply_all = false
            reply_to_author = true
            cc_maintainers = true
            mute_all = false
            cc = []

            [subsystems.mm]
            lists = ["linux-mm@kvack.org", "linux-mm@vger.kernel.org"]
            reply_all = true
            reply_to_author = true
            cc_maintainers = true

            [subsystems.bpf]
            lists = ["bpf@vger.kernel.org"]
            reply_all = false
            reply_to_author = true
            cc_maintainers = false

            [subsystems.net]
            lists = ["netdev@vger.kernel.org"]
            mute_all = true
        "#;

        let mut file = NamedTempFile::new().unwrap();
        write!(file, "{}", toml_content).unwrap();

        let config = EmailPolicyConfig::load(file.path()).expect("Failed to load policy");

        assert!(!config.defaults.reply_all);
        assert!(config.defaults.reply_to_author);

        let mm_policy = config.subsystems.get("mm").expect("mm subsystem missing");
        assert_eq!(
            mm_policy.lists,
            vec!["linux-mm@kvack.org", "linux-mm@vger.kernel.org"]
        );
        assert!(mm_policy.reply_all);

        let bpf_policy = config.subsystems.get("bpf").expect("bpf subsystem missing");
        assert!(!bpf_policy.reply_all);
        assert!(bpf_policy.reply_to_author);
        assert!(!bpf_policy.cc_maintainers);

        let net_policy = config.subsystems.get("net").expect("net subsystem missing");
        assert!(net_policy.mute_all);
    }

    #[test]
    fn test_load_missing_policy() {
        let config = EmailPolicyConfig::load("non_existent_file.toml")
            .expect("Failed to load default policy");
        assert!(!config.defaults.reply_to_author);
        assert!(config.subsystems.is_empty());
    }
}
