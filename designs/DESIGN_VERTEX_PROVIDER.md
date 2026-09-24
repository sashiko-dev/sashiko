# DESIGN: Vertex AI Provider

## Context

Google Cloud Vertex AI is GCP's managed model hosting platform, equivalent
to AWS Bedrock. It provides access to Claude, Gemini, and other model
families through Google Cloud infrastructure with IAM-based authentication.

Unlike Bedrock's unified Converse API, Vertex AI uses per-publisher wire
formats: Claude models use `rawPredict` with Claude's native Messages API
format, while Gemini models use `generateContent` with Gemini's format.

The Vertex provider is a model-agnostic routing layer that handles shared
concerns (authentication, endpoint construction) and delegates wire-format
translation to existing provider modules.

## Architecture

```
AiRequest
    |
    v
VertexClient
    |
    +-- detect_model_family(model_name)
    |       |
    |       +-- Claude  --> claude::translate_ai_request()
    |       |                  --> VertexClaudeRequest (no model, + anthropic_version)
    |       |                  --> HTTP POST rawPredict
    |       |                  --> claude::translate_ai_response()
    |       |
    |       +-- Gemini  --> gemini::translate_ai_request()
    |       |                  --> GenerateContentRequest (unchanged)
    |       |                  --> HTTP POST generateContent
    |       |                  --> gemini::read_generate_content_response()
    |       |                  --> gemini::translate_ai_response()
    |       |
    |       +-- Others  --> (future: per-publisher translation)
    |
    +-- Google OAuth (ADC) --> Authorization: Bearer {token}
    |
    v
AiResponse
```

### Comparison with Bedrock

| Aspect | Bedrock (AWS) | Vertex AI (GCP) |
|--------|--------------|-----------------|
| Wire format | Unified Converse API (all models) | Per-publisher (rawPredict, generateContent) |
| Auth | AWS IAM SDK (aws-config crate) | Google OAuth ADC (google-cloud-auth crate) |
| Code reuse from providers | 0% -- full custom translation | ~95% -- reuses existing translation functions |
| Model-agnostic | Yes (Converse handles all) | Yes (routing layer dispatches per family) |
| Feature flag | `bedrock` | `vertex` |
| Dependencies | 3 AWS SDK crates | 1 auth crate (google-cloud-auth) |

### Model Family Detection

The `detect_model_family()` function maps model names to families:

```rust
enum ModelFamily {
    Claude,   // claude-* --> publishers/anthropic, rawPredict
    Gemini,   // gemini-* --> publishers/google, generateContent
    // Future:
    // Llama,  // llama-*  --> publishers/meta, rawPredict
}
```

### Shared Auth Layer

All model families use the same authentication: Google Cloud Application
Default Credentials (ADC) via the `google-cloud-auth` crate. The crate
handles credential discovery, token refresh, and caching internally.

Token acquisition:
1. `Builder::default().build_access_token_credentials()` at construction
2. `credentials.access_token().await?.token` per request
3. Sent as `Authorization: Bearer {token}` header

## Supported Model Families

### Claude (implemented)

| Property | Value |
|----------|-------|
| Publisher | `anthropic` |
| API method | `rawPredict` |
| Wire format | Claude Messages API (same as direct API) |
| Request wrapper | `VertexClaudeRequest` (no `model`, adds `anthropic_version`) |
| Translation | Reuses `claude::translate_ai_request/response()` |

Vertex API differences from direct Claude API:
1. `model` is NOT in the request body -- specified in URL only
2. `anthropic_version: "vertex-2023-10-16"` is in the request body (not a header)
3. Auth: Bearer token (not `x-api-key`)
4. No `anthropic-version` header

Context window on Vertex:
- 1M tokens: Claude Opus 4.7, Opus 4.6, Sonnet 4.6
- 200K tokens: Sonnet 4.5, Sonnet 4, Haiku 4.5, older models

### Gemini (implemented)

