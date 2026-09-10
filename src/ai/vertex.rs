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

//! Vertex AI provider — a model-agnostic routing layer for Google Cloud.
//!
//! Vertex AI hosts multiple model families (Claude, Gemini, etc.) behind
//! different API endpoints. This provider handles shared concerns (auth,
//! endpoint routing) and delegates wire-format translation to existing
//! provider modules based on the detected model family.
//!
//! Currently supports Claude models via the `rawPredict` endpoint.
//! Gemini support can be added by making gemini.rs translation functions
//! public and adding the Gemini path — no structural changes required.

use crate::ai::claude::{
    self, ClaudeError, ClaudeMessage, ClaudeResponse, ClaudeTool, SystemBlock, ThinkingConfig,
};
use crate::ai::{AiProvider, AiRequest, AiResponse, ProviderCapabilities};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use google_cloud_auth::credentials::{AccessTokenCredentials, Builder};
use reqwest::Client;
use serde::Serialize;
use std::time::Duration;
use tracing::info;

// --- Model family detection ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelFamily {
    Claude,
    // Future: Gemini, Llama, Mistral, etc.
}

fn detect_model_family(model: &str) -> Result<ModelFamily> {
    if model.starts_with("claude") {
        Ok(ModelFamily::Claude)
    } else {
        bail!(
            "Unsupported model family on Vertex AI: {}. \
             Supported prefixes: claude-*",
            model
        )
    }
}

// --- Vertex-specific request wrapper for Claude ---

/// Claude request body for Vertex AI's rawPredict endpoint.
///
/// Differs from direct Claude API in two ways:
/// 1. No `model` field — Vertex gets it from the URL
/// 2. `anthropic_version` is in the body (not a header)
#[derive(Debug, Serialize)]
struct VertexClaudeRequest {
    anthropic_version: String,
    messages: Vec<ClaudeMessage>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<SystemBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ClaudeTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingConfig>,
}

// --- Endpoint construction ---

struct EndpointInfo {
    publisher: &'static str,
    method: &'static str,
}

fn endpoint_info(family: ModelFamily) -> EndpointInfo {
    match family {
        ModelFamily::Claude => EndpointInfo {
            publisher: "anthropic",
            method: "rawPredict",
        },
    }
}

fn build_endpoint_url(region: &str, project_id: &str, model: &str, info: &EndpointInfo) -> String {
    let path = format!(
        "v1/projects/{}/locations/{}/publishers/{}/models/{}:{}",
        project_id, region, info.publisher, model, info.method
    );

    match region {
        "global" => format!("https://global-aiplatform.googleapis.com/{path}"),
        "us" | "eu" => {
            format!("https://aiplatform.{region}.rep.googleapis.com/{path}")
        }
        _ => format!("https://{region}-aiplatform.googleapis.com/{path}"),
    }
}

// --- VertexClient ---

pub struct VertexClient {
    project_id: String,
    region: String,
    model: String,
    model_family: ModelFamily,
    client: Client,
    enable_caching: bool,
    max_tokens: u32,
    thinking: Option<String>,
    effort: Option<String>,
    context_window_size: usize,
    credentials: AccessTokenCredentials,
}

impl VertexClient {
    pub fn new(
        model: String,
        project_id: String,
        region: String,
        enable_caching: bool,
        max_tokens: u32,
        thinking: Option<String>,
        effort: Option<String>,
    ) -> Result<Self> {
        let model_family = detect_model_family(&model)?;

        let context_window_size = match model_family {
            ModelFamily::Claude => {
                // Opus 4.7/4.6 and Sonnet 4.6 get 1M on Vertex
                if model.contains("opus-4-7")
                    || model.contains("opus-4-6")
                    || model.contains("sonnet-4-6")
                {
                    1_000_000
                } else {
                    200_000
                }
            }
        };

        let credentials = Builder::default()
            .build_access_token_credentials()
            .map_err(|e| anyhow::anyhow!("Failed to initialize Google Cloud credentials: {e}"))?;

        Ok(Self {
            project_id,
            region,
            model,
            model_family,
            client: Client::new(),
            enable_caching,
            max_tokens,
            thinking,
            effort,
            context_window_size,
            credentials,
        })
    }

