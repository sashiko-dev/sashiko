# Design: O(1) Prometheus-Compatible Statistics & 4-Tab Telemetry Rework

## 1. Problem Statement & Motivation

Sashiko's current statistics implementation has three structural limitations as
production instances (`sashiko.dev`, `sashiko.sashiko.dev`, `gcc.sashiko.dev`)
accumulate millions of messages and hundreds of thousands of reviews:

1. **O(Database Size) Read Queries:**
   - `Database::get_timeline_stats` executes five unbounded `GROUP BY day`
     scans across `messages`, `patchsets`, `patches JOIN messages`, `reviews`,
     and `findings JOIN reviews`.
   - `Database::get_review_stats` executes full-table `COUNT(*)` queries over
     `reviews`, including `lower(status) LIKE '%failed%'` which bypasses B-tree
     indexes entirely.
   - `MetricsWorker` runs `SELECT COUNT(*) FROM messages` and `patchsets` every
     60 seconds, walking full B-tree leaf chains because SQLite does not store
     per-table row counts in headers.
2. **Missing Operational, Maintainer, Bug, and Pipeline Visibility:**
   - SRE signals—ingested message rates over 1h/24h vs 7d/30d baselines, stale
     git trees (`.sashiko/fetch_timestamps`), `tracing` warnings/errors, LLM
     turn and review turnaround latencies (`p50`/`p90`/`p99`), 429 rate-limit
     cooldowns, cache hit rates, worker supervision restarts, and outbound
     email/forge/patchwork delivery rates—are either trapped in local variables
     or written only to `stderr`.
   - Rich bug database facets (CVSS v4.0 security ratings, `security_class`,
     engineering severities, duplicate/fix velocity, reproducer/fix coverage,
     and `BugAccess`-scoped subsystem breakdowns) are absent from `#/stats`.
   - Maintainer-oriented series health metrics (weekly patch/patchset volume,
     findings per patch and per patchset by severity, clean series rate, author
     fixups from `prior_feedback_resolutions`, unchanged-clean review skips, and
     baseline `Failed To Apply` rates) are not surfaced.
   - Pipeline stage telemetry (`review_map`) is embedded inside gzip-compressed
     `ai_interactions.output_raw` blobs (~20–100 KiB each), preventing fast
     aggregation of stage discovery yield, exclusive vs overlapping stage finds,
     and Stage-1 / Verification / Post-Verification dismissal funnels across the
     last `N` reviews.
3. **No Standard Monitoring Interface:**
   - External scrapers (Prometheus, Google Managed Prometheus, Grafana Alloy)
     and Grafana dashboards have no `/metrics` or `/api/v1/query*` endpoints to
     consume, and the web UI uses bespoke one-off JSON shapes.

## 2. Goals & Non-Goals

### Goals

1. **Strictly O(1) Read Complexity:** Reading statistics—whether via `/metrics`,
   `/api/v1/query`, `/api/v1/query_range`, or `/api/stats`—must never scan
   unbounded application tables (`messages`, `patchsets`, `patches`, `reviews`,
   `findings`, `bugs`, `ai_interactions`, `tool_usages`). Every query is bounded
   by fixed bucket windows or fixed facet/summary cardinalities, verified in
   automated tests via `EXPLAIN QUERY PLAN`.
2. **Prometheus-Compatible API Powering Both Frontend & External Tools:**
   - Expose `GET /metrics` (Prometheus text exposition `0.0.4`) for scrapers.
   - Expose `GET|POST /api/v1/query`, `GET|POST /api/v1/query_range`,
     `GET /api/v1/labels`, `GET /api/v1/label/{name}/values`, and
     `GET /api/v1/metadata` backed by Sashiko's embedded SQLite rollup and
     summary tables so `static/index.html` and external Grafana instances share
     one canonical Prometheus query interface.
3. **4 Persona-Focused Frontend Tabs (`#/stats/<tab>`):**
   - **Operational (`#/stats/ops`):** SRE view over Last `1h` / `24h` compared
     against `7d` and `30d` averages and `offset 7d` windows.
   - **Bugs (`#/stats/bugs`):** Triage, security (`SecurityClass` & CVSS v4.0
     rating), engineering severity, fix/duplicate velocity, and subsystem stats.
   - **Maintainers (`#/stats/maintainers`):** Weekly patchset/patch throughput,
     issues per patch(set) by severity, clean rate, author fixups across `vN`
     series, unchanged-clean skips, baseline apply hygiene, and turnaround SLA.
   - **Pipeline (`#/stats/pipeline`):** Last `N` completed reviews (`N` in
     `{100, 500, 1000}`): stage discovery yield (total vs exclusive), pairwise
     stage overlap heatmap, full concern-to-finding dismissal funnel, per-stage
     token/turn efficiency, tool usage, and human-feedback recall alignment.
