// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Lock-free runtime telemetry registry, fixed-bucket histograms, two-phase
//! rollup delta snapshotting, and Prometheus text `0.0.4` exposition helpers.

use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

// ============================================================================
// Metric Metadata Catalog
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricType {
    Counter,
    Gauge,
    Histogram,
}

impl MetricType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct MetricDef {
    pub name: &'static str,
    #[serde(rename = "type")]
    pub metric_type: MetricType,
    pub help: &'static str,
    pub unit: &'static str,
}

pub static METRIC_CATALOG: &[MetricDef] = &[
    MetricDef {
        name: "sashiko_build_info",
        metric_type: MetricType::Gauge,
        help: "Sashiko build version metadata (constant 1).",
        unit: "",
    },
    MetricDef {
        name: "sashiko_messages_stored",
        metric_type: MetricType::Gauge,
        help: "Total messages stored in the database.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_patchsets_stored",
        metric_type: MetricType::Gauge,
        help: "Total patchsets stored in the database.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_queue_depth",
        metric_type: MetricType::Gauge,
        help: "Current item count in Sashiko processing queues.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_repo_packs",
        metric_type: MetricType::Gauge,
        help: "Number of git packfiles in the review repository.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_repo_pack_bytes",
        metric_type: MetricType::Gauge,
        help: "Total size in bytes of git packfiles in the review repository.",
        unit: "bytes",
    },
    MetricDef {
        name: "sashiko_git_remotes",
        metric_type: MetricType::Gauge,
        help: "Tracked git remotes grouped by freshness state.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_git_remote_max_age_seconds",
        metric_type: MetricType::Gauge,
        help: "Maximum time in seconds since the oldest tracked git remote synced cleanly.",
        unit: "seconds",
    },
    MetricDef {
        name: "sashiko_nntp_lag_articles",
        metric_type: MetricType::Gauge,
        help: "Unread NNTP articles behind upstream group high-water marks.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_ai_inflight_requests",
        metric_type: MetricType::Gauge,
        help: "Currently executing uncached LLM provider HTTP requests.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_ai_quota_cooldown_seconds",
        metric_type: MetricType::Gauge,
        help: "Remaining provider rate-limit cooldown duration in seconds.",
        unit: "seconds",
    },
    MetricDef {
        name: "sashiko_email_worker_heartbeat_age_seconds",
        metric_type: MetricType::Gauge,
        help: "Seconds elapsed since the email outbox worker last completed a poll.",
        unit: "seconds",
    },
    MetricDef {
        name: "sashiko_messages_ingested_total",
        metric_type: MetricType::Counter,
        help: "Total messages ingested by transport source.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_patchsets_ingested_total",
        metric_type: MetricType::Counter,
        help: "Total patchsets created during ingestion.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_patches_ingested_total",
        metric_type: MetricType::Counter,
        help: "Total individual patches created during ingestion.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_ingest_errors_total",
        metric_type: MetricType::Counter,
        help: "Total message/patch ingestion errors by transport source.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_log_events_total",
        metric_type: MetricType::Counter,
        help: "Total tracing warning and error log events by component.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_worker_restarts_total",
        metric_type: MetricType::Counter,
        help: "Total unexpected supervised background worker exits and panics.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_git_sync_runs_total",
        metric_type: MetricType::Counter,
        help: "Total git remote refresh attempts grouped by outcome.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_reviews_completed_total",
        metric_type: MetricType::Counter,
        help: "Total completed patch review attempts grouped by terminal outcome.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_reviews_skipped_unchanged_total",
        metric_type: MetricType::Counter,
        help: "Total reviews skipped because an identical patch diff already reviewed clean.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_ai_tokens_total",
        metric_type: MetricType::Counter,
        help: "Total LLM tokens consumed by direction and workload.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_ai_errors_total",
        metric_type: MetricType::Counter,
        help: "Total LLM provider errors and validation retries by error class.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_ai_cache_requests_total",
        metric_type: MetricType::Counter,
        help: "Total local AI response-cache lookups by hit or miss outcome.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_outbox_events_total",
        metric_type: MetricType::Counter,
        help: "Total outbound notification delivery outcomes by channel.",
        unit: "",
    },
    MetricDef {
        name: "sashiko_ai_request_duration_seconds",
        metric_type: MetricType::Histogram,
        help: "Latency distribution of uncached LLM provider calls in seconds.",
        unit: "seconds",
    },
    MetricDef {
        name: "sashiko_review_duration_seconds",
        metric_type: MetricType::Histogram,
        help: "Wall-clock execution duration of individual patch reviews in seconds.",
        unit: "seconds",
    },
    MetricDef {
        name: "sashiko_patchset_turnaround_seconds",
        metric_type: MetricType::Histogram,
        help: "End-to-end turnaround from patchset arrival to completed review in seconds.",
        unit: "seconds",
    },
    MetricDef {
        name: "sashiko_git_op_duration_seconds",
        metric_type: MetricType::Histogram,
        help: "Duration of git fetch, repack, and baseline preparation operations in seconds.",
        unit: "seconds",
    },
];

pub fn catalog() -> &'static [MetricDef] {
    METRIC_CATALOG
}

// ============================================================================
// Bounded Label Domain Enums
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum LogLevelMetric {
    Warn = 0,
    Error = 1,
}

impl LogLevelMetric {
    pub const COUNT: usize = 2;
    pub const ALL: [Self; Self::COUNT] = [Self::Warn, Self::Error];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum LogComponent {
    Ingestor = 0,
    Nntp = 1,
    Fetcher = 2,
    GitSync = 3,
    Repack = 4,
    Reviewer = 5,
    BugWorker = 6,
    Email = 7,
    Forge = 8,
    Patchwork = 9,
    Server = 10,
    Ai = 11,
    Db = 12,
    Other = 13,
}

impl LogComponent {
    pub const COUNT: usize = 14;
    pub const ALL: [Self; Self::COUNT] = [
        Self::Ingestor,
        Self::Nntp,
        Self::Fetcher,
        Self::GitSync,
        Self::Repack,
        Self::Reviewer,
        Self::BugWorker,
        Self::Email,
        Self::Forge,
        Self::Patchwork,
        Self::Server,
        Self::Ai,
        Self::Db,
        Self::Other,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ingestor => "ingestor",
            Self::Nntp => "nntp",
            Self::Fetcher => "fetcher",
            Self::GitSync => "git_sync",
            Self::Repack => "repack",
            Self::Reviewer => "reviewer",
            Self::BugWorker => "bug_worker",
            Self::Email => "email",
            Self::Forge => "forge",
            Self::Patchwork => "patchwork",
            Self::Server => "server",
            Self::Ai => "ai",
            Self::Db => "db",
            Self::Other => "other",
        }
    }

