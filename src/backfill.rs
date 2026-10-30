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

//! One-shot repairs of data that predates the code which would have produced
//! it.
//!
//! These are not migrations. A migration is SQL and runs inside the version
//! ladder; the work here needs the MAINTAINERS index, which is a file read
//! from a git tree and cannot be consulted from SQL.

use crate::db::{AttributedSubsystem, Database};
use anyhow::Result;
use tracing::{info, warn};

/// Marks the attribution walk as finished, so it happens once per database
/// rather than on every boot.
const SECTIONS_BACKFILLED_KEY: &str = "patchset_maintainer_sections_backfilled";

/// How many patchsets to look at per query. Small enough that the walk never
/// holds a large result set on the single shared connection, large enough that
/// a production sized archive does not cost thousands of round trips.
const PAGE: usize = 500;

/// Attributes patchsets that were ingested before attribution existed.
///
/// Every patchset ingested from now on records its MAINTAINERS sections as it
/// arrives, but the archive already holds series that predate that. Until they
/// are attributed they have no sections, and an unattributed patchset is
/// closed rather than open, so the effect of not running this is that old
/// series' transcripts stay visible only to operators. That is the safe
/// direction to fail, which is why this is best effort and never blocks
/// startup.
///
/// Runs once and records that it did. Re-running would be harmless, because
/// the writes are upserts keyed on the patchset and section, but it would walk
/// the whole archive on every boot to discover that the patchsets it keeps
/// finding are the ones that genuinely match no section.
pub async fn backfill_patchset_maintainer_sections(db: &Database) -> Result<()> {
    if db.get_meta(SECTIONS_BACKFILLED_KEY).await?.is_some() {
        return Ok(());
    }

    // Startup installs an empty index when the kernel tree cannot be read, and
    // for projects that have no MAINTAINERS file at all, so an index being
    // present is not the same as it being usable. Walking against an empty one
    // would attribute every patchset to nothing and then stamp itself done,
    // restricting the whole archive permanently. Leaving the marker unset lets
    // a later boot, with the tree in place, do the real work.
    if crate::maintainers::get_global_maintainers().is_none_or(|index| index.is_empty()) {
        warn!(
            "Skipping patchset MAINTAINERS attribution backfill: no sections are loaded. \
             Old patchsets' review transcripts stay restricted to operators until a \
             boot that has the kernel tree available."
        );
        return Ok(());
    }

    let mut after_id = 0i64;
    let mut walked = 0usize;
    let mut attributed = 0usize;

    loop {
        let ids = db
            .patchsets_missing_maintainer_sections(after_id, PAGE)
            .await?;
        if ids.is_empty() {
            break;
        }
        for id in ids {
            after_id = after_id.max(id);
            walked += 1;

            let sections = match sections_for_patchset(db, id).await {
                Ok(sections) => sections,
                Err(e) => {
                    // One unreadable series must not stop the walk, or every
                    // series after it stays unattributed too.
                    warn!("Could not read diffs for patchset {id} during backfill: {e}");
                    continue;
                }
            };
            if sections.is_empty() {
                continue;
            }
            if let Err(e) = db.replace_patchset_maintainer_sections(id, &sections).await {
                warn!("Could not attribute patchset {id} during backfill: {e}");
                continue;
            }
            attributed += 1;
        }
    }

    db.set_meta(SECTIONS_BACKFILLED_KEY, "1").await?;
    info!(
        "Attributed {attributed} of {walked} previously unattributed patchsets to \
         MAINTAINERS sections."
    );
    Ok(())
}

/// The union of the sections every part of this patchset touches.
async fn sections_for_patchset(db: &Database, id: i64) -> Result<Vec<AttributedSubsystem>> {
    let mut sections: Vec<AttributedSubsystem> = Vec::new();
    for diff in db.patch_diffs_only(id).await? {
        for section in crate::maintainers::sections_for_diff(&diff) {
            if !sections.contains(&section) {
                sections.push(section);
            }
        }
    }
    Ok(sections)
}

/// Records which upstream grants the stored grant rows were derived from.
const UPSTREAM_GRANTS_FINGERPRINT_KEY: &str = "upstream_grants_fingerprint";

