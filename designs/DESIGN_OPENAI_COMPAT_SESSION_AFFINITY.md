# DESIGN: Session Affinity Header for OpenAI-Compatible Providers

Tracking issue: https://github.com/sashiko-dev/sashiko/issues/738

## Context

A review stage is a conversation with the model. Each turn sends the
whole conversation again, plus a few new messages. A server with prompt
caching can reuse the part it has already seen, so most of each turn
should come from cache.

Some inference services run many copies of a model and keep a separate
cache on each copy. A follow-up turn that lands on a different copy finds
nothing cached and pays full price. These services let the client name a
conversation in an HTTP header, and they send every request with the same
header value to the same copy.

One fixed value in `extra_headers` does not work, because it would send
every conversation to the same copy. Each stage conversation needs its own
value, and that value must stay the same for the whole conversation.

## Configuration

```toml
[ai.openai_compat]
session_affinity_header = "x-session-affinity"
```

Operators set this to the header name their provider documents. If it is
unset, Sashiko behaves exactly as before.

## Design

The change has three parts: create one ID per conversation, keep it out
of the response cache, and send it in the configured header. It adds no
new traits, wrapper types, or IPC message types.

### 1. Requests carry the ID (`src/ai/mod.rs`)

`AiRequest` gets one new field:

```rust
/// Random ID shared by every request of one stage conversation. It is
/// never part of the prompt; a provider may send it so the conversation
/// reaches a server with a warm cache.
#[serde(default, skip_serializing_if = "Option::is_none")]
pub conversation_id: Option<uuid::Uuid>,
```

The field is a `Uuid`, not a `String`, so that only a real UUID can be
stored in it. `uuid` is already a dependency; it only needs its `serde`
feature turned on.

In the daemon, each review runs in a child process that sends its
requests to the parent as JSON. The new field travels inside that JSON,
so the message format does not change. When the parent reads a request,
anything that is not a valid UUID is rejected, so a broken or hostile
value from the child can never end up in a header.

### 2. Each conversation gets one ID (`src/ai/session.rs`)

`SessionRunner::run` is the only production code that builds requests,
and each call to `run` is one stage conversation. At the start of `run`,
the runner creates one random ID with `uuid::Uuid::new_v4()`. It puts that
ID on every request it builds. As a result:

- Every turn, tool call, and retry inside the conversation uses the same
  ID, because they all happen inside the same `run` call.
- When a request is resent (after a network error, a rate limit, or an
  endpoint that rejects `temperature`), the resend is a copy of the same
  request, so it keeps the same ID.
- Different stages, stages running in parallel, and different reviews
  each make their own `run` call, so they each get their own ID.

The runner creates the ID even when no provider sends it. Generating one
random number per stage costs nothing noticeable, and the runner does not
need to know about provider settings.

### 3. The response cache ignores the ID (`src/ai/cache.rs`)

The local response cache looks answers up by the content of the request.
It already drops `context_tag` before it computes the lookup key. It now
drops `conversation_id` too. Without this, every request would have a new
random ID, so no cached answer would ever match.

The header name is also left out of the cache's provider identity. The
header only chooses which copy of the model answers, not what the answer
is. Turning the setting on or off therefore keeps old cache entries
usable.

Parts 2 and 3 must ship in the same commit. Otherwise the commit between
them would break cache replay.

### 4. The client sends the header (`src/ai/openai.rs`, `src/ai/mod.rs`)

The setting is an `Option<String>` in `OpenAiCompatSettings`. When the
provider is created at startup, a new builder method checks it:

```rust
pub fn with_session_affinity_header(mut self, name: Option<&str>) -> Result<Self>
```

Startup fails if:

- the name is not a valid HTTP header name, or
- `extra_headers` already sets the same header. Header names are not
  case-sensitive, so `X-Session-Affinity` and `x-session-affinity`
  conflict.

The conflict is caught no matter which setting is applied first. One
helper, `header_owner`, reports which setting already sends a header
name, and both `with_extra_headers` and `with_session_affinity_header`
call it.

For each request, if the setting is on and the request has an ID, the
client copies `extra_headers` and adds the header with the value
`sashiko-conversation-<uuid>`. Otherwise it sends `extra_headers`
unchanged, exactly as before. The code that builds the JSON body never
reads the ID, so the ID cannot appear in the prompt.

With `api = "responses"`, startup fails if the setting is present, the
same way it already does for `extra_headers`.

## Decisions

- **The header value is `sashiko-conversation-<uuid>`.** The prefix lets
  people recognize Sashiko requests in gateway and provider logs. The
  UUID part is random, so the value says nothing about the patchset,
  stage, or host. Only the client adds the prefix, and the request stores
  the bare `Uuid`.
- **`api = "responses"` refuses the setting.** That client does not
  support extra headers today, and that endpoint is OpenAI's own.
  Support can be added when someone needs it.
- **No list of forbidden header names.** As with `extra_headers`, the
  operator is trusted to name the header their provider documents.

## Tests

- **End to end** (`src/ai/openai.rs`): build the provider from settings
  with the factory, run two sessions that each make a request and a
  retry, and check what a local test server receives. Every request has
  the header; a retry keeps its session's ID; the two sessions have
  different IDs. Deleting the factory's builder call makes this test
  fail.
- **One client, many conversations** (`src/ai/openai.rs`): send
  conversations A, B, A, then a request with no ID, through one client,
  and check the exact header on each. A client without the setting sends
  no header. No request body contains an ID.
- **Same ID within a conversation** (`src/ai/session.rs`): a tool turn,
  a rejected answer, and the retry all carry the same ID.
- **Different IDs for overlapping conversations** (`src/ai/session.rs`):
  the mock provider holds each request until a second one arrives, so the
  test only finishes if two sessions really run at the same time. They
  get different IDs.
- **Child to parent** (`src/reviewer.rs`): a scripted fake review child,
  the pattern the existing reviewer tests use, sends one request with an
  ID and one without. The parent passes `Some(id)` and `None` on to the
  provider.
- **JSON format** (`src/ai/mod.rs`): a request keeps its ID when written
  to JSON and read back. A request without the field reads as `None`. A
  value that is not a UUID is rejected.
- **Startup checks** (`src/ai/openai.rs`, `src/ai/mod.rs`): an invalid
  or empty name is rejected. A clash with `extra_headers` is rejected in
  either setter order and in any letter case. Setting the name twice
  replaces it. `api = "responses"` refuses the setting.
- **Response cache** (`src/ai/cache.rs`, `src/ai/openai.rs`): requests
  that differ only in their ID have the same cache key. The provider's
  cache identity is the same with and without the setting.
