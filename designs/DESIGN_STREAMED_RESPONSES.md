# Streamed responses for HTTP providers

Tracking issue: #947. Related: #572.

## Why stream replies

A proxy with an HTTP idle timeout can close a request while the model is
still generating a non-streamed reply. When Sashiko retries, it starts
and pays for a new generation that can hit the same timeout. Streaming
allows the endpoint to send events during generation. TCP keepalive
(#916) cannot reset an HTTP idle timer.

Streaming is opt-in for Claude Messages and for both OpenAI transports.
See the [provider guide](../docs/llm-providers.md) for configuration.
OpenAI's `api_timeout_secs` still covers the whole request, and Claude
still sets no total request timeout. Stages receive a single response
after assembly, and retries start the request again.

## Reuse response handling

Use the SSE path for a successful `text/event-stream` reply to a request
that asked for streaming. An endpoint that returns JSON uses the usual
response path. Each transport reconstructs the JSON document its
non-streamed endpoint would return and passes it to the existing
response decoder and translator. This keeps validation, truncation and
usage accounting in the same place. The streaming flag stays out of the
response cache key because both paths yield the same response shape.

The [shared SSE reader](../src/ai/sse.rs) dispatches only events closed
by a blank line, even when an event spans several HTTP chunks. Once a
handler signals completion, the reader can return without waiting for
the server to close the connection. A dropped body or an EOF before
completion is transient. Malformed events and illegal event sequences
are protocol errors. Provider-reported failures use the provider's
existing error classification.

## What completes a reply

### OpenAI Responses

`response.completed`, `response.incomplete`, and `response.failed`
carry the full response object. Intermediate events can be skipped. An
`error` event is converted to a failed response so it goes through the
same classification. `server_error` can retry with the caller's
backoff, and `rate_limit_exceeded` uses the default rate-limit delay.

### OpenAI Chat Completions

Chat reconstructs each choice from its text, reasoning and tool-call
deltas. It requests `stream_options.include_usage` for token accounting.
To support compatible servers that reuse a tool index, a new tool-call
ID starts a new call even when its index matches the previous one.

`[DONE]` is the normal terminator. Some compatible servers close the
connection without it. At EOF, accept the reply only when at least one
choice exists and every choice has a finish reason other than `error`.
The final usage event can be missing on this path.

### Anthropic Messages

The content-block lifecycle determines whether a reply is complete.
The [state machine](../src/ai/claude_stream.rs) validates event order and
block indices from `message_start` through `message_stop`. Unknown event
types can be skipped. An unknown delta for an active block is a
protocol error because losing it could change the reconstructed block.

Tool-call input is parsed when its block closes. An unparseable input
fragment is allowed only in the last block of a reply whose stop reason
is `max_tokens`. This lets the session handle the reply as truncated.

## Verification

Test SSE framing across chunk boundaries and exercise each transport
through its HTTP entry point with streamed success, an interrupted
body, an in-stream error and a JSON fallback. Compare the decoded
streamed and non-streamed replies to verify that they produce the same
`AiResponse`. Cover Claude's state transitions separately.
