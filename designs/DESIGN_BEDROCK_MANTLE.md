# Bedrock OpenAI models on bedrock-mantle

OpenAI models on Amazon Bedrock fit the Converse API poorly: they reject
`temperature`, take reasoning effort only through pass-through fields, and
have no prompt-cache controls there. Bedrock also serves them through the
OpenAI Responses API on the `bedrock-mantle` endpoint, which supports
encrypted reasoning replay and explicit prompt-cache breakpoints.

## Routing

`provider = "bedrock"` keeps one configuration surface for every Bedrock
model. An OpenAI foundation model ID (`openai.gpt-...`) is served by
`MantleClient` (`src/ai/bedrock_mantle.rs`) unless `[ai.bedrock] openai_api =
"converse"` keeps it on `BedrockClient`. Mantle does not accept inference
profile IDs (`global.openai.*`) or ARNs, which work on Converse, so those stay
there with an info log rather than break existing configurations. Every other
model is unaffected, and the model ID is always sent unchanged.

## Endpoint and authentication

The endpoint is `https://bedrock-mantle.<region>.api.aws/v1/responses`, or
`mantle_base_url` for models served under `/openai/v1`. The signing region
comes from that URL, then `[ai.bedrock] region`, then the AWS default chain,
resolved once on first use.

Mantle authenticates with Amazon Bedrock API keys. Without
`AWS_BEARER_TOKEN_BEDROCK`, `bedrock_token.rs` derives a short-term key the
way the official `aws-bedrock-token-generator` does: a SigV4 query-presigned
`POST https://bedrock.amazonaws.com/?Action=CallWithBearerToken`, without its
scheme, base64-encoded behind `bedrock-api-key-`. The key is cached and
renewed an hour before the earlier of its own 12-hour expiry and the expiry
of the credentials it embeds, so temporary SSO or STS credentials are not
outlived. A 401 or 403 drops the cached key so the next request signs a
fresh one.

## Request flow

Requests are stateless (`store: false`) and ask for
`reasoning.encrypted_content`. `temperature` is never sent; `effort` becomes
`reasoning.effort`; JSON responses use `json_object` mode.

The Responses API wants reasoning items replayed in their original order
around messages and function calls. The response's output items, without a
trailing reasoning suffix the API would reject, are serialized under
`bedrock_mantle_output` in `AiResponse::thought_signature`. The session
already copies that field onto the assistant message it replays, and the
response cache key and review logs already scrub it, as they do for Gemini
signatures. On the next request the stored items replace the normalized
content and tool calls for that assistant turn. A signature without that key,
for example from another provider, falls back to the normalized fields.

## Errors

HTTP failures map onto `OpenAiCompatError`, so `classify_ai_error` and the
backoff provider treat them like the OpenAI providers: 429 is a rate limit
honouring `Retry-After`, 5xx and transport failures are transient, 401/403
and other 4xx are fatal. A response that is `incomplete` for any reason other
than `max_output_tokens` is an error; `max_output_tokens` marks the response
truncated.

## Prompt caching

With `[ai.bedrock] prompt_caching` (default on), requests carry
`prompt_cache_options` in `implicit` mode with the optional
`prompt_cache_ttl`, and an explicit `prompt_cache_breakpoint` on the last
`input_text` of the final system message and of the last user message.
Implicit mode adds Bedrock's own breakpoint on the latest message, which
covers the tool-call history; two explicit breakpoints keep the total within
Bedrock's four cache writes per request. Replayed output items and tool
results are never marked. Cache reads are reported as `cached_tokens`; cache
writes are only logged, since `AiUsage` has no field for them.

## Caching

`cache_identity` adds `api=mantle`, the region, the base URL, the effort and
the output cap to the model, so a response recorded through Converse is never
replayed for Mantle and vice versa. Prompt-cache settings change cost, not
answers, so they stay out of it.
