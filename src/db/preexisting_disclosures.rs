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

//! Records of pre-existing bugs that a review mentioned to the patch author.
//!
//! See designs/DESIGN_RELATED_PREEXISTING_BUGS.md. A review reserves a row
//! per bug before mailing it; the reservation checks the rows already present
//! in the same files inside one write transaction, so concurrent reviews can
//! never report the same bug twice. Rows are keyed by the Message-ID of the
//! reviewed patch, which survives patchset merges, reruns and embargo
//! re-queues that rewrite patch, review and outbox rows.

use super::{Database, DisclosureRelation, Severity};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

/// Bound on bound parameters per `IN (...)` lookup.
const FILE_CHUNK_SIZE: usize = 100;

/// How long an unmailed reservation holds its bug. A review can be dropped
/// between reserving and mailing in many ways (cancellation, a failed
/// sibling patch, a crash), so instead of releasing on every such path a
/// reservation simply stops counting, and stops being mailable, once it is
/// this old. It comfortably exceeds any embargo plus review time.
pub const RESERVATION_TTL_SECS: i64 = 14 * 24 * 3600;

/// SQL condition keeping rows that still hold their bug: mailed ones, and
/// reservations younger than `RESERVATION_TTL_SECS` (bound as `?`, the
/// oldest live `reserved_at`).
const LIVE_ROW: &str = "(d.disclosed_at IS NOT NULL OR d.reserved_at > ?)";

/// A related pre-existing bug a review wants to report.
#[derive(Debug, Clone)]
pub struct NewPreexistingDisclosure {
    pub fingerprint: String,
    pub problem: String,
    pub severity: Severity,
    pub relation: DisclosureRelation,
    pub locations_json: String,
    pub bug_key_json: String,
    pub report_text: String,
    /// Files the bug's locations point at; duplicate checks look here.
    pub files: Vec<String>,
}

/// A recorded disclosure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreexistingDisclosure {
    pub id: i64,
    pub patch_message_id: String,
    pub fingerprint: String,
    pub problem: String,
    pub severity: Severity,
    pub relation: DisclosureRelation,
    pub locations_json: String,
    pub bug_key_json: String,
    pub report_text: String,
    pub reserved_at: i64,
    /// When the review email carrying it was queued; `None` while reserved.
    pub disclosed_at: Option<i64>,
    pub bug_id: Option<i64>,
    pub files: Vec<String>,
}

const DISCLOSURE_COLUMNS: &str = "d.id, d.patch_message_id, d.fingerprint, d.problem, d.severity, \
     d.relation, d.locations_json, d.bug_key_json, d.report_text, d.reserved_at, d.disclosed_at, \
     d.bug_id";

fn disclosure_from_row(row: &libsql::Row) -> Result<PreexistingDisclosure> {
    let relation: String = row.get(5)?;
    Ok(PreexistingDisclosure {
        id: row.get(0)?,
        patch_message_id: row.get(1)?,
        fingerprint: row.get(2)?,
        problem: row.get(3)?,
        severity: Severity::from_i32(row.get(4)?),
        relation: DisclosureRelation::from_db_str(&relation)?,
        locations_json: row.get(6)?,
        bug_key_json: row.get(7)?,
        report_text: row.get(8)?,
        reserved_at: row.get(9)?,
        disclosed_at: row.get(10)?,
        bug_id: row.get(11)?,
        files: Vec::new(),
    })
}

/// Fills `files` for each disclosure in `found`.
async fn attach_files(
    conn: &libsql::Connection,
    found: &mut BTreeMap<i64, PreexistingDisclosure>,
) -> Result<()> {
    let ids: Vec<i64> = found.keys().copied().collect();
    for chunk in ids.chunks(FILE_CHUNK_SIZE) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT disclosure_id, file FROM preexisting_disclosure_files
              WHERE disclosure_id IN ({placeholders}) ORDER BY disclosure_id, file"
        );
        let params: Vec<libsql::Value> =
            chunk.iter().map(|id| libsql::Value::Integer(*id)).collect();
        let mut rows = conn.query(&sql, params).await?;
        while let Some(row) = rows.next().await? {
            let id: i64 = row.get(0)?;
            if let Some(d) = found.get_mut(&id) {
                d.files.push(row.get(1)?);
            }
        }
    }
    Ok(())
}

