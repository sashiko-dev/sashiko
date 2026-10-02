# DESIGN: OpenAI and OpenAI-Compatible Provider Support

## Context

Sashiko supports two OpenAI-related providers:

1. **`"openai"`** — a dedicated provider targeting OpenAI's `/v1/responses` endpoint (`src/ai/openai_responses.rs`). This is the recommended path for OpenAI's reasoning, tool-calling, and multi-turn workflows, including the GPT-5.6 family.

2. **`"openai-compatible"`** — a shared provider targeting the standard `/v1/chat/completions` endpoint (`src/ai/openai.rs`). This handles third-party OpenAI-compatible services (LM Studio, OpenRouter, z.ai, OrcaRouter, etc.) via configuration.

Previously, both provider names were backed by a single `OpenAiCompatClient` with a serialization flag (`OpenAiProviderType`) to switch between `max_tokens` and `max_completion_tokens`. The split allows the dedicated provider to use the Responses API's native item protocol instead of translating state through the chat-completions format.

## Design Decisions

| Decision | Choice |
|---|---|
| Client architecture | Two separate clients: `OpenAiClient` in `openai_responses.rs` for the Responses API, `OpenAiCompatClient` in `openai.rs` for chat completions |
| Provider names | `"openai"` (Responses API) and `"openai-compatible"` (chat completions) |
| Config sections | `[ai.openai]` for the Responses provider, `[ai.openai_compat]` for chat completions |
| Legacy configuration | Preserve `provider = "openai"` without `[ai.openai]` as deprecated Chat Completions mode using `max_completion_tokens` |
| Reasoning | `reasoning_effort` on `[ai.openai]`; GPT-5.6 accepts `none`, `low`, `medium`, `high`, `xhigh`, and `max` (default: `medium`) |
| Temperature | Suppressed for known reasoning families (`gpt-5`, `o1`, `o3`, `o4`); passed through for other Responses models and on `openai-compatible` |
| Output continuity | Preserve every Responses output item in an accepted response, including reasoning and future item types, and replay it verbatim before tool outputs |
| Response limits | Reject successful bodies larger than 17 MiB, error bodies larger than 64 KiB, JSON documents containing more than 131,072 values, responses containing more than 4,096 output items, and cumulative continuation metadata larger than 16 MiB |
| Function-call identity | Preserve both the Responses output-item `id` and its `call_id`; never synthesize an item ID |
| JSON mode | Send `text.format: { type: "json_object" }` and ensure the input explicitly asks for JSON |
| Token accounting | Map `usage.input_tokens_details.cached_tokens` when present and valid |
| Token limit field | Responses API uses `max_output_tokens`; chat completions uses `max_tokens` |
| Cache compatibility | Give the Responses client a distinct provider cache identity; keep the shared cache wrapper provider-agnostic |
| Cache bounds | Omit opaque continuation data from diagnostic request JSON, skip entries larger than 16 MiB, and retain at most 256 MiB of serialized cache payload; replacement and single-pass oldest-entry pruning share an immediate database transaction |
| API key | Both providers: `OPENAI_API_KEY` env → `LLM_API_KEY` fallback |
| Retry guidance | Prefer `Retry-After`, fall back to a body hint, and cap either value at five minutes; response-level `server_error` failures are transient |

## Provider: `"openai"` (Responses API)

### Files

- `src/ai/openai_responses.rs` — client, wire types, translation
- `src/settings.rs` — `OpenAiSettings` struct
- Config section: `[ai.openai]`

### `OpenAiSettings`

```rust
pub struct OpenAiSettings {
    pub base_url: Option<String>,
    pub context_window_size: Option<usize>,
    pub max_tokens: Option<u32>,
    pub reasoning_effort: Option<String>,  // GPT-5.6: none, low, medium, high, xhigh, max
}
```

### Wire-Format Types (Responses API)

