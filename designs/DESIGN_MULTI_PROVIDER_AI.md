# DESIGN: Multi-Provider AI Architecture

## Context
Sashiko is currently tightly coupled to the Google Gemini API. To ensure long-term flexibility and allow users to use their preferred models (OpenAI, Anthropic, Local LLMs), we need to abstract the AI interaction layer.

## Proposed Changes

### 1. The `AiProvider` Trait
We will introduce a central trait that defines the required behavior for any AI provider:

```rust
#[async_trait]
pub trait AiProvider: Send + Sync {
    /// Generate a response based on a prompt and optional tools.
    async fn generate_content(&self, request: AiRequest) -> Result<AiResponse>;
    
    /// Estimate token usage for a given request.
    fn estimate_tokens(&self, request: &AiRequest) -> usize;
    
    /// Get provider-specific constraints (e.g., max tokens, context window).
    fn get_capabilities(&self) -> ProviderCapabilities;
}
```

### 2. Unified Data Structures
We will move away from `GeminiRequest` and `GeminiResponse` in the core logic, replacing them with generic Sashiko types:
- `AiMessage`: Represents a turn in the conversation (User, Model, or Tool).
- `AiTool`: A provider-agnostic definition of a function call.
- `AiResponse`: Contains the generated text and/or tool calls.

### 3. Provider Implementation Registry & Configuration
The `Settings.toml` will be updated to allow selecting a provider and model, but **secrets will not be stored in the file**.

```toml
[ai]
provider = "openai" # or "gemini", "anthropic", "ollama"
model = "gpt-4o"
```

#### Secret Management (Environment Variables)
To maintain security and follow current Sashiko patterns, API keys will be sourced exclusively from environment variables. The `ProviderFactory` will look for the following variables based on the active provider:

- `LLM_API_KEY`: A generic variable (current behavior, can be used for the active provider).
- `OPENAI_API_KEY`: Specific to OpenAI.
- `ANTHROPIC_API_KEY`: Specific to Anthropic.
- `GEMINI_API_KEY`: Specific to Gemini.

The user can either set up `LLM_API_KEY` or their service key, which ever they prefer and it should work.

### 4. Impact on Worker Logic
The `Reviewer` and `Worker` (in `src/worker/`) will no longer care which model is being used. They will simply:
1. Build a `Vec<AiMessage>`.
2. Call `provider.generate_content()`.
3. Handle the resulting `AiResponse` or `ToolCall`.

## Alternatives Considered

- **OpenAI-API Standardization:** Forcing all providers to use the OpenAI-compatible format. This is simpler but loses access to unique features of other models (like Gemini's native PDF support or Anthropic's specific caching headers).
- **External Proxy:** Using an external tool like LiteLLM. While efficient, it adds a mandatory external dependency for users who just want to use a single API key.

## Refactor Plan

1. **Phase 1: Abstraction.** Create `src/ai/mod.rs` with the trait and generic types.
2. **Phase 2: Gemini Adapter.** Refactor the existing Gemini code to implement the new `AiProvider` trait.
3. **Phase 3: Reviewer Update.** Update `src/reviewer.rs` to use the trait instead of `GeminiClient`.
4. **Phase 4: New Providers.** Implement OpenAI and Anthropic adapters.