4. **Zero-Stall Upgrade & Crash-Consistent Backfill:**
   - Initialize live gauges immediately and backfill historical time-series
     rollups and the latest 1,000 `review_pipeline_stats` rows asynchronously in
     small bounded transactions tracked via `meta` cursors without blocking
     daemon startup or `/health`.
5. **Strict Access Control & Embargo Preservation:**
   - Enforce `Principal` and `BugAccess` on per-subsystem bug counts so
     unprivileged callers never learn bug counts in unauthorized subsystems,
     while keeping non-identifying global bug totals public.
   - Support optional `server.metrics_token` bearer authentication on
     `GET /metrics`.
   - Exclude currently embargoed patchsets (`embargo_until > now`) from public
     Maintainer and Pipeline finding aggregations until embargo release.

### Non-Goals

- Implementing a full arbitrary Turing-complete PromQL engine (e.g., arbitrary
  binary vector-to-vector joins, `holt_winters`, `predict_linear`, subqueries).
  We implement the well-defined, deterministic PromQL subset required by the
  Sashiko UI and standard Grafana dashboards.
- Storing unbounded high-cardinality labels (such as `patchset_id`, `bugid`,
  `commit_sha`, `author`, or raw error strings) in time-series metrics.
- Changing local `sashiko review` CLI behavior (`--no-default-features` builds
  compile cleanly with lightweight no-op or in-memory atomic stubs where needed).

## 3. O(1) Storage & In-Memory Telemetry Architecture

### 3.1 Four Bounded Data Structures

All read paths consult only these four bounded structures:

| Structure | Backing | Max Cardinality / Rows Scanned | Purpose |
|---|---|---|---|
| **Live Registry** | Lock-free `AtomicU64` / `AtomicI64` / `AtomicHistogram` in `src/metrics.rs` | ~150 series in RAM (< 64 KiB) | Sub-millisecond `/metrics` scrape, live sub-minute deltas, latencies, `WARN`/`ERROR` rates, git tree staleness, LLM concurrency/cooldowns |
| **State Gauges** | `stats_state_gauges` & `stats_bug_subsystem_facets` in SQLite | ~250 global rows + <= ~500 subsystem facet rows | Exact crash-consistent counts of `messages`, `patchsets`, `reviews`, outboxes, and `bugs` facets maintained by SQLite triggers |
| **Time-Series Rollups** | `stats_series` + `stats_rollups` in SQLite | `5m` (48h = 576 pts), `1h` (45d = 1,080 pts), `1d` (5y = 1,825 pts) per series | O(1) range & `offset` queries (`1h`, `24h`, `7d`, `30d`, `52w`) across restarts |
| **Pipeline Summaries** | `review_pipeline_stats` in SQLite | `ORDER BY review_id DESC LIMIT 1000` (<= 1,000 narrow uncompressed rows) | O(1) Last-`N` (`100`, `500`, `1000`) stage yield, overlap matrix, dismissal funnel, stage tokens/turns, and tool stats |

### 3.2 Database Schema (`src/migrations/024_stats_rollups.sql`)