    /// Maps a `tracing` target path to a bounded component label without heap
    /// allocation so log storms cannot inflate series cardinality.
    pub fn from_target(target: &str) -> Self {
        let rest = target.strip_prefix("sashiko::").unwrap_or(target);
        if rest.starts_with("worker::patchwork") || rest.starts_with("patchwork") {
            Self::Patchwork
        } else if rest.starts_with("ingestor")
            || rest.starts_with("mbox")
            || rest == "patch"
            || rest.starts_with("patch::")
        {
            Self::Ingestor
        } else if rest.starts_with("nntp") {
            Self::Nntp
        } else if rest.starts_with("fetcher") || rest.starts_with("prerequisites") {
            Self::Fetcher
        } else if rest.starts_with("worker::sync")
            || rest.starts_with("repo_sync")
            || rest.starts_with("git_ops")
            || rest.starts_with("git_cmd")
            || rest.starts_with("baseline")
        {
            Self::GitSync
        } else if rest.starts_with("worker::repack") {
            Self::Repack
        } else if rest.starts_with("reviewer")
            || rest.starts_with("workflow")
            || rest.starts_with("worker::prompts")
            || rest.starts_with("worker::prefetch")
            || rest.starts_with("prior_context")
        {
            Self::Reviewer
        } else if rest.starts_with("worker::bug_worker") || rest.starts_with("security_eval") {
            Self::BugWorker
        } else if rest.starts_with("worker::email")
            || rest.starts_with("email_")
            || rest.starts_with("lettre")
        {
            Self::Email
        } else if rest.starts_with("worker::forge") || rest.starts_with("forge") {
            Self::Forge
        } else if rest.starts_with("server")
            || rest.starts_with("auth")
            || rest.starts_with("access")
            || rest.starts_with("axum")
            || rest.starts_with("hyper")
            || rest.starts_with("tower")
        {
            Self::Server
        } else if rest.starts_with("ai") || rest.starts_with("reqwest") {
            Self::Ai
        } else if rest.starts_with("db") || rest.starts_with("libsql") {
            Self::Db
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum WorkerKind {
    FetchAgent = 0,
    Ingestor = 1,
    EmailWorker = 2,
    PatchworkWorker = 3,
    ForgeWorker = 4,
    ForgePoller = 5,
    BugWorker = 6,
    GitSyncWorker = 7,
    RepackWorker = 8,
    Reviewer = 9,
    MetricsWorker = 10,
    StatsBackfill = 11,
    Other = 12,
}

impl WorkerKind {
    pub const COUNT: usize = 13;
    pub const ALL: [Self; Self::COUNT] = [
        Self::FetchAgent,
        Self::Ingestor,
        Self::EmailWorker,
        Self::PatchworkWorker,
        Self::ForgeWorker,
        Self::ForgePoller,
        Self::BugWorker,
        Self::GitSyncWorker,
        Self::RepackWorker,
        Self::Reviewer,
        Self::MetricsWorker,
        Self::StatsBackfill,
        Self::Other,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FetchAgent => "FetchAgent",
            Self::Ingestor => "Ingestor",
            Self::EmailWorker => "EmailWorker",
            Self::PatchworkWorker => "PatchworkWorker",
            Self::ForgeWorker => "ForgeWorker",
            Self::ForgePoller => "ForgePoller",
            Self::BugWorker => "BugWorker",
            Self::GitSyncWorker => "GitSyncWorker",
            Self::RepackWorker => "RepackWorker",
            Self::Reviewer => "Reviewer",
            Self::MetricsWorker => "MetricsWorker",
            Self::StatsBackfill => "StatsBackfill",
            Self::Other => "Other",
        }
    }

    pub fn from_name(name: &str) -> Self {
        match name {
            "FetchAgent" => Self::FetchAgent,
            "Ingestor" => Self::Ingestor,
            "EmailWorker" => Self::EmailWorker,
            "PatchworkWorker" => Self::PatchworkWorker,
            "ForgeWorker" => Self::ForgeWorker,
            "ForgePoller" => Self::ForgePoller,
            "BugWorker" => Self::BugWorker,
            "GitSyncWorker" => Self::GitSyncWorker,
            "RepackWorker" => Self::RepackWorker,
            "Reviewer" => Self::Reviewer,
            "MetricsWorker" => Self::MetricsWorker,
            "StatsBackfill" => Self::StatsBackfill,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum IngestSource {
    Nntp = 0,
    Api = 1,
    Git = 2,
    Forge = 3,
}

impl IngestSource {
    pub const COUNT: usize = 4;
    pub const ALL: [Self; Self::COUNT] = [Self::Nntp, Self::Api, Self::Git, Self::Forge];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Nntp => "nntp",
            Self::Api => "api",
            Self::Git => "git",
            Self::Forge => "forge",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum GitRemoteState {
    Fresh = 0,
    Stale = 1,
    BackedOff = 2,
    Failed = 3,
}

impl GitRemoteState {
    pub const COUNT: usize = 4;
    pub const ALL: [Self; Self::COUNT] = [Self::Fresh, Self::Stale, Self::BackedOff, Self::Failed];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::BackedOff => "backed_off",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum GitSyncOutcome {
    Fetched = 0,
    Skipped = 1,
    BackedOff = 2,
    Stale = 3,
    Failed = 4,
}

impl GitSyncOutcome {
    pub const COUNT: usize = 5;
    pub const ALL: [Self; Self::COUNT] = [
        Self::Fetched,
        Self::Skipped,
        Self::BackedOff,
        Self::Stale,
        Self::Failed,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fetched => "fetched",
            Self::Skipped => "skipped",
            Self::BackedOff => "backed_off",
            Self::Stale => "stale",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum GitOpKind {
    Fetch = 0,
    Repack = 1,
    Baseline = 2,
}

impl GitOpKind {
    pub const COUNT: usize = 3;
    pub const ALL: [Self; Self::COUNT] = [Self::Fetch, Self::Repack, Self::Baseline];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fetch => "fetch",
            Self::Repack => "repack",
            Self::Baseline => "baseline",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum AiWorkload {
    Review = 0,
    Bug = 1,
}

impl AiWorkload {
    pub const COUNT: usize = 2;
    pub const ALL: [Self; Self::COUNT] = [Self::Review, Self::Bug];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Review => "review",
            Self::Bug => "bug",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum AiTokenDir {
    UncachedIn = 0,
    CachedIn = 1,
    Out = 2,
}

impl AiTokenDir {
    pub const COUNT: usize = 3;
    pub const ALL: [Self; Self::COUNT] = [Self::UncachedIn, Self::CachedIn, Self::Out];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UncachedIn => "uncached_in",
            Self::CachedIn => "cached_in",
            Self::Out => "out",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum AiErrorKind {
    RateLimit = 0,
    Transient = 1,
    Fatal = 2,
    FormatViolation = 3,
}

impl AiErrorKind {
    pub const COUNT: usize = 4;
    pub const ALL: [Self; Self::COUNT] = [
        Self::RateLimit,
        Self::Transient,
        Self::Fatal,
        Self::FormatViolation,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::Transient => "transient",
            Self::Fatal => "fatal",
            Self::FormatViolation => "format_violation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum CacheOutcome {
    Hit = 0,
    Miss = 1,
}

impl CacheOutcome {
    pub const COUNT: usize = 2;
    pub const ALL: [Self; Self::COUNT] = [Self::Hit, Self::Miss];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum OutboxChannel {
    Email = 0,
    Forge = 1,
    Patchwork = 2,
}

impl OutboxChannel {
    pub const COUNT: usize = 3;
    pub const ALL: [Self; Self::COUNT] = [Self::Email, Self::Forge, Self::Patchwork];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Forge => "forge",
            Self::Patchwork => "patchwork",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum OutboxOutcome {
    Sent = 0,
    Failed = 1,
    DryRun = 2,
    Muted = 3,
    Skipped = 4,
    Embargoed = 5,
}

impl OutboxOutcome {
    pub const COUNT: usize = 6;
    pub const ALL: [Self; Self::COUNT] = [
        Self::Sent,
        Self::Failed,
        Self::DryRun,
        Self::Muted,
        Self::Skipped,
        Self::Embargoed,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::DryRun => "dry_run",
            Self::Muted => "muted",
            Self::Skipped => "skipped",
            Self::Embargoed => "embargoed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum ReviewOutcomeMetric {
    Reviewed = 0,
    Failed = 1,
    FailedToApply = 2,
    SkippedUnchanged = 3,
    SkippedOther = 4,
    Cancelled = 5,
}

impl ReviewOutcomeMetric {
    pub const COUNT: usize = 6;
    pub const ALL: [Self; Self::COUNT] = [
        Self::Reviewed,
        Self::Failed,
        Self::FailedToApply,
        Self::SkippedUnchanged,
        Self::SkippedOther,
        Self::Cancelled,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reviewed => "Reviewed",
            Self::Failed => "Failed",
            Self::FailedToApply => "Failed To Apply",
            Self::SkippedUnchanged => "Skipped (Unchanged)",
            Self::SkippedOther => "Skipped",
            Self::Cancelled => "Cancelled",
        }
    }
}

// ============================================================================
// Lock-Free Fixed-Bucket Atomic Histogram
// ============================================================================

/// Fixed-bucket latency histogram storing non-cumulative counts per slot (`N`
/// finite upper bounds in milliseconds plus one `+Inf` overflow slot `B = N+1`).
/// Cumulative `le` series are derived on read so hot-path observations touch
/// only a single bucket counter plus `count` and `sum_micros`.
pub struct AtomicHistogram<const N: usize, const B: usize> {
    bounds_ms: [u64; N],
    le_labels: [&'static str; B],
    buckets: [AtomicU64; B],
    sum_micros: AtomicU64,
    count: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistogramState<const B: usize> {
    /// Cumulative `le` counts across `0..B` (`cumulative[B - 1] == count`).
    pub cumulative: [u64; B],
    pub sum_micros: u64,
    pub count: u64,
}

impl<const B: usize> HistogramState<B> {
    pub const fn zero() -> Self {
        Self {
            cumulative: [0; B],
            sum_micros: 0,
            count: 0,
        }
    }
}

impl<const N: usize, const B: usize> AtomicHistogram<N, B> {
    pub const fn new(bounds_ms: [u64; N], le_labels: [&'static str; B]) -> Self {
        Self {
            bounds_ms,
            le_labels,
            buckets: [const { AtomicU64::new(0) }; B],
            sum_micros: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    pub fn observe(&self, duration: Duration) {
        let millis = duration.as_millis().min(u128::from(u64::MAX)) as u64;
        let micros = duration.as_micros().min(u128::from(u64::MAX)) as u64;
        let mut slot = N;
        let mut idx = 0;
        while idx < N {
            if millis <= self.bounds_ms[idx] {
                slot = idx;
                break;
            }
            idx += 1;
        }
        self.buckets[slot].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
    }

    pub const fn le_labels(&self) -> &[&'static str; B] {
        &self.le_labels
    }

    pub fn snapshot(&self) -> HistogramState<B> {
        let mut cumulative = [0u64; B];
        let mut running = 0u64;
        let mut idx = 0;
        while idx < B {
            running = running.saturating_add(self.buckets[idx].load(Ordering::Relaxed));
            cumulative[idx] = running;
            idx += 1;
        }
        let sum_micros = self.sum_micros.load(Ordering::Relaxed);
        // Derive total count from the final +Inf bucket so concurrent relaxed
        // reads always satisfy cumulative[B - 1] == count on exposition.
        HistogramState {
            cumulative,
            sum_micros,
            count: running,
        }
    }
}

const AI_HIST_BOUNDS_MS: [u64; 11] = [
    250, 500, 1_000, 2_500, 5_000, 10_000, 20_000, 45_000, 90_000, 180_000, 300_000,
];
const AI_HIST_LABELS: [&str; 12] = [
    "0.25", "0.5", "1", "2.5", "5", "10", "20", "45", "90", "180", "300", "+Inf",
];

const REVIEW_HIST_BOUNDS_MS: [u64; 9] = [
    5_000, 15_000, 30_000, 60_000, 120_000, 300_000, 600_000, 1_200_000, 1_800_000,
];
const REVIEW_HIST_LABELS: [&str; 10] = [
    "5", "15", "30", "60", "120", "300", "600", "1200", "1800", "+Inf",
];

const TURNAROUND_HIST_BOUNDS_MS: [u64; 8] = [
    30_000, 60_000, 300_000, 900_000, 1_800_000, 3_600_000, 14_400_000, 86_400_000,
];
const TURNAROUND_HIST_LABELS: [&str; 9] = [
    "30", "60", "300", "900", "1800", "3600", "14400", "86400", "+Inf",
];

const GIT_OP_HIST_BOUNDS_MS: [u64; 7] = [100, 500, 2_000, 10_000, 30_000, 120_000, 300_000];
const GIT_OP_HIST_LABELS: [&str; 8] = ["0.1", "0.5", "2", "10", "30", "120", "300", "+Inf"];

// ============================================================================
// Global Atomic Gauges, Counters, and Histograms
// ============================================================================

pub static PENDING_PATCHES: AtomicUsize = AtomicUsize::new(0);
pub static REVIEWING_PATCHES: AtomicUsize = AtomicUsize::new(0);
pub static MESSAGES: AtomicUsize = AtomicUsize::new(0);
pub static PATCHSETS: AtomicUsize = AtomicUsize::new(0);
pub static REPO_PACKS: AtomicUsize = AtomicUsize::new(0);
pub static REPO_PACK_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Reviews this process did not run because an identical diff had already
/// been reviewed with no findings. Counts since startup; the database holds
/// the durable record in reviews.inherited_from_review_id.
pub static REVIEWS_SKIPPED_UNCHANGED: AtomicUsize = AtomicUsize::new(0);

static GIT_REMOTES: [AtomicU64; GitRemoteState::COUNT] =
    [const { AtomicU64::new(0) }; GitRemoteState::COUNT];
static GIT_REMOTE_MAX_AGE_SECS: AtomicU64 = AtomicU64::new(0);
static NNTP_LAG_TOTAL: AtomicU64 = AtomicU64::new(0);
static NNTP_LAG_MAX: AtomicU64 = AtomicU64::new(0);
static AI_INFLIGHT_REQUESTS: AtomicU64 = AtomicU64::new(0);
static AI_QUOTA_COOLDOWN_UNTIL_UNIX: AtomicI64 = AtomicI64::new(0);
static EMAIL_WORKER_HEARTBEAT_AGE_SECS: AtomicI64 = AtomicI64::new(-1);

static LOG_EVENTS: [[AtomicU64; LogComponent::COUNT]; LogLevelMetric::COUNT] = [
    [const { AtomicU64::new(0) }; LogComponent::COUNT],
    [const { AtomicU64::new(0) }; LogComponent::COUNT],
];
static WORKER_EXITS: [AtomicU64; WorkerKind::COUNT] =
    [const { AtomicU64::new(0) }; WorkerKind::COUNT];
static WORKER_PANICS: [AtomicU64; WorkerKind::COUNT] =
    [const { AtomicU64::new(0) }; WorkerKind::COUNT];
static MESSAGES_INGESTED: [AtomicU64; IngestSource::COUNT] =
    [const { AtomicU64::new(0) }; IngestSource::COUNT];
static PATCHSETS_INGESTED: AtomicU64 = AtomicU64::new(0);
static PATCHES_INGESTED: AtomicU64 = AtomicU64::new(0);
static INGEST_ERRORS: [AtomicU64; IngestSource::COUNT] =
    [const { AtomicU64::new(0) }; IngestSource::COUNT];
static GIT_SYNC_RUNS: [AtomicU64; GitSyncOutcome::COUNT] =
    [const { AtomicU64::new(0) }; GitSyncOutcome::COUNT];
static REVIEWS_COMPLETED: [AtomicU64; ReviewOutcomeMetric::COUNT] =
    [const { AtomicU64::new(0) }; ReviewOutcomeMetric::COUNT];
static AI_TOKENS: [[AtomicU64; AiTokenDir::COUNT]; AiWorkload::COUNT] = [
    [const { AtomicU64::new(0) }; AiTokenDir::COUNT],
    [const { AtomicU64::new(0) }; AiTokenDir::COUNT],
];
static AI_ERRORS: [AtomicU64; AiErrorKind::COUNT] =
    [const { AtomicU64::new(0) }; AiErrorKind::COUNT];
static AI_CACHE_REQUESTS: [AtomicU64; CacheOutcome::COUNT] =
    [const { AtomicU64::new(0) }; CacheOutcome::COUNT];
static OUTBOX_EVENTS: [[AtomicU64; OutboxOutcome::COUNT]; OutboxChannel::COUNT] = [
    [const { AtomicU64::new(0) }; OutboxOutcome::COUNT],
    [const { AtomicU64::new(0) }; OutboxOutcome::COUNT],
    [const { AtomicU64::new(0) }; OutboxOutcome::COUNT],
];

pub static AI_REQUEST_DURATION: AtomicHistogram<11, 12> =
    AtomicHistogram::new(AI_HIST_BOUNDS_MS, AI_HIST_LABELS);
pub static REVIEW_DURATION: AtomicHistogram<9, 10> =
    AtomicHistogram::new(REVIEW_HIST_BOUNDS_MS, REVIEW_HIST_LABELS);
pub static PATCHSET_TURNAROUND_DURATION: AtomicHistogram<8, 9> =
    AtomicHistogram::new(TURNAROUND_HIST_BOUNDS_MS, TURNAROUND_HIST_LABELS);
pub static GIT_OP_DURATION: [AtomicHistogram<7, 8>; GitOpKind::COUNT] = [
    AtomicHistogram::new(GIT_OP_HIST_BOUNDS_MS, GIT_OP_HIST_LABELS),
    AtomicHistogram::new(GIT_OP_HIST_BOUNDS_MS, GIT_OP_HIST_LABELS),
    AtomicHistogram::new(GIT_OP_HIST_BOUNDS_MS, GIT_OP_HIST_LABELS),
];

// ============================================================================
// Producer & Consumer Accessors
// ============================================================================

pub fn inc_reviews_skipped_unchanged() {
    REVIEWS_SKIPPED_UNCHANGED.fetch_add(1, Ordering::Relaxed);
    REVIEWS_COMPLETED[ReviewOutcomeMetric::SkippedUnchanged as usize]
        .fetch_add(1, Ordering::Relaxed);
}

pub fn get_reviews_skipped_unchanged() -> usize {
    REVIEWS_SKIPPED_UNCHANGED.load(Ordering::Relaxed)
}

pub fn set_pending_patches(count: usize) {
    PENDING_PATCHES.store(count, Ordering::Relaxed);
}

pub fn set_reviewing_patches(count: usize) {
    REVIEWING_PATCHES.store(count, Ordering::Relaxed);
}

pub fn set_messages(count: usize) {
    MESSAGES.store(count, Ordering::Relaxed);
}

pub fn set_patchsets(count: usize) {
    PATCHSETS.store(count, Ordering::Relaxed);
}

/// Records the size of the review repository's pack directory. With git's own
/// maintenance turned off, nothing reclaims packs, so this is where that cost
/// shows up.
pub fn set_repo_packs(count: usize, bytes: u64) {
    REPO_PACKS.store(count, Ordering::Relaxed);
    REPO_PACK_BYTES.store(bytes as usize, Ordering::Relaxed);
}

pub fn get_pending_patches() -> usize {
    PENDING_PATCHES.load(Ordering::Relaxed)
}

pub fn get_reviewing_patches() -> usize {
    REVIEWING_PATCHES.load(Ordering::Relaxed)
}

pub fn get_messages() -> usize {
    MESSAGES.load(Ordering::Relaxed)
}

pub fn get_patchsets() -> usize {
    PATCHSETS.load(Ordering::Relaxed)
}

pub fn get_repo_packs() -> usize {
    REPO_PACKS.load(Ordering::Relaxed)
}

pub fn get_repo_pack_bytes() -> usize {
    REPO_PACK_BYTES.load(Ordering::Relaxed)
}

pub fn inc_log_event(level: LogLevelMetric, component: LogComponent) {
    LOG_EVENTS[level as usize][component as usize].fetch_add(1, Ordering::Relaxed);
}

pub fn get_log_events(level: LogLevelMetric, component: LogComponent) -> u64 {
    LOG_EVENTS[level as usize][component as usize].load(Ordering::Relaxed)
}

pub fn inc_worker_restart(worker: WorkerKind, panicked: bool) {
    if panicked {
        WORKER_PANICS[worker as usize].fetch_add(1, Ordering::Relaxed);
    } else {
        WORKER_EXITS[worker as usize].fetch_add(1, Ordering::Relaxed);
    }
}

pub fn inc_messages_ingested(source: IngestSource, count: u64) {
    MESSAGES_INGESTED[source as usize].fetch_add(count, Ordering::Relaxed);
}

pub fn inc_patchsets_ingested(count: u64) {
    PATCHSETS_INGESTED.fetch_add(count, Ordering::Relaxed);
}

pub fn inc_patches_ingested(count: u64) {
    PATCHES_INGESTED.fetch_add(count, Ordering::Relaxed);
}

pub fn inc_ingest_error(source: IngestSource) {
    INGEST_ERRORS[source as usize].fetch_add(1, Ordering::Relaxed);
}

pub fn set_git_remotes(fresh: u64, stale: u64, backed_off: u64, failed: u64, max_age_secs: u64) {
    GIT_REMOTES[GitRemoteState::Fresh as usize].store(fresh, Ordering::Relaxed);
    GIT_REMOTES[GitRemoteState::Stale as usize].store(stale, Ordering::Relaxed);
    GIT_REMOTES[GitRemoteState::BackedOff as usize].store(backed_off, Ordering::Relaxed);
    GIT_REMOTES[GitRemoteState::Failed as usize].store(failed, Ordering::Relaxed);
    GIT_REMOTE_MAX_AGE_SECS.store(max_age_secs, Ordering::Relaxed);
}

pub fn get_git_remotes(state: GitRemoteState) -> u64 {
    GIT_REMOTES[state as usize].load(Ordering::Relaxed)
}

pub fn get_git_remote_max_age_secs() -> u64 {
    GIT_REMOTE_MAX_AGE_SECS.load(Ordering::Relaxed)
}

pub fn add_git_sync_runs(outcome: GitSyncOutcome, count: u64) {
    if count > 0 {
        GIT_SYNC_RUNS[outcome as usize].fetch_add(count, Ordering::Relaxed);
    }
}

pub fn observe_git_op(op: GitOpKind, duration: Duration) {
    GIT_OP_DURATION[op as usize].observe(duration);
}

pub fn set_nntp_lag(total_articles: u64, max_group_articles: u64) {
    NNTP_LAG_TOTAL.store(total_articles, Ordering::Relaxed);
    NNTP_LAG_MAX.store(max_group_articles, Ordering::Relaxed);
}

pub fn get_nntp_lag() -> (u64, u64) {
    (
        NNTP_LAG_TOTAL.load(Ordering::Relaxed),
        NNTP_LAG_MAX.load(Ordering::Relaxed),
    )
}

pub fn inc_review_completed(outcome: ReviewOutcomeMetric) {
    REVIEWS_COMPLETED[outcome as usize].fetch_add(1, Ordering::Relaxed);
}

pub fn add_reviews_completed(outcome: ReviewOutcomeMetric, count: u64) {
    if count > 0 {
        REVIEWS_COMPLETED[outcome as usize].fetch_add(count, Ordering::Relaxed);
    }
}

pub fn observe_review_duration(duration: Duration) {
    REVIEW_DURATION.observe(duration);
}

pub fn observe_patchset_turnaround(duration: Duration) {
    PATCHSET_TURNAROUND_DURATION.observe(duration);
}

pub fn observe_ai_request_duration(duration: Duration) {
    AI_REQUEST_DURATION.observe(duration);
}

pub fn add_ai_tokens(workload: AiWorkload, uncached_in: u64, cached_in: u64, out: u64) {
    if uncached_in > 0 {
        AI_TOKENS[workload as usize][AiTokenDir::UncachedIn as usize]
            .fetch_add(uncached_in, Ordering::Relaxed);
    }
    if cached_in > 0 {
        AI_TOKENS[workload as usize][AiTokenDir::CachedIn as usize]
            .fetch_add(cached_in, Ordering::Relaxed);
    }
    if out > 0 {
        AI_TOKENS[workload as usize][AiTokenDir::Out as usize].fetch_add(out, Ordering::Relaxed);
    }
}

pub fn inc_ai_error(kind: AiErrorKind) {
    AI_ERRORS[kind as usize].fetch_add(1, Ordering::Relaxed);
}

pub fn add_ai_errors(kind: AiErrorKind, count: u64) {
    if count > 0 {
        AI_ERRORS[kind as usize].fetch_add(count, Ordering::Relaxed);
    }
}

pub fn get_ai_errors(kind: AiErrorKind) -> u64 {
    AI_ERRORS[kind as usize].load(Ordering::Relaxed)
}

pub fn inc_ai_cache_request(outcome: CacheOutcome) {
    AI_CACHE_REQUESTS[outcome as usize].fetch_add(1, Ordering::Relaxed);
}

pub fn set_ai_inflight_requests(inflight: u64) {
    AI_INFLIGHT_REQUESTS.store(inflight, Ordering::Relaxed);
}

pub fn inc_ai_inflight_requests() {
    AI_INFLIGHT_REQUESTS.fetch_add(1, Ordering::Relaxed);
}

pub fn dec_ai_inflight_requests() {
    let _ = AI_INFLIGHT_REQUESTS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
        Some(cur.saturating_sub(1))
    });
}

pub fn get_ai_inflight_requests() -> u64 {
    AI_INFLIGHT_REQUESTS.load(Ordering::Relaxed)
}

fn unix_secs_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn set_ai_quota_cooldown_secs(secs: u64) {
    let until = if secs == 0 {
        0
    } else {
        unix_secs_now().saturating_add(secs as i64)
    };
    AI_QUOTA_COOLDOWN_UNTIL_UNIX.store(until, Ordering::Relaxed);
}

pub fn get_ai_quota_cooldown_secs() -> u64 {
    let until = AI_QUOTA_COOLDOWN_UNTIL_UNIX.load(Ordering::Relaxed);
    if until <= 0 {
        0
    } else {
        until.saturating_sub(unix_secs_now()).max(0) as u64
    }
}

pub fn inc_outbox_event(channel: OutboxChannel, outcome: OutboxOutcome) {
    OUTBOX_EVENTS[channel as usize][outcome as usize].fetch_add(1, Ordering::Relaxed);
}

pub fn set_email_worker_heartbeat_age_secs(age_secs: i64) {
    EMAIL_WORKER_HEARTBEAT_AGE_SECS.store(age_secs, Ordering::Relaxed);
}

pub fn get_email_worker_heartbeat_age_secs() -> i64 {
    EMAIL_WORKER_HEARTBEAT_AGE_SECS.load(Ordering::Relaxed)
}

// ============================================================================
// Two-Phase Rollup Delta Snapshotter
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
pub struct RollupSample {
    pub metric: &'static str,
    pub labels: Vec<(&'static str, &'static str)>,
    pub delta: f64,
}

#[derive(Debug, Clone, Copy)]
struct RuntimeCounterSnapshot {
    log_events: [[u64; LogComponent::COUNT]; LogLevelMetric::COUNT],
    worker_exits: [u64; WorkerKind::COUNT],
    worker_panics: [u64; WorkerKind::COUNT],
    messages_ingested: [u64; IngestSource::COUNT],
    patchsets_ingested: u64,
    patches_ingested: u64,
    ingest_errors: [u64; IngestSource::COUNT],
    git_sync_runs: [u64; GitSyncOutcome::COUNT],
    reviews_completed: [u64; ReviewOutcomeMetric::COUNT],
    reviews_skipped_unchanged: u64,
    ai_tokens: [[u64; AiTokenDir::COUNT]; AiWorkload::COUNT],
    ai_errors: [u64; AiErrorKind::COUNT],
    ai_cache_requests: [u64; CacheOutcome::COUNT],
    outbox_events: [[u64; OutboxOutcome::COUNT]; OutboxChannel::COUNT],
    ai_hist: HistogramState<12>,
    review_hist: HistogramState<10>,
    turnaround_hist: HistogramState<9>,
    git_op_hist: [HistogramState<8>; GitOpKind::COUNT],
}

impl RuntimeCounterSnapshot {
    const fn zero() -> Self {
        Self {
            log_events: [[0; LogComponent::COUNT]; LogLevelMetric::COUNT],
            worker_exits: [0; WorkerKind::COUNT],
            worker_panics: [0; WorkerKind::COUNT],
            messages_ingested: [0; IngestSource::COUNT],
            patchsets_ingested: 0,
            patches_ingested: 0,
            ingest_errors: [0; IngestSource::COUNT],
            git_sync_runs: [0; GitSyncOutcome::COUNT],
            reviews_completed: [0; ReviewOutcomeMetric::COUNT],
            reviews_skipped_unchanged: 0,
            ai_tokens: [[0; AiTokenDir::COUNT]; AiWorkload::COUNT],
            ai_errors: [0; AiErrorKind::COUNT],
            ai_cache_requests: [0; CacheOutcome::COUNT],
            outbox_events: [[0; OutboxOutcome::COUNT]; OutboxChannel::COUNT],
            ai_hist: HistogramState::zero(),
            review_hist: HistogramState::zero(),
            turnaround_hist: HistogramState::zero(),
            git_op_hist: [HistogramState::zero(); GitOpKind::COUNT],
        }
    }

    fn capture_current() -> Self {
        let mut snap = Self::zero();
        for lvl in LogLevelMetric::ALL {
            for comp in LogComponent::ALL {
                snap.log_events[lvl as usize][comp as usize] = get_log_events(lvl, comp);
            }
        }
        for w in WorkerKind::ALL {
            snap.worker_exits[w as usize] = WORKER_EXITS[w as usize].load(Ordering::Relaxed);
            snap.worker_panics[w as usize] = WORKER_PANICS[w as usize].load(Ordering::Relaxed);
        }
        for src in IngestSource::ALL {
            snap.messages_ingested[src as usize] =
                MESSAGES_INGESTED[src as usize].load(Ordering::Relaxed);
            snap.ingest_errors[src as usize] = INGEST_ERRORS[src as usize].load(Ordering::Relaxed);
        }
        snap.patchsets_ingested = PATCHSETS_INGESTED.load(Ordering::Relaxed);
        snap.patches_ingested = PATCHES_INGESTED.load(Ordering::Relaxed);
        for out in GitSyncOutcome::ALL {
            snap.git_sync_runs[out as usize] = GIT_SYNC_RUNS[out as usize].load(Ordering::Relaxed);
        }
        for r in ReviewOutcomeMetric::ALL {
            snap.reviews_completed[r as usize] =
                REVIEWS_COMPLETED[r as usize].load(Ordering::Relaxed);
        }
        snap.reviews_skipped_unchanged = get_reviews_skipped_unchanged() as u64;
        for wl in AiWorkload::ALL {
            for dir in AiTokenDir::ALL {
                snap.ai_tokens[wl as usize][dir as usize] =
                    AI_TOKENS[wl as usize][dir as usize].load(Ordering::Relaxed);
            }
        }
        for ek in AiErrorKind::ALL {
            snap.ai_errors[ek as usize] = AI_ERRORS[ek as usize].load(Ordering::Relaxed);
        }
        for co in CacheOutcome::ALL {
            snap.ai_cache_requests[co as usize] =
                AI_CACHE_REQUESTS[co as usize].load(Ordering::Relaxed);
        }
        for ch in OutboxChannel::ALL {
            for out in OutboxOutcome::ALL {
                snap.outbox_events[ch as usize][out as usize] =
                    OUTBOX_EVENTS[ch as usize][out as usize].load(Ordering::Relaxed);
            }
        }
        snap.ai_hist = AI_REQUEST_DURATION.snapshot();
        snap.review_hist = REVIEW_DURATION.snapshot();
        snap.turnaround_hist = PATCHSET_TURNAROUND_DURATION.snapshot();
        for op in GitOpKind::ALL {
            snap.git_op_hist[op as usize] = GIT_OP_DURATION[op as usize].snapshot();
        }
        snap
    }

    fn diff_from(&self, prev: &Self) -> Vec<RollupSample> {
        let mut out = Vec::new();
        push_counter_deltas(self, prev, &mut out);
        push_hist_deltas(
            "sashiko_ai_request_duration_seconds",
            &[],
            AI_REQUEST_DURATION.le_labels(),
            &self.ai_hist,
            &prev.ai_hist,
            &mut out,
        );
        push_hist_deltas(
            "sashiko_review_duration_seconds",
            &[],
            REVIEW_DURATION.le_labels(),
            &self.review_hist,
            &prev.review_hist,
            &mut out,
        );
        push_hist_deltas(
            "sashiko_patchset_turnaround_seconds",
            &[],
            PATCHSET_TURNAROUND_DURATION.le_labels(),
            &self.turnaround_hist,
            &prev.turnaround_hist,
            &mut out,
        );
        for op in GitOpKind::ALL {
            push_hist_deltas(
                "sashiko_git_op_duration_seconds",
                &[("op", op.as_str())],
                GIT_OP_DURATION[op as usize].le_labels(),
                &self.git_op_hist[op as usize],
                &prev.git_op_hist[op as usize],
                &mut out,
            );
        }
        out
    }
}

fn push_u64_delta(
    out: &mut Vec<RollupSample>,
    metric: &'static str,
    labels: Vec<(&'static str, &'static str)>,
    curr: u64,
    prev: u64,
) {
    let delta = curr.saturating_sub(prev);
    if delta > 0 {
        out.push(RollupSample {
            metric,
            labels,
            delta: delta as f64,
        });
    }
}

fn push_counter_deltas(
    curr: &RuntimeCounterSnapshot,
    prev: &RuntimeCounterSnapshot,
    out: &mut Vec<RollupSample>,
) {
    for lvl in LogLevelMetric::ALL {
        for comp in LogComponent::ALL {
            push_u64_delta(
                out,
                "sashiko_log_events_total",
                vec![("component", comp.as_str()), ("level", lvl.as_str())],
                curr.log_events[lvl as usize][comp as usize],
                prev.log_events[lvl as usize][comp as usize],
            );
        }
    }
    for w in WorkerKind::ALL {
        push_u64_delta(
            out,
            "sashiko_worker_restarts_total",
            vec![("reason", "exit"), ("worker", w.as_str())],
            curr.worker_exits[w as usize],
            prev.worker_exits[w as usize],
        );
        push_u64_delta(
            out,
            "sashiko_worker_restarts_total",
            vec![("reason", "panic"), ("worker", w.as_str())],
            curr.worker_panics[w as usize],
            prev.worker_panics[w as usize],
        );
    }
    for src in IngestSource::ALL {
        push_u64_delta(
            out,
            "sashiko_messages_ingested_total",
            vec![("source", src.as_str())],
            curr.messages_ingested[src as usize],
            prev.messages_ingested[src as usize],
        );
        push_u64_delta(
            out,
            "sashiko_ingest_errors_total",
            vec![("source", src.as_str())],
            curr.ingest_errors[src as usize],
            prev.ingest_errors[src as usize],
        );
    }
    push_u64_delta(
        out,
        "sashiko_patchsets_ingested_total",
        Vec::new(),
        curr.patchsets_ingested,
        prev.patchsets_ingested,
    );
    push_u64_delta(
        out,
        "sashiko_patches_ingested_total",
        Vec::new(),
        curr.patches_ingested,
        prev.patches_ingested,
    );
    for go in GitSyncOutcome::ALL {
        push_u64_delta(
            out,
            "sashiko_git_sync_runs_total",
            vec![("outcome", go.as_str())],
            curr.git_sync_runs[go as usize],
            prev.git_sync_runs[go as usize],
        );
    }
    for r in ReviewOutcomeMetric::ALL {
        push_u64_delta(
            out,
            "sashiko_reviews_completed_total",
            vec![("status", r.as_str())],
            curr.reviews_completed[r as usize],
            prev.reviews_completed[r as usize],
        );
    }
    push_u64_delta(
        out,
        "sashiko_reviews_skipped_unchanged_total",
        Vec::new(),
        curr.reviews_skipped_unchanged,
        prev.reviews_skipped_unchanged,
    );
    for wl in AiWorkload::ALL {
        for dir in AiTokenDir::ALL {
            push_u64_delta(
                out,
                "sashiko_ai_tokens_total",
                vec![("direction", dir.as_str()), ("workload", wl.as_str())],
                curr.ai_tokens[wl as usize][dir as usize],
                prev.ai_tokens[wl as usize][dir as usize],
            );
        }
    }
    for ek in AiErrorKind::ALL {
        push_u64_delta(
            out,
            "sashiko_ai_errors_total",
            vec![("class", ek.as_str())],
            curr.ai_errors[ek as usize],
            prev.ai_errors[ek as usize],
        );
    }
    for co in CacheOutcome::ALL {
        push_u64_delta(
            out,
            "sashiko_ai_cache_requests_total",
            vec![("outcome", co.as_str())],
            curr.ai_cache_requests[co as usize],
            prev.ai_cache_requests[co as usize],
        );
    }
    for ch in OutboxChannel::ALL {
        for oc in OutboxOutcome::ALL {
            push_u64_delta(
                out,
                "sashiko_outbox_events_total",
                vec![("channel", ch.as_str()), ("outcome", oc.as_str())],
                curr.outbox_events[ch as usize][oc as usize],
                prev.outbox_events[ch as usize][oc as usize],
            );
        }
    }
}

fn push_hist_deltas<const B: usize>(
    base_name: &'static str,
    extra_labels: &[(&'static str, &'static str)],
    le_labels: &[&'static str; B],
    curr: &HistogramState<B>,
    prev: &HistogramState<B>,
    out: &mut Vec<RollupSample>,
) {
    let count_delta = curr.count.saturating_sub(prev.count);
    if count_delta == 0 {
        return;
    }
    let (bucket_metric, sum_metric, count_metric) = match base_name {
        "sashiko_ai_request_duration_seconds" => (
            "sashiko_ai_request_duration_seconds_bucket",
            "sashiko_ai_request_duration_seconds_sum",
            "sashiko_ai_request_duration_seconds_count",
        ),
        "sashiko_review_duration_seconds" => (
            "sashiko_review_duration_seconds_bucket",
            "sashiko_review_duration_seconds_sum",
            "sashiko_review_duration_seconds_count",
        ),
        "sashiko_patchset_turnaround_seconds" => (
            "sashiko_patchset_turnaround_seconds_bucket",
            "sashiko_patchset_turnaround_seconds_sum",
            "sashiko_patchset_turnaround_seconds_count",
        ),
        _ => (
            "sashiko_git_op_duration_seconds_bucket",
            "sashiko_git_op_duration_seconds_sum",
            "sashiko_git_op_duration_seconds_count",
        ),
    };

    for (idx, le) in le_labels.iter().enumerate() {
        let b_delta = curr.cumulative[idx].saturating_sub(prev.cumulative[idx]);
        if b_delta > 0 {
            let mut labels = Vec::with_capacity(extra_labels.len() + 1);
            labels.extend_from_slice(extra_labels);
            labels.push(("le", le));
            labels.sort_by_key(|(k, _)| *k);
            out.push(RollupSample {
                metric: bucket_metric,
                labels,
                delta: b_delta as f64,
            });
        }
    }

    let sum_secs = curr.sum_micros.saturating_sub(prev.sum_micros) as f64 / 1_000_000.0;
    let mut base_labels = extra_labels.to_vec();
    base_labels.sort_by_key(|(k, _)| *k);
    out.push(RollupSample {
        metric: sum_metric,
        labels: base_labels.clone(),
        delta: sum_secs,
    });
    out.push(RollupSample {
        metric: count_metric,
        labels: base_labels,
        delta: count_delta as f64,
    });
}

static FLUSH_WATERMARK: Mutex<RuntimeCounterSnapshot> = Mutex::new(RuntimeCounterSnapshot::zero());

/// Uncommitted counter snapshot prepared for durable SQLite rollup flushing.
/// Call [`commit_runtime_flush`] only after the database transaction succeeds.
pub struct PendingRuntimeFlush {
    curr: RuntimeCounterSnapshot,
    samples: Vec<RollupSample>,
}

impl PendingRuntimeFlush {
    pub fn samples(&self) -> &[RollupSample] {
        &self.samples
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

fn read_flush_watermark() -> RuntimeCounterSnapshot {
    // Only copies a plain fixed-size struct; never panics while locked.
    match FLUSH_WATERMARK.lock() {
        Ok(guard) => *guard,
        Err(poisoned) => *poisoned.into_inner(),
    }
}

pub fn prepare_runtime_flush() -> PendingRuntimeFlush {
    let prev = read_flush_watermark();
    let curr = RuntimeCounterSnapshot::capture_current();
    let samples = curr.diff_from(&prev);
    PendingRuntimeFlush { curr, samples }
}

pub fn commit_runtime_flush(pending: &PendingRuntimeFlush) {
    match FLUSH_WATERMARK.lock() {
        Ok(mut guard) => *guard = pending.curr,
        Err(poisoned) => *poisoned.into_inner() = pending.curr,
    }
}

/// Returns non-zero counter and histogram deltas accumulated since the last
/// committed database flush so live queries reflect sub-minute activity.
pub fn unflushed_runtime_deltas() -> Vec<RollupSample> {
    let prev = read_flush_watermark();
    let curr = RuntimeCounterSnapshot::capture_current();
    curr.diff_from(&prev)
}

// ============================================================================
// Prometheus Text 0.0.4 Exposition Formatting
// ============================================================================

pub fn escape_prometheus_label_value(raw: &str, out: &mut String) {
    for ch in raw.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

pub fn write_prometheus_header(buf: &mut String, name: &str, metric_type: MetricType, help: &str) {
    let _ = writeln!(buf, "# HELP {name} {help}");
    let _ = writeln!(buf, "# TYPE {name} {}", metric_type.as_str());
}

pub fn write_prometheus_sample(buf: &mut String, name: &str, labels: &[(&str, &str)], value: f64) {
    buf.push_str(name);
    if !labels.is_empty() {
        buf.push('{');
        for (idx, (k, v)) in labels.iter().enumerate() {
            if idx > 0 {
                buf.push(',');
            }
            buf.push_str(k);
            buf.push_str("=\"");
            escape_prometheus_label_value(v, buf);
            buf.push('"');
        }
        buf.push('}');
    }
    buf.push(' ');
    if value.is_nan() {
        buf.push_str("NaN");
    } else if value.is_infinite() {
        if value.is_sign_positive() {
            buf.push_str("+Inf");
        } else {
            buf.push_str("-Inf");
        }
    } else if value.fract() == 0.0 && value.abs() < 1e18 {
        let _ = write!(buf, "{}", value as i64);
    } else {
        let _ = write!(buf, "{value:.6}");
    }
    buf.push('\n');
}

fn write_histogram_exposition<const B: usize>(
    buf: &mut String,
    base_name: &str,
    extra_labels: &[(&str, &str)],
    le_labels: &[&str; B],
    state: &HistogramState<B>,
) {
    let bucket_name = format!("{base_name}_bucket");
    let sum_name = format!("{base_name}_sum");
    let count_name = format!("{base_name}_count");
    let mut labels = Vec::with_capacity(extra_labels.len() + 1);
    for (idx, le) in le_labels.iter().enumerate() {
        labels.clear();
        labels.extend_from_slice(extra_labels);
        labels.push(("le", *le));
        write_prometheus_sample(buf, &bucket_name, &labels, state.cumulative[idx] as f64);
    }
    write_prometheus_sample(
        buf,
        &sum_name,
        extra_labels,
        state.sum_micros as f64 / 1_000_000.0,
    );
    write_prometheus_sample(buf, &count_name, extra_labels, state.count as f64);
}

/// Renders all process-level runtime counters, gauges, and histograms in
/// Prometheus text `0.0.4` format without performing any database queries.
pub fn render_runtime_prometheus(buf: &mut String) {
    let snap = RuntimeCounterSnapshot::capture_current();

    write_prometheus_header(
        buf,
        "sashiko_build_info",
        MetricType::Gauge,
        "Sashiko build version metadata (constant 1).",
    );
    write_prometheus_sample(
        buf,
        "sashiko_build_info",
        &[("version", env!("CARGO_PKG_VERSION"))],
        1.0,
    );

    write_runtime_gauges(buf);
    write_runtime_counters(buf, &snap);
    write_runtime_histograms(buf, &snap);
}

fn write_runtime_gauges(buf: &mut String) {
    write_prometheus_header(
        buf,
        "sashiko_repo_packs",
        MetricType::Gauge,
        "Number of git packfiles in the review repository.",
    );
    write_prometheus_sample(buf, "sashiko_repo_packs", &[], get_repo_packs() as f64);

    write_prometheus_header(
        buf,
        "sashiko_repo_pack_bytes",
        MetricType::Gauge,
        "Total size in bytes of git packfiles in the review repository.",
    );
    write_prometheus_sample(
        buf,
        "sashiko_repo_pack_bytes",
        &[],
        get_repo_pack_bytes() as f64,
    );

    write_prometheus_header(
        buf,
        "sashiko_git_remotes",
        MetricType::Gauge,
        "Tracked git remotes grouped by freshness state.",
    );
    for st in GitRemoteState::ALL {
        write_prometheus_sample(
            buf,
            "sashiko_git_remotes",
            &[("state", st.as_str())],
            get_git_remotes(st) as f64,
        );
    }

    write_prometheus_header(
        buf,
        "sashiko_git_remote_max_age_seconds",
        MetricType::Gauge,
        "Maximum time in seconds since the oldest tracked git remote synced cleanly.",
    );
    write_prometheus_sample(
        buf,
        "sashiko_git_remote_max_age_seconds",
        &[],
        get_git_remote_max_age_secs() as f64,
    );

    let (lag_total, lag_max) = get_nntp_lag();
    write_prometheus_header(
        buf,
        "sashiko_nntp_lag_articles",
        MetricType::Gauge,
        "Unread NNTP articles behind upstream group high-water marks.",
    );
    write_prometheus_sample(
        buf,
        "sashiko_nntp_lag_articles",
        &[("agg", "total")],
        lag_total as f64,
    );
    write_prometheus_sample(
        buf,
        "sashiko_nntp_lag_articles",
        &[("agg", "max_group")],
        lag_max as f64,
    );

    write_prometheus_header(
        buf,
        "sashiko_ai_inflight_requests",
        MetricType::Gauge,
        "Currently executing uncached LLM provider HTTP requests.",
    );
    write_prometheus_sample(
        buf,
        "sashiko_ai_inflight_requests",
        &[],
        get_ai_inflight_requests() as f64,
    );

    write_prometheus_header(
        buf,
        "sashiko_ai_quota_cooldown_seconds",
        MetricType::Gauge,
        "Remaining provider rate-limit cooldown duration in seconds.",
    );
    write_prometheus_sample(
        buf,
        "sashiko_ai_quota_cooldown_seconds",
        &[],
        get_ai_quota_cooldown_secs() as f64,
    );

    let hb = get_email_worker_heartbeat_age_secs();
    if hb >= 0 {
        write_prometheus_header(
            buf,
            "sashiko_email_worker_heartbeat_age_seconds",
            MetricType::Gauge,
            "Seconds elapsed since the email outbox worker last completed a poll.",
        );
        write_prometheus_sample(
            buf,
            "sashiko_email_worker_heartbeat_age_seconds",
            &[],
            hb as f64,
        );
    }
}

fn write_runtime_counters(buf: &mut String, snap: &RuntimeCounterSnapshot) {
    write_prometheus_header(
        buf,
        "sashiko_log_events_total",
        MetricType::Counter,
        "Total tracing warning and error log events by component.",
    );
    for lvl in LogLevelMetric::ALL {
        for comp in LogComponent::ALL {
            let v = snap.log_events[lvl as usize][comp as usize];
            if v > 0 || matches!(comp, LogComponent::Reviewer | LogComponent::Server) {
                write_prometheus_sample(
                    buf,
                    "sashiko_log_events_total",
                    &[("level", lvl.as_str()), ("component", comp.as_str())],
                    v as f64,
                );
            }
        }
    }

    write_prometheus_header(
        buf,
        "sashiko_worker_restarts_total",
        MetricType::Counter,
        "Total unexpected supervised background worker exits and panics.",
    );
    for w in WorkerKind::ALL {
        let exits = snap.worker_exits[w as usize];
        let panics = snap.worker_panics[w as usize];
        if exits > 0 {
            write_prometheus_sample(
                buf,
                "sashiko_worker_restarts_total",
                &[("worker", w.as_str()), ("reason", "exit")],
                exits as f64,
            );
        }
        if panics > 0 {
            write_prometheus_sample(
                buf,
                "sashiko_worker_restarts_total",
                &[("worker", w.as_str()), ("reason", "panic")],
                panics as f64,
            );
        }
    }

    write_prometheus_header(
        buf,
        "sashiko_messages_ingested_total",
        MetricType::Counter,
        "Total messages ingested by transport source.",
    );
    for src in IngestSource::ALL {
        write_prometheus_sample(
            buf,
            "sashiko_messages_ingested_total",
            &[("source", src.as_str())],
            snap.messages_ingested[src as usize] as f64,
        );
    }

    write_prometheus_header(
        buf,
        "sashiko_patchsets_ingested_total",
        MetricType::Counter,
        "Total patchsets created during ingestion.",
    );
    write_prometheus_sample(
        buf,
        "sashiko_patchsets_ingested_total",
        &[],
        snap.patchsets_ingested as f64,
    );

    write_prometheus_header(
        buf,
        "sashiko_patches_ingested_total",
        MetricType::Counter,
        "Total individual patches created during ingestion.",
    );
    write_prometheus_sample(
        buf,
        "sashiko_patches_ingested_total",
        &[],
        snap.patches_ingested as f64,
    );

    write_prometheus_header(
        buf,
        "sashiko_ingest_errors_total",
        MetricType::Counter,
        "Total message/patch ingestion errors by transport source.",
    );
    for src in IngestSource::ALL {
        write_prometheus_sample(
            buf,
            "sashiko_ingest_errors_total",
            &[("source", src.as_str())],
            snap.ingest_errors[src as usize] as f64,
        );
    }

    write_prometheus_header(
        buf,
        "sashiko_git_sync_runs_total",
        MetricType::Counter,
        "Total git remote refresh attempts grouped by outcome.",
    );
    for go in GitSyncOutcome::ALL {
        write_prometheus_sample(
            buf,
            "sashiko_git_sync_runs_total",
            &[("outcome", go.as_str())],
            snap.git_sync_runs[go as usize] as f64,
        );
    }

    write_prometheus_header(
        buf,
        "sashiko_reviews_completed_total",
        MetricType::Counter,
        "Total completed patch review attempts grouped by terminal outcome.",
    );
    for r in ReviewOutcomeMetric::ALL {
        write_prometheus_sample(
            buf,
            "sashiko_reviews_completed_total",
            &[("status", r.as_str())],
            snap.reviews_completed[r as usize] as f64,
        );
    }

    write_prometheus_header(
        buf,
        "sashiko_reviews_skipped_unchanged_total",
        MetricType::Counter,
        "Total reviews skipped because an identical patch diff already reviewed clean.",
    );
    write_prometheus_sample(
        buf,
        "sashiko_reviews_skipped_unchanged_total",
        &[],
        snap.reviews_skipped_unchanged as f64,
    );

    write_prometheus_header(
        buf,
        "sashiko_ai_tokens_total",
        MetricType::Counter,
        "Total LLM tokens consumed by direction and workload.",
    );
    for wl in AiWorkload::ALL {
        for dir in AiTokenDir::ALL {
            write_prometheus_sample(
                buf,
                "sashiko_ai_tokens_total",
                &[("workload", wl.as_str()), ("direction", dir.as_str())],
                snap.ai_tokens[wl as usize][dir as usize] as f64,
            );
        }
    }

    write_prometheus_header(
        buf,
        "sashiko_ai_errors_total",
        MetricType::Counter,
        "Total LLM provider errors and validation retries by error class.",
    );
    for ek in AiErrorKind::ALL {
        write_prometheus_sample(
            buf,
            "sashiko_ai_errors_total",
            &[("class", ek.as_str())],
            snap.ai_errors[ek as usize] as f64,
        );
    }

    write_prometheus_header(
        buf,
        "sashiko_ai_cache_requests_total",
        MetricType::Counter,
        "Total local AI response-cache lookups by hit or miss outcome.",
    );
    for co in CacheOutcome::ALL {
        write_prometheus_sample(
            buf,
            "sashiko_ai_cache_requests_total",
            &[("outcome", co.as_str())],
            snap.ai_cache_requests[co as usize] as f64,
        );
    }

    write_prometheus_header(
        buf,
        "sashiko_outbox_events_total",
        MetricType::Counter,
        "Total outbound notification delivery outcomes by channel.",
    );
    for ch in OutboxChannel::ALL {
        for oc in OutboxOutcome::ALL {
            write_prometheus_sample(
                buf,
                "sashiko_outbox_events_total",
                &[("channel", ch.as_str()), ("outcome", oc.as_str())],
                snap.outbox_events[ch as usize][oc as usize] as f64,
            );
        }
    }
}

fn write_runtime_histograms(buf: &mut String, snap: &RuntimeCounterSnapshot) {
    write_prometheus_header(
        buf,
        "sashiko_ai_request_duration_seconds",
        MetricType::Histogram,
        "Latency distribution of uncached LLM provider calls in seconds.",
    );
    write_histogram_exposition(
        buf,
        "sashiko_ai_request_duration_seconds",
        &[],
        AI_REQUEST_DURATION.le_labels(),
        &snap.ai_hist,
    );

    write_prometheus_header(
        buf,
        "sashiko_review_duration_seconds",
        MetricType::Histogram,
        "Wall-clock execution duration of individual patch reviews in seconds.",
    );
    write_histogram_exposition(
        buf,
        "sashiko_review_duration_seconds",
        &[],
        REVIEW_DURATION.le_labels(),
        &snap.review_hist,
    );

    write_prometheus_header(
        buf,
        "sashiko_patchset_turnaround_seconds",
        MetricType::Histogram,
        "End-to-end turnaround from patchset arrival to completed review in seconds.",
    );
    write_histogram_exposition(
        buf,
        "sashiko_patchset_turnaround_seconds",
        &[],
        PATCHSET_TURNAROUND_DURATION.le_labels(),
        &snap.turnaround_hist,
    );

    write_prometheus_header(
        buf,
        "sashiko_git_op_duration_seconds",
        MetricType::Histogram,
        "Duration of git fetch, repack, and baseline preparation operations in seconds.",
    );
    for op in GitOpKind::ALL {
        write_histogram_exposition(
            buf,
            "sashiko_git_op_duration_seconds",
            &[("op", op.as_str())],
            GIT_OP_DURATION[op as usize].le_labels(),
            &snap.git_op_hist[op as usize],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_component_mapping_covers_targets_without_allocation() {
        assert_eq!(
            LogComponent::from_target("sashiko::ingestor"),
            LogComponent::Ingestor
        );
        assert_eq!(
            LogComponent::from_target("sashiko::patch"),
            LogComponent::Ingestor
        );
        assert_eq!(
            LogComponent::from_target("sashiko::patchwork"),
            LogComponent::Patchwork
        );
        assert_eq!(
            LogComponent::from_target("sashiko::worker::patchwork"),
            LogComponent::Patchwork
        );
        assert_eq!(
            LogComponent::from_target("sashiko::nntp"),
            LogComponent::Nntp
        );
        assert_eq!(
            LogComponent::from_target("sashiko::worker::sync"),
            LogComponent::GitSync
        );
        assert_eq!(
            LogComponent::from_target("sashiko::worker::email"),
            LogComponent::Email
        );
        assert_eq!(
            LogComponent::from_target("sashiko::ai::gemini"),
            LogComponent::Ai
        );
        assert_eq!(
            LogComponent::from_target("hyper::proto::h1"),
            LogComponent::Server
        );
        assert_eq!(
            LogComponent::from_target("unknown_crate::foo"),
            LogComponent::Other
        );
    }

    #[test]
    fn test_atomic_histogram_cumulative_buckets_and_overflow() {
        let hist: AtomicHistogram<3, 4> =
            AtomicHistogram::new([100, 500, 1_000], ["0.1", "0.5", "1", "+Inf"]);
        hist.observe(Duration::from_millis(50));
        hist.observe(Duration::from_millis(500));
        hist.observe(Duration::from_millis(2_500));

        let snap = hist.snapshot();
        assert_eq!(snap.cumulative, [1, 2, 2, 3]);
        assert_eq!(snap.count, 3);
        assert_eq!(snap.sum_micros, 3_050_000);
    }

    #[test]
    fn test_two_phase_flush_preserves_deltas_until_committed() {
        // Clear any prior deltas first so the test observes isolated increments.
        let baseline = prepare_runtime_flush();
        commit_runtime_flush(&baseline);

        inc_messages_ingested(IngestSource::Nntp, 5);
        add_ai_tokens(AiWorkload::Review, 1200, 800, 300);

        let unflushed = unflushed_runtime_deltas();
        let has_nntp_delta = |samples: &[RollupSample]| {
            samples.iter().any(|s| {
                s.metric == "sashiko_messages_ingested_total"
                    && s.labels == vec![("source", "nntp")]
            })
        };

        assert!(unflushed.iter().any(|s| {
            s.metric == "sashiko_messages_ingested_total"
                && s.labels == vec![("source", "nntp")]
                && (s.delta - 5.0).abs() < f64::EPSILON
        }));

        // Preparing without committing keeps the delta visible for retries.
        let pending = prepare_runtime_flush();
        assert!(has_nntp_delta(pending.samples()));
        assert!(has_nntp_delta(&unflushed_runtime_deltas()));

        commit_runtime_flush(&pending);
        assert!(!has_nntp_delta(&unflushed_runtime_deltas()));
    }

    #[test]
    fn test_prometheus_exposition_escapes_labels_and_formats_series() {
        let mut buf = String::new();
        render_runtime_prometheus(&mut buf);
        assert!(buf.contains("# TYPE sashiko_build_info gauge\n"));
        assert!(buf.contains("# TYPE sashiko_ai_request_duration_seconds histogram\n"));
        assert!(buf.contains("sashiko_ai_request_duration_seconds_bucket{le=\"+Inf\"} "));

        let mut custom = String::new();
        write_prometheus_sample(
            &mut custom,
            "sashiko_test_metric",
            &[("label", "a\"b\\c\nd")],
            42.0,
        );
        assert_eq!(
            custom,
            "sashiko_test_metric{label=\"a\\\"b\\\\c\\nd\"} 42\n"
        );
    }
}
