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

//! O(1) statistics queries, SQLite trigger/seed installation for Migration 023,
//! `BugAccess`-scoped subsystem facet lookups, and multi-resolution rollup I/O.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{BugLifecycleStatus, BugVisibility, Database, DatabaseTransaction};
use crate::metrics::RollupSample;

/// Bounded chunk size for `IN (...)` subsystem queries so large principal
/// scopes never approach SQLite's bind-parameter limit.
const SUB_BATCH_SIZE: usize = 100;

/// Supported rollup bucket resolutions in seconds:
/// - `300` (5m): retained for 48 hours (`172_800`s)
/// - `3600` (1h): retained for 45 days (`3_888_000`s)
/// - `86400` (1d): retained for 5 years (`157_680_000`s)
pub const ROLLUP_RESOLUTIONS: [(i64, i64); 3] = [
    (300, 48 * 3600),
    (3600, 45 * 86_400),
    (86_400, 5 * 365 * 86_400),
];

/// Pre-aggregated per-subsystem bug counts across severity and CVSS v4.0
/// security rating dimensions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BugSubsystemFacetSummary {
    pub subsystem: String,
    pub total: u64,
    pub open: u64,
    pub fixed: u64,
    pub duplicate: u64,
    pub dismissed: u64,
    pub new_untriaged: u64,
    pub closed: u64,
    pub sev_critical: u64,
    pub sev_high: u64,
    pub sev_medium: u64,
    pub sev_low: u64,
    pub sec_critical: u64,
    pub sec_high: u64,
    pub sec_medium: u64,
    pub sec_low: u64,
    pub sec_hardening: u64,
    pub open_sev_critical: u64,
    pub open_sev_high: u64,
    pub open_sec_critical: u64,
    pub open_sec_high: u64,
}

#[derive(Debug, Clone)]
pub struct RollupBucketRow {
    pub metric: String,
    pub labels: BTreeMap<String, String>,
    pub bucket_ts: i64,
    pub value: f64,
}

/// Encodes label pairs as compact JSON with keys sorted alphabetically,
/// matching SQLite `json_object(...)` called with alphabetically ordered keys.
pub fn canonical_labels_json(labels: &[(&str, &str)]) -> String {
    if labels.is_empty() {
        return "{}".to_string();
    }
    let mut map = BTreeMap::new();
    for (k, v) in labels {
        map.insert(*k, *v);
    }
    serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string())
}

/// Decodes a canonical `labels_json` object into a sorted label map.
pub fn decode_labels_json(raw: &str) -> BTreeMap<String, String> {
    serde_json::from_str::<BTreeMap<String, String>>(raw).unwrap_or_default()
}

async fn table_exists(tx: &DatabaseTransaction, table_name: &str) -> Result<bool> {
    let mut rows = tx
        .query(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?",
            libsql::params![table_name],
        )
        .await?;
    let exists = rows.next().await?.is_some();
    drop(rows);
    Ok(exists)
}

async fn column_exists(tx: &DatabaseTransaction, table: &str, column: &str) -> Result<bool> {
    let mut rows = tx.query(&format!("PRAGMA table_info({table})"), ()).await?;
    let mut found = false;
    while let Some(row) = rows.next().await? {
        let name: String = row.get(1)?;
        if name == column {
            found = true;
        }
    }
    drop(rows);
    Ok(found)
}

const BUG_SEVERITY_SQL: &str = "CASE {alias}.severity_int
    WHEN 4 THEN 'critical'
    WHEN 3 THEN 'high'
    WHEN 2 THEN 'medium'
    WHEN 1 THEN 'low'
    ELSE 'unknown'
END";

const BUG_SEC_RATING_SQL: &str = "CASE {alias}.security_class
    WHEN 'vulnerability' THEN CASE
        WHEN COALESCE({alias}.security_score_tenths, 0) >= 90 THEN 'critical'
        WHEN COALESCE({alias}.security_score_tenths, 0) >= 70 THEN 'high'
        WHEN COALESCE({alias}.security_score_tenths, 0) >= 40 THEN 'medium'
        WHEN COALESCE({alias}.security_score_tenths, 0) >= 1 THEN 'low'
        ELSE 'none'
    END
    WHEN 'hardening' THEN 'hardening'
    WHEN 'not_security' THEN 'none'
    ELSE 'pending'
END";

fn bug_labels_expr(alias: &str) -> String {
    let sev = BUG_SEVERITY_SQL.replace("{alias}", alias);
    let sec_rating = BUG_SEC_RATING_SQL.replace("{alias}", alias);
    format!(
        "json_object(
            'pipeline', {alias}.pipeline_state,
            'security_class', COALESCE({alias}.security_class, 'pending'),
            'security_rating', {sec_rating},
            'severity', {sev},
            'status', {alias}.lifecycle_status
        )"
    )
}

fn bug_sec_rating_expr(alias: &str) -> String {
    BUG_SEC_RATING_SQL.replace("{alias}", alias)
}

/// Drops any pre-existing `023` source triggers, seeds live gauges from narrow
/// covering indexes, records high-water marks in `meta` for the asynchronous
/// historical rollup backfill, and installs incremental triggers on all
/// present source tables. Safe and idempotent on partial-schema test databases
/// and `PRAGMA user_version` rewinds.
pub(super) async fn apply_migration_24_seeds_and_triggers(tx: &DatabaseTransaction) -> Result<()> {
    let has_messages = table_exists(tx, "messages").await?;
    let has_patchsets = table_exists(tx, "patchsets").await?;
    let has_patches = table_exists(tx, "patches").await?;
    let has_reviews = table_exists(tx, "reviews").await?;
    let has_reviews_inherited =
        has_reviews && column_exists(tx, "reviews", "inherited_from_review_id").await?;
    let has_email_outbox = table_exists(tx, "email_outbox").await?;
    let has_forge_outbox = table_exists(tx, "forge_outbox").await?;
    let has_patchwork_outbox = table_exists(tx, "patchwork_outbox").await?;
    let has_fetch_queue = table_exists(tx, "fetch_queue").await?;
    let has_bugs = table_exists(tx, "bugs").await?;
    let has_bug_subsystems = table_exists(tx, "bug_subsystems").await?;
    let has_bug_upstream = table_exists(tx, "bug_upstream_sections").await?;
    let has_bug_enrichments = table_exists(tx, "bug_enrichments").await?;
    let has_prior_resolutions = table_exists(tx, "prior_feedback_resolutions").await?;
    let has_meta = table_exists(tx, "meta").await?;

    drop_source_triggers(tx).await?;

    // Reset live projection tables before deterministic seeding so rewound
    // migrations never double-count existing rows.
    tx.execute("DELETE FROM stats_state_gauges", ()).await?;
    tx.execute("DELETE FROM stats_bug_section_links", ())
        .await?;
    tx.execute("DELETE FROM stats_bug_subsystem_facets", ())
        .await?;

    if has_reviews {
        tx.execute(
            "CREATE INDEX IF NOT EXISTS idx_reviews_active_status_patch
             ON reviews(status, patch_id)
             WHERE status IN ('Pending', 'In Review', 'Applying')",
            (),
        )
        .await?;
    }

    seed_core_gauges(
        tx,
        has_messages,
        has_patchsets,
        has_reviews,
        has_reviews_inherited,
    )
    .await?;
    seed_queue_gauges(
        tx,
        has_email_outbox,
        has_forge_outbox,
        has_patchwork_outbox,
        has_fetch_queue,
    )
    .await?;
    if has_bugs {
        seed_bug_gauges_and_facets(
            tx,
            has_bug_subsystems,
            has_bug_upstream,
            has_bug_enrichments,
        )
        .await?;
    }

    if has_meta {
        record_backfill_cutoffs(
            tx,
            &[
                ("messages", has_messages),
                ("patchsets", has_patchsets),
                ("patches", has_patches),
                ("reviews", has_reviews),
                ("prior_feedback_resolutions", has_prior_resolutions),
                ("bugs", has_bugs),
                ("bug_enrichments", has_bug_enrichments),
                ("email_outbox", has_email_outbox),
                ("forge_outbox", has_forge_outbox),
                ("patchwork_outbox", has_patchwork_outbox),
            ],
        )
        .await?;
        if has_reviews {
            tx.execute(
                "INSERT OR IGNORE INTO meta (key, value)
                 SELECT 'stats_backfill:skip_review:' || id, '1'
                 FROM reviews
                 WHERE status IN ('Pending', 'In Review', 'Applying')",
                (),
            )
            .await?;
        }
    }

    install_core_triggers(
        tx,
        has_messages,
        has_patchsets,
        has_reviews,
        has_reviews_inherited,
    )
    .await?;
    install_queue_triggers(
        tx,
        has_email_outbox,
        has_forge_outbox,
        has_patchwork_outbox,
        has_fetch_queue,
    )
    .await?;
    if has_bugs {
        install_bug_triggers(
            tx,
            has_bug_subsystems,
            has_bug_upstream,
            has_bug_enrichments,
        )
        .await?;
    }

    Ok(())
}

async fn drop_source_triggers(tx: &DatabaseTransaction) -> Result<()> {
    for trig in [
        "trg_stats_messages_insert",
        "trg_stats_messages_delete",
        "trg_stats_patchsets_insert",
        "trg_stats_patchsets_update_status",
        "trg_stats_patchsets_delete",
        "trg_stats_reviews_insert",
        "trg_stats_reviews_update",
        "trg_stats_reviews_reparent",
        "trg_stats_reviews_delete",
        "trg_stats_email_outbox_insert",
        "trg_stats_email_outbox_update",
        "trg_stats_email_outbox_delete",
        "trg_stats_forge_outbox_insert",
        "trg_stats_forge_outbox_update",
        "trg_stats_forge_outbox_delete",
        "trg_stats_patchwork_outbox_insert",
        "trg_stats_patchwork_outbox_update",
        "trg_stats_patchwork_outbox_delete",
        "trg_stats_fetch_queue_insert",
        "trg_stats_fetch_queue_update",
        "trg_stats_fetch_queue_delete",
        "trg_stats_bugs_insert",
        "trg_stats_bugs_update",
        "trg_stats_bugs_rollup_lifecycle",
        "trg_stats_bugs_before_delete",
        "trg_stats_bug_subsystems_insert",
        "trg_stats_bug_subsystems_update",
        "trg_stats_bug_subsystems_delete",
        "trg_stats_bug_upstream_insert",
        "trg_stats_bug_upstream_delete",
        "trg_stats_bug_enrichments_repro_insert",
        "trg_stats_bug_enrichments_repro_delete",
        "trg_stats_bug_enrichments_fix_insert",
        "trg_stats_bug_enrichments_fix_delete",
    ] {
        tx.execute(&format!("DROP TRIGGER IF EXISTS {trig}"), ())
            .await?;
    }
    Ok(())
}

async fn seed_core_gauges(
    tx: &DatabaseTransaction,
    has_messages: bool,
    has_patchsets: bool,
    has_reviews: bool,
    has_reviews_inherited: bool,
) -> Result<()> {
    if has_messages {
        tx.execute(
            "INSERT INTO stats_state_gauges (metric, labels_json, value)
             SELECT 'sashiko_messages_stored', '{}', COUNT(*) FROM messages",
            (),
        )
        .await?;
    }
    if has_patchsets {
        tx.execute(
            "INSERT INTO stats_state_gauges (metric, labels_json, value)
             SELECT 'sashiko_patchsets_stored', json_object('status', COALESCE(status, 'Incomplete')), COUNT(*)
             FROM patchsets
             GROUP BY COALESCE(status, 'Incomplete')",
            (),
        )
        .await?;
    }
    if has_reviews {
        tx.execute(
            "INSERT INTO stats_state_gauges (metric, labels_json, value)
             SELECT 'sashiko_reviews_stored', json_object('status', COALESCE(status, 'Pending')), COUNT(*)
             FROM reviews
             GROUP BY COALESCE(status, 'Pending')",
            (),
        )
        .await?;
    }
    if has_reviews_inherited {
        tx.execute(
            "INSERT INTO stats_state_gauges (metric, labels_json, value)
             SELECT 'sashiko_reviews_inherited_stored', '{}', COUNT(*)
             FROM reviews
             WHERE inherited_from_review_id IS NOT NULL",
            (),
        )
        .await?;
    }
    Ok(())
}

async fn seed_queue_gauges(
    tx: &DatabaseTransaction,
    has_email_outbox: bool,
    has_forge_outbox: bool,
    has_patchwork_outbox: bool,
    has_fetch_queue: bool,
) -> Result<()> {
    for (present, table, channel) in [
        (has_email_outbox, "email_outbox", "email"),
        (has_forge_outbox, "forge_outbox", "forge"),
        (has_patchwork_outbox, "patchwork_outbox", "patchwork"),
    ] {
        if present {
            tx.execute(
                &format!(
                    "INSERT INTO stats_state_gauges (metric, labels_json, value)
                     SELECT 'sashiko_outbox_depth',
                            json_object('channel', '{channel}', 'status', COALESCE(status, 'Pending')),
                            COUNT(*)
                     FROM {table}
                     GROUP BY COALESCE(status, 'Pending')"
                ),
                (),
            )
            .await?;
        }
    }
    if has_fetch_queue {
        tx.execute(
            "INSERT INTO stats_state_gauges (metric, labels_json, value)
             SELECT 'sashiko_fetch_queue_depth',
                    json_object('status', COALESCE(status, 'Pending')),
                    COUNT(*)
             FROM fetch_queue
             GROUP BY COALESCE(status, 'Pending')",
            (),
        )
        .await?;
    }
    Ok(())
}

async fn seed_bug_gauges_and_facets(
    tx: &DatabaseTransaction,
    has_bug_subsystems: bool,
    has_bug_upstream: bool,
    has_bug_enrichments: bool,
) -> Result<()> {
    let labels = bug_labels_expr("b");
    tx.execute(
        &format!(
            "INSERT INTO stats_state_gauges (metric, labels_json, value)
             SELECT 'sashiko_bugs_total', {labels}, COUNT(*)
             FROM bugs b
             GROUP BY {labels}"
        ),
        (),
    )
    .await?;

    if has_bug_enrichments {
        tx.execute(
            "INSERT INTO stats_state_gauges (metric, labels_json, value)
             SELECT 'sashiko_bugs_with_attachment',
                    json_object('attachment', 'reproducer', 'status', b.lifecycle_status),
                    COUNT(DISTINCT b.id)
             FROM bugs b
             JOIN bug_enrichments e ON e.bug_id = b.id
             WHERE e.kind IN ('reproducer', 'testcase', 'poc')
             GROUP BY b.lifecycle_status",
            (),
        )
        .await?;
        tx.execute(
            "INSERT INTO stats_state_gauges (metric, labels_json, value)
             SELECT 'sashiko_bugs_with_attachment',
                    json_object('attachment', 'proposed_fix', 'status', b.lifecycle_status),
                    COUNT(DISTINCT b.id)
             FROM bugs b
             JOIN bug_enrichments e ON e.bug_id = b.id
             WHERE e.kind IN ('proposed_fix', 'suggested_fix')
             GROUP BY b.lifecycle_status",
            (),
        )
        .await?;
    }

    let sec = bug_sec_rating_expr("b");
    if has_bug_subsystems {
        // Inserting into stats_bug_section_links fires trg_stats_bug_links_insert,
        // populating stats_bug_subsystem_facets in lockstep.
        tx.execute(
            &format!(
                "INSERT OR IGNORE INTO stats_bug_section_links (
                    bug_id, subsystem, is_authorizing, lifecycle_status, severity_int, security_rating
                )
                SELECT b.id,
                       bs.subsystem,
                       CASE WHEN bs.source = 'maintainers_section' THEN 1 ELSE 0 END,
                       b.lifecycle_status,
                       b.severity_int,
                       {sec}
                FROM bug_subsystems bs
                JOIN bugs b ON b.id = bs.bug_id
                WHERE bs.source IN ('maintainers_section', 'path_prefix')
                  AND length(trim(bs.subsystem)) > 0"
            ),
            (),
        )
        .await?;
    }
    if has_bug_upstream {
        tx.execute(
            &format!(
                "INSERT OR IGNORE INTO stats_bug_section_links (
                    bug_id, subsystem, is_authorizing, lifecycle_status, severity_int, security_rating
                )
                SELECT b.id,
                       u.subsystem,
                       1,
                       b.lifecycle_status,
                       b.severity_int,
                       {sec}
                FROM bug_upstream_sections u
                JOIN bugs b ON b.id = u.bug_id
                WHERE length(trim(u.subsystem)) > 0"
            ),
            (),
        )
        .await?;
    }
    Ok(())
}