```sql
-- 1. Exact live state counts maintained incrementally via SQLite triggers.
CREATE TABLE IF NOT EXISTS stats_state_gauges (
    metric TEXT NOT NULL,
    labels_key TEXT NOT NULL,
    labels_json TEXT NOT NULL DEFAULT '{}',
    value INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (metric, labels_key)
);

-- 2. Pre-aggregated bug counts per subsystem for O(1) BugAccess-scoped reads.
--    is_authorizing = 1 when the link confers Principal read access
--    (bug_subsystems.source = 'maintainers_section' OR bug_upstream_sections).
CREATE TABLE IF NOT EXISTS stats_bug_subsystem_facets (
    subsystem TEXT NOT NULL,
    is_authorizing INTEGER NOT NULL CHECK (is_authorizing IN (0, 1)),
    lifecycle_status TEXT NOT NULL,
    severity_int INTEGER NOT NULL,
    security_rating TEXT NOT NULL,
    count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (
        subsystem,
        is_authorizing,
        lifecycle_status,
        severity_int,
        security_rating
    )
);

CREATE INDEX IF NOT EXISTS idx_stats_bug_subsys_rank
    ON stats_bug_subsystem_facets(is_authorizing, lifecycle_status, count DESC);

CREATE INDEX IF NOT EXISTS idx_stats_bug_subsys_nocase
    ON stats_bug_subsystem_facets(subsystem COLLATE NOCASE, is_authorizing);

-- 3. Multi-resolution time-series catalog and fixed-retention buckets.
CREATE TABLE IF NOT EXISTS stats_series (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    metric TEXT NOT NULL,
    labels_key TEXT NOT NULL,
    labels_json TEXT NOT NULL DEFAULT '{}',
    UNIQUE(metric, labels_key)
);

-- resolution_secs: 300 (5m, kept 48h), 3600 (1h, kept 45d), 86400 (1d, kept 5y).
CREATE TABLE IF NOT EXISTS stats_rollups (
    series_id INTEGER NOT NULL,
    resolution_secs INTEGER NOT NULL,
    bucket_ts INTEGER NOT NULL,
    value REAL NOT NULL DEFAULT 0,
    PRIMARY KEY (series_id, resolution_secs, bucket_ts),
    FOREIGN KEY(series_id) REFERENCES stats_series(id) ON DELETE CASCADE
);

-- 4. Pre-extracted uncompressed per-review summary (< 500 bytes/row) so Last-N
--    pipeline queries never decompress ai_interactions.output_raw blobs.
CREATE TABLE IF NOT EXISTS review_pipeline_stats (
    review_id INTEGER PRIMARY KEY,
    patchset_id INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    duration_secs INTEGER NOT NULL DEFAULT 0,
    project TEXT NOT NULL DEFAULT '',
    provider TEXT NOT NULL DEFAULT '',
    model TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT '',
    inherited INTEGER NOT NULL DEFAULT 0,
    raw_concerns INTEGER NOT NULL DEFAULT 0,
    stage1_dismissed INTEGER NOT NULL DEFAULT 0,
    deduplicated_clusters INTEGER NOT NULL DEFAULT 0,
    verified_1a INTEGER NOT NULL DEFAULT 0,
    dismissed_1b INTEGER NOT NULL DEFAULT 0,
    inconclusive INTEGER NOT NULL DEFAULT 0,
    verified_pv INTEGER NOT NULL DEFAULT 0,
    dismissed_pv INTEGER NOT NULL DEFAULT 0,
    findings_patch INTEGER NOT NULL DEFAULT 0,
    findings_preexisting INTEGER NOT NULL DEFAULT 0,
    tokens_in INTEGER NOT NULL DEFAULT 0,
    tokens_out INTEGER NOT NULL DEFAULT 0,
    tokens_cached INTEGER NOT NULL DEFAULT 0,
    stages_json TEXT NOT NULL DEFAULT '[]',
    overlap_json TEXT NOT NULL DEFAULT '[]',
    tools_json TEXT NOT NULL DEFAULT '[]',
    prior_json TEXT NOT NULL DEFAULT '{}',
    FOREIGN KEY(review_id) REFERENCES reviews(id) ON DELETE CASCADE,
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id) ON DELETE CASCADE
);
```

### 3.3 Trigger-Maintained Live State Gauges

SQLite triggers keep live state counts exact inside existing write transactions
(touching 1–5 cached B-tree rows per state change, ~2–5 us overhead):

- `messages`: `AFTER INSERT` / `AFTER DELETE` updates
  `('sashiko_messages_stored', '')`.
- `patchsets`: `AFTER INSERT` / `AFTER UPDATE OF status` / `AFTER DELETE`
  updates `('sashiko_patchsets_current', status)`.
- `patches`: `AFTER INSERT` / `AFTER UPDATE OF status` / `AFTER DELETE` updates
  `('sashiko_patches_current', status)`.
- `reviews`: `AFTER INSERT` / `AFTER UPDATE OF status, inherited_from_review_id`
  / `AFTER DELETE` updates `('sashiko_reviews_current', status_and_skip_kind)`.
- `email_outbox`, `forge_outbox`, `patchwork_outbox`: `AFTER INSERT` /
  `AFTER UPDATE OF status` / `AFTER DELETE` updates
  `('sashiko_outbox_depth', channel + status)`.
- `fetch_queue`: `AFTER INSERT` / `AFTER UPDATE OF status` / `AFTER DELETE`
  updates `('sashiko_fetch_queue_depth', status)`.
- `bugs`, `bug_subsystems`, `bug_upstream_sections`, and `bug_enrichments`
  (when `kind IN ('reproducer', 'testcase', 'poc', 'proposed_fix', 'suggested_fix')`):
  - Maintain global `('sashiko_bugs_current', lifecycle + pipeline + severity + security_class + security_rating)`
    and `('sashiko_bugs_attachments_current', attachment_kind + lifecycle)` in
    `stats_state_gauges`.
  - Maintain per-subsystem counts in `stats_bug_subsystem_facets`, deduplicating
    when a bug has the same section title in both `bug_subsystems`
    (`source = 'maintainers_section'`) and `bug_upstream_sections` via a
    primary-key `EXISTS` check so `count` always equals `COUNT(DISTINCT bug_id)`.
  - Ignore `caller_supplied` subsystem strings in `stats_bug_subsystem_facets`
    so untrusted API payloads cannot inflate table cardinality.

### 3.4 Runtime Hooks & 60-Second Rollup Flusher

Runtime events increment lock-free atomic counters and fixed-bucket histograms
in `src/metrics.rs`:

