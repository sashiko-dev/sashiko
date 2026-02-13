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

use crate::ai::{LegacyAiProvider, LegacyAiRequest, LegacyAiResponse};
use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::sleep;

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Content {
    pub role: String,
    pub parts: Vec<Part>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(untagged)]
pub enum Part {
    Text {
        text: String,
        #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
        #[serde(default)]
        thought: bool,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: FunctionCall,
        #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: FunctionResponse,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FunctionCall {
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FunctionResponse {
    pub name: String,
    pub response: Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub function_declarations: Vec<FunctionDeclaration>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FunctionDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: Value, // JSON Schema
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentRequest {
    pub contents: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GenerationConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingConfig {
    pub include_thoughts: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<ThinkingConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentResponse {
    pub candidates: Option<Vec<Candidate>>,
    pub usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub content: Content,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageMetadata {
    pub prompt_token_count: u32,
    pub candidates_token_count: Option<u32>,
    pub total_token_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_content_token_count: Option<u32>,
    #[serde(flatten)]
    pub extra: Option<std::collections::HashMap<String, Value>>,
}

// --- Caching API Types ---

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CachedContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contents: Option<Vec<Content>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expire_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateCachedContentRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contents: Option<Vec<Content>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentWithCacheRequest {
    pub cached_content: String, // Resource name
    pub contents: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GenerationConfig>,
}

#[async_trait]
pub trait GenAiClient: Send + Sync {
    async fn generate_content(
        &self,
        request: GenerateContentRequest,
    ) -> Result<GenerateContentResponse>;

    async fn create_cached_content(
        &self,
        request: CreateCachedContentRequest,
    ) -> Result<CachedContent>;

    async fn list_cached_contents(&self) -> Result<Vec<CachedContent>>;

    async fn delete_cached_content(&self, name: &str) -> Result<()>;

    async fn generate_content_with_cache(
        &self,
        request: GenerateContentWithCacheRequest,
    ) -> Result<GenerateContentResponse>;
}

#[derive(Debug)]
pub enum GeminiError {
    QuotaExceeded(Duration),
    PermissionDenied(String),
    ApiError(reqwest::StatusCode, String),
    Other(String),
}

impl std::fmt::Display for GeminiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GeminiError::QuotaExceeded(d) => write!(f, "Quota exceeded, retry in {:?}", d),
            GeminiError::PermissionDenied(msg) => write!(f, "Permission denied: {}", msg),
            GeminiError::ApiError(s, msg) => write!(f, "Gemini API error ({}): {}", s, msg),
            GeminiError::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for GeminiError {}

pub struct GeminiClient {
    api_key: String,
    model: String,
    client: Client,
}

impl GeminiClient {
    pub fn new(model: String) -> Self {
        let api_key = std::env::var("LLM_API_KEY").unwrap_or_default();
        Self {
            api_key,
            model,
            client: Client::new(),
        }
    }

    pub async fn generate_content_single(
        &self,
        request: &GenerateContentRequest,
    ) -> Result<GenerateContentResponse> {
        tracing::info!("Sending Gemini request...");

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, self.api_key
        );
        self.post_request(&url, request).await
    }

    pub async fn generate_content_with_cache_single(
        &self,
        request: &GenerateContentWithCacheRequest,
    ) -> Result<GenerateContentResponse> {
        tracing::info!("Sending Gemini request (cached)...");

        // When using cached content, the URL model parameter is effectively ignored by the backend
        // in favor of the 'cached_content' field, but we still need a valid endpoint.
        // The documentation says: POST https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, self.api_key
        );
        self.post_request(&url, request).await
    }

    async fn post_request<T: Serialize>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<GenerateContentResponse> {
        let re = Regex::new(r"Please retry in ([0-9.]+)s").unwrap();
        let res = self.client.post(url).json(body).send().await;

        if let Err(e) = &res {
            tracing::error!("Gemini request failed (transport): {}", e);
        }
        let res = res?;

        if res.status().is_success() {
            let body_text = res.text().await?;
            match serde_json::from_str::<GenerateContentResponse>(&body_text) {
                Ok(response) => {
                    if let Some(usage) = &response.usage_metadata {
                        tracing::info!(
                            "Gemini response received. Tokens: in={}, cached={}, out={}",
                            usage.prompt_token_count,
                            usage.cached_content_token_count.unwrap_or(0),
                            usage.candidates_token_count.unwrap_or(0)
                        );
                    } else {
                        tracing::info!("Gemini response received. No usage metadata.");
                    }
                    return Ok(response);
                }
                Err(e) => {
                    tracing::error!("Failed to decode Gemini response: {}", e);
                    anyhow::bail!("Failed to decode response: {}. Body: {}", e, body_text);
                }
            }
        }

        if res.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after_header = res
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse::<f64>().ok());

            let error_text = res.text().await?;
            let retry_seconds = if let Some(secs) = retry_after_header {
                secs
            } else if let Some(caps) = re.captures(&error_text) {
                caps[1].parse::<f64>().unwrap_or(30.0)
            } else {
                30.0
            };
            tracing::warn!(
                "Gemini 429 Quota Exceeded. Retry suggested in {}s. Body: {}",
                retry_seconds,
                error_text
            );
            return Err(
                GeminiError::QuotaExceeded(Duration::from_secs_f64(retry_seconds + 1.0)).into(),
            );
        }

        let status = res.status();
        let error_text = res.text().await?;

        if status == reqwest::StatusCode::FORBIDDEN {
            tracing::error!("Gemini Permission Denied (403): {}", error_text);
            return Err(GeminiError::PermissionDenied(error_text).into());
        }

        tracing::error!("Gemini API Error: status={}, body={}", status, error_text);
        Err(GeminiError::ApiError(status, error_text).into())
    }
}

#[async_trait]
impl GenAiClient for GeminiClient {
    async fn generate_content(
        &self,
        request: GenerateContentRequest,
    ) -> Result<GenerateContentResponse> {
        loop {
            match self.generate_content_single(&request).await {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    if let Some(GeminiError::QuotaExceeded(sleep_duration)) =
                        e.downcast_ref::<GeminiError>()
                    {
                        tracing::warn!(
                            "Gemini API quota exceeded. Retrying in {:.2}s...",
                            sleep_duration.as_secs_f64()
                        );
                        sleep(*sleep_duration).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    async fn create_cached_content(
        &self,
        request: CreateCachedContentRequest,
    ) -> Result<CachedContent> {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/cachedContents?key={}",
            self.api_key
        );
        let res = self.client.post(&url).json(&request).send().await?;

        if res.status().is_success() {
            let body = res.text().await?;
            let content: CachedContent = serde_json::from_str(&body)?;
            Ok(content)
        } else {
            let status = res.status();
            let err = res.text().await?;
            anyhow::bail!("Failed to create cached content ({}): {}", status, err);
        }
    }

    async fn list_cached_contents(&self) -> Result<Vec<CachedContent>> {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/cachedContents?key={}",
            self.api_key
        );
        let res = self.client.get(&url).send().await?;

        if res.status().is_success() {
            let body = res.text().await?;
            #[derive(Deserialize)]
            struct ListResponse {
                #[serde(rename = "cachedContents")]
                cached_contents: Option<Vec<CachedContent>>,
            }
            let list: ListResponse = serde_json::from_str(&body)?;
            Ok(list.cached_contents.unwrap_or_default())
        } else {
            let status = res.status();
            let err = res.text().await?;
            anyhow::bail!("Failed to list cached contents ({}): {}", status, err);
        }
    }

    async fn delete_cached_content(&self, name: &str) -> Result<()> {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/{}?key={}",
            name, self.api_key
        );
        let res = self.client.delete(&url).send().await?;

        if res.status().is_success() {
            Ok(())
        } else {
            let status = res.status();
            let err = res.text().await?;
            anyhow::bail!("Failed to delete cached content ({}): {}", status, err);
        }
    }

    async fn generate_content_with_cache(
        &self,
        request: GenerateContentWithCacheRequest,
    ) -> Result<GenerateContentResponse> {
        loop {
            match self.generate_content_with_cache_single(&request).await {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    if let Some(GeminiError::QuotaExceeded(sleep_duration)) =
                        e.downcast_ref::<GeminiError>()
                    {
                        tracing::warn!(
                            "Gemini API quota exceeded (cache). Retrying in {:.2}s...",
                            sleep_duration.as_secs_f64()
                        );
                        sleep(*sleep_duration).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }
}

pub struct StdioGeminiClient;

#[async_trait]
impl GenAiClient for StdioGeminiClient {
    async fn generate_content(
        &self,
        request: GenerateContentRequest,
    ) -> Result<GenerateContentResponse> {
        let msg = json!({
            "type": "ai_request",
            "payload": request
        });
        self.exec_stdio(msg).await
    }

    async fn create_cached_content(
        &self,
        _request: CreateCachedContentRequest,
    ) -> Result<CachedContent> {
        anyhow::bail!("StdioGeminiClient does not support caching yet")
    }

    async fn list_cached_contents(&self) -> Result<Vec<CachedContent>> {
        Ok(vec![])
    }

    async fn delete_cached_content(&self, _name: &str) -> Result<()> {
        Ok(())
    }

    async fn generate_content_with_cache(
        &self,
        request: GenerateContentWithCacheRequest,
    ) -> Result<GenerateContentResponse> {
        let msg = json!({
            "type": "ai_request_with_cache",
            "payload": request
        });
        self.exec_stdio(msg).await
    }
}

impl StdioGeminiClient {
    async fn exec_stdio(&self, msg: Value) -> Result<GenerateContentResponse> {
        tokio::task::spawn_blocking(move || -> Result<GenerateContentResponse> {
            println!("{}", serde_json::to_string(&msg)?);
            use std::io::Write;
            std::io::stdout().flush()?;

            let stdin = std::io::stdin();
            let mut line = String::new();
            if stdin.read_line(&mut line)? == 0 {
                anyhow::bail!("Unexpected EOF from stdin waiting for AI response");
            }

            let resp_msg: Value = serde_json::from_str(&line)?;
            if resp_msg["type"] == "ai_response" {
                let payload = serde_json::from_value(resp_msg["payload"].clone())?;
                Ok(payload)
            } else if resp_msg["type"] == "error" {
                let err_msg = resp_msg["payload"].as_str().unwrap_or("Unknown error");
                anyhow::bail!("Remote AI Error: {}", err_msg)
            } else {
                anyhow::bail!("Unexpected response type: {:?}", resp_msg["type"])
            }
        })
        .await?
    }
}

#[async_trait]
impl LegacyAiProvider for GeminiClient {
    async fn completion(&self, request: LegacyAiRequest) -> Result<LegacyAiResponse> {
        // Implementation remains same, assuming AiRequest to GenerateContentRequest mapping
        // For brevity, I'll copy the existing logic or simpler:
        // Agent uses GenAiClient, so AiProvider might be legacy.
        // review.rs uses Agent.
        // Reviewer.rs uses AiProvider for DB logging.
        // reviewer.rs: `db.create_review(..., &settings.ai.provider, ...)`
        // `reviewer.rs` does NOT call `completion`.
        // AiProvider is maintained for compatibility.
        // It's used in `src/ai/mod.rs` trait definition.
        // `src/ai/gemini.rs` implemented it.
        // I will keep it implemented for `GeminiClient` to be safe.

        let contents = vec![Content {
            role: "user".to_string(),
            parts: vec![Part::Text {
                text: request.prompt,
                thought_signature: None,
                thought: false,
            }],
        }];

        let system_instruction = request.system_prompt.map(|s| Content {
            role: "user".to_string(),
            parts: vec![Part::Text {
                text: s,
                thought_signature: None,
                thought: false,
            }],
        });

        let gen_req = GenerateContentRequest {
            contents,
            tools: None,
            system_instruction,
            generation_config: None,
        };

        // Use the trait method
        let resp = GenAiClient::generate_content(self, gen_req).await?;

        let candidate = resp
            .candidates
            .as_ref()
            .and_then(|c| c.first())
            .ok_or_else(|| anyhow::anyhow!("No candidates returned from Gemini"))?;

        let mut content = String::new();
        for part in &candidate.content.parts {
            if let Part::Text { text, .. } = part {
                content.push_str(text);
            }
        }

        let usage = resp.usage_metadata.unwrap_or(UsageMetadata {
            prompt_token_count: 0,
            candidates_token_count: Some(0),
            total_token_count: 0,
            cached_content_token_count: None,
            extra: None,
        });

        Ok(LegacyAiResponse {
            content,
            tokens_in: usage.prompt_token_count,
            tokens_out: usage.candidates_token_count.unwrap_or(0),
            tokens_cached: usage.cached_content_token_count.unwrap_or(0),
        })
    }
}
