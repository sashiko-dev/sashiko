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

use crate::ai::token_budget::TokenBudget;
use crate::ai::{
    AiProvider, AiRequest, AiResponse, AiRole, AiUsage, ProviderCapabilities, ToolCall,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use aws_sdk_bedrockruntime::Client;
use aws_sdk_bedrockruntime::types::{
    ContentBlock, ConversationRole, InferenceConfiguration, Message, SystemContentBlock, Tool,
    ToolConfiguration, ToolInputSchema, ToolResultBlock, ToolResultContentBlock, ToolSpecification,
    ToolUseBlock,
};
use aws_smithy_types::{Document, Number};
use std::collections::HashMap;
use tracing::info;

// --- serde_json::Value <-> aws_smithy_types::Document conversion ---

fn json_to_document(value: &serde_json::Value) -> Document {
    match value {
        serde_json::Value::Null => Document::Null,
        serde_json::Value::Bool(b) => Document::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Document::Number(Number::PosInt(u))
            } else if let Some(i) = n.as_i64() {
                Document::Number(Number::NegInt(i))
            } else {
                Document::Number(Number::Float(n.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::String(s) => Document::String(s.clone()),
        serde_json::Value::Array(arr) => {
            Document::Array(arr.iter().map(json_to_document).collect())
        }
        serde_json::Value::Object(obj) => {
            let map: HashMap<String, Document> = obj
                .iter()
                .map(|(k, v)| (k.clone(), json_to_document(v)))
                .collect();
            Document::Object(map)
        }
    }
}

fn document_to_json(doc: &Document) -> serde_json::Value {
    match doc {
        Document::Null => serde_json::Value::Null,
        Document::Bool(b) => serde_json::Value::Bool(*b),
        Document::Number(n) => match n {
            Number::PosInt(u) => serde_json::json!(*u),
            Number::NegInt(i) => serde_json::json!(*i),
            Number::Float(f) => serde_json::json!(*f),
        },
        Document::String(s) => serde_json::Value::String(s.clone()),
        Document::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(document_to_json).collect())
        }
        Document::Object(obj) => {
            let map: serde_json::Map<String, serde_json::Value> = obj
                .iter()
                .map(|(k, v)| (k.clone(), document_to_json(v)))
                .collect();
            serde_json::Value::Object(map)
        }
    }
}

// --- Bedrock client ---

pub struct BedrockClient {
    client: tokio::sync::OnceCell<Client>,
    region: Option<String>,
    model_id: String,
    context_window_size: usize,
    max_tokens: u32,
    thinking: Option<String>,
    effort: Option<String>,
}

impl BedrockClient {
    pub fn new(
        model_id: String,
        region: Option<String>,
        max_tokens: u32,
        thinking: Option<String>,
        effort: Option<String>,
    ) -> Self {
        let context_window_size = if model_id.contains("claude") {
            200_000
        } else {
            128_000
        };

        Self {
            client: tokio::sync::OnceCell::new(),
            region,
            model_id,
            context_window_size,
            max_tokens,
            thinking,
            effort,
        }
    }

    async fn get_client(&self) -> &Client {
        self.client
            .get_or_init(|| async {
                let mut config_loader = aws_config::from_env();
                if let Some(r) = &self.region {
                    config_loader = config_loader.region(aws_config::Region::new(r.clone()));
                }
                let sdk_config = config_loader.load().await;
                Client::new(&sdk_config)
            })
            .await
    }
}

// --- Request translation ---

struct ConverseParams {
    messages: Vec<Message>,
    system: Option<Vec<SystemContentBlock>>,
    tool_config: Option<ToolConfiguration>,
    inference_config: Option<InferenceConfiguration>,
    additional_model_request_fields: Option<Document>,
}

fn build_additional_fields(thinking: Option<&str>, effort: Option<&str>) -> Option<Document> {
    let mut map: HashMap<String, Document> = HashMap::new();

    if let Some(t) = thinking {
        let mut thinking_obj: HashMap<String, Document> = HashMap::new();
        thinking_obj.insert("type".to_string(), Document::String(t.to_string()));
        map.insert("thinking".to_string(), Document::Object(thinking_obj));
    }

    if let Some(e) = effort {
        let mut output_cfg: HashMap<String, Document> = HashMap::new();
        output_cfg.insert("effort".to_string(), Document::String(e.to_string()));
        map.insert("output_config".to_string(), Document::Object(output_cfg));
    }

    if map.is_empty() {
        None
    } else {
        Some(Document::Object(map))
    }
}

/// Translate Sashiko's generic AiRequest into Bedrock Converse API parameters.
fn translate_request(
    request: &AiRequest,
    max_tokens: u32,
    thinking: Option<&str>,
    effort: Option<&str>,
) -> Result<ConverseParams> {
    let system = request
        .system
        .as_ref()
        .map(|s| vec![SystemContentBlock::Text(s.clone())]);

    let mut messages: Vec<Message> = Vec::new();

    // We need to merge consecutive Tool messages into a single User message
    // because Bedrock requires alternating user/assistant turns.
    let mut pending_tool_results: Vec<ContentBlock> = Vec::new();

    let flush_tool_results =
        |pending: &mut Vec<ContentBlock>, messages: &mut Vec<Message>| -> Result<()> {
            if pending.is_empty() {
                return Ok(());
            }
            let mut builder = Message::builder().role(ConversationRole::User);
            for block in pending.drain(..) {
                builder = builder.content(block);
            }
            messages.push(
                builder
                    .build()
                    .context("Failed to build tool result message")?,
            );
            Ok(())
        };

    for msg in &request.messages {
        match msg.role {
            AiRole::System => {} // handled above
            AiRole::User => {
                flush_tool_results(&mut pending_tool_results, &mut messages)?;
                let text = msg.content.clone().unwrap_or_default();
                messages.push(
                    Message::builder()
                        .role(ConversationRole::User)
                        .content(ContentBlock::Text(text))
                        .build()
                        .context("Failed to build user message")?,
                );
            }
            AiRole::Assistant => {
                flush_tool_results(&mut pending_tool_results, &mut messages)?;
                let mut builder = Message::builder().role(ConversationRole::Assistant);
                if let Some(text) = &msg.content {
                    builder = builder.content(ContentBlock::Text(text.clone()));
                }
                if let Some(tool_calls) = &msg.tool_calls {
                    for call in tool_calls {
                        let input_doc = json_to_document(&call.arguments);
                        builder = builder.content(ContentBlock::ToolUse(
                            ToolUseBlock::builder()
                                .tool_use_id(&call.id)
                                .name(&call.function_name)
                                .input(input_doc)
                                .build()
                                .context("Failed to build tool use block")?,
                        ));
                    }
                }
                messages.push(
                    builder
                        .build()
                        .context("Failed to build assistant message")?,
                );
            }
            AiRole::Tool => {
                let tool_call_id = msg
                    .tool_call_id
                    .as_ref()
                    .context("Tool message missing tool_call_id")?;
                let result_text = msg.content.clone().unwrap_or_else(|| "{}".to_string());
                pending_tool_results.push(ContentBlock::ToolResult(
                    ToolResultBlock::builder()
                        .tool_use_id(tool_call_id)
                        .content(ToolResultContentBlock::Text(result_text))
                        .build()
                        .context("Failed to build tool result block")?,
                ));
            }
        }
    }
    flush_tool_results(&mut pending_tool_results, &mut messages)?;

    let tool_config = request.tools.as_ref().and_then(|tools| {
        if tools.is_empty() {
            return None;
        }
        let bedrock_tools: Vec<Tool> = tools
            .iter()
            .filter_map(|t| {
                let schema_doc = json_to_document(&t.parameters);
                Some(Tool::ToolSpec(
                    ToolSpecification::builder()
                        .name(&t.name)
                        .description(&t.description)
                        .input_schema(ToolInputSchema::Json(schema_doc))
                        .build()
                        .ok()?,
                ))
            })
            .collect();
        ToolConfiguration::builder()
            .set_tools(Some(bedrock_tools))
            .build()
            .ok()
    });

    let inference_config = {
        let mut builder = InferenceConfiguration::builder().max_tokens(max_tokens as i32);
        if thinking.is_none() {
            if let Some(temp) = request.temperature {
                builder = builder.temperature(temp);
            }
        }
        Some(builder.build())
    };

    let additional_model_request_fields = build_additional_fields(thinking, effort);

    Ok(ConverseParams {
        messages,
        system,
        tool_config,
        inference_config,
        additional_model_request_fields,
    })
}

/// Translate Bedrock Converse API response into Sashiko's generic AiResponse.
fn translate_response(
    output: &aws_sdk_bedrockruntime::operation::converse::ConverseOutput,
) -> Result<AiResponse> {
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    if let Some(aws_sdk_bedrockruntime::types::ConverseOutput::Message(ref msg)) = output.output {
        for block in msg.content() {
            match block {
                ContentBlock::Text(t) => text_parts.push(t.clone()),
                ContentBlock::ToolUse(tu) => {
                    let args = document_to_json(tu.input());
                    tool_calls.push(ToolCall {
                        id: tu.tool_use_id().to_string(),
                        function_name: tu.name().to_string(),
                        arguments: args,
                        thought_signature: None,
                    });
                }
                _ => {}
            }
        }
    }

    let usage = output.usage.as_ref().map(|u| AiUsage {
        prompt_tokens: u.input_tokens() as usize,
        completion_tokens: u.output_tokens() as usize,
        total_tokens: (u.input_tokens() + u.output_tokens()) as usize,
        cached_tokens: None,
    });

    Ok(AiResponse {
        content: if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join(""))
        },
        thought: None,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        usage,
    })
}

