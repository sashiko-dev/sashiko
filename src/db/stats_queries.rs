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
                ("email_outbox", has_email_outbox),
                ("forge_outbox", has_forge_outbox),
                ("patchwork_outbox", has_patchwork_outbox),
            ],
        )
        .await?;
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
        let key = format!("stats_backfill:cutoff:{table}");
        if *present {
            tx.execute(
                &format!(
                    "INSERT OR IGNORE INTO meta (key, value)
                     VALUES (?, CAST(COALESCE((SELECT MAX(id) FROM {table}), 0) AS TEXT))"
                ),
                libsql::params![key],
            )
            .await?;
        } else {
            tx.execute(
                "INSERT OR IGNORE INTO meta (key, value) VALUES (?, '0')",
                libsql::params![key],
            )
            .await?;
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
