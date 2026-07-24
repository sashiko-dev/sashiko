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

use config::{Config, ConfigError, Environment, File};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct SubsystemMapping {
    pub pattern: String,
    pub name: String,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct SubsystemsSettings {
    #[serde(default)]
    pub mapping: Vec<SubsystemMapping>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[allow(unused)]
pub struct ProjectSettings {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct ForgeSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub disable_nntp: bool,
    pub provider: Option<String>,
    pub webhook_secret: Option<String>,
    pub api_token: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct DatabaseSettings {
    pub url: String,
    pub token: String,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct NntpSettings {
    pub server: String,
    pub port: u16,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct SmtpSettings {
    pub server: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub sender_address: String,
    pub reply_to: Option<String>,
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
}

fn default_dry_run() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct MailingListsSettings {
    #[serde(deserialize_with = "deserialize_string_or_vec")]
    pub track: Vec<String>,
}

fn deserialize_string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct StringOrVec;

    impl<'de> serde::de::Visitor<'de> for StringOrVec {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("string or list of strings")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(value
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect())
        }

        fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
        where
            S: serde::de::SeqAccess<'de>,
        {
            let mut vec = Vec::new();
            while let Some(elem) = seq.next_element()? {
                vec.push(elem);
            }
            Ok(vec)
        }
    }

    deserializer.deserialize_any(StringOrVec)
}

fn default_max_input_tokens() -> usize {
    150_000
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct ClaudeSettings {
    #[serde(default = "default_prompt_caching")]
    pub prompt_caching: bool,
    #[serde(default = "default_claude_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

fn default_claude_max_tokens() -> u32 {
    4096
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct GeminiSettings {
    #[serde(default)]
    pub explicit_prompt_caching: bool,
}

#[cfg(feature = "bedrock")]
#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct BedrockSettings {
    /// AWS region for Bedrock API calls (e.g. "us-east-1").
    /// If omitted, uses the standard AWS SDK default chain.
    pub region: Option<String>,
    #[serde(default = "default_prompt_caching")]
    pub prompt_caching: bool,
    /// Max output tokens per Converse call.
    #[serde(default = "default_bedrock_max_tokens")]
    pub max_tokens: u32,
    /// Thinking mode sent as additional_model_request_fields. Opus 4.7 only accepts "adaptive".
    /// Leave unset to omit (thinking disabled). Valid values: "adaptive".
    #[serde(default)]
    pub thinking: Option<String>,
    /// output_config.effort level. Valid values: "low", "medium", "high", "xhigh", "max".
    /// Leave unset to use the model default. "xhigh" is Opus 4.7-only.
    #[serde(default)]
    pub effort: Option<String>,
}

#[cfg(feature = "bedrock")]
fn default_bedrock_max_tokens() -> u32 {
    8192
}

fn default_prompt_caching() -> bool {
    true
}

#[cfg(feature = "vertex")]
#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct VertexSettings {
    /// GCP project ID. Falls back to ANTHROPIC_VERTEX_PROJECT_ID env var.
    #[serde(default)]
    pub project_id: Option<String>,
    /// GCP region (e.g., "us-east5", "global"). Falls back to CLOUD_ML_REGION env var.
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default = "default_prompt_caching")]
    pub prompt_caching: bool,
    #[serde(default = "default_vertex_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

#[cfg(feature = "vertex")]
fn default_vertex_max_tokens() -> u32 {
    8192
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct OpenAiCompatSettings {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub context_window_size: Option<usize>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct OllamaSettings {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub context_window_size: Option<usize>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub think: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct KiroCliSettings {
    #[serde(default = "default_kiro_cli_binary")]
    pub binary: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default = "default_kiro_cli_context_window")]
    pub context_window_size: usize,
}

fn default_kiro_cli_binary() -> String {
    "kiro-cli".to_string()
}

fn default_kiro_cli_context_window() -> usize {
    200_000
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct ClaudeCliSettings {
    /// Effort level passed to `claude --effort`. Valid values per Claude Code:
    /// "low", "medium", "high", "xhigh", "max". Leave unset for the model default.
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct DevinCliSettings {
    /// Path to a Devin declarative agent config file (JSON or YAML) passed via
    /// `--agent-config`. Use this to disable all tools for a strictly
    /// text-completion backend.
    #[serde(default)]
    pub agent_config: Option<String>,
    /// Path to a Devin config file passed via `--config`. Use this to apply
    /// custom permission rules (e.g. deny-all) for the provider session
    /// without polluting the user's `~/.config/devin/config.json`.
    #[serde(default)]
    pub config: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct AiSettings {
    pub provider: String,
    pub model: String,
    #[serde(default = "default_max_input_tokens")]
    pub max_input_tokens: usize,
    #[serde(default = "default_max_interactions")]
    pub max_interactions: usize,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_api_timeout_secs")]
    pub api_timeout_secs: u64,
    #[serde(skip, default)]
    pub no_ai: bool,
    /// Log each AI request/response turn at info level (content previews + token counts).
    /// Useful for debugging but verbose; disabled by default.
    #[serde(default)]
    pub log_turns: bool,
    #[serde(default)]
    pub response_cache: bool,
    #[serde(default = "default_response_cache_ttl_days")]
    pub response_cache_ttl_days: u64,
    // Provider-specific settings
    pub claude: Option<ClaudeSettings>,
    pub gemini: Option<GeminiSettings>,
    #[cfg(feature = "bedrock")]
    pub bedrock: Option<BedrockSettings>,
    #[cfg(feature = "vertex")]
    pub vertex: Option<VertexSettings>,
    pub openai_compat: Option<OpenAiCompatSettings>,
    pub ollama: Option<OllamaSettings>,
    pub kiro_cli: Option<KiroCliSettings>,
    pub claude_cli: Option<ClaudeCliSettings>,
    pub devin_cli: Option<DevinCliSettings>,
}

fn default_response_cache_ttl_days() -> u64 {
    7
}

fn default_api_timeout_secs() -> u64 {
    300
}

fn default_temperature() -> f32 {
    1.0
}

fn default_max_interactions() -> usize {
    100
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct CustomRemoteSettings {
    pub name: String,
    pub url: String,
    pub check_all_branches: bool,
    pub only_branches: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct GitSettings {
    pub repository_path: String,
    pub custom_remotes: Option<Vec<CustomRemoteSettings>>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct ReviewSettings {
    pub concurrency: usize,
    pub worktree_dir: String,
    #[serde(default = "default_review_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_max_lines_changed")]
    pub max_lines_changed: usize,
    #[serde(default = "default_max_files_touched")]
    pub max_files_touched: usize,
    #[serde(default)]
    pub ignore_files: Vec<String>,
    #[serde(default = "default_email_policy_path")]
    pub email_policy_path: String,
    /// Maximum cumulative non-cached tokens (uncached input + output) across all turns in a
    /// single review. Cached input tokens are excluded because they cost ~10x less and don't
    /// reflect runaway model behaviour. At Sonnet 4.6 pricing ($3/M uncached input, $15/M
    /// output) the 5M default costs roughly $15–75 depending on input/output mix; a typical
    /// 7-stage review uses ~300–500k tokens total. Set to 0 to disable.
    #[serde(default = "default_max_total_tokens")]
    pub max_total_tokens: usize,
    /// Maximum cumulative output tokens across all turns in a single review.
    /// Conservative default; set to 0 to disable.
    #[serde(default = "default_max_total_output_tokens")]
    pub max_total_output_tokens: usize,
    /// Override the review tool binary path. Not read from config; set programmatically
    /// (e.g. in tests or via environment).
    #[serde(skip)]
    pub review_tool_override: Option<std::path::PathBuf>,
    #[serde(skip)]
    pub stages: Option<Vec<u8>>,
}

fn default_max_total_tokens() -> usize {
    5_000_000
}

fn default_max_total_output_tokens() -> usize {
    500_000
}

fn default_max_lines_changed() -> usize {
    10_000
}

fn default_max_files_touched() -> usize {
    200
}

fn default_review_timeout() -> u64 {
    3600
}

fn default_max_retries() -> u32 {
    3
}

fn default_email_policy_path() -> String {
    "email_policy.toml".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct Settings {
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default)]
    pub project: ProjectSettings,
    #[serde(default = "default_subsystems")]
    pub subsystems: SubsystemsSettings,
    #[serde(default = "default_forge")]
    pub forge: ForgeSettings,
    pub database: DatabaseSettings,
    pub nntp: NntpSettings,
    pub smtp: Option<SmtpSettings>,
    pub mailing_lists: MailingListsSettings,
    pub ai: AiSettings,
    pub server: ServerSettings,
    pub git: GitSettings,
    pub review: ReviewSettings,
}

fn default_subsystems() -> SubsystemsSettings {
    SubsystemsSettings { mapping: vec![] }
}

fn default_forge() -> ForgeSettings {
    ForgeSettings {
        enabled: false,
        disable_nntp: true,
        provider: None,
        webhook_secret: None,
        api_token: None,
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct LocalReviewReviewSettings {
    pub concurrency: Option<usize>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LocalReviewSettings {
    pub ai: AiSettings,
    pub review: Option<LocalReviewReviewSettings>,
}
impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        Self::from_file("Settings")
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let s = Config::builder()
            // Start with default settings
            .add_source(File::from(path.as_ref()))
            // Add settings from environment variables (with a prefix of SASHIKO)
            // e.g. SASHIKO__SERVER__PORT=8081 would set the server port
            .add_source(Environment::with_prefix("SASHIKO").separator("__"))
            .build()?;

        s.try_deserialize()
    }

    pub fn local_review_path() -> PathBuf {
        Self::local_review_path_in(Path::new("."))
    }

    pub fn local_review_path_in(base: &Path) -> PathBuf {
        let local = base.join("Settings.toml");
        if local.exists() {
            return local;
        }

        Self::user_config_path()
    }

    pub fn user_config_path() -> PathBuf {
        if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(config_home).join("sashiko.toml");
        }

        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".config/sashiko.toml");
        }

        PathBuf::from(".config/sashiko.toml")
    }

    pub fn local_review() -> Result<Self, ConfigError> {
        Self::from_file(Self::local_review_path())
    }

    pub fn local_review_settings() -> Result<LocalReviewSettings, ConfigError> {
        Self::local_review_from_file(Self::local_review_path())
    }

    pub fn local_review_from_file(
        path: impl AsRef<Path>,
    ) -> Result<LocalReviewSettings, ConfigError> {
        let s = Config::builder()
            .add_source(File::from(path.as_ref()))
            .add_source(Environment::with_prefix("SASHIKO").separator("__"))
            .build()?;

        s.try_deserialize()
    }

    pub fn local_review_ai() -> Result<AiSettings, ConfigError> {
        Self::ai_from_file(Self::local_review_path())
    }

    pub fn ai_from_file(path: impl AsRef<Path>) -> Result<AiSettings, ConfigError> {
        let s = Config::builder()
            .add_source(File::from(path.as_ref()))
            .add_source(Environment::with_prefix("SASHIKO").separator("__"))
            .build()?;

        let settings: LocalReviewSettings = s.try_deserialize()?;
        Ok(settings.ai)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_review_path_prefers_current_directory() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("Settings.toml"), "").unwrap();
        assert_eq!(
            Settings::local_review_path_in(temp.path()),
            temp.path().join("Settings.toml")
        );
    }

    #[test]
    fn test_user_config_path_uses_xdg_config_home() {
        let temp = tempfile::tempdir().unwrap();
        let old_xdg = std::env::var_os("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path());
        }

        assert_eq!(
            Settings::user_config_path(),
            temp.path().join("sashiko.toml")
        );

        unsafe {
            if let Some(value) = old_xdg {
                std::env::set_var("XDG_CONFIG_HOME", value);
            } else {
                std::env::remove_var("XDG_CONFIG_HOME");
            }
        }
    }
}