| Struct | Purpose |
|---|---|
| `ResponsesRequest` | `model`, `input`, `tools?`, `temperature?`, `max_output_tokens?`, `reasoning?`, `text?` |
| `ResponsesInputItem` | Untagged enum: `Message`, `FunctionCall`, `FunctionCallOutput` |
| `ResponsesTool` | `type` ("function"), `name`, `description`, `parameters` |
| `ReasoningConfig` | `effort?` — GPT-5.6: `"none"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"` |
| `ResponsesResponse` | `id`, `status`, `error?`, `incomplete_details?`, `output`, `usage` |
| `ResponsesOutputItem` | Complete raw output item, including `message`, `function_call`, `reasoning`, and future types |
| `ResponsesContent` | Tagged enum: `OutputText`, `Refusal` |

### Request Translation: `AiRequest` → `ResponsesRequest`

| `AiRequest` | `ResponsesRequest` |
|---|---|
| `system: Some(text)` | Input item: `{ role: "system", content: text }` |
| `AiRole::System` message | Input item: `{ role: "system", content }` |
| `AiRole::User` message | Input item: `{ role: "user", content }` |
| `AiRole::Assistant` text | Input item: `{ role: "assistant", content }`; an assistant tool call without Responses metadata is rejected because its output-item `id` cannot be reconstructed from `call_id` |
| Prior Responses output | Replayed verbatim in its original order, retaining reasoning items and function-call item IDs |
| `AiRole::Tool` message | Input item: `{ type: "function_call_output", call_id, output }` |
| `tools` | `[{ type: "function", name, description, parameters }]` |
| `temperature` | Omitted for known reasoning families; otherwise forwarded |
| `reasoning_effort` | `{ reasoning: { effort: "..." } }` when configured |
| `response_format: Json` without a schema | `{ text: { format: { type: "json_object" } } }` plus an unconditional leading `{ role: "system", content: "Respond in JSON format." }` message, keeping the prompt-cache prefix stable across turns |
| `response_format: Json` with a schema | `{ text: { format: { type: "json_schema", name: "sashiko_response", schema } } }` |

Before sending a structured-output schema, the Responses provider verifies
that every object node explicitly sets `additionalProperties: false` and marks
every declared property as required, as required by OpenAI's supported JSON
Schema subset. Incompatible schemas fail locally instead of being rewritten,
so translation does not change caller semantics. Validation follows only
schema-bearing keywords such as `properties`, `items`, `anyOf`, and `$defs`;
literal objects under keywords such as `const`, `enum`, and `default` are
unchanged.

### Response Translation: `ResponsesResponse` → `AiResponse`

| `ResponsesResponse` | `AiResponse` |
|---|---|
| `output[].Message.content[].OutputText` | `content` (joined) |
| `output[].Message.content[].Refusal` | Logged as a content-free warning, discarded |
| `output[].FunctionCall` | Exposed as a Sashiko tool call only when `id`, `call_id`, and `name` are non-empty, `call_id` is unique within the response, and arguments contain a valid JSON object; the complete original item is retained for continuation |
| `output[]` (all types) | Preserved as opaque provider metadata for the next request; no item type is silently dropped |
| `status == "incomplete"` | `truncated: true`; log a bounded, control-character-safe `incomplete_details.reason` and output-token usage |
| `status == "failed"` or `error != null` | Return the provider's error directly; retry `server_error` and `server_is_overloaded`, and never treat failures as empty successful responses |
| `usage.input_tokens` | `prompt_tokens` |
| `usage.output_tokens` | `completion_tokens` |
| `usage.input_tokens_details.cached_tokens` | `cached_tokens` when nonzero and no larger than `input_tokens` |

### Endpoint Normalization

The default endpoint is `https://api.openai.com/v1/responses`. Custom values
may be complete `/responses` endpoints or recognized API roots. Sashiko
appends `/responses` to a bare host, `/v1`, or `/api/v1`; unsupported partial
paths are rejected rather than guessed. Custom remote endpoints must use
HTTPS. Plain HTTP is accepted only for `localhost` or a loopback IP address.
Redirect following is disabled so a validated endpoint cannot forward request
content or credentials to an unvalidated destination.

### Function Call ID Mapping