fn estimate_tokens_generic(request: &AiRequest) -> usize {
    let mut total = 0;
    if let Some(system) = &request.system {
        total += TokenBudget::estimate_tokens(system);
    }
    for msg in &request.messages {
        if let Some(content) = &msg.content {
            total += TokenBudget::estimate_tokens(content);
        }
        if let Some(tool_calls) = &msg.tool_calls {
            for call in tool_calls {
                total += TokenBudget::estimate_tokens(&call.function_name);
                total += TokenBudget::estimate_tokens(&call.arguments.to_string());
            }
        }
    }
    if let Some(tools) = &request.tools {
        for tool in tools {
            total += TokenBudget::estimate_tokens(&tool.name);
            total += TokenBudget::estimate_tokens(&tool.description);
            total += TokenBudget::estimate_tokens(&tool.parameters.to_string());
        }
    }
    total
}

#[async_trait]
impl AiProvider for BedrockClient {
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
        let params = translate_request(
            &request,
            self.max_tokens,
            self.thinking.as_deref(),
            self.effort.as_deref(),
        )?;

        let resp = self
            .get_client()
            .await
            .converse()
            .model_id(&self.model_id)
            .set_messages(Some(params.messages))
            .set_system(params.system)
            .set_tool_config(params.tool_config)
            .set_inference_config(params.inference_config)
            .set_additional_model_request_fields(params.additional_model_request_fields)
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                let is_throttle = format!("{e:?}").contains("ThrottlingException");
                if is_throttle {
                    tracing::warn!("Bedrock throttled, waiting 30s before retry...");
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                }
                return Err(anyhow::anyhow!("Bedrock Converse API error: {e:#}"));
            }
        };

        let usage_str = resp
            .usage
            .as_ref()
            .map(|u| format!("in={}, out={}", u.input_tokens(), u.output_tokens()))
            .unwrap_or_else(|| "unknown".to_string());
        info!("Bedrock response received. Tokens: {}", usage_str);

        translate_response(&resp)
    }

    fn estimate_tokens(&self, request: &AiRequest) -> usize {
        estimate_tokens_generic(request)
    }

    fn get_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            model_name: self.model_id.clone(),
            context_window_size: self.context_window_size,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{AiMessage, AiRequest, AiRole, AiTool, ToolCall};
    use serde_json::json;

    fn make_request(messages: Vec<AiMessage>) -> AiRequest {
        AiRequest {
            system: None,
            messages,
            tools: None,
            temperature: None,
            response_format: None,
            context_tag: None,
        }
    }

    #[test]
    fn test_json_document_roundtrip() {
        let pi_value = std::f64::consts::PI;
        let original = json!({
            "name": "test",
            "count": 42,
            "negative": -7,
            "ratio": pi_value,
            "active": true,
            "nothing": null,
            "tags": ["a", "b"],
            "nested": {"x": 1}
        });
        let doc = json_to_document(&original);
        let back = document_to_json(&doc);
        assert_eq!(original, back);
    }

    #[test]
    fn test_translate_system_and_user() -> Result<()> {
        let mut req = make_request(vec![AiMessage {
            role: AiRole::User,
            content: Some("Hello!".to_string()),
            thought: None,
            tool_calls: None,
            tool_call_id: None,
        }]);
        req.system = Some("You are helpful.".to_string());
        req.temperature = Some(0.5);

        let params = translate_request(&req, 4096, None, None)?;

        let sys = params.system.unwrap();
        assert_eq!(sys.len(), 1);
        if let SystemContentBlock::Text(t) = &sys[0] {
            assert_eq!(t, "You are helpful.");
        } else {
            panic!("Expected Text system block");
        }

        assert_eq!(params.messages.len(), 1);
        assert_eq!(params.messages[0].role(), &ConversationRole::User);
        if let ContentBlock::Text(t) = &params.messages[0].content()[0] {
            assert_eq!(t, "Hello!");
        } else {
            panic!("Expected Text content block");
        }

        let ic = params.inference_config.unwrap();
        assert_eq!(ic.temperature(), Some(0.5));

        Ok(())
    }

    #[test]
    fn test_translate_assistant_with_tool_calls() -> Result<()> {
        let req = make_request(vec![AiMessage {
            role: AiRole::Assistant,
            content: Some("Let me check.".to_string()),
            thought: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                function_name: "git_log".to_string(),
                arguments: json!({"n": 5}),
                thought_signature: None,
            }]),
            tool_call_id: None,
        }]);

        let params = translate_request(&req, 4096, None, None)?;
        let content = params.messages[0].content();
        assert_eq!(content.len(), 2);

        if let ContentBlock::Text(t) = &content[0] {
            assert_eq!(t, "Let me check.");
        } else {
            panic!("Expected Text block");
        }

        if let ContentBlock::ToolUse(tu) = &content[1] {
            assert_eq!(tu.tool_use_id(), "call_1");
            assert_eq!(tu.name(), "git_log");
            assert_eq!(document_to_json(tu.input()), json!({"n": 5}));
        } else {
            panic!("Expected ToolUse block");
        }

        Ok(())
    }

    #[test]
    fn test_translate_tool_result() -> Result<()> {
        let req = make_request(vec![AiMessage {
            role: AiRole::Tool,
            content: Some("commit abc123".to_string()),
            thought: None,
            tool_calls: None,
            tool_call_id: Some("call_1".to_string()),
        }]);

        let params = translate_request(&req, 4096, None, None)?;
        assert_eq!(params.messages.len(), 1);
        assert_eq!(params.messages[0].role(), &ConversationRole::User);

        if let ContentBlock::ToolResult(tr) = &params.messages[0].content()[0] {
            assert_eq!(tr.tool_use_id(), "call_1");
        } else {
            panic!("Expected ToolResult block");
        }

        Ok(())
    }

    #[test]
    fn test_translate_tools() -> Result<()> {
        let mut req = make_request(vec![AiMessage {
            role: AiRole::User,
            content: Some("hi".to_string()),
            thought: None,
            tool_calls: None,
            tool_call_id: None,
        }]);
        req.tools = Some(vec![AiTool {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}}
            }),
        }]);

        let params = translate_request(&req, 4096, None, None)?;
        let tools = params.tool_config.unwrap().tools().to_vec();
        assert_eq!(tools.len(), 1);

        if let Tool::ToolSpec(spec) = &tools[0] {
            assert_eq!(spec.name(), "read_file");
            assert_eq!(spec.description(), Some("Read a file"));
        } else {
            panic!("Expected ToolSpec");
        }

        Ok(())
    }

    #[test]
    fn test_translate_empty_tools_omitted() -> Result<()> {
        let mut req = make_request(vec![AiMessage {
            role: AiRole::User,
            content: Some("hi".to_string()),
            thought: None,
            tool_calls: None,
            tool_call_id: None,
        }]);
        req.tools = Some(vec![]);

        let params = translate_request(&req, 4096, None, None)?;
        assert!(params.tool_config.is_none());

        Ok(())
    }

    #[test]
    fn test_additional_fields_none_when_unset() {
        assert!(build_additional_fields(None, None).is_none());
    }

    #[test]
    fn test_additional_fields_thinking_and_effort() {
        let doc = build_additional_fields(Some("adaptive"), Some("xhigh")).unwrap();
        let json = document_to_json(&doc);
        assert_eq!(
            json,
            serde_json::json!({
                "thinking": {"type": "adaptive"},
                "output_config": {"effort": "xhigh"}
            })
        );
    }

    #[test]
    fn test_max_tokens_propagates_to_inference_config() -> Result<()> {
        let req = make_request(vec![AiMessage {
            role: AiRole::User,
            content: Some("hi".to_string()),
            thought: None,
            tool_calls: None,
            tool_call_id: None,
        }]);

        let params = translate_request(&req, 8192, None, None)?;
        assert_eq!(params.inference_config.unwrap().max_tokens(), Some(8192));
        Ok(())
    }

    #[test]
    fn test_translate_request_wires_additional_fields() -> Result<()> {
        let req = make_request(vec![AiMessage {
            role: AiRole::User,
            content: Some("hi".to_string()),
            thought: None,
            tool_calls: None,
            tool_call_id: None,
        }]);

        let params = translate_request(&req, 8192, Some("adaptive"), Some("high"))?;
        let extra = params.additional_model_request_fields.unwrap();
        let json = document_to_json(&extra);
        assert_eq!(json["thinking"]["type"], "adaptive");
        assert_eq!(json["output_config"]["effort"], "high");
        Ok(())
    }
}
