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
