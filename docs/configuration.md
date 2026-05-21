# Configuration Reference

Sashiko is configured through two files in the project root:

- **Settings.toml** -- application settings (AI, server, git, review)
- **email_policy.toml** -- email delivery policy

Both can be bootstrapped from the examples in [docs/examples/](examples/).
All settings can also be overridden via environment variables using the
`SASHIKO` prefix with `__` (double underscore) as the separator (e.g.
`SASHIKO__AI__PROVIDER=gemini`).

For LLM provider-specific setup (API keys, auth, provider features), see
the [LLM Provider Configuration Guide](llm-providers.md).

## Settings.toml sections

### `[database]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `url` | string | `"sashiko.db"` | Path to the SQLite database file. |
| `token` | string | `""` | Database token (unused for SQLite). |

### `[mailing_lists]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `track` | string or list | -- | Mailing lists to monitor. Accepts a TOML array or a comma-separated string. |

### `[nntp]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `server` | string | `"nntp.lore.kernel.org"` | NNTP server hostname. |
| `port` | integer | `119` | NNTP server port. |

### `[smtp]`

Optional. If omitted, no review emails are sent. Even when present,
`dry_run` defaults to `true` as a safety measure.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `server` | string | -- | SMTP server hostname. |
| `port` | integer | -- | SMTP server port. |
| `username` | string | -- | SMTP username (optional). |
| `password` | string | -- | SMTP password (optional). |
| `sender_address` | string | -- | From address for review emails. |
| `reply_to` | string | -- | Reply-To address (optional). |
| `dry_run` | bool | `true` | When true, emails are logged but not sent. |

### `[ai]`

Core AI settings that apply to all providers.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `provider` | string | -- | LLM provider: `gemini`, `claude`, `claude-cli`, `codex-cli`, `copilot-cli`, `bedrock`, `vertex`, `kiro-cli`, `openai-compat`. |
| `model` | string | -- | Model identifier (provider-specific). |
| `max_input_tokens` | integer | `150000` | Maximum input tokens per request. |
| `max_interactions` | integer | `100` | Maximum tool-call rounds per review turn. |
| `temperature` | float | `1.0` | Sampling temperature. |
| `api_timeout_secs` | integer | `300` | Timeout for individual API calls (seconds). |
| `log_turns` | bool | `false` | Log each AI request/response turn at info level. Verbose but useful for debugging. |
| `response_cache` | bool | `false` | Cache AI responses to disk. |
| `response_cache_ttl_days` | integer | `7` | TTL for cached responses (days). |

#### `[ai.claude]`