    async fn get_access_token(&self) -> Result<String> {
        let token = self
            .credentials
            .access_token()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to get Google Cloud access token: {e}"))?;
        Ok(token.token)
    }

    fn endpoint_url(&self) -> String {
        let info = endpoint_info(self.model_family);
        build_endpoint_url(&self.region, &self.project_id, &self.model, &info)
    }

    async fn post_claude_request(&self, body: &VertexClaudeRequest) -> Result<ClaudeResponse> {
        let token = self.get_access_token().await?;
        let url = self.endpoint_url();

        let res = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .context("Failed to send request to Vertex AI")?;

        let status = res.status();

        if status.is_success() {
            let body_text = res.text().await?;
            let response: ClaudeResponse = serde_json::from_str(&body_text)
                .context("Failed to parse Vertex AI Claude response")?;

            info!(
                "Vertex AI response received. Tokens: in={}, out={}, cache_read={} cache_write={}",
                response.usage.input_tokens,
                response.usage.output_tokens,
                response.usage.cache_read_input_tokens.unwrap_or(0),
                response.usage.cache_creation_input_tokens.unwrap_or(0),
            );

            Ok(response)
        } else {
            let retry_after_duration = res
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(Duration::from_secs);

            let error_body = res
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());

            match status.as_u16() {
                429 => {
                    let duration = retry_after_duration.unwrap_or(Duration::from_secs(60));
                    Err(ClaudeError::RateLimitExceeded(duration))?
                }
                500..=599 => {
                    let duration = retry_after_duration.unwrap_or(Duration::from_secs(0));
                    Err(ClaudeError::OverloadedError(duration))?
                }
                400 => Err(ClaudeError::InvalidRequest(error_body))?,
                401 | 403 => Err(ClaudeError::AuthenticationError(error_body))?,
                _ => Err(ClaudeError::ApiError(status, error_body))?,
            }
        }
    }

    async fn generate_claude(&self, request: AiRequest) -> Result<AiResponse> {
        let claude_req = claude::translate_ai_request(
            &request,
            self.enable_caching,
            self.max_tokens,
            self.thinking.clone(),
            self.effort.clone(),
        )?;

        let vertex_req = VertexClaudeRequest {
            anthropic_version: "vertex-2023-10-16".to_string(),
            messages: claude_req.messages,
            max_tokens: claude_req.max_tokens,
            system: claude_req.system,
            tools: claude_req.tools,
            thinking: claude_req.thinking,
        };

        let response = self.post_claude_request(&vertex_req).await?;
        claude::translate_ai_response(&response)
    }
}

