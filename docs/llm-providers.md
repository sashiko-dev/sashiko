# LLM Provider Configuration

Sashiko supports multiple LLM providers. This guide covers setup for each
one. Gemini is the default and simplest to configure; the others are
alternatives you can swap in depending on your infrastructure and
preferences.

For all providers, configuration lives in `Settings.toml` at the project
root. Per-provider example files are available in
[docs/examples/](examples/). Each one is a complete configuration for
`sashiko review`, so copying it over `Settings.toml` works as shown
below. A daemon needs more than that (`[database]`, `[git]`,
`[review] worktree_dir`, ...): start from the repository's
`Settings.toml` and replace its `[ai]` tables with the example's.
Every key is described in the
[Configuration Reference](configuration.md).

## Gemini (default)

The quickest way to get started.

```bash
cp docs/examples/Settings.example.toml Settings.toml
export LLM_API_KEY="your-gemini-api-key"
```

The example file sets `provider = "gemini"` and
`model = "gemini-4-argon"`. Adjust the model name as needed.

You can also set any config value via environment variables using the
`SASHIKO` prefix with `__` (double underscore) as the separator:

```bash
export SASHIKO__AI__PROVIDER=gemini
export SASHIKO__AI__MODEL=gemini-4-argon
```

## Claude (API)

Uses Anthropic's Claude API directly.

**Get an API key:** https://console.anthropic.com/

**Set credentials:**

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
# Or use the generic fallback:
export LLM_API_KEY="sk-ant-..."
```

**Apply the example config:**

```bash
cp docs/examples/Settings.claude.toml Settings.toml
```

**What you get:**

- Automatic prompt caching (5-minute TTL) to reduce costs on repeated context
- Full tool/function calling support for git operations
- Automatic retry on rate limits and API overload
- 200K context window (use `max_input_tokens = 40000` for cost-conscious
  defaults)
- Adaptive thinking and effort via the `thinking` and `effort` settings
  in `[ai.claude]`

## Claude Code CLI

Uses a local [Claude Code](https://claude.com/claude-code) installation as
the completion backend. This uses your Claude Code subscription -- no
per-token charge, no API key.

**Prerequisites:** Install Claude Code and sign in. Verify with
`claude --version`.

**Apply the example config:**

```bash
cp docs/examples/Settings.claude-cli.toml Settings.toml
```

**Model selection:**

`model` accepts any identifier the CLI supports via `--model` -- aliases
like `opus` or `sonnet`, or full names like `claude-opus-4-7`,
`claude-sonnet-4-6`.

Context window sizes:

- `claude-opus-4-7`: 1M tokens by default.
- `claude-sonnet-4-6` and `claude-opus-4-6`: 1M capable, but the CLI
  defaults to 200K. Append `[1m]` (e.g. `claude-sonnet-4-6[1m]`) to
  opt into the 1M variant.
- `claude-haiku-4-5`: 200K only, no 1M variant.
- Pre-thinking models (Claude 3.x) are rejected with HTTP 404.

**What you get:**

- No API key needed -- uses Claude Code's subscription auth.
- Stateless: spawns `claude --print --output-format json` per request.
  Built-in tools and user/project customizations (MCP servers, skills, hooks,
  instruction files and automatic memory) are disabled explicitly. Each call
  runs in a fresh temporary working directory with no session reuse.
  This invocation was validated with Claude Code 2.1.170.
- Earlier claude-cli response-cache entries are not reused with this restricted
  completion profile; responses are regenerated on the next request.
- User/project settings are not loaded. CLI subscription credentials and
  inherited environment variables still supply authentication and provider
  routing. If authentication or routing depended on settings-based environment
  variables or an API key helper, supply the equivalent credentials and routing
  through the invoking environment instead.
- Managed policy remains authoritative: mandatory hooks and instructions may
  still apply, and managed MCP configurations can cause strict MCP mode to
  reject startup.
- Prompt caching handled automatically by Claude Code.
- `effort` controls the model's thinking budget. Opus 4.7 uses adaptive
  thinking; Sonnet 4.6 and Haiku 4.5 use extended thinking. If you need
  to pick the mode explicitly or disable thinking, use `provider = "claude"`
  (API) instead.
- `[ai.claude]` settings (`prompt_caching`, `thinking`, etc.) apply only
  to the API provider above -- they are ignored on this path.

**Note:** Each review may spawn many CLI processes. Lower
`review.concurrency` if you hit subscription rate limits.

## GitHub Copilot CLI

Uses a local
[GitHub Copilot CLI](https://docs.github.com/en/copilot/github-copilot-in-the-cli)
installation as the completion backend. This uses your GitHub Copilot
subscription -- no per-token charge, no API key.

**Prerequisites:**

- `copilot` CLI installed and on `$PATH`
- Authenticated session (run `copilot` once interactively to log in)

**Apply the example config:**

```bash
cp docs/examples/Settings.copilot-cli.toml Settings.toml
```

**What you get:**

- `model` follows GitHub Copilot's catalog (e.g. `claude-sonnet-4.5`,
  `gpt-5.5`); pick a model your subscription has access to.
- Sashiko invokes `copilot` with `--disable-builtin-mcps`,
  `--no-custom-instructions`, and `--allow-all-tools` so it acts as a
  pure text-completion-with-tools backend.
- The prompt is sent via stdin (not `-p`) to avoid Linux's
  `MAX_ARG_STRLEN` cap (~128 KB per argv element).
- `max_interactions` controls tool-call rounds before aborting.

**Note:** Each review may spawn many `copilot` processes. Lower
`review.concurrency` if you hit subscription rate limits.

## AWS Bedrock

Uses AWS Bedrock via the Converse API, or the OpenAI Responses API on the
`bedrock-mantle` endpoint for OpenAI models (see below). Works with any
Bedrock-hosted model (Claude, GPT, Llama, Mistral, etc.). Requires building
with `--features bedrock`.

**Prerequisites:** Enable model access in the
[AWS Bedrock console](https://console.aws.amazon.com/bedrock/) for your
desired model and region.

**Set AWS credentials** using any standard method:

```bash
# Option 1: Environment variables
export AWS_ACCESS_KEY_ID="..."
export AWS_SECRET_ACCESS_KEY="..."
export AWS_REGION="us-east-1"

