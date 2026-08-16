# DESIGN: A /v1/responses Transport for the OpenAI Provider

## Context

`provider = "openai"` reaches OpenAI over `/v1/chat/completions`, the
endpoint `OpenAiCompatClient` has spoken since it was written. OpenAI now
serves its reasoning models over a second endpoint, `/v1/responses`, and
two constraints make that endpoint the only one that runs a review.

The gpt-5.6 family rejects any request that carries function tools unless
`reasoning_effort` is `"none"`. Omitting the field does not help: the
server applies its default effort and rejects anyway. Sashiko attaches the
toolbox only on the code-exploration stages, so the standing workaround --
`effort = "none"` in Settings.toml -- buys working reviews by running the
stages that matter with no reasoning at all. The same request on
`/v1/responses` carries tools and an effort together and is accepted.

The gpt-5.4 family is a separate case. Access is granted per endpoint. A
project can hold the grant, list all four models on `GET /v1/models`, and
still get "Project ... does not have access" from `/v1/chat/completions`
while `/v1/responses` answers normally. Without this transport those
models are unreachable.

Neither constraint applies to the third-party endpoints sashiko supports
through `provider = "openai-compatible"`. OpenRouter, LM Studio, glm,
moonshot, and MiniMax implement chat completions and not `/v1/responses`,
so the chat transport stays and stays the default.

## Design Decisions

| Decision | Choice |
|---|---|
| Client architecture | A second client, `OpenAiResponsesClient` in `src/ai/openai_responses.rs`, rather than a mode flag inside `OpenAiCompatClient` |
| Shared code | `src/ai/openai_common.rs` holds what does not depend on the endpoint: the error enum, HTTP client construction, URL normalization, status classification, and the resend that drops a refused temperature |
| Transport selection | `api = "chat" \| "responses"` in `[ai.openai_compat]`, read by `create_provider_from_ai`; defaults to `"chat"` |
| Conversation state | None. `store: false` on every request, no `previous_response_id` |
| Reasoning continuity | The reasoning items come back through `AiMessage::thought_signature` and `ToolCall::thought_signature`, and are replayed on the next turn in front of the item each one produced |
| Streaming | Not used. Sashiko consumes whole responses |

### Why a second client rather than a mode flag

The two endpoints differ in every field that matters. The request carries a
different envelope, the reply is an array of typed items rather than a
choice, and the usage keys are spelled differently. A mode flag inside
`build_request` and `parse_response` would put two wire formats behind one
set of structs and one set of tests, and the chat path is the one that must
not regress: it serves every non-OpenAI endpoint sashiko supports.

### Why a settings flag rather than a provider name

A `provider = "openai-responses"` value would be more visible in a config
file, but it forces either a duplicate settings block or a provider that
reads its configuration from a section named after a different provider.
The flag keeps one settings block for one vendor. Everything else in that
block -- `base_url`, `context_window_size`, `max_tokens`, `effort` --
applies to both transports unchanged.

## Request Mapping

| `AiRequest` | chat/completions | responses |
|---|---|---|
| `system` | `messages[0]`, role `system` | `instructions`, top level |
| `messages` | `messages` | `input` array |
| `tools` | `{type, function: {name, description, parameters}}` | `{type: "function", name, description, parameters}`, flat |
| `temperature` | `temperature` | `temperature` |
| effort, from settings | `reasoning_effort` | `reasoning.effort` |
| `max_tokens`, from settings | `max_completion_tokens` | `max_output_tokens` |
| `response_format` | `response_format` | `text.format` |

User and assistant messages map straight across as input items of type
`message`. Tool results are the exception. Chat sends a `tool` role message
carrying `tool_call_id`; responses sends an item of type
`function_call_output` keyed by `call_id`.

A returned tool call carries two identifiers, an `id` (`fc_...`) and a
`call_id` (`call_...`). The `function_call_output` must reference the
`call_id`. Feeding back the `id` is a silent mismatch, so `ToolCall::id`
holds the `call_id`.

`store` is set to `false` explicitly. It defaults to true, which retains
the request server-side. A patch review bot should not leave copies of its
prompts on the vendor's side by default.

`include` carries `reasoning.encrypted_content`, which is what makes the
reasoning items come back in a form that can be replayed when nothing is
stored server-side.

## Response Mapping

The reply is an `output` array of typed items rather than a single message.
The types sashiko acts on are `reasoning`, `message`, and `function_call`;
any other item type is ignored rather than failing the response.

- `message` -> `AiResponse::content`
- `function_call` -> `AiResponse::tool_calls`, mapping `call_id` to
  `ToolCall::id`, `name` to `function_name`, and parsing the `arguments`
  string into `ToolCall::arguments`
- `reasoning` -> `AiResponse::thought`, plus a thought signature on the
  item it precedes

The reasoning mapping needs no new types. `AiMessage` and `AiResponse`
already carry `thought` and `thought_signature` for Gemini's thought
signatures, and `ToolCall` carries one per call, and a reasoning item is
the same idea: an opaque blob handed back on the next turn so the model
keeps its chain across a tool call. The items are stored as their own JSON
in a signature and spliced back into `input`. The summary text, when the
request asked for one, goes in `thought`.

