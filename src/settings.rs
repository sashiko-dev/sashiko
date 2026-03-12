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
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct MailingListsSettings {
    pub track: Vec<String>,
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

fn default_prompt_caching() -> bool {
    true
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
    #[serde(skip, default)]
    pub no_ai: bool,
    // Provider-specific settings
    pub claude: Option<ClaudeSettings>,
    pub gemini: Option<GeminiSettings>,
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