1. **`tracing` Warning & Error Layer (`src/logging.rs`):**
   - Switch `tracing_subscriber::fmt()...init()` in `src/main.rs` to
     `tracing_subscriber::registry().with(env_filter).with(fmt_layer).with(MetricsLayer)`.
   - `MetricsLayer` inspects `Metadata::level()` (`WARN` and `ERROR` only) and
     maps `Metadata::target()` prefix (`sashiko::ingestor`, `sashiko::nntp`,
     `sashiko::worker::email`, `sashiko::ai`, etc.) to a closed `Component` enum
     (`14` variants, fallback `Other`) and increments an `AtomicU64` slot in an
     array indexed by `(level, component)`—zero heap allocations, zero locks.
2. **LLM Provider Telemetry (`src/ai/`):**
   - Record per-turn HTTP latency into `AI_REQUEST_DURATION` histogram, token
     usage (`uncached_in`, `cached_in`, `out`) by `(workload, model_family)`,
     error classes (`rate_limit`, `transient`, `fatal`, `format_violation`),
     `CachingAiProvider` hit/miss deltas, `QuotaManager` remaining cooldown
     seconds, and `ConcurrencyLimitedProvider` in-flight requests.
3. **Git Tree Freshness & Sync (`src/worker/sync.rs`, `src/git_ops.rs`):**
   - Add `git_ops::inspect_remotes_freshness(&repo_path)` (run via
     `spawn_blocking` on startup and after every `GitSyncWorker` cycle) reading
     `.sashiko/fetch_timestamps/<remote>` and `<remote>.fail` `mtime`s without
     network I/O to publish exact gauges: `fresh`, `stale`, `backed_off`,
     `failed` remote counts and `max_remote_age_seconds`.
   - Record `fetch`, `repack`, and `baseline` durations into
     `GIT_OP_DURATION` histograms.
4. **Worker Supervision & Ingestion (`src/main.rs`, `src/ingestor.rs`):**
   - `spawn_supervised_worker_with_backoff` increments
     `sashiko_worker_restarts_total{worker}` and
     `sashiko_worker_panics_total{worker}`.
   - NNTP cycles update `sashiko_nntp_lag_articles` (sum and max of
     `high - committed` across tracked groups) and ingested/failed counters.
5. **Review, Lineage, Prior-Feedback & Outbox Completion:**
   - When a review completes, `Database` writes its `review_pipeline_stats` row
     (extracted in Rust from `review_map`, `ai_interactions`, `tool_usages`, and
     `reviews.created_at`) in the completion path and records review duration,
     patchset turnaround duration (`now - patchset.date`, clamped `>= 0`),
     finding severities, prior-feedback outcomes (`addressed`,
     `confirmed_finding`, `settled`, `disproven`), and patch lineage counts
     (`unchanged`, `modified`, `new`).
6. **60-Second Rollup Flush & Reaper (`MetricsWorker`):**
   - Every 60s (and on graceful `SIGTERM`/`SIGINT` shutdown), `MetricsWorker`
     snapshots atomic counter/histogram deltas since the previous flush and
     writes them in one batched transaction into `stats_rollups` across `300s`
     (`5m`), `3600s` (`1h`), and `86400s` (`1d`) buckets (`value = value + ?`).
   - Queries over `/api/v1/query` and `/api/v1/query_range` add the current
     in-memory unflushed delta onto the latest bucket so live operational graphs
     reflect events within milliseconds rather than waiting for the 60s tick.
   - Once per hour, `MetricsWorker` reaps expired buckets (`5m` > 48h, `1h` >
     45d, `1d` > 5y) with bounded `DELETE FROM stats_rollups WHERE rowid IN
     (SELECT rowid FROM stats_rollups WHERE resolution_secs = ? AND bucket_ts < ?
     LIMIT 5000)` statements so pruning never stalls writers.

### 3.5 Zero-Stall Migration & Crash-Safe Historical Backfill

To avoid blocking startup probes on multi-hundred-GB production databases:

1. **Synchronous Startup Step (`Database::migrate` to `v24`):**
   - Creates the `023` tables, indexes, and triggers.
   - Immediately populates `bugs` facets (`stats_state_gauges` and
     `stats_bug_subsystem_facets`), outbox depths, `fetch_queue` depths, and
     covering-index status counts (`idx_patchsets_status`,
     `idx_reviews_grouping`).
   - Captures O(1) high-water marks `MAX(id)` for `messages`, `patchsets`,
     `patches`, `reviews`, `bugs`, `email_outbox`, `forge_outbox`, and
     `patchwork_outbox` into `meta` (`stats_backfill:cutoffs`) so live triggers
     and runtime accumulators handle all rows above the cutoff without overlap.
