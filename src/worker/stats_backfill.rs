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

//! Bounded, crash-resumable background worker that backfills `stats_rollups`
//! and the latest 1,000 `review_pipeline_stats` rows from pre-upgrade tables
//! using `meta` high-water marks recorded during Migration `023`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::compression::decompress_string_value;
use crate::db::{
    Database, ExtractSummaryInput, ROLLUP_RESOLUTIONS, ReviewPipelineSummary,
    canonical_labels_json, extract_review_pipeline_summary,
};

const DONE_KEY: &str = "stats_backfill:v1:done";
const CLEAN_PATCHES_DONE_KEY: &str = "stats_backfill:v1:clean_patches_done";
const PIPELINE_CURSOR_KEY: &str = "stats_backfill:v1:cursor:pipeline";
const PIPELINE_COUNT_KEY: &str = "stats_backfill:v1:pipeline_count";
const PIPELINE_TARGET: usize = 1000;
const PIPELINE_BATCH: usize = 50;
const TABLE_CHUNK: i64 = 2000;
const TX_ENTITY_CHUNK: i64 = 250;
const REVIEW_CHUNK: i64 = 1000;

type BucketKey = (String, String, i64, i64);

#[derive(Default)]
struct RollupBatchAccumulator {
    now_ts: i64,
    buckets: BTreeMap<BucketKey, f64>,
}

impl RollupBatchAccumulator {
    fn new(now_ts: i64) -> Self {
        Self {
            now_ts,
            buckets: BTreeMap::new(),
        }
    }

    fn add(&mut self, ts: i64, metric: &str, labels: &[(&str, &str)], delta: f64) {
        if ts <= 0 || delta <= 0.0 || !delta.is_finite() {
            return;
        }
        let labels_json = canonical_labels_json(labels);
        for (res_secs, retention_secs) in ROLLUP_RESOLUTIONS {
            let bucket_ts = (ts / res_secs) * res_secs;
            if bucket_ts >= self.now_ts.saturating_sub(retention_secs) {
                *self
                    .buckets
                    .entry((metric.to_string(), labels_json.clone(), res_secs, bucket_ts))
                    .or_insert(0.0) += delta;
            }
        }
    }
}

