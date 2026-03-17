# DESIGN: OpenAI and OpenAI-Compatible Provider Support

## Context

Sashiko currently supports two AI providers with custom API formats: Gemini (`src/ai/gemini.rs`) and Claude (`src/ai/claude.rs`). Many popular AI providers — OpenAI, GLM (Zhipu AI), Kimi (Moonshot AI), and Minimax — use an OpenAI-compatible chat completions API format. Rather than implementing separate clients for each, we use a single shared `OpenAiCompatClient` that handles all of them via configuration.

The official OpenAI API uses `max_completion_tokens` in the request body (introduced with the `o1` model family), while third-party OpenAI-compatible providers use the legacy `max_tokens` field. To support both, we expose two provider names — `"openai"` and `"openai-compatible"` — backed by the same client with a serialization flag.

## Design Decisions

| Decision | Choice |
|---|---|
| Client architecture | Single `OpenAiCompatClient` in `src/ai/openai.rs`, no per-provider structs or files |
| Provider names | `"openai"` (official API, uses `max_completion_tokens`) and `"openai-compatible"` (third-party, uses `max_tokens`) |
| Token limit field | `"openai"` serializes `max_completion_tokens`; `"openai-compatible"` serializes `max_tokens`. Controlled by `OpenAiProviderType` enum on the client. |
| Stdio support | Not needed for OpenAI-compatible provider |
| Thinking/reasoning support | Not included in initial implementation (`thought: None` always) |
| Temperature | Always passed through from `AiRequest` when present |
| URL configuration | `base_url` from settings → model-based default (glm-*, moonshot-*, abab7-*, others) |
| API key | `OPENAI_API_KEY` env only (fallback to `LLM_API_KEY`), no provider-specific keys |

## Provider Compatibility

`OpenAiCompatClient` supports any OpenAI-compatible API. Model name determines provider-specific defaults:

| Model Prefix | Default Endpoint | Default Context Window |
|---|---|---|
| `gpt-4o`, `gpt-4-turbo`, or other | `https://api.openai.com/v1/chat/completions` | 128,000 |
| `gpt-3.5` | `https://api.openai.com/v1/chat/completions` | 16,385 |
| `glm-` | `https://open.bigmodel.cn/api/paas/v4/chat/completions` | 128,000 |
| `moonshot-` | `https://api.moonshot.cn/v1/chat/completions` | 128,000 |
| `abab7-` | `https://api.minimax.chat/v1/text/chatcompletion_v2` | 245,760 |

All providers use `Authorization: Bearer <OPENAI_API_KEY>` for authentication.

## `max_completion_tokens` vs `max_tokens`

The official OpenAI API deprecated `max_tokens` in favor of `max_completion_tokens` starting with the `o1` model family. The key differences:

| Aspect | `max_tokens` (legacy) | `max_completion_tokens` (OpenAI) |
|---|---|---|
| Used by | Third-party OpenAI-compatible APIs | Official OpenAI API |
| Provider name | `"openai-compatible"` | `"openai"` |
| Serialization | `"max_tokens": N` in JSON body | `"max_completion_tokens": N` in JSON body |

Both fields are defined as `Option<u32>` on `OpenAiRequest` with `skip_serializing_if = "Option::is_none"`. The `translate_ai_request()` function sets only the relevant field based on the `OpenAiProviderType` enum.

## Files

### `src/ai/openai.rs`

#### Provider Type Enum

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiProviderType {
    /// Official OpenAI API — uses `max_completion_tokens`.
    OpenAi,
    /// Third-party OpenAI-compatible APIs — uses `max_tokens`.
    OpenAiCompatible,
}
```

#### Client Struct (matches Gemini pattern)

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
| `response_format: Json` | `{ type: "json_object" }`. **JSON word injection:** OpenAI requires the word "json" to appear in at least one message when using `json_object` mode. If no message already contains "json" (case-insensitive), the provider appends `"\nRespond in JSON format."` to the first system message, or prepends a new system message `"Respond in JSON format."` if none exists. |
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
| (no cached tokens in standard OpenAI) | `cached_tokens: None` |

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
| 7 | `test_translate_request_json_format` | `AiResponseFormat::Json` → `{"type": "json_object"}`. When no message contains "json", a system message `"Respond in JSON format."` is prepended. |
| 7.1 | `test_translate_request_json_format_no_injection_when_present` | When messages already contain "json" (case-insensitive), no additional system message is injected. |
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

## Environment Variables

| Variable | Purpose |
|---|---|
| `OPENAI_API_KEY` | API key for OpenAI-compatible provider |
| `LLM_API_KEY` | Fallback API key if `OPENAI_API_KEY` not set |

**API key resolution:** `OPENAI_API_KEY` env → `LLM_API_KEY` env → empty string

**Note:** `OPENAI_BASE_URL` env var is NOT read. Use `[ai.openai_compat].base_url` in settings instead.

## Configuration Examples

```toml
# Standard OpenAI — uses max_completion_tokens
[ai]
provider = "openai"
model = "gpt-4o"
temperature = 0.7

# OpenAI via Azure proxy — uses max_completion_tokens
[ai]
provider = "openai"
model = "gpt-4o"

[ai.openai_compat]
base_url = "https://my-azure-instance.openai.azure.com/openai/deployments/gpt-4o/chat/completions?api-version=2024-02-01"

# GLM (Zhipu AI) via OpenAI-compatible endpoint — uses max_tokens
[ai]
provider = "openai-compatible"
model = "glm-4"

[ai.openai_compat]
base_url = "https://api.eliza.yandex.net/raw/internal/glm-latest/v1/chat/completions"

# Kimi (Moonshot AI) — uses max_tokens
[ai]
provider = "openai-compatible"
model = "moonshot-v1-128k"

[ai.openai_compat]
base_url = "https://api.moonshot.cn/v1/chat/completions"

# Minimax — uses max_tokens
[ai]
provider = "openai-compatible"
model = "abab7-chat-preview"

[ai.openai_compat]
base_url = "https://api.minimax.chat/v1/text/chatcompletion_v2"
context_window_size = 245760
max_tokens = 8192
```
