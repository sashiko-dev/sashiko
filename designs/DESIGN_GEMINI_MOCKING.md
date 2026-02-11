# Design: Gemini API Mocking for End-to-End Testing

## Objective
To enable robust end-to-end (E2E) testing of Sashiko's review pipeline without relying on the live Gemini API. This ensures tests are deterministic, fast, and cost-free, while also allowing simulation of edge cases (rate limits, errors).

## Strategy
We will use the `wiremock` crate to spin up a local HTTP server that mimics the Gemini API behavior. The `GeminiClient` in Sashiko will be configured to point to this mock server during tests.

## 1. Dependency Changes
Add `wiremock` to `[dev-dependencies]` in `Cargo.toml`.

```toml
[dev-dependencies]
wiremock = "0.6"
```

## 2. Code Refactoring (`src/ai/gemini.rs`)
The `GeminiClient` currently hardcodes the Google API URL. We need to make this configurable.

**Current:**
```rust
let url = format!(
    "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
    self.model, self.api_key
);
```

**Proposed:**
Add a `base_url` field to `GeminiClient`.

```rust
pub struct GeminiClient {
    api_key: String,
    model: String,
    base_url: String, // New field
    client: Client,
}

impl GeminiClient {
    pub fn new(model: String) -> Self {
        // ...
        let base_url = std::env::var("GEMINI_BASE_URL")
            .unwrap_or_else(|_| "https://generativelanguage.googleapis.com".to_string());
        // ...
    }
    // ...
}
```

## 3. Mock Server Setup (`tests/common/mod.rs` or `tests/e2e_gemini_mock.rs`)
We will create a helper to set up the mock server and return the `MockServer` instance along with the configured `GeminiClient` (or just the URL to pass to the app settings).

### Mock Response Templates

These are only two of the possible response templates, more can be captured by a trace:

**1. Generate Content (Success - Review)**
Simulate a model response providing a review verdict.

```json
{
  "candidates": [
    {
      "content": {
        "parts": [
          {
            "text": "{"analysis_trace": ["step1"], "verdict": "LGTM", "findings": []}"
          }
        ],
        "role": "model"
      },
      "finishReason": "STOP",
      "index": 0,
      "safetyRatings": []
    }
  ],
  "usageMetadata": {
    "promptTokenCount": 100,
    "candidatesTokenCount": 20,
    "totalTokenCount": 120
  }
}
```

**2. Generate Content (Function Call)**
Simulate the model asking to read a file.

```json
{
  "candidates": [
    {
      "content": {
        "parts": [
          {
            "functionCall": {
              "name": "read_file",
              "args": {
                "path": "kernel/sched/core.c"
              }
            }
          }
        ],
        "role": "model"
      },
      "finishReason": "STOP"
    }
  ]
}
```

## 4. Test Implementation Plan

1.  **Setup:**
    *   Start `MockServer`.
    *   Set `GEMINI_BASE_URL` environment variable to `mock_server.uri()`.
    *   Initialize `Sashiko` components (DB, Worker, etc.).

2.  **Define Expectations:**
    *   Use `Mock::given(method("POST"))` and `path_regex(...)` to match Gemini endpoints.
    *   `respond_with(ResponseTemplate::new(200).set_body_json(mock_response))`.

3.  **Execute:**
    *   Feed a dummy patch into the `Worker`.
    *   Run `worker.run()`.

4.  **Verify:**
    *   Assert that the worker produces the expected `WorkerResult`.
    *   Verify that the mock server received the expected requests (e.g., correct JSON schema in body).

## 5. Future Considerations
*   **Caching:** Mock the `cachedContents` endpoints to test the caching logic.
*   **Error Handling:** Simulate 429 (Quota Exceeded) and 500 errors to verify retry logic.