Settings specific to the Claude API provider (`provider = "claude"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `prompt_caching` | bool | `true` | Enable Anthropic prompt caching (5-minute TTL). |
| `max_tokens` | integer | `4096` | Max output tokens per response. |
| `base_url` | string | -- | Override the API base URL (optional, for proxies like Portkey). |
| `thinking` | string | -- | Extended thinking mode: `"enabled"` or `"adaptive"` (Sonnet 4.6+). |
| `effort` | string | -- | Thinking effort: `"low"`, `"medium"`, `"high"`. |

#### `[ai.claude_cli]`

Settings for the Claude Code CLI provider (`provider = "claude-cli"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `effort` | string | -- | Thinking effort: `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`. |

#### `[ai.gemini]`

Settings for the Gemini provider (`provider = "gemini"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `explicit_prompt_caching` | bool | `false` | Use explicit caching hints in requests. |

#### `[ai.openai_compat]`

Settings for OpenAI-compatible providers (`provider = "openai-compat"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `base_url` | string | -- | API endpoint URL. |
| `context_window_size` | integer | -- | Context window size (optional). |
| `max_tokens` | integer | -- | Max output tokens (optional). |

#### `[ai.kiro_cli]`

Settings for the Kiro CLI provider (`provider = "kiro-cli"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `binary` | string | `"kiro-cli"` | Path to the kiro-cli binary. |
| `agent` | string | -- | Custom agent name (optional). |
| `context_window_size` | integer | `200000` | Context window size. |

### `[server]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `host` | string | `"::"` | Listen address. `"::"` binds to all interfaces (IPv4 and IPv6). |
| `port` | integer | `8080` | Listen port for the web UI and API. |
| `read_only` | bool | `false` | When true, disables write API endpoints. Set automatically by `--no-api`. |

### `[git]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `repository_path` | string | -- | Path to the kernel git repository used for patch application and context. |

#### `[[git.custom_remotes]]`

Optional array of additional git remotes to track.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `name` | string | -- | Remote name. |
| `url` | string | -- | Remote URL. |
| `check_all_branches` | bool | -- | Try all branches as baselines. |
| `only_branches` | list | -- | Restrict to specific branches (optional). |

### `[review]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `concurrency` | integer | -- | Number of concurrent reviews. |
| `worktree_dir` | string | -- | Directory for git worktrees used during review. |
| `timeout_seconds` | integer | `3600` | Maximum time per review (seconds). |
| `max_retries` | integer | `3` | Retry count on transient failures. |
| `max_lines_changed` | integer | `10000` | Skip patches with more changed lines than this. |
| `max_files_touched` | integer | `200` | Skip patches touching more files than this. |
| `ignore_files` | list | `[]` | File patterns to skip during review (e.g. `MAINTAINERS`). |
| `email_policy_path` | string | `"email_policy.toml"` | Path to the email policy file. |
| `max_total_tokens` | integer | `5000000` | Maximum cumulative uncached tokens (input + output) per review. Cached tokens are excluded. Set to 0 to disable. |
| `max_total_output_tokens` | integer | `500000` | Maximum cumulative output tokens per review. Set to 0 to disable. |

## email_policy.toml

Controls how Sashiko sends (or suppresses) review emails. See
[docs/examples/email_policy.toml](examples/email_policy.toml) for an
annotated example.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `defaults.reply_all` | bool | `false` | Allow sending to public mailing lists. |
| `defaults.reply_to_author` | bool | `false` | Send review to the patch author. |
| `defaults.cc_individuals` | bool | `false` | CC individual recipients (non-mailing-list) on review emails. |
| `defaults.mute_all` | bool | `true` | Suppress all email sending. |
| `defaults.cc` | list | `[]` | Static CC addresses. |
| `defaults.ignored_emails` | list | `[]` | Author addresses to ignore entirely. |
| `defaults.subject_prefixes` | list | `[]` | Subject prefix patterns to match for this scope. |
| `defaults.embargo_hours` | integer | -- | Hours to wait before sending a review. When a patch matches multiple subsystems, the shortest configured embargo wins. |
| `defaults.send_positive_review` | bool | `false` | Send email even when no issues are found. |

The email policy also supports per-subsystem overrides via
`[subsystems.<name>]` sections. Each subsystem section accepts the same
fields as `[defaults]`, plus:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `lists` | list | `[]` | Mailing list addresses that map to this subsystem. |
| `patchwork.enabled` | bool | `false` | Enable Patchwork integration for this subsystem. |
| `patchwork.api_url` | string | -- | Patchwork API URL. |
| `patchwork.token` | string | -- | Patchwork API token. |

## Environment variables

| Variable | Description |
|----------|-------------|
| `LLM_API_KEY` | API key for the configured LLM provider (universal fallback). |
| `GEMINI_API_KEY` | API key for Gemini (takes precedence over `LLM_API_KEY`). |
| `ANTHROPIC_API_KEY` | API key for Claude (takes precedence over `LLM_API_KEY`). |
| `OPENAI_API_KEY` | API key for OpenAI-compatible providers (takes precedence over `LLM_API_KEY`). |
| `ANTHROPIC_BASE_URL` | Override the Claude API base URL (for proxies). |
| `ANTHROPIC_VERTEX_PROJECT_ID` | GCP project ID for Vertex AI provider. |
| `CLOUD_ML_REGION` | GCP region for Vertex AI provider. |
| `SASHIKO_SERVER` | Override daemon URL for CLI commands. |
| `SASHIKO__*` | Override any Settings.toml value (e.g. `SASHIKO__AI__PROVIDER`). |
| `NO_COLOR` | Disable ANSI color output. |
| `SASHIKO_LOG_PLAIN` | Use plain log format (no level/target/timestamp). |
