# DESIGN: Ollama Provider

## Context

Sashiko supports running LLMs locally via Ollama. Rather than using an OpenAI-compatible adapter, Sashiko implements a direct integration with the Ollama API. This provides better support for Ollama-specific options like `think` (for reasoning models) and accurate mapping of Ollama's unique endpoints and wire formats.

## Design Decisions

| Decision | Choice |
|---|---|
| Client architecture | Single `OllamaClient` in `src/ai/ollama.rs`. |
| Reasoning support | Integrated via the `think` parameter in `OllamaOptions`, allowing reasoning effort control (e.g. "medium", "high") for supported models like DeepSeek. |
| Endpoint normalization | Ollama's base URL often needs normalizing, so `normalize_base_url` automatically appends `/api/chat` as needed. |
| Token options | Uses `num_ctx` and `num_predict` in `OllamaOptions` instead of standard max_tokens fields. |

## Provider Compatibility

`OllamaClient` interacts directly with Ollama's local HTTP API (by default `http://localhost:11434/api/chat`).
- No API key or subscription required.
- Global settings in `Settings.toml` map transparently to Ollama features.

## Files

### `src/ai/ollama.rs`

#### Client Struct

```rust
pub struct OllamaClient {
    model: String,
    base_url: String,
    context_window_size: usize,
    max_tokens: u32,
    think: Option<String>,
    client: reqwest::Client,
}
```

#### Ollama Wire-Format Structs (serde-annotated)

| Struct | Key Fields |
|---|---|
| `OllamaRequest` | `model`, `messages`, `stream`, `options?` |
| `OllamaMessage` | `role`, `content?`, `tool_calls?` |
| `OllamaToolCall` | `function: OllamaToolCallFunction` |
| `OllamaToolCallFunction` | `name`, `arguments` (JSON object) |
| `OllamaOptions` | `temperature?`, `num_ctx?`, `num_predict?`, `format?`, `think?` |
| `OllamaResponse` | `message`, metrics (`total_duration`, `eval_count`, etc.) |

#### Error Enum

```rust
pub enum OllamaError {
    BadRequest(String),
    NotFound(String),
    ServerError(String),
    BadGateway(String),
    TransientError(Duration, String),
    RateLimitExceeded(Duration, String),
    ApiError(u16, String),
}
```

#### Client Methods

| Method | Purpose |
|---|---|
| `new(...)` | Initializes `reqwest::Client` with 120s timeout. |
| `post_request(...)` | Submits JSON request to endpoint, handles HTTP status codes, parses response. |
| `normalize_base_url(...)` | Ensures the provided base URL correctly ends with `/api/chat`. |
| `default_context_window_for_model(...)` | Provides a fallback context window based on the model name. |
| `translate_ollama_request(...)` | Converts standard `AiRequest` to `OllamaRequest`. |
| `translate_ollama_response(...)` | Converts `OllamaResponse` to `AiResponse`. |

#### Request Translation: `AiRequest` -> `OllamaRequest`
- Roles (System, User, Assistant) are mapped directly.
- Handles edge cases where Ollama lacks a direct equivalent role (e.g. mapping `AiRole::Tool` messages intelligently to fit the Ollama schema).
- Tool definitions are serialized according to Ollama's expectations.
- Options like `temperature`, `context_window_size`, and `think` are placed into the `OllamaOptions` object (`num_ctx`, `num_predict`).

#### Response Translation: `OllamaResponse` -> `AiResponse`
- Extracts text content and optional `tool_calls`.
- Maps metrics (if available) to token usage.

### `src/settings.rs`

```rust
pub struct OllamaSettings {
    pub base_url: Option<String>,
    pub context_window_size: Option<usize>,
    pub max_tokens: Option<u32>,
    pub think: Option<String>,
}
```

## Testing (in `src/ai/ollama.rs`)

### Request Translation Tests
- `test_translate_ollama_request_with_system`
- `test_translate_ollama_request_with_tool_calls`
- `test_translate_request_preserves_temperature`
- `test_translate_request_with_none_temperature`

### Response Translation Tests
- `test_translate_ollama_response_text`
- `test_translate_ollama_response_with_tool_calls`

### Configuration and Error Tests
- `test_normalize_base_url_with_chat_endpoint`
- `test_normalize_base_url_with_api_only`
- `test_normalize_base_url_with_base_only`
- `test_normalize_base_url_with_trailing_slash`
- `test_error_classification_connection`
- `test_error_classification_api`
- `test_error_classification_model_not_found`
- `test_default_base_url`
- `test_default_context_window`
- `test_estimate_tokens_basic`