2. **Asynchronous Bounded Backfill (`src/worker/stats_backfill.rs`):**
   - Step A (Priority — < 1s): Backfills `review_pipeline_stats` for the latest
     `1,000` completed reviews (`id <= cutoff_review_id ORDER BY id DESC LIMIT
     1000`) in batches of `50` rows, decompressing `ai_interactions.output_raw`
     inside `spawn_blocking` so Tab 4 is populated almost immediately after boot.
   - Step B: Walks `messages` ( counting total + `5m`/`1h`/`1d` buckets),
     `patchsets`, `patches`, `reviews` + `findings` + `ai_interactions`,
     `prior_feedback_resolutions`, and `bugs` backwards from each table's cutoff
     ID in chunks of `2,000` rows.
   - Each chunk updates `stats_rollups` (and `messages` total gauge) AND
     advances `stats_backfill:cursor:<table_name>` inside the **same**
     `DatabaseTransaction`, sleeping `10ms` between chunks to yield the write
     lock. A crash or pod restart resumes from the exact saved cursor with zero
     double-counting or skipped rows.

## 4. Prometheus-Compatible HTTP API

### 4.1 Endpoints

| Endpoint | Format | Auth Policy | Purpose |
|---|---|---|---|
| `GET /metrics` | `text/plain; version=0.0.4; charset=utf-8` | Public by default; if `server.metrics_token` is set, requires `Bearer <metrics_token>` or Admin `Principal`. Per-subsystem bug series require global `BugAccess` or `metrics_token`. | External Prometheus / GMP / Alloy scraping |
| `GET\|POST /api/v1/query` | Prometheus JSON (`vector` / `scalar`) | `OptionalPrincipal` (`sashiko_bugs_by_subsystem` filtered by `Principal`) | Instant vector queries, KPI cards, comparison deltas, Pipeline Last-`N` vectors |
| `GET\|POST /api/v1/query_range` | Prometheus JSON (`matrix`) | `OptionalPrincipal` (`sashiko_bugs_by_subsystem` filtered by `Principal`) | Time-series charts (`5m`, `1h`, `1d`, `7d` steps) |
| `GET /api/v1/labels` | Prometheus JSON | `OptionalPrincipal` | Label discovery for Grafana & UI |
| `GET /api/v1/label/{name}/values` | Prometheus JSON | `OptionalPrincipal` (`subsystem` values scoped to `Principal`) | Label value autocomplete (e.g. subsystem selector) |
| `GET /api/v1/metadata` | Prometheus JSON | Public | Metric `type`, `help`, and `unit` catalog |
| `GET /api/stats` | Sashiko JSON | Public | Preserved in O(1) for `sashiko-cli status` and header/footer summary bar |

Legacy `/api/stats/timeline`, `/api/stats/reviews`, and `/api/stats/tools` are
re-pointed to O(1) projections over `stats_rollups` and `review_pipeline_stats`
so existing scripts or bookmarks cannot trigger table scans or suffer broken
contracts.

### 4.2 Supported PromQL Subset & Safety Bounds

`src/promql.rs` implements a deterministic, zero-dependency parser and evaluator
over Sashiko's rollup and gauge store:

- **Instant & Range Selectors:**
  `metric_name{label="val", label!="val", label=~"a|b", label!~"a|b"}` with
  optional `[window]` (`5m`, `1h`, `24h`, `7d`, `1w`, `30d`, `365d`) and
  optional `offset <duration>` (`offset 24h`, `offset 7d`, `offset 30d`).
- **Range Vector Functions:**
  - `increase(v[d])`: exact bucket sum across `[t - d, t]` (automatically
    selecting the finest resolution tier covering `t - d`: `5m` for `<= 48h`,
    `1h` for `<= 45d`, `1d` beyond `45d`).
  - `rate(v[d])`: `increase(v[d]) / d_seconds`.
  - `sum_over_time(v[d])`, `avg_over_time(v[d])`, `max_over_time(v[d])`.
- **Aggregations & Operators:**
  - `sum`, `avg`, `min`, `max`, `count` with optional `by (l1, ...)` or
    `without (l1, ...)`.
  - `topk(k, expr)` (`k` clamped to `1..=100`).
  - `histogram_quantile(phi, expr)` over `le`-labeled bucket vectors (`p50`,
    `p90`, `p95`, `p99`).
  - Scalar arithmetic (`/`, `*`, `+`, `-`) for computing averages such as
    `increase(sashiko_messages_ingested_total[7d]) / 7`.
- **Resource & DoS Guards (`maintainers/rust-async.md`):**
  - Query string length capped at `2,048` bytes; AST depth capped at `8`.
  - Regex matchers (`=~`, `!~`) compiled with
    `regex::RegexBuilder::new(...).size_limit(64 * 1024)` to prevent ReDoS.
  - `query_range` enforces `step >= 60` seconds and
    `((end - start) / step) <= 1,500` points, returning `400 Bad Request` with a
    Prometheus `{"status": "error", "errorType": "bad_data", ...}` body when
    exceeded.

## 5. Access Control, `metrics_token`, & Embargo Model