/// Re-derives every stored upstream grant row when the grants change.
///
/// Grant rows are written as bugs and patchsets are attributed, so on their
/// own they only reflect the grants in force at that moment. This brings the
/// archive in line with the grants loaded now: a new grant reaches existing
/// bugs and patchsets, and a removed one stops conferring access. It runs
/// only when the fingerprint of the loaded grants differs from the one
/// recorded after the last complete walk, and records the new one only once
/// the walk has finished, so an interrupted walk is redone on the next boot.
///
/// It does nothing without a loaded MAINTAINERS index: every grant is dropped
/// when its section cannot be found, so walking then would revoke every grant
/// until a boot that has the kernel tree.
pub async fn reconcile_upstream_grant_sections(db: &Database) -> Result<()> {
    let Some(index) = crate::maintainers::get_global_maintainers().filter(|i| !i.is_empty()) else {
        warn!(
            "Skipping upstream grant reconciliation: no MAINTAINERS sections are loaded. \
             Stored grant rows are left as they are."
        );
        return Ok(());
    };
    let fingerprint = index.upstream_grants_fingerprint();
    if db
        .get_meta(UPSTREAM_GRANTS_FINGERPRINT_KEY)
        .await?
        .as_deref()
        == Some(&fingerprint)
    {
        return Ok(());
    }

    if index.has_upstream_grants() {
        let (bugs, bugs_skipped) = reconcile_bugs(db, &index).await?;
        let patchsets = reconcile_patchsets(db, &index).await?;
        info!(
            "Re-derived upstream grant sections for {bugs} bugs ({bugs_skipped} without \
             recorded files get none) and {patchsets} patchsets."
        );
    } else {
        db.clear_upstream_sections().await?;
        info!("No upstream grants are configured; cleared all upstream grant sections.");
    }
    db.set_meta(UPSTREAM_GRANTS_FINGERPRINT_KEY, &fingerprint)
        .await?;
    Ok(())
}

/// Returns how many bugs were walked and how many had no recorded files.
async fn reconcile_bugs(
    db: &Database,
    index: &crate::maintainers::MaintainersIndex,
) -> Result<(usize, usize)> {
    let (mut after_id, mut walked, mut skipped) = (0i64, 0usize, 0usize);
    loop {
        let ids = db.bug_ids_after(after_id, PAGE).await?;
        let Some(&last) = ids.last() else {
            return Ok((walked, skipped));
        };
        after_id = last;
        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            let Some(bug) = db.get_bug(id).await? else {
                continue;
            };
            walked += 1;
            let files = bug.attributable_files().unwrap_or_default();
            if files.is_empty() {
                skipped += 1;
            }
            rows.push((id, index.upstream_grant_sections(&files)));
        }
        db.replace_bug_upstream_sections_batch(&rows).await?;
    }
}

