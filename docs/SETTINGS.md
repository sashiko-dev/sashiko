# Sashiko Settings

Terse guide for `Settings.toml`.

## Global
- `log_level`: verbosity. `info`, `debug`, `error`.

## [project]
- `name`: display name.
- `description`: project info.

## [forge]
- `enabled`: use GitHub/GitLab webhooks. `true` or `false`.
- `provider`: `github` or `gitlab`.
- `webhook_secret`: auth token for webhook.
- `api_token`: token for PR comments/actions.

## [subsystems]
- `mapping`: regex link email to subsystem name.
  - `pattern`: email regex.
  - `name`: subsystem label.

## [database]
- `url`: SQLite file path. `sashiko.db`.
- `token`: secret for DB auth (if needed).

## [mailing_lists]
- `track`: list of Lore names to monitor. `linux-kernel`, `netdev`.

## [nntp]
- `server`: Lore NNTP host. `nntp.lore.kernel.org`.
- `port`: NNTP port. Default `119`.

## [ai]
- `provider`: `gemini`, `openai`, `claude`, `bedrock`, `claude-cli`.
- `model`: model ID.
- `max_input_tokens`: cap for context.
- `max_interactions`: loop limit.
- `temperature`: randomness. `0.0` to `1.0`.

## [ai.claude]
- `prompt_caching`: reuse context. Save tokens.

## [server]
- `host`: API listen address. `127.0.0.1`.
- `port`: API port. `8080`.

## [git]
- `repository_path`: local path to target repo.

## [review]
- `concurrency`: max simultaneous reviews.
- `worktree_dir`: path for temporary git worktrees.
- `timeout_seconds`: kill stuck review.
- `max_retries`: attempt count on transient fail.
- `ignore_files`: files to skip in review.

## [tools] (Optional)
- `enabled`: allowlist tools.
- `disabled`: denylist tools.

## [prompts] (Optional)
- `directory`: local path or remote Git URL for prompts.
- `stages_config`: path to `stages.toml`.
- `variables`: key-value map for prompt templates.