Which signature carries them is the subtle part. The endpoint reads a
reasoning item as belonging to the item that follows it, and rejects a
request that files one anywhere else: "Item 'rs_...' of type 'reasoning'
was provided without its required following item", a 400, which the
classifier calls fatal. A model that thinks between calls returns
`[reasoning, function_call, reasoning, function_call]`, so collecting a
turn's reasoning into one block ahead of its calls breaks the second pair.

Each reasoning item therefore travels on the item it produced: the ones
ahead of the message go in `AiResponse::thought_signature`, and the ones
ahead of a call go in that `ToolCall::thought_signature`. The replay walks
the same order back out. Reasoning that produced nothing is dropped, which
covers a trailing item, a turn that produced neither text nor calls, and a
message signature arriving with no message.

Usage keys:

```
input_tokens                           -> AiUsage::prompt_tokens
output_tokens                          -> AiUsage::completion_tokens
total_tokens                           -> AiUsage::total_tokens
input_tokens_details.cached_tokens     -> AiUsage::cached_tokens
output_tokens_details.reasoning_tokens -> logged; no field today
input_tokens_details.cache_write_tokens -> logged; no field today
```

`cached_tokens` here is already a breakdown of `input_tokens` rather than
an addend, which is what the `AiUsage` doc comment requires, so it passes
through unadjusted. A count larger than `input_tokens` is dropped, as on
the chat path.

Truncation arrives as `status: "incomplete"` with
`incomplete_details.reason` of `"max_output_tokens"`. That sets
`AiResponse::truncated`, which the session loop already acts on. The third
status, `"failed"`, is a failure the endpoint reports in a 200 reply; see
"Errors".

## Statelessness

`previous_response_id` is not used. Sashiko resends the whole conversation
on every turn, and its response cache is keyed on request content.
Server-side threading would make a request's meaning depend on hidden state
the cache cannot see, and a cache hit would replay an answer built from a
different history. Every request stays self-describing.

This is also why the reasoning items have to round-trip through
`thought_signature`. With no server-side thread, the encrypted blob is the
only continuity across a tool call.

## Cache Identity

`OpenAiResponsesClient::cache_identity` names the endpoint alongside the
model, `max_tokens`, `base_url`, and effort. Two clients pointed at the
same model over different endpoints do not produce the same reply -- one of
them cannot reach the gpt-5.4 family at all -- so their entries must not
collide.

## Errors

`OpenAiCompatError` and its `ClassifyAiError` impl are shared. Status
handling is identical on both endpoints: 429 with the same retry-after
parse, 401 and 403 as authentication failures, 5xx as transient,
everything else fatal.

This endpoint also fails inside a 200. `status` is then `"failed"` and
`error` carries a code and a message, alongside an empty `output`. That
reply has to be recognized on arrival. Everything downstream reads it as a
reply that held nothing. The response translation bails on the empty
array, and an anyhow error the classifier cannot downcast is fatal by
default, so a failure that would have been retried ends the review
instead.

A `server_error` reported this way is the failure a 5xx reports, so it
maps to `TransientError` and retries on the same terms. Every other code
maps to `ResponseFailed`, a variant of `OpenAiCompatError` that classifies
fatal and carries the endpoint's own message into the log. That message is
what a dead review is diagnosed from.

The valid set of effort levels is model-dependent, and a level a model
rejects returns a 400, which classifies fatal and ends the review.
gpt-5.4-pro takes only "medium" and "high"; gpt-5.6 with tools takes only
"none". The setting is passed through verbatim and documented as
model-dependent rather than validated against a fixed list that no model
matches.

## Testing

Unit tests in `src/ai/openai_responses.rs`, mirroring the layout in
`openai.rs`:

1. Request translation: the system prompt reaches `instructions`, tools are
   flat, effort is nested under `reasoning`, `max_output_tokens` is set, and
   `store: false` is present.
2. Tool-result round trip: an `AiMessage` carrying `tool_call_id` becomes a
   `function_call_output` keyed on `call_id`.
3. Response parsing, from payloads recorded against the live API in
   `src/ai/testdata/`, for each output shape: message alone, reasoning plus
   message, `function_call`, and the reasoning plus `function_call` case a
   tool stage hits.
4. Reasoning placement, through both translations: a turn with two calls
   and reasoning ahead of each replays each blob in front of its own call.
5. A 200 reply with `status: "failed"`: the error message survives, and
   the code decides whether the class is transient or fatal.
6. Usage mapping, asserting `cached_tokens <= prompt_tokens` so the
   breakdown contract is enforced by a test rather than by a comment.
7. Truncation: `status: "incomplete"` sets `truncated`.
8. `cache_identity` differs between a chat client and a responses client
   configured identically otherwise.

## Configuration Example

```toml
[ai]
provider = "openai"
model = "gpt-5.4"

[ai.openai_compat]
api = "responses"
max_tokens = 65536
effort = "medium"
```
