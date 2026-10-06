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

use crate::project::ProjectId;

/// The name of the file holding the server's local operator token.
///
/// The leading dot keeps it out of a casual listing of the state directory,
/// which is where an operator would otherwise be tempted to copy it from.
pub const LOCAL_TOKEN_FILE_NAME: &str = ".sashiko-local-token";

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct SubsystemMapping {
    pub pattern: String,
    pub name: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct SubsystemsSettings {
    #[serde(default)]
    pub mapping: Vec<SubsystemMapping>,
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct ProjectSettings {
    /// The project this configuration file is for.
    ///
    /// Optional, and absent means "any": a configuration written before
    /// projects existed cannot be expected to name one. When it is present it
    /// is checked against the selected project, because a configuration that
    /// names a project and is used for a different one is pointing at the
    /// wrong database.
    #[serde(default)]
    pub kind: Option<ProjectId>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub domain: String,
    #[serde(default)]
    pub attribution: Option<String>,
}

impl ProjectSettings {
    pub fn attribution(&self) -> &str {
        if let Some(ref attr) = self.attribution {
            attr.as_str()
        } else if !self.domain.is_empty() {
            self.domain.as_str()
        } else {
            "sashiko"
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ForgePostMode {
    #[default]
    Off,
    DryRun,
    Live,
}

impl ForgePostMode {
    pub fn outbox_status(self, embargoed: bool) -> &'static str {
        match self {
            ForgePostMode::Off => "Disabled",
            ForgePostMode::DryRun => "Dry-Run",
            ForgePostMode::Live if embargoed => "Embargoed",
            ForgePostMode::Live => "Pending",
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct ForgeSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub disable_nntp: bool,
    pub provider: Option<String>,
    pub webhook_secret: Option<String>,
    pub api_token: Option<String>,
    #[serde(default)]
    pub post_mode: ForgePostMode,
    #[serde(default)]
    pub app_id: Option<u64>,
    #[serde(default)]
    pub installation_id: Option<u64>,
    #[serde(default)]
    pub app_private_key: Option<String>,
    #[serde(default)]
    pub app_private_key_path: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct DatabaseSettings {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub token: String,
}

/// An empty url, which is no database at all.
///
/// The section is optional because a local review has none and never asks. The
/// daemon cannot run without one, which `validate_for_daemon` reports rather
/// than letting an empty string reach a connection attempt.
impl Default for DatabaseSettings {
    fn default() -> Self {
        Self {
            url: String::new(),
            token: String::new(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct NntpSettings {
    #[serde(default)]
    pub server: String,
    #[serde(default = "default_nntp_port")]
    pub port: u16,
    /// Implicit NNTPS. The server port is set separately, so an
    /// operator turning this on also moves `port` to 563.
    #[serde(default)]
    pub tls: bool,
}

fn default_nntp_port() -> u16 {
    119
}

impl Default for NntpSettings {
    fn default() -> Self {
        Self {
            server: String::new(),
            port: default_nntp_port(),
            tls: false,
        }
    }
}

/// How a completed message reaches the outside world.
#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MailTransport {
    /// Connect to a remote submission service over implicit TLS.
    #[default]
    Smtp,
    /// Pipe the message to a local sendmail binary and let the host
    /// MTA relay it.
    Sendmail,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct SmtpSettings {
    #[serde(default)]
    pub transport: MailTransport,
    pub server: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Path to the sendmail binary. Defaults to DEFAULT_SENDMAIL_PATH
    /// rather than to a PATH lookup, since a service unit rarely
    /// inherits an operator's PATH.
    pub sendmail_path: Option<String>,
    pub sender_address: String,
    pub reply_to: Option<String>,
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
}

pub const DEFAULT_SENDMAIL_PATH: &str = "/usr/sbin/sendmail";

impl SmtpSettings {
    /// The binary the sendmail transport spawns. Callers that check
    /// the path and callers that run it must agree on the fallback,
    /// or the check reports on a file the transport never opens.
    pub fn sendmail_command(&self) -> &str {
        self.sendmail_path
            .as_deref()
            .unwrap_or(DEFAULT_SENDMAIL_PATH)
    }

    /// Rejects a configuration whose transport and its operands
    /// disagree. Both transports share this section, so serde cannot
    /// tell a missing key from an inapplicable one.
    pub fn validate(&self) -> Result<(), String> {
        match self.transport {
            MailTransport::Smtp => {
                if self.server.is_none() || self.port.is_none() {
                    return Err(
                        "smtp.server and smtp.port are required when smtp.transport is \"smtp\""
                            .to_string(),
                    );
                }
            }
            MailTransport::Sendmail => {
                if self.username.is_some() || self.password.is_some() {
                    return Err(
                        "smtp.username and smtp.password have no effect when smtp.transport is \"sendmail\""
                            .to_string(),
                    );
                }
            }
        }
        Ok(())
    }
}

fn default_dry_run() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct MailingListsSettings {
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub track: Vec<String>,
}

/// Reads a list written either as a TOML array or as one comma separated
/// string.
///
/// The second form exists for the environment, which has no arrays: a
/// deployment that sets a list through SASHIKO__* would otherwise fail to
/// start with "invalid type: string, expected a sequence", and its only
/// recourse would be to bake the value into Settings.toml.
///
/// Empty entries are dropped, so a trailing comma and an empty variable both
/// mean what they look like rather than naming a list with a blank member.
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
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct GeminiSettings {
    #[serde(default)]
    pub explicit_prompt_caching: bool,
    /// Optional custom base URL (overrides default https://generativelanguage.googleapis.com).
    /// Can also be set via GOOGLE_GEMINI_BASE_URL or GEMINI_BASE_URL env vars.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Optional shell command to start a local HTTP proxy daemon on demand.
    /// Can include `{port_file}` placeholder for dynamic port discovery.
    /// Can also be set via GEMINI_PROXY_COMMAND env var.
    #[serde(default)]
    pub proxy_command: Option<String>,
    /// Optional shell command to obtain a Bearer token for Authorization header.
    /// Can also be set via GEMINI_AUTH_TOKEN_COMMAND env var.
    #[serde(default)]
    pub auth_token_command: Option<String>,
}

#[cfg(feature = "bedrock")]
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct VertexSettings {
    /// GCP project ID. Falls back to the ANTHROPIC_VERTEX_PROJECT_ID or
    /// GOOGLE_CLOUD_PROJECT env var, in that order.
    #[serde(default)]
    pub project_id: Option<String>,
    /// GCP region (e.g., "us-east5", "global"). Falls back to the
    /// CLOUD_ML_REGION or GOOGLE_CLOUD_LOCATION env var, in that order.
    #[serde(default)]
    pub region: Option<String>,
    /// Claude only.
    #[serde(default = "default_prompt_caching")]
    pub prompt_caching: bool,
    /// Claude only.
    #[serde(default = "default_vertex_max_tokens")]
    pub max_tokens: u32,
    /// Claude only.
    #[serde(default)]
    pub thinking: Option<String>,
    /// Claude only.
    #[serde(default)]
    pub effort: Option<String>,
}

#[cfg(feature = "vertex")]
fn default_vertex_max_tokens() -> u32 {
    8192
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct VllmSettings {
    #[serde(default)]
    pub base_url: Option<String>,
    /// Should match the server-side `--max-model-len`.
    #[serde(default)]
    pub context_window_size: Option<usize>,
    /// Completion token limit. Leave unset to let vLLM generate up to the
    /// remaining context (`max_model_len - prompt_tokens`).
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Enable or disable thinking for reasoning models (e.g. Qwen3) via
    /// `chat_template_kwargs`. Leave unset for the model default.
    #[serde(default)]
    pub enable_thinking: Option<bool>,
    /// Enforce JSON responses with guided decoding (`response_format`).
    /// Disabled by default because not every vLLM backend supports it;
    /// without it the JSON requirement is injected into the system prompt.
    #[serde(default)]
    pub guided_json: bool,
    /// Forward tool definitions to the server. Disabled by default because a
    /// server started without `--enable-auto-tool-choice` and
    /// `--tool-call-parser` rejects requests carrying tools with HTTP 400.
    #[serde(default)]
    pub enable_tools: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct GooseCliSettings {
    #[serde(default = "default_goose_cli_binary")]
    pub binary: String,
    /// Backend goose itself talks to, passed as GOOSE_PROVIDER. Use "openai"
    /// with an OPENAI_HOST override for a local vLLM server.
    #[serde(default = "default_goose_cli_provider")]
    pub goose_provider: String,
    /// Environment for the goose child process, e.g. OPENAI_HOST. goose
    /// inherits Sashiko's environment and these entries win over it, but
    /// not over the variables Sashiko pins to keep goose a completion
    /// backend.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_goose_cli_context_window")]
    pub context_window_size: usize,
}

fn default_goose_cli_binary() -> String {
    "goose".to_string()
}

fn default_goose_cli_provider() -> String {
    "openai".to_string()
}

fn default_goose_cli_context_window() -> usize {
    128_000
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct ClaudeCliSettings {
    /// Effort level passed to `claude --effort`. Valid values per Claude Code:
    /// "low", "medium", "high", "xhigh", "max". Leave unset for the model default.
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct CodexCliSettings {
    /// Reasoning effort passed as `codex exec -c model_reasoning_effort=<v>`.
    /// Valid values: "none", "minimal", "low", "medium", "high", "xhigh",
    /// "max". Leave unset for the account default. A `-c` override outranks
    /// `~/.codex/config.toml`, but not an enterprise-managed requirements
    /// layer, which substitutes its own value whatever the origin. A run
    /// whose effort that layer substitutes fails.
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
    pub vllm: Option<VllmSettings>,
    pub kiro_cli: Option<KiroCliSettings>,
    pub goose_cli: Option<GooseCliSettings>,
    pub claude_cli: Option<ClaudeCliSettings>,
    pub codex_cli: Option<CodexCliSettings>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Ingest,
    Cancel,
    Review,
}

impl Permission {
    /// Whether a caller holding the server's local token may exercise this
    /// capability without presenting an identity.
    ///
    /// The token exists so a developer running the server can drive it without
    /// configuring a JWT secret, and it is tolerable only where the blast
    /// radius is that local instance. Authority over a Linux kernel bug is
    /// deliberately not a Permission: it is resolved per bug by Principal,
    /// which never reaches this path, so no amount of local access opens the
    /// bug database.
    ///
    /// The match is exhaustive rather than defaulted so that a capability
    /// added later is not reachable until someone writes it down here.
    pub fn granted_by_local_token(self) -> bool {
        match self {
            Permission::Ingest => true,
            Permission::Cancel => true,
            Permission::Review => true,
        }
    }
}

/// Access Control List settings utilizing fine-grained capability endpoints.
/// By default (if omitted), all vectors are safely initialized empty (Fail-Closed).
/// Users must explicitly be added to the necessary capability lists to perform mutations.
/// The `blocklist` explicitly denies all capabilities, overriding any grants.
///
/// Every list reads a comma separated string as well as an array, because who
/// holds a capability is deployment state rather than a property of the
/// program: an image ships one Settings.toml and each deployment has to be
/// able to name its own operators through the environment.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct AclSettings {
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub admins: Vec<String>,
    /// The kernel security list. Reads and comments on every bug, and reads
    /// the raw analysis transcripts, without gaining any of the capabilities
    /// below.
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub security: Vec<String>,
    /// Principals allowed to file a bug over HTTP. Empty means only operators
    /// can, which is the shipped configuration.
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub bug_reporters: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub ingest: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub cancel: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub review: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub blocklist: Vec<String>,
}

/// Matches an address against a capability list.
///
/// Both sides are trimmed and compared case insensitively: the list is written
/// by hand in a configuration file and the address arrives from a sign-in
/// form, so a stray space on either side must not decide who gets in or,
/// worse, let a blocklisted address slip past.
fn list_contains(list: &[String], email: &str) -> bool {
    let email = email.trim();
    list.iter().any(|e| e.trim().eq_ignore_ascii_case(email))
}

impl AclSettings {
    pub fn is_blocklisted(&self, email: &str) -> bool {
        list_contains(&self.blocklist, email)
    }

    /// Whether the address is an operator. Operators are never blocklisted
    /// implicitly; the caller checks the blocklist first.
    pub fn is_admin(&self, email: &str) -> bool {
        list_contains(&self.admins, email)
    }

    /// Whether the address is on the kernel security list.
    pub fn is_security(&self, email: &str) -> bool {
        list_contains(&self.security, email)
    }

    /// Whether the address may file a bug over HTTP.
    pub fn is_bug_reporter(&self, email: &str) -> bool {
        list_contains(&self.bug_reporters, email)
    }

    /// Whether the address appears in any capability list.
    ///
    /// This answers "is this somebody the operator has configured", which is
    /// the question a sign-in request asks. It deliberately covers every list,
    /// including the ones that grant nothing beyond bug access, because an
    /// address that can do something must be able to sign in and do it.
    pub fn is_known_identity(&self, email: &str) -> bool {
        !self.is_blocklisted(email)
            && [
                &self.admins,
                &self.security,
                &self.bug_reporters,
                &self.ingest,
                &self.cancel,
                &self.review,
            ]
            .iter()
            .any(|list| list_contains(list, email))
    }

    pub fn has_permission(&self, email: &str, perm: Permission) -> bool {
        if self.is_blocklisted(email) {
            return false;
        }
        if self.is_admin(email) {
            return true;
        }
        match perm {
            Permission::Ingest => list_contains(&self.ingest, email),
            Permission::Cancel => list_contains(&self.cancel, email),
            Permission::Review => list_contains(&self.review, email),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct ServerSettings {
    #[serde(default = "default_server_host")]
    pub host: String,
    #[serde(default = "default_server_port")]
    pub port: u16,
    /// The URL the service is reachable at from outside, without a trailing
    /// slash.
    ///
    /// The bind address cannot stand in for this: the shipped host is the
    /// wildcard "::", which renders a sign-in link nobody can open.
    #[serde(default)]
    pub public_base_url: Option<String>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub testing_mode: bool,
    pub jwt_secret: Option<String>,
    /// Prints sign-in links in full to the log.
    ///
    /// A sign-in link is a bearer credential, and the log is the one place it
    /// is read by something other than its recipient: proxies, log shippers and
    /// anyone with journal access all see it. It is therefore withheld unless
    /// this is switched on deliberately, which is only reasonable on a
    /// developer machine with no real users.
    #[serde(default)]
    pub log_sign_in_links: bool,

    #[serde(default)]
    pub acl: AclSettings,
}

fn default_server_host() -> String {
    "::".to_string()
}

fn default_server_port() -> u16 {
    8080
}

/// The host and port the shipped Settings.toml names, so a file that omits the
/// section binds where one that spelled it out would.
impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            host: default_server_host(),
            port: default_server_port(),
            public_base_url: None,
            read_only: false,
            testing_mode: false,
            jwt_secret: None,
            log_sign_in_links: false,
            acl: AclSettings::default(),
        }
    }
}

impl ServerSettings {
    /// The base URL to build a sign-in link on, without a trailing slash.
    ///
    /// Falls back to the bind address, which is only good enough when the link
    /// is written to the log for a local operator to read.
    pub fn sign_in_base_url(&self) -> String {
        match self.public_base_url.as_deref().map(str::trim) {
            Some(url) if !url.is_empty() => url.trim_end_matches('/').to_string(),
            _ => format!("http://{}:{}", self.host, self.port),
        }
    }
}

/// Whether a configured public base URL is usable in a message sent to
/// somebody else.
///
/// A bind address is not: the wildcard forms resolve to whatever interface the
/// process happens to be listening on, and loopback means nothing to a reader
/// on another machine.
fn is_reachable_base_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.trim().split_once("://") else {
        return false;
    };
    if !matches!(scheme, "http" | "https") {
        return false;
    }
    let authority = rest.split('/').next().unwrap_or("");
    // An IPv6 literal is bracketed, so only a colon outside the brackets
    // separates the port.
    let host = match authority.strip_prefix('[') {
        Some(inside) => inside.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    !matches!(
        host,
        "" | "::" | "0.0.0.0" | "*" | "localhost" | "127.0.0.1" | "::1"
    )
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct CustomRemoteSettings {
    pub name: String,
    pub url: String,
    pub check_all_branches: bool,
    pub only_branches: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct GitSettings {
    #[serde(default)]
    pub repository_path: String,
    pub custom_remotes: Option<Vec<CustomRemoteSettings>>,
}

/// No repository. A local review has none to configure, since it reviews the
/// checkout it was run in, and `main` writes that path here. The daemon must be
/// given one, which `validate_for_daemon` checks.
impl Default for GitSettings {
    fn default() -> Self {
        Self {
            repository_path: String::new(),
            custom_remotes: None,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct ReviewSettings {
    pub concurrency: usize,
    /// Parent directory for the worktrees a review worker checks patches out
    /// into, or None for one under the system temporary directory.
    ///
    /// Not read by `sashiko review`, which reviews commits in the checkout it
    /// was run in and makes no worktree. The daemon must name one, because it
    /// empties the directory on startup; `validate_for_daemon` enforces that.
    #[serde(default)]
    pub worktree_dir: Option<String>,
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
    #[serde(skip)]
    pub stages: Option<Vec<String>>,
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
#[serde(deny_unknown_fields)]
#[allow(unused)]
pub struct LinuxBugSettings {
    /// Whether pre-existing bug tracking and the background bug worker are enabled.
    /// Disabled by default so pre-existing issues are ignored unless opted in.
    #[serde(default)]
    pub enabled: bool,
    /// Maximum number of concurrent bug analyses run by the background bug worker.
    #[serde(default = "default_bug_concurrency")]
    pub concurrency: usize,
    /// Whether periodic upstream bug fix verification is enabled.
    /// Disabled by default (`false`).
    #[serde(default, alias = "verify_fixes")]
    pub fix_check_enabled: bool,
    #[serde(default = "default_bug_lease_ttl_seconds")]
    pub lease_ttl_seconds: i64,
    #[serde(default = "default_bug_max_attempts")]
    pub max_attempts: i64,
    /// Interval in seconds between periodic upstream fix checks against Linus's tree.
    /// Set to 0 to disable periodic upstream fix checks.
    #[serde(default = "default_fix_check_interval_seconds")]
    pub fix_check_interval_seconds: u64,
    /// Maximum number of open bugs to evaluate per upstream fix check cycle.
    #[serde(default = "default_fix_check_batch_size")]
    pub fix_check_batch_size: usize,
}

fn default_bug_concurrency() -> usize {
    4
}

fn default_bug_lease_ttl_seconds() -> i64 {
    300
}

fn default_bug_max_attempts() -> i64 {
    3
}

fn default_fix_check_interval_seconds() -> u64 {
    21_600
}

fn default_fix_check_batch_size() -> usize {
    50
}

impl Default for LinuxBugSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            concurrency: default_bug_concurrency(),
            fix_check_enabled: false,
            lease_ttl_seconds: default_bug_lease_ttl_seconds(),
            max_attempts: default_bug_max_attempts(),
            fix_check_interval_seconds: default_fix_check_interval_seconds(),
            fix_check_batch_size: default_fix_check_batch_size(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
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
    /// Defaulted, like the other sections only the daemon reads. A local review
    /// has no database and no server, so requiring those sections would mean one
    /// file shape for the daemon and a second for a review, and two shapes can
    /// disagree. The daemon asks for what it needs through
    /// `validate_for_daemon` instead.
    #[serde(default)]
    pub database: DatabaseSettings,
    #[serde(default)]
    pub nntp: NntpSettings,
    pub smtp: Option<SmtpSettings>,
    #[serde(default)]
    pub mailing_lists: MailingListsSettings,
    pub ai: AiSettings,
    #[serde(default)]
    pub server: ServerSettings,
    #[serde(default)]
    pub git: GitSettings,
    /// Not defaulted: concurrency has no answer worth guessing, since it says
    /// how much of the machine a review may take.
    pub review: ReviewSettings,
    #[serde(default, alias = "bugs")]
    pub linux_bug: LinuxBugSettings,
}

/// What a command line can say that a settings file says too.
///
/// A flag not given is None, or false for the two that can only turn something
/// off. Every command fills in the ones it has and leaves the rest alone.
#[derive(Debug, Default)]
pub struct Overrides {
    pub repository_path: Option<String>,
    pub worktree_dir: Option<String>,
    pub no_ai: bool,
    pub ai_provider: Option<String>,
    pub stages: Option<Vec<String>>,
    pub read_only: bool,
    pub port: Option<u16>,
}

impl Settings {
    /// Merges command-line overrides in, each winning over the file.
    ///
    /// One resolver for every command, so `sashiko review`, a worker, and the
    /// daemon cannot come to differ on which source wins. Returns the lines
    /// the daemon has always logged at startup for its own four flags, for the
    /// daemon to log. No other override was ever reported, so none produces a
    /// line.
    pub fn apply_overrides(&mut self, overrides: Overrides) -> Vec<String> {
        let mut changed = Vec::new();
        if let Some(path) = overrides.repository_path {
            self.git.repository_path = path;
        }
        if let Some(dir) = overrides.worktree_dir {
            self.review.worktree_dir = Some(dir);
        }
        if overrides.no_ai {
            changed.push("AI interactions disabled via --no-ai flag".to_string());
            self.ai.no_ai = true;
        }
        if let Some(provider) = overrides.ai_provider {
            self.ai.provider = provider;
        }
        if overrides.read_only {
            changed.push("API enabled in READ-ONLY mode via --no-api flag".to_string());
            self.server.read_only = true;
        }
        if let Some(port) = overrides.port {
            changed.push(format!("Server port overridden via --port flag: {port}"));
            self.server.port = port;
        }
        if let Some(stages) = overrides.stages {
            changed.push(format!("Selected stages via --stages flag: {stages:?}"));
            self.review.stages = Some(stages);
        }
        changed
    }

    /// Refuses a configuration that names no database, no repository, or no
    /// worktree directory.
    ///
    /// All three are optional to parse, because a local review needs none of
    /// them and reads the same file shape. The daemon cannot work without them,
    /// and an empty one reaching a connection attempt or a git command produces
    /// an error far from its cause.
    pub fn validate_for_daemon(&self) -> Result<(), String> {
        if self.database.url.trim().is_empty() {
            return Err(
                "[database] url must be set: the daemon keeps every patchset, review and \
                 finding there"
                    .to_string(),
            );
        }
        if self.git.repository_path.trim().is_empty() {
            return Err(
                "[git] repository_path must be set: the daemon reviews patches against a \
                 checkout, and has no working directory to fall back on"
                    .to_string(),
            );
        }
        if self
            .review
            .worktree_dir
            .as_deref()
            .is_none_or(|dir| dir.trim().is_empty())
        {
            return Err(
                "[review] worktree_dir must be set: the daemon makes a worktree per review and \
                 empties that directory when it starts, so it needs one of its own rather than \
                 the temporary directory a single review would use"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Whether NNTP server and tracked mailing lists are configured.
    pub fn has_nntp_config(&self) -> bool {
        !self.nntp.server.trim().is_empty() && !self.mailing_lists.track.is_empty()
    }
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
        post_mode: ForgePostMode::Off,
        app_id: None,
        installation_id: None,
        app_private_key: None,
        app_private_key_path: None,
    }
}

impl Settings {
    /// The settings file this process reads, in order: the path a caller
    /// names, then SASHIKO_CONFIG, then Settings.toml in the working directory,
    /// then the user's configuration file.
    ///
    /// Separate from `load` so a caller can name the file in an error when it
    /// fails to load.
    pub fn resolve_path(named: Option<&Path>) -> PathBuf {
        match named {
            Some(path) => path.to_path_buf(),
            None => match std::env::var_os("SASHIKO_CONFIG") {
                Some(from_env) => PathBuf::from(from_env),
                None => Self::local_review_path(),
            },
        }
    }

    /// Loads the settings for this process from the file `resolve_path`
    /// chooses, with environment variables prefixed SASHIKO layered over it.
    ///
    /// One routine for every command, so the daemon and a review cannot read
    /// different files and disagree about what they say.
    pub fn load(named: Option<&Path>) -> Result<Self, ConfigError> {
        Self::from_file(Self::resolve_path(named))
    }

    pub fn new() -> Result<Self, ConfigError> {
        Self::load(None)
    }

    /// Refuses a configuration that would mail sign-in links nobody can open.
    ///
    /// Without SMTP the link is written to the log for a local operator to
    /// read, so the base URL is optional. With SMTP it is the only thing
    /// standing between a maintainer and a dead link, and a deployment that
    /// fails to start is far kinder than one that silently mails
    /// http://:::8080/ at three in the morning.
    pub fn validate_sign_in_delivery(&self) -> Result<(), String> {
        if self.smtp.is_none() {
            return Ok(());
        }
        match self.server.public_base_url.as_deref() {
            Some(url) if is_reachable_base_url(url) => Ok(()),
            Some(url) => Err(format!(
                "server.public_base_url is {:?}, which names a bind address rather than a host a \
                 recipient can reach. Set it to the URL the service is served at",
                url
            )),
            None => Err(
                "server.public_base_url must be set when SMTP is configured, because sign-in \
                 links are mailed and the bind address does not name a reachable host"
                    .to_string(),
            ),
        }
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let s = Config::builder()
            // Start with default settings
            .add_source(File::from(path.as_ref()))
            // Add settings from environment variables (with a prefix of SASHIKO)
            // e.g. SASHIKO__SERVER__PORT=8081 would set the server port
            .add_source(Environment::with_prefix("SASHIKO").separator("__"))
            .build()?;

        let settings: Self = s.try_deserialize()?;
        if let Some(smtp) = &settings.smtp {
            smtp.validate().map_err(ConfigError::Message)?;
        }

        Ok(settings)
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

    /// The file the server writes its local operator token to.
    ///
    /// The path is derived rather than configured so that a local tool finds
    /// the token without being told where to look. The database is the one
    /// thing every participant already agrees on: a client that reads a
    /// different Settings.toml than the server would talk to a different
    /// database too, and is by definition not local to it.
    ///
    /// A remote database names no directory, so the token falls back to the
    /// working directory, which is where the configuration was read from.
    pub fn local_token_path(&self) -> PathBuf {
        let url = self.database.url.trim();
        let dir = if url.contains("://") {
            Path::new("")
        } else {
            Path::new(url).parent().unwrap_or(Path::new(""))
        };

        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };

        dir.join(LOCAL_TOKEN_FILE_NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_production_settings_is_valid() {
        let path = "Settings.toml";
        if Path::new(path).exists() {
            let _ = Settings::from_file("Settings")
                .expect("Production 'Settings.toml' failed to parse");
        }
    }

    #[test]
    fn test_project_settings_attribution_and_domain() {
        let default_proj = ProjectSettings::default();
        assert_eq!(default_proj.domain, "");
        assert_eq!(default_proj.attribution(), "sashiko");

        let toml_default: ProjectSettings = toml::from_str("name = \"Test\"").unwrap();
        assert_eq!(toml_default.domain, "");
        assert_eq!(toml_default.attribution(), "sashiko");

        let toml_domain: ProjectSettings = toml::from_str("domain = \"sashiko.dev\"").unwrap();
        assert_eq!(toml_domain.domain, "sashiko.dev");
        assert_eq!(toml_domain.attribution(), "sashiko.dev");

        let toml_attr: ProjectSettings =
            toml::from_str("domain = \"custom.org\"\nattribution = \"custom-team\"").unwrap();
        assert_eq!(toml_attr.attribution(), "custom-team");
    }

    /// A file with no database, server, or repository is what a local review
    /// has, and it parses: one shape serves the daemon and a review both.
    /// concurrency is still required.
    #[test]
    fn test_one_shape_reads_a_file_with_no_daemon_sections() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Settings.toml");
        let ai = "[ai]\nprovider = \"gemini\"\nmodel = \"gemini-3-pro\"\n";

        std::fs::write(&path, ai).unwrap();
        assert!(
            Settings::load(Some(&path)).is_err(),
            "[review] is still required"
        );

        std::fs::write(&path, format!("{}\n[review]\nconcurrency = 8\n", ai)).unwrap();
        let settings = Settings::load(Some(&path)).expect("a local review's file parses");
        assert_eq!(settings.review.concurrency, 8);
        // Defaults, as they do for the daemon.
        assert_eq!(settings.review.timeout_seconds, 3600);
        assert_eq!(settings.server.port, 8080);
        // And the sections it does not have read as absent rather than as an
        // error. The daemon checks for them itself.
        assert!(settings.database.url.is_empty());
        assert!(settings.git.repository_path.is_empty());
        assert!(settings.validate_for_daemon().is_err());
    }

    /// A local review's own reduced shape ignored keys it did not know, so a
    /// mistyped one did nothing and said nothing. The one shape refuses an
    /// unknown key or section by name, for a review as for the daemon.
    #[test]
    fn test_a_local_review_refuses_what_it_does_not_know() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Settings.toml");
        let ai = "[ai]\nprovider = \"gemini\"\nmodel = \"gemini-3-pro\"\n";

        for (unknown, extra) in [
            (
                "concurency",
                "\n[review]\nconcurrency = 8\nconcurency = 8\n",
            ),
            ("bogus", "\n[review]\nconcurrency = 8\n\n[bogus]\nx = 1\n"),
        ] {
            std::fs::write(&path, format!("{ai}{extra}")).unwrap();
            let error = Settings::load(Some(&path)).unwrap_err().to_string();
            assert!(
                error.contains(&format!("unknown field `{unknown}`")),
                "{error}"
            );
        }
    }

    /// The daemon cannot run on a file a local review is happy with, and says so
    /// at startup rather than failing later at a connection or a git command.
    #[test]
    fn test_the_daemon_asks_for_what_only_it_needs() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Settings.toml");
        let base = "[ai]\nprovider = \"gemini\"\nmodel = \"gemini-3-pro\"\n\
                    \n[review]\nconcurrency = 8\n";

        std::fs::write(&path, format!("{base}\n[database]\nurl = \"sashiko.db\"\n")).unwrap();
        let settings = Settings::load(Some(&path)).unwrap();
        assert!(
            settings.validate_for_daemon().is_err(),
            "a database without a repository is not enough"
        );

        std::fs::write(
            &path,
            format!(
                "{base}\n[database]\nurl = \"sashiko.db\"\n\n[git]\nrepository_path = \"linux\"\n"
            ),
        )
        .unwrap();
        let settings = Settings::load(Some(&path)).unwrap();
        assert!(
            settings.validate_for_daemon().is_err(),
            "and a worktree directory is required as it was before, since the daemon empties it"
        );

        std::fs::write(
            &path,
            format!(
                "{base}worktree_dir = \"review_trees\"\n\
                 \n[database]\nurl = \"sashiko.db\"\n\n[git]\nrepository_path = \"linux\"\n"
            ),
        )
        .unwrap();
        let settings = Settings::load(Some(&path)).unwrap();
        assert!(settings.validate_for_daemon().is_ok());
    }

    /// A flag wins over the file, a flag not given leaves the file alone, and
    /// only the daemon's own flags produce the lines it has always logged.
    #[test]
    fn test_overrides_win_over_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("Settings.toml");
        std::fs::write(
            &path,
            "[ai]\nprovider = \"gemini\"\nmodel = \"gemini-3-pro\"\n\
             \n[review]\nconcurrency = 8\nworktree_dir = \"from_file\"\n\
             \n[git]\nrepository_path = \"from_file\"\n",
        )
        .unwrap();

        let mut settings = Settings::load(Some(&path)).unwrap();
        assert!(settings.apply_overrides(Overrides::default()).is_empty());
        assert_eq!(settings.ai.provider, "gemini");
        assert_eq!(settings.git.repository_path, "from_file");
        assert_eq!(settings.review.worktree_dir.as_deref(), Some("from_file"));
        assert!(!settings.ai.no_ai);
        assert!(settings.review.stages.is_none());

        let changed = settings.apply_overrides(Overrides {
            repository_path: Some("from_flag".to_string()),
            worktree_dir: Some("from_flag".to_string()),
            no_ai: true,
            ai_provider: Some("stdio-claude".to_string()),
            stages: Some(vec!["pre-screen".to_string()]),
            read_only: true,
            port: Some(9090),
        });
        // The daemon logs these at startup, so their wording and order are
        // user-visible. The other three overrides were never reported.
        assert_eq!(
            changed,
            [
                "AI interactions disabled via --no-ai flag",
                "API enabled in READ-ONLY mode via --no-api flag",
                "Server port overridden via --port flag: 9090",
                "Selected stages via --stages flag: [\"pre-screen\"]",
            ]
        );
        assert_eq!(settings.git.repository_path, "from_flag");
        assert_eq!(settings.review.worktree_dir.as_deref(), Some("from_flag"));
        assert!(settings.ai.no_ai);
        assert_eq!(settings.ai.provider, "stdio-claude");
        assert_eq!(
            settings.review.stages.as_deref(),
            Some(&["pre-screen".to_string()][..])
        );
        assert!(settings.server.read_only);
        assert_eq!(settings.server.port, 9090);
    }

    /// `sashiko init` writes this template, so it has to parse or the command
    /// disagrees with itself out of the box.
    #[test]
    fn test_init_template_parses() {
        Settings::load(Some(Path::new("docs/examples/Settings.example.toml")))
            .expect("init template must parse");
    }

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
    fn test_nntp_tls_defaults_to_off() {
        let nntp: NntpSettings =
            toml::from_str("server = \"nntp.lore.kernel.org\"\nport = 119\n").unwrap();
        assert!(!nntp.tls);
    }

    #[test]
    fn test_nntp_tls_is_configurable() {
        let nntp: NntpSettings =
            toml::from_str("server = \"news.internal.example\"\nport = 563\ntls = true\n").unwrap();
        assert!(nntp.tls);
    }

    #[test]
    fn test_smtp_transport_defaults_to_smtp() {
        let smtp: SmtpSettings = toml::from_str(
            "server = \"smtp.example.com\"\nport = 587\nsender_address = \"bot@example.com\"\n",
        )
        .unwrap();
        assert_eq!(smtp.transport, MailTransport::Smtp);
        assert!(smtp.validate().is_ok());
    }

    #[test]
    fn test_sendmail_transport_needs_no_server() {
        let smtp: SmtpSettings = toml::from_str(
            "transport = \"sendmail\"\nsender_address = \"bot@example.com\"\ndry_run = false\n",
        )
        .unwrap();
        assert_eq!(smtp.transport, MailTransport::Sendmail);
        assert!(smtp.sendmail_path.is_none());
        assert!(smtp.validate().is_ok());
    }

    #[test]
    fn test_smtp_transport_requires_server_and_port() {
        let smtp: SmtpSettings =
            toml::from_str("port = 587\nsender_address = \"bot@example.com\"\n").unwrap();
        assert!(smtp.validate().is_err());
    }

    #[test]
    fn test_sendmail_transport_rejects_credentials() {
        let smtp: SmtpSettings = toml::from_str(
            "transport = \"sendmail\"\nusername = \"bot\"\nsender_address = \"bot@example.com\"\n",
        )
        .unwrap();
        assert!(smtp.validate().is_err());
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

    #[test]
    fn test_acl_default_fails_closed() {
        let acl = AclSettings::default();
        let email = "user@example.com";
        assert!(!acl.is_blocklisted(email));
        assert!(!acl.has_permission(email, Permission::Ingest));
        assert!(!acl.has_permission(email, Permission::Cancel));
        assert!(!acl.has_permission(email, Permission::Review));
    }

    #[test]
    fn test_acl_admin_grants_all_permissions() {
        let acl = AclSettings {
            admins: vec!["admin@example.com".to_string()],
            ..Default::default()
        };
        assert!(acl.has_permission("admin@example.com", Permission::Ingest));
        assert!(acl.has_permission("admin@example.com", Permission::Cancel));
        assert!(acl.has_permission("admin@example.com", Permission::Review));
    }

    #[test]
    fn test_acl_granular_capabilities() {
        let acl = AclSettings {
            ingest: vec!["bot@example.com".to_string()],
            cancel: vec!["cron@example.com".to_string()],
            review: vec!["reviewer@example.com".to_string()],
            ..Default::default()
        };

        assert!(acl.has_permission("bot@example.com", Permission::Ingest));
        assert!(!acl.has_permission("bot@example.com", Permission::Cancel));
        assert!(!acl.has_permission("bot@example.com", Permission::Review));

        assert!(acl.has_permission("reviewer@example.com", Permission::Review));
        assert!(!acl.has_permission("reviewer@example.com", Permission::Ingest));
    }

    #[test]
    fn test_acl_blocklist_preempts_all_capabilities_and_admin() {
        let acl = AclSettings {
            admins: vec!["rogue_admin@example.com".to_string()],
            ingest: vec!["rogue_admin@example.com".to_string()],
            cancel: vec!["rogue_admin@example.com".to_string()],
            review: vec!["rogue_admin@example.com".to_string()],
            blocklist: vec!["rogue_admin@example.com".to_string()],
            ..Default::default()
        };

        assert!(acl.is_blocklisted("rogue_admin@example.com"));
        assert!(!acl.has_permission("rogue_admin@example.com", Permission::Ingest));
        assert!(!acl.has_permission("rogue_admin@example.com", Permission::Cancel));
        assert!(!acl.has_permission("rogue_admin@example.com", Permission::Review));
    }

    #[test]
    fn test_acl_blocklist_case_insensitivity() {
        let acl = AclSettings {
            admins: vec!["User@Example.COM".to_string()],
            blocklist: vec!["User@Example.COM".to_string()],
            ..Default::default()
        };

        assert!(acl.is_blocklisted("user@example.com"));
        assert!(acl.is_blocklisted("USER@EXAMPLE.COM"));
        assert!(acl.is_blocklisted("uSeR@eXaMpLe.CoM"));
        assert!(!acl.has_permission("user@example.com", Permission::Review));
        assert!(!acl.has_permission("USER@EXAMPLE.COM", Permission::Review));
    }

    #[test]
    fn test_acl_deserialization_with_blocklist() {
        let toml_blocklist = r#"
            admins = ["alice@example.com"]
            blocklist = ["mallory@example.com"]
        "#;
        let acl: AclSettings = toml::from_str(toml_blocklist).expect("deserialization failed");
        assert_eq!(acl.blocklist, vec!["mallory@example.com"]);
        assert!(acl.is_blocklisted("mallory@example.com"));
    }

    #[test]
    fn test_security_list_grants_no_capabilities() {
        let acl = AclSettings {
            security: vec!["gregkh@linuxfoundation.org".to_string()],
            ..Default::default()
        };
        assert!(acl.is_security("gregkh@linuxfoundation.org"));
        // Membership is about bugs. It must not leak into the capabilities
        // that spend money or move patches around.
        for perm in [Permission::Ingest, Permission::Cancel, Permission::Review] {
            assert!(!acl.has_permission("gregkh@linuxfoundation.org", perm));
        }
        assert!(!acl.is_admin("gregkh@linuxfoundation.org"));
        assert!(!acl.is_bug_reporter("gregkh@linuxfoundation.org"));
    }

    #[test]
    fn test_bug_reporters_defaults_to_nobody() {
        let acl = AclSettings::default();
        assert!(!acl.is_bug_reporter("tool@example.com"));

        let acl = AclSettings {
            bug_reporters: vec!["tool@example.com".to_string()],
            ..Default::default()
        };
        assert!(acl.is_bug_reporter("tool@example.com"));
        assert!(!acl.is_security("tool@example.com"));
    }

    #[test]
    fn test_every_configured_list_may_sign_in() {
        let acl = AclSettings {
            admins: vec!["operator@example.org".to_string()],
            security: vec!["gregkh@linuxfoundation.org".to_string()],
            bug_reporters: vec!["tool@example.org".to_string()],
            ingest: vec!["bot@example.org".to_string()],
            cancel: vec!["cron@example.org".to_string()],
            review: vec!["reviewer@example.org".to_string()],
            blocklist: vec!["mallory@example.org".to_string()],
        };

        // An address that can do something has to be able to sign in and do
        // it, whichever list put it there.
        for known in [
            "operator@example.org",
            "GregKH@LinuxFoundation.org",
            "tool@example.org",
            "bot@example.org",
            "cron@example.org",
            " reviewer@example.org ",
        ] {
            assert!(acl.is_known_identity(known), "{} cannot sign in", known);
        }

        assert!(!acl.is_known_identity("mallory@example.org"));
        assert!(!acl.is_known_identity("stranger@example.org"));
        assert!(!AclSettings::default().is_known_identity("anyone@example.org"));
    }

    #[test]
    fn test_list_matching_tolerates_surrounding_whitespace() {
        let acl = AclSettings {
            security: vec![" gregkh@linuxfoundation.org ".to_string()],
            blocklist: vec!["  mallory@example.com".to_string()],
            ..Default::default()
        };
        assert!(acl.is_security("gregkh@linuxfoundation.org"));
        // A space in the configuration file must not let a denied address
        // through.
        assert!(acl.is_blocklisted("mallory@example.com"));
        assert!(acl.is_blocklisted(" mallory@example.com "));
    }

    #[test]
    fn test_acl_rejects_unknown_keys() {
        // The lists are the whole security model, so a typo has to be loud.
        let toml = r#"
            admins = ["alice@example.com"]
            securty = ["typo@example.com"]
        "#;
        assert!(toml::from_str::<AclSettings>(toml).is_err());
    }

    #[test]
    fn test_reachable_base_url_rejects_bind_addresses() {
        for good in [
            "https://sashiko.example.org",
            "https://sashiko.example.org/",
            "http://review.example.org:8080",
            "https://[2001:db8::1]:8443",
        ] {
            assert!(is_reachable_base_url(good), "{} rejected", good);
        }
        // A bind address, a loopback address and a bare host are all things a
        // recipient on another machine cannot open.
        for bad in [
            "http://::8080",
            "http://[::]:8080",
            "http://0.0.0.0:8080",
            "https://localhost:8080",
            "http://127.0.0.1:8080",
            "sashiko.example.org",
            "ftp://sashiko.example.org",
            "",
        ] {
            assert!(!is_reachable_base_url(bad), "{} accepted", bad);
        }
    }

    #[test]
    fn test_sign_in_base_url_drops_the_trailing_slash() {
        let mut server = ServerSettings {
            host: "::".to_string(),
            port: 8080,
            public_base_url: Some("https://sashiko.example.org/".to_string()),
            read_only: false,
            testing_mode: false,
            jwt_secret: None,
            log_sign_in_links: false,
            acl: AclSettings::default(),
        };
        assert_eq!(server.sign_in_base_url(), "https://sashiko.example.org");

        // With nothing configured the link never leaves the machine, so a
        // best-effort address is enough.
        server.public_base_url = None;
        assert_eq!(server.sign_in_base_url(), "http://:::8080");
    }

    #[test]
    fn test_sign_in_link_logging_is_off_unless_asked_for() {
        // A configuration that never mentions the switch must not print
        // credentials, because that is the configuration everyone deploys.
        let server: ServerSettings = toml::from_str("host = \"::\"\nport = 8080").unwrap();
        assert!(!server.log_sign_in_links);

        let opted_in: ServerSettings =
            toml::from_str("host = \"::\"\nport = 8080\nlog_sign_in_links = true").unwrap();
        assert!(opted_in.log_sign_in_links);
    }

    #[test]
    fn test_startup_refuses_to_mail_links_nobody_can_open() {
        let mut settings = Settings::new().unwrap();

        // Shipped configuration has no SMTP, so the link is logged and the
        // base URL is nobody's problem.
        assert!(settings.smtp.is_none());
        assert!(settings.validate_sign_in_delivery().is_ok());

        settings.smtp = Some(SmtpSettings {
            transport: MailTransport::Smtp,
            server: Some("smtp.example.org".to_string()),
            port: Some(587),
            username: None,
            password: None,
            sendmail_path: None,
            sender_address: "sashiko@example.org".to_string(),
            reply_to: None,
            dry_run: true,
        });
        assert!(settings.validate_sign_in_delivery().is_err());

        settings.server.public_base_url = Some("http://[::]:8080".to_string());
        assert!(settings.validate_sign_in_delivery().is_err());

        settings.server.public_base_url = Some("https://sashiko.example.org".to_string());
        assert!(settings.validate_sign_in_delivery().is_ok());
    }

    #[test]
    fn test_local_token_path_follows_the_database() {
        let mut settings = Settings::new().unwrap();

        settings.database.url = "sashiko.db".to_string();
        assert_eq!(
            settings.local_token_path(),
            Path::new(".").join(LOCAL_TOKEN_FILE_NAME)
        );

        settings.database.url = "/var/lib/sashiko/sashiko.db".to_string();
        assert_eq!(
            settings.local_token_path(),
            Path::new("/var/lib/sashiko").join(LOCAL_TOKEN_FILE_NAME)
        );

        // A remote database names no directory to share, so the token sits
        // where the configuration was read from instead.
        settings.database.url = "libsql://sashiko.example.turso.io".to_string();
        assert_eq!(
            settings.local_token_path(),
            Path::new(".").join(LOCAL_TOKEN_FILE_NAME)
        );
    }

    /// An environment variable is always a string, so a list spelled that way
    /// has to mean the same thing as the array a file writes.
    #[test]
    fn test_acl_lists_read_a_string_as_well_as_an_array() {
        let from_env: AclSettings = serde_json::from_str(
            r#"{"admins": "first@example.org, second@example.org", "security": ""}"#,
        )
        .unwrap();
        assert_eq!(from_env.admins, ["first@example.org", "second@example.org"]);
        assert!(
            from_env.security.is_empty(),
            "an empty variable grants nothing"
        );

        let from_file: AclSettings =
            serde_json::from_str(r#"{"admins": ["first@example.org"]}"#).unwrap();
        assert_eq!(from_file.admins, ["first@example.org"]);

        // An omitted list stays fail-closed rather than becoming a list with
        // one blank member that matches a caller presenting no address.
        let omitted: AclSettings = serde_json::from_str("{}").unwrap();
        assert!(omitted.admins.is_empty());
        assert!(!omitted.is_admin(""));
    }

    #[test]
    fn test_linux_bug_settings_defaults_to_disabled() {
        let default_bug = LinuxBugSettings::default();
        assert!(!default_bug.enabled);
        assert_eq!(default_bug.concurrency, 4);
        assert!(!default_bug.fix_check_enabled);
        assert_eq!(default_bug.lease_ttl_seconds, 300);
        assert_eq!(default_bug.max_attempts, 3);
        assert_eq!(default_bug.fix_check_interval_seconds, 21_600);
        assert_eq!(default_bug.fix_check_batch_size, 50);

        let settings = Settings::new().unwrap();
        assert!(!settings.linux_bug.enabled);
        assert_eq!(settings.linux_bug.concurrency, 4);
        assert!(!settings.linux_bug.fix_check_enabled);

        let custom: LinuxBugSettings = toml::from_str(
            "enabled = true\nconcurrency = 8\nfix_check_enabled = true\nfix_check_interval_seconds = 3600\nfix_check_batch_size = 20\n",
        )
        .unwrap();
        assert!(custom.enabled);
        assert_eq!(custom.concurrency, 8);
        assert!(custom.fix_check_enabled);
        assert_eq!(custom.fix_check_interval_seconds, 3600);
        assert_eq!(custom.fix_check_batch_size, 20);
    }
}
