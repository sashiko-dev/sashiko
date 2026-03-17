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
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct GeminiSettings {
    #[serde(default)]
    pub explicit_prompt_caching: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct BedrockSettings {
    /// AWS region for Bedrock API calls (e.g. "us-east-1").
    /// If omitted, uses the standard AWS SDK default chain.
    pub region: Option<String>,
}

fn default_prompt_caching() -> bool {
    true
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
    // Provider-specific settings
    pub claude: Option<ClaudeSettings>,
    pub gemini: Option<GeminiSettings>,
    pub bedrock: Option<BedrockSettings>,
    pub openai_compat: Option<OpenAiCompatSettings>,
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
pub struct GitSettings {
    pub repository_path: String,
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

fn default_log_level() -> String {
    "info".to_string()
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct Settings {
    #[serde(default = "default_log_level")]
    pub log_level: String,
    pub database: DatabaseSettings,
    pub nntp: NntpSettings,
    pub smtp: Option<SmtpSettings>,
    pub mailing_lists: MailingListsSettings,
    pub ai: AiSettings,
    pub server: ServerSettings,
    pub git: GitSettings,
    pub review: ReviewSettings,
}

impl Settings {
    pub fn new() -> Result<Self, ConfigError> {
        let s = Config::builder()
            // Start with default settings
            .add_source(File::with_name("Settings"))
            // Add settings from environment variables (with a prefix of SASHIKO)
            // e.g. SASHIKO_SERVER__PORT=8081 would set the server port
            .add_source(Environment::with_prefix("SASHIKO").separator("__"))
            .build()?;

        s.try_deserialize()
    }
}
