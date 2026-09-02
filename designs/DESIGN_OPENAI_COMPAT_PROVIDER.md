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
| Reasoning | `reasoning_effort` on `[ai.openai]`; GPT-5.6 accepts `none`, `low`, `medium`, `high`, `xhigh`, and `max` (default: `medium`) |
| Temperature | Always suppressed on the Responses API provider; passed through on `openai-compatible` |
| Output continuity | Preserve every Responses output item, including reasoning and future item types, and replay it verbatim before tool outputs |
| Function-call identity | Preserve both the Responses output-item `id` and its `call_id`; never synthesize an item ID |
| JSON mode | Send `text.format: { type: "json_object" }` and ensure the input explicitly asks for JSON |
| Token accounting | Map `usage.input_tokens_details.cached_tokens` when present and valid |
| Token limit field | Responses API uses `max_output_tokens`; chat completions uses `max_tokens` |
| API key | Both providers: `OPENAI_API_KEY` env → `LLM_API_KEY` fallback |

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
| `ResponsesResponse` | `id`, `status`, `output`, `usage` |
| `ResponsesOutputItem` | Complete raw output item, including `message`, `function_call`, `reasoning`, and future types |
| `ResponsesContent` | Tagged enum: `OutputText`, `Refusal` |

### Request Translation: `AiRequest` → `ResponsesRequest`

| `AiRequest` | `ResponsesRequest` |
|---|---|
| `system: Some(text)` | Input item: `{ role: "system", content: text }` |
| `AiRole::System` message | Input item: `{ role: "system", content }` |
| `AiRole::User` message | Input item: `{ role: "user", content }` |
| `AiRole::Assistant` text | Input item: `{ role: "assistant", content }` |
| Prior Responses output | Replayed verbatim in its original order, retaining reasoning items and function-call item IDs |
| `AiRole::Tool` message | Input item: `{ type: "function_call_output", call_id, output }` |
| `tools` | `[{ type: "function", name, description, parameters }]` |
| `temperature` | Always `None` (reasoning models reject it) |
| `reasoning_effort` | `{ reasoning: { effort: "..." } }` when configured |
| `response_format: Json` | `{ text: { format: { type: "json_object" } } }` plus an explicit JSON instruction if no existing input contains `JSON` |

### Response Translation: `ResponsesResponse` → `AiResponse`

| `ResponsesResponse` | `AiResponse` |
|---|---|
| `output[].Message.content[].OutputText` | `content` (joined) |
| `output[].Message.content[].Refusal` | Logged as warning, discarded |
| `output[].FunctionCall` | Exposed as a Sashiko tool call while the complete original item is retained for continuation |
| `output[]` (all types) | Preserved as opaque provider metadata for the next request; no item type is silently dropped |
| `status == "incomplete"` | `truncated: true` |
| `usage.input_tokens` | `prompt_tokens` |
| `usage.output_tokens` | `completion_tokens` |
| `usage.input_tokens_details.cached_tokens` | `cached_tokens` when nonzero and no larger than `input_tokens` |

### Function Call ID Mapping

The Responses API uses two distinct identifiers for function calls:
- `id` — the output-item identifier
- `call_id` — the correlation key linking a `function_call` to its
  `function_call_output`

Sashiko retains the complete original function-call item so a later request
reuses both values exactly. Tool output uses the original `call_id`; the
provider must not infer or synthesize the output-item `id` from it.

### Stateless Continuation

When a response requests tools, the next request includes all of the prior
response's output items, followed by the corresponding `function_call_output`
items. In particular, reasoning items must survive this boundary. This lets the
provider remain stateless and safe to share across concurrent reviews while
still giving the Responses API the context it requires to continue a tool
workflow.

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
| Token limit | `max_tokens: N` |

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
    // Reads from ai.openai settings.
    // Rejects the legacy provider="openai" + only ai.openai_compat setup.
    // Creates openai_responses::OpenAiClient
}
"openai-compatible" => {
    // Reads from ai.openai_compat settings
    // Creates openai::OpenAiCompatClient
}
```

### Configuration Migration

`[ai.openai_compat]` is reserved for `provider = "openai-compatible"`. A
configuration that selects `provider = "openai"` but supplies only the legacy
`[ai.openai_compat]` table fails with an actionable migration error instead of
silently discarding its URL and token settings. Move those values to
`[ai.openai]`; replace a `/v1/chat/completions` URL with a `/v1/responses` URL
or omit `base_url` to use the default.

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
max_tokens = 65536
reasoning_effort = "medium"  # recommended default
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