/// Live disclosures pointing at any of `files`, ordered by id.
async fn disclosures_in_files_on(
    conn: &libsql::Connection,
    files: &[String],
    now: i64,
) -> Result<Vec<PreexistingDisclosure>> {
    let unique_files: Vec<&str> = files
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut found: BTreeMap<i64, PreexistingDisclosure> = BTreeMap::new();
    for chunk in unique_files.chunks(FILE_CHUNK_SIZE) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let sql = format!(
            "SELECT DISTINCT {DISCLOSURE_COLUMNS}
               FROM preexisting_disclosures d
               JOIN preexisting_disclosure_files f ON f.disclosure_id = d.id
              WHERE f.file IN ({placeholders}) AND {LIVE_ROW}"
        );
        let mut params: Vec<libsql::Value> = chunk
            .iter()
            .map(|f| libsql::Value::Text((*f).to_string()))
            .collect();
        params.push(libsql::Value::Integer(now - RESERVATION_TTL_SECS));
        let mut rows = conn.query(&sql, params).await?;
        while let Some(row) = rows.next().await? {
            let disclosure = disclosure_from_row(&row)?;
            found.insert(disclosure.id, disclosure);
        }
    }
    attach_files(conn, &mut found).await?;
    Ok(found.into_values().collect())
}