1. **`BugAccess` Enforcement on Subsystem Bug Metrics:**
   - Global bug totals (`sashiko_bugs_total`, `sashiko_bugs_security_total`,
     `sashiko_bugs_events_total`) expose aggregate counts only (never bug IDs,
     titles, files, or SHAs) and are publicly readable.
   - `sashiko_bugs_by_subsystem` enforces `Principal`:
     - `BugVisibility::Unrestricted` (operators in `acl.admins`, `acl.security`,
       or global maintainers): reads top subsystems from
       `stats_bug_subsystem_facets WHERE is_authorizing = 1` (or all tracked
       subsystems) in O(1).
     - `BugVisibility::Sections(scope)`: executes an O(|scope|) point query
       `WHERE is_authorizing = 1 AND subsystem COLLATE NOCASE IN (...)`,
       returning exact counts strictly for sections the maintainer is authorized
       to read and never acting as an oracle for other subsystems.
     - Unauthenticated callers or callers with an empty `scope` (including
       `LocalToken`, which intentionally never grants `BugAccess` per
       `src/settings.rs`): receive an empty vector `[]`.
2. **Optional `server.metrics_token`:**
   - Added to `ServerSettings` (`#[serde(default)] pub metrics_token: Option<String>`),
     redacted in custom `Debug` formatting and `utils::redact_secret`, and
     documented across `Settings.toml`, `docs/examples/Settings.example.toml`,
     and `docs/configuration.md`.
   - Compared strictly via `subtle::ConstantTimeEq` on raw bytes.
   - Accepted by `reject_invalid_authorization` **only** when
     `request.uri().path() == "/metrics"`, so a scrape token can never be used
     on `/api/*` endpoints.
3. **Patchset `embargo_until` Filtering:**
   - Public Pipeline (`sashiko_pipeline_*`) and Maintainer finding queries join
     `review_pipeline_stats.patchset_id -> patchsets.id` (on the `<= 1,000`
     bounded rows) and exclude reviews whose parent patchset has
     `embargo_until > now`. Rollup finding counters for embargoed patchsets are
     recorded when the embargo releases (`embargo_until <= now`), preventing
     1-patchset finding-severity timing leaks during an active embargo.

## 6. Metric Catalog & 4-Tab Frontend Design (`static/index.html`)

### 6.1 Routing & Shared UI Controls

- `#/stats` defaults to `#/stats/ops`, with deep-linkable sub-routes:
  `#/stats/ops`, `#/stats/bugs`, `#/stats/maintainers`, `#/stats/pipeline`.
- Tab bar uses Sashiko's existing clean card/button styling, dark/light/colorblind
  theme awareness (`applySettings()`), and `state.activeCharts` lifecycle
  management with Apache ECharts `5.4.3`.
- A lightweight JS helper `promQuery(expr, time)` and
  `promQueryRange(expr, start, end, step)` attaches `Authorization: Bearer ...`
  when signed in (`getAuthToken()`) so `Principal`-scoped bug subsystem charts
  populate automatically for logged-in maintainers.

### 6.2 Tab 1: Operational / SRE (`#/stats/ops`)

- **Controls:** Window toggle **`Last 1h`** (`5m` chart step) vs **`Last 24h`**
  (`1h` chart step).
- **Comparison Badges on Every KPI Card:**
  - Shows primary count/rate for selected window (`W` = `1h` or `24h`), plus:
    - `vs 7d avg` (`(current - avg_7d) / avg_7d` %) and `vs same window 7d ago`
      (`offset 7d`),
    - `vs 30d avg` (`(current - avg_30d) / avg_30d` %),
    - Semantic SRE coloring (e.g., higher errors/latency/stale trees = red/amber;
      stable throughput = neutral/green).
- **Metrics & Panels:**
  1. **Ingestion & Queue Health:**
     - `sashiko_messages_ingested_total{source}`, `sashiko_patchsets_ingested_total`,
       `sashiko_patches_ingested_total`, `sashiko_ingest_errors_total{source}`.
     - Live queue depth strip: `sashiko_queue_depth{queue}` (`pending_patches`,
       `reviewing_patches`, `fetch_queue`, `pending_bugs`, `running_bugs`) and
       `sashiko_nntp_lag_articles`.
  2. **Git Trees & Repository Health:**
     - **Stale Trees Card:** `sashiko_git_remotes{state="stale"}` &
       `{state="failed"}` vs `{state="fresh"}` & `{state="backed_off"}`, plus
       `sashiko_git_remote_max_age_seconds`.
     - Packfile footprint: `sashiko_repo_packs`, `sashiko_repo_pack_bytes`, and
       `sashiko_git_op_duration_seconds` (`fetch`, `repack`, `baseline`).
  3. **Errors, Warnings & Failures:**
     - `sashiko_log_events_total{level="error"|"warn", component}` stacked
       time-series chart + top components breakdown.
     - Review failures (`Failed`, `Failed To Apply`) & worker supervision
       restarts/panics (`sashiko_worker_restarts_total{worker}`).
  4. **Latency Distributions (P50 / P90 / P99):**
     - LLM API turn latency (`histogram_quantile(0.50|0.90|0.99, ...)` on
       `sashiko_ai_request_duration_seconds_bucket`).
     - Per-patch review execution duration (`sashiko_review_duration_seconds_bucket`).
     - End-to-end patchset turnaround (`sashiko_patchset_turnaround_seconds_bucket`).
  5. **LLM Tokens, Caching & Quota:**
     - Uncached input, cached input, and output tokens (`1h`/`24h` vs `7d`/`30d`
       averages) split by `workload="review"|"bug"`.
     - Provider prompt-cache % + local `CachingAiProvider` hit rate
       (`sashiko_ai_cache_requests_total{outcome}`).
     - 429 rate-limit hits (`sashiko_ai_errors_total{class="rate_limit"}`),
       transient retries, active `QuotaManager` cooldown, and in-flight LLM
       permits.
  6. **Outbound Delivery:**
     - Emails sent / failed / dry-run / embargoed (`sashiko_outbox_events_total{channel="email", outcome}`),
       Forge PR comments, and Patchwork checks, plus pending outbox gauges and
       `sashiko_email_worker_heartbeat_age_seconds`.