# Option 2: AWS CLI profile (~/.aws/credentials)
aws configure
```

**Apply the example config:**

```bash
cp docs/examples/Settings.claude-bedrock.toml Settings.toml
```

**What you get:**

- Converse API -- works with any Bedrock-hosted model
- No API key needed -- uses standard AWS IAM authentication
- Cross-region inference profiles (e.g. `us.anthropic.claude-*`)
- Full tool/function calling support

**Non-Claude models:** `prompt_caching` and `thinking` under
`[ai.bedrock]` are Anthropic-specific on the Converse API; set
`prompt_caching = false` and leave `thinking` unset. Application inference
profile ARNs carry no model name and always get the Anthropic fields.

### OpenAI models (bedrock-mantle)

OpenAI foundation model IDs (starting with `openai.`, such as
`openai.gpt-5.6-sol`) are sent to the OpenAI Responses API on the
`bedrock-mantle` endpoint (`https://bedrock-mantle.<region>.api.aws/v1`)
instead of the Converse API. Mantle does not accept inference profile IDs
(`global.openai.*`, `us.openai.*`) or ARNs, so those stay on the Converse
API; the model ID is always sent as configured.

```bash
cp docs/examples/Settings.gpt-bedrock.toml Settings.toml
```

- **Authentication:** Mantle takes Amazon Bedrock API keys. Sashiko derives a
  short-term key (valid up to 12 hours, and no longer than the AWS
  credentials it is signed with) from the standard AWS credential chain and
  renews it before it expires, so no extra secret is needed. Set
  `AWS_BEARER_TOKEN_BEDROCK` to use a long-term key instead. IAM needs
  `bedrock-mantle:CreateInference`.
- **Region:** `[ai.bedrock] region`, else the AWS default chain
  (`AWS_REGION`, profile). Mantle is available in a subset of Bedrock
  regions.
- **Endpoint:** some models are served under `/openai/v1` rather than `/v1`
  (the request then fails with ``model `<id>` isn't supported on this
  route``). Set `mantle_base_url = "https://bedrock-mantle.<region>.api.aws/openai/v1"`
  for those, as listed on the model card's Programmatic Access table.
