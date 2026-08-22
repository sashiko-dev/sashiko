# AI Provider and Cost Boundaries

Provider responses, tool calls, and usage metadata are untrusted external data.
Validate schemas and bounds, redact secrets from errors, and preserve provider
capability differences. Token and output budgets must include the intended
cached/uncached quantities without underflow or double counting.

Rate-limit and transient retries must honor cancellation and deadlines. Tests
must use deterministic fakes and must not require credentials, live models,
network access, or paid quota.