#[async_trait]
impl AiProvider for VertexClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        match self.model_family {
            ModelFamily::Claude => self.generate_claude(request).await,
        }
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        claude::estimate_tokens_generic(request)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model.clone(),
            context_window_size: self.context_window_size,
        }
    }

    fn cache_identity(&self) -> String {
        // max_tokens is what truncates a response, so a raised limit has to
        // miss the entry recorded under the lower one rather than replay it.
        let max_tokens = self.max_tokens.to_string();
        crate::ai::cache_identity_with(
            &self.model,
            &[
                ("thinking", self.thinking.as_deref()),
                ("effort", self.effort.as_deref()),
                ("max_tokens", Some(max_tokens.as_str())),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // --- Model family detection ---

    #[test]
    fn test_detect_model_family_claude() {
        assert_eq!(
            detect_model_family("claude-sonnet-4-6").unwrap(),
            ModelFamily::Claude
        );
        assert_eq!(
            detect_model_family("claude-opus-4-7").unwrap(),
            ModelFamily::Claude
        );
    }

    #[test]
    fn test_detect_model_family_unsupported() {
        assert!(detect_model_family("llama-3").is_err());
        assert!(detect_model_family("mistral-large").is_err());
    }

    // --- Endpoint URL construction ---

    #[test]
    fn test_endpoint_url_global_claude() {
        let info = endpoint_info(ModelFamily::Claude);
        let url = build_endpoint_url("global", "my-project", "claude-sonnet-4-6", &info);
        assert_eq!(
            url,
            "https://global-aiplatform.googleapis.com/v1/projects/my-project/locations/global/publishers/anthropic/models/claude-sonnet-4-6:rawPredict"
        );
    }

    #[test]
    fn test_endpoint_url_regional_claude() {
        let info = endpoint_info(ModelFamily::Claude);
        let url = build_endpoint_url("us-east5", "my-project", "claude-sonnet-4-6", &info);
        assert_eq!(
            url,
            "https://us-east5-aiplatform.googleapis.com/v1/projects/my-project/locations/us-east5/publishers/anthropic/models/claude-sonnet-4-6:rawPredict"
        );
    }

    #[test]
    fn test_endpoint_url_multi_region_us() {
        let info = endpoint_info(ModelFamily::Claude);
        let url = build_endpoint_url("us", "my-project", "claude-opus-4-7", &info);
        assert_eq!(
            url,
            "https://aiplatform.us.rep.googleapis.com/v1/projects/my-project/locations/us/publishers/anthropic/models/claude-opus-4-7:rawPredict"
        );
    }

    #[test]
    fn test_endpoint_url_multi_region_eu() {
        let info = endpoint_info(ModelFamily::Claude);
        let url = build_endpoint_url("eu", "my-project", "claude-sonnet-4-6", &info);
        assert_eq!(
            url,
            "https://aiplatform.eu.rep.googleapis.com/v1/projects/my-project/locations/eu/publishers/anthropic/models/claude-sonnet-4-6:rawPredict"
        );
    }

    // --- VertexClaudeRequest serialization ---

    #[test]
    fn test_vertex_claude_request_no_model_field() {
        let req = VertexClaudeRequest {
            anthropic_version: "vertex-2023-10-16".to_string(),
            messages: vec![],
            max_tokens: 4096,
            system: None,
            tools: None,
            thinking: None,
        };

        let json = serde_json::to_value(&req).unwrap();
        let obj = json.as_object().unwrap();
        assert!(!obj.contains_key("model"));
    }

    #[test]
    fn test_vertex_claude_request_has_anthropic_version() {
        let req = VertexClaudeRequest {
            anthropic_version: "vertex-2023-10-16".to_string(),
            messages: vec![],
            max_tokens: 4096,
            system: None,
            tools: None,
            thinking: None,
        };

        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["anthropic_version"], "vertex-2023-10-16");
    }

    #[test]
    fn test_vertex_claude_request_from_translate() {
        use crate::ai::{AiMessage, AiRole};

        let ai_req = AiRequest {
            system: Some("You are helpful.".to_string()),
            messages: vec![AiMessage {
                role: AiRole::User,
                content: Some("Hello".to_string()),
                thought: None,
                thought_signature: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        };

        let claude_req = claude::translate_ai_request(&ai_req, false, 4096, None, None).unwrap();

        let vertex_req = VertexClaudeRequest {
            anthropic_version: "vertex-2023-10-16".to_string(),
            messages: claude_req.messages,
            max_tokens: claude_req.max_tokens,
            system: claude_req.system,
            tools: claude_req.tools,
            thinking: claude_req.thinking,
        };

        let json = serde_json::to_value(&vertex_req).unwrap();
        assert_eq!(json["anthropic_version"], "vertex-2023-10-16");
        assert!(!json.as_object().unwrap().contains_key("model"));
        assert!(json["system"].is_array());
        assert!(json["messages"].is_array());
        assert_eq!(json["max_tokens"], 4096);
        // thinking should be absent (both None)
        assert!(!json.as_object().unwrap().contains_key("thinking"));
    }

    // --- Context window detection ---

    #[test]
    fn test_context_window_1m_for_new_models() {
        // Can't fully test new() without GCP credentials, so test the logic directly
        let models_1m = ["claude-opus-4-7", "claude-opus-4-6", "claude-sonnet-4-6"];
        for model in models_1m {
            let is_1m = model.contains("opus-4-7")
                || model.contains("opus-4-6")
                || model.contains("sonnet-4-6");
            assert!(is_1m, "Expected 1M context for {model}");
        }
    }

    #[test]
    fn test_context_window_200k_for_older_models() {
        let models_200k = [
            "claude-sonnet-4-5@20250929",
            "claude-sonnet-4@20250514",
            "claude-haiku-4-5@20251001",
        ];
        for model in models_200k {
            let is_1m = model.contains("opus-4-7")
                || model.contains("opus-4-6")
                || model.contains("sonnet-4-6");
            assert!(!is_1m, "Expected 200K context for {model}");
        }
    }
}
