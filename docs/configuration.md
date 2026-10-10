# Configuration Reference

Sashiko is configured through two files:

- **Settings.toml** -- application settings (AI, server, git, review)
- **projects/\<project\>/mailing_lists.toml** -- per-mailing-list tracking, embargo, email delivery, and Patchwork policy

Both can be bootstrapped from the examples in [docs/examples/](examples/).
All settings can also be overridden via environment variables using the
`SASHIKO` prefix with `__` (double underscore) as the separator (e.g.
`SASHIKO__AI__PROVIDER=gemini`).

Every command reads one file, the first of `--settings`, `$SASHIKO_CONFIG`,
`Settings.toml` in the working directory, and `~/.config/sashiko.toml` (or
`$XDG_CONFIG_HOME/sashiko.toml`).

The daemon and `sashiko review` read the same shape, so one file serves both.
`[database]`, `[server]`, and `[git]` are optional, since a local review has
none of them; the daemon refuses to start without `[database] url`,
`[git] repository_path`, and `[review] worktree_dir`.

For LLM provider-specific setup (API keys, auth, provider features), see
the [LLM Provider Configuration Guide](llm-providers.md).

## Settings.toml sections

### Top-level keys

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `log_level` | string | `"info"` | Default log filter for the daemon (e.g. `"warn"`, `"info"`, `"debug"`). `RUST_LOG` overrides it when set, `--debug` forces `"info"`, and `sashiko review` defaults to `"warn"` regardless of this value. |

### `[project]`

Optional. Describes the project this configuration is for.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `kind` | string | -- | Project this file is for: `"linux"` or `"sashiko"`. When set, it is checked against the project selected by `--project` / `SASHIKO_PROJECT`, and a mismatch is an error. When absent, the file is accepted for any project. |
| `name` | string | `""` | Display name shown in the web UI. |
| `description` | string | `""` | Short description shown in the web UI. |
| `domain` | string | `""` | Public hostname used to build links to the web UI in forge comments (`https://<domain>/#/patchset/...`). Falls back to `sashiko.sashiko.dev` when empty. |
| `attribution` | string | -- | Actor name recorded on bug discoveries. Defaults to `domain`, or `"sashiko"` when neither is set. |

### `[forge]`

Optional. Controls forge (GitHub/GitLab) webhook integration.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable forge webhook endpoint. |
| `disable_nntp` | bool | `true` | Disable NNTP ingestion when forge is enabled. |
| `provider` | string | -- | Forge provider: `"github"` or `"gitlab"`. |
| `webhook_secret` | string | -- | Webhook signing token or shared secret for authenticating incoming requests. When configured, requests are authenticated via signature verification, which is the only way a forge can authenticate. See the [Webhook Security Guide](WEBHOOK_SECURITY.md). |
| `api_token` | string | -- | Forge API token (for future API-based features). |

> **Security:** When `Settings.toml` contains secrets, restrict file
> permissions: `chmod 600 Settings.toml`.

### `[database]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `url` | string | `"sashiko.db"` | Path to the SQLite database file. |
| `token` | string | `""` | Database token (unused for SQLite). |

### `[mailing_lists]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `track` | string or list | `[]` | Optional override for mailing lists to monitor via NNTP. When empty (default), tracked lists are loaded from `projects/<project>/mailing_lists.toml` (all entries with `track = true`). Accepts a TOML array or a comma-separated string. |

### `[nntp]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `server` | string | `"nntp.lore.kernel.org"` | NNTP server hostname. |
| `port` | integer | `119` | NNTP server port. |
| `tls` | bool | `false` | Wrap the session in TLS (implicit NNTPS). |

