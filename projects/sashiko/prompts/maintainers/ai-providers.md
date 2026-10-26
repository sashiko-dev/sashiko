+++
title = "AI Providers, Decorators, Caching, and Quota Policy"
paths = [
    "src/ai/",
    "src/local_review.rs",
    "src/reviewer.rs",
]
symbols = [
    "AiProvider",
    "AiErrorClass",
    "BackoffProvider",
    "CachingAiProvider",
    "ConcurrencyLimitedProvider",
    "LoggingProvider",
    "QuotaManager",
    "TokenBudget",
    "classify_ai_error",
    "create_provider_cached",
    "decorate_provider",
]
+++

## Provider Decorator Stack Policy

- Maintain the decorator wrapping order from outermost to innermost:
  `BackoffProvider` -> `ConcurrencyLimitedProvider` -> `LoggingProvider` ->
  `CachingAiProvider` -> concrete provider.
  - `BackoffProvider` must remain outside `ConcurrencyLimitedProvider` so tasks
    sleeping on a rate-limit or transient backoff window never hold a
    concurrency semaphore permit.
  - `CachingAiProvider` must remain innermost (wrapping the concrete provider
    directly), or any outer decorator placed inside it must explicitly forward
    `cache_identity` and `cache_stats` so model/effort/token knobs are never
    dropped from the response-cache key.
- Any new provider-shaping configuration knob that alters model responses
  without appearing in `AiRequest` JSON (such as thinking budget, reasoning
  effort, or output token cap) must be included in `cache_identity`.
- `stdio-*` IPC providers must bypass local rate-limit/concurrency decorators in
  worker subprocesses because throttling is enforced by the parent daemon.

## Error Classification and Retry Policy

- Any new provider error type must be wired into `classify_ai_error` so HTTP 429
  (`RateLimit`) and 5xx/529 (`Transient`) responses are not misclassified as
  `Fatal`.
- Authentication, permission, and malformed-request errors must classify as
  `Fatal` so retries do not burn quota on permanent failures.
- When formatting transport or HTTP client errors, require `{:#}` formatting so
  the underlying cause chain is preserved, while redacting any embedded URL
  credentials or API keys (`redact_secret`).
- Provider and decorator log lines emitted during concurrent review execution
  should include `get_log_prefix()` so warnings remain attributable to a patch
  and stage.

## Token Budgeting and Subprocess Providers

- Sashiko deliberately uses the `TokenBudget::BYTES_PER_TOKEN` byte heuristic
  rather than external tokenizer crates (`tiktoken` must not be added to
  `Cargo.toml`). Do not flag the byte-per-token heuristic as inaccurate unless a
  change breaks UTF-8 char-boundary safety or overflows the byte budget.
- Subprocess CLI providers (`claude_cli`, `codex_cli`, `gemini` proxy/ACP,
  `copilot_cli`, `devin_cli`, `kiro_cli`, `goose_cli`) must:
  - configure `.kill_on_drop(true)` on spawned child processes;
  - pass large prompts via `stdin` rather than command-line `argv`;
  - pin non-interactive / completion-only environment flags *after* applying
    user-supplied environment maps, and redirect tool state directories out of
    `$HOME`.
