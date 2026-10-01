# Running the Sashiko Daemon (Server Mode)

While `sashiko review` runs standalone local reviews without a server or database, Sashiko can also run as a continuous **daemon** for automated mailing list (`lore.kernel.org` / NNTP) and forge (GitHub/GitLab) monitoring, hosting the Web UI, and serving remote CLI requests via `sashiko-cli`.

See also:
- [Main README](../README.md) for an overview and local review quick start
- [Configuration Reference](configuration.md) for all `Settings.toml` and `email_policy.toml` options
- [sashiko-cli Reference](sashiko-cli.md) for interacting with a running daemon

## Prerequisites

- **Rust**: Version 1.90 or later.
- **Git**: For managing the repository and reference kernel tree.
- **LLM Provider API Key**: Access to an LLM provider (see the [LLM Provider Configuration Guide](llm-providers.md)).

## Building from Source

### 1. Clone the repository

```bash
git clone --recursive https://github.com/sashiko-dev/sashiko.git
cd sashiko
```

*Note: The `--recursive` flag initializes the `third_party/linux` reference kernel submodule and prompt submodules.*

### 2. Configure

Copy the example configuration and set your LLM API key:

```bash
cp docs/examples/Settings.example.toml Settings.toml
export LLM_API_KEY="your-api-key-here"
```

For a full reference of every setting, see the [Configuration Reference](configuration.md). The default `Settings.toml` includes sections for:

- **Database**: SQLite database path (`sashiko.db`).
- **NNTP & Mailing Lists**: Server details, optional TLS, and `lore.kernel.org` groups to monitor.
- **AI**: Provider and model selection (see [LLM Provider Configuration Guide](llm-providers.md)).
- **Server**: API and Web UI host, port, and `[server.acl]` capabilities.
- **Git**: Path to the reference kernel repository and optional `[[git.custom_remotes]]`.
- **Review**: Concurrency, token limits, and worktree settings.
- **Forge** *(Experimental)*: GitHub/GitLab webhook integration (optional, unsupported). See the [Forge Setup Guide](FORGE_SETUP.md) ([GitHub](GITHUB_SETUP.md), [GitLab](GITLAB_SETUP.md), [Webhook Security](WEBHOOK_SECURITY.md)).
  - When enabling `[forge]`, the NNTP (mailing list) ingestor is disabled by default. To monitor both, set `disable_nntp = false` in `[forge]`.
- **Subsystems**: Map file patterns or mailing list recipients to subsystems for targeted reviews and email policies.

### 3. Build

```bash
cargo build --release
```

## Starting the Daemon

The daemon monitors configured mailing lists (NNTP) or forge webhooks, manages the SQLite database, coordinates background AI review workers, and serves the Web UI and HTTP API.

```bash
sashiko
```

(Or from source: `cargo run --release --bin sashiko`, or via Nix: `nix run github:sashiko-dev/sashiko`)

### Web Interface

Once the daemon is running, it prints the local URL (default `http://localhost:8080`) where you can access the Web UI to browse patchsets, review statuses, inline findings, and bug tracking.

## Interacting with the Daemon (`sashiko-cli`)

Use `sashiko-cli` to submit patches or query review status on a running daemon:

```bash
# Submit a commit range to the running daemon
sashiko-cli submit HEAD~3..HEAD

# Submit a lore.kernel.org thread
sashiko-cli submit https://lore.kernel.org/linux-kernel/some-msgid/

# Check server queue status
sashiko-cli status

# Show the latest review
sashiko-cli show latest
```

(From source: `cargo run --bin sashiko-cli -- [OPTIONS] [COMMAND]`, or via Nix: `nix profile add github:sashiko-dev/sashiko`)

For the complete command list, see the [CLI Reference](sashiko-cli.md).

## Security & Access Control

Sashiko implements stateless JWT-based authorization and login via sign-in links:

- **Capability-based ACLs**: API mutations are controlled by `[server.acl]` in `Settings.toml`. Administrators assign user identities to specific capability arrays (`admins`, `ingest`, `cancel`, `review`), adhering to the principle of least privilege.
- **Bug Tracker Access**: Two additional arrays cover the Linux kernel bug tracker:
  - `security`: Lists the kernel security team, whose members can read and comment on every bug and inspect raw AI transcripts without gaining mutation capabilities.
  - `bug_reporters`: Lists principals permitted to file bugs over HTTP (empty by default so only admins can).
  - Subsystem maintainer authority over individual bugs is resolved dynamically per request from `MAINTAINERS`.
- **Blocklist & Fail-Closed Default**: An explicit `blocklist` array denies access unconditionally, overriding all capabilities and admin rights. If `[server.acl]` is omitted or empty, the server operates in a fail-closed mode where no remote mutations are permitted.
- **Data Retention**: The server operates without persistent user accounts and retains no personal user data aside from explicit comments, actions, and review history metadata submitted onto hosted bugs.