Setting `tls` does not change `port`; set it to `563` as well when
enabling NNTPS. The certificate is verified against the host trust
store, so an internal CA must be installed there. `lore.kernel.org`
offers no TLS-protected NNTP, so this is for internal mirrors.

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
| `provider` | string | -- | LLM provider: `gemini`, `claude`, `claude-cli`, `codex-cli`, `copilot-cli`, `bedrock`, `vertex`, `kiro-cli`, `goose`, `openai`, `openai-compatible`. |
| `model` | string | -- | Model identifier (provider-specific). |
| `max_input_tokens` | integer | `150000` | Maximum input tokens per request. |
| `max_interactions` | integer | `100` | Maximum tool-call rounds per review turn. |
| `temperature` | float | `1.0` | Sampling temperature. |
| `api_timeout_secs` | integer | `300` | Timeout for individual API calls (seconds). |
| `log_turns` | bool | `false` | Log each AI request/response turn at info level. Verbose but useful for debugging. |
| `response_cache` | bool | `false` | Cache AI responses to disk. The daemon keeps the cache beside its database; a local review, which has none, keeps it under `$XDG_DATA_HOME/sashiko/`. Entries are keyed on the provider's own settings as well as the request, so changing `model`, an endpoint, a reasoning level, or an output cap misses the entries recorded under the old value rather than replaying them. |
| `response_cache_ttl_days` | integer | `7` | TTL for cached responses (days). Entries stranded by a settings change age out on this schedule. |

#### `[ai.claude]`

