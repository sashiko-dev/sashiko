# DESIGN: Claude Provider

## Context

Sashiko accesses Claude models through multiple paths: the direct Anthropic
Messages API, the Claude Code CLI, AWS Bedrock (Converse API), and Google
Cloud Vertex AI (rawPredict). This document covers the direct API and CLI
providers. Vertex AI and Bedrock have their own design documents.

## Architecture

### Direct API (`src/ai/claude.rs`)

`ClaudeClient` sends HTTP requests to the Anthropic Messages API
(`https://api.anthropic.com/v1/messages`). The client handles:

- Request/response translation between Sashiko's generic `AiRequest`/`AiResponse`
  and Claude's wire format (`ClaudeRequest`/`ClaudeResponse`)
- Prompt caching via ephemeral cache control markers
- Thinking via `ThinkingConfig`, and effort via `OutputConfig`
- Rate limit and overload retry signaling

#### Wire Format Types

All Claude-compatible providers (direct API, Vertex AI) share these types:

| Type | Purpose |
|------|---------|
| `ClaudeRequest` | Top-level request body (model, messages, system, tools, thinking, output_config) |
| `ClaudeMessage` | A message with role and content blocks |
| `ClaudeContent` | Tagged enum: Text, Thinking, ToolUse, ToolResult |
| `ClaudeResponse` | Response with content blocks and usage |
| `ThinkingConfig` | Thinking type |
| `OutputConfig` | Effort level |
| `SystemBlock` | System prompt text with optional cache control |
| `ClaudeTool` | Tool definition with input schema |
| `ClaudeError` | Typed errors: rate limit, overload, invalid request, auth |

#### Translation Functions

These are `pub` for reuse by Vertex AI:

- `translate_ai_request()` -- converts `AiRequest` to `ClaudeRequest`
- `translate_ai_response()` -- converts `ClaudeResponse` to `AiResponse`
- `apply_cache_control()` -- adds ephemeral cache markers to last system/tool/message
- `estimate_tokens_generic()` -- token estimation using cl100k_base tokenizer

#### Thinking and Effort

The two settings go to different places in the request. The thinking mode is
`ThinkingConfig`, serialized as `"thinking": {"type": "adaptive"}`. Effort is
`OutputConfig`, a top-level `"output_config": {"effort": "high"}`; the API
rejects an effort nested inside `thinking`.

`ClaudeRequest.thinking` and `ClaudeRequest.output_config` are both `Option`s
with `skip_serializing_if = "Option::is_none"`, each set only when its setting
is. An unset thinking mode therefore emits no `thinking` object at all: the API
rejects one without a `type`.

#### Authentication

Uses `ANTHROPIC_API_KEY` or `LLM_API_KEY` environment variables. Sent as
`x-api-key` header alongside `anthropic-version: 2023-06-01`.

### Prompt Caching

Cache control markers are applied to the last element of each category:
1. Last system block
2. Last tool definition
3. Last message content block

The Anthropic API provides a 5-minute TTL ephemeral cache. Cached tokens
appear in the response usage as `cache_read_input_tokens` and
`cache_creation_input_tokens`.

### Claude CLI Provider (`src/ai/claude_cli.rs`)

`ClaudeCliProvider` shells out to the `claude --print` command, which runs
in text-completion mode (no tools, no file access, no session persistence).
The CLI reads a prompt from stdin and writes JSON to stdout.

#### Prompt Construction

`build_prompt()` constructs a text prompt with XML-style tags:
- `<system>` for system prompts
- `<user>`, `<assistant>`, `<tool_result>`, `<tool_call>` for messages
- `<available_tools>` for tool definitions
- `RESPONSE FORMAT` instructions for JSON output

JSON format instructions are emitted when:
1. Tools are present (tool_calls/content format)
2. `response_format` is `AiResponseFormat::Json` without tools (schema-aware)

#### Error Handling

The CLI writes JSON to stdout even on non-zero exit. The provider reads stdout
before checking exit status and extracts the `result` field from the JSON
for structured error messages.

## Settings

```toml
[ai.claude]
prompt_caching = true              # Default: true
max_tokens = 32768                 # Default: 32768; max output tokens, thinking included
base_url = "https://..."           # Override API endpoint
thinking = "adaptive"              # Sent as thinking.type
effort = "high"                    # Sent as output_config.effort: "low", "medium", "high", "xhigh", "max"
```

## Error Handling

| HTTP Status | Error Type | Retry Strategy |
|-------------|-----------|----------------|
| 429 | RateLimitExceeded | Use Retry-After header, default 60s |
| 529 | OverloadedError | Exponential backoff starting at 5s |
| 400 | InvalidRequest | No retry |
| 401/403 | AuthenticationError | No retry |

## Testing

Tests are in `src/ai/claude.rs` and `src/ai/claude_cli.rs`:
- Thinking and effort serialization (each omitted when unset, effort at the top level)
- Request translation (system, user, assistant, tool calls, tool results)
- Response translation (text, tool calls, thinking, usage with cache)
- Cache control (applied when enabled, absent when disabled)
- `build_prompt()` format instructions (with/without tools, with/without JSON format)