async fn record_backfill_cutoffs(tx: &DatabaseTransaction, tables: &[(&str, bool)]) -> Result<()> {
    for (table, present) in tables {
        let cutoff_key = format!("stats_backfill:cutoff:{table}");
        let cursor_key = format!("stats_backfill:v1:cursor:{table}");
        if *present {
            for key in [&cutoff_key, &cursor_key] {
                tx.execute(
                    &format!(
                        "INSERT OR IGNORE INTO meta (key, value)
                         VALUES (?, CAST(COALESCE((SELECT MAX(id) FROM {table}), 0) AS TEXT))"
                    ),
                    libsql::params![key.as_str()],
                )
                .await?;
            }
        } else {
            for key in [&cutoff_key, &cursor_key] {
                tx.execute(
                    "INSERT OR IGNORE INTO meta (key, value) VALUES (?, '0')",
                    libsql::params![key.as_str()],
                )
                .await?;
            }
        }
    }
    Ok(())
}

async fn install_core_triggers(
    tx: &DatabaseTransaction,
    has_messages: bool,
    has_patchsets: bool,
    has_reviews: bool,
    has_reviews_inherited: bool,
) -> Result<()> {
    if has_messages {
        tx.execute(
            "CREATE TRIGGER trg_stats_messages_insert
             AFTER INSERT ON messages
             FOR EACH ROW
             BEGIN
                 INSERT INTO stats_state_gauges (metric, labels_json, value)
                 VALUES ('sashiko_messages_stored', '{}', 1)
                 ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;
             END",
            (),
        )
        .await?;
        tx.execute(
            "CREATE TRIGGER trg_stats_messages_delete
             AFTER DELETE ON messages
             FOR EACH ROW
             BEGIN
                 UPDATE stats_state_gauges
                 SET value = MAX(0, value - 1)
                 WHERE metric = 'sashiko_messages_stored' AND labels_json = '{}';
             END",
            (),
        )
        .await?;
    }

    if has_patchsets {
        tx.execute(
            "CREATE TRIGGER trg_stats_patchsets_insert
             AFTER INSERT ON patchsets
             FOR EACH ROW
             BEGIN
                 INSERT INTO stats_state_gauges (metric, labels_json, value)
                 VALUES ('sashiko_patchsets_stored', json_object('status', COALESCE(new.status, 'Incomplete')), 1)
                 ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;
             END",
            (),
        )
        .await?;
        tx.execute(
            "CREATE TRIGGER trg_stats_patchsets_update_status
             AFTER UPDATE OF status ON patchsets
             FOR EACH ROW
             WHEN COALESCE(old.status, 'Incomplete') != COALESCE(new.status, 'Incomplete')
             BEGIN
                 UPDATE stats_state_gauges
                 SET value = MAX(0, value - 1)
                 WHERE metric = 'sashiko_patchsets_stored'
                   AND labels_json = json_object('status', COALESCE(old.status, 'Incomplete'));
                 INSERT INTO stats_state_gauges (metric, labels_json, value)
                 VALUES ('sashiko_patchsets_stored', json_object('status', COALESCE(new.status, 'Incomplete')), 1)
                 ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;
             END",
            (),
        )
        .await?;
        tx.execute(
            "CREATE TRIGGER trg_stats_patchsets_delete
             AFTER DELETE ON patchsets
             FOR EACH ROW
             BEGIN
                 UPDATE stats_state_gauges
                 SET value = MAX(0, value - 1)
                 WHERE metric = 'sashiko_patchsets_stored'
                   AND labels_json = json_object('status', COALESCE(old.status, 'Incomplete'));
             END",
            (),
        )
        .await?;
    }

    if has_reviews {
        let inh_ins = if has_reviews_inherited {
            "INSERT INTO stats_state_gauges (metric, labels_json, value)
             SELECT 'sashiko_reviews_inherited_stored', '{}', 1
             WHERE new.inherited_from_review_id IS NOT NULL
             ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;"
        } else {
            ""
        };
        tx.execute(
            &format!(
                "CREATE TRIGGER trg_stats_reviews_insert
                 AFTER INSERT ON reviews
                 FOR EACH ROW
                 BEGIN
                     INSERT INTO stats_state_gauges (metric, labels_json, value)
                     VALUES ('sashiko_reviews_stored', json_object('status', COALESCE(new.status, 'Pending')), 1)
                     ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;
                     {inh_ins}
                 END"
            ),
            (),
        )
        .await?;

        if has_reviews_inherited {
            tx.execute(
                "CREATE TRIGGER trg_stats_reviews_update
                 AFTER UPDATE OF status, inherited_from_review_id ON reviews
                 FOR EACH ROW
                 WHEN COALESCE(old.status, 'Pending') != COALESCE(new.status, 'Pending')
                   OR (old.inherited_from_review_id IS NOT NULL) != (new.inherited_from_review_id IS NOT NULL)
                 BEGIN
                     UPDATE stats_state_gauges
                     SET value = MAX(0, value - 1)
                     WHERE COALESCE(old.status, 'Pending') != COALESCE(new.status, 'Pending')
                       AND metric = 'sashiko_reviews_stored'
                       AND labels_json = json_object('status', COALESCE(old.status, 'Pending'));
                     INSERT INTO stats_state_gauges (metric, labels_json, value)
                     SELECT 'sashiko_reviews_stored', json_object('status', COALESCE(new.status, 'Pending')), 1
                     WHERE COALESCE(old.status, 'Pending') != COALESCE(new.status, 'Pending')
                     ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;

                     UPDATE stats_state_gauges
                     SET value = MAX(0, value - 1)
                     WHERE old.inherited_from_review_id IS NOT NULL
                       AND new.inherited_from_review_id IS NULL
                       AND metric = 'sashiko_reviews_inherited_stored'
                       AND labels_json = '{}';
                     INSERT INTO stats_state_gauges (metric, labels_json, value)
                     SELECT 'sashiko_reviews_inherited_stored', '{}', 1
                     WHERE old.inherited_from_review_id IS NULL
                       AND new.inherited_from_review_id IS NOT NULL
                     ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;
                 END",
                (),
            )
            .await?;
        } else {
            tx.execute(
                "CREATE TRIGGER trg_stats_reviews_update
                 AFTER UPDATE OF status ON reviews
                 FOR EACH ROW
                 WHEN COALESCE(old.status, 'Pending') != COALESCE(new.status, 'Pending')
                 BEGIN
                     UPDATE stats_state_gauges
                     SET value = MAX(0, value - 1)
                     WHERE metric = 'sashiko_reviews_stored'
                       AND labels_json = json_object('status', COALESCE(old.status, 'Pending'));
                     INSERT INTO stats_state_gauges (metric, labels_json, value)
                     VALUES ('sashiko_reviews_stored', json_object('status', COALESCE(new.status, 'Pending')), 1)
                     ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;
                 END",
                (),
            )
            .await?;
        }

        let inh_del = if has_reviews_inherited {
            "UPDATE stats_state_gauges
             SET value = MAX(0, value - 1)
             WHERE old.inherited_from_review_id IS NOT NULL
               AND metric = 'sashiko_reviews_inherited_stored'
               AND labels_json = '{}';"
        } else {
            ""
        };
        tx.execute(
            &format!(
                "CREATE TRIGGER trg_stats_reviews_delete
                 AFTER DELETE ON reviews
                 FOR EACH ROW
                 BEGIN
                     UPDATE stats_state_gauges
                     SET value = MAX(0, value - 1)
                     WHERE metric = 'sashiko_reviews_stored'
                       AND labels_json = json_object('status', COALESCE(old.status, 'Pending'));
                     {inh_del}
                 END"
            ),
            (),
        )
        .await?;

        tx.execute(
            "CREATE TRIGGER trg_stats_reviews_reparent
             AFTER UPDATE OF patchset_id ON reviews
             FOR EACH ROW
             WHEN old.patchset_id IS NOT new.patchset_id
             BEGIN
                 UPDATE review_pipeline_stats
                 SET patchset_id = new.patchset_id
                 WHERE review_id = new.id;
             END",
            (),
        )
        .await?;
    }
    Ok(())
}

async fn install_queue_triggers(
    tx: &DatabaseTransaction,
    has_email_outbox: bool,
    has_forge_outbox: bool,
    has_patchwork_outbox: bool,
    has_fetch_queue: bool,
) -> Result<()> {
    for (present, table, prefix, channel) in [
        (has_email_outbox, "email_outbox", "email_outbox", "email"),
        (has_forge_outbox, "forge_outbox", "forge_outbox", "forge"),
        (
            has_patchwork_outbox,
            "patchwork_outbox",
            "patchwork_outbox",
            "patchwork",
        ),
    ] {
        if !present {
            continue;
        }
        tx.execute(
            &format!(
                "CREATE TRIGGER trg_stats_{prefix}_insert
                 AFTER INSERT ON {table}
                 FOR EACH ROW
                 BEGIN
                     INSERT INTO stats_state_gauges (metric, labels_json, value)
                     VALUES ('sashiko_outbox_depth', json_object('channel', '{channel}', 'status', COALESCE(new.status, 'Pending')), 1)
                     ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;
                 END"
            ),
            (),
        )
        .await?;
        tx.execute(
            &format!(
                "CREATE TRIGGER trg_stats_{prefix}_update
                 AFTER UPDATE OF status ON {table}
                 FOR EACH ROW
                 WHEN COALESCE(old.status, 'Pending') != COALESCE(new.status, 'Pending')
                 BEGIN
                     UPDATE stats_state_gauges
                     SET value = MAX(0, value - 1)
                     WHERE metric = 'sashiko_outbox_depth'
                       AND labels_json = json_object('channel', '{channel}', 'status', COALESCE(old.status, 'Pending'));
                     INSERT INTO stats_state_gauges (metric, labels_json, value)
                     VALUES ('sashiko_outbox_depth', json_object('channel', '{channel}', 'status', COALESCE(new.status, 'Pending')), 1)
                     ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;
                 END"
            ),
            (),
        )
        .await?;
        tx.execute(
            &format!(
                "CREATE TRIGGER trg_stats_{prefix}_delete
                 AFTER DELETE ON {table}
                 FOR EACH ROW
                 BEGIN
                     UPDATE stats_state_gauges
                     SET value = MAX(0, value - 1)
                     WHERE metric = 'sashiko_outbox_depth'
                       AND labels_json = json_object('channel', '{channel}', 'status', COALESCE(old.status, 'Pending'));
                 END"
            ),
            (),
        )
        .await?;
    }

    if has_fetch_queue {
        tx.execute(
            "CREATE TRIGGER trg_stats_fetch_queue_insert
             AFTER INSERT ON fetch_queue
             FOR EACH ROW
             BEGIN
                 INSERT INTO stats_state_gauges (metric, labels_json, value)
                 VALUES ('sashiko_fetch_queue_depth', json_object('status', COALESCE(new.status, 'Pending')), 1)
                 ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;
             END",
            (),
        )
        .await?;
        tx.execute(
            "CREATE TRIGGER trg_stats_fetch_queue_update
             AFTER UPDATE OF status ON fetch_queue
             FOR EACH ROW
             WHEN COALESCE(old.status, 'Pending') != COALESCE(new.status, 'Pending')
             BEGIN
                 UPDATE stats_state_gauges
                 SET value = MAX(0, value - 1)
                 WHERE metric = 'sashiko_fetch_queue_depth'
                   AND labels_json = json_object('status', COALESCE(old.status, 'Pending'));
                 INSERT INTO stats_state_gauges (metric, labels_json, value)
                 VALUES ('sashiko_fetch_queue_depth', json_object('status', COALESCE(new.status, 'Pending')), 1)
                 ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;
             END",
            (),
        )
        .await?;
        tx.execute(
            "CREATE TRIGGER trg_stats_fetch_queue_delete
             AFTER DELETE ON fetch_queue
             FOR EACH ROW
             BEGIN
                 UPDATE stats_state_gauges
                 SET value = MAX(0, value - 1)
                 WHERE metric = 'sashiko_fetch_queue_depth'
                   AND labels_json = json_object('status', COALESCE(old.status, 'Pending'));
             END",
            (),
        )
        .await?;
    }
    Ok(())
}