Settings specific to the Claude API provider (`provider = "claude"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `prompt_caching` | bool | `true` | Enable Anthropic prompt caching (5-minute TTL). |
| `max_tokens` | integer | `32768` | Max output tokens per response, thinking included. |
| `base_url` | string | -- | Override the API base URL (optional, for proxies like Portkey). |
| `thinking` | string | -- | Thinking mode, sent as `thinking.type`: `"adaptive"` (Opus 4.6, Sonnet 4.6, and later), or `"disabled"` where the model allows it. |
| `effort` | string | -- | Effort level, sent as `output_config.effort`: `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`. Which levels a model accepts, and its default, vary by model. |

#### `[ai.claude_cli]`

Settings for the Claude Code CLI provider (`provider = "claude-cli"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `effort` | string | -- | Thinking effort: `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`. |

#### `[ai.codex_cli]`

Settings for the Codex CLI provider (`provider = "codex-cli"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `effort` | string | -- | Reasoning effort: `"none"`, `"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`. Passed as `-c model_reasoning_effort=<effort>`, which outranks `~/.codex/config.toml` but not an enterprise-managed requirements layer. A run whose effort that layer substitutes fails. |

#### `[ai.gemini]`

Settings for the Gemini provider (`provider = "gemini"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `explicit_prompt_caching` | bool | `false` | Use explicit caching hints in requests. |

#### `[ai.openai]`

Settings for `provider = "openai"` and `provider = "openai-compatible"`.
Unknown keys and unsupported values are rejected at startup.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `api` | string | `"chat"` | API protocol: `"chat"` for Chat Completions or `"responses"` for the Responses API. Responses requires explicit opt-in for both providers. |
| `reasoning_effort` | string | omitted | Optional reasoning effort: `"low"`, `"medium"`, `"high"`, `"xhigh"`. Omitted by default to preserve the model's own default. Choose a value supported by the model and endpoint. |

With Responses, `ai.openai_compat.base_url` must be a full `/responses`
endpoint or a base URL ending in `/v1`, `/api/v1`, or no path. An explicitly
selected API rejects a URL for the other API with a configuration error;
existing `/chat/completions` URLs keep working with the default Chat API.
Responses conversation history is sent on each request rather than stored
through a previous response ID.

#### `[ai.openai_compat]`

Settings for the OpenAI providers (`provider = "openai"` or `provider = "openai-compatible"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `base_url` | string | model-derived | API endpoint URL. For Chat, derived from the model name, `https://api.openai.com/v1/chat/completions` for anything unrecognized. With `ai.openai.api = "responses"`, defaults to `https://api.openai.com/v1/responses`. |
| `context_window_size` | integer | model-derived | Context window size. `128000` for most models. |
| `max_tokens` | integer | `4096` | Max output tokens per response. Sent as `max_output_tokens` with Responses, `max_completion_tokens` with the OpenAI Chat provider, or `max_tokens` with compatible Chat providers. The OpenAI caps include reasoning tokens as well as the reply. |

#### `[ai.kiro_cli]`

Settings for the Kiro CLI provider (`provider = "kiro-cli"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `binary` | string | `"kiro-cli"` | Path to the kiro-cli binary. |
| `agent` | string | -- | Custom agent name (optional). |
| `context_window_size` | integer | `200000` | Context window size. |

#### `[ai.goose_cli]`

Settings for the goose provider (`provider = "goose"`).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `binary` | string | `"goose"` | Path to the goose binary. |
| `goose_provider` | string | `"openai"` | Backend goose itself talks to, passed as GOOSE_PROVIDER. |
| `env` | table | `{}` | Environment for the goose child process, e.g. `OPENAI_HOST`. goose inherits Sashiko's environment and these entries win over it, but they cannot override the variables Sashiko pins to keep goose a completion backend (`GOOSE_MODE`, `GOOSE_MODEL`, `GOOSE_PROVIDER`, `GOOSE_CONTEXT_LIMIT`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`). |
| `context_window_size` | integer | `128000` | Context window size. goose adds roughly 5k tokens of its own prompt, so keep `max_input_tokens` well below this. |

### `[server]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `host` | string | `"::"` | Listen address. `"::"` binds to all interfaces (IPv4 and IPv6). |
| `port` | integer | `8080` | Listen port for the web UI and API. |
| `read_only` | bool | `false` | When true, disables write API endpoints. Set automatically by `--no-api`. |
| `public_base_url` | string | -- | The URL the service is reachable at from outside, with no trailing slash. Required whenever `[smtp]` is configured: sign-in links are mailed, and the bind address names no host a recipient can open. The server refuses to start without it. |
| `jwt_secret` | string | -- | Signs sign-in links and session tokens. Without it, sign-in returns `501` and no identity can be established. Keep it stable: replacing it invalidates every session and every unopened link. Prefer `SASHIKO__SERVER__JWT_SECRET` over writing it to disk. |
| `log_sign_in_links` | bool | `false` | Writes sign-in links to the log. For a developer machine with no real users; a link in a log is a credential anyone reading the log can spend. |

### `[server.acl]`

Capability lists, matched against the address a caller signed in with. Every
list is empty by default, which grants nothing (fail-closed). The local
operator token covers `ingest`, `cancel` and `review` for tooling that can read
the server's token file, so these lists are only about remote, identified
callers.

| Key | Type | Description |
|-----|------|-------------|
| `admins` | list | All capabilities, including root-level maintenance operations. |
| `security` | list | Reads and comments on every bug and reads raw AI transcripts. Grants no ingest, cancel or review. |
| `bug_reporters` | list | May file new bugs over HTTP. Empty means only admins can. |
| `ingest` | list | May submit patches (`/api/submit`). |
| `cancel` | list | May halt running workloads. |
| `review` | list | May trigger AI analyses of existing patches. |
| `blocklist` | list | Denies everything, overriding every grant above. |

Each list accepts either a TOML array or a comma separated string, so a
deployment can name its operators through the environment instead of baking
them into the image:

```bash
SASHIKO__SERVER__ACL__ADMINS="first@example.org,second@example.org"
```

### `[git]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `repository_path` | string | -- | Path to the kernel git repository used for patch application and context. |

#### `[[git.custom_remotes]]`

Optional array of additional git remotes to track.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `name` | string | -- | Remote name. Required. |
| `url` | string | -- | Remote URL. Required. |
| `check_all_branches` | bool | -- | Try all branches as baselines. Required -- omitting it is a parse error, not a default of `false`. |
| `only_branches` | list | -- | Additional specific branches to try (optional). Additive: when `check_all_branches` is also true, these are appended to the full branch list rather than replacing it. |

Baselines are tried in order and the first one the series applies to wins.
A tree taken from a MAINTAINERS `T:` entry without a branch resolves to the
remote's HEAD. When that HEAD is a strict ancestor of the local mainline ref,
it is tried after every other candidate instead of in its usual place: it has
no commit that mainline lacks, and on some trees HEAD points to a commit dated
years ago.
Custom remotes are tried after any `base-commit:` trailer and the MAINTAINERS
subsystem heuristic, but **before** linux-next and mainline. A subsystem topic
branch is where a series was actually developed, whereas linux-next carries a
snapshot of that branch which lags by at least one daily build -- and shares no
SHAs with it once the maintainer rebases.

Use this to reach topic branches that the MAINTAINERS `T:` entry doesn't name.
For example, the NFSD entry lists a tree but no branch, so the only candidate it
yields is `cel/HEAD` (a symref to `cel/master`):

```toml
[[git.custom_remotes]]
name = "cel"
url = "git://git.kernel.org/pub/scm/linux/kernel/git/cel/linux.git"
check_all_branches = false
only_branches = ["nfsd-next", "nfsd-testing"]
```

Match `name` and `url` to a remote already in the repository when one exists.
`ensure_remote` rewrites the URL whenever the configured one differs, so an
`https://` URL here against a `git://` remote flips it back and forth on every
pass.

### `[review]`

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `concurrency` | integer | -- | Number of concurrent reviews. |
| `worktree_dir` | string | -- | Directory for git worktrees used during daemon reviews. The daemon empties it on startup, so it must hold nothing else. |
| `timeout_seconds` | integer | `3600` | Maximum time per review (seconds). |
| `max_retries` | integer | `3` | Retry count on transient failures. |
| `max_lines_changed` | integer | `10000` | Skip patches with more changed lines than this. |
| `max_files_touched` | integer | `200` | Skip patches touching more files than this. |
| `ignore_files` | list | `[]` | File patterns to skip during review (e.g. `MAINTAINERS`). |
| `email_policy_path` | string | `"projects/linux/mailing_lists.toml"` | Path to the per-project mailing list and email delivery policy file (`projects/<project>/mailing_lists.toml`). |
| `max_total_tokens` | integer | `5000000` | Maximum cumulative uncached tokens (input + output) per review. Cached tokens are excluded. Set to 0 to disable. |
| `max_total_output_tokens` | integer | `500000` | Maximum cumulative output tokens per review. Set to 0 to disable. |

### `[linux_bug]`

Controls pre-existing Linux kernel bug tracking and background analysis.
By default (`enabled = false`), pre-existing issues discovered during patch
reviews are ignored and the background bug worker is not started.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | bool | `false` | Enable pre-existing bug tracking, the `/api/bug/analyze` endpoint, and the background `BugWorker`. |
| `concurrency` | integer | `4` | Maximum number of concurrent bug analyses run by the background `BugWorker`. |
| `fix_check_enabled` | bool | `false` | Enable periodic verification of whether open bugs have been fixed in the upstream mainline branch (`master` for Linux, `main` for Sashiko). |
| `lease_ttl_seconds` | integer | `300` | How long a worker's claim on a bug stays valid without renewal. If the worker dies, the bug becomes claimable again once this elapses. |
| `max_attempts` | integer | `3` | How many analysis attempts a bug gets before it is abandoned. Abandoned bugs are never retried automatically. |
| `fix_check_interval_seconds` | integer | `21600` | Interval in seconds between periodic upstream fix checks against the mainline tree. Set to `0` to disable periodic checks. |
| `fix_check_batch_size` | integer | `50` | Maximum number of open bugs evaluated per upstream fix check cycle. |

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

When a patch is tagged with a subsystem, it can trigger subsystem-specific AI review rules (context loading) and specific email/embargo policies defined in `projects/<project>/mailing_lists.toml`.

```toml
[subsystems]
mapping = [
    { pattern = ".*drivers/.*", name = "Drivers" },
    { pattern = ".*net/.*", name = "Networking" },
    { pattern = ".*fs/.*", name = "Filesystems" },
    { pattern = ".*mm/.*", name = "Memory Management" },
]
```

## `projects/<project>/mailing_lists.toml`

Controls which mailing lists Sashiko tracks via NNTP, embargo delays, how Sashiko routes or suppresses review emails, and Patchwork check delivery. See [projects/linux/mailing_lists.toml](../projects/linux/mailing_lists.toml) and [docs/examples/email_policy.toml](examples/email_policy.toml).

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `defaults.track` | bool | `true` | Whether listed mailing lists are tracked via NNTP by default. |
| `defaults.embargo_hours` | integer | `0` | Hours to wait before publishing a review with findings. Clean reviews are released immediately after the complete patchset review succeeds. |
| `defaults.reply_to` | list | `[]` | Recipient targets when findings are present: `"author"`, `"list"`, `"recipients"`. |
| `defaults.positive_review` | string | `"none"` | Who receives a review email when 0 issues are found: `"none"`, `"author"`, or `"all"`. |
| `defaults.mute_all` | bool | `false` | Suppress all email sending for this scope. |
| `defaults.cc` | list | `[]` | Static CC addresses always included on review emails. |
| `defaults.ignored_emails` | list | `[]` | Author addresses to mute entirely. |

Each mailing list is configured as a `["<list-email>"]` table (e.g. `["bpf@vger.kernel.org"]`). Omitted fields inherit from `[defaults]`. Each mailing list section accepts:

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `track` | bool | `defaults.track` | Whether Sashiko ingests patches from this mailing list via NNTP. |
| `nntp_group` | string | derived from email | Explicit Lore NNTP group override when the group name cannot be derived by reversing the email domain and appending the list's local part. |
| `embargo_hours` | integer | `defaults.embargo_hours` | Hours to wait before publishing a review with findings. |
| `subject_prefixes` | list | `[]` | Subject prefix tags (e.g. `["net", "net-next"]`) used to disambiguate `embargo_hours` on cross-posted series. |
| `reply_to` | list | `defaults.reply_to` | Recipient targets for this list (`"author"`, `"list"`, `"recipients"`). |
| `positive_review` | string | `defaults.positive_review` | Positive review delivery mode (`"none"`, `"author"`, `"all"`). |
| `mute_all` | bool | `defaults.mute_all` | Suppress all email sending for patches matching this list. |
| `cc` | list | `[]` | Additional static CC addresses for this list (merged with `defaults.cc`). |
| `ignored_emails` | list | `[]` | Additional author addresses to mute for this list. |
| `patchwork.enabled` | bool | `false` | Enable Patchwork integration for this mailing list. |
| `patchwork.api_url` | string | -- | Patchwork REST API URL (e.g. `https://patchwork.kernel.org/api/1.3`). Trailing slashes are stripped automatically. Invalid schemes are rejected with a warning. |
| `patchwork.token` | string | -- | Patchwork API token. Can also be set via `SASHIKO_PATCHWORK_TOKEN` env var (fills in where token is omitted in TOML). |
| `patchwork.email` | string | -- | Email address for email-based Patchwork notifications. |
| `patchwork.min_severity` | string | -- | Minimum finding severity to include in patchwork checks. Findings below this threshold are excluded. Accepts: `Low`, `Medium`, `High`, `Critical` (case-insensitive). Default: all findings included. |
| `patchwork.fail_severity` | string | `High` | Minimum severity of NEW findings that triggers the `fail` check state instead of `warning`. New findings at or above this threshold produce `fail`; below it produce `warning`. Pre-existing findings never affect the check state. |

### Author-only delivery

A mailing list can be **tracked** (its patches are ingested and reviewed) while **not pinging** that list with the review email — sending only to the patch author instead:

```toml
["intel-xe@lists.freedesktop.org"]
reply_to = ["author"]
```

### Patchwork integration

Sashiko can report review results as
[checks](https://patchwork.readthedocs.io/en/latest/usage/overview/#checks)
on a Patchwork instance. Two delivery modes are available and can be
enabled simultaneously for the same mailing list.

**API mode** posts checks directly to the Patchwork REST API with
retry-queuing (3 attempts, exponential backoff). Requires a maintainer
API token. Note: Patchwork tokens grant full project-maintainer
permissions (state changes, delegation, etc.), not just check access.

```toml
["netdev@vger.kernel.org".patchwork]
enabled = true
api_url = "https://patchwork.kernel.org/api/1.3"
token = "your-api-token"   # or set SASHIKO_PATCHWORK_TOKEN env var
```

**Email mode** sends a structured notification email to a bot address.
A local script (such as
[pw_tools](https://github.com/mchehab/pw_tools)) parses the email and
posts the check. This avoids giving Sashiko a write token.

```toml
["linux-media@vger.kernel.org".patchwork]
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
["netdev@vger.kernel.org".patchwork]
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
| `SASHIKO__FORGE__WEBHOOK_SECRET` | Override webhook secret from Settings.toml. Avoids storing the secret on disk. |
| `SASHIKO_PATCHWORK_TOKEN` | Patchwork API token. Fills in `patchwork.token` for enabled subsystems that have `api_url` set but no explicit token in TOML. |
| `NO_COLOR` | Disable ANSI color output. |
| `SASHIKO_LOG_PLAIN` | Use plain log format (no level/target/timestamp). |