- **Reasoning:** `effort` is sent as `reasoning.effort`. `temperature` is
  never sent, since OpenAI reasoning models reject it, and `thinking` is
  ignored. Requests are stateless (`store: false`); encrypted reasoning is
  returned with each response and replayed on the next tool-call turn.
- **Output limit:** `max_tokens` becomes `max_output_tokens`, which includes
  hidden reasoning tokens. Raise it well above the default `8192` for high
  effort levels, or answers come back truncated.
- **Prompt caching:** with `prompt_caching = true` (the default) requests
  carry `prompt_cache_options` in `implicit` mode, plus explicit breakpoints
  on the system prompt and the last user message. Implicit mode lets Bedrock
  add an automatic breakpoint on the latest message, which covers the
  growing tool-call history; Bedrock allows four cache writes per request
  and Sashiko places at most two. `prompt_cache_ttl` (e.g. `"30m"`) sets the
  TTL. Cache writes are billed above the input rate and reads below it;
  `RUST_LOG=sashiko::ai::bedrock_mantle=info` logs `cache_read` and
  `cache_write` per response. Set `prompt_caching = false` for a model that
  rejects `prompt_cache_options`.

Set `openai_api = "converse"` under `[ai.bedrock]` to keep OpenAI
foundation models on the Converse API, where inference profile IDs always
run. There, `effort` is sent as `reasoning.effort` in
`additionalModelRequestFields`, and a model that rejects `temperature` with
an explicit unsupported-temperature `ValidationException` is retried
without it, which is then omitted on later requests to the same model.

## Google Cloud Vertex AI

Uses Claude and Gemini models via Google Cloud infrastructure. Requires
building with `--features vertex`.

