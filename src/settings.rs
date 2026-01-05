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
    pub groups: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct AiSettings {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct ServerSettings {
    pub host: String,
    pub port: u16,
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
}

#[derive(Debug, Deserialize, Clone)]
#[allow(unused)]
pub struct Settings {
    pub database: DatabaseSettings,
    pub nntp: NntpSettings,
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
