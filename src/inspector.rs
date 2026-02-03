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

use crate::db::Database;
use anyhow::Result;
use std::sync::Arc;
use tracing::{info, warn};

pub async fn run_inspection(db: Arc<Database>) -> Result<()> {
    info!("Starting database inspection...");

    inspect_missed_patches(&db).await?;
    inspect_patchsets(&db).await?;

    info!("Inspection complete.");
    Ok(())
}

async fn inspect_missed_patches(db: &Database) -> Result<()> {
    info!("Scanning for missed patches (false negatives)...");

    // Fetch all messages that are potential roots (no in-reply-to)
    // We stream them to avoid OOM.
    // Since we can't easily stream with libsql in a simple loop without holding a transaction open or paginating,
    // we'll paginate.
    let mut offset = 0;
    let limit = 1000;
    let mut total_checked = 0;
    let mut anomalies = 0;

    loop {
        let mut rows = db.conn.query(
            "SELECT message_id, subject, body, in_reply_to, author, date FROM messages LIMIT ? OFFSET ?",
            libsql::params![limit, offset],
        ).await?;

        let mut batch_count = 0;
        let mut batch_ids = Vec::new();

        while let Ok(Some(row)) = rows.next().await {
            batch_count += 1;
            let mid: String = row.get(0)?;
            let subject: String = row.get(1)?;
            let body: String = row.get(2)?;
            let in_reply_to: Option<String> = row.get(3).ok();

            // Checks
            if in_reply_to.is_some() {
                continue;
            }
            if subject.to_lowercase().trim().starts_with("re:") {
                continue;
            }

            // Check if it's already a patch
            // We can't check this efficiently inside the loop for every single message if we query DB.
            // Better: Load known patch message IDs into a Bloom filter or HashSet if memory allows.
            // 1M messages might be heavy for HashSet of Strings (1M * 64B ~ 64MB - actually fine).
            batch_ids.push((mid, subject, body));
        }

        if batch_count == 0 {
            break;
        }

        // Check if these IDs are in patches or patchsets (cover letters)
        for (mid, subject, body) in batch_ids {
            let is_patch = db
                .conn
                .query(
                    "SELECT 1 FROM patches WHERE message_id = ?",
                    libsql::params![mid.clone()],
                )
                .await?
                .next()
                .await?
                .is_some();
            let is_cover = db
                .conn
                .query(
                    "SELECT 1 FROM patchsets WHERE cover_letter_message_id = ?",
                    libsql::params![mid.clone()],
                )
                .await?
                .next()
                .await?
                .is_some();

            if !is_patch && !is_cover {
                // Analyze content
                // We use a simplified check here similar to `patch.rs` but we construct a fake email body
                // because we stored 'body' (text) not raw email.
                // But `patch.rs` `parse_email` expects raw bytes with headers.
                // We'll just look for indicators in the stored body and subject.

                let has_diff = body.contains("diff --git")
                    || (body.contains("--- ") && body.contains("+++ ") && body.contains("@@ -"));
                let subject_lower = subject.to_lowercase();
                let has_patch_tag = subject_lower.contains("patch");

                // Heuristic: If it has [PATCH] and NO diff, it might be a cover letter that was missed?
                // Or if it has diff but no [PATCH] (rare in LKML but possible).

                if has_diff {
                    warn!(
                        "POSSIBLE MISSED PATCH: ID={} Subject='{}' (Has Diff)",
                        mid, subject
                    );
                    anomalies += 1;
                } else if has_patch_tag {
                    // Check if it looks like a cover letter 0/N
                    if subject.contains("0/") || subject.contains("00/") {
                        warn!(
                            "POSSIBLE MISSED COVER LETTER: ID={} Subject='{}'",
                            mid, subject
                        );
                        anomalies += 1;
                    }
                }
            }
        }

        total_checked += batch_count;
        offset += limit;
        if total_checked % 10000 == 0 {
            info!("Checked {} messages...", total_checked);
        }
    }

    info!("Missed patches check done. Found {} anomalies.", anomalies);
    Ok(())
}

async fn inspect_patchsets(db: &Database) -> Result<()> {
    info!("Scanning patchsets for anomalies...");

    let mut rows = db
        .conn
        .query(
            "SELECT id, subject, total_parts, received_parts, status, date FROM patchsets",
            libsql::params![],
        )
        .await?;

    let mut total_sets = 0;
    let mut overcomplete = 0;
    let mut stuck = 0;
    let mut inconsistent = 0;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;

    while let Ok(Some(row)) = rows.next().await {
        total_sets += 1;
        let id: i64 = row.get(0)?;
        let subject: String = row.get(1).unwrap_or_default();
        let total: u32 = row.get(2).unwrap_or(0);
        let received: u32 = row.get(3).unwrap_or(0);
        let status: String = row.get(4).unwrap_or_default();
        let date: i64 = row.get(5).unwrap_or(0);

        if received > total {
            warn!(
                "OVERCOMPLETE: ID={} Subject='{}' Recv={}/Total={}",
                id, subject, received, total
            );
            overcomplete += 1;
        }

        if status == "Incomplete" && (now - date) > 86400 * 2 {
            // Older than 2 days and still incomplete
            // Only warn if received > 0, otherwise it might be a ghost or just created?
            // Actually, if received < total, it's stuck.
            // Lots of these might exist if parts were missed.
            if received > 0 {
                // warn!("STUCK INCOMPLETE: ID={} Subject='{}' Recv={}/Total={} Age={}s", id, subject, received, total, now - date);
                stuck += 1;
            }
        }

        // Consistency check: Real patch count vs received_parts
        // We can't query count(*) for every row efficiently in this loop if n is large.
        // But for verification we should.
        // Let's sample or just accept the cost for now (it's a CLI tool).

        let count_row = db
            .conn
            .query(
                "SELECT COUNT(*) FROM patches WHERE patchset_id = ?",
                libsql::params![id],
            )
            .await?
            .next()
            .await?;
        if let Some(r) = count_row {
            let real_count: i64 = r.get(0)?;
            if real_count != received as i64 {
                warn!(
                    "INCONSISTENT COUNT: ID={} Subject='{}' stored_recv={} actual_patches={}",
                    id, subject, received, real_count
                );
                inconsistent += 1;
            }
        }
    }

    info!(
        "Patchset check done. Total={}. Overcomplete={}. Stuck (>48h)={}. Inconsistent={}.",
        total_sets, overcomplete, stuck, inconsistent
    );
    Ok(())
}
