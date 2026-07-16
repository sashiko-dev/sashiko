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

### `[subsystems]`

Controls how patches and emails are categorized into subsystems for targeted reviews and specific email policies. By default, this section is empty, meaning the system relies on fallback heuristics (like identifying `@vger.kernel.org` addresses) to determine subsystems.

This feature is **globally active** and applies to both mailing list (NNTP) and Forge (Webhook) ingestion.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `mapping` | list of objects | `[]` | A list of rules mapping a regular expression pattern to a subsystem name. |

Each mapping object in the list requires two fields:
* `pattern` (string): A regular expression used for matching.
* `name` (string): The resulting subsystem name if the pattern matches.

**How it works:**
- **For Git Forges (Webhooks):** The system applies the `pattern` against the **file paths** modified by a pull request (e.g., matching `^drivers/net/.*`).
- **For Mailing Lists (NNTP):** The system applies the `pattern` against the **To and Cc email addresses** of the incoming patch email.

When a patch is tagged with a subsystem, it can trigger subsystem-specific AI review rules (context loading) and specific email/embargo policies defined in `email_policy.toml`.

```toml
[subsystems]
mapping = [
    { pattern = ".*drivers/.*", name = "Drivers" },
    { pattern = ".*net/.*", name = "Networking" },
    { pattern = ".*fs/.*", name = "Filesystems" },
    { pattern = ".*mm/.*", name = "Memory Management" },
]
```

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
| `patchwork.api_url` | string | -- | Patchwork REST API URL (e.g. `https://patchwork.kernel.org/api/1.3`). Trailing slashes are stripped automatically. Invalid schemes are rejected with a warning. |
| `patchwork.token` | string | -- | Patchwork API token. Can also be set via `SASHIKO_PATCHWORK_TOKEN` env var (fills in where token is omitted in TOML). |
| `patchwork.email` | string | -- | Email address for email-based Patchwork notifications. |
| `patchwork.min_severity` | string | -- | Minimum finding severity to include in patchwork checks. Findings below this threshold are excluded. Accepts: `Low`, `Medium`, `High`, `Critical` (case-insensitive). Default: all findings included. |
| `patchwork.fail_severity` | string | `High` | Minimum severity of NEW findings that triggers the `fail` check state instead of `warning`. New findings at or above this threshold produce `fail`; below it produce `warning`. Pre-existing findings never affect the check state. |

### Patchwork integration

Sashiko can report review results as
[checks](https://patchwork.readthedocs.io/en/latest/usage/overview/#checks)
on a Patchwork instance. Two delivery modes are available and can be
enabled simultaneously for the same subsystem.

**API mode** posts checks directly to the Patchwork REST API with
retry-queuing (3 attempts, exponential backoff). Requires a maintainer
API token. Note: Patchwork tokens grant full project-maintainer
permissions (state changes, delegation, etc.), not just check access.

```toml
[subsystems.net.patchwork]
enabled = true
api_url = "https://patchwork.kernel.org/api/1.3"
token = "your-api-token"   # or set SASHIKO_PATCHWORK_TOKEN env var
```

**Email mode** sends a structured notification email to a bot address.
A local script (such as
[pw_tools](https://github.com/mchehab/pw_tools)) parses the email and
posts the check. This avoids giving Sashiko a write token.

```toml
[subsystems.linux-media.patchwork]
enabled = true
email = "pw-bot@lists.example.org"
```

#### Severity filtering and check state mapping

By default, all findings are included in the patchwork check count.
Set `min_severity` to exclude findings below a threshold. When all
findings fall below the threshold, the check is posted as `success`.

The check state depends only on **new** findings (not pre-existing):

- `fail` -- new findings at or above `fail_severity` (default: `High`)
- `warning` -- new findings below `fail_severity`
- `success` -- no new findings (pre-existing findings are still
  shown in the description but do not affect the state)

The check description shows a per-severity breakdown with
pre-existing counts in parentheses, dropping zero-count severities.
For example: `Critical: 1 · High: 2 (1 pre-existing)`.

```toml
[subsystems.net.patchwork]
enabled = true
api_url = "https://patchwork.kernel.org/api/1.3"
min_severity = "Medium"    # exclude Low findings entirely
fail_severity = "High"     # High+ new findings = fail (default)
```

Edge case behaviors:

- Missing or null `preexisting` flag on a finding is treated as new
- When `min_severity` filters out all findings, the check is `success`
  with "Sashiko AI review found no regressions"
- When only pre-existing findings remain after filtering, the check
  is `success` but the description shows the pre-existing breakdown

#### Email notification format

When email mode is enabled, Sashiko sends a plain-text email with:

- **To**: the configured `patchwork.email` address
- **Subject**: `[sashiko-check] {status} - {patch_subject}`
- **Body** (one key-value pair per line):

```
msgid: <message-id>
status: success|warning
description: Sashiko AI review found N potential issue(s)
target_url: https://sashiko.dev/#/patchset/...
context: sashiko
```

Downstream tools can parse this format with simple line splitting.

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
| `SASHIKO_PATCHWORK_TOKEN` | Patchwork API token. Fills in `patchwork.token` for enabled subsystems that have `api_url` set but no explicit token in TOML. |
| `NO_COLOR` | Disable ANSI color output. |
| `SASHIKO_LOG_PLAIN` | Use plain log format (no level/target/timestamp). |
