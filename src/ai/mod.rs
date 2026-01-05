use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct AiRequest {
    pub model: String,
    pub prompt: String,
    pub system_prompt: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct AiResponse {
    pub content: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
}

#[async_trait]
pub trait AiProvider: Send + Sync {
    #[allow(dead_code)]
    async fn completion(&self, request: AiRequest) -> Result<AiResponse>;
}

#[allow(dead_code)]
pub struct OpenAiProvider {
    api_key: String,
    client: reqwest::Client,
}

#[allow(dead_code)]
impl OpenAiProvider {
    pub fn new() -> Self {
        let api_key = std::env::var("LLM_API_KEY").unwrap_or_default();
        Self {
            api_key,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
#[allow(dead_code)]
impl AiProvider for OpenAiProvider {
    async fn completion(&self, request: AiRequest) -> Result<AiResponse> {
        let url = "https://api.openai.com/v1/chat/completions";

        let mut messages = Vec::new();
        if let Some(system) = request.system_prompt {
            messages.push(serde_json::json!({
                "role": "system",
                "content": system
            }));
        }
        messages.push(serde_json::json!({
            "role": "user",
            "content": request.prompt
        }));

        let body = serde_json::json!({
            "model": request.model,
            "messages": messages
        });

        let res = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await?;

        let json: serde_json::Value = res.json().await?;

        let content = json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        let tokens_in = json["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32;
        let tokens_out = json["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32;

        Ok(AiResponse {
            content,
            tokens_in,
            tokens_out,
        })
    }
}
pub mod gemini;