impl Database {
    /// Reserves the related pre-existing bugs the review of
    /// `patch_message_id` wants to report, returning the rows it won.
    ///
    /// The review's own earlier reservations (a rerun) are replaced first.
    /// Every other row in the candidate's files counts, including rows of
    /// other patches in the same series and rows this patch already mailed,
    /// so a bug is reported at most once. A worker whose patchset claim was
    /// superseded changes nothing. A candidate is skipped when
    /// `is_duplicate` matches any of them or an earlier candidate of this
    /// call. Everything runs in one IMMEDIATE transaction, which serializes
    /// concurrent reservations.
    pub async fn reserve_preexisting_disclosures<F>(
        &self,
        patch_message_id: &str,
        candidates: &[NewPreexistingDisclosure],
        is_duplicate: F,
    ) -> Result<Vec<PreexistingDisclosure>>
    where
        F: Fn(&NewPreexistingDisclosure, &PreexistingDisclosure) -> bool,
    {
        let now = chrono::Utc::now().timestamp();
        let tx = self.begin_immediate_transaction().await?;
        // A worker superseded by a rerun or cancellation must not replace the
        // reservations of the generation that took over.
        if !self
            .with_connection((*tx).clone())
            .is_patchset_claim_active_for_outbox(None)
            .await?
        {
            return Ok(Vec::new());
        }
        tx.execute(
            "DELETE FROM preexisting_disclosures
              WHERE patch_message_id = ? AND disclosed_at IS NULL",
            libsql::params![patch_message_id],
        )
        .await?;
        let mut won = Vec::new();
        for candidate in candidates {
            let conn: &libsql::Connection = &tx;
            let existing = disclosures_in_files_on(conn, &candidate.files, now).await?;
            if existing.iter().any(|e| is_duplicate(candidate, e)) {
                continue;
            }
            let id: i64 = {
                let mut rows = tx
                    .query(
                        "INSERT INTO preexisting_disclosures
                             (patch_message_id, fingerprint, problem, severity, relation,
                              locations_json, bug_key_json, report_text, reserved_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                         ON CONFLICT(patch_message_id, fingerprint) DO NOTHING
                         RETURNING id",
                        libsql::params![
                            patch_message_id,
                            candidate.fingerprint.as_str(),
                            candidate.problem.as_str(),
                            candidate.severity as i32,
                            candidate.relation.as_db_str(),
                            candidate.locations_json.as_str(),
                            candidate.bug_key_json.as_str(),
                            candidate.report_text.as_str(),
                            now
                        ],
                    )
                    .await?;
                match rows.next().await? {
                    Some(row) => row.get(0)?,
                    // Already mailed by this patch under the same fingerprint.
                    None => continue,
                }
            };
            let files: BTreeSet<&str> = candidate.files.iter().map(String::as_str).collect();
            for file in &files {
                tx.execute(
                    "INSERT INTO preexisting_disclosure_files (disclosure_id, file) VALUES (?, ?)",
                    libsql::params![id, *file],
                )
                .await?;
            }
            won.push(PreexistingDisclosure {
                id,
                patch_message_id: patch_message_id.to_string(),
                fingerprint: candidate.fingerprint.clone(),
                problem: candidate.problem.clone(),
                severity: candidate.severity,
                relation: candidate.relation,
                locations_json: candidate.locations_json.clone(),
                bug_key_json: candidate.bug_key_json.clone(),
                report_text: candidate.report_text.clone(),
                reserved_at: now,
                disclosed_at: None,
                bug_id: None,
                files: files.into_iter().map(str::to_string).collect(),
            });
        }
        tx.commit().await?;
        Ok(won)
    }

    /// Disclosures pointing at any of `files`, for showing a review which
    /// bugs were already reported near the code it touches.
    pub async fn preexisting_disclosures_in_files(
        &self,
        files: &[String],
    ) -> Result<Vec<PreexistingDisclosure>> {
        disclosures_in_files_on(&self.conn, files, chrono::Utc::now().timestamp()).await
    }

    /// The live disclosures of the review of `patch_message_id` (mailed, or
    /// reserved within `RESERVATION_TTL_SECS`), ordered by id.
    pub async fn preexisting_disclosures_for_patch(
        &self,
        patch_message_id: &str,
    ) -> Result<Vec<PreexistingDisclosure>> {
        let sql = format!(
            "SELECT {DISCLOSURE_COLUMNS} FROM preexisting_disclosures d
              WHERE d.patch_message_id = ? AND {LIVE_ROW} ORDER BY d.id"
        );
        let oldest_live = chrono::Utc::now().timestamp() - RESERVATION_TTL_SECS;
        let mut rows = self
            .conn
            .query(&sql, libsql::params![patch_message_id, oldest_live])
            .await?;
        let mut found = BTreeMap::new();
        while let Some(row) = rows.next().await? {
            let disclosure = disclosure_from_row(&row)?;
            found.insert(disclosure.id, disclosure);
        }
        attach_files(&self.conn, &mut found).await?;
        Ok(found.into_values().collect())
    }

    /// Stamps the live reservations of `patch_message_id` as mailed, keeping
    /// the time of an earlier stamp. Expired reservations are not stamped:
    /// their bug may already belong to another review. Returns how many rows
    /// were stamped now.
    pub async fn mark_preexisting_disclosed(&self, patch_message_id: &str) -> Result<u64> {
        let now = chrono::Utc::now().timestamp();
        Ok(self
            .execute(
                "UPDATE preexisting_disclosures SET disclosed_at = ?1
                  WHERE patch_message_id = ?2 AND disclosed_at IS NULL AND reserved_at > ?3",
                libsql::params![now, patch_message_id, now - RESERVATION_TTL_SECS],
            )
            .await?)
    }

    /// Drops the reservations of `patch_message_id` that were never mailed,
    /// so a review whose email is muted or skipped does not block a later
    /// review from reporting the same bugs.
    pub async fn release_preexisting_reservations(&self, patch_message_id: &str) -> Result<u64> {
        let tx = self.begin_immediate_transaction().await?;
        // Same fence as reserving: a superseded worker leaves the newer
        // generation's reservations alone.
        if !self
            .with_connection((*tx).clone())
            .is_patchset_claim_active_for_outbox(None)
            .await?
        {
            return Ok(0);
        }
        let released = tx
            .execute(
                "DELETE FROM preexisting_disclosures
                  WHERE patch_message_id = ? AND disclosed_at IS NULL",
                libsql::params![patch_message_id],
            )
            .await?;
        tx.commit().await?;
        Ok(released)
    }

    /// Links the disclosure of `fingerprint` in the review of
    /// `patch_message_id` to the tracked bug the bug pipeline settled on.
    /// Returns whether a still-unlinked disclosure was updated; an existing
    /// link is kept.
    pub async fn link_preexisting_disclosure_to_bug(
        &self,
        patch_message_id: &str,
        fingerprint: &str,
        bug_id: i64,
    ) -> Result<bool> {
        let updated = self
            .execute(
                "UPDATE preexisting_disclosures SET bug_id = ?1
                  WHERE patch_message_id = ?2 AND fingerprint = ?3 AND bug_id IS NULL",
                libsql::params![bug_id, patch_message_id, fingerprint],
            )
            .await?;
        Ok(updated > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DatabaseSettings;
    use std::sync::Arc;

    async fn setup_db() -> Database {
        let settings = DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&settings).await.unwrap();
        db.migrate().await.unwrap();
        db
    }

    fn candidate(fingerprint: &str, files: &[&str]) -> NewPreexistingDisclosure {
        NewPreexistingDisclosure {
            fingerprint: fingerprint.to_string(),
            problem: format!("problem {fingerprint}"),
            severity: Severity::High,
            relation: DisclosureRelation::SamePattern,
            locations_json: "[]".to_string(),
            bug_key_json: "{}".to_string(),
            report_text: format!("report {fingerprint}"),
            files: files.iter().map(|f| f.to_string()).collect(),
        }
    }

    fn same_fingerprint(c: &NewPreexistingDisclosure, e: &PreexistingDisclosure) -> bool {
        c.fingerprint == e.fingerprint
    }

    fn fingerprints(rows: &[PreexistingDisclosure]) -> Vec<&str> {
        rows.iter().map(|d| d.fingerprint.as_str()).collect()
    }

    #[tokio::test]
    async fn test_reserve_skips_bugs_reserved_by_other_patches() {
        let db = setup_db().await;
        let won = db
            .reserve_preexisting_disclosures(
                "p1@x",
                &[candidate("a", &["mm/foo.c"]), candidate("b", &["mm/bar.c"])],
                same_fingerprint,
            )
            .await
            .unwrap();
        assert_eq!(fingerprints(&won), vec!["a", "b"]);
        assert_eq!(won[0].files, vec!["mm/foo.c".to_string()]);
        assert_eq!(won[0].disclosed_at, None);

        // Another patch of the same series, or another series, loses "a".
        let won = db
            .reserve_preexisting_disclosures(
                "p2@x",
                &[candidate("a", &["mm/foo.c"]), candidate("c", &["mm/foo.c"])],
                same_fingerprint,
            )
            .await
            .unwrap();
        assert_eq!(fingerprints(&won), vec!["c"]);

        // Duplicates within one call: the first candidate wins.
        let won = db
            .reserve_preexisting_disclosures(
                "p3@x",
                &[candidate("d", &["mm/baz.c"]), candidate("d", &["mm/baz.c"])],
                same_fingerprint,
            )
            .await
            .unwrap();
        assert_eq!(fingerprints(&won), vec!["d"]);

        let in_foo = db
            .preexisting_disclosures_in_files(&["mm/foo.c".to_string()])
            .await
            .unwrap();
        assert_eq!(fingerprints(&in_foo), vec!["a", "c"]);
    }

    #[tokio::test]
    async fn test_rerun_replaces_reservations_but_never_remails() {
        let db = setup_db().await;
        db.reserve_preexisting_disclosures(
            "p1@x",
            &[candidate("a", &["mm/foo.c"]), candidate("b", &["mm/foo.c"])],
            same_fingerprint,
        )
        .await
        .unwrap();

        // A rerun before the email went out replaces the reservations.
        let won = db
            .reserve_preexisting_disclosures(
                "p1@x",
                &[candidate("a", &["mm/foo.c"])],
                same_fingerprint,
            )
            .await
            .unwrap();
        assert_eq!(fingerprints(&won), vec!["a"]);
        assert_eq!(
            fingerprints(&db.preexisting_disclosures_for_patch("p1@x").await.unwrap()),
            vec!["a"]
        );

        assert_eq!(db.mark_preexisting_disclosed("p1@x").await.unwrap(), 1);
        let mailed = db.preexisting_disclosures_for_patch("p1@x").await.unwrap();
        let first_stamp = mailed[0].disclosed_at.expect("stamped");
        assert_eq!(db.mark_preexisting_disclosed("p1@x").await.unwrap(), 0);

        // A rerun after the email went out does not report the bug again,
        // and keeps the original record.
        let won = db
            .reserve_preexisting_disclosures(
                "p1@x",
                &[candidate("a", &["mm/foo.c"])],
                same_fingerprint,
            )
            .await
            .unwrap();
        assert!(won.is_empty());
        let rows = db.preexisting_disclosures_for_patch("p1@x").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].disclosed_at, Some(first_stamp));
    }