The Responses API uses two distinct identifiers for function calls:
- `id` — the output-item identifier
- `call_id` — the correlation key linking a `function_call` to its
  `function_call_output`

Sashiko retains the complete original function-call item so a later request
reuses both values exactly. Tool output uses the original `call_id`; the
provider must not infer or synthesize the output-item `id` from it. Request
translation consumes each pending raw `call_id` exactly once and rejects
unmatched, duplicate, or missing tool outputs.

### Stateless Continuation

When a response requests tools, the next request includes all of the prior
response's output items, followed by the corresponding `function_call_output`
items. In particular, reasoning items must survive this boundary. This lets the
provider remain stateless and safe to share across concurrent reviews while
still giving the Responses API the context it requires to continue a tool
workflow. Replayed output arrays use the same item-count limit as fresh
responses, malformed tool results are rejected, and the cumulative metadata
budget counts only responses retained for another provider turn.

## Provider: `"openai-compatible"` (Chat Completions)

### Files

- `src/ai/openai.rs` — client, wire types, translation
- `src/settings.rs` — `OpenAiCompatSettings` struct
- Config section: `[ai.openai_compat]`

### `OpenAiCompatSettings`

```rust
pub struct OpenAiCompatSettings {
    pub base_url: Option<String>,
    pub context_window_size: Option<usize>,
    pub max_tokens: Option<u32>,
    pub token_limit_field: OpenAiTokenLimitField,
}
```

### Client Struct

```rust
pub struct OpenAiCompatClient {
    model: String,
    base_url: String,
    context_window_size: usize,
    max_tokens: u32,
    client: reqwest::Client,
}
```

### Request Translation

| `AiRequest` | `OpenAiRequest` |
|---|---|
| `system: Some(text)` | Message: `{ role: "system", content: text }` |
| `AiRole::*` messages | Standard chat completions message format |
| `tools` | `[{ type: "function", function: { name, description, parameters } }]` |
| `temperature` | Passed through directly |
| `response_format: Json` | `{ type: "json_object" }` + "json" word injection |
| Token limit | `max_tokens: N` by default; `max_completion_tokens: N` when `token_limit_field = "max_completion_tokens"` |

### URL Defaults by Model Prefix

| Model Prefix | Default Endpoint | Default Context Window |
|---|---|---|
| `gpt-4o`, `gpt-4-turbo`, or other | `https://api.openai.com/v1/chat/completions` | 128,000 |
| `gpt-3.5` | `https://api.openai.com/v1/chat/completions` | 16,385 |
| `glm-` | `https://open.bigmodel.cn/api/paas/v4/chat/completions` | 128,000 |
| `moonshot-` | `https://api.moonshot.cn/v1/chat/completions` | 128,000 |
| `abab7-` / `MiniMax-` | `https://api.minimax.chat/v1/text/chatcompletion_v2` | 245,760 |

## Factory: `create_provider_from_ai()`

The `"openai"` and `"openai-compatible"` match arms in `src/ai/mod.rs`
are separate:

```rust
"openai" => {
    // The absence of ai.openai preserves Chat Completions and emits a
    // deprecation warning. Otherwise creates the Responses client.
}
"openai-compatible" => {
    // Reads from ai.openai_compat settings
    // Creates openai::OpenAiCompatClient
}
```

### Configuration Migration

```rust
pub struct OpenAiCompatClient {
    model: String,
    base_url: String,
    context_window_size: usize,
    max_tokens: u32,  // Default: 4096
    provider_type: OpenAiProviderType,
    client: reqwest::Client,
}
```

#### OpenAI Wire-Format Structs (serde-annotated)

| Struct | Key Fields |
|---|---|
| `OpenAiRequest` | `model`, `messages`, `tools?`, `temperature?`, `max_tokens?`, `max_completion_tokens?`, `response_format?` |
| `OpenAiMessage` | `role`, `content?`, `tool_calls?`, `tool_call_id?` |
| `OpenAiToolCall` | `id`, `type` ("function"), `function: OpenAiToolCallFunction` |
| `OpenAiToolCallFunction` | `name`, `arguments` (JSON **string**, not object) |
| `OpenAiTool` | `type` ("function"), `function: OpenAiFunction` |
| `OpenAiFunction` | `name`, `description`, `parameters` |
| `OpenAiResponse` | `choices`, `usage` |
| `OpenAiChoice` | `index`, `message: OpenAiMessage`, `finish_reason` |
| `OpenAiUsage` | `prompt_tokens`, `completion_tokens`, `total_tokens` |