**Prerequisites:** Enable the Vertex AI API and model access in the
[Vertex AI Model Garden](https://cloud.google.com/model-garden).

**Authenticate:**

```bash
gcloud auth application-default login
```

**Set project and region:**

```bash
export ANTHROPIC_VERTEX_PROJECT_ID="my-gcp-project"  # Or GOOGLE_CLOUD_PROJECT
export CLOUD_ML_REGION="us-east5"                    # Or GOOGLE_CLOUD_LOCATION
```

`[ai.vertex]` in `Settings.toml` outranks all four.

**Apply the example config:**

```bash
cp docs/examples/Settings.claude-vertex.toml Settings.toml   # Claude
cp docs/examples/Settings.gemini-vertex.toml Settings.toml   # Gemini
```

**What you get:**

- No API key needed -- uses Google Cloud Application Default Credentials
- Global, multi-region, and regional endpoint support
- 1M context window for Claude Opus 4.7/4.6 and Sonnet 4.6 on Vertex, and
  for Gemini
- Full tool/function calling support

**Model selection:**

`model` selects the wire format: a `claude-` name goes to Anthropic's
`rawPredict` endpoint, a `gemini-` name to Google's `generateContent`.
Anything else is rejected at startup.

`prompt_caching` under `[ai.vertex]` applies to the Claude path only.
`max_tokens`, `thinking`, and `effort` under `[ai.vertex]` apply to both
Claude and Gemini models (falling back to `[ai.gemini]` for Gemini models when
unset).
For Claude, `thinking` takes `"adaptive"` (or `"disabled"` where the model
allows it) and `effort` takes `"low"`, `"medium"`, `"high"`, `"xhigh"` or
`"max"`. For Gemini, both take a thinking level (`"LOW"`, `"MEDIUM"`,
`"HIGH"`), and `effort` wins when both are set.

## Kiro CLI

Uses the local `kiro-cli` as a completion backend.

**Prerequisites:** Install `kiro-cli` and authenticate with
`KIRO_API_KEY` or a browser login.

**Apply the example config:**

```bash
cp docs/examples/Settings.kiro-cli.toml Settings.toml
```

**What you get:**

- Runs `kiro-cli acp` as a stateless completion backend
- Kiro native tools are disabled by default; Sashiko's own tool protocol
  is used instead
- An isolated temporary agent with a deny-all hook prevents accidental
  tool execution

## goose

Uses [goose](https://goose-docs.ai/) as the completion backend.
goose speaks the Agent Client Protocol on stdio, and it fronts any backend
goose itself supports, which makes this the shortest path to a fully local
review: goose in front of a vLLM or Ollama server.

Like Sashiko, goose is a Linux Foundation project -- it sits under the
[Agentic AI Foundation (AAIF)](https://aaif.io/).

**Prerequisites:** Install `goose` and make sure the backend it points at is
reachable.

**Apply the example config:**

```bash
cp docs/examples/Settings.goose-cli.toml Settings.toml
```

**What you get:**

- Runs `goose acp` as a stateless completion backend
- Each request gets a throwaway `XDG_CONFIG_HOME` whose `config.yaml` pins
  goose to chat mode, so goose never runs a tool of its own and Sashiko's
  tool protocol stays the only tool layer
- The throwaway config also keeps the user's own goose configuration and
  extensions out of a review
- `XDG_DATA_HOME` and `XDG_STATE_HOME` are redirected alongside it, so the
  session database and the logs goose writes per request are discarded with
  the request instead of accumulating in the user's home directory, and
  concurrent reviews never share one session database
- Token usage is taken from goose's own `session/prompt` accounting rather
  than estimated

**Pointing goose at a local model:**

```toml
[ai]
provider = "goose"
model = "qwen3-8b-ov"

[ai.goose_cli]
goose_provider = "openai"
context_window_size = 32768

[ai.goose_cli.env]
OPENAI_HOST = "http://localhost:8000"
OPENAI_BASE_PATH = "v1/chat/completions"
OPENAI_API_KEY = "dummy"
```

`goose_provider` accepts any goose provider id (`openai`, `ollama`,
`anthropic`, `google`, ...). The `[ai.goose_cli.env]` table is passed to the
goose child process; goose inherits Sashiko's environment, so exported
variables work too and the table overrides them. Keep real API keys in
the environment rather than in the settings file.

The table cannot override the variables that make goose a completion
backend. `GOOSE_MODE`, the three XDG directories and the model and provider
taken from `[ai]` are pinned after it, so a stray entry cannot hand goose
back its own tools, its own session history or the user's own
configuration.

goose prepends its own system prompt and platform tool schemas to every
request, which costs roughly 5k tokens before Sashiko's prompt is even
counted. Set `max_input_tokens` well below `context_window_size` so a review
prompt plus that overhead still fits.

## Codex CLI

Uses a local [Codex CLI](https://github.com/openai/codex) (OpenAI)
installation as the completion backend. This uses your Codex
subscription -- no per-token charge, no API key.

**Prerequisites:** Install the `codex` CLI and authenticate.

**Apply the example config:**

```bash
cp docs/examples/Settings.codex-cli.toml Settings.toml
```

**What you get:**

- Runs `codex exec --json --sandbox read-only` as a stateless backend
- Prompt sent via stdin to avoid `ARG_MAX` issues
- No tool access -- sandbox is read-only

**Reasoning effort:**

```toml
[ai.codex_cli]
effort = "xhigh"    # "none", "minimal", "low", "medium", "high", "xhigh", "max"
```

Sashiko passes this as `codex exec -c model_reasoning_effort=<effort>`,
so the setting applies only to a reasoning model such as `gpt-5-codex`.
A `-c` override outranks `~/.codex/config.toml`. It does not outrank an
enterprise-managed requirements layer, which substitutes its own value
whatever the origin. A run whose effort that layer substitutes fails
rather than record a review at an effort other than the one configured.
Leave the setting unset to accept the account default.

## Devin CLI

Sashiko can use a local [Devin for Terminal](https://cli.devin.ai/) install as
a completion backend. This path uses your Devin subscription, so there is no
per-token API charge and no API key to configure.

**Prerequisites:**

- `devin` CLI installed and on `$PATH`
- Authenticated session (run `devin auth login` once)

**Apply the example config:**

```bash
cp docs/examples/Settings.devin-cli.toml Settings.toml
```

**Notes:**

- `model` accepts any identifier `devin --model` accepts (e.g. `opus`,
  `swe`, `gpt`, `codex`). Leave it empty in `[ai]` to use the Devin default.
- Sashiko invokes `devin --print --prompt-file <tmp>`
  per request. The prompt is written to a temp file which is passed to `devin`.
- Each review may spawn many `devin` processes. Lower `review.concurrency`
  if you hit subscription rate limits.

## Ollama

[Ollama](https://ollama.com/) allows running LLMs locally.

**Prerequisites:** Install Ollama and pull your desired model (e.g., `ollama pull deepseek-v3`).

**Apply the example config:**

```bash
cp docs/examples/Settings.ollama.toml Settings.toml
```

**What you get:**

- Private, local execution of LLMs
- No API key or subscription required
- `context_window_size` maps to Ollama's `num_ctx`

## vLLM

[vLLM](https://docs.vllm.ai/) serves local models behind an
OpenAI-compatible API.

**Prerequisites:** Start a vLLM server, e.g.:

```bash
vllm serve Qwen/Qwen3-8B --max-model-len 32768 --host 0.0.0.0 --port 8000
```

**Apply the example config:**

```bash
cp docs/examples/Settings.vllm.toml Settings.toml
```

**What you get:**

- Private, local execution of LLMs
- Reasoning model support: `<think>` blocks and `reasoning_content` are
  separated from the answer automatically, and thinking can be toggled via
  the `enable_thinking` setting
- Leaving `max_tokens` unset lets vLLM generate up to the remaining context,
  which is useful for servers running with a small `--max-model-len`
- Optional `guided_json` setting to enforce JSON responses through guided
  decoding on backends that support it
- Optional `enable_tools` setting to forward tool definitions; requires a
  server started with `--enable-auto-tool-choice` and `--tool-call-parser`
- If the server was started with `--api-key`, export it as `VLLM_API_KEY`
  (or `LLM_API_KEY`)

Set `context_window_size` to match the server's `--max-model-len`.

## OpenAI-Compatible Providers

Sashiko includes an OpenAI-compatible provider for endpoints that
implement the OpenAI chat completions API.

**Apply the example config:**

```bash
cp docs/examples/Settings.openai-compat.toml Settings.toml
```

Adjust `base_url` to point to your provider's endpoint.

`base_url` may be either:

- a shorthand such as `http://localhost:8080/v1` (the `/chat/completions`
  suffix is appended automatically), or
- the full chat completions URL, e.g.
  `https://api.z.ai/api/coding/paas/v4/chat/completions`. Use this form for
  providers whose path is not a recognised shorthand.

**z.ai / Zhipu (glm-*) example:**

z.ai exposes two OpenAI-compatible gateways with **separate billing**: a
direct API (`…/api/paas/v4/…`, billed to the API resource package) and a
coding-plan gateway (`…/api/coding/paas/v4/…`, billed to the coding-plan
subscription). To review against the coding-plan quota, point `base_url` at
the full coding endpoint and set `LLM_API_KEY` (or `OPENAI_API_KEY`):

```toml
[ai]
provider = "openai-compatible"
model = "glm-5.2"

[ai.openai_compat]
base_url = "https://api.z.ai/api/coding/paas/v4/chat/completions"
context_window_size = 128000
max_tokens = 16384
```

**OrcaRouter example:**

[OrcaRouter](https://www.orcarouter.ai) is an OpenAI-compatible gateway to
models from OpenAI, Anthropic, Google, and other providers behind a single
endpoint and API key. Point `base_url` at `https://api.orcarouter.ai/v1` and
use a namespaced model id such as `openai/gpt-4o-mini`:

```toml
[ai]
provider = "openai-compatible"
model = "openai/gpt-4o-mini"

[ai.openai_compat]
base_url = "https://api.orcarouter.ai/v1"
context_window_size = 128000
max_tokens = 16384
```

For OpenAI's own API with an API key (rather than a self-hosted
compatible endpoint), use `docs/examples/Settings.openai-api.toml`:
set `provider = "openai"`, `model = "gpt-5.6-sol"`, and export
`OPENAI_API_KEY`.

Some compatible gateways reject the `temperature` field for reasoning
models. Sashiko retries an explicit unsupported-temperature error without
the field and omits it on later requests to the same endpoint.

`effort` in `[ai.openai_compat]` sets reasoning effort on a reasoning
model, sent as the `reasoning_effort` request field. It is omitted
when unset, so a compatible endpoint that does not implement the
field is unaffected until you set it; one that does not recognize it
may reject the request outright rather than ignore it.

The valid levels belong to the model, not to the API. `"none"` is
legal, and is what a gpt-5.6 chat completion carrying function tools
requires; gpt-5.4-pro rejects `"low"` and takes only `"medium"` and
`"high"`. The value is passed through as configured, so a level the
model does not accept fails every request with a 400.

Reasoning models that return their chain of thought as
`reasoning_content` (DeepSeek, Moonshot/Kimi, z.ai) or `reasoning`
(OpenRouter, newer vLLM) have it kept in the conversation. Set
`send_reasoning_content = true` in `[ai.openai_compat]` to send it back
as `reasoning_content` on each later assistant turn, which thinking
models such as DeepSeek and Kimi require across tool calls. It is off by
default because an endpoint that does not know the field may reject the
request. OpenRouter's structured `reasoning_details` are not read, so
encrypted reasoning from OpenAI and Gemini models routed through it is
lost between turns.

Assistant messages that only carry tool calls are sent without a
`content` field, as the OpenAI API allows. Some compatible endpoints
reject them, and some chat templates (GLM models served by vLLM, for
example) render the missing field as a literal `None` in the prompt. Set
`send_empty_tool_call_content = true` under `[ai.openai_compat]` to send
an empty string instead.

**Gateways that require extra request fields:**

Enterprise gateways in front of an OpenAI-compatible endpoint often
authenticate clients with a key sent in a header next to the bearer
token, such as `x-api-key`. AI providers behind AWS are a common example.
That key differs from the endpoint's `LLM_API_KEY`, so it needs a variable
with a different name. Header values are credentials or auth keys, so they
are read from the environment rather than from settings.
Name each header and the variable under `[ai.openai_compat.extra_headers]`.
The headers are sent with every request and an unset variable
fails at startup.

```toml
[ai.openai_compat]
base_url = "https://llm-gateway.example.com/v1"

[ai.openai_compat.extra_headers]
"x-api-key" = "LLM_CLIENT_API_KEY"
```

```bash
export LLM_CLIENT_API_KEY=...
```

Some gateways also expect client metadata in the request body. Add it
under `[ai.openai_compat.extra_body]`; its fields are merged into the top
level of every request. Fields Sashiko sets itself, such as `model` or
`messages`, are rejected at startup:

```toml
[ai.openai_compat.extra_body]
extraData = { clientName = "sashiko", sessionId = "review" }
```

Unlike header values, body fields are read from the settings file, so
do not put credentials in them.

## Mistral AI

Sashiko has a native Mistral provider (`provider = "mistral"`) for
`https://api.mistral.ai` and Mistral-shaped gateways. The Mistral chat
completions API is close to the OpenAI one but not close enough for the
generic compatible client: replies carry the assistant text as an array of
typed content blocks, where the chain of thought arrives as `thinking`
blocks next to `text` blocks instead of a `reasoning_content` string. The
provider speaks that shape natively, so a reasoning model's thinking is
kept in the conversation across tool calls without extra settings.

**Apply the example config:**

```bash
cp docs/examples/Settings.mistral.toml Settings.toml
```

Export the API key as `MISTRAL_API_KEY` (or `LLM_API_KEY`):

```bash
export MISTRAL_API_KEY=...
```

Third-party models hosted on Mistral, such as the `zai-glm-*` series, work
the same way; their 1M-token context is picked up from the model name, so
`max_input_tokens` in `[ai]` can be raised accordingly.

```toml
[ai]
provider = "mistral"
model = "zai-glm-5-3-flash"
max_input_tokens = 400000

[ai.mistral]
# Sent when a stage does not ask for an effort level itself. The API also
# accepts "none", "minimal", "xhigh" and "max".
# effort = "medium"
# max_tokens = 32768
```

`base_url` accepts the API host (`https://api.mistral.ai`), any `…/v1`
root (the `/chat/completions` suffix is appended, which covers gateways
behind a path prefix), or the full chat completions URL. Any other path is
rejected at startup. Tool calling, JSON mode, and `reasoning_effort` are
supported; an effort level the model rejects comes back as a 400, which
ends the run as a fatal error.