async fn install_bug_triggers(
    tx: &DatabaseTransaction,
    has_bug_subsystems: bool,
    has_bug_upstream: bool,
    has_bug_enrichments: bool,
) -> Result<()> {
    let old_labels = bug_labels_expr("old");
    let new_labels = bug_labels_expr("new");
    let new_sec = bug_sec_rating_expr("new");

    tx.execute(
        &format!(
            "CREATE TRIGGER trg_stats_bugs_insert
             AFTER INSERT ON bugs
             FOR EACH ROW
             BEGIN
                 INSERT INTO stats_state_gauges (metric, labels_json, value)
                 VALUES ('sashiko_bugs_total', {new_labels}, 1)
                 ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;
             END"
        ),
        (),
    )
    .await?;

    let attach_update = if has_bug_enrichments {
        "UPDATE stats_state_gauges
         SET value = MAX(0, value - 1)
         WHERE old.lifecycle_status IS NOT new.lifecycle_status
           AND EXISTS (SELECT 1 FROM bug_enrichments e WHERE e.bug_id = new.id AND e.kind IN ('reproducer', 'testcase', 'poc'))
           AND metric = 'sashiko_bugs_with_attachment'
           AND labels_json = json_object('attachment', 'reproducer', 'status', old.lifecycle_status);
         INSERT INTO stats_state_gauges (metric, labels_json, value)
         SELECT 'sashiko_bugs_with_attachment', json_object('attachment', 'reproducer', 'status', new.lifecycle_status), 1
         WHERE old.lifecycle_status IS NOT new.lifecycle_status
           AND EXISTS (SELECT 1 FROM bug_enrichments e WHERE e.bug_id = new.id AND e.kind IN ('reproducer', 'testcase', 'poc'))
         ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;

         UPDATE stats_state_gauges
         SET value = MAX(0, value - 1)
         WHERE old.lifecycle_status IS NOT new.lifecycle_status
           AND EXISTS (SELECT 1 FROM bug_enrichments e WHERE e.bug_id = new.id AND e.kind IN ('proposed_fix', 'suggested_fix'))
           AND metric = 'sashiko_bugs_with_attachment'
           AND labels_json = json_object('attachment', 'proposed_fix', 'status', old.lifecycle_status);
         INSERT INTO stats_state_gauges (metric, labels_json, value)
         SELECT 'sashiko_bugs_with_attachment', json_object('attachment', 'proposed_fix', 'status', new.lifecycle_status), 1
         WHERE old.lifecycle_status IS NOT new.lifecycle_status
           AND EXISTS (SELECT 1 FROM bug_enrichments e WHERE e.bug_id = new.id AND e.kind IN ('proposed_fix', 'suggested_fix'))
         ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;"
    } else {
        ""
    };

    tx.execute(
        &format!(
            "CREATE TRIGGER trg_stats_bugs_update
             AFTER UPDATE OF lifecycle_status, pipeline_state, severity_int, security_class, security_score_tenths ON bugs
             FOR EACH ROW
             WHEN old.lifecycle_status IS NOT new.lifecycle_status
               OR old.pipeline_state IS NOT new.pipeline_state
               OR old.severity_int IS NOT new.severity_int
               OR old.security_class IS NOT new.security_class
               OR old.security_score_tenths IS NOT new.security_score_tenths
             BEGIN
                 UPDATE stats_state_gauges
                 SET value = MAX(0, value - 1)
                 WHERE metric = 'sashiko_bugs_total'
                   AND labels_json = {old_labels};
                 INSERT INTO stats_state_gauges (metric, labels_json, value)
                 VALUES ('sashiko_bugs_total', {new_labels}, 1)
                 ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;

                 UPDATE stats_bug_section_links
                 SET lifecycle_status = new.lifecycle_status,
                     severity_int = new.severity_int,
                     security_rating = {new_sec}
                 WHERE bug_id = new.id;

                 {attach_update}
             END"
        ),
        (),
    )
    .await?;

    let attach_del = if has_bug_enrichments {
        "UPDATE stats_state_gauges
         SET value = MAX(0, value - 1)
         WHERE EXISTS (SELECT 1 FROM bug_enrichments e WHERE e.bug_id = old.id AND e.kind IN ('reproducer', 'testcase', 'poc'))
           AND metric = 'sashiko_bugs_with_attachment'
           AND labels_json = json_object('attachment', 'reproducer', 'status', old.lifecycle_status);
         UPDATE stats_state_gauges
         SET value = MAX(0, value - 1)
         WHERE EXISTS (SELECT 1 FROM bug_enrichments e WHERE e.bug_id = old.id AND e.kind IN ('proposed_fix', 'suggested_fix'))
           AND metric = 'sashiko_bugs_with_attachment'
           AND labels_json = json_object('attachment', 'proposed_fix', 'status', old.lifecycle_status);"
    } else {
        ""
    };

    // BEFORE DELETE ensures stats_bug_section_links rows are explicitly removed
    // (firing trg_stats_bug_links_delete even with recursive_triggers = OFF)
    // and attachment checks observe bug_enrichments prior to ON DELETE CASCADE.
    tx.execute(
        &format!(
            "CREATE TRIGGER trg_stats_bugs_before_delete
             BEFORE DELETE ON bugs
             FOR EACH ROW
             BEGIN
                 UPDATE stats_state_gauges
                 SET value = MAX(0, value - 1)
                 WHERE metric = 'sashiko_bugs_total'
                   AND labels_json = {old_labels};
                 {attach_del}
                 DELETE FROM stats_bug_section_links WHERE bug_id = old.id;
             END"
        ),
        (),
    )
    .await?;

    tx.execute(
        "CREATE TRIGGER trg_stats_bugs_rollup_lifecycle
         AFTER UPDATE OF lifecycle_status ON bugs
         FOR EACH ROW
         WHEN old.lifecycle_status IS NOT new.lifecycle_status
           AND new.lifecycle_status IN ('open', 'fixed', 'duplicate', 'dismissed')
           AND (new.lifecycle_status != 'open' OR old.lifecycle_status = 'new')
           AND new.id > COALESCE(
               (SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'stats_backfill:v1:cursor:bugs'),
               0
           )
         BEGIN
             INSERT OR IGNORE INTO stats_series (metric, labels_json)
             VALUES (
                 'sashiko_bugs_events_total',
                 json_object(
                     'event',
                     CASE new.lifecycle_status
                         WHEN 'open' THEN 'discovered'
                         ELSE new.lifecycle_status
                     END
                 )
             );
             INSERT INTO stats_rollups (series_id, resolution_secs, bucket_ts, value)
             SELECT s.id,
                    res.sec,
                    (MAX(COALESCE(new.updated_at, 1), 1) / res.sec) * res.sec,
                    1.0
             FROM stats_series s
             JOIN (SELECT 300 AS sec UNION ALL SELECT 3600 UNION ALL SELECT 86400) res
             WHERE s.metric = 'sashiko_bugs_events_total'
               AND s.labels_json = json_object(
                   'event',
                   CASE new.lifecycle_status
                       WHEN 'open' THEN 'discovered'
                       ELSE new.lifecycle_status
                   END
               )
             ON CONFLICT(series_id, resolution_secs, bucket_ts)
             DO UPDATE SET value = value + 1.0;
         END",
        (),
    )
    .await?;

    install_bug_subsystem_triggers(tx, has_bug_subsystems, has_bug_upstream).await?;
    if has_bug_enrichments {
        install_bug_enrichment_triggers(tx).await?;
    }
    Ok(())
}

async fn install_bug_subsystem_triggers(
    tx: &DatabaseTransaction,
    has_bug_subsystems: bool,
    has_bug_upstream: bool,
) -> Result<()> {
    let b_sec = bug_sec_rating_expr("b");
    let not_in_upstream = if has_bug_upstream {
        "AND NOT EXISTS (SELECT 1 FROM bug_upstream_sections u WHERE u.bug_id = old.bug_id AND u.subsystem = old.subsystem)"
    } else {
        ""
    };
    let not_in_maint = if has_bug_subsystems {
        "AND NOT EXISTS (SELECT 1 FROM bug_subsystems bs WHERE bs.bug_id = old.bug_id AND bs.subsystem = old.subsystem AND bs.source = 'maintainers_section')"
    } else {
        ""
    };

    if has_bug_subsystems {
        tx.execute(
            &format!(
                "CREATE TRIGGER trg_stats_bug_subsystems_insert
                 AFTER INSERT ON bug_subsystems
                 FOR EACH ROW
                 WHEN new.source IN ('maintainers_section', 'path_prefix')
                   AND length(trim(new.subsystem)) > 0
                 BEGIN
                     INSERT OR IGNORE INTO stats_bug_section_links (
                         bug_id, subsystem, is_authorizing, lifecycle_status, severity_int, security_rating
                     )
                     SELECT b.id,
                            new.subsystem,
                            CASE WHEN new.source = 'maintainers_section' THEN 1 ELSE 0 END,
                            b.lifecycle_status,
                            b.severity_int,
                            {b_sec}
                     FROM bugs b WHERE b.id = new.bug_id;
                 END"
            ),
            (),
        )
        .await?;

        tx.execute(
            &format!(
                "CREATE TRIGGER trg_stats_bug_subsystems_update
                 AFTER UPDATE OF subsystem, source ON bug_subsystems
                 FOR EACH ROW
                 WHEN old.subsystem IS NOT new.subsystem OR old.source IS NOT new.source
                 BEGIN
                     DELETE FROM stats_bug_section_links
                     WHERE old.source = 'path_prefix'
                       AND bug_id = old.bug_id
                       AND subsystem = old.subsystem
                       AND is_authorizing = 0;

                     DELETE FROM stats_bug_section_links
                     WHERE old.source = 'maintainers_section'
                       AND bug_id = old.bug_id
                       AND subsystem = old.subsystem
                       AND is_authorizing = 1
                       {not_in_upstream};

                     INSERT OR IGNORE INTO stats_bug_section_links (
                         bug_id, subsystem, is_authorizing, lifecycle_status, severity_int, security_rating
                     )
                     SELECT b.id,
                            new.subsystem,
                            CASE WHEN new.source = 'maintainers_section' THEN 1 ELSE 0 END,
                            b.lifecycle_status,
                            b.severity_int,
                            {b_sec}
                     FROM bugs b
                     WHERE b.id = new.bug_id
                       AND new.source IN ('maintainers_section', 'path_prefix')
                       AND length(trim(new.subsystem)) > 0;
                 END"
            ),
            (),
        )
        .await?;

        tx.execute(
            &format!(
                "CREATE TRIGGER trg_stats_bug_subsystems_delete
                 AFTER DELETE ON bug_subsystems
                 FOR EACH ROW
                 WHEN old.source IN ('maintainers_section', 'path_prefix')
                 BEGIN
                     DELETE FROM stats_bug_section_links
                     WHERE old.source = 'path_prefix'
                       AND bug_id = old.bug_id
                       AND subsystem = old.subsystem
                       AND is_authorizing = 0;

                     DELETE FROM stats_bug_section_links
                     WHERE old.source = 'maintainers_section'
                       AND bug_id = old.bug_id
                       AND subsystem = old.subsystem
                       AND is_authorizing = 1
                       {not_in_upstream};
                 END"
            ),
            (),
        )
        .await?;
    }

    if has_bug_upstream {
        tx.execute(
            &format!(
                "CREATE TRIGGER trg_stats_bug_upstream_insert
                 AFTER INSERT ON bug_upstream_sections
                 FOR EACH ROW
                 WHEN length(trim(new.subsystem)) > 0
                 BEGIN
                     INSERT OR IGNORE INTO stats_bug_section_links (
                         bug_id, subsystem, is_authorizing, lifecycle_status, severity_int, security_rating
                     )
                     SELECT b.id, new.subsystem, 1, b.lifecycle_status, b.severity_int, {b_sec}
                     FROM bugs b WHERE b.id = new.bug_id;
                 END"
            ),
            (),
        )
        .await?;

        tx.execute(
            &format!(
                "CREATE TRIGGER trg_stats_bug_upstream_delete
                 AFTER DELETE ON bug_upstream_sections
                 FOR EACH ROW
                 BEGIN
                     DELETE FROM stats_bug_section_links
                     WHERE bug_id = old.bug_id
                       AND subsystem = old.subsystem
                       AND is_authorizing = 1
                       {not_in_maint};
                 END"
            ),
            (),
        )
        .await?;
    }
    Ok(())
}

async fn install_bug_enrichment_triggers(tx: &DatabaseTransaction) -> Result<()> {
    tx.execute(
        "CREATE TRIGGER trg_stats_bug_enrichments_repro_insert
         AFTER INSERT ON bug_enrichments
         FOR EACH ROW
         WHEN new.kind IN ('reproducer', 'testcase', 'poc')
           AND NOT EXISTS (
               SELECT 1 FROM bug_enrichments e
               WHERE e.bug_id = new.bug_id AND e.id != new.id AND e.kind IN ('reproducer', 'testcase', 'poc')
           )
         BEGIN
             INSERT INTO stats_state_gauges (metric, labels_json, value)
             SELECT 'sashiko_bugs_with_attachment',
                    json_object('attachment', 'reproducer', 'status', b.lifecycle_status),
                    1
             FROM bugs b WHERE b.id = new.bug_id
             ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;
         END",
        (),
    )
    .await?;

    tx.execute(
        "CREATE TRIGGER trg_stats_bug_enrichments_repro_delete
         AFTER DELETE ON bug_enrichments
         FOR EACH ROW
         WHEN old.kind IN ('reproducer', 'testcase', 'poc')
           AND EXISTS (SELECT 1 FROM bugs WHERE id = old.bug_id)
           AND NOT EXISTS (
               SELECT 1 FROM bug_enrichments e
               WHERE e.bug_id = old.bug_id AND e.kind IN ('reproducer', 'testcase', 'poc')
           )
         BEGIN
             UPDATE stats_state_gauges
             SET value = MAX(0, value - 1)
             WHERE metric = 'sashiko_bugs_with_attachment'
               AND labels_json = json_object(
                   'attachment', 'reproducer',
                   'status', (SELECT lifecycle_status FROM bugs WHERE id = old.bug_id)
               );
         END",
        (),
    )
    .await?;

    tx.execute(
        "CREATE TRIGGER trg_stats_bug_enrichments_fix_insert
         AFTER INSERT ON bug_enrichments
         FOR EACH ROW
         WHEN new.kind IN ('proposed_fix', 'suggested_fix')
           AND NOT EXISTS (
               SELECT 1 FROM bug_enrichments e
               WHERE e.bug_id = new.bug_id AND e.id != new.id AND e.kind IN ('proposed_fix', 'suggested_fix')
           )
         BEGIN
             INSERT INTO stats_state_gauges (metric, labels_json, value)
             SELECT 'sashiko_bugs_with_attachment',
                    json_object('attachment', 'proposed_fix', 'status', b.lifecycle_status),
                    1
             FROM bugs b WHERE b.id = new.bug_id
             ON CONFLICT(metric, labels_json) DO UPDATE SET value = value + 1;
         END",
        (),
    )
    .await?;

    tx.execute(
        "CREATE TRIGGER trg_stats_bug_enrichments_fix_delete
         AFTER DELETE ON bug_enrichments
         FOR EACH ROW
         WHEN old.kind IN ('proposed_fix', 'suggested_fix')
           AND EXISTS (SELECT 1 FROM bugs WHERE id = old.bug_id)
           AND NOT EXISTS (
               SELECT 1 FROM bug_enrichments e
               WHERE e.bug_id = old.bug_id AND e.kind IN ('proposed_fix', 'suggested_fix')
           )
         BEGIN
             UPDATE stats_state_gauges
             SET value = MAX(0, value - 1)
             WHERE metric = 'sashiko_bugs_with_attachment'
               AND labels_json = json_object(
                   'attachment', 'proposed_fix',
                   'status', (SELECT lifecycle_status FROM bugs WHERE id = old.bug_id)
               );
         END",
        (),
    )
    .await?;

    Ok(())
}

impl Database {
    /// Reads a single live gauge row in O(1) by primary key `(metric, labels_json)`.
    pub async fn get_state_gauge(&self, metric: &str, labels_json: &str) -> Result<i64> {
        let mut rows = self
            .read_conn()
            .query(
                "SELECT value FROM stats_state_gauges WHERE metric = ? AND labels_json = ?",
                libsql::params![metric, labels_json],
            )
            .await?;
        let val = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        Ok(val)
    }

