# AI Provider and Cost Boundaries

Provider responses, tool calls, and usage metadata are untrusted external data.
Validate schemas and bounds, redact secrets from errors, and preserve provider
capability differences. Preserve the documented budget accounting:
`cached_tokens` is part of `prompt_tokens`, the cumulative total charges
uncached input plus output, and the output budget charges output. Check for
underflow and double counting.

Response-cache keys must cover the complete request and every provider setting
outside the request that can shape a response. A cache hit must preserve the
usage-field contract seen by quota and budget enforcement.

Rate-limit and transient retries must honor cancellation and deadlines. Tests
in ordinary unit and PR checks must use deterministic fakes and must not require
credentials, live models, external network access, or paid quota. Keep any
explicitly authorized provider or integration evidence opt-in and separate from
those deterministic checks.
