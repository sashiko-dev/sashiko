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

use super::*;
use crate::ai::{AiMessage, AiResponseFormat};
use serde_json::json;

fn request(response_format: Option<AiResponseFormat>) -> AiRequest {
    AiRequest {
        system: None,
        messages: vec![AiMessage {
            role: AiRole::User,
            content: Some("Return the answer.".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        tools: None,
        temperature: Some(0.5),
        response_format,
        context_tag: None,
        reasoning_effort: None,
        conversation_id: None,
    }
}

#[test]
fn response_format_is_serialized_at_the_request_root() -> Result<()> {
    let schema = json!({
        "type": "object",
        "properties": {"answer": {"type": "integer"}},
        "required": ["answer"],
        "additionalProperties": false
    });
    let cases = [
        ("default", None, None),
        ("text", Some(AiResponseFormat::Text), None),
        (
            "json",
            Some(AiResponseFormat::Json { schema: None }),
            Some(json!("json")),
        ),
        (
            "schema",
            Some(AiResponseFormat::Json {
                schema: Some(schema.clone()),
            }),
            Some(schema),
        ),
    ];

    for (label, format, expected) in cases {
        let translated = translate_ollama_request(request(format), 128_000, 2048, None)?;
        let body = serde_json::to_value(translated)?;

        assert_eq!(body.get("format"), expected.as_ref(), "{label}");
        assert_eq!(
            body["options"],
            json!({"temperature": 0.5, "num_ctx": 128_000, "num_predict": 2048}),
            "{label}: format belongs outside runtime options"
        );
        assert_eq!(body["stream"], false);
    }
    Ok(())
}

#[cfg(feature = "server")]
#[tokio::test]
async fn generate_content_sends_the_requested_schema() -> Result<()> {
    use axum::{Json, Router, routing::post};

    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let app = Router::new().route(
        "/api/chat",
        post(move |Json(body): Json<Value>| {
            let tx = tx.clone();
            async move {
                tx.send(body).await.unwrap();
                Json(json!({
                    "message": {"role": "assistant", "content": "{\"answer\":42}"},
                    "total_duration": 1,
                    "load_duration": 1,
                    "prompt_eval_count": 3,
                    "prompt_eval_duration": 1,
                    "eval_count": 4,
                    "eval_duration": 1
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let client = OllamaClient {
        client: crate::ai::test_http::test_client(),
        ..OllamaClient::new(
            format!("http://{}", listener.local_addr()?),
            "test-model".to_string(),
            128_000,
            2048,
            5,
            None,
        )?
    };
    let schema = json!({
        "type": "object",
        "properties": {"answer": {"type": "integer"}},
        "required": ["answer"]
    });
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let response = client
        .generate_content(request(Some(AiResponseFormat::Json {
            schema: Some(schema.clone()),
        })))
        .await;
    server.abort();
    let _ = server.await;

    assert_eq!(response?.content.as_deref(), Some("{\"answer\":42}"));
    let body = rx.recv().await.expect("provider did not POST a request");
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["format"], schema);
    assert!(body["options"].get("format").is_none());
    Ok(())
}