    /// Reads all non-zero `(labels_json, value)` entries for `metric` in O(facets).
    pub async fn get_state_gauges_by_metric(&self, metric: &str) -> Result<Vec<(String, i64)>> {
        let mut rows = self
            .read_conn()
            .query(
                "SELECT labels_json, value FROM stats_state_gauges WHERE metric = ? AND value > 0",
                libsql::params![metric],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push((row.get::<String>(0)?, row.get::<i64>(1)?));
        }
        Ok(out)
    }

    /// Sums all label facets of `metric` in `stats_state_gauges` in O(facets).
    pub async fn sum_state_gauge_metric(&self, metric: &str) -> Result<i64> {
        let mut rows = self
            .read_conn()
            .query(
                "SELECT COALESCE(SUM(value), 0) FROM stats_state_gauges WHERE metric = ?",
                libsql::params![metric],
            )
            .await?;
        let sum = match rows.next().await? {
            Some(row) => row.get::<i64>(0)?,
            None => 0,
        };
        Ok(sum)
    }

    /// O(1) total stored message count backed by `stats_state_gauges`.
    pub async fn count_messages_stored_o1(&self) -> Result<usize> {
        let v = self
            .get_state_gauge("sashiko_messages_stored", "{}")
            .await?;
        Ok(v.max(0) as usize)
    }

    /// O(1) total stored patchset count backed by `stats_state_gauges`.
    pub async fn count_patchsets_stored_o1(&self) -> Result<usize> {
        let v = self
            .sum_state_gauge_metric("sashiko_patchsets_stored")
            .await?;
        Ok(v.max(0) as usize)
    }

