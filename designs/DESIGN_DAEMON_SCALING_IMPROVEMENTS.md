# Design: Daemon Scaling, Ingestion, Database, and GCE Storage Improvements

## Overview

Production telemetry from the `sashiko.dev` GKE deployment (`sashiko-cluster`
in `us-central1-b`) over a 14-day window shows that the daemon is **not
CPU-bound**:

- The main Linux review node (`n2d-standard-32`, 32 vCPUs, 128 GiB RAM,
  2x 375 GB Local SSDs) averages **11.4% CPU** (~3.6 cores), with p90 at
  **24.7%**, p99 at **43.9%**, and cgroup CPU throttling at **0.047%**
  (`3.27s` across 15 hours).
- Review workers spend >85% of wall-clock time awaiting remote LLM responses.

Instead, end-to-end turnaround latency (~50 minutes average from patch email
timestamp to completed review) and burst throughput are constrained by five
concrete bottlenecks across the daemon and its Kubernetes manifests:

1. **Serial single-socket NNTP polling and reconnect cascades** (`src/nntp.rs`,
   `src/ingestor.rs`): `Ingestor::process_nntp_cycle` polls all 112 tracked
   mailing lists serially over one `NntpClient` with a 15-second timeout. When
   a command times out, `NntpClient` drops `self.stream` (`None`) and attempts
   a single reconnect; if that reconnect also times out, `client` remains
   disconnected (`stream: None`) and every subsequent group immediately fails
   with `NNTP connection closed after prior I/O error or timeout` before
   attempting another timed-out reconnect, stretching a 60-second poll cycle to
   over 21 minutes.
2. **Single-connection SQLite reads and conservative WAL defaults**
   (`src/db.rs`): All dashboard HTTP queries, 24+ concurrent review workers,
   and background sweeps share one `libsql::Connection` (`conn`) and one
   mutex-guarded write connection (`tx_conn`) with SQLite's default
   `synchronous = FULL` and 2 MiB `cache_size` against a 56.6 GB database
   (24.1M `tool_usages` rows).
3. **Unscoped `git log -S`/`-G` pickaxe history walks and missing commit-graph
   Bloom filters** (`src/toolbox/git_log.rs`, `src/git_ops.rs`): When the model
   invokes `git_log` with `pickaxe` (`-S`/`-G`), no `paths`, and a single
   revision such as `HEAD`, git diffs every commit back to 2005 (1.45M commits)
   whenever fewer than `limit` commits match, pegging 100% of a vCPU and ~2.7
   GB RSS per subprocess until the 120-second tool timeout. Meanwhile,
   `write_commit_graph` omits `--changed-paths` and is not refreshed after
   `repack_repository`.
4. **Unreaped zombie subprocesses in containers** (`Dockerfile`,
   `deployment/*/base/app/*-k8s.yaml`): `docker-entrypoint.sh` `exec`s
   `sashiko` directly as PID 1 (and Kubernetes manifests override `command:`
   to `/bin/bash /usr/local/bin/docker-entrypoint.sh`), leaving reparented
   grandchild `git` processes unreaped (170+ `<defunct>` processes within 15
   hours).
5. **Persistent Disk saturation and same-volume backup staging**
   (`deployment/sashiko.dev/`): `sashiko-data-pvc` uses `standard-rwo`
   (`pd-balanced`, 500 GiB), hitting GCE's 140 MB/s and 3,000 IOPS caps (peaks
   of 135 MB/s read, 173.5 MB/s write, and 2,779 read IOPS). Nightly
   `VACUUM INTO` stages a 57 GB snapshot onto the same `/data` volume while
   leaving the node's second 375 GB Local SSD (`/mnt/disks/ssd1`) unused and
   allowing unbounded `backup-worker` page cache growth (up to 51 GiB on a
   128 GiB node).

---

## Detailed Design

### 1. NNTP Reconnect Recovery and Bounded Concurrent Group Polling

#### 1a. Lazy Reconnect and Broken-Stream Recovery (`src/nntp.rs`, `src/ingestor.rs`)
- Expose `pub fn is_connected(&self) -> bool` on `NntpClient` returning
  `self.stream.is_some()`.
- Before issuing `GROUP` on a mailing list, check `client.is_connected()`. If
  the previous group or article dropped the stream and its inline reconnect
  failed, establish a fresh connection before calling `client.group()` rather
  than failing immediately with `NNTP connection closed after prior I/O error
  or timeout`.