### 6.3 Tab 2: Bugs Database (`#/stats/bugs`)

- **KPI Row (Public Non-Identifying Totals):**
  - `Open`, `Fixed`, `Duplicates`, `Dismissed`, `New (Untriaged)`, `Closed`,
    `Upstream Fix Rate %`, `With Reproducer (r)`, `With Proposed Fix (f)`, and
    `Analysis Queue (Pending / Running)`.
- **Panels:**
  1. **Security Classification & CVSS v4.0 Severity:**
     - Stacked chart of `sashiko_bugs_security_total{status, security_class, security_rating}`:
       `Vulnerability` split by CVSS v4.0 rating (`Critical` 9.0–10.0, `High`
       7.0–8.9, `Medium` 4.0–6.9, `Low` 0.1–3.9) alongside `Hardening`,
       `Not Security`, and `Pending Evaluation`.
  2. **Engineering Severity Breakdown:**
     - `Critical`, `High`, `Medium`, `Low` stacked by `Open` vs `Fixed` vs
       `Dismissed` (`sashiko_bugs_total{status, severity}`).
  3. **Weekly Discovery, Fix & Duplicate Velocity:**
     - Time-series of `sashiko_bugs_events_total{event="discovered"|"fixed"|"duplicate"|"dismissed"}`
       per week + median time-to-fix (`sashiko_bug_time_to_fix_seconds`).
  4. **Bugs by Subsystem (`BugAccess`-Scoped):**
     - Horizontal stacked bar + clickable table linking to `#/?mode=bugs&subsystem=...`
       for top subsystems (`Open`, `Fixed`, `Vulnerabilities`, `Critical/High`).
     - Shows an inline sign-in prompt if the viewer is unauthenticated.

### 6.4 Tab 3: Project Leaders & Maintainers (`#/stats/maintainers`)

- **Controls:** Range selector (`12w`, `26w`, `52w`) + optional public mailing-list
  subsystem filter (`subsystems` table).
- **KPI Row:**
  - `Patchsets / Week`, `Patches / Week`, `Findings / Reviewed Patchset`,
    `Clean Patchsets %`, `Author Fixup Rate %` (on `v2+`), `Median Review Turnaround`.
- **Panels:**
  1. **Weekly Patchset & Patch Volume (with Revision Mix):**
     - Weekly bars/lines of submitted & reviewed patchsets and patches, plus
       `v1` (initial postings) vs `v2` vs `v3+` (revisions) mix.
  2. **Discovered Issues per Patch & per Patchset by Severity:**
     - Weekly ratio chart of average findings per reviewed patchset and per
       reviewed patch stacked by `Critical`, `High`, `Medium`, `Low`, overlaid
       with `Clean Patchset %` (0 findings).
  3. **Author Fixups & Prior-Feedback Outcomes Across Revisions (`v1 -> vN`):**
     - Weekly count and % of prior review items resolved in `vN`
       (`sashiko_prior_feedback_total{origin_kind, outcome}`):
       - **Fixups (`addressed`):** Defects fixed by the author in `vN` (split by
         prior Sashiko findings vs human reviewer threads).
       - **Still Unaddressed (`confirmed_finding`):** Carried over unaddressed.
       - **Settled by Authority / Disproven (`settled`, `disproven`).**
     - **Repost Efficiency:** Unchanged patches skipped via clean-review reuse
       vs modified vs newly added patches across `v2+` series.
  4. **Baseline Apply Hygiene & Turnaround SLA:**
     - Weekly `Failed To Apply %` (highlighting baseline friction for
       maintainers) and `p50` / `p90` contributor turnaround time.

### 6.5 Tab 4: Sashiko Pipeline (`#/stats/pipeline`)

- **Controls:** Window selector **`Last 100`**, **`Last 500`**, **`Last 1,000`**
  completed AI reviews (`window="100"|"500"|"1000"`).
