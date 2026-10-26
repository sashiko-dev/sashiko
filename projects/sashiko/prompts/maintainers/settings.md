+++
title = "Application Settings and Configuration Policy"
paths = [
    "Settings.toml",
    "docs/configuration.md",
    "src/settings.rs",
]
symbols = [
    "AiSettings",
    "DatabaseSettings",
    "ReviewSettings",
    "Settings",
    "SmtpSettings",
    "redact_secret",
]
+++

## Settings Schema and Documentation Sync

- Because top-level configuration structs in `src/settings.rs` use
  `#[serde(deny_unknown_fields)]`, adding, renaming, or removing any setting
  field requires updating `Settings.toml`, `Default` implementations, and
  `docs/configuration.md` in lockstep so shipped example configs never fail to
  parse.
- Precedence order must be preserved: CLI flags override environment variables
  (`SASHIKO_*`), which override the selected configuration file
  (`--settings` / `SASHIKO_CONFIG` / `./Settings.toml` / `~/.config/sashiko.toml`),
  which overrides compiled defaults.

## Secret Masking in Settings

- Any new credential, token, password, or webhook secret field added to a
  settings struct must be masked (`"***"`) in custom `fmt::Debug`
  implementations and redacted via `redact_secret` whenever URLs or connection
  strings are logged.
- Never log or serialize raw `Settings` structs containing unredacted secrets into
  API responses or error messages.
