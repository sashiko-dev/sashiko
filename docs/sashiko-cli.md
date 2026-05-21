# sashiko-cli Reference

`sashiko-cli` is the command-line interface for interacting with a Sashiko
daemon or for running standalone local reviews. It can submit patches for
review, query review status, and display findings -- all from the terminal.

See also: [main README](../README.md) for project overview and daemon setup.

## Installation

```bash
# From source
cargo build --release --bin sashiko-cli

# Or via Nix
nix profile add github:sashiko-dev/sashiko
```

For local reviews the review worker binary is also required:

```bash
cargo build --release --bin review
```

## Global Options

| Flag | Description |
|------|-------------|
| `--server <URL>` | Override the server URL (default: from Settings.toml or `http://127.0.0.1:8080`). Also settable via `SASHIKO_SERVER` env var. |
| `--format <FORMAT>` | Output format: `text` (default) or `json`. |
| `--color <COLOR>` | Color output: `auto` (default), `always`, or `never`. |
| `-V, --version` | Print the tool version. |

## Commands

### submit

Submit a patch or range for review on a running daemon.

```
sashiko-cli submit [OPTIONS] [INPUT]
```

**Arguments:**

- `INPUT` -- Revision range, commit SHA, mbox file path, or lore.kernel.org
  URL. Defaults to `HEAD` if in a git repo, or reads mbox from stdin if piped.

**Options:**

| Flag | Description |
|------|-------------|
| `--type <TYPE>` | Override auto-detection: `mbox`, `remote`, `range`, or `thread`. |
| `-r, --repo <PATH>` | Override repository path (defaults to Settings.toml value). |
| `--baseline <REF>` | Baseline commit for mbox injection. |
| `--skip-subject <PATTERN>` | Skip patches matching subject pattern (supports wildcards, e.g. `mm:*`). May be specified multiple times. |
| `--only-subject <PATTERN>` | Only review patches matching subject pattern. May be specified multiple times. |

**Examples:**

```bash
# Review the last 3 commits
sashiko-cli submit HEAD~3..HEAD

# Review a specific commit
sashiko-cli submit abc1234

# Submit an mbox file
sashiko-cli submit my-patches.mbox

# Submit from stdin
git format-patch --stdout HEAD~2..HEAD | sashiko-cli submit

# Review a lore.kernel.org thread
sashiko-cli submit https://lore.kernel.org/linux-kernel/some-msgid/
```

### status

Show server status and queue statistics.

```
sashiko-cli status
```

### list

List patchsets or reviews.

```
sashiko-cli list [OPTIONS] [FILTER]
```

**Arguments:**

- `FILTER` -- Filter by status (e.g. `pending`, `failed`) or search term (e.g. `linux-mm`).

**Options:**

| Flag | Description |
|------|-------------|
| `--page <N>` | Page number (default: 1). |
| `--per-page <N>` | Items per page (default: 20). |

### show

Show detailed information about a patchset and its AI review.

```
sashiko-cli show [OPTIONS] [ID]
```

**Arguments:**

- `ID` -- Patchset ID or `latest` (default: `latest`).

**Options:**

| Flag | Description |
|------|-------------|
| `-w, --watch` | Stream status updates linearly. |
| `--patch <N>` | Show only patch N (1-indexed). Mutually exclusive with `--issues`. |
| `-s, --summary` | Show compact progress summary. |
| `-i, --issues` | Show only patches with issues found. Mutually exclusive with `--patch`. |
| `--since <ID>` | Show only reviews newer than this review ID. |
| `--inline` | Include inline review content in text output. |
| `-d, --diff <ID>` | Compare with another patchset ID. |

### rerun

Request a re-review of a completed patchset.

```
sashiko-cli rerun <ID>
```

`ID` is a numeric patchset ID.

### cancel

Cancel a pending review.

```
sashiko-cli cancel [OPTIONS] <ID>
```

`ID` is a numeric patchset ID.

**Options:**

| Flag | Description |
|------|-------------|
| `-f, --force` | Force cancel even if the review is already in progress. |

### local

Run a local review without requiring a running daemon, database, or network
connection. This is the primary command for reviewing your own patches during
development.

```
sashiko-cli local [OPTIONS] [INPUT]
```

**Arguments:**

- `INPUT` -- Git revision, range (e.g. `HEAD~3..HEAD`), or commit SHA.
  Defaults to `HEAD`.

**Options:**

| Flag | Description |
|------|-------------|
| `--baseline <REF>` | Baseline reference for patch application. Defaults to the parent of the first commit in the range. |
| `-r, --repo <PATH>` | Path to git repository. Defaults to current directory. |
| `--no-ai` | Skip AI review; only test that patches apply cleanly. |
| `--custom-prompt <TEXT>` | Append a custom prompt to the review task (e.g. focus on memory safety). |
| `--force-local` | Force local execution even if a sashiko daemon is running (see note below). |
| `--interactive` | On failure, pause and wait for code fixes or a typed rebuttal, then re-run automatically. |

