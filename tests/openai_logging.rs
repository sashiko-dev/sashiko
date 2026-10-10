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

#![cfg(feature = "server")]

use anyhow::Result;
use sashiko::ai::openai::{OpenAiApiType, OpenAiCompatClient, OpenAiProviderType};
use sashiko::ai::{self, AiProvider, AiRequest};
use serde_json::json;

#[tokio::test]
async fn regression_request_and_usage_logs_include_patch_context() -> Result<()> {
    use axum::{Json, Router, routing::post};
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::prelude::*;
    let logs = tempfile::NamedTempFile::new()?;
    let writer = logs.reopen()?;
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.try_clone().unwrap())
            .with_filter(tracing_subscriber::filter::dynamic_filter_fn(|_, _| true)),
    );
    let app = Router::new()
        .route("/v1/chat/completions", post(|| async { Json(json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120,
                "prompt_tokens_details": {"cached_tokens": 101}}
        })) }))
        .route("/v1/responses", post(|| async { Json(json!({
            "status": "completed", "output": [],
            "usage": {"input_tokens": 100, "output_tokens": 20, "total_tokens": 120,
                "input_tokens_details": {"cached_tokens": 101}}
        })) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let result: Result<()> = ai::LOG_CONTEXT
        .scope("[patch-review] ".into(), async {
            for api in [OpenAiApiType::Chat, OpenAiApiType::Responses] {
                let client = OpenAiCompatClient::new(
                    url.clone(),
                    OpenAiProviderType::OpenAi,
                    api,
                    "test".into(),
                    4096,
                    128,
                    5,
                    None,
                )?;
                client
                    .generate_content(AiRequest {
                        system: None,
                        messages: vec![],
                        tools: None,
                        temperature: None,
                        response_format: None,
                        context_tag: None,
                    })
                    .await?;
            }
            Ok(())
        })
        .with_subscriber(subscriber)
        .await;
    server.abort();
    result?;
    let recorded = std::fs::read_to_string(logs.path())?;
    let relevant: Vec<_> = recorded
        .lines()
        .filter(|line| line.contains("Sending OpenAI") || line.contains("Tokens:"))
        .collect();
    assert_eq!(relevant.len(), 4, "{recorded}");
    for line in relevant {
        assert!(line.contains("[patch-review] "), "{line}");
        if line.contains("Tokens:") {
            assert!(line.contains("in=100, cached=0, out=20"), "{line}");
        }
    }
    Ok(())
}