#### `OpenAiRequest` Token Limit Fields

```rust
#[derive(Debug, Serialize, Deserialize)]
pub struct OpenAiRequest {
    pub model: String,
    pub messages: Vec<OpenAiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<OpenAiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,
}
```

#### Error Enum

```rust
#[derive(Debug, thiserror::Error)]
pub enum OpenAiCompatError {
    #[error("Rate limit exceeded, retry after {0:?}")]
    RateLimitExceeded(Duration),
    #[error("Transient error: {1}, retry after {0:?}")]
    TransientError(Duration, String),
    #[error("Authentication error: {0}")]
    AuthenticationError(String),
    #[error("API error {0}: {1}")]
    ApiError(reqwest::StatusCode, String),
}
```

#### Client Methods

| Method | Purpose |
|---|---|
| `new(base_url, provider_type, model, context_window_size, max_tokens) -> Self` | Build `reqwest::Client` with `Authorization: Bearer {key}` header (from `OPENAI_API_KEY` → `LLM_API_KEY` env), 120s timeout |
| `post_request(&self, body: &Value) -> Result<OpenAiResponse, OpenAiCompatError>` | POST JSON `body` to `self.base_url`. Transport error → `TransientError(30s)` (error string sanitized via `redact_secret()`). On HTTP success, reads body as text and parses JSON; parse failure → `ApiError`. HTTP errors: 429 → `RateLimitExceeded` (`Retry-After` header parsed first; body regex `"Please retry in ([0-9.]+)s"` overrides if matched; default 60s), 401/403 → `AuthenticationError`, 500/502/503/504 → `TransientError(30s)`, other → `ApiError`. Includes logging of response tokens on success. |
| `translate_ai_request(AiRequest, max_tokens, provider_type) -> OpenAiRequest` | See translation mapping below |
| `translate_ai_response(OpenAiResponse) -> AiResponse` | See translation mapping below |
| `estimate_tokens_generic(AiRequest) -> usize` | Reuse `TokenBudget::estimate_tokens`. Must include `request.system` along with messages and tools. |

#### Helper Methods

| Method | Purpose |
|---|---|
| `default_base_url_for_model(model: &str) -> String` | Returns provider-specific default URL based on model name prefix |
| `default_context_window_for_model(model: &str) -> usize` | Returns provider-specific default context window based on model name prefix |

#### Request Translation: `AiRequest` → `OpenAiRequest`

| `AiRequest` | `OpenAiRequest` |
|---|---|
| `system: Some(text)` | Message: `{ role: "system", content: text }` |
| `AiRole::System` message | `{ role: "system", content }` |
| `AiRole::User` message | `{ role: "user", content }` |
| `AiRole::Assistant` message | `{ role: "assistant", content?, tool_calls? }` — tool_calls with `arguments` serialized as JSON **string** |
| `AiRole::Tool` message | `{ role: "tool", tool_call_id, content }` |
| `tools` | `[{ type: "function", function: { name, description, parameters } }]` |
| `temperature` | Passed through directly when present |
| `response_format: Json` | `{ type: "json_object" }`. **JSON word injection:** Some Responses-backed gateways require "json" in a user message. If the first user message lacks it (case-insensitive), append `"\nRespond in JSON format."` there so the prompt prefix stays stable across turns. If there is no user message, append a new one with the hint. |
| `response_format: Text` | `{ type: "text" }` |
| `OpenAiProviderType::OpenAi` | `{ max_completion_tokens: N }` (OpenAI) |
| `OpenAiProviderType::OpenAiCompatible` | `{ max_tokens: N }` (OpenAI-compatible) |

