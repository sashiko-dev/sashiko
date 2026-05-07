# AI Provider Error Handling and Rate Limiting Design

## Objective
Sashiko interacts with external LLM providers (like Gemini and Claude) which can occasionally return transient errors (e.g., 503 Service Unavailable, 529 Overloaded, or connection timeouts) or rate-limiting errors (429 Too Many Requests). 

To ensure robustness and high availability, Sashiko implements a unified, configurable error handling and rate limiting strategy with the following goals:
1.  **Unified Error Handling:** All transient and rate-limiting errors from any provider trigger a consistent backoff/delay mechanism.
2.  **Configurable Scope (Global vs. Local):** Support both global rate limiting (blocking all workers to protect API quota) and local rate limiting (per-worker backoff to maximize concurrency).
3.  **Flexible Backoff Strategies:** Support both exponential backoff (for gradual recovery) and flat backoff (for aggressive, predictable retries).
4.  **Infinite Retries for Transients:** Transient errors are retried indefinitely to ensure reviews eventually complete.
5.  **Timeout Exemption:** Time spent waiting for rate limits or transient backoffs does *not* count towards review timeouts. The active deadline is dynamically extended by the duration of the sleep.

## Architecture

### 1. AI Rate Limiter / Quota Manager (`QuotaManager`)
The `QuotaManager` (in `src/ai/quota.rs`) manages the backoff state:
*   It tracks a `blocked_until` timestamp.
*   It tracks consecutive transient failures to implement exponential backoff.
*   It provides `wait_for_access()` which blocks the caller if the rate limit is active, returning the `Duration` spent sleeping.
*   It supports two backoff types for transient errors:
    *   **Exponential:** Backoff starts at 1s and doubles on consecutive failures (`1s, 2s, 4s, 8s...`) capped at 60 seconds.
    *   **Flat:** Backoff always waits a configured flat duration (e.g., 30 seconds).

### 2. Global vs. Local Backoff
The scope of the backoff is determined by how the `QuotaManager` is shared:
*   **Global Backoff (`global_backoff = true`):** A single `QuotaManager` is shared as a singleton (`Arc<QuotaManager>`) across all workers. If one worker hits a rate limit or transient error, it blocks *all* workers. This is useful to protect API keys from getting banned under severe quota limits.
*   **Local Backoff (`global_backoff = false`):** Each review task instantiates its own local `QuotaManager`. If a worker handling a review hits an error, it backs off individually without affecting other active workers. This maximizes throughput and retries "as soon as possible" across different reviews.

### 3. Configuration (`Settings.toml`)
The behavior is fully configurable under the `[ai]` section:
```toml
[ai]
# Scope of the backoff
global_backoff = false # If false, workers back off independently (local backoff)

# Quota (429) error handling
quota_backoff_secs = 60 # Flat delay when hitting 429

# Transient (503, timeout) error handling
transient_backoff_type = "flat" # "exponential" or "flat"
transient_flat_backoff_secs = 30 # Flat delay when using "flat" transient backoff
```

### 4. Standardized Error Categorization (`AiError`)
To avoid fragile string-matching in the orchestration layer (`reviewer.rs`), Sashiko abstracts provider-specific errors at the API boundary.

We introduce a unified `AiError` enum in `src/ai/mod.rs`:
```rust
#[derive(Debug, thiserror::Error, Clone)]
pub enum AiError {
    #[error("Quota exceeded: retry after {0:?}")]
    QuotaExceeded(std::time::Duration),
    #[error("Transient error: {1}, retry after {0:?}")]
    Transient(std::time::Duration, String),
    #[error("Fatal AI error: {0}")]
    Fatal(String),
}
```

Each provider client (Gemini, Claude, OpenAI) is responsible for mapping its raw HTTP status codes or client-specific errors into this standardized enum before returning them:
*   **Gemini:** Maps `GeminiError::QuotaExceeded` and `GeminiError::TransientError`.
*   **Claude:** Maps `ClaudeError::RateLimitExceeded` and `ClaudeError::OverloadedError`.
*   **OpenAI:** Maps `OpenAiCompatError::RateLimitExceeded` and `OpenAiCompatError::TransientError`.

In `reviewer.rs`, the orchestration layer downcasts the returned `anyhow::Error` to `AiError`. This allows robust, type-safe classification to trigger the appropriate backoff:
*   `AiError::QuotaExceeded(delay)`: Triggers `report_quota_error(delay)`.
*   `AiError::Transient(delay, msg)`: Triggers `report_transient_error()`.
*   `AiError::Fatal(msg)`: Fails the review immediately (no retries).

A fallback string-matching mechanism is preserved only for backward compatibility with custom or legacy external tools.

### 5. Worker Timeout Exemption (Dynamic Deadline)
To prevent reviews from timing out while waiting for API recovery, the review deadline is dynamically extended.

In `src/reviewer.rs` (`run_review_tool`), the active timeout is managed using a mutable `deadline` (`TokioInstant`):
```rust
let mut deadline = TokioInstant::now() + Duration::from_secs(settings.review.timeout_seconds);
```

During the AI interaction loop, before each request, the worker calls `wait_for_access()` and extends the deadline by the slept duration:
```rust
let slept = quota_manager.wait_for_access().await;
deadline += slept; // Extend deadline by the time we slept!

if TokioInstant::now() > deadline {
    return Err(anyhow!("Review tool timed out (active time exceeded)"));
}
```

This ensures that the `timeout_seconds` config applies strictly to **active processing time** (e.g., waiting for child process I/O, local processing), while all time spent waiting for external AI provider availability is exempt.