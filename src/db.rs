use crate::settings::DatabaseSettings;
use anyhow::Result;
use libsql::Builder;
use serde::Serialize;
use tracing::info;

pub struct Database {
    pub conn: libsql::Connection,
}

#[derive(Debug, Serialize)]
pub struct PatchsetRow {
    pub id: i64,
    pub subject: Option<String>,
    pub status: Option<String>,
    pub thread_id: Option<i64>,
    pub author: Option<String>,
    pub date: Option<i64>,
    pub message_id: Option<String>,
    pub total_parts: Option<u32>,
    pub received_parts: Option<u32>,
}

impl Database {
    pub async fn new(settings: &DatabaseSettings) -> Result<Self> {
        info!("Connecting to database at {}", settings.url);

        let db = if settings.url.starts_with("libsql://") || settings.url.starts_with("https://") {
            Builder::new_remote(settings.url.clone(), settings.token.clone())
                .build()
                .await?
        } else {
            Builder::new_local(&settings.url).build().await?
        };

        let conn = db.connect()?;

        Ok(Self { conn })
    }

    pub async fn migrate(&self) -> Result<()> {
        let schema = include_str!("schema.sql");
        self.conn.execute_batch(schema).await?;
        info!("Database schema applied");
        Ok(())
    }

    pub async fn ensure_mailing_list(&self, name: &str, group: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO mailing_lists (name, nntp_group, last_article_num) VALUES (?, ?, 0)",
                libsql::params![name, group],
            )
            .await?;
        Ok(())
    }

    pub async fn get_last_article_num(&self, group: &str) -> Result<u64> {
        let mut rows = self
            .conn
            .query(
                "SELECT last_article_num FROM mailing_lists WHERE nntp_group = ?",
                libsql::params![group],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let num: i64 = row.get(0)?;
            Ok(num as u64)
        } else {
            Ok(0)
        }
    }

    pub async fn update_last_article_num(&self, group: &str, num: u64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE mailing_lists SET last_article_num = ? WHERE nntp_group = ?",
                libsql::params![num as i64, group],
            )
            .await?;
        Ok(())
    }

    pub async fn create_thread(
        &self,
        root_message_id: &str,
        subject: &str,
        date: i64,
    ) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO threads (root_message_id, subject, last_updated) VALUES (?, ?, ?)",
                libsql::params![root_message_id, subject, date],
            )
            .await?;

        let mut rows = self.conn.query("SELECT last_insert_rowid()", ()).await?;
        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to get thread ID"))
        }
    }

    pub async fn get_thread_id_for_message(&self, message_id: &str) -> Result<Option<i64>> {
        let mut rows = self
            .conn
            .query(
                "SELECT thread_id FROM messages WHERE message_id = ?",
                libsql::params![message_id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub async fn ensure_thread_for_message(&self, message_id: &str, date: i64) -> Result<i64> {
        // 1. Check if message exists
        if let Some(tid) = self.get_thread_id_for_message(message_id).await? {
            return Ok(tid);
        }

        // 2. Not found, create new thread and placeholder message
        let thread_id = self.create_thread(message_id, "(placeholder)", date).await?;
        
        self.create_message(
            message_id,
            thread_id,
            None,
            "unknown",
            "(placeholder)",
            date,
            "",
        ).await?;

        Ok(thread_id)
    }

    pub async fn create_message(
        &self,
        message_id: &str,
        thread_id: i64,
        in_reply_to: Option<&str>,
        author: &str,
        subject: &str,
        date: i64,
        body: &str,
    ) -> Result<()> {
        // Use INSERT OR REPLACE to handle updating placeholders
        // But we want to preserve thread_id if it was set by placeholder (which is correct).
        // Actually, if we are "creating" the real message now, we should overwrite the placeholder fields.
        // But we must ensure we keep the same thread_id if it exists? 
        // No, the caller (main.rs) resolves thread_id before calling create_message.
        // If we found a placeholder, we use its thread_id.
        // So here we just upsert.
        
        // However, if we blindly REPLACE, we might change the thread_id if we passed a different one?
        // But main.rs logic should ensure consistency.
        // Let's use INSERT OR REPLACE.
        self.conn.execute(
            "INSERT OR REPLACE INTO messages (message_id, thread_id, in_reply_to, author, subject, date, body) VALUES (?, ?, ?, ?, ?, ?, ?)",
            libsql::params![message_id, thread_id, in_reply_to, author, subject, date, body],
        ).await?;
        Ok(())
    }

    pub async fn create_baseline(
        &self,
        repo_url: Option<&str>,
        branch: Option<&str>,
        commit: Option<&str>,
    ) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO baselines (repo_url, branch, last_known_commit) VALUES (?, ?, ?)",
                libsql::params![repo_url, branch, commit],
            )
            .await?;

        let mut rows = self
            .conn
            .query("SELECT last_insert_rowid()", libsql::params![])
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to get baseline ID"))
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_patchset(
        &self,
        thread_id: i64,
        cover_letter_message_id: Option<&str>,
        subject: &str,
        author: &str,
        date: i64,
        total_parts: u32,
        parser_version: i32,
        to: &str,
        cc: &str,
        baseline_id: Option<i64>,
        version: Option<u32>,
        part_index: u32,
    ) -> Result<Option<i64>> {
        // Find candidate patchsets in this thread
        let mut rows = self
            .conn
            .query(
                "SELECT id, date, author, subject, subject_index FROM patchsets WHERE thread_id = ?",
                libsql::params![thread_id],
            )
            .await?;

        let mut matched_id = None;
        let mut existing_subject_index = 9999;
        let mut has_existing_patchsets = false;
        let mut author_exists_in_thread = false;

        while let Ok(Some(row)) = rows.next().await {
            has_existing_patchsets = true;
            let id: i64 = row.get(0)?;
            let existing_date: i64 = row.get(1)?;
            let existing_author: String = row.get(2)?;
            let existing_subject: String = row.get(3)?;

            if existing_author == author {
                author_exists_in_thread = true;
            }

            // Parse version from existing subject
            let existing_version = crate::patch::parse_subject_version(&existing_subject);
            
            let v_new = version.unwrap_or(1);
            let v_old = existing_version.unwrap_or(1);

            // Matching logic:
            // 1. Author must match (patches in a set are from same person)
            // 2. Time must be close (within 15 mins / 900s)
            // 3. Versions must match (v1 vs v2 are different patchsets)
            if existing_author == author && (date - existing_date).abs() < 900 && v_new == v_old {
                matched_id = Some(id);
                existing_subject_index = row.get(4).unwrap_or(9999);
                break;
            }
        }

        // Enforce Same Sender constraint:
        // If there are existing patchsets in this thread, but NONE match our author,
        // then this message is likely a reply/review from someone else, NOT a new patchset.
        // We skip creating a patchset.
        if has_existing_patchsets && !author_exists_in_thread {
            info!("Skipping patchset creation for thread {} author '{}': different from existing patchset authors", thread_id, author);
            return Ok(None);
        }

        if let Some(id) = matched_id {
            // Update existing patchset
            // Note: We do NOT update 'date' to prevent the window from creeping. 
            // We assume the existing date (from first received part) is the anchor.
            // We update other fields that might be better defined now (e.g. baseline).
            self.conn.execute(
                "UPDATE patchsets SET author = ?, total_parts = ?, parser_version = ?, to_recipients = ?, cc_recipients = ?, baseline_id = ? WHERE id = ?",
                libsql::params![author, total_parts, parser_version, to, cc, baseline_id, id],
            ).await?;

            // Conditionally update subject: Prefer the one with lowest index (usually 0/N cover letter)
            if part_index < existing_subject_index {
                self.conn.execute(
                    "UPDATE patchsets SET subject = ?, subject_index = ? WHERE id = ?",
                    libsql::params![subject, part_index, id],
                ).await?;
            }
            
            // If this message is explicitly a cover letter (has cover_letter_message_id and index 0 logic from caller passed here as cover_letter_message_id arg),
            // we should update the cover_letter_message_id field.
            if let Some(clid) = cover_letter_message_id {
                 self.conn.execute(
                    "UPDATE patchsets SET cover_letter_message_id = ? WHERE id = ?",
                    libsql::params![clid, id],
                ).await?;
            }

            return Ok(Some(id));
        }

        // No match found, create new patchset
        self.conn
            .execute(
                "INSERT INTO patchsets (thread_id, cover_letter_message_id, subject, author, date, total_parts, received_parts, status, parser_version, to_recipients, cc_recipients, baseline_id, subject_index) 
                 VALUES (?, ?, ?, ?, ?, ?, 0, 'Pending', ?, ?, ?, ?, ?)",
                libsql::params![thread_id, cover_letter_message_id, subject, author, date, total_parts, parser_version, to, cc, baseline_id, part_index],
            )
            .await?;

        let mut rows = self
            .conn
            .query("SELECT last_insert_rowid()", libsql::params![])
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            Ok(Some(id))
        } else {
            Err(anyhow::anyhow!(
                "Failed to retrieve patchset ID after insert"
            ))
        }
    }

    pub async fn create_patch(
        &self,
        patchset_id: i64,
        message_id: &str,
        part_index: u32,
        diff: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO patches (patchset_id, message_id, part_index, diff) VALUES (?, ?, ?, ?)",
            libsql::params![patchset_id, message_id, part_index, diff]
        ).await?;

        // Update received_parts based on actual patch count to be idempotent
        self.conn
            .execute(
                "UPDATE patchsets SET received_parts = (SELECT COUNT(*) FROM patches WHERE patchset_id = ?) WHERE id = ?",
                libsql::params![patchset_id, patchset_id],
            )
            .await?;
        Ok(())
    }

    pub async fn get_patchsets(&self, limit: usize, offset: usize) -> Result<Vec<PatchsetRow>> {
        let mut rows = self.conn.query(
            "SELECT id, subject, status, thread_id, author, date, cover_letter_message_id, total_parts, received_parts FROM patchsets ORDER BY id DESC LIMIT ? OFFSET ?",
            libsql::params![limit as i64, offset as i64],
        ).await?;

        let mut patchsets = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            patchsets.push(PatchsetRow {
                id: row.get(0)?,
                subject: row.get(1).ok(),
                status: row.get(2).ok(),
                thread_id: row.get(3).ok(),
                author: row.get(4).ok(),
                date: row.get(5).ok(),
                message_id: row.get(6).ok(),
                total_parts: row.get(7).ok(),
                received_parts: row.get(8).ok(),
            });
        }
        Ok(patchsets)
    }

    pub async fn count_patchsets(&self) -> Result<usize> {
        let mut rows = self
            .conn
            .query("SELECT COUNT(*) FROM patchsets", libsql::params![])
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        } else {
            Ok(0)
        }
    }

    pub async fn get_patchset_details(&self, id: i64) -> Result<Option<serde_json::Value>> {
        let mut rows = self.conn.query(
            "SELECT p.id, p.subject, p.status, p.to_recipients, p.cc_recipients, 
                    b.repo_url, b.branch, b.last_known_commit, p.author, p.date, p.cover_letter_message_id, p.thread_id
             FROM patchsets p 
             LEFT JOIN baselines b ON p.baseline_id = b.id
             WHERE p.id = ?",
            libsql::params![id],
        ).await?;

        if let Ok(Some(row)) = rows.next().await {
            let pid: i64 = row.get(0)?;
            let subject: Option<String> = row.get(1).ok();
            let status: Option<String> = row.get(2).ok();
            let to: Option<String> = row.get(3).ok();
            let cc: Option<String> = row.get(4).ok();
            let repo_url: Option<String> = row.get(5).ok();
            let branch: Option<String> = row.get(6).ok();
            let commit: Option<String> = row.get(7).ok();
            let author: Option<String> = row.get(8).ok();
            let date: Option<i64> = row.get(9).ok();
            let mid: Option<String> = row.get(10).ok();
            let thread_id: Option<i64> = row.get(11).ok();

            // Fetch reviews
            let mut reviews = Vec::new();
            let mut rev_rows = self
                .conn
                .query(
                    "SELECT r.model_name, r.summary, r.created_at, ai.input_context, ai.output_raw
                 FROM reviews r
                 LEFT JOIN ai_interactions ai ON r.interaction_id = ai.id
                 WHERE r.patchset_id = ?",
                    libsql::params![pid],
                )
                .await?;

            while let Ok(Some(r)) = rev_rows.next().await {
                reviews.push(serde_json::json!({
                    "model": r.get::<Option<String>>(0).ok(),
                    "summary": r.get::<Option<String>>(1).ok(),
                    "created_at": r.get::<Option<i64>>(2).ok(),
                    "input": r.get::<Option<String>>(3).ok(),
                    "output": r.get::<Option<String>>(4).ok(),
                }));
            }

            // Fetch patches
            let mut patches = Vec::new();
            let mut patch_rows = self.conn.query(
                "SELECT id, message_id, part_index FROM patches WHERE patchset_id = ? ORDER BY part_index ASC",
                libsql::params![pid]
            ).await?;
            while let Ok(Some(p)) = patch_rows.next().await {
                patches.push(serde_json::json!({
                    "id": p.get::<i64>(0)?,
                    "message_id": p.get::<String>(1)?,
                    "part_index": p.get::<Option<i64>>(2).ok(),
                }));
            }

            // Fetch thread messages
            let mut messages = Vec::new();
            if let Some(tid) = thread_id {
                let mut msg_rows = self.conn.query(
                    "SELECT message_id, author, date, subject FROM messages WHERE thread_id = ? ORDER BY date ASC",
                    libsql::params![tid]
                ).await?;
                while let Ok(Some(m)) = msg_rows.next().await {
                    messages.push(serde_json::json!({
                        "message_id": m.get::<String>(0)?,
                        "author": m.get::<Option<String>>(1).ok(),
                        "date": m.get::<Option<i64>>(2).ok(),
                        "subject": m.get::<Option<String>>(3).ok(),
                    }));
                }
            }

            Ok(Some(serde_json::json!({
                "id": pid,
                "message_id": mid,
                "subject": subject,
                "author": author,
                "date": date,
                "status": status,
                "to": to,
                "cc": cc,
                "baseline": {
                    "repo_url": repo_url,
                    "branch": branch,
                    "commit": commit,
                },
                "reviews": reviews,
                "patches": patches,
                "thread": messages
            })))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DatabaseSettings;
    use std::sync::Arc;

    async fn setup_db() -> Arc<Database> {
        let settings = DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Database::new(&settings).await.unwrap();
        db.migrate().await.unwrap();
        Arc::new(db)
    }

    #[tokio::test]
    async fn test_create_multiple_patchsets_in_thread() {
        let db = setup_db().await;

        // Create a thread
        let thread_id = db.create_thread("root", "Test Thread", 1000).await.unwrap();

        // 1. Create first patchset from Patch 1 (index 1)
        db.create_message("msg1", thread_id, None, "Author A", "Patch 1", 1000, "").await.unwrap();
        let ps1 = db.create_patchset(
            thread_id, None, "Patch 1", "Author A", 1000, 2, 1, "to", "cc", None, Some(1), 1
        ).await.unwrap();
        assert!(ps1.is_some());

        // 2. Add Cover Letter (index 0)
        // Should return same ID and update subject to "Cover Letter"
        db.create_message("root", thread_id, None, "Author A", "Cover Letter", 1005, "").await.unwrap();
        let ps1_update = db.create_patchset(
            thread_id, Some("root"), "Cover Letter", "Author A", 1005, 2, 1, "to", "cc", None, Some(1), 0
        ).await.unwrap();
        assert_eq!(ps1, ps1_update);

        let list = db.get_patchsets(1, 0).await.unwrap();
        assert_eq!(list[0].subject.as_deref(), Some("Cover Letter"));

        // 3. Add Patch 2 (index 2)
        // Should NOT update subject (index 2 > index 0)
        db.create_message("msg2", thread_id, None, "Author A", "Patch 2", 1006, "").await.unwrap();
        db.create_patchset(
            thread_id, None, "Patch 2", "Author A", 1006, 2, 1, "to", "cc", None, Some(1), 2
        ).await.unwrap();

        let list = db.get_patchsets(1, 0).await.unwrap();
        assert_eq!(list[0].subject.as_deref(), Some("Cover Letter"));

        // 4. Create NEW patchset in same thread (Author A, Time 2000 - > 15 mins later)
        let ps2 = db.create_patchset(
            thread_id, None, "New Version", "Author A", 2000, 2, 1, "to", "cc", None, Some(1), 0
        ).await.unwrap();
        assert_ne!(ps1, ps2);

        // 5. Create NEW patchset in same thread (Author B, Time 1000 - same time but diff author)
        let ps3 = db.create_patchset(
            thread_id, None, "Other Author", "Author B", 1000, 2, 1, "to", "cc", None, Some(1), 0
        ).await.unwrap();
        assert!(ps3.is_none());

        // 6. Create NEW patchset v2 (Author A, Time 1002 - close time, but v2)
        let ps_v2 = db.create_patchset(
            thread_id, None, "[PATCH v2] Patchset 1", "Author A", 1002, 2, 1, "to", "cc", None, Some(2), 0
        ).await.unwrap();
        assert_ne!(ps1, ps_v2);
    }
}