#### Response Translation: `OpenAiResponse` → `AiResponse`

| `OpenAiResponse` | `AiResponse` |
|---|---|
| `choices[0].message.content` | `content` |
| (no reasoning support) | `thought: None` |
| `choices[0].message.tool_calls` | `tool_calls` — `function.arguments` (JSON string) parsed to `serde_json::Value`, `thought_signature: None` |
| `usage.prompt_tokens` | `prompt_tokens` |
| `usage.completion_tokens` | `completion_tokens` |
| `usage.total_tokens` | `total_tokens` |
| `usage.prompt_tokens_details.cached_tokens` | `cached_tokens` — a breakdown of `prompt_tokens`, which passes through unchanged. A missing or malformed `prompt_tokens_details` is dropped rather than failing the response; zero, or a count larger than `prompt_tokens`, yields `None`. |

#### `impl AiProvider for OpenAiCompatClient`

```rust
async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
    tracing::info!("Sending OpenAI request...");

    let mut openai_req = translate_ai_request(request, self.max_tokens, self.provider_type)?;
    openai_req.model = self.model.clone();

    let resp_body = serde_json::to_value(&openai_req)?;
    let resp = self.post_request(&resp_body).await?;
    translate_ai_response(resp)
}

fn estimate_tokens(&self, request: &AiRequest) -> usize {
    estimate_tokens_generic(request)
}

fn get_capabilities(&self) -> ProviderCapabilities {
    ProviderCapabilities {
        model_name: self.model.clone(),
        context_window_size: self.context_window_size,
    }
}
```

#### Tests (16 tests in `#[cfg(test)] mod tests`)

##### Request Translation Tests

| # | Test Name | Verifies |
|---|---|---|
| 1 | `test_translate_request_system_and_user` | `AiRequest.system` → `{"role": "system"}` message. User → `{"role": "user"}`. Temperature passed through. |
| 2 | `test_translate_request_system_in_messages` | `AiRole::System` message (not the `system` field) → `{"role": "system"}`. |
| 3 | `test_translate_request_assistant_tool_call` | Assistant with `tool_calls` → `{"role": "assistant", "tool_calls": [...]}`. `arguments` is a JSON **string**. |
| 4 | `test_translate_request_tool_response` | Tool message → `{"role": "tool", "tool_call_id": "...", "content": "..."}`. |
| 5 | `test_translate_request_tools_definition` | `AiTool` → `{"type": "function", "function": {"name", "description", "parameters"}}`. |
| 5.1 | `test_translate_request_empty_tools` | `Some(vec![])` tools → `None` (for `skip_serializing_if` compatibility). |
| 6 | `test_translate_request_conversation_chain` | Full user → assistant (tool_calls) → tool response chain. Correct roles and ordering. |
| 7 | `test_translate_request_json_format` | `AiResponseFormat::Json` → `{"type": "json_object"}`. The first user message receives the JSON hint when it lacks one. |
| 7.1 | `test_translate_request_json_format_no_injection_when_present` | A first user message already mentioning JSON keeps its original content. |
| 7.2 | `test_json_hint_stays_on_first_user_across_turns` | Later user messages do not move the hint or change the first user message. |
| 7.3 | `test_json_hint_adds_user_when_request_has_none` | A request without a user message receives one containing the JSON hint. |
| 8 | `test_translate_request_temperature` | Temperature from `AiRequest` included in `OpenAiRequest.temperature`. |

##### Response Translation Tests

| # | Test Name | Verifies |
|---|---|---|
| 9 | `test_translate_response_text` | `choices[0].message.content` → `AiResponse.content`. `thought` is `None`. Usage mapped. |
| 10 | `test_translate_response_tool_calls` | `tool_calls` with `arguments` as JSON string → parsed `Vec<ToolCall>`. `thought_signature: None`. |
| 11 | `test_translate_response_empty_choices` | Empty/missing `choices` → error. |

##### Token Estimation Test

| # | Test Name | Verifies |
|---|---|---|
| 12 | `test_estimate_tokens` | Token count in reasonable range for known input. Same pattern as `gemini.rs::test_estimate_tokens_logic`. |

