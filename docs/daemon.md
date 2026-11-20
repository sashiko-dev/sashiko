# Running the Sashiko Daemon (Server Mode)

While `sashiko review` runs standalone local reviews without a server or database, Sashiko can also run as a continuous **daemon** for automated mailing list (NNTP, such as `lore.kernel.org` or `inbox.sourceware.org`) and forge (GitHub/GitLab) monitoring, hosting the Web UI, and serving remote CLI requests via `sashiko-cli`. Each daemon instance serves one configured project (`--project linux` by default).

See also:
- [Main README](../README.md) for an overview and local review quick start
- [Configuration Reference](configuration.md) for all `Settings.toml` and `projects/<project>/mailing_lists.toml` options
- [sashiko-cli Reference](sashiko-cli.md) for interacting with a running daemon

## Prerequisites

- **Rust**: Version 1.90 or later.
- **Git**: For managing the repository and target project tree (Linux kernel by default).
- **LLM Provider API Key**: Access to an LLM provider (see the [LLM Provider Configuration Guide](llm-providers.md)).

## Building from Source

### 1. Clone the repository

```bash
git clone https://github.com/sashiko-dev/sashiko.git
cd sashiko
```

Then fetch the tree the daemon applies and reviews patches against:

```bash
make repo                   # Linux, into third_party/linux
make repo PROJECT=sashiko   # another project, into third_party/<project>
```

`make repo` runs `sashiko repo sync`, which clones the project's upstream
with full history (several GB for Linux) when the directory is missing and
fetches new commits when it is not; run it again whenever you like. With an
installed binary, run `sashiko [--project <project>] repo sync` directly: it
syncs the directory `[git] repository_path` names, else
`third_party/<project>`, or `--path <dir>`.

The daemon reads the tree from `[git] repository_path` and refuses to start
when that is not a git repository, so any existing clone works too: point the
setting at it instead of cloning another copy.

### 2. Configure

Copy the example configuration and set your LLM API key:

```bash
cp docs/examples/Settings.example.toml Settings.toml
export LLM_API_KEY="your-api-key-here"
```

The example targets local review and lacks the three settings the daemon
refuses to start without. Add them, putting `worktree_dir` into the
example's existing `[review]` table:

```toml
[database]
url = "sashiko.db"

[git]
repository_path = "third_party/linux"

[review]
worktree_dir = "review_trees"
```

For a full reference of every setting, see the [Configuration Reference](configuration.md). The default `Settings.toml` includes sections for:

- **Database**: SQLite database path (`sashiko.db`).
- **NNTP & Mailing Lists**: Server details, optional TLS, and NNTP mailing list groups to monitor (such as `lore.kernel.org` for Linux or `inbox.sourceware.org` for GCC).
- **AI**: Provider and model selection (see [LLM Provider Configuration Guide](llm-providers.md)).
- **Server**: API and Web UI host, port, and `[server.acl]` capabilities.
- **Git**: Path to the reference target repository and optional `[[git.custom_remotes]]`.
- **Review**: Concurrency, token limits, and worktree settings.
- **Forge**: GitHub/GitLab webhook integration (used in production on `sashiko.sashiko.dev`; general third-party setups remain experimental). See the [Forge Setup Guide](FORGE_SETUP.md) ([GitHub](GITHUB_SETUP.md), [GitLab](GITLAB_SETUP.md), [Webhook Security](WEBHOOK_SECURITY.md)).
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

(Or from source: `cargo run --release --bin sashiko`)

By default (`log_level = "info"`), the daemon logs concise lifecycle milestones, completed reviews and bug analyses, outbound deliveries, warnings, and errors while clamping third-party transport crates to `warn`. Pass `--debug` (or set `log_level = "debug"` in `Settings.toml`) to inspect per-turn AI requests, git/worktree mechanics, HTTP read queries, and polling cycles, or set `RUST_LOG` for fine-grained `tracing` target filters.

### Web Interface

Once the daemon is running, it prints the local URL (default `http://localhost:8080`) where you can access the Web UI to browse patchsets, review statuses, inline findings, and bug tracking.

## Monitoring, Statistics UI & Prometheus API

Sashiko exposes an O(1) Prometheus-compatible telemetry surface backed by live SQLite state gauges, multi-resolution rollups (`5m`, `1h`, `1d`), bounded per-review pipeline summaries, and lock-free runtime atomics.

### 4-Tab Statistics Dashboard (`#/stats`)

The built-in Web UI consumes Sashiko's Prometheus HTTP API directly across four persona-focused views:

