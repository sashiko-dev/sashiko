# Sashiko Statistics Rework: Final Design (SRE Perspective)

## 1. Executive Summary

As `sashiko` scales to ingest the complete history and real-time firehose of the Linux Kernel Mailing List (LKML), the current statistics implementation has become a critical bottleneck.

**The Problem:** Current API endpoints (`/api/stats`, `/api/stats/timeline`) perform full table scans with `GROUP BY` and `strftime` operations on the core `messages` and `patchsets` tables. This is an **$O(N)$ operation** that degrades linearly. At millions of rows, this will result in locked databases, multi-second API latencies, and out-of-memory crashes.

**The Solution:** We are shifting from a "Pull" architecture (querying the database on read) to a "Push/Rollup" architecture (pre-aggregating metrics at ingestion/review time). This guarantees **$O(1)$ read performance** (< 5ms response times) for the frontend, ensuring the application remains snappy regardless of database size.

## 2. Core Architecture

We will implement an in-application, time-series metrics pipeline modeled after Prometheus/StatsD, but persisted entirely within our local SQLite database to avoid external dependencies.

1. **Instrumentation (Event Emitters):** The core application logic (ingestion, review workers) will emit events to a central metrics registry using fire-and-forget atomic operations.
2. **In-Memory Aggregation (`StatsRegistry`):** A lock-free or highly concurrent data structure (e.g., atomic counters and atomic gauges) stores the *deltas* and current state.
3. **Periodic Flush (The Rollup):** A background Tokio task runs every 60 seconds. It reads the current aggregated values, `UPSERT`s them into the SQLite time-series tables, and resets the in-memory delta counters.
    * *SRE Note on Durability:* In the event of an ungraceful crash (e.g., `SIGKILL`, panic), up to 60 seconds of statistical data *could* be lost or skewed. This is a standard and acceptable trade-off for high-throughput metrics systems, preventing the database from being hammered by per-patch updates. Graceful shutdown (`SIGTERM`) will include a final synchronous flush.
4. **Read-Only API:** The frontend queries simple, pre-aggregated rows via indexed range scans.

## 3. Metric Definitions (RED / USE Methodology)

To understand the system's health, we track metrics strictly at the **Patch** level, utilizing standard reliability engineering dimensions.

### 3.1. Rate (Throughput)
*   `sashiko_patches_ingested_total` (Counter): Total patches parsed and stored.
*   `sashiko_patches_reviewed_total` (Counter): Total patches successfully processed by the AI worker.

### 3.2. Errors / Outcomes (Quality)
*   `sashiko_findings_total{severity="info|warning|error"}` (Counter): Breakdown of AI-generated review findings. Tracks if the models are becoming excessively noisy or missing critical errors over time.
*   `sashiko_review_failures_total` (Counter): Count of times the review worker failed (e.g., API timeout, context window exceeded) and had to abort or retry.

### 3.3. Duration (Latency)
*   `sashiko_time_to_review_seconds` (Histogram/Average): The wall-clock time from a patch being fully ingested to its review being published. Tracked as an hourly average.

### 3.4. Saturation (Queue Health)
*   `sashiko_queue_pending_patches` (Gauge): Current size of the review backlog.
*   `sashiko_queue_in_progress_patches` (Gauge): Number of patches currently locked by active workers.
*   `sashiko_catchup_ratio` (Derived): `Rate(Reviewed) / Rate(Ingested)`. If $< 1.0$ for sustained periods, the system is permanently falling behind.

### 3.5. Resource Utilization
*   `sashiko_tokens_total{model="gpt4o|claude...", type="prompt|completion"}` (Counter): Cost tracking.
*   `sashiko_tool_usage_total{tool="check_markdown|..."}` (Counter): Frequency of AI tool invocation.

## 4. User Interface Mapping

### 4.1. Global Status Bar (Main Page Footer)
A high-signal, highly compressed overview of current system health.
*   **Pending Patches:** `sashiko_queue_pending_patches` (Gauge)
*   **In-Progress:** `sashiko_queue_in_progress_patches` (Gauge)
*   **Ingested (24h):** Sum of `sashiko_patches_ingested_total` for the last 24h.
*   **Reviewed (24h):** Sum of `sashiko_patches_reviewed_total` for the last 24h.
*   **Avg TTR (24h):** Average `sashiko_time_to_review_seconds` over the last 24h.

### 4.2. Detailed Stats Page
Rich visualizations driven by the time-series API.
*   **Throughput Timeline:** Combined bar/line chart of `Ingested` vs. `Reviewed` patches per hour/day.
*   **Queue Health:** Line chart tracking the `Pending Queue Size` and `Catch-up Ratio` over time.
*   **Review Outcomes:** Stacked bar chart of `Findings` grouped by `severity` over time.
*   **Latency Trends:** Line chart of the `Average TTR (Time to Review)` per patch.
*   **AI Resource Usage:**
    *   Area chart of `Tokens Used` split by `model` and `type` (prompt/completion).
    *   Bar chart of `Tool Usage` counts to understand which tools the AI relies on most.

## 5. Database Schema

We introduce dedicated tables optimized for rapid time-slice querying.

```sql
-- Survives restarts, holds absolute instantaneous values.
CREATE TABLE stats_gauges (
    metric_name TEXT PRIMARY KEY,
    value INTEGER NOT NULL,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Stores hourly aggregations for counters and averages.
-- The composite primary key acts as a covering index for fast range queries.
CREATE TABLE stats_timeseries_hourly (
    bucket_time TIMESTAMP NOT NULL,  -- e.g., '2026-03-12 10:00:00'
    metric_name TEXT NOT NULL,       -- e.g., 'patches_ingested_total'
    label       TEXT NOT NULL,       -- e.g., 'gpt4o' or 'warning' (or 'none' if N/A)
    value       INTEGER NOT NULL,
    PRIMARY KEY (bucket_time, metric_name, label)
);

-- Note: We can rely on SQLite's internal speed for daily rollups by querying
-- the hourly table (24 rows per day per metric is negligible overhead).
```

## 6. Implementation & Rollout Plan

1.  **Phase 1: Instrumentation Library**
    *   Create `src/stats.rs` introducing a globally accessible `StatsRegistry` (using `std::sync::atomic` types to avoid lock contention).
    *   Implement `inc_counter`, `set_gauge`, `record_latency`.
2.  **Phase 2: Database Migration & Background Flusher**
    *   Add SQL migration for `stats_gauges` and `stats_timeseries_hourly`.
    *   Spawn a background `tokio` task on startup that runs every 60s, draining the atomic counters and writing via `INSERT ... ON CONFLICT DO UPDATE`.
    *   Hook into the application's shutdown handler to perform a final, synchronous flush.
3.  **Phase 3: Event Binding**
    *   Wire the `StatsRegistry` into the existing `events.rs` pub/sub system.
    *   Update `src/ingestor.rs` and `src/reviewer.rs` to emit the granular tool usage and tokens events.
4.  **Phase 4: API & Frontend Rework**
    *   Rewrite `/api/stats`, `/api/stats/timeline`, `/api/stats/reviews`, and `/api/stats/tools` to read *only* from the new `stats_` tables.
    *   Update the UI to render the new Status Bar and Stats Page charts.