/// Runs the asynchronous historical rollup backfill until `stats_backfill:v1:done`
/// is recorded, retrying after a short delay on transient SQLite lock errors.
pub async fn run_stats_backfill_loop(db: Arc<Database>) {
    loop {
        match run_stats_backfill_once(&db).await {
            Ok(()) => return,
            Err(e) => {
                warn!("StatsBackfill encountered transient error (retrying in 10s): {e}");
                sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

/// Executes one complete pass of all backfill phases (also callable directly in
/// deterministic integration tests).
pub async fn run_stats_backfill_once(db: &Database) -> Result<()> {
    let main_done = read_meta_str(db, DONE_KEY).await?.as_deref() == Some("1");
    let email_cursor = read_phase_cursor(db, "email_outbox").await?;
    let clean_patches_done =
        read_meta_str(db, CLEAN_PATCHES_DONE_KEY).await?.as_deref() == Some("1");
    if main_done && email_cursor <= 0 && clean_patches_done {
        debug!("StatsBackfill already completed; exiting.");
        return Ok(());
    }

    if !main_done {
        info!("StatsBackfill: starting bounded historical statistics backfill...");
        backfill_pipeline_summaries(db).await?;
        backfill_messages(db).await?;
        backfill_patchsets(db).await?;
        backfill_patches(db).await?;
        backfill_reviews(db).await?;
        backfill_bugs(db).await?;
        backfill_email_outbox(db).await?;
        if !clean_patches_done {
            // Covers any patchsets already processed before upgrading mid-backfill
            // or post-cutoff; per-patchset ps_patches_done guards skip already-counted rows.
            backfill_clean_patches(db).await?;
            db.execute(
                "INSERT INTO meta (key, value) VALUES (?, '1')
                 ON CONFLICT(key) DO UPDATE SET value = '1'",
                libsql::params![CLEAN_PATCHES_DONE_KEY],
            )
            .await?;
        }
        db.execute(
            "INSERT INTO meta (key, value) VALUES (?, '1')
             ON CONFLICT(key) DO UPDATE SET value = '1'",
            libsql::params![DONE_KEY],
        )
        .await?;
        info!("StatsBackfill: historical statistics backfill completed.");
    } else {
        if email_cursor > 0 {
            info!("StatsBackfill: running incremental email_outbox rollup backfill...");
            backfill_email_outbox(db).await?;
            info!("StatsBackfill: email_outbox rollup backfill completed.");
        }
        if !clean_patches_done {
            info!("StatsBackfill: running incremental clean_patches rollup backfill...");
            backfill_clean_patches(db).await?;
            db.execute(
                "INSERT INTO meta (key, value) VALUES (?, '1')
                 ON CONFLICT(key) DO UPDATE SET value = '1'",
                libsql::params![CLEAN_PATCHES_DONE_KEY],
            )
            .await?;
            info!("StatsBackfill: clean_patches rollup backfill completed.");
        }
    }
    Ok(())
}

async fn read_meta_str(db: &Database, key: &str) -> Result<Option<String>> {
    let mut rows = db
        .read_conn()
        .query("SELECT value FROM meta WHERE key = ?", libsql::params![key])
        .await?;
    let out = match rows.next().await? {
        Some(r) => Some(r.get::<String>(0)?),
        None => None,
    };
    Ok(out)
}

async fn read_phase_cursor(db: &Database, phase: &str) -> Result<i64> {
    let cursor_key = format!("stats_backfill:v1:cursor:{phase}");
    if let Some(s) = read_meta_str(db, &cursor_key).await? {
        return Ok(s.parse::<i64>().unwrap_or(0).max(0));
    }
    let cutoff_key = format!("stats_backfill:cutoff:{phase}");
    let cutoff = read_meta_str(db, &cutoff_key)
        .await?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);
    Ok(cutoff)
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

struct RawReviewRow {
    review_id: i64,
    patchset_id: i64,
    created_at: i64,
    completed_at: i64,
    provider: String,
    model: String,
    status: String,
    tokens_in: i64,
    tokens_out: i64,
    tokens_cached: i64,
    output_raw: libsql::Value,
}

async fn backfill_pipeline_summaries(db: &Database) -> Result<()> {
    let cutoff = read_meta_str(db, "stats_backfill:cutoff:reviews")
        .await?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    if cutoff <= 0 {
        return Ok(());
    }

    let mut cursor = read_meta_str(db, PIPELINE_CURSOR_KEY)
        .await?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(cutoff + 1);
    let mut done_count = read_meta_str(db, PIPELINE_COUNT_KEY)
        .await?
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);

    while cursor > 0 && done_count < PIPELINE_TARGET {
        let take = (PIPELINE_TARGET - done_count).min(PIPELINE_BATCH) as i64;
        let mut rows = db
            .read_conn()
            .query(
                "SELECT r.id, r.patchset_id, COALESCE(r.created_at, 0),
                        COALESCE(ai.created_at, r.created_at, 0),
                        COALESCE(r.provider, ''), COALESCE(r.model, ''),
                        COALESCE(r.status, 'Reviewed'),
                        COALESCE(ai.tokens_in, 0), COALESCE(ai.tokens_out, 0),
                        COALESCE(ai.tokens_cached, 0), ai.output_raw
                 FROM reviews r
                 JOIN patchsets ps ON ps.id = r.patchset_id
                 LEFT JOIN ai_interactions ai ON ai.id = r.interaction_id
                 WHERE r.id < ? AND r.id <= ?
                   AND r.status = 'Reviewed'
                   AND r.inherited_from_review_id IS NULL
                   AND NOT EXISTS (
                       SELECT 1 FROM meta m
                       WHERE m.key = 'stats_backfill:skip_review:' || r.id
                   )
                 ORDER BY r.id DESC
                 LIMIT ?",
                libsql::params![cursor, cutoff, take],
            )
            .await?;

        let mut batch = Vec::new();
        while let Some(r) = rows.next().await? {
            batch.push(RawReviewRow {
                review_id: r.get(0)?,
                patchset_id: r.get(1)?,
                created_at: r.get(2)?,
                completed_at: r.get(3)?,
                provider: r.get(4)?,
                model: r.get(5)?,
                status: r.get(6)?,
                tokens_in: r.get(7)?,
                tokens_out: r.get(8)?,
                tokens_cached: r.get(9)?,
                output_raw: r.get(10).unwrap_or(libsql::Value::Null),
            });
        }
        drop(rows);

        if batch.is_empty() {
            db.execute(
                "INSERT INTO meta (key, value) VALUES (?, '0')
                 ON CONFLICT(key) DO UPDATE SET value = '0'",
                libsql::params![PIPELINE_CURSOR_KEY],
            )
            .await?;
            break;
        }

        let min_id = batch.iter().map(|b| b.review_id).min().unwrap_or(0);
        let ids: Vec<i64> = batch.iter().map(|b| b.review_id).collect();
        let mut tools_by_review = fetch_tools_for_reviews(db, &ids).await?;

        // Offload gzip decompression and JSON parsing to the blocking pool.
        let summaries: Vec<ReviewPipelineSummary> = tokio::task::spawn_blocking(move || {
            batch
                .into_iter()
                .map(|raw| {
                    let parsed = decompress_string_value(&raw.output_raw)
                        .ok()
                        .filter(|s| !s.is_empty())
                        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
                    let tools = tools_by_review.remove(&raw.review_id).unwrap_or_default();
                    extract_review_pipeline_summary(ExtractSummaryInput {
                        review_id: raw.review_id,
                        patchset_id: raw.patchset_id,
                        created_at: raw.created_at,
                        completed_at: raw.completed_at,
                        provider: &raw.provider,
                        model: &raw.model,
                        status: &raw.status,
                        inherited: false,
                        tokens_in: raw.tokens_in,
                        tokens_out: raw.tokens_out,
                        tokens_cached: raw.tokens_cached,
                        review_json: parsed.as_ref(),
                        tool_rows: &tools,
                    })
                })
                .collect()
        })
        .await?;

        let new_done = done_count + summaries.len();
        db.persist_pipeline_backfill_batch(&summaries, min_id, new_done)
            .await?;
        cursor = min_id;
        done_count = new_done;
        sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

async fn fetch_tools_for_reviews(
    db: &Database,
    review_ids: &[i64],
) -> Result<HashMap<i64, Vec<(String, i64, i64)>>> {
    let mut map: HashMap<i64, Vec<(String, i64, i64)>> = HashMap::new();
    if review_ids.is_empty() {
        return Ok(map);
    }
    let placeholders = vec!["?"; review_ids.len()].join(", ");
    let params: Vec<libsql::Value> = review_ids
        .iter()
        .map(|id| libsql::Value::Integer(*id))
        .collect();
    let sql = format!(
        "SELECT review_id, COALESCE(tool_name, 'unknown'), COUNT(*), COALESCE(SUM(output_length), 0)
         FROM tool_usages
         WHERE review_id IN ({placeholders})
         GROUP BY review_id, COALESCE(tool_name, 'unknown')"
    );
    let mut rows = db.read_conn().query(&sql, params).await?;
    while let Some(r) = rows.next().await? {
        let rid: i64 = r.get(0)?;
        map.entry(rid)
            .or_default()
            .push((r.get(1)?, r.get(2)?, r.get(3)?));
    }
    Ok(map)
}

async fn backfill_messages(db: &Database) -> Result<()> {
    let mut cursor = read_phase_cursor(db, "messages").await?;
    while cursor > 0 {
        let low = (cursor - TABLE_CHUNK).max(0);
        let mut acc = RollupBatchAccumulator::new(now_unix());
        let mut rows = db
            .read_conn()
            .query(
                "SELECT m.date,
                        EXISTS (
                            SELECT 1 FROM patchsets ps
                            WHERE ps.cover_letter_message_id = m.message_id
                              AND ps.mr_url IS NOT NULL
                        )
                 FROM messages m
                 WHERE m.id > ? AND m.id <= ? AND m.date IS NOT NULL AND m.date > 0",
                libsql::params![low, cursor],
            )
            .await?;
        while let Some(r) = rows.next().await? {
            let ts: i64 = r.get(0)?;
            let is_forge = r.get::<i64>(1).unwrap_or(0) != 0;
            let src = if is_forge { "forge" } else { "nntp" };
            acc.add(
                ts,
                "sashiko_messages_ingested_total",
                &[("source", src)],
                1.0,
            );
        }
        drop(rows);
        db.commit_backfill_chunk("messages", low, &acc.buckets)
            .await?;
        cursor = low;
        sleep(Duration::from_millis(5)).await;
    }
    Ok(())
}

pub fn version_bucket_label(version: Option<i64>) -> &'static str {
    match version.unwrap_or(1) {
        v if v <= 1 => "v1",
        2 => "v2",
        3 => "v3",
        _ => "v4_plus",
    }
}

async fn backfill_patchsets(db: &Database) -> Result<()> {
    let mut cursor = read_phase_cursor(db, "patchsets").await?;
    while cursor > 0 {
        let low = (cursor - TX_ENTITY_CHUNK).max(0);
        db.backfill_patchsets_chunk(low, cursor, now_unix()).await?;
        cursor = low;
        sleep(Duration::from_millis(5)).await;
    }
    Ok(())
}

async fn backfill_patches(db: &Database) -> Result<()> {
    let mut cursor = read_phase_cursor(db, "patches").await?;
    while cursor > 0 {
        let low = (cursor - TX_ENTITY_CHUNK).max(0);
        db.backfill_patches_chunk(low, cursor, now_unix()).await?;
        cursor = low;
        sleep(Duration::from_millis(5)).await;
    }
    Ok(())
}

async fn backfill_reviews(db: &Database) -> Result<()> {
    let mut cursor = read_phase_cursor(db, "reviews").await?;
    while cursor > 0 {
        let low = (cursor - REVIEW_CHUNK).max(0);
        let now = now_unix();
        let mut acc = RollupBatchAccumulator::new(now);

        let mut rows = db
            .read_conn()
            .query(
                "SELECT COALESCE(r.created_at, 0),
                        COALESCE(r.status, 'Pending'), r.inherited_from_review_id,
                        COALESCE(ai.tokens_in, 0), COALESCE(ai.tokens_out, 0),
                        COALESCE(ai.tokens_cached, 0)
                 FROM reviews r
                 LEFT JOIN ai_interactions ai INDEXED BY idx_ai_interactions_tokens
                     ON ai.id = r.interaction_id
                 WHERE r.id > ? AND r.id <= ?
                   AND r.created_at IS NOT NULL AND r.created_at > 0
                   AND NOT EXISTS (
                       SELECT 1 FROM meta m
                       WHERE m.key = 'stats_backfill:skip_review:' || r.id
                   )",
                libsql::params![low, cursor],
            )
            .await?;

        while let Some(r) = rows.next().await? {
            let ts: i64 = r.get(0)?;
            let status: String = r.get(1)?;
            let inh: Option<i64> = r.get(2).ok();
            let tok_in: i64 = r.get(3)?;
            let tok_out: i64 = r.get(4)?;
            let tok_cached: i64 = r.get(5)?;

            if matches!(status.as_str(), "Pending" | "In Review") {
                continue;
            }
            let out_status = if status == "Skipped" && inh.is_some() {
                "Skipped (Unchanged)"
            } else {
                status.as_str()
            };
            acc.add(
                ts,
                "sashiko_reviews_completed_total",
                &[("status", out_status)],
                1.0,
            );
            if inh.is_some() {
                acc.add(ts, "sashiko_reviews_skipped_unchanged_total", &[], 1.0);
            }
            let uncached_in = (tok_in - tok_cached).max(0);
            if uncached_in > 0 {
                acc.add(
                    ts,
                    "sashiko_ai_tokens_total",
                    &[("direction", "uncached_in"), ("workload", "review")],
                    uncached_in as f64,
                );
            }
            if tok_cached > 0 {
                acc.add(
                    ts,
                    "sashiko_ai_tokens_total",
                    &[("direction", "cached_in"), ("workload", "review")],
                    tok_cached as f64,
                );
            }
            if tok_out > 0 {
                acc.add(
                    ts,
                    "sashiko_ai_tokens_total",
                    &[("direction", "out"), ("workload", "review")],
                    tok_out as f64,
                );
            }
        }
        drop(rows);

        db.commit_backfill_chunk("reviews", low, &acc.buckets)
            .await?;
        cursor = low;
        sleep(Duration::from_millis(5)).await;
    }
    Ok(())
}

pub fn normalize_prior_outcome(verdict: &str, vn_outcome: Option<&str>) -> &'static str {
    match vn_outcome.unwrap_or("") {
        "addressed" => "addressed",
        "confirmed_finding" => "confirmed_finding",
        "settled" => "settled",
        "disproven" => "disproven",
        "dismissed" | "dismissed_in_verification" => "dismissed",
        _ => match verdict {
            "addressed_in_vN" => "addressed",
            "still_unaddressed" => "confirmed_finding",
            "settled_by_authority" => "settled",
            "disproven_by_code" => "disproven",
            _ => "dismissed",
        },
    }
}

async fn backfill_bugs(db: &Database) -> Result<()> {
    let mut cursor = read_phase_cursor(db, "bugs").await?;
    let enrich_cutoff = read_meta_str(db, "stats_backfill:cutoff:bug_enrichments")
        .await?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);
    while cursor > 0 {
        let low = (cursor - TX_ENTITY_CHUNK).max(0);
        db.backfill_bugs_chunk(low, cursor, enrich_cutoff, now_unix())
            .await?;
        cursor = low;
        sleep(Duration::from_millis(5)).await;
    }
    Ok(())
}

async fn backfill_email_outbox(db: &Database) -> Result<()> {
    let mut cursor = read_phase_cursor(db, "email_outbox").await?;
    while cursor > 0 {
        let low = (cursor - TABLE_CHUNK).max(0);
        let now = now_unix();
        let mut acc = RollupBatchAccumulator::new(now);
        let mut rows = db
            .read_conn()
            .query(
                "SELECT COALESCE(e.created_at, 0), COALESCE(e.status, 'Pending')
                 FROM email_outbox e
                 WHERE e.id > ? AND e.id <= ?
                   AND e.created_at IS NOT NULL AND e.created_at > 0
                   AND NOT EXISTS (
                       SELECT 1 FROM meta m
                       WHERE m.key = 'stats_backfill:skip_email_outbox:' || e.id
                   )",
                libsql::params![low, cursor],
            )
            .await?;
        while let Some(r) = rows.next().await? {
            let ts: i64 = r.get(0)?;
            let status: String = r.get(1)?;
            let outcome = match status.as_str() {
                "Sent" => "sent",
                "Failed" => "failed",
                "Muted" => "muted",
                "Skipped" | "Disabled" => "skipped",
                "Dry-Run" => "dry_run",
                _ => continue,
            };
            acc.add(
                ts,
                "sashiko_outbox_events_total",
                &[("channel", "email"), ("outcome", outcome)],
                1.0,
            );
        }
        drop(rows);
        db.commit_backfill_chunk("email_outbox", low, &acc.buckets)
            .await?;
        cursor = low;
        sleep(Duration::from_millis(5)).await;
    }
    Ok(())
}

async fn backfill_clean_patches(db: &Database) -> Result<()> {
    let cursor_key = "stats_backfill:v1:cursor:clean_patches";
    let mut cursor = match read_meta_str(db, cursor_key).await? {
        Some(s) => s.parse::<i64>().unwrap_or(0).max(0),
        None => {
            let mut r = db
                .read_conn()
                .query("SELECT COALESCE(MAX(id), 0) FROM patchsets", ())
                .await?;
            match r.next().await? {
                Some(row) => row.get::<i64>(0)?.max(0),
                None => 0,
            }
        }
    };
    while cursor > 0 {
        let low = (cursor - TX_ENTITY_CHUNK).max(0);
        db.backfill_clean_patches_chunk(low, cursor, now_unix())
            .await?;
        cursor = low;
        sleep(Duration::from_millis(5)).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DatabaseSettings;
    use serde_json::json;

    #[tokio::test]
    async fn test_stats_backfill_populates_rollups_and_pipeline_idempotently() -> Result<()> {
        let db = Database::new(&DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await?;
        // Migrate to v23 first, insert pre-upgrade data, then migrate to v24 so cutoffs capture it.
        db.migrate().await?;
        db.execute("PRAGMA user_version = 23", ()).await?;
        db.execute("DELETE FROM meta WHERE key LIKE 'stats_backfill:%'", ())
            .await?;

        let now = chrono::Utc::now().timestamp() - 600;
        db.execute(
            "INSERT INTO subsystems (id, name, mailing_list_address) VALUES (1, 'netdev', 'netdev@vger.kernel.org')",
            (),
        )
        .await?;
        db.execute(
            "INSERT INTO messages (id, message_id, subject, date) VALUES (1, '<m1@ex>', '[PATCH v2] net: fix', ?)",
            libsql::params![now],
        )
        .await?;
        db.execute(
            "INSERT INTO patchsets (id, subject, status, date, version) VALUES (1, '[PATCH v2] net: fix', 'Reviewed', ?, 2)",
            libsql::params![now],
        )
        .await?;
        db.execute(
            "INSERT INTO patchsets_subsystems (patchset_id, subsystem_id) VALUES (1, 1)",
            (),
        )
        .await?;
        db.execute(
            "INSERT INTO patches (id, patchset_id, message_id, part_index, previous_patch_status)
             VALUES (1, 1, '<m1@ex>', 1, 'modified')",
            (),
        )
        .await?;
        let out_raw = json!({
            "findings": [{"severity": "High", "preexisting": false}],
            "review_map": {
                "project": "linux",
                "stages": [{"name": "locking", "skipped": false, "turns": 2, "tokens_in": 500, "tokens_out": 80, "tokens_cached": 200, "concerns_count": 1, "dismissed_count": 0}],
                "threads": [{"outcome": "finding", "stages": ["locking"]}]
            }
        })
        .to_string();
        db.execute(
            "INSERT INTO ai_interactions (id, provider, model, output_raw, tokens_in, tokens_out, tokens_cached, created_at)
             VALUES ('ai-1', 'gemini', 'gemini-2.5-pro', ?, 500, 80, 200, ?)",
            libsql::params![out_raw, now + 15],
        )
        .await?;
        db.execute(
            "INSERT INTO reviews (id, patchset_id, patch_id, interaction_id, status, provider, model, created_at)
             VALUES (1, 1, 1, 'ai-1', 'Reviewed', 'gemini', 'gemini-2.5-pro', ?)",
            libsql::params![now],
        )
        .await?;
        db.execute(
            "INSERT INTO findings (id, review_id, severity, preexisting) VALUES (1, 1, 3, 0)",
            (),
        )
        .await?;
        db.execute(
            "INSERT INTO prior_feedback_resolutions (id, review_id, patchset_id, patch_id, prior_id, origin_kind, verdict, v_n_outcome)
             VALUES (1, 1, 1, 1, 'PF1', 'sashiko_review', 'addressed_in_vN', 'addressed')",
            (),
        )
        .await?;
        db.execute(
            "INSERT INTO bugs (id, bugid, title, lifecycle_status, pipeline_state, reporter, reported_at, created_at, updated_at)
             VALUES (1, 'linux-bf1', 'fixed race', 'fixed', 'succeeded', 'bot', ?, ?, ?)",
            libsql::params![now - 100, now - 100, now],
        )
        .await?;

        // Re-run v24 migration so cutoffs capture the pre-upgrade rows.
        db.migrate().await?;

        run_stats_backfill_once(&db).await?;

        let multi = db.get_pipeline_multi_window_stats(now + 1000).await?;
        assert_eq!(multi.windows[0].reviews_count, 1);
        assert_eq!(multi.windows[0].stages["locking"].exclusive_findings, 1);

        let mut r = db
            .read_conn()
            .query(
                "SELECT COALESCE(SUM(sr.value), 0)
                 FROM stats_rollups sr
                 JOIN stats_series ss ON ss.id = sr.series_id
                 WHERE ss.metric = 'sashiko_prior_feedback_total' AND sr.resolution_secs = 86400",
                (),
            )
            .await?;
        let pf_sum: f64 = r.next().await?.unwrap().get(0)?;
        drop(r);
        assert!((pf_sum - 1.0).abs() < f64::EPSILON);

        // Second invocation must be a strict no-op (never double-counts).
        run_stats_backfill_once(&db).await?;
        let mut r2 = db
            .read_conn()
            .query(
                "SELECT COALESCE(SUM(sr.value), 0)
                 FROM stats_rollups sr
                 JOIN stats_series ss ON ss.id = sr.series_id
                 WHERE ss.metric = 'sashiko_prior_feedback_total' AND sr.resolution_secs = 86400",
                (),
            )
            .await?;
        let pf_sum2: f64 = r2.next().await?.unwrap().get(0)?;
        assert!((pf_sum2 - 1.0).abs() < f64::EPSILON);

        let dirty_patches = sum_rollup_86400(
            &db,
            "sashiko_patches_reviewed_total",
            r#"{"clean":"false"}"#,
        )
        .await?;
        assert!((dirty_patches - 1.0).abs() < f64::EPSILON);

        Ok(())
    }

    async fn sum_rollup_86400(db: &Database, metric: &str, labels_json: &str) -> Result<f64> {
        let mut r = db
            .read_conn()
            .query(
                "SELECT COALESCE(SUM(sr.value), 0)
                 FROM stats_rollups sr
                 JOIN stats_series ss ON ss.id = sr.series_id
                 WHERE ss.metric = ? AND ss.labels_json = ? AND sr.resolution_secs = 86400",
                libsql::params![metric, labels_json],
            )
            .await?;
        let val = match r.next().await? {
            Some(row) => row.get::<f64>(0)?,
            None => 0.0,
        };
        Ok(val)
    }

    #[tokio::test]
    async fn test_backfill_guards_prevent_double_counts_with_live_updates() -> Result<()> {
        let db = Database::new(&DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await?;
        db.migrate().await?;
        db.execute("PRAGMA user_version = 23", ()).await?;
        db.execute("DELETE FROM meta WHERE key LIKE 'stats_backfill:%'", ())
            .await?;

        let now = chrono::Utc::now().timestamp() - 300;
        db.execute(
            "INSERT INTO subsystems (id, name, mailing_list_address) VALUES
             (1, 'mm', 'linux-mm@kvack.org'),
             (2, 'fs', 'linux-fsdevel@vger.kernel.org')",
            (),
        )
        .await?;
        // Pre-upgrade embargoed Reviewed patchset (id=1) and mailing-list Failed To Apply (id=2).
        db.execute(
            "INSERT INTO patchsets (id, subject, status, date, version, embargo_until) VALUES
             (1, '[PATCH] mm: fix', 'Reviewed', ?, 1, ?),
             (2, '[PATCH] fs: broken', 'Failed To Apply', ?, 1, ?)",
            libsql::params![now, now + 3600, now, now - 10],
        )
        .await?;
        db.execute(
            "INSERT INTO bugs (id, bugid, title, lifecycle_status, pipeline_state, reporter, reported_at, created_at, updated_at) VALUES
             (1, 'linux-new1', 'unverified candidate', 'new', 'running', 'bot', ?, ?, ?),
             (2, 'linux-open1', 'pre-upgrade open', 'open', 'succeeded', 'bot', ?, ?, ?)",
            libsql::params![now, now, now, now, now, now],
        )
        .await?;
        // Pre-upgrade email_outbox rows: id=1 Sent, id=2 Pending (skipped by backfill),
        // id=3 Failed, id=4 Disabled (-> skipped), id=5 Dry-Run (-> dry_run).
        db.execute(
            "INSERT INTO email_outbox (id, status, created_at) VALUES
             (1, 'Sent', ?),
             (2, 'Pending', ?),
             (3, 'Failed', ?),
             (4, 'Disabled', ?),
             (5, 'Dry-Run', ?)",
            libsql::params![now, now, now, now, now],
        )
        .await?;

        db.migrate().await?;

        // Mailing-list Failed To Apply with expired embargo_until <= now must be releasable.
        let releasable = db.get_releasable_embargoed_patchsets(now, 10).await?;
        assert!(releasable.iter().any(|p| p.id == 2));

        // Clear patchset 1 embargo BEFORE backfill runs, and transition bug 2 open->fixed BEFORE backfill.
        db.clear_patchset_embargo(1).await?;
        db.execute(
            "UPDATE bugs SET lifecycle_status = 'fixed', updated_at = ? WHERE id = 2",
            libsql::params![now + 10],
        )
        .await?;
        // Transition email 2 Pending->Sent BEFORE backfill runs; skip_email_outbox guard prevents backfill double-count.
        db.execute("UPDATE email_outbox SET status = 'Sent' WHERE id = 2", ())
            .await?;

        run_stats_backfill_once(&db).await?;

        // Only pre-upgrade terminal Sent (id=1) and Failed (id=3) are backfilled; id=2 was Pending at upgrade.
        let email_sent = sum_rollup_86400(
            &db,
            "sashiko_outbox_events_total",
            r#"{"channel":"email","outcome":"sent"}"#,
        )
        .await?;
        let email_failed = sum_rollup_86400(
            &db,
            "sashiko_outbox_events_total",
            r#"{"channel":"email","outcome":"failed"}"#,
        )
        .await?;
        let email_skipped = sum_rollup_86400(
            &db,
            "sashiko_outbox_events_total",
            r#"{"channel":"email","outcome":"skipped"}"#,
        )
        .await?;
        let email_dry_run = sum_rollup_86400(
            &db,
            "sashiko_outbox_events_total",
            r#"{"channel":"email","outcome":"dry_run"}"#,
        )
        .await?;
        assert!((email_sent - 1.0).abs() < f64::EPSILON);
        assert!((email_failed - 1.0).abs() < f64::EPSILON);
        assert!((email_skipped - 1.0).abs() < f64::EPSILON);
        assert!((email_dry_run - 1.0).abs() < f64::EPSILON);

        // Patchset 1 completion must be counted ONCE (not twice).
        let ps1_done = sum_rollup_86400(
            &db,
            "sashiko_patchsets_completed_total",
            r#"{"clean":"true","status":"Reviewed"}"#,
        )
        .await?;
        assert!((ps1_done - 1.0).abs() < f64::EPSILON);

        // Bug 1 ('new') must NOT be counted as discovered; Bug 2 must have 1 discovered + 1 fixed.
        let disc = sum_rollup_86400(
            &db,
            "sashiko_bugs_events_total",
            r#"{"event":"discovered"}"#,
        )
        .await?;
        let fixed =
            sum_rollup_86400(&db, "sashiko_bugs_events_total", r#"{"event":"fixed"}"#).await?;
        assert!((disc - 1.0).abs() < f64::EPSILON);
        assert!((fixed - 1.0).abs() < f64::EPSILON);

        // Multi-part post-upgrade patchset (id=3) linking two subsystems sequentially records both.
        db.execute(
            "INSERT INTO patchsets (id, subject, status, date, version) VALUES (3, '[PATCH 0/2] both', 'Pending', ?, 1)",
            libsql::params![now + 20],
        )
        .await?;
        db.record_patchset_ingested_rollup(3).await?;
        db.add_subsystem_to_patchset(3, 1).await?;
        db.add_subsystem_to_patchset(3, 2).await?;
        let sub_mm = sum_rollup_86400(
            &db,
            "sashiko_subsystem_patchsets_total",
            r#"{"subsystem":"mm"}"#,
        )
        .await?;
        let sub_fs = sum_rollup_86400(
            &db,
            "sashiko_subsystem_patchsets_total",
            r#"{"subsystem":"fs"}"#,
        )
        .await?;
        assert!((sub_mm - 1.0).abs() < f64::EPSILON);
        assert!((sub_fs - 1.0).abs() < f64::EPSILON);

        Ok(())
    }

    #[tokio::test]
    async fn test_clean_patches_backfill_handles_post_cutoff_and_no_double_count() -> Result<()> {
        let db = Database::new(&DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await?;
        db.migrate().await?;
        db.execute("PRAGMA user_version = 23", ()).await?;
        db.execute("DELETE FROM meta WHERE key LIKE 'stats_backfill:%'", ())
            .await?;

        let now = chrono::Utc::now().timestamp() - 300;
        db.execute(
            "INSERT INTO messages (id, message_id, subject, date) VALUES
             (1, '<p1@ex>', '[PATCH] pre-cutoff embargo', ?),
             (2, '<p2@ex>', '[PATCH] post-cutoff', ?)",
            libsql::params![now, now],
        )
        .await?;
        // Pre-cutoff embargoed patchset (id=1) with 1 clean patch.
        db.execute(
            "INSERT INTO patchsets (id, subject, status, date, version, embargo_until)
             VALUES (1, '[PATCH] pre-cutoff embargo', 'Reviewed', ?, 1, ?)",
            libsql::params![now, now + 3600],
        )
        .await?;
        db.execute(
            "INSERT INTO patches (id, patchset_id, message_id, part_index) VALUES (1, 1, '<p1@ex>', 1)",
            (),
        )
        .await?;
        db.execute(
            "INSERT INTO reviews (id, patchset_id, patch_id, status, created_at)
             VALUES (1, 1, 1, 'Reviewed', ?)",
            libsql::params![now],
        )
        .await?;

        db.migrate().await?;

        // Simulate older binary finishing v1 backfill (main_done=1, clean_patches_done unset)
        // AND a post-cutoff patchset (id=2) completing on the older binary (ps_done=1, no ps_patches_done).
        db.execute(
            "INSERT INTO meta (key, value) VALUES
             ('stats_backfill:v1:done', '1'),
             ('stats_backfill:v1:cursor:email_outbox', '0'),
             ('stats_rollup:ps_done:2', '1')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (),
        )
        .await?;
        db.execute(
            "INSERT INTO patchsets (id, subject, status, date, version)
             VALUES (2, '[PATCH] post-cutoff', 'Reviewed', ?, 1)",
            libsql::params![now],
        )
        .await?;
        db.execute(
            "INSERT INTO patches (id, patchset_id, message_id, part_index) VALUES (2, 2, '<p2@ex>', 1)",
            (),
        )
        .await?;
        db.execute(
            "INSERT INTO reviews (id, patchset_id, patch_id, status, created_at)
             VALUES (2, 2, 2, 'Reviewed', ?)",
            libsql::params![now],
        )
        .await?;

        // Release embargo on patchset 1 under new binary BEFORE incremental clean_patches backfill runs.
        db.clear_patchset_embargo(1).await?;

        // Incremental backfill must count post-cutoff patchset 2 and NOT double-count patchset 1.
        run_stats_backfill_once(&db).await?;
        let clean_cnt =
            sum_rollup_86400(&db, "sashiko_patches_reviewed_total", r#"{"clean":"true"}"#).await?;
        assert!(
            (clean_cnt - 2.0).abs() < f64::EPSILON,
            "expected exactly 2 clean patches (ps 1 + ps 2), got {clean_cnt}"
        );

        // Re-running backfill and release must stay at 2 (strictly idempotent).
        db.record_patchset_release_rollups(1).await?;
        db.record_patchset_release_rollups(2).await?;
        run_stats_backfill_once(&db).await?;
        let clean_cnt2 =
            sum_rollup_86400(&db, "sashiko_patches_reviewed_total", r#"{"clean":"true"}"#).await?;
        assert!((clean_cnt2 - 2.0).abs() < f64::EPSILON);

        Ok(())
    }
}