- **KPI Row:**
  - `Raw Stage-1 Concerns / Review`, `Overall % Dismissed`, `Confirmed Findings / Review`,
    `Pre-Screen Stage Skip %`, `Avg Tokens / Review`, `Avg Review Duration`.
- **Panels:**
  1. **Pipeline Funnel & Dismissal Breakdown (`sashiko_pipeline_funnel_items`):**
     - Visual funnel from:
       `Raw Concerns Raised` (+ `Stage-1 Self-Dismissed` negative-data count)
       -> `Deduplicated Clusters` (cross-stage merge factor)
       -> `Verification: Confirmed (1a) | Inconclusive | Dismissed (1b)`
       -> `Post-Verification: Confirmed | Dismissed`
       -> `Final: Patch Regressions vs Pre-Existing Bug Candidates`.
  2. **Bugs Discovered by Stage — Total vs Exclusive & Dismissal %:**
     - Per-stage chart (`goal`, `implementation`, `locking`, `memory`,
       `security`, `performance`, `error-handling`, `api-contracts`,
       `prior-feedback`, etc.) comparing:
       - **Exclusive Findings** (caught *only* by this stage),
       - **Shared Findings** (co-discovered with >= 1 other stage),
       - **Dismissed Concerns** (raised by this stage but rejected in
         verification — stage false-alarm / dismissal %),
       - **Stage Precision %** (`confirmed / raw_concerns`).
  3. **Stage Overlap Matrix (2D Heatmap):**
     - `stage_a x stage_b` co-occurrence matrix over confirmed findings
       (`sashiko_pipeline_stage_overlap{stage_a, stage_b, window}`), showing
       diagonal exclusive hits and off-diagonal redundancy/reinforcement.
  4. **Per-Stage Token Cost, Turns & ROI:**
     - Avg uncached input, cached input, and output tokens + tool turns per
       stage, alongside **Tokens per Confirmed Finding** and **Tokens per
       Exclusive Finding** to guide prompt/workflow tuning.
  5. **Toolbox Call Frequency & Payload Size:**
     - Tool calls per review and average output bytes (`read_file`, `git_grep`,
       `git_blame`, `git_show`, `git_log`, etc.).
  6. **Prior-Feedback Fresh Recall Alignment:**
     - On `v2+` patches with unaddressed human reviewer threads (`HM*`), % also
       independently detected by fresh Sashiko stages (`also_found_by_sashiko`).

## 7. Step-by-Step Implementation Plan

Each step lands as a self-sufficient, tested commit passing `make check-pr` and
local `sashiko review --project sashiko --agent`:

1. **Commit 1 — Design Document:**
   - Add `designs/DESIGN_STATISTICS_REWORK.md`.
2. **Commit 2 — Core In-Memory Telemetry Registry & `tracing` Layer:**
   - Expand `src/metrics.rs` with lock-free counters, gauges, fixed-bucket
     `AtomicHistogram`s, Prometheus text exposition serializer, and bounded
     `Component` `MetricsLayer` wired in `src/logging.rs` & `src/main.rs`
     (compatible across `--all-features`, `--no-default-features`, and
     `--features cache`).
3. **Commit 3 — Schema Migration `023` & Trigger-Maintained State Gauges:**
   - Add `src/migrations/024_stats_rollups.sql` (`stats_state_gauges`,
     `stats_bug_subsystem_facets`, `stats_series`, `stats_rollups`,
     `review_pipeline_stats`), SQLite triggers, `Database::migrate` `v24`
     upgrade logic, and migration + invariant unit tests.
4. **Commit 4 — Rollup Accumulator, Pipeline Summary Extractor & Async Backfill:**
   - Extract `review_pipeline_stats` on review completion, hook worker telemetry
     (`BackoffProvider`, `CachingAiProvider`, `GitSyncWorker` remote freshness,
     `RepackWorker`, `Ingestor`/NNTP lag, outbox workers, worker supervision),
     add 60s `MetricsWorker` rollup flush + reaper, and implement the bounded
     resumable `stats_backfill` worker.
5. **Commit 5 — PromQL Subset Engine & Prometheus HTTP API (`/metrics`, `/api/v1/*`):**
   - Implement `src/promql.rs`, mount `/metrics` and `/api/v1/{query,query_range,labels,label/{name}/values,metadata}`,
     add `server.metrics_token` (`Settings.toml`, `Settings.example.toml`,
     `docs/configuration.md`), enforce `Principal` `BugAccess` and patchset
     `embargo_until` rules, re-point `/api/stats*` to O(1) tables, and add
     `EXPLAIN QUERY PLAN` tests proving zero full-table scans.
6. **Commit 6 — 4-Tab Statistics Frontend Rework (`static/index.html`) & Docs:**
   - Replace `renderStatsView()` in `static/index.html` with the 4-tab
     Prometheus-backed dashboard (`#/stats/ops`, `#/stats/bugs`,
     `#/stats/maintainers`, `#/stats/pipeline`) and update user/operator
     documentation (`docs/daemon.md`, `docs/configuration.md`).