| Property | Value |
|----------|-------|
| Publisher | `google` |
| API method | `generateContent` |
| Wire format | Gemini generateContent (same as the API-key endpoint) |
| Request wrapper | None -- the body is what `gemini.rs` produces |
| Translation | Reuses `gemini::translate_ai_request/response()` |
| Response handling | Reuses `gemini::read_generate_content_response()` |

Vertex API differences from the Gemini API-key endpoint:
1. Host and path are Vertex's, and `model` is in the URL only
2. Auth: ADC Bearer token (not an API key)

Context window on Vertex: 1M tokens, matching the API-key path.

`prompt_caching`, `max_tokens`, `thinking` and `effort` in `[ai.vertex]` are
Claude-only and ignored here.

## Endpoint Types

Vertex AI offers three endpoint types, with pricing implications:

| Type | Region value | URL pattern | Premium |
|------|-------------|-------------|---------|
| Global (recommended) | `"global"` | `https://aiplatform.googleapis.com/...` | None |
| Multi-region | `"us"` or `"eu"` | `https://aiplatform.{region}.rep.googleapis.com/...` | 10% |
| Regional | e.g. `"us-east5"` | `https://{region}-aiplatform.googleapis.com/...` | 10% |

Full URL pattern:
```
{base}/v1/projects/{PROJECT}/locations/{REGION}/publishers/{PUBLISHER}/models/{MODEL}:{METHOD}
```

## User Setup Guide

### Prerequisites