- Track consecutive connection failures inside a worker pass: if `connect`
  fails `MAX_CONSECUTIVE_CONNECT_FAILURES` (3) times in a row, abort the
  remaining groups on that worker for the current cycle so an unreachable NNTP
  server fails fast instead of stalling `N * 15s`.
- Ignore `QUIT` errors at the end of a cycle when `!client.is_connected()`, so
  a transient disconnect on the last group does not discard an otherwise
  successful cycle.

#### 1b. Concurrent NNTP Group Workers (`src/settings.rs`, `src/ingestor.rs`)
- Add `concurrency: usize` to `NntpSettings` (`#[serde(default =
  "default_nntp_concurrency")]`, default `4`), documented in `Settings.toml`,
  `docs/examples/Settings.example.toml`, and `docs/configuration.md`, and
  validated (`>= 1`) in `Settings::validate_for_daemon` / `NntpSettings`.
- **Per-group `IngestTracker` isolation:** `IngestTracker` tracks article
  sequence numbers (`u64`), which are local to a single NNTP group. Sharing one
  `IngestTracker` across concurrent groups would mix `lowest_lost` article
  numbers from unrelated groups and block `wait_until_quiet` on peers. Each
  group pass therefore instantiates its own local `IngestTracker::new()`,
  preserving exact per-group high-water mark safety while `worker_count =
  settings.nntp.concurrency.min(groups.len())` workers pull `(name, group)`
  entries from a shared `Mutex<VecDeque<(String, String)>>`.

---

### 2. SQLite WAL PRAGMAs and File-Backed Read Connection Pool (`src/db.rs`)

- Configure every file-backed SQLite connection (`conn`, pooled read handles,
  and `tx_conn`) with:
  - `PRAGMA busy_timeout = 5000;`
  - `PRAGMA foreign_keys = ON;`
  - `PRAGMA synchronous = NORMAL;` (crash-safe in WAL mode; avoids redundant
    per-commit WAL `fsync` calls while checkpoints still sync before advancing)
  - `PRAGMA temp_store = MEMORY;`
  - `PRAGMA cache_size = -32768;` (32 MiB lazy page cache per read handle) and
    `PRAGMA cache_size = -65536;` (64 MiB lazy page cache on `tx_conn`). Pages
    are allocated on demand, so small test databases incur negligible memory.
- Add an 8-connection round-robin read pool (`READ_POOL_SIZE = 8`) on
  file-backed databases (`!settings.url.contains(":memory:")`), with extra read
  connections marked `PRAGMA query_only = ON;`.
- Add `pub fn read_conn(&self) -> &libsql::Connection`:
  - Returns `&self.conn` when `self.in_transaction` is true (so reads inside
    `DatabaseTransaction` continue to observe uncommitted writes on `tx_conn`)
    or when `read_pool` is empty (`:memory:` databases).
  - Otherwise selects `&self.read_pool[idx % len]` via an atomic counter so
    concurrent HTTP queries, review workers, and bug sweeps execute in parallel
    across SQLite WAL readers without a single cursor pinning or serializing
    all other readers.
  - Keep `pub conn: libsql::Connection` intact for external callers and tests,
    and keep claim/status/release read-after-write queries on `tx_conn` under
    `tx_lock` (preserving commit `bf31476d2148`).

---

### 3. Commit-Graph Changed-Path Bloom Filters and Bounded Pickaxe (`src/git_ops.rs`, `src/toolbox/git_log.rs`)

- **Changed-path Bloom filters (`src/git_ops.rs`):**
  - Update `run_commit_graph_write` to pass `--changed-paths` and
    `--max-new-filters=50000` (`git commit-graph write --reachable
    --changed-paths --max-new-filters=50000`). Bounding new filters per pass
    keeps startup and maintenance bounded to a few seconds while progressively
    populating changed-path Bloom filters across history.
  - Refresh the commit-graph inside `repack_repository` (while already holding
    `get_object_store_lock()`) right after `git repack --geometric=2 -d
    --write-midx` succeeds, keeping the commit-graph and Bloom filters up to
    date every 6 hours and preventing stale graph entries after repacks.