    /// Reads all positive rows in `stats_state_gauges` (~300 bounded facets).
    pub async fn get_all_state_gauges(
        &self,
    ) -> Result<Vec<(String, BTreeMap<String, String>, i64)>> {
        let mut rows = self
            .read_conn()
            .query(
                "SELECT metric, labels_json, value FROM stats_state_gauges WHERE value > 0",
                (),
            )
            .await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            let m: String = r.get(0)?;
            let lj: String = r.get(1)?;
            let v: i64 = r.get(2)?;
            out.push((m, decode_labels_json(&lj), v));
        }
        Ok(out)
    }

    /// Returns `true` once `stats_backfill:v1:done` is `'1'` in `meta`.
    pub async fn is_stats_backfill_done(&self) -> Result<bool> {
        let mut rows = self
            .read_conn()
            .query(
                "SELECT value FROM meta WHERE key = 'stats_backfill:v1:done'",
                (),
            )
            .await?;
        let done = match rows.next().await? {
            Some(r) => r.get::<String>(0)? == "1",
            None => false,
        };
        Ok(done)
    }

    /// Queries bounded multi-resolution `stats_rollups` buckets for `metrics`,
    /// resolving matching `stats_series` rows first via `UNIQUE(metric, labels_json)`
    /// and then seeking `stats_rollups` directly by `(series_id, resolution_secs, bucket_ts)`.
    pub async fn query_rollup_buckets(
        &self,
        metrics: &[String],
        resolution_secs: i64,
        min_bucket_ts: i64,
        max_bucket_ts: i64,
    ) -> Result<Vec<RollupBucketRow>> {
        self.query_rollup_buckets_filtered(
            metrics,
            resolution_secs,
            min_bucket_ts,
            max_bucket_ts,
            |_, _| true,
        )
        .await
    }

    /// Queries `stats_rollups` after pre-filtering `stats_series` in memory so
    /// selective label filters (e.g. `{subsystem="netdev"}`) only seek matching
    /// `series_id` ranges in `stats_rollups` and decode `labels_json` once per series.
    pub async fn query_rollup_buckets_filtered<F>(
        &self,
        metrics: &[String],
        resolution_secs: i64,
        min_bucket_ts: i64,
        max_bucket_ts: i64,
        accept_series: F,
    ) -> Result<Vec<RollupBucketRow>>
    where
        F: Fn(&str, &BTreeMap<String, String>) -> bool,
    {
        if metrics.is_empty() {
            return Ok(Vec::new());
        }
        let mut series_meta: std::collections::HashMap<i64, (String, BTreeMap<String, String>)> =
            std::collections::HashMap::new();
        let mut series_ids: Vec<i64> = Vec::new();
        for chunk in metrics.chunks(SUB_BATCH_SIZE) {
            let ph = vec!["?"; chunk.len()].join(", ");
            let params: Vec<libsql::Value> = chunk
                .iter()
                .map(|m| libsql::Value::Text(m.clone()))
                .collect();
            let sql =
                format!("SELECT id, metric, labels_json FROM stats_series WHERE metric IN ({ph})");
            let mut rows = self.read_conn().query(&sql, params).await?;
            while let Some(r) = rows.next().await? {
                let id: i64 = r.get(0)?;
                let metric: String = r.get(1)?;
                let lj: String = r.get(2)?;
                let labels = decode_labels_json(&lj);
                if accept_series(&metric, &labels) {
                    series_ids.push(id);
                    series_meta.insert(id, (metric, labels));
                }
            }
        }
        if series_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        for chunk in series_ids.chunks(SUB_BATCH_SIZE) {
            let ph = vec!["?"; chunk.len()].join(", ");
            let mut params: Vec<libsql::Value> =
                chunk.iter().map(|&id| libsql::Value::Integer(id)).collect();
            params.push(libsql::Value::Integer(resolution_secs));
            params.push(libsql::Value::Integer(min_bucket_ts));
            params.push(libsql::Value::Integer(max_bucket_ts));

            let sql = format!(
                "SELECT series_id, bucket_ts, value
                 FROM stats_rollups
                 WHERE series_id IN ({ph})
                   AND resolution_secs = ?
                   AND bucket_ts >= ?
                   AND bucket_ts <= ?"
            );
            let mut rows = self.read_conn().query(&sql, params).await?;
            while let Some(r) = rows.next().await? {
                let sid: i64 = r.get(0)?;
                let bucket_ts: i64 = r.get(1)?;
                let value: f64 = r.get(2)?;
                if let Some((metric, labels)) = series_meta.get(&sid) {
                    out.push(RollupBucketRow {
                        metric: metric.clone(),
                        labels: labels.clone(),
                        bucket_ts,
                        value,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Queries `stats_rollups` for a single instant window `[query_start, query_end]`
    /// by pushing interior-bucket aggregation (`full_start..=full_end`) into SQLite
    /// `SUM(value) GROUP BY series_id` while returning boundary buckets verbatim for
    /// exact fractional edge weighting.
    #[allow(clippy::too_many_arguments)]
    pub async fn query_rollup_sums_filtered<F>(
        &self,
        metrics: &[String],
        resolution_secs: i64,
        query_start: i64,
        full_start: i64,
        full_end: i64,
        query_end: i64,
        accept_series: F,
    ) -> Result<Vec<RollupBucketRow>>
    where
        F: Fn(&str, &BTreeMap<String, String>) -> bool,
    {
        if metrics.is_empty() {
            return Ok(Vec::new());
        }
        if full_start > full_end {
            return self
                .query_rollup_buckets_filtered(
                    metrics,
                    resolution_secs,
                    query_start,
                    query_end,
                    accept_series,
                )
                .await;
        }
        let mut series_meta: std::collections::HashMap<i64, (String, BTreeMap<String, String>)> =
            std::collections::HashMap::new();
        let mut series_ids: Vec<i64> = Vec::new();
        for chunk in metrics.chunks(SUB_BATCH_SIZE) {
            let ph = vec!["?"; chunk.len()].join(", ");
            let params: Vec<libsql::Value> = chunk
                .iter()
                .map(|m| libsql::Value::Text(m.clone()))
                .collect();
            let sql =
                format!("SELECT id, metric, labels_json FROM stats_series WHERE metric IN ({ph})");
            let mut rows = self.read_conn().query(&sql, params).await?;
            while let Some(r) = rows.next().await? {
                let id: i64 = r.get(0)?;
                let metric: String = r.get(1)?;
                let lj: String = r.get(2)?;
                let labels = decode_labels_json(&lj);
                if accept_series(&metric, &labels) {
                    series_ids.push(id);
                    series_meta.insert(id, (metric, labels));
                }
            }
        }
        if series_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        for chunk in series_ids.chunks(SUB_BATCH_SIZE) {
            let ph = vec!["?"; chunk.len()].join(", ");

            // 1. Interior buckets summed in SQLite (1 row per series_id).
            let mut sum_params: Vec<libsql::Value> =
                chunk.iter().map(|&id| libsql::Value::Integer(id)).collect();
            sum_params.push(libsql::Value::Integer(resolution_secs));
            sum_params.push(libsql::Value::Integer(full_start));
            sum_params.push(libsql::Value::Integer(full_end));
            let sum_sql = format!(
                "SELECT series_id, SUM(value)
                 FROM stats_rollups
                 WHERE series_id IN ({ph})
                   AND resolution_secs = ?
                   AND bucket_ts >= ?
                   AND bucket_ts <= ?
                 GROUP BY series_id"
            );
            let mut sum_rows = self.read_conn().query(&sum_sql, sum_params).await?;
            while let Some(r) = sum_rows.next().await? {
                let sid: i64 = r.get(0)?;
                let value: f64 = r.get(1)?;
                if let Some((metric, labels)) = series_meta.get(&sid) {
                    out.push(RollupBucketRow {
                        metric: metric.clone(),
                        labels: labels.clone(),
                        bucket_ts: full_start,
                        value,
                    });
                }
            }
            drop(sum_rows);

            // 2. Boundary buckets (left/right edges outside full_start..=full_end).
            let mut edge_params: Vec<libsql::Value> =
                chunk.iter().map(|&id| libsql::Value::Integer(id)).collect();
            edge_params.push(libsql::Value::Integer(resolution_secs));
            edge_params.push(libsql::Value::Integer(query_start));
            edge_params.push(libsql::Value::Integer(full_start));
            edge_params.push(libsql::Value::Integer(full_end));
            edge_params.push(libsql::Value::Integer(query_end));
            let edge_sql = format!(
                "SELECT series_id, bucket_ts, value
                 FROM stats_rollups
                 WHERE series_id IN ({ph})
                   AND resolution_secs = ?
                   AND ((bucket_ts >= ? AND bucket_ts < ?) OR (bucket_ts > ? AND bucket_ts <= ?))"
            );
            let mut edge_rows = self.read_conn().query(&edge_sql, edge_params).await?;
            while let Some(r) = edge_rows.next().await? {
                let sid: i64 = r.get(0)?;
                let bucket_ts: i64 = r.get(1)?;
                let value: f64 = r.get(2)?;
                if let Some((metric, labels)) = series_meta.get(&sid) {
                    out.push(RollupBucketRow {
                        metric: metric.clone(),
                        labels: labels.clone(),
                        bucket_ts,
                        value,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Lists public mailing-list subsystem names from `subsystems` (bounded by `limit`).
    pub async fn list_public_subsystem_names(&self, limit: usize) -> Result<Vec<String>> {
        let cap = limit.clamp(1, 500) as i64;
        let mut rows = self
            .read_conn()
            .query(
                "SELECT name FROM subsystems ORDER BY name ASC LIMIT ?",
                libsql::params![cap],
            )
            .await?;
        let mut out = Vec::new();
        while let Some(r) = rows.next().await? {
            out.push(r.get::<String>(0)?);
        }
        Ok(out)
    }

    /// Lists distinct values for a non-restricted label key across `stats_series`
    /// (`subsystem` is deliberately handled separately with ACL checks).
    pub async fn list_series_label_values(
        &self,
        label_name: &str,
        limit: usize,
    ) -> Result<Vec<String>> {
        if label_name == "subsystem" || label_name.is_empty() || label_name.len() > 64 {
            return Ok(Vec::new());
        }
        let cap = limit.clamp(1, 500);
        let mut rows = self
            .read_conn()
            .query("SELECT labels_json FROM stats_series LIMIT 2000", ())
            .await?;
        let mut vals = std::collections::BTreeSet::new();
        while let Some(r) = rows.next().await? {
            let lj: String = r.get(0)?;
            let map = decode_labels_json(&lj);
            if let Some(v) = map.get(label_name) {
                vals.insert(v.clone());
                if vals.len() >= cap {
                    break;
                }
            }
        }
        Ok(vals.into_iter().collect())
    }

    /// Serves legacy `/api/stats/timeline` in O(1) from 1-day (`86400`s) rollups
    /// without scanning `messages`, `patchsets`, `patches`, `reviews`, or `findings`.
    pub async fn get_timeline_stats_o1(
        &self,
        subsystem_id: Option<i64>,
    ) -> Result<serde_json::Value> {
        let now_ts = chrono::Utc::now().timestamp();
        let min_ts = now_ts.saturating_sub(365 * 86_400);
        let sub_name: Option<String> = if let Some(sid) = subsystem_id {
            let mut r = self
                .read_conn()
                .query(
                    "SELECT name FROM subsystems WHERE id = ?",
                    libsql::params![sid],
                )
                .await?;
            match r.next().await? {
                Some(row) => Some(row.get::<String>(0)?),
                None => {
                    return Ok(serde_json::json!({
                        "messages": [],
                        "patchsets": [],
                        "patches": [],
                        "reviews": [],
                        "findings": [],
                    }));
                }
            }
        } else {
            None
        };

        let metrics: Vec<String> = if sub_name.is_some() {
            vec![
                "sashiko_subsystem_patchsets_total".into(),
                "sashiko_subsystem_patches_total".into(),
                "sashiko_subsystem_findings_total".into(),
            ]
        } else {
            vec![
                "sashiko_messages_ingested_total".into(),
                "sashiko_patchsets_completed_total".into(),
                "sashiko_patches_ingested_total".into(),
                "sashiko_reviews_completed_total".into(),
                "sashiko_findings_discovered_total".into(),
            ]
        };
        let rows = self
            .query_rollup_buckets_filtered(&metrics, 86_400, min_ts, now_ts, |_, labels| {
                sub_name
                    .as_ref()
                    .is_none_or(|target| labels.get("subsystem") == Some(target))
            })
            .await?;

        let fmt_day = |ts: i64| -> String {
            chrono::DateTime::from_timestamp(ts, 0)
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_default()
        };

        let mut msg_by_day: BTreeMap<String, i64> = BTreeMap::new();
        let mut ps_by_day: BTreeMap<(String, String), i64> = BTreeMap::new();
        let mut patch_by_day: BTreeMap<String, i64> = BTreeMap::new();
        let mut rev_by_day: BTreeMap<(String, String), i64> = BTreeMap::new();
        let mut find_by_day: BTreeMap<(String, String), i64> = BTreeMap::new();

        for r in rows {
            if let Some(ref target_sub) = sub_name
                && r.labels.get("subsystem") != Some(target_sub)
            {
                continue;
            }
            let day = fmt_day(r.bucket_ts);
            if day.is_empty() {
                continue;
            }
            let cnt = r.value.round() as i64;
            if cnt <= 0 {
                continue;
            }
            match r.metric.as_str() {
                "sashiko_messages_ingested_total" => {
                    *msg_by_day.entry(day).or_insert(0) += cnt;
                }
                "sashiko_patchsets_completed_total" => {
                    let st = r.labels.get("status").cloned().unwrap_or_default();
                    *ps_by_day.entry((day, st)).or_insert(0) += cnt;
                }
                "sashiko_subsystem_patchsets_total" => {
                    *ps_by_day.entry((day, "Reviewed".to_string())).or_insert(0) += cnt;
                }
                "sashiko_patches_ingested_total" | "sashiko_subsystem_patches_total" => {
                    *patch_by_day.entry(day).or_insert(0) += cnt;
                }
                "sashiko_reviews_completed_total" => {
                    let st = r
                        .labels
                        .get("status")
                        .cloned()
                        .unwrap_or_else(|| "Reviewed".to_string());
                    *rev_by_day.entry((day, st)).or_insert(0) += cnt;
                }
                "sashiko_findings_discovered_total" | "sashiko_subsystem_findings_total" => {
                    let sev = r
                        .labels
                        .get("severity")
                        .cloned()
                        .unwrap_or_else(|| "low".to_string());
                    *find_by_day.entry((day, sev)).or_insert(0) += cnt;
                }
                _ => {}
            }
        }

        let messages_data: Vec<_> = msg_by_day
            .into_iter()
            .map(|(day, count)| serde_json::json!({"day": day, "count": count}))
            .collect();
        let patchsets_data: Vec<_> = ps_by_day
            .into_iter()
            .map(|((day, status), count)| {
                serde_json::json!({"day": day, "status": status, "count": count})
            })
            .collect();
        let patches_data: Vec<_> = patch_by_day
            .into_iter()
            .map(|(day, count)| serde_json::json!({"day": day, "count": count}))
            .collect();
        let reviews_data: Vec<_> = rev_by_day
            .into_iter()
            .map(|((day, status), count)| {
                serde_json::json!({"day": day, "status": status, "count": count})
            })
            .collect();
        let findings_data: Vec<_> = find_by_day
            .into_iter()
            .map(|((day, severity), count)| {
                serde_json::json!({"day": day, "severity": severity, "count": count})
            })
            .collect();

        Ok(serde_json::json!({
            "messages": messages_data,
            "patchsets": patchsets_data,
            "patches": patches_data,
            "reviews": reviews_data,
            "findings": findings_data,
        }))
    }

    /// Reads `BugAccess`-enforced per-subsystem bug summaries in O(1) from
    /// `stats_bug_subsystem_facets` without joining `bugs`.
    ///
    /// - `BugVisibility::Unrestricted`: returns top `limit` authorizing
    ///   subsystems ordered by open then total bug count.
    /// - `BugVisibility::Sections(scope)`: queries only authorizing sections in
    ///   `scope` (`COLLATE NOCASE`, chunked by `SUB_BATCH_SIZE`), leaking zero
    ///   counts for unauthorized subsystems.
    pub async fn get_bug_subsystem_facets_o1(
        &self,
        lifecycle_status: Option<BugLifecycleStatus>,
        visibility: BugVisibility<'_>,
        limit: usize,
    ) -> Result<Vec<BugSubsystemFacetSummary>> {
        let clamped_limit = limit.clamp(1, 200);
        let status_filter = lifecycle_status.as_ref().map(BugLifecycleStatus::as_str);

        let mut map: BTreeMap<String, BugSubsystemFacetSummary> = BTreeMap::new();
        match visibility {
            BugVisibility::Unrestricted => {
                let status_sql = if status_filter.is_some() {
                    "AND lifecycle_status = ?"
                } else {
                    ""
                };
                let sql = format!(
                    "WITH top_subs AS (
                        SELECT subsystem,
                               SUM(CASE WHEN lifecycle_status = 'open' THEN count ELSE 0 END) AS open_cnt,
                               SUM(count) AS total_cnt
                        FROM stats_bug_subsystem_facets
                        WHERE is_authorizing = 1 AND count > 0 {status_sql}
                        GROUP BY subsystem
                        ORDER BY open_cnt DESC, total_cnt DESC, subsystem ASC
                        LIMIT ?
                    )
                    SELECT f.subsystem, f.lifecycle_status, f.severity_int, f.security_rating, f.count
                    FROM stats_bug_subsystem_facets f
                    JOIN top_subs t ON t.subsystem = f.subsystem
                    WHERE f.is_authorizing = 1 AND f.count > 0 {status_sql}"
                );
                // Duplicate status bind if status_sql appears in both CTE and outer query.
                let query_params: Vec<libsql::Value> = if let Some(st) = status_filter {
                    vec![
                        libsql::Value::Text(st.to_string()),
                        libsql::Value::Integer(clamped_limit as i64),
                        libsql::Value::Text(st.to_string()),
                    ]
                } else {
                    vec![libsql::Value::Integer(clamped_limit as i64)]
                };
                let mut rows = self.read_conn().query(&sql, query_params).await?;
                while let Some(row) = rows.next().await? {
                    fold_subsystem_facet_row(&mut map, &row)?;
                }
            }
            BugVisibility::Sections(scope) => {
                let titles: Vec<&str> = scope
                    .iter()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .collect();
                if titles.is_empty() {
                    return Ok(Vec::new());
                }
                for chunk in titles.chunks(SUB_BATCH_SIZE) {
                    let placeholders = vec!["?"; chunk.len()].join(", ");
                    let mut params: Vec<libsql::Value> = chunk
                        .iter()
                        .map(|t| libsql::Value::Text((*t).to_string()))
                        .collect();
                    let status_sql = if let Some(st) = status_filter {
                        params.push(libsql::Value::Text(st.to_string()));
                        "AND lifecycle_status = ?"
                    } else {
                        ""
                    };
                    let sql = format!(
                        "SELECT subsystem, lifecycle_status, severity_int, security_rating, count
                         FROM stats_bug_subsystem_facets
                         WHERE is_authorizing = 1
                           AND count > 0
                           AND subsystem COLLATE NOCASE IN ({placeholders})
                           {status_sql}"
                    );
                    let mut rows = self.read_conn().query(&sql, params).await?;
                    while let Some(row) = rows.next().await? {
                        fold_subsystem_facet_row(&mut map, &row)?;
                    }
                }
            }
        }

        let mut list: Vec<BugSubsystemFacetSummary> =
            map.into_values().filter(|s| s.total > 0).collect();
        list.sort_by(|a, b| {
            b.open
                .cmp(&a.open)
                .then_with(|| b.total.cmp(&a.total))
                .then_with(|| a.subsystem.cmp(&b.subsystem))
        });
        list.truncate(clamped_limit);
        Ok(list)
    }

    /// Writes a batch of positive counter/histogram deltas into all three
    /// multi-resolution rollup tiers (`5m`, `1h`, `1d`) inside a single
    /// immediate transaction.
    pub async fn flush_rollup_samples(
        &self,
        event_ts: i64,
        samples: &[RollupSample],
    ) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        let tx = self.begin_immediate_transaction().await?;
        flush_rollup_samples_in_tx(&tx, event_ts, samples).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Reaps expired time-series buckets across all resolution tiers using
    /// `idx_stats_rollups_expiry` with a bounded `batch_limit` per tier so
    /// pruning never holds a long write lock.
    pub async fn reap_expired_rollups(&self, now_ts: i64, batch_limit: usize) -> Result<u64> {
        let cap = batch_limit.clamp(100, 20_000) as i64;
        let tx = self.begin_immediate_transaction().await?;
        let mut total_deleted = 0u64;
        for (res_secs, retention_secs) in ROLLUP_RESOLUTIONS {
            let cutoff = now_ts.saturating_sub(retention_secs);
            let deleted = tx
                .execute(
                    "DELETE FROM stats_rollups
                     WHERE rowid IN (
                         SELECT rowid FROM stats_rollups
                         WHERE resolution_secs = ? AND bucket_ts < ?
                         LIMIT ?
                     )",
                    libsql::params![res_secs, cutoff, cap],
                )
                .await?;
            total_deleted = total_deleted.saturating_add(deleted);
        }
        tx.commit().await?;
        Ok(total_deleted)
    }

    /// Commits one bounded historical backfill chunk and advances its cursor in
    /// `meta` inside a single immediate transaction so interrupted runs resume
    /// cleanly without double-counting.
    pub async fn commit_backfill_chunk(
        &self,
        phase: &str,
        new_cursor: i64,
        buckets: &BTreeMap<(String, String, i64, i64), f64>,
    ) -> Result<()> {
        let tx = self.begin_immediate_transaction().await?;
        let mut series_cache: BTreeMap<(String, String), i64> = BTreeMap::new();
        for ((metric, labels_json, res_secs, bucket_ts), delta) in buckets {
            if *delta <= 0.0 || !delta.is_finite() {
                continue;
            }
            let series_id =
                if let Some(&id) = series_cache.get(&(metric.clone(), labels_json.clone())) {
                    id
                } else {
                    let id = ensure_series_id(&tx, metric, labels_json).await?;
                    series_cache.insert((metric.clone(), labels_json.clone()), id);
                    id
                };
            tx.execute(
                "INSERT INTO stats_rollups (series_id, resolution_secs, bucket_ts, value)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(series_id, resolution_secs, bucket_ts)
                 DO UPDATE SET value = value + excluded.value",
                libsql::params![series_id, *res_secs, *bucket_ts, *delta],
            )
            .await?;
        }
        let cursor_key = format!("stats_backfill:v1:cursor:{phase}");
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            libsql::params![cursor_key, new_cursor.to_string()],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Records Maintainer-tab rollup counters (`sashiko_patchsets_completed_total`,
    /// `sashiko_patches_ingested_by_lineage_total`, `sashiko_findings_discovered_total`,
    /// `sashiko_subsystem_findings_total`, and `sashiko_prior_feedback_total`)
    /// once a patchset is terminal and not under an active embargo. Guarded by
    /// `meta` key `stats_rollup:ps_done:<id>` so reruns, backfill, or duplicate
    /// embargo-clear calls never double-count a series.
    pub async fn record_patchset_release_rollups(&self, patchset_id: i64) -> Result<()> {
        let now_ts = chrono::Utc::now().timestamp();
        let tx = self.begin_immediate_transaction().await?;
        let cutoff = read_cutoff_in_tx(&tx, "patchsets").await?;
        let event_ts = if patchset_id <= cutoff {
            let mut rows = tx
                .query(
                    "SELECT COALESCE(date, 0) FROM patchsets WHERE id = ?",
                    libsql::params![patchset_id],
                )
                .await?;
            match rows.next().await? {
                Some(row) => {
                    let ps_date: i64 = row.get(0)?;
                    if ps_date > 0 { ps_date } else { now_ts }
                }
                None => now_ts,
            }
        } else {
            now_ts
        };
        record_single_patchset_release_in_tx(&tx, patchset_id, event_ts, now_ts).await?;
        tx.commit().await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubsystemLinkTarget {
    Patch,
    Patchset,
}

impl SubsystemLinkTarget {
    const fn insert_sql(self) -> &'static str {
        match self {
            Self::Patch => {
                "INSERT OR IGNORE INTO patches_subsystems (patch_id, subsystem_id) VALUES (?, ?)"
            }
            Self::Patchset => {
                "INSERT OR IGNORE INTO patchsets_subsystems (patchset_id, subsystem_id) VALUES (?, ?)"
            }
        }
    }

    const fn cursor_key(self) -> &'static str {
        match self {
            Self::Patch => "stats_backfill:v1:cursor:patches",
            Self::Patchset => "stats_backfill:v1:cursor:patchsets",
        }
    }

    const fn metric(self) -> &'static str {
        match self {
            Self::Patch => "sashiko_subsystem_patches_total",
            Self::Patchset => "sashiko_subsystem_patchsets_total",
        }
    }
}

impl Database {
    /// Atomically inserts a subsystem link row and records the corresponding
    /// `sashiko_subsystem_{patchsets,patches}_total` rollup inside a single
    /// immediate transaction serialized with backfill chunk commits, preventing
    /// any race where both live ingestion and backfill count the same link.
    pub(crate) async fn add_subsystem_link_with_rollup(
        &self,
        target: SubsystemLinkTarget,
        entity_id: i64,
        subsystem_id: i64,
    ) -> Result<()> {
        let now_ts = chrono::Utc::now().timestamp();
        let tx = self.begin_immediate_transaction().await?;
        let inserted = tx
            .execute(
                target.insert_sql(),
                libsql::params![entity_id, subsystem_id],
            )
            .await?;
        if inserted > 0 && table_exists(&tx, "stats_rollups").await? {
            let mut s_rows = tx
                .query(
                    "SELECT name FROM subsystems WHERE id = ?",
                    libsql::params![subsystem_id],
                )
                .await?;
            let sub_name = match s_rows.next().await? {
                Some(sr) => Some(sr.get::<String>(0)?),
                None => None,
            };
            drop(s_rows);

            if let Some(sub_name) = sub_name {
                let mut c_rows = tx
                    .query(
                        "SELECT COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key = ?), 0)",
                        libsql::params![target.cursor_key()],
                    )
                    .await?;
                let cursor = match c_rows.next().await? {
                    Some(r) => r.get::<i64>(0)?,
                    None => 0,
                };
                drop(c_rows);

                if entity_id > cursor {
                    upsert_single_rollup(
                        &tx,
                        now_ts,
                        target.metric(),
                        &canonical_labels_json(&[("subsystem", sub_name.as_str())]),
                        1.0,
                    )
                    .await?;
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Records version-bucketed patchset ingestion once per `patchset_id`
    /// (returning `true` on the first part that registers the series).
    pub async fn record_patchset_ingested_rollup(&self, patchset_id: i64) -> Result<bool> {
        let now_ts = chrono::Utc::now().timestamp();
        let tx = self.begin_immediate_transaction().await?;
        if patchset_id <= read_cutoff_in_tx(&tx, "patchsets").await? {
            tx.rollback().await?;
            return Ok(false);
        }
        let mut rows = tx
            .query(
                "SELECT version FROM patchsets WHERE id = ?",
                libsql::params![patchset_id],
            )
            .await?;
        let Some(r) = rows.next().await? else {
            tx.rollback().await?;
            return Ok(false);
        };
        let ver: Option<i64> = r.get(0).ok();
        drop(rows);

        let guard_key = format!("stats_rollup:ps_ing:{patchset_id}");
        let inserted = tx
            .execute(
                "INSERT OR IGNORE INTO meta (key, value) VALUES (?, '1')",
                libsql::params![guard_key],
            )
            .await?;
        if inserted == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        let vb = crate::worker::stats_backfill::version_bucket_label(ver);
        upsert_single_rollup(
            &tx,
            now_ts,
            "sashiko_patchsets_ingested_by_version_total",
            &canonical_labels_json(&[("version_bucket", vb)]),
            1.0,
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Marks `patch_id` as counted for `sashiko_patches_ingested_total` once per
    /// unique post-upgrade patch row (subsystem links are recorded by
    /// `add_subsystem_to_patch` and finalized lineage by
    /// `record_patchset_release_rollups`).
    pub async fn record_patch_ingested_rollup(&self, patch_id: i64) -> Result<bool> {
        let tx = self.begin_immediate_transaction().await?;
        if patch_id <= read_cutoff_in_tx(&tx, "patches").await? {
            tx.rollback().await?;
            return Ok(false);
        }
        let guard_key = format!("stats_rollup:patch_ing:{patch_id}");
        let inserted = tx
            .execute(
                "INSERT OR IGNORE INTO meta (key, value) VALUES (?, '1')",
                libsql::params![guard_key],
            )
            .await?;
        if inserted == 0 {
            tx.rollback().await?;
            return Ok(false);
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Atomically backfills one bounded `patchsets` ID chunk `(low, cursor]`
    /// inside an immediate transaction so subsystem links and `stats_rollup:ps_done`
    /// release guards serialize cleanly with live worker updates.
    pub async fn backfill_patchsets_chunk(&self, low: i64, cursor: i64, now_ts: i64) -> Result<()> {
        let tx = self.begin_immediate_transaction().await?;
        let mut acc: BTreeMap<(String, String, i64, i64), f64> = BTreeMap::new();
        let mut ps_dates: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        let mut terminal_candidates: Vec<(i64, i64)> = Vec::new();

        let mut rows = tx
            .query(
                "SELECT id, COALESCE(date, 0), COALESCE(status, 'Incomplete'), version, embargo_until
                 FROM patchsets
                 WHERE id > ? AND id <= ? AND date IS NOT NULL AND date > 0",
                libsql::params![low, cursor],
            )
            .await?;
        while let Some(r) = rows.next().await? {
            let pid: i64 = r.get(0)?;
            let ts: i64 = r.get(1)?;
            let status: String = r.get(2)?;
            let ver: Option<i64> = r.get(3).ok();
            let embargo: Option<i64> = r.get(4).ok();
            ps_dates.insert(pid, ts);
            add_rollup_to_map(
                &mut acc,
                now_ts,
                ts,
                "sashiko_patchsets_ingested_by_version_total",
                &canonical_labels_json(&[(
                    "version_bucket",
                    crate::worker::stats_backfill::version_bucket_label(ver),
                )]),
                1.0,
            );
            add_rollup_to_map(
                &mut acc,
                now_ts,
                ts,
                "sashiko_patchsets_ingested_total",
                "{}",
                1.0,
            );
            if embargo.is_none_or(|u| u <= now_ts)
                && matches!(status.as_str(), "Reviewed" | "Failed" | "Failed To Apply")
            {
                terminal_candidates.push((pid, ts));
            }
        }
        drop(rows);

        if !ps_dates.is_empty() {
            let mut sub_rows = tx
                .query(
                    "SELECT pss.patchset_id, s.name
                     FROM patchsets_subsystems pss
                     JOIN subsystems s ON s.id = pss.subsystem_id
                     WHERE pss.patchset_id > ? AND pss.patchset_id <= ?",
                    libsql::params![low, cursor],
                )
                .await?;
            while let Some(sr) = sub_rows.next().await? {
                let pid: i64 = sr.get(0)?;
                let sub: String = sr.get(1)?;
                if let Some(&ts) = ps_dates.get(&pid) {
                    add_rollup_to_map(
                        &mut acc,
                        now_ts,
                        ts,
                        "sashiko_subsystem_patchsets_total",
                        &canonical_labels_json(&[("subsystem", sub.as_str())]),
                        1.0,
                    );
                }
            }
            drop(sub_rows);
        }

        flush_bucket_map_in_tx(&tx, &acc).await?;
        for (pid, event_ts) in terminal_candidates {
            record_single_patchset_release_in_tx(&tx, pid, event_ts, now_ts).await?;
        }
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('stats_backfill:v1:cursor:patchsets', ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            libsql::params![low.to_string()],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Atomically backfills one bounded `patches` ID chunk `(low, cursor]`
    /// inside an immediate transaction serialized with `add_subsystem_to_patch`.
    pub async fn backfill_patches_chunk(&self, low: i64, cursor: i64, now_ts: i64) -> Result<()> {
        let tx = self.begin_immediate_transaction().await?;
        let mut acc: BTreeMap<(String, String, i64, i64), f64> = BTreeMap::new();
        let mut patch_dates: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();

        let mut rows = tx
            .query(
                "SELECT p.id, COALESCE(m.date, 0)
                 FROM patches p
                 JOIN messages m ON m.message_id = p.message_id
                 WHERE p.id > ? AND p.id <= ? AND m.date IS NOT NULL AND m.date > 0",
                libsql::params![low, cursor],
            )
            .await?;
        while let Some(r) = rows.next().await? {
            let id: i64 = r.get(0)?;
            let ts: i64 = r.get(1)?;
            patch_dates.insert(id, ts);
            add_rollup_to_map(
                &mut acc,
                now_ts,
                ts,
                "sashiko_patches_ingested_total",
                "{}",
                1.0,
            );
        }
        drop(rows);

        if !patch_dates.is_empty() {
            let mut sub_rows = tx
                .query(
                    "SELECT ps.patch_id, s.name
                     FROM patches_subsystems ps
                     JOIN subsystems s ON s.id = ps.subsystem_id
                     WHERE ps.patch_id > ? AND ps.patch_id <= ?",
                    libsql::params![low, cursor],
                )
                .await?;
            while let Some(sr) = sub_rows.next().await? {
                let id: i64 = sr.get(0)?;
                let sub: String = sr.get(1)?;
                if let Some(&ts) = patch_dates.get(&id) {
                    add_rollup_to_map(
                        &mut acc,
                        now_ts,
                        ts,
                        "sashiko_subsystem_patches_total",
                        &canonical_labels_json(&[("subsystem", sub.as_str())]),
                        1.0,
                    );
                }
            }
            drop(sub_rows);
        }

        flush_bucket_map_in_tx(&tx, &acc).await?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('stats_backfill:v1:cursor:patches', ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            libsql::params![low.to_string()],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Atomically backfills one bounded `bugs` ID chunk `(low, cursor]` inside
    /// an immediate transaction serialized with `trg_stats_bugs_rollup_lifecycle`
    /// and bounded to pre-migration `bug_enrichments` (`id <= enrich_cutoff`).
    pub async fn backfill_bugs_chunk(
        &self,
        low: i64,
        cursor: i64,
        enrich_cutoff: i64,
        now_ts: i64,
    ) -> Result<()> {
        let tx = self.begin_immediate_transaction().await?;
        let mut acc: BTreeMap<(String, String, i64, i64), f64> = BTreeMap::new();

        let mut rows = tx
            .query(
                "SELECT lifecycle_status, COALESCE(reported_at, created_at, 0),
                        COALESCE(updated_at, created_at, 0)
                 FROM bugs
                 WHERE id > ? AND id <= ?",
                libsql::params![low, cursor],
            )
            .await?;
        while let Some(r) = rows.next().await? {
            let status: String = r.get(0)?;
            let created_ts: i64 = r.get(1)?;
            let updated_ts: i64 = r.get(2)?;
            if matches!(status.as_str(), "open" | "fixed" | "closed") {
                add_rollup_to_map(
                    &mut acc,
                    now_ts,
                    created_ts,
                    "sashiko_bugs_events_total",
                    &canonical_labels_json(&[("event", "discovered")]),
                    1.0,
                );
            }
            if matches!(status.as_str(), "fixed" | "duplicate" | "dismissed") {
                add_rollup_to_map(
                    &mut acc,
                    now_ts,
                    updated_ts,
                    "sashiko_bugs_events_total",
                    &canonical_labels_json(&[("event", status.as_str())]),
                    1.0,
                );
            }
        }
        drop(rows);

        if enrich_cutoff > 0 {
            let mut e_rows = tx
                .query(
                    "SELECT created_at, COALESCE(tokens_in, 0), COALESCE(tokens_out, 0),
                            COALESCE(tokens_cached, 0)
                     FROM bug_enrichments
                     WHERE bug_id > ? AND bug_id <= ? AND id <= ?
                       AND (tokens_in IS NOT NULL OR tokens_out IS NOT NULL OR tokens_cached IS NOT NULL)",
                    libsql::params![low, cursor, enrich_cutoff],
                )
                .await?;
            while let Some(er) = e_rows.next().await? {
                let ts: i64 = er.get(0)?;
                let ti: i64 = er.get(1)?;
                let to: i64 = er.get(2)?;
                let tc: i64 = er.get(3)?;
                let uncached = (ti - tc).max(0);
                if uncached > 0 {
                    add_rollup_to_map(
                        &mut acc,
                        now_ts,
                        ts,
                        "sashiko_ai_tokens_total",
                        &canonical_labels_json(&[
                            ("direction", "uncached_in"),
                            ("workload", "bug"),
                        ]),
                        uncached as f64,
                    );
                }
                if tc > 0 {
                    add_rollup_to_map(
                        &mut acc,
                        now_ts,
                        ts,
                        "sashiko_ai_tokens_total",
                        &canonical_labels_json(&[("direction", "cached_in"), ("workload", "bug")]),
                        tc as f64,
                    );
                }
                if to > 0 {
                    add_rollup_to_map(
                        &mut acc,
                        now_ts,
                        ts,
                        "sashiko_ai_tokens_total",
                        &canonical_labels_json(&[("direction", "out"), ("workload", "bug")]),
                        to as f64,
                    );
                }
            }
            drop(e_rows);
        }

        flush_bucket_map_in_tx(&tx, &acc).await?;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('stats_backfill:v1:cursor:bugs', ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            libsql::params![low.to_string()],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn read_cutoff_in_tx(tx: &DatabaseTransaction, table: &str) -> Result<i64> {
    let key = format!("stats_backfill:cutoff:{table}");
    let mut rows = tx
        .query(
            "SELECT COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key = ?), 0)",
            libsql::params![key],
        )
        .await?;
    let val = match rows.next().await? {
        Some(r) => r.get::<i64>(0)?,
        None => 0,
    };
    Ok(val)
}

fn add_rollup_to_map(
    acc: &mut BTreeMap<(String, String, i64, i64), f64>,
    now_ts: i64,
    event_ts: i64,
    metric: &str,
    labels_json: &str,
    delta: f64,
) {
    if event_ts <= 0 || delta <= 0.0 || !delta.is_finite() {
        return;
    }
    for (res_secs, retention_secs) in ROLLUP_RESOLUTIONS {
        let bucket_ts = (event_ts / res_secs) * res_secs;
        if bucket_ts >= now_ts.saturating_sub(retention_secs) {
            *acc.entry((
                metric.to_string(),
                labels_json.to_string(),
                res_secs,
                bucket_ts,
            ))
            .or_insert(0.0) += delta;
        }
    }
}

async fn flush_bucket_map_in_tx(
    tx: &DatabaseTransaction,
    buckets: &BTreeMap<(String, String, i64, i64), f64>,
) -> Result<()> {
    let mut series_cache: BTreeMap<(String, String), i64> = BTreeMap::new();
    for ((metric, labels_json, res_secs, bucket_ts), delta) in buckets {
        if *delta <= 0.0 || !delta.is_finite() {
            continue;
        }
        let series_id = if let Some(&id) = series_cache.get(&(metric.clone(), labels_json.clone()))
        {
            id
        } else {
            let id = ensure_series_id(tx, metric, labels_json).await?;
            series_cache.insert((metric.clone(), labels_json.clone()), id);
            id
        };
        tx.execute(
            "INSERT INTO stats_rollups (series_id, resolution_secs, bucket_ts, value)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(series_id, resolution_secs, bucket_ts)
             DO UPDATE SET value = value + excluded.value",
            libsql::params![series_id, *res_secs, *bucket_ts, *delta],
        )
        .await?;
    }
    Ok(())
}

async fn record_single_patchset_release_in_tx(
    tx: &DatabaseTransaction,
    patchset_id: i64,
    event_ts: i64,
    now_ts: i64,
) -> Result<()> {
    let mut ps_rows = tx
        .query(
            "SELECT COALESCE(status, 'Incomplete'), embargo_until
             FROM patchsets WHERE id = ?",
            libsql::params![patchset_id],
        )
        .await?;
    let Some(ps_row) = ps_rows.next().await? else {
        return Ok(());
    };
    let status: String = ps_row.get(0)?;
    let embargo_until: Option<i64> = ps_row.get(1).ok();
    drop(ps_rows);

    if embargo_until.is_some_and(|u| u > now_ts)
        || !matches!(status.as_str(), "Reviewed" | "Failed" | "Failed To Apply")
    {
        return Ok(());
    }

    let guard_key = format!("stats_rollup:ps_done:{patchset_id}");
    let existing_guard = {
        let mut g_rows = tx
            .query(
                "SELECT value FROM meta WHERE key = ?",
                libsql::params![guard_key.clone()],
            )
            .await?;
        match g_rows.next().await? {
            Some(r) => Some(r.get::<String>(0)?),
            None => None,
        }
    };
    let mut upgraded_from_non_reviewed = false;
    match existing_guard.as_deref() {
        None => {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES (?, ?)",
                libsql::params![guard_key, status.as_str()],
            )
            .await?;
        }
        Some(prev) if prev != "Reviewed" && prev != "1" && status == "Reviewed" => {
            tx.execute(
                "UPDATE meta SET value = 'Reviewed' WHERE key = ?",
                libsql::params![guard_key],
            )
            .await?;
            upgraded_from_non_reviewed = true;
        }
        Some(_) => return Ok(()),
    }

    let mut patch_lineages: BTreeMap<&'static str, f64> = BTreeMap::new();
    if !upgraded_from_non_reviewed {
        let mut p_rows = tx
            .query(
                "SELECT previous_patch_status FROM patches WHERE patchset_id = ?",
                libsql::params![patchset_id],
            )
            .await?;
        while let Some(pr) = p_rows.next().await? {
            let prev_st: Option<String> = pr.get(0).ok();
            let lin = match prev_st.as_deref() {
                Some("unchanged") => "unchanged",
                Some("modified") => "modified",
                _ => "new",
            };
            *patch_lineages.entry(lin).or_insert(0.0) += 1.0;
        }
        drop(p_rows);
    }

    let mut subsystems = Vec::new();
    let mut findings_list: Vec<(i64, bool)> = Vec::new();
    let mut prior_list: Vec<(String, String, Option<String>)> = Vec::new();
    if status == "Reviewed" {
        let mut sub_rows = tx
            .query(
                "SELECT s.name FROM patchsets_subsystems pss
                 JOIN subsystems s ON s.id = pss.subsystem_id
                 WHERE pss.patchset_id = ?",
                libsql::params![patchset_id],
            )
            .await?;
        while let Some(sr) = sub_rows.next().await? {
            subsystems.push(sr.get::<String>(0)?);
        }
        drop(sub_rows);

        let mut f_rows = tx
            .query(
                "SELECT f.severity, COALESCE(f.preexisting, 0)
                 FROM findings f
                 JOIN reviews r ON r.id = f.review_id
                 WHERE r.patchset_id = ? AND r.status = 'Reviewed'",
                libsql::params![patchset_id],
            )
            .await?;
        while let Some(fr) = f_rows.next().await? {
            findings_list.push((fr.get::<i64>(0)?, fr.get::<i64>(1)? != 0));
        }
        drop(f_rows);

        let mut pr_rows = tx
            .query(
                "SELECT origin_kind, verdict, v_n_outcome
                 FROM prior_feedback_resolutions
                 WHERE patchset_id = ?",
                libsql::params![patchset_id],
            )
            .await?;
        while let Some(pr) = pr_rows.next().await? {
            prior_list.push((pr.get(0)?, pr.get(1)?, pr.get(2).ok()));
        }
        drop(pr_rows);
    }

    let mut acc: BTreeMap<(String, String, i64, i64), f64> = BTreeMap::new();
    let effective_ts = if event_ts > 0 { event_ts } else { now_ts };
    let has_patch_finding = findings_list.iter().any(|(_, preex)| !*preex);
    let clean = if status == "Reviewed" && !has_patch_finding {
        "true"
    } else {
        "false"
    };
    add_rollup_to_map(
        &mut acc,
        now_ts,
        effective_ts,
        "sashiko_patchsets_completed_total",
        &canonical_labels_json(&[("clean", clean), ("status", status.as_str())]),
        1.0,
    );
    if !upgraded_from_non_reviewed {
        for (lin, cnt) in patch_lineages {
            add_rollup_to_map(
                &mut acc,
                now_ts,
                effective_ts,
                "sashiko_patches_ingested_by_lineage_total",
                &canonical_labels_json(&[("lineage", lin)]),
                cnt,
            );
        }
    }
    for (sev_int, preex) in &findings_list {
        let sev = match sev_int {
            4 => "critical",
            3 => "high",
            2 => "medium",
            _ => "low",
        };
        let preex_s = if *preex { "true" } else { "false" };
        add_rollup_to_map(
            &mut acc,
            now_ts,
            effective_ts,
            "sashiko_findings_discovered_total",
            &canonical_labels_json(&[("preexisting", preex_s), ("severity", sev)]),
            1.0,
        );
        if !*preex {
            for sub in &subsystems {
                add_rollup_to_map(
                    &mut acc,
                    now_ts,
                    effective_ts,
                    "sashiko_subsystem_findings_total",
                    &canonical_labels_json(&[("severity", sev), ("subsystem", sub.as_str())]),
                    1.0,
                );
            }
        }
    }
    for (origin, verdict, vn_out) in &prior_list {
        let outcome =
            crate::worker::stats_backfill::normalize_prior_outcome(verdict, vn_out.as_deref());
        add_rollup_to_map(
            &mut acc,
            now_ts,
            effective_ts,
            "sashiko_prior_feedback_total",
            &canonical_labels_json(&[("origin_kind", origin.as_str()), ("outcome", outcome)]),
            1.0,
        );
    }
    flush_bucket_map_in_tx(tx, &acc).await
}

async fn upsert_single_rollup(
    tx: &DatabaseTransaction,
    event_ts: i64,
    metric: &str,
    labels_json: &str,
    delta: f64,
) -> Result<()> {
    let clamped_ts = event_ts.max(0);
    let series_id = ensure_series_id(tx, metric, labels_json).await?;
    for (res_secs, _) in ROLLUP_RESOLUTIONS {
        let bucket_ts = (clamped_ts / res_secs) * res_secs;
        tx.execute(
            "INSERT INTO stats_rollups (series_id, resolution_secs, bucket_ts, value)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(series_id, resolution_secs, bucket_ts)
             DO UPDATE SET value = value + excluded.value",
            libsql::params![series_id, res_secs, bucket_ts, delta],
        )
        .await?;
    }
    Ok(())
}

pub(super) async fn flush_rollup_samples_in_tx(
    tx: &DatabaseTransaction,
    event_ts: i64,
    samples: &[RollupSample],
) -> Result<()> {
    let clamped_ts = event_ts.max(0);
    for sample in samples {
        if sample.delta <= 0.0 || !sample.delta.is_finite() {
            continue;
        }
        let labels_json = canonical_labels_json(&sample.labels);
        let series_id = ensure_series_id(tx, sample.metric, &labels_json).await?;
        for (res_secs, _) in ROLLUP_RESOLUTIONS {
            let bucket_ts = (clamped_ts / res_secs) * res_secs;
            tx.execute(
                "INSERT INTO stats_rollups (series_id, resolution_secs, bucket_ts, value)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(series_id, resolution_secs, bucket_ts)
                 DO UPDATE SET value = value + excluded.value",
                libsql::params![series_id, res_secs, bucket_ts, sample.delta],
            )
            .await?;
        }
    }
    Ok(())
}

pub(super) async fn ensure_series_id(
    tx: &DatabaseTransaction,
    metric: &str,
    labels_json: &str,
) -> Result<i64> {
    tx.execute(
        "INSERT OR IGNORE INTO stats_series (metric, labels_json) VALUES (?, ?)",
        libsql::params![metric, labels_json],
    )
    .await?;
    let mut rows = tx
        .query(
            "SELECT id FROM stats_series WHERE metric = ? AND labels_json = ?",
            libsql::params![metric, labels_json],
        )
        .await?;
    let row = rows
        .next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing stats_series row for {metric}"))?;
    let id: i64 = row.get(0)?;
    drop(rows);
    Ok(id)
}

fn fold_subsystem_facet_row(
    map: &mut BTreeMap<String, BugSubsystemFacetSummary>,
    row: &libsql::Row,
) -> Result<()> {
    let sub: String = row.get(0)?;
    let status: String = row.get(1)?;
    let sev_int: i64 = row.get(2)?;
    let sec_rating: String = row.get(3)?;
    let cnt = row.get::<i64>(4)?.max(0) as u64;
    if cnt == 0 {
        return Ok(());
    }
    let entry = map
        .entry(sub.clone())
        .or_insert_with(|| BugSubsystemFacetSummary {
            subsystem: sub,
            ..Default::default()
        });
    entry.total += cnt;
    match status.as_str() {
        "open" => entry.open += cnt,
        "fixed" => entry.fixed += cnt,
        "duplicate" => entry.duplicate += cnt,
        "dismissed" => entry.dismissed += cnt,
        "new" => entry.new_untriaged += cnt,
        "closed" => entry.closed += cnt,
        _ => {}
    }
    match sev_int {
        4 => entry.sev_critical += cnt,
        3 => entry.sev_high += cnt,
        2 => entry.sev_medium += cnt,
        1 => entry.sev_low += cnt,
        _ => {}
    }
    match sec_rating.as_str() {
        "critical" => entry.sec_critical += cnt,
        "high" => entry.sec_high += cnt,
        "medium" => entry.sec_medium += cnt,
        "low" => entry.sec_low += cnt,
        "hardening" => entry.sec_hardening += cnt,
        _ => {}
    }
    if status == "open" {
        match sev_int {
            4 => entry.open_sev_critical += cnt,
            3 => entry.open_sev_high += cnt,
            _ => {}
        }
        match sec_rating.as_str() {
            "critical" => entry.open_sec_critical += cnt,
            "high" => entry.open_sec_high += cnt,
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DatabaseSettings;

    async fn setup_migrated_db() -> Database {
        let db = Database::new(&DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await
        .expect("db init");
        db.migrate().await.expect("db migrate");
        db
    }

    #[tokio::test]
    async fn test_migration_23_triggers_and_rewind_idempotency() -> Result<()> {
        let db = setup_migrated_db().await;

        db.execute(
            "INSERT INTO messages (message_id, subject, date) VALUES ('<m1>', 'subj', 1700000000)",
            (),
        )
        .await?;
        db.execute(
            "INSERT INTO patchsets (id, subject, status, date) VALUES (10, 'ps', 'Pending', 1700000000)",
            (),
        )
        .await?;
        db.execute(
            "INSERT INTO email_outbox (status, created_at) VALUES ('Pending', 1700000000)",
            (),
        )
        .await?;
        assert_eq!(db.count_messages_stored_o1().await?, 1);
        assert_eq!(db.count_patchsets_stored_o1().await?, 1);
        assert_eq!(
            db.get_state_gauge(
                "sashiko_patchsets_stored",
                &canonical_labels_json(&[("status", "Pending")])
            )
            .await?,
            1
        );

        // Update patchset status -> gauge transitions cleanly.
        db.execute("UPDATE patchsets SET status = 'Reviewed' WHERE id = 10", ())
            .await?;
        assert_eq!(
            db.get_state_gauge(
                "sashiko_patchsets_stored",
                &canonical_labels_json(&[("status", "Pending")])
            )
            .await?,
            0
        );
        assert_eq!(
            db.get_state_gauge(
                "sashiko_patchsets_stored",
                &canonical_labels_json(&[("status", "Reviewed")])
            )
            .await?,
            1
        );

        // Rewind user_version to 23 and re-run migrate() -> counts remain 1 (never doubled).
        db.execute("PRAGMA user_version = 23", ()).await?;
        db.migrate().await?;
        assert_eq!(db.count_messages_stored_o1().await?, 1);
        assert_eq!(db.count_patchsets_stored_o1().await?, 1);
        assert_eq!(
            db.get_state_gauge(
                "sashiko_outbox_depth",
                &canonical_labels_json(&[("channel", "email"), ("status", "Pending")])
            )
            .await?,
            1
        );

        // Verify reparenting a review across patchset merges preserves review_pipeline_stats
        // when the merged-from patchset row is subsequently deleted.
        db.execute(
            "INSERT INTO patchsets (id, subject, status, date) VALUES (11, 'ps2', 'Reviewed', 1700000001)",
            (),
        )
        .await?;
        db.execute(
            "INSERT INTO reviews (id, patchset_id, status, created_at) VALUES (50, 11, 'Reviewed', 1700000002)",
            (),
        )
        .await?;
        db.execute(
            "INSERT INTO review_pipeline_stats (review_id, patchset_id, created_at, status) VALUES (50, 11, 1700000002, 'Reviewed')",
            (),
        )
        .await?;
        db.execute("UPDATE reviews SET patchset_id = 10 WHERE id = 50", ())
            .await?;
        db.execute("DELETE FROM patchsets WHERE id = 11", ())
            .await?;
        let mut rps_rows = db
            .read_conn()
            .query(
                "SELECT patchset_id FROM review_pipeline_stats WHERE review_id = 50",
                (),
            )
            .await?;
        let rps_pid: i64 = rps_rows.next().await?.expect("rps survived merge").get(0)?;
        drop(rps_rows);
        assert_eq!(rps_pid, 10);

        Ok(())
    }

    #[tokio::test]
    async fn test_bug_triggers_dedup_sections_enforce_visibility_and_handle_delete() -> Result<()> {
        let db = setup_migrated_db().await;

        db.execute(
            "INSERT INTO bugs (id, bugid, title, lifecycle_status, pipeline_state, reporter, reported_at, severity_int, created_at, updated_at)
             VALUES (1, 'linux-1111', 'net race', 'open', 'succeeded', 'bot', 100, 3, 100, 100)",
            (),
        )
        .await?;
        // Same section 'NETWORKING' added via both MAINTAINERS and upstream_grant must count once;
        // caller_supplied must be ignored.
        db.execute(
            "INSERT INTO bug_subsystems (bug_id, subsystem, source) VALUES
             (1, 'NETWORKING', 'maintainers_section'),
             (1, 'net/core', 'path_prefix'),
             (1, 'BOGUS_CALLER', 'caller_supplied')",
            (),
        )
        .await?;
        db.execute(
            "INSERT INTO bug_upstream_sections (bug_id, subsystem) VALUES
             (1, 'NETWORKING'),
             (1, 'BPF')",
            (),
        )
        .await?;

        // Two reproducer comments on the same bug must count as 1 bug with reproducer.
        db.execute(
            "INSERT INTO bug_enrichments (bug_id, kind, tool, created_at, content) VALUES
             (1, 'reproducer', 'human', 101, 'repro 1'),
             (1, 'poc', 'human', 102, 'repro 2')",
            (),
        )
        .await?;
        assert_eq!(
            db.get_state_gauge(
                "sashiko_bugs_with_attachment",
                &canonical_labels_json(&[("attachment", "reproducer"), ("status", "open")])
            )
            .await?,
            1
        );

        // Security evaluation enrichment trigger chain updates bugs AND subsystem facets.
        db.execute(
            "INSERT INTO bug_enrichments (bug_id, kind, tool, created_at, data_json)
             VALUES (1, 'security_evaluation', 'sashiko', 105, '{\"security_class\":\"vulnerability\",\"score_tenths\":82}')",
            (),
        )
        .await?;

        let all = db
            .get_bug_subsystem_facets_o1(None, BugVisibility::Unrestricted, 50)
            .await?;
        assert_eq!(all.len(), 2); // NETWORKING and BPF (authorizing only; BOGUS_CALLER ignored)
        assert_eq!(all[0].total, 1);
        assert_eq!(all[0].open, 1);
        assert_eq!(all[0].sev_high, 1);
        assert_eq!(all[0].sec_high, 1);

        // Maintainer scoped to ["networking"] (case-insensitive) sees only NETWORKING.
        let scoped = db
            .get_bug_subsystem_facets_o1(
                Some(BugLifecycleStatus::Open),
                BugVisibility::Sections(&["networking".to_string()]),
                50,
            )
            .await?;
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].subsystem, "NETWORKING");
        assert_eq!(scoped[0].open, 1);

        // Empty scope sees nothing (zero oracle).
        let none = db
            .get_bug_subsystem_facets_o1(None, BugVisibility::Sections(&[]), 50)
            .await?;
        assert!(none.is_empty());

        // Deleting one of the two overlapping NETWORKING attributions keeps count = 1.
        db.execute(
            "DELETE FROM bug_upstream_sections WHERE bug_id = 1 AND subsystem = 'NETWORKING'",
            (),
        )
        .await?;
        let after_partial = db
            .get_bug_subsystem_facets_o1(
                None,
                BugVisibility::Sections(&["NETWORKING".to_string()]),
                50,
            )
            .await?;
        assert_eq!(after_partial.len(), 1);
        assert_eq!(after_partial[0].total, 1);

        // Deleting the bug itself decrements global gauges, attachments, and subsystem facets to 0.
        db.execute("DELETE FROM bugs WHERE id = 1", ()).await?;
        assert_eq!(db.sum_state_gauge_metric("sashiko_bugs_total").await?, 0);
        assert_eq!(
            db.sum_state_gauge_metric("sashiko_bugs_with_attachment")
                .await?,
            0
        );
        let after_del = db
            .get_bug_subsystem_facets_o1(None, BugVisibility::Unrestricted, 50)
            .await?;
        assert!(after_del.is_empty());

        // Verify top_subs SQL truncation prioritizes open bugs over larger dismissed totals.
        for id in 10..=14 {
            db.execute(
                "INSERT INTO bugs (id, bugid, title, lifecycle_status, pipeline_state, reporter, reported_at, severity_int, created_at, updated_at)
                 VALUES (?, ?, 'dismissed noise', 'dismissed', 'succeeded', 'bot', 200, 1, 200, 200)",
                libsql::params![id, format!("linux-d{id}")],
            )
            .await?;
            db.execute(
                "INSERT INTO bug_subsystems (bug_id, subsystem, source) VALUES (?, 'NOISY_SUB', 'maintainers_section')",
                libsql::params![id],
            )
            .await?;
        }
        db.execute(
            "INSERT INTO bugs (id, bugid, title, lifecycle_status, pipeline_state, reporter, reported_at, severity_int, created_at, updated_at)
             VALUES (20, 'linux-open20', 'active bug', 'open', 'succeeded', 'bot', 200, 4, 200, 200)",
            (),
        )
        .await?;
        db.execute(
            "INSERT INTO bug_subsystems (bug_id, subsystem, source) VALUES (20, 'ACTIVE_SUB', 'maintainers_section')",
            (),
        )
        .await?;

        let top1 = db
            .get_bug_subsystem_facets_o1(None, BugVisibility::Unrestricted, 1)
            .await?;
        assert_eq!(top1.len(), 1);
        assert_eq!(top1[0].subsystem, "ACTIVE_SUB");
        assert_eq!(top1[0].open, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_pre_upgrade_embargo_release_uses_historical_patchset_date() -> Result<()> {
        let db = setup_migrated_db().await;
        let now = chrono::Utc::now().timestamp();
        let old_ts = now - 30 * 86_400;
        let expected_day_bucket = (old_ts / 86_400) * 86_400;

        db.execute(
            "INSERT INTO patchsets (id, subject, status, date, version, embargo_until)
             VALUES (1, '[PATCH] old failed apply', 'Failed To Apply', ?, 1, ?)",
            libsql::params![old_ts, old_ts + 3600],
        )
        .await?;
        db.execute(
            "INSERT INTO meta (key, value) VALUES ('stats_backfill:cutoff:patchsets', '1')
             ON CONFLICT(key) DO UPDATE SET value = '1'",
            (),
        )
        .await?;

        db.clear_patchset_embargo(1).await?;

        let mut rows = db
            .read_conn()
            .query(
                "SELECT sr.bucket_ts, sr.value
                 FROM stats_rollups sr
                 JOIN stats_series ss ON ss.id = sr.series_id
                 WHERE ss.metric = 'sashiko_patchsets_completed_total'
                   AND sr.resolution_secs = 86400",
                (),
            )
            .await?;
        let row = rows.next().await?.expect("historical rollup bucket");
        let bucket_ts: i64 = row.get(0)?;
        let val: f64 = row.get(1)?;
        assert_eq!(bucket_ts, expected_day_bucket);
        assert!((val - 1.0).abs() < f64::EPSILON);
        Ok(())
    }

    #[tokio::test]
    async fn test_cancel_aborted_review_updates_description_when_already_cancelled() -> Result<()> {
        let db = setup_migrated_db().await;
        let now = chrono::Utc::now().timestamp();
        db.execute(
            "INSERT INTO patchsets (id, subject, status, date, version)
             VALUES (1, '[PATCH] cancelled series', 'Cancelled', ?, 1)",
            libsql::params![now],
        )
        .await?;
        db.execute(
            "INSERT INTO reviews (id, patchset_id, status, created_at)
             VALUES (1, 1, 'Cancelled', ?)",
            libsql::params![now],
        )
        .await?;

        db.cancel_aborted_review(1, "aborted by worker unwind")
            .await?;

        let mut rows = db
            .read_conn()
            .query(
                "SELECT status, result_description FROM reviews WHERE id = 1",
                (),
            )
            .await?;
        let row = rows.next().await?.expect("review row");
        assert_eq!(row.get::<String>(0)?, "Cancelled");
        assert_eq!(row.get::<String>(1)?, "aborted by worker unwind");
        Ok(())
    }

    #[tokio::test]
    async fn test_failed_patchset_rerun_upgrades_release_guard_to_reviewed() -> Result<()> {
        let db = setup_migrated_db().await;
        let now = chrono::Utc::now().timestamp();
        db.execute(
            "INSERT INTO messages (id, message_id, subject, date) VALUES
             (1, '<m1@ex>', '[PATCH 1/2] fix', ?),
             (2, '<m2@ex>', '[PATCH 2/2] fix', ?)",
            libsql::params![now, now],
        )
        .await?;
        db.execute(
            "INSERT INTO patchsets (id, subject, status, date, version)
             VALUES (1, '[PATCH 0/2] fix', 'Failed', ?, 2)",
            libsql::params![now],
        )
        .await?;
        db.execute(
            "INSERT INTO patches (id, patchset_id, message_id, part_index) VALUES
             (1, 1, '<m1@ex>', 1),
             (2, 1, '<m2@ex>', 2)",
            (),
        )
        .await?;
        db.execute(
            "INSERT INTO subsystems (id, name, mailing_list_address) VALUES (1, 'mm', 'linux-mm@kvack.org')",
            (),
        )
        .await?;
        db.execute(
            "INSERT INTO patchsets_subsystems (patchset_id, subsystem_id) VALUES (1, 1)",
            (),
        )
        .await?;
        // Patch 1 already finished Reviewed with a finding and prior feedback, while
        // patch 2 failed, leaving the series in Failed before rerun_patchset().
        db.execute(
            "INSERT INTO reviews (id, patchset_id, patch_id, status, created_at) VALUES
             (1, 1, 1, 'Reviewed', ?),
             (2, 1, 2, 'Failed', ?)",
            libsql::params![now, now],
        )
        .await?;
        db.execute(
            "INSERT INTO findings (id, review_id, severity, preexisting) VALUES (1, 1, 3, 0)",
            (),
        )
        .await?;
        db.execute(
            "INSERT INTO prior_feedback_resolutions
             (id, review_id, patchset_id, patch_id, prior_id, origin_kind, verdict, v_n_outcome)
             VALUES (1, 1, 1, 1, 'p1', 'human', 'addressed', 'addressed')",
            (),
        )
        .await?;

        // Initial failure records Failed + lineage once, and defers findings/prior feedback
        // until the patchset actually reaches Reviewed.
        db.record_patchset_release_rollups(1).await?;
        db.record_patchset_release_rollups(1).await?;

        for (metric, labels, expected) in [
            (
                "sashiko_patchsets_completed_total",
                r#"{"clean":"false","status":"Failed"}"#,
                1.0f64,
            ),
            (
                "sashiko_findings_discovered_total",
                r#"{"preexisting":"false","severity":"high"}"#,
                0.0f64,
            ),
            (
                "sashiko_subsystem_findings_total",
                r#"{"severity":"high","subsystem":"mm"}"#,
                0.0f64,
            ),
            (
                "sashiko_prior_feedback_total",
                r#"{"origin_kind":"human","outcome":"addressed"}"#,
                0.0f64,
            ),
        ] {
            let mut rows = db
                .read_conn()
                .query(
                    "SELECT COALESCE(SUM(sr.value), 0.0)
                     FROM stats_rollups sr
                     JOIN stats_series ss ON ss.id = sr.series_id
                     WHERE ss.metric = ? AND ss.labels_json = ? AND sr.resolution_secs = 86400",
                    libsql::params![metric, labels],
                )
                .await?;
            let val: f64 = rows.next().await?.expect("sum row").get(0)?;
            assert!(
                (val - expected).abs() < f64::EPSILON,
                "pre-rerun: expected {expected} for {metric}{labels}, got {val}"
            );
        }

        // Simulate rerun_patchset() retrying only patch 2 while preserving patch 1's
        // Reviewed row, finding, and prior feedback resolution.
        db.execute("UPDATE reviews SET status = 'Reviewed' WHERE id = 2", ())
            .await?;
        db.execute("UPDATE patchsets SET status = 'Reviewed' WHERE id = 1", ())
            .await?;

        // Upgrading the guard records Reviewed + findings + prior feedback once without
        // double-counting lineage; a second Reviewed call is strictly idempotent.
        db.record_patchset_release_rollups(1).await?;
        db.record_patchset_release_rollups(1).await?;

        for (metric, labels, expected) in [
            (
                "sashiko_patchsets_completed_total",
                r#"{"clean":"false","status":"Failed"}"#,
                1.0f64,
            ),
            (
                "sashiko_patchsets_completed_total",
                r#"{"clean":"false","status":"Reviewed"}"#,
                1.0f64,
            ),
            (
                "sashiko_findings_discovered_total",
                r#"{"preexisting":"false","severity":"high"}"#,
                1.0f64,
            ),
            (
                "sashiko_subsystem_findings_total",
                r#"{"severity":"high","subsystem":"mm"}"#,
                1.0f64,
            ),
            (
                "sashiko_prior_feedback_total",
                r#"{"origin_kind":"human","outcome":"addressed"}"#,
                1.0f64,
            ),
            (
                "sashiko_patches_ingested_by_lineage_total",
                r#"{"lineage":"new"}"#,
                2.0f64,
            ),
        ] {
            let mut rows = db
                .read_conn()
                .query(
                    "SELECT COALESCE(SUM(sr.value), 0.0)
                     FROM stats_rollups sr
                     JOIN stats_series ss ON ss.id = sr.series_id
                     WHERE ss.metric = ? AND ss.labels_json = ? AND sr.resolution_secs = 86400",
                    libsql::params![metric, labels],
                )
                .await?;
            let val: f64 = rows.next().await?.expect("sum row").get(0)?;
            assert!(
                (val - expected).abs() < f64::EPSILON,
                "expected {expected} for {metric}{labels}, got {val}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_patchset_clean_rollup_excludes_preexisting_findings() -> Result<()> {
        let db = setup_migrated_db().await;
        let now = chrono::Utc::now().timestamp();
        db.execute(
            "INSERT INTO patchsets (id, subject, status, date, version) VALUES
             (1, '[PATCH] clean with preexisting only', 'Reviewed', ?, 1),
             (2, '[PATCH] dirty with patch finding', 'Reviewed', ?, 1)",
            libsql::params![now, now],
        )
        .await?;
        db.execute(
            "INSERT INTO reviews (id, patchset_id, status, created_at) VALUES
             (1, 1, 'Reviewed', ?),
             (2, 2, 'Reviewed', ?)",
            libsql::params![now, now],
        )
        .await?;
        db.execute(
            "INSERT INTO findings (id, review_id, severity, preexisting) VALUES
             (1, 1, 3, 1),
             (2, 2, 2, 0)",
            (),
        )
        .await?;

        db.record_patchset_release_rollups(1).await?;
        db.record_patchset_release_rollups(2).await?;

        for (labels, expected) in [
            (r#"{"clean":"true","status":"Reviewed"}"#, 1.0f64),
            (r#"{"clean":"false","status":"Reviewed"}"#, 1.0f64),
        ] {
            let mut rows = db
                .read_conn()
                .query(
                    "SELECT COALESCE(SUM(sr.value), 0)
                     FROM stats_rollups sr
                     JOIN stats_series ss ON ss.id = sr.series_id
                     WHERE ss.metric = 'sashiko_patchsets_completed_total'
                       AND ss.labels_json = ?
                       AND sr.resolution_secs = 86400",
                    libsql::params![labels],
                )
                .await?;
            let val: f64 = rows.next().await?.expect("sum row").get(0)?;
            assert!(
                (val - expected).abs() < f64::EPSILON,
                "expected {expected} for {labels}, got {val}"
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_bug_subsystem_open_severity_facets_exclude_non_open_bugs() -> Result<()> {
        let db = setup_migrated_db().await;
        let now = chrono::Utc::now().timestamp();
        db.execute(
            "INSERT INTO bugs (id, bugid, title, lifecycle_status, pipeline_state, reporter, reported_at, severity_int, security_score_tenths, security_class, created_at, updated_at)
             VALUES
             (1, 'linux-o1', 'open crit', 'open', 'succeeded', 'bot', ?, 4, 95, 'vulnerability', ?, ?),
             (2, 'linux-f1', 'fixed crit', 'fixed', 'succeeded', 'bot', ?, 4, 95, 'vulnerability', ?, ?),
             (3, 'linux-d1', 'dismissed high', 'dismissed', 'succeeded', 'bot', ?, 3, 75, 'vulnerability', ?, ?)",
            libsql::params![now, now, now, now, now, now, now, now, now],
        )
        .await?;
        db.execute(
            "INSERT INTO bug_subsystems (bug_id, subsystem, source) VALUES
             (1, 'NET', 'maintainers_section'),
             (2, 'NET', 'maintainers_section'),
             (3, 'NET', 'maintainers_section')",
            (),
        )
        .await?;

        let facets = db
            .get_bug_subsystem_facets_o1(None, BugVisibility::Unrestricted, 50)
            .await?;
        assert_eq!(facets.len(), 1);
        assert_eq!(facets[0].open, 1);
        assert_eq!(facets[0].open_sev_critical, 1);
        assert_eq!(facets[0].open_sev_high, 0);
        assert_eq!(facets[0].open_sec_critical, 1);
        assert_eq!(facets[0].open_sec_high, 0);
        assert_eq!(facets[0].sev_critical, 2);
        assert_eq!(facets[0].sec_high, 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_rollups_flush_reap_and_o1_query_plans() -> Result<()> {
        let db = setup_migrated_db().await;
        let samples = vec![RollupSample {
            metric: "sashiko_messages_ingested_total",
            labels: vec![("source", "nntp")],
            delta: 12.0,
        }];
        db.flush_rollup_samples(1_700_000_120, &samples).await?;
        db.flush_rollup_samples(1_700_000_180, &samples).await?;

        // Both timestamps fall in the same 5m (300s) bucket 1_700_000_100 -> summed to 24.0.
        let mut rows = db
            .read_conn()
            .query(
                "SELECT r.value FROM stats_rollups r
                 JOIN stats_series s ON s.id = r.series_id
                 WHERE s.metric = 'sashiko_messages_ingested_total' AND r.resolution_secs = 300",
                (),
            )
            .await?;
        let val: f64 = rows.next().await?.expect("rollup row").get(0)?;
        drop(rows);
        assert!((val - 24.0).abs() < f64::EPSILON);

        // Advance clock past 48h -> 5m bucket reaped while 1h and 1d buckets remain.
        let deleted = db
            .reap_expired_rollups(1_700_000_120 + 49 * 3600, 500)
            .await?;
        assert_eq!(deleted, 1);

        // Verify EXPLAIN QUERY PLAN never scans unbounded application tables.
        for sql in [
            "SELECT value FROM stats_state_gauges WHERE metric = 'sashiko_messages_stored' AND labels_json = '{}'",
            "SELECT r.bucket_ts, r.value FROM stats_rollups r WHERE r.series_id = 1 AND r.resolution_secs = 3600 AND r.bucket_ts BETWEEN 1000 AND 50000",
            "SELECT subsystem, lifecycle_status, severity_int, security_rating, count FROM stats_bug_subsystem_facets WHERE is_authorizing = 1 AND count > 0 AND subsystem COLLATE NOCASE IN ('NET')",
        ] {
            let mut plan_rows = db
                .read_conn()
                .query(&format!("EXPLAIN QUERY PLAN {sql}"), ())
                .await?;
            while let Some(pr) = plan_rows.next().await? {
                let detail: String = pr.get(3)?;
                for forbidden in [
                    "SCAN messages",
                    "SCAN patchsets",
                    "SCAN patches",
                    "SCAN reviews",
                    "SCAN findings",
                    "SCAN bugs",
                ] {
                    assert!(
                        !detail.contains(forbidden),
                        "O(1) stats query plan must never contain {forbidden}: {detail}"
                    );
                }
            }
        }

        Ok(())
    }
}