/// Returns how many patchsets were walked.
async fn reconcile_patchsets(
    db: &Database,
    index: &crate::maintainers::MaintainersIndex,
) -> Result<usize> {
    let (mut after_id, mut walked) = (0i64, 0usize);
    loop {
        let ids = db.patchset_ids_after(after_id, PAGE).await?;
        let Some(&last) = ids.last() else {
            return Ok(walked);
        };
        after_id = last;
        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            walked += 1;
            let mut files: Vec<String> = Vec::new();
            for diff in db.patch_diffs_only(id).await? {
                files.extend(crate::baseline::extract_files_from_diff(&diff));
            }
            rows.push((id, index.upstream_grant_sections(&files)));
        }
        db.replace_patchset_upstream_sections_batch(&rows).await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maintainers::{
        GLOBAL_INDEX_TEST_LOCK, MaintainersIndex, clear_global_maintainers, init_global_maintainers,
    };
    use crate::settings::DatabaseSettings;
    use std::sync::Arc;

    /// Everything before the "Maintainers List" header is preamble the parser
    /// skips, so the fixture has to carry it.
    const SAMPLE_MAINTAINERS: &str = r#"
Maintainers List
===================

BTRFS FILE SYSTEM
M:	Chris Mason <clm@fb.com>
S:	Maintained
F:	fs/btrfs/

NETWORKING [GENERAL]
M:	David S. Miller <davem@davemloft.net>
S:	Maintained
F:	net/

"#;

    async fn setup_db() -> Database {
        let settings = DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&settings).await.unwrap();
        db.migrate().await.unwrap();
        db
    }

    async fn patchset_with_diffs(db: &Database, id: i64, diffs: &[&str]) {
        db.conn
            .execute(
                "INSERT INTO patchsets (id, subject, author, date, status)
                 VALUES (?, '[PATCH] subject', 'An Author <a@b.com>', 1000, 'Reviewed')",
                libsql::params![id],
            )
            .await
            .unwrap();
        for (part, diff) in diffs.iter().enumerate() {
            let message_id = format!("msg-{id}-{part}");
            // patches.message_id is a foreign key, so the message has to exist
            // before the part that points at it.
            db.conn
                .execute(
                    "INSERT INTO messages (message_id, subject, author, date)
                     VALUES (?, '[PATCH] subject', 'An Author <a@b.com>', 1000)",
                    libsql::params![message_id.clone()],
                )
                .await
                .unwrap();
            db.conn
                .execute(
                    "INSERT INTO patches (patchset_id, message_id, part_index, diff)
                     VALUES (?, ?, ?, ?)",
                    libsql::params![id, message_id, part as i64 + 1, diff.to_string()],
                )
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn test_backfill_attributes_old_patchsets_once() {
        let _guard = GLOBAL_INDEX_TEST_LOCK.lock().await;
        let db = setup_db().await;

        // Two parts touching different subsystems, so the attribution has to
        // be the union rather than whichever part is read last.
        patchset_with_diffs(
            &db,
            1,
            &[
                "diff --git a/fs/btrfs/inode.c b/fs/btrfs/inode.c\n",
                "diff --git a/net/core/dev.c b/net/core/dev.c\n",
            ],
        )
        .await;
        // Matches no section, so it stays closed.
        patchset_with_diffs(&db, 2, &["diff --git a/unclaimed/x.c b/unclaimed/x.c\n"]).await;
        // A cover letter only series has no diffs at all.
        patchset_with_diffs(&db, 3, &[]).await;

        clear_global_maintainers();
        init_global_maintainers(Arc::new(
            MaintainersIndex::from_reader(SAMPLE_MAINTAINERS.as_bytes()).unwrap(),
        ));

        backfill_patchset_maintainer_sections(&db).await.unwrap();

        assert_eq!(
            db.authorizing_sections_for_patchset(1).await.unwrap(),
            vec![
                "BTRFS FILE SYSTEM".to_string(),
                "NETWORKING [GENERAL]".to_string()
            ],
            "the walk unions every part of the series"
        );
        assert!(
            db.authorizing_sections_for_patchset(2)
                .await
                .unwrap()
                .is_empty(),
            "a series matching no section is attributed to nobody, not to a guess"
        );
        assert!(
            db.authorizing_sections_for_patchset(3)
                .await
                .unwrap()
                .is_empty()
        );

        // Having run, it records that and does not walk again.
        assert_eq!(
            db.get_meta(SECTIONS_BACKFILLED_KEY).await.unwrap(),
            Some("1".to_string())
        );

        // A patchset added later is ingestion's job, not the backfill's, so a
        // second call must leave it alone.
        patchset_with_diffs(
            &db,
            4,
            &["diff --git a/net/core/sock.c b/net/core/sock.c\n"],
        )
        .await;
        backfill_patchset_maintainer_sections(&db).await.unwrap();
        assert!(
            db.authorizing_sections_for_patchset(4)
                .await
                .unwrap()
                .is_empty(),
            "the walk does not run a second time"
        );

        clear_global_maintainers();
    }

    #[tokio::test]
    async fn test_backfill_without_an_index_leaves_the_work_for_later() {
        let _guard = GLOBAL_INDEX_TEST_LOCK.lock().await;
        let db = setup_db().await;
        patchset_with_diffs(&db, 1, &["diff --git a/net/core/dev.c b/net/core/dev.c\n"]).await;

        clear_global_maintainers();
        backfill_patchset_maintainer_sections(&db).await.unwrap();

        // Nothing was attributed, and crucially nothing was marked done, so a
        // boot that can read MAINTAINERS still will.
        assert!(
            db.authorizing_sections_for_patchset(1)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(db.get_meta(SECTIONS_BACKFILLED_KEY).await.unwrap(), None);

        // An index that loaded but holds no sections is the shape startup
        // produces when the kernel tree is unreadable, and it must be treated
        // the same way rather than as a walk that found nothing.
        init_global_maintainers(Arc::new(MaintainersIndex::new()));
        backfill_patchset_maintainer_sections(&db).await.unwrap();
        assert_eq!(db.get_meta(SECTIONS_BACKFILLED_KEY).await.unwrap(), None);

        clear_global_maintainers();
    }

    #[tokio::test]
    async fn test_reconcile_upstream_grants_follows_the_loaded_grants() {
        let _guard = GLOBAL_INDEX_TEST_LOCK.lock().await;
        let db = setup_db().await;
        let net = "NETWORKING [GENERAL]".to_string();

        patchset_with_diffs(
            &db,
            1,
            &["diff --git a/fs/btrfs/inode.c b/fs/btrfs/inode.c\n"],
        )
        .await;
        patchset_with_diffs(&db, 2, &["diff --git a/unclaimed/x.c b/unclaimed/x.c\n"]).await;
        let mut bug: crate::db::NewBug =
            serde_json::from_value(serde_json::json!({"bugid": "b1", "title": "t"})).unwrap();
        bug.subsystems = vec![AttributedSubsystem::from_maintainers("BTRFS FILE SYSTEM")];
        let bug_id = db
            .create_bug_with_enrichment(
                &bug,
                Some(&crate::db::NewBugEnrichment {
                    kind: "candidate".to_string(),
                    tool: "test".to_string(),
                    created_at: 1,
                    data_json: Some(serde_json::json!({"source_files": ["fs/btrfs/inode.c"]})),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        // A bug filed without files gets no grant, and does not stop the walk.
        let mut bare: crate::db::NewBug =
            serde_json::from_value(serde_json::json!({"bugid": "b2", "title": "t"})).unwrap();
        bare.subsystems = vec![];
        let bare_id = db.create_bug(&bare).await.unwrap();

        let with_grant = |grants: Vec<crate::upstream_grants::RawUpstreamGrant>| {
            let mut index = MaintainersIndex::from_reader(SAMPLE_MAINTAINERS.as_bytes()).unwrap();
            index.set_upstream_grants(grants);
            clear_global_maintainers();
            init_global_maintainers(Arc::new(index));
        };

        with_grant(vec![crate::upstream_grants::RawUpstreamGrant {
            section: net.clone(),
            paths: vec!["fs/btrfs/".into()],
        }]);
        reconcile_upstream_grant_sections(&db).await.unwrap();
        assert_eq!(
            db.authorizing_sections_for_bug(bug_id).await.unwrap(),
            vec!["BTRFS FILE SYSTEM".to_string(), net.clone()]
        );
        assert!(
            db.authorizing_sections_for_bug(bare_id)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.authorizing_sections_for_patchset(1).await.unwrap(),
            vec![net.clone()]
        );
        assert!(
            db.authorizing_sections_for_patchset(2)
                .await
                .unwrap()
                .is_empty()
        );
        let recorded = db.get_meta(UPSTREAM_GRANTS_FINGERPRINT_KEY).await.unwrap();
        assert!(recorded.is_some());

        // Without an index nothing changes and nothing is recorded.
        clear_global_maintainers();
        reconcile_upstream_grant_sections(&db).await.unwrap();
        assert_eq!(
            db.get_meta(UPSTREAM_GRANTS_FINGERPRINT_KEY).await.unwrap(),
            recorded
        );
        assert_eq!(
            db.authorizing_sections_for_patchset(1).await.unwrap(),
            vec![net.clone()]
        );

        // Removing the grant revokes what it gave.
        with_grant(Vec::new());
        reconcile_upstream_grant_sections(&db).await.unwrap();
        assert_eq!(
            db.authorizing_sections_for_bug(bug_id).await.unwrap(),
            vec!["BTRFS FILE SYSTEM".to_string()]
        );
        assert!(
            db.authorizing_sections_for_patchset(1)
                .await
                .unwrap()
                .is_empty()
        );
        assert_ne!(
            db.get_meta(UPSTREAM_GRANTS_FINGERPRINT_KEY).await.unwrap(),
            recorded
        );

        clear_global_maintainers();
    }
}