**Important: two-tier behavior.** By default, `local` checks whether a sashiko
daemon is reachable at the configured host/port. If a daemon *is* running, the
command delegates to it via the API (equivalent to `submit`) and prints a
confirmation with the queued review ID. To guarantee a true local review -- no
daemon, no database -- pass `--force-local`.

**Prerequisites for true local review:**

1. The `sashiko-review` (or `review`) binary must be built and findable
   (looked up next to the CLI binary first, then via `PATH`).
   Build it with: `cargo build --release --bin review`.
2. `Settings.toml` must exist and configure at least the `[ai]` section.
3. An LLM API key must be set via the `LLM_API_KEY` environment variable
   (unless using a CLI-based provider like `claude-cli` or `copilot-cli`).

**Exit codes:**

| Code | Meaning |
|------|---------|
| 0 | Review completed with no high or critical findings (medium/low findings may still be present). |
| 1 | Review completed but high or critical severity findings were reported. |
| 3 | An error occurred during the review process. |

**Examples:**

```bash
# Review the HEAD commit locally (no daemon required)
sashiko-cli local --force-local

# Review the last 3 commits
sashiko-cli local HEAD~3..HEAD --force-local

# Review with a specific baseline branch
sashiko-cli local HEAD --baseline origin/main --force-local

# Dry run: only test that patches apply, skip AI
sashiko-cli local HEAD --no-ai --force-local

# Interactive mode: fix issues and re-run in a loop
sashiko-cli local HEAD --interactive --force-local

# Focus the AI on a specific concern
sashiko-cli local HEAD --custom-prompt "Focus on locking correctness" \
    --force-local

# Output as JSON (for scripting)
sashiko-cli --format json local HEAD --force-local
```

## Review Modes Compared

Sashiko supports three modes of operation depending on your needs:

### 1. True local review (no daemon)

Use `sashiko-cli local --force-local`. This is the simplest setup for
reviewing your own patches during development.

- **Requires:** `sashiko-review` binary, `Settings.toml` with `[ai]`
  configured, `LLM_API_KEY` set.
- **Does not require:** a running daemon, database, NNTP, or SMTP.
- **Results:** printed to stdout/stderr, not persisted.
- **Emails:** none sent.

### 2. Daemon with email disabled (local with persistence)

Run the daemon normally but leave SMTP unconfigured (default) and set
`mute_all = true` in `email_policy.toml` (also default). Submit patches
with `sashiko-cli submit` or `sashiko-cli local`.

- **Requires:** a running `sashiko` daemon.
- **Provides:** database persistence, web UI at `http://localhost:8080`,
  review history, re-runs.
- **Emails:** none sent (SMTP commented out, `mute_all = true`).

This mode is useful when you want to review multiple patch series over
time and compare results.

### 3. Full deployment (daemon + email)

Configure SMTP in `Settings.toml` and adjust `email_policy.toml` to
control delivery. The daemon will send review emails to authors,
maintainers, and/or mailing lists.

- **Requires:** full SMTP configuration, careful email policy setup.
- **Provides:** everything in mode 2, plus automated email delivery.
- **Emails:** sent per the policy in `email_policy.toml`.

See the [Guide for Kernel Maintainers](../MAINTAINERS_GUIDE.md) for
email policy configuration.

## Configuration

Configuration files live in the project root. Per-provider example
configurations are in [docs/examples/](examples/).

- **Settings.toml** -- main application config (AI provider, server,
  git repo path, review settings). Copy one of the provider-specific
  examples from [docs/examples/](examples/) as a starting point.
- **email_policy.toml** -- email delivery policy. See
  [docs/examples/email_policy.toml](examples/email_policy.toml).

### Minimal Settings.toml for local review

If you only want to run local reviews, the minimum configuration is:

```toml
[ai]
provider = "gemini"                    # or "claude", "claude-cli", etc.
model = "gemini-3.1-pro-preview"

[git]
repository_path = "/path/to/your/kernel/tree"

[review]
worktree_dir = "review_trees"
```

Set your API key:

```bash
export LLM_API_KEY="your-api-key-here"
```

If you use a CLI-based provider (`claude-cli`, `copilot-cli`, `kiro-cli`),
no API key is needed -- the provider authenticates via its own subscription.

## Environment Variables

| Variable | Description |
|----------|-------------|
| `LLM_API_KEY` | API key for the configured LLM provider. |
| `SASHIKO_SERVER` | Override the daemon URL for CLI commands. |