- **Bounding unscoped pickaxe walks (`src/toolbox/git_log.rs`):**
  - When `git_log` is called with `pickaxe` (`-S` or `-G`), no `paths`, no
    `line_range`, and a revision specifier containing no explicit range
    operator (`!range.contains("..")`), probe whether
    `<range>~10000^{commit}` exists via `git rev-parse --verify --quiet
    --end-of-options <range>~10000^{commit}`.
  - On small repositories (< 10,000 commits), `rev-parse` exits non-zero in
    < 1 ms and `git_log` searches full history unchanged.
  - On deep repositories like Linux or GCC, `git_log` scopes the traversal to
    `<range>~10000..<range>` (~145x fewer commits than full kernel history) and
    appends a concise note when fewer than `limit` commits are returned,
    guiding the model to pass `paths` (accelerated by Bloom filters) or an
    explicit `A..B` range if older history is needed.

---

### 4. Container Init Process Reaper (`Dockerfile`, `deployment/`)

- Install `tini` in the runtime image (`Dockerfile`) and set
  `ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/docker-entrypoint.sh"]`.
- Remove the redundant `command: ["/bin/bash", "/usr/local/bin/docker-entrypoint.sh"]`
  overrides from `sashiko-k8s.yaml`, `sashiko-self-k8s.yaml`, and
  `sashiko-gcc-k8s.yaml` so Kubernetes invokes the image `ENTRYPOINT` (`tini`)
  as PID 1 and forwards `args:` to `docker-entrypoint.sh`.

---

### 5. GCE Storage, Nightly Backup Staging, and Review Concurrency (`deployment/sashiko.dev/`)

- **`sashiko-k8s.yaml`:**
  - Set `storageClassName: premium-rwo` (`pd-ssd`, 15,000 IOPS / 240 MB/s at
    500 GiB) on `sashiko-data-pvc`, with operator migration notes since PVC
    `storageClassName` is immutable on existing claims.
  - Increase `SASHIKO__REVIEW__CONCURRENCY` from `"24"` to `"36"` (108 LLM
    permits), utilizing idle node CPU capacity during mailing-list bursts.
- **`management-tools.yaml`:**
  - Mount the node's unused second Local SSD (`/mnt/disks/ssd1`, `hostPath`
    `Directory`) at `/mnt/local-ssd` in the `sashiko-nightly-backup` CronJob
    and stage `VACUUM INTO` at `/mnt/local-ssd/sashiko_backup.db` (falling back
    to `/data` only if `/mnt/local-ssd` is unavailable), eliminating 57 GB of
    concurrent read+write contention and `ENOSPC` risk on `/data`.
  - Register `trap 'rm -f "$TEMP_BACKUP"' EXIT` in `backup-db.sh` so failed
    uploads never leave a 57 GB temporary snapshot behind.
  - Set `resources` (`requests: 500m CPU / 2Gi RAM`, `limits: 4 CPU / 8Gi RAM`)
    on `backup-worker` so cgroup v2 bounds sequential backup file-cache memory
    at 8 GiB instead of evicting up to 51 GiB from the main node.

---

## Implementation Plan

1. Commit design document (`designs/DESIGN_DAEMON_SCALING_IMPROVEMENTS.md`).
2. Fix disconnected `NntpClient` reconnect cascades in `src/nntp.rs` and
   `src/ingestor.rs` with unit tests.
3. Add `[nntp] concurrency` setting and concurrent per-group `IngestTracker`
   worker pool in `src/settings.rs`, `src/ingestor.rs`, and configuration docs.
4. Tune SQLite WAL PRAGMAs and add the file-backed read connection pool in
   `src/db.rs` and `src/db/*.rs` with concurrency and transaction-isolation
   tests.
5. Enable `--changed-paths --max-new-filters=50000` commit-graph writes and
   post-repack commit-graph refresh in `src/git_ops.rs`.
6. Bound unscoped `git_log` pickaxe walks on deep histories in
   `src/toolbox/git_log.rs` with unit tests.
7. Add `tini` as PID 1 in `Dockerfile` and remove `command:` overrides in
   `deployment/*/base/app/*-k8s.yaml`.
8. Update `deployment/sashiko.dev/` manifests (`premium-rwo`, concurrency `36`,
   Local SSD backup staging, `trap` cleanup, `backup-worker` memory limit).
9. Run `make check-pr` and `sashiko review --project sashiko --agent` across
   the commit series and resolve any findings.