    #[tokio::test]
    async fn test_release_drops_only_unmailed_reservations() {
        let db = setup_db().await;
        db.reserve_preexisting_disclosures(
            "p1@x",
            &[candidate("a", &["mm/foo.c"])],
            same_fingerprint,
        )
        .await
        .unwrap();
        db.mark_preexisting_disclosed("p1@x").await.unwrap();
        db.reserve_preexisting_disclosures(
            "p2@x",
            &[candidate("b", &["mm/foo.c"])],
            same_fingerprint,
        )
        .await
        .unwrap();

        assert_eq!(
            db.release_preexisting_reservations("p1@x").await.unwrap(),
            0
        );
        assert_eq!(
            db.release_preexisting_reservations("p2@x").await.unwrap(),
            1
        );
        assert!(
            db.preexisting_disclosures_for_patch("p2@x")
                .await
                .unwrap()
                .is_empty()
        );
        // The released bug can now be reported by a later review.
        let won = db
            .reserve_preexisting_disclosures(
                "p3@x",
                &[candidate("b", &["mm/foo.c"])],
                same_fingerprint,
            )
            .await
            .unwrap();
        assert_eq!(fingerprints(&won), vec!["b"]);
    }

    #[tokio::test]
    async fn test_expired_reservations_neither_block_nor_mail() {
        let db = setup_db().await;
        db.reserve_preexisting_disclosures(
            "p1@x",
            &[candidate("a", &["mm/foo.c"])],
            same_fingerprint,
        )
        .await
        .unwrap();
        // A review dropped between reserving and mailing leaves this behind.
        db.execute(
            "UPDATE preexisting_disclosures SET reserved_at = reserved_at - ?",
            libsql::params![RESERVATION_TTL_SECS + 1],
        )
        .await
        .unwrap();

        assert!(
            db.preexisting_disclosures_for_patch("p1@x")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(db.mark_preexisting_disclosed("p1@x").await.unwrap(), 0);
        let won = db
            .reserve_preexisting_disclosures(
                "p2@x",
                &[candidate("a", &["mm/foo.c"])],
                same_fingerprint,
            )
            .await
            .unwrap();
        assert_eq!(fingerprints(&won), vec!["a"]);
    }

    #[tokio::test]
    async fn test_superseded_worker_leaves_reservations_alone() {
        let db = setup_db().await;
        let thread_id = db.create_thread("p1@x", "Subject", 1000).await.unwrap();
        db.create_message(
            "p1@x", thread_id, None, "A <a@x>", "Subject", 1000, "body", "", "", None, None,
        )
        .await
        .unwrap();
        let ps_id = db
            .create_patchset(
                thread_id, None, "p1@x", "Subject", "A", 1000, 1, 1, "", "", None, 1, None, false,
                None, None,
            )
            .await
            .unwrap()
            .unwrap();
        db.create_patch(ps_id, "p1@x", 1, "diff").await.unwrap();

        let gen1 = db
            .claim_patchset_for_review_with_gen(ps_id)
            .await
            .unwrap()
            .unwrap();
        let worker1 = db.with_patchset_claim(ps_id, gen1);
        db.rerun_patchset(ps_id).await.unwrap();
        let gen2 = db
            .claim_patchset_for_review_with_gen(ps_id)
            .await
            .unwrap()
            .unwrap();
        let worker2 = db.with_patchset_claim(ps_id, gen2);

        let won = worker2
            .reserve_preexisting_disclosures(
                "p1@x",
                &[candidate("a", &["mm/foo.c"])],
                same_fingerprint,
            )
            .await
            .unwrap();
        assert_eq!(fingerprints(&won), vec!["a"]);

        // The superseded worker can neither replace nor release them.
        let won = worker1
            .reserve_preexisting_disclosures(
                "p1@x",
                &[candidate("b", &["mm/foo.c"])],
                same_fingerprint,
            )
            .await
            .unwrap();
        assert!(won.is_empty());
        assert_eq!(
            worker1
                .release_preexisting_reservations("p1@x")
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            fingerprints(&db.preexisting_disclosures_for_patch("p1@x").await.unwrap()),
            vec!["a"]
        );
    }

    #[tokio::test]
    async fn test_concurrent_reservations_report_a_bug_once() {
        let db = Arc::new(setup_db().await);
        let mut tasks = Vec::new();
        for i in 0..8 {
            let db = db.clone();
            tasks.push(tokio::spawn(async move {
                db.reserve_preexisting_disclosures(
                    &format!("p{i}@x"),
                    &[candidate("a", &["mm/foo.c"])],
                    same_fingerprint,
                )
                .await
                .unwrap()
                .len()
            }));
        }
        let mut total = 0;
        for task in tasks {
            total += task.await.unwrap();
        }
        assert_eq!(total, 1);
    }

    #[tokio::test]
    async fn test_link_to_bug_only_once() {
        let db = setup_db().await;
        db.reserve_preexisting_disclosures(
            "p1@x",
            &[candidate("a", &["mm/foo.c"])],
            same_fingerprint,
        )
        .await
        .unwrap();
        db.execute(
            "INSERT INTO bugs (id, bugid, title, lifecycle_status, pipeline_state, reporter,
                               reported_at, created_at, updated_at)
             VALUES (7, 'linux-7', 't', 'open', 'succeeded', 'r', 0, 0, 0),
                    (8, 'linux-8', 't', 'open', 'succeeded', 'r', 0, 0, 0)",
            (),
        )
        .await
        .unwrap();

        assert!(
            db.link_preexisting_disclosure_to_bug("p1@x", "a", 7)
                .await
                .unwrap()
        );
        assert!(
            !db.link_preexisting_disclosure_to_bug("p1@x", "a", 8)
                .await
                .unwrap()
        );
        assert!(
            !db.link_preexisting_disclosure_to_bug("p1@x", "zz", 8)
                .await
                .unwrap()
        );
        let rows = db.preexisting_disclosures_for_patch("p1@x").await.unwrap();
        assert_eq!(rows[0].bug_id, Some(7));
    }
}