1. A Google Cloud project with billing enabled
2. Vertex AI API enabled: `gcloud services enable aiplatform.googleapis.com`
3. Model access enabled in the [Vertex AI Model Garden](https://cloud.google.com/model-garden)
4. `gcloud` CLI installed

### Authentication

```bash
gcloud auth application-default login
```

This creates credentials at `~/.config/gcloud/application_default_credentials.json`
that the `google-cloud-auth` crate discovers automatically.

### Environment Variables

```bash
export ANTHROPIC_VERTEX_PROJECT_ID="my-gcp-project"
export CLOUD_ML_REGION="us-east5"  # Regional endpoint, or "global"
```

`GOOGLE_CLOUD_PROJECT` and `GOOGLE_CLOUD_LOCATION` are consulted after those
two. All four can alternatively be set in `[ai.vertex]` in Settings.toml,
which outranks the environment.

**Note on endpoint selection**: `global` routes dynamically and carries no
pricing premium. Regional endpoints (e.g., `us-east5`) pin traffic to one
geography for data residency or provisioned throughput, at a 10% premium.

### Settings.toml Configuration

```toml
[ai]
provider = "vertex"
model = "claude-sonnet-4-6"
max_input_tokens = 40000

[ai.vertex]
# project_id = "my-gcp-project"  # Falls back to the env vars above
# region = "us-east5"            # Falls back to the env vars above
prompt_caching = true
# thinking = "enabled"
# effort = "high"
```

For Gemini:

```toml
[ai]
provider = "vertex"
model = "gemini-3.1-pro-preview"
max_input_tokens = 200000

[ai.vertex]
region = "global"
```

### Build

```bash
cargo build --features vertex --release
```

### Troubleshooting

| Symptom | Cause | Fix |
|---------|-------|-----|
| "Failed to initialize Google Cloud credentials" | No ADC found | Run `gcloud auth application-default login` |
| 403 Forbidden | Model not enabled | Enable model in Vertex AI Model Garden console |
| 403 Permission denied | Missing IAM role | Grant `roles/aiplatform.user` to your principal |
| "Unsupported model family" | Model prefix not recognized | Check model name starts with `claude-` or `gemini-` |
| 404 Not Found on global endpoint | Model not offered at the global location, or not enabled for the project | Enable the model in Model Garden, or use a regional endpoint (e.g., `us-east5`) |
| 404 with `@version` suffix in model name | Versioned model IDs not supported for all endpoints | Use the base model name without version suffix (e.g., `claude-sonnet-4-6` not `claude-sonnet-4-6@20250514`) |
| "quota_exceeded" or "API not enabled" after ADC warning | ADC account lacks `serviceusage.services.use` on project | Run `gcloud auth application-default set-quota-project PROJECT_ID` or grant the permission |

## Developer Guide: Adding a New Model Family

To add a new model publisher (e.g., Llama via Meta):

### Step 1: Add ModelFamily variant

In `src/ai/vertex.rs`:
```rust
enum ModelFamily {
    Claude,
    Gemini,
    Llama,  // New
}
```

### Step 2: Update detection

```rust
fn detect_model_family(model: &str) -> Result<ModelFamily> {
    if model.starts_with("claude") { Ok(ModelFamily::Claude) }
    else if model.starts_with("gemini") { Ok(ModelFamily::Gemini) }
    else if model.starts_with("llama") { Ok(ModelFamily::Llama) }  // New
    else { bail!("Unsupported model family: {}", model) }
}
```

### Step 3: Define endpoint info

```rust
fn endpoint_info(family: ModelFamily) -> EndpointInfo {
    match family {
        ModelFamily::Claude => EndpointInfo {
            publisher: "anthropic",
            method: "rawPredict",
        },
        ModelFamily::Llama => EndpointInfo {  // New
            publisher: "meta",
            method: "rawPredict",
        },
    }
}
```

### Step 4: Implement or reuse translation

If the model uses a wire format already supported by an existing provider
module, make that module's translation and response handling `pub(crate)`
and reuse them, as the Claude and Gemini families do. Otherwise, implement
new translation functions.

### Step 5: Add dispatch branch

```rust
async fn generate_content(&self, request: AiRequest) -> Result<AiResponse> {
    match self.model_family {
        ModelFamily::Claude => self.generate_claude(request).await,
        ModelFamily::Gemini => self.generate_gemini(request).await,
        ModelFamily::Llama => self.generate_llama(request).await,  // New
    }
}
```

### Step 6: Add tests

- Model family detection test
- Endpoint URL construction test
- Request serialization test (if new wire format)

### Step 7: Update documentation

- Update this design doc with the new family entry
- Add setup instructions to README.md
- Add commented example to Settings.toml

## Testing Strategy

### Unit Tests (`src/ai/vertex.rs`)

- Model family detection (Claude, Gemini, unsupported)
- Endpoint URL construction (global, regional, multi-region us/eu)
- VertexClaudeRequest serialization (no model field, has anthropic_version)
- Request conversion from translate_ai_request output
- Context window detection (1M vs 200K models)

Gemini translation and failure classification are covered by the `ai::gemini`
tests; the Vertex path adds no logic of its own there.

### Integration Testing

Requires GCP credentials. Manual testing steps:

1. Set `provider = "vertex"` and a `model` of each family in Settings.toml
   (`claude-sonnet-4-6`, then `gemini-3.1-pro-preview`)
2. Export the project and region variables
3. Run `cargo run --features vertex` and submit a patch for review
4. Verify response in logs (token counts, no errors)

## Verified Behavior

End-to-end integration test performed on 2026-05-07:

- **Region**: `us-east5` (regional endpoint; `global` returned 404 at the time)
- **Model**: `claude-sonnet-4-6` (no version suffix)
- **Auth**: `google-cloud-auth` ADC via `gcloud auth application-default login`
- **Result**: Full multi-stage review completed successfully

Key observations:
- Global endpoint returned 404 because the request went to
  `global-aiplatform.googleapis.com`, which serves no Vertex AI API; regional
  endpoint (`us-east5-aiplatform.googleapis.com`) worked immediately
- Model name with `@version` suffix (e.g., `claude-sonnet-4-6@20250514`) also
  returned 404; bare model name worked
- ADC quota project warning did not block API access
- Prompt caching active and growing across successive calls within a review