1. **Project (`#/stats/project`)**: Project and maintainer metrics across `4w` / `12w` / `26w` / `52w` / `All` windows with upstream release date markers — weekly patchsets (`v1` vs `v2+`) and patches, clean patchset and clean patch rates + weekly clean percentage trend, issues per patchset/patch by severity, prior review issues fixed vs left unfixed in `v2+` revisions across Sashiko reviews, human reviewers, and other bots (LKP/CI), `Failed To Apply` rate, and optional mailing list filtering.
2. **Bugs (`#/stats/bugs`)**: Open bugs, CVSS v4.0 high/critical open security backlog, upstream fixes, duplicate/dismissed filtering, cumulative open vs fixed line trend across `12w` / `26w` / `52w` / `All` windows with upstream release date markers, engineering vs CVSS severity comparison, and `BugAccess`-scoped per-subsystem open vs 60-day fixed facets.
3. **Health (`#/stats/health`)**: Ongoing service monitoring across `1h` and `24h` windows with `7d` / `30d` baselines — active queue backlog, message ingestion vs 7d ago, review outcomes and failure rates, outgoing email delivery (pending and sent), errors/warnings/AI errors/worker restarts, git remote freshness with a list of stale git trees, and LLM token burn + prompt cache hit rate.
4. **Pipeline (`#/stats/pipeline`)**: Last `100` / `500` / `1000` non-inherited review summaries for Sashiko developers — active-concern verification funnel and dismissal rate (excluding Stage-1 self-dismissals), findings per review, exclusive stage catches, 2D stage × stage co-discovery overlap heatmap, stage precision and token-per-bug ROI table, and agent tool usage.

### Prometheus Scrape & HTTP Query Endpoints

- **`GET /metrics`**: Prometheus text `0.0.4` exposition endpoint for external Prometheus or VictoriaMetrics scrapers. Public by default; set `metrics_token` under `[server]` in `Settings.toml` (or `SASHIKO__SERVER__METRICS_TOKEN`) to require `Authorization: Bearer <metrics_token>` exclusively on `GET /metrics` without affecting Web UI or API routes.
- **`GET|POST /api/v1/query`, `/api/v1/query_range`, & `POST /api/v1/query_batch`**: Bounded PromQL instant, range, and batched evaluation (`increase`, `rate`, `sum_over_time`, `avg_over_time`, `max_over_time`, `sum`/`avg`/`min`/`max`/`count` with `by`/`without`, `topk`, `clamp_min`, `histogram_quantile`, `offset`, regex label matchers, and binary arithmetic). Long-term trend views also query `sashiko_project_release_timestamp_seconds` to mark upstream project releases. All stats and Prometheus endpoints enforce per-IP and global token-bucket rate limits (`429`), an in-flight concurrency cap (`503`), per-request timeouts (`504`), bucket-aligned caching, and AST/row/sample budgets so polling loops or floods cannot starve the daemon.
- **`GET|POST /api/v1/labels`, `GET /api/v1/label/{name}/values`, `GET /api/v1/metadata`**: Label and metric discovery endpoints. `sashiko_bugs_by_subsystem` and `subsystem` bug facets enforce `MAINTAINERS` and `[server.acl]` visibility (`OptionalPrincipal`), returning only sections authorized for the caller.

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

(From source: `cargo run --bin sashiko-cli -- [OPTIONS] [COMMAND]`)

For the complete command list, see the [CLI Reference](sashiko-cli.md).

## Security & Access Control

Sashiko implements stateless JWT-based authorization and login via sign-in links:

- **Capability-based ACLs**: API mutations are controlled by `[server.acl]` in `Settings.toml`. Administrators assign user identities to specific capability arrays (`admins`, `ingest`, `cancel`, `review`), adhering to the principle of least privilege.
- **Bug Tracker Access**: Two additional arrays cover the pre-existing bug tracker:
  - `security`: Lists security team members, who can read and comment on every bug and inspect raw AI transcripts without gaining mutation capabilities.
  - `bug_reporters`: Lists principals permitted to file bugs over HTTP (empty by default so only admins can).
  - For Linux deployments, subsystem maintainer authority over individual bugs is also resolved dynamically per request from `MAINTAINERS` and `projects/linux/upstream_maintainers.toml`.
- **Blocklist & Fail-Closed Default**: An explicit `blocklist` array denies access unconditionally, overriding all capabilities and admin rights. If `[server.acl]` is omitted or empty, the server operates in a fail-closed mode where no remote mutations are permitted.
- **Data Retention**: The server operates without persistent user accounts and retains no personal user data aside from explicit comments, actions, and review history metadata submitted onto hosted bugs.