##### Config Tests

| # | Test Name | Verifies |
|---|---|---|
| 13 | `test_max_tokens_for_openai_compatible` | `OpenAiProviderType::OpenAiCompatible` → serialized JSON has `max_tokens` and no `max_completion_tokens`. |
| 14 | `test_max_completion_tokens_for_openai` | `OpenAiProviderType::OpenAi` → serialized JSON has `max_completion_tokens` and no `max_tokens`. |

### `src/settings.rs`

```rust
#[derive(Debug, Deserialize, Clone)]
pub struct OpenAiCompatSettings {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub context_window_size: Option<usize>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}
```

Field in `AiSettings`:

```rust
pub openai_compat: Option<OpenAiCompatSettings>,
```

### `src/ai/mod.rs`

#### Module Declaration

```rust
pub mod openai;
```

#### Factory: Combined Match Arm in `create_provider()`

Both arms share the same config-reading logic, differing only in `provider_type`:

```rust
"openai" | "openai-compatible" => {
    let provider_type = match settings.ai.provider.to_lowercase().as_str() {
        "openai" => openai::OpenAiProviderType::OpenAi,
        _ => openai::OpenAiProviderType::OpenAiCompatible,
    };

    let base_url = settings.ai.openai_compat
        .as_ref()
        .and_then(|c| c.base_url.clone())
        .unwrap_or_else(|| openai::OpenAiCompatClient::default_base_url_for_model(&settings.ai.model));

    let context_window = settings.ai.openai_compat
        .as_ref()
        .and_then(|c| c.context_window_size)
        .unwrap_or_else(|| openai::OpenAiCompatClient::default_context_window_for_model(&settings.ai.model));

    let max_tokens = settings.ai.openai_compat
        .as_ref()
        .and_then(|c| c.max_tokens)
        .unwrap_or(4096);

    Ok(Arc::new(openai::OpenAiCompatClient::new(
        base_url,
        provider_type,
        settings.ai.model.clone(),
        context_window,
        max_tokens,
    )))
}
```

**Note:** Factory does not manipulate environment variables. Constructor reads `OPENAI_API_KEY` → `LLM_API_KEY` internally.

#### Test in `test_create_provider`

```rust
settings.ai.provider = "openai".to_string();
settings.ai.model = "gpt-4o".to_string();
let provider = create_provider(&settings)?;
assert_eq!(provider.get_capabilities().model_name, "gpt-4o");
```

For backward compatibility, `provider = "openai"` without an `[ai.openai]`
table keeps using `OpenAiCompatClient`, emits a deprecation warning, and forces
`max_completion_tokens` to preserve the former official OpenAI wire format.
This includes table-less configurations and configurations with only
`[ai.openai_compat]`. To retain that behavior explicitly, select
`provider = "openai-compatible"` and set
`token_limit_field = "max_completion_tokens"`. To opt into Responses, add or
migrate values to `[ai.openai]`; replace a `/v1/chat/completions` URL with a
`/v1/responses` URL or omit `base_url` to use the default. Provider tables for
inactive providers may coexist, so `[ai.openai]` takes precedence when both
tables are present.

## Environment Variables

| Variable | Purpose |
|---|---|
| `OPENAI_API_KEY` | API key for both providers |
| `LLM_API_KEY` | Fallback API key if `OPENAI_API_KEY` not set |

## Configuration Examples

```toml
# OpenAI Responses API — reasoning model with tool support
[ai]
provider = "openai"
model = "gpt-5.6-terra"

[ai.openai]
max_tokens = 16384  # default
reasoning_effort = "medium"  # recommended default
# base_url = "https://proxy.example/v1"  # /responses is appended
# context_window_size = 1050000  # GPT-5.6; maximum output is 128000

# OpenAI-compatible — third-party endpoint
[ai]
provider = "openai-compatible"
model = "glm-5.2"

[ai.openai_compat]
base_url = "https://api.z.ai/api/coding/paas/v4/chat/completions"
context_window_size = 128000
max_tokens = 16384
```
