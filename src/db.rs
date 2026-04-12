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

use crate::ReviewStatus;
use crate::settings::DatabaseSettings;
use anyhow::Result;
use libsql::Builder;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;

pub struct Database {
    pub conn: libsql::Connection,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Subsystem {
    pub id: i64,
    pub name: String,
    pub mailing_list_address: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
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
    pub subsystems: Vec<String>,
    pub findings_low: Option<i64>,
    pub findings_medium: Option<i64>,
    pub findings_high: Option<i64>,
    pub findings_critical: Option<i64>,
    pub baseline_id: Option<i64>,
    pub failed_reason: Option<String>,
    pub skip_filters: Option<String>,
    pub only_filters: Option<String>,
    pub target_review_count: Option<u32>,
    pub model_name: Option<String>,
    pub prompts_git_hash: Option<String>,
    pub baseline_logs: Option<String>,
    pub provider: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MessageRow {
    pub id: i64,
    pub message_id: String,
    pub thread_id: Option<i64>,
    pub in_reply_to: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub date: Option<i64>,
    pub body: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
    pub thread: Option<Vec<serde_json::Value>>,
    pub git_blob_hash: Option<String>,
    pub mailing_list: Option<String>,
    pub diff: Option<String>,
}

pub struct AiInteractionParams<'a> {
    pub id: &'a str,
    pub parent_id: Option<&'a str>,
    pub workflow_id: Option<&'a str>,
    pub provider: &'a str,
    pub model: &'a str,
    pub input: &'a str,
    pub output: &'a str,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tokens_cached: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ToolUsage {
    pub review_id: i64,
    pub provider: String,
    pub model: String,
    pub tool_name: String,
    pub arguments: Option<String>,
    pub output_length: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Low = 1,
    Medium = 2,
    High = 3,
    Critical = 4,
}

impl Severity {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        let s = s.trim();
        if s.eq_ignore_ascii_case("critical") {
            Severity::Critical
        } else if s.to_lowercase().starts_with("high") {
            Severity::High
        } else if s.to_lowercase().starts_with("medium") {
            Severity::Medium
        } else {
            Severity::Low
        }
    }
}

pub struct Finding {
    pub review_id: i64,
    pub severity: Severity,
    pub severity_explanation: Option<String>,
    pub problem: String,
}

pub struct EmailOutboxRow {
    pub id: i64,
    pub patch_id: i64,
    pub status: String,
    pub to_addresses: String,
    pub cc_addresses: String,
    pub subject: String,
    pub in_reply_to: String,
    pub references_hdr: String,
    pub body: String,
    pub locked_at: Option<i64>,
    pub error_log: Option<String>,
    pub created_at: i64,
}

impl Database {
    pub async fn get_oldest_message_timestamp(&self) -> Result<Option<i64>> {
        let mut rows = self
            .conn
            .query("SELECT MIN(date) FROM messages WHERE date > 0", ())
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0).ok())
        } else {
            Ok(None)
        }
    }

    pub async fn get_message_details(&self, id: i64) -> Result<Option<MessageRow>> {
        let mut rows = self.conn.query(
            "SELECT m.id, m.message_id, m.thread_id, m.in_reply_to, m.author, m.subject, m.date, m.body, m.to_recipients, m.cc_recipients, m.git_blob_hash, m.mailing_list, p.diff 
             FROM messages m 
             LEFT JOIN patches p ON m.message_id = p.message_id
             WHERE m.id = ?",
             libsql::params![id],
        ).await?;

        if let Ok(Some(row)) = rows.next().await {
            let thread_id: Option<i64> = row.get(2).ok();

            // Fetch thread messages
            let mut messages = Vec::new();
            if let Some(tid) = thread_id {
                let mut msg_rows = self.conn.query(
                    "SELECT id, message_id, author, date, subject, in_reply_to FROM messages WHERE thread_id = ? AND subject != '(placeholder)' ORDER BY date ASC",
                    libsql::params![tid]
                ).await?;
                while let Ok(Some(m)) = msg_rows.next().await {
                    messages.push(serde_json::json!({
                        "id": m.get::<i64>(0)?,
                        "message_id": m.get::<String>(1)?,
                        "author": m.get::<Option<String>>(2).ok(),
                        "date": m.get::<Option<i64>>(3).ok(),
                        "subject": m.get::<Option<String>>(4).ok(),
                        "in_reply_to": m.get::<Option<String>>(5).ok(),
                    }));
                }
            }

            let body: Option<String> = row.get(7).ok();
            let raw_diff: Option<String> = row.get(12).ok();

            // For email-based patches, the diff is often just the body.
            // We don't want to show it twice in the UI.
            // For git commits, body is the commit message and diff is the actual diff.
            let diff = if let (Some(b), Some(d)) = (&body, &raw_diff) {
                if b == d { None } else { raw_diff }
            } else {
                raw_diff
            };

            Ok(Some(MessageRow {
                id: row.get(0)?,
                message_id: row.get(1)?,
                thread_id,
                in_reply_to: row.get(3).ok(),
                author: row.get(4).ok(),
                subject: row.get(5).ok(),
                date: row.get(6).ok(),
                body,
                to: row.get(8).ok(),
                cc: row.get(9).ok(),
                git_blob_hash: row.get(10).ok(),
                mailing_list: row.get(11).ok(),
                diff,
                thread: Some(messages),
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn get_message_details_by_msgid(&self, msg_id: &str) -> Result<Option<MessageRow>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM messages WHERE message_id = ?",
                libsql::params![msg_id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            self.get_message_details(id).await
        } else {
            Ok(None)
        }
    }

    pub async fn get_patchset_details_by_msgid(
        &self,
        msg_id: &str,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Option<serde_json::Value>> {
        // 1. Try to find a patchset where this is the cover letter
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM patchsets WHERE cover_letter_message_id = ?",
                libsql::params![msg_id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            return self.get_patchset_details(id, page, limit).await;
        }

        // 2. Fallback: Find a patchset that contains this message as a patch
        let mut rows = self
            .conn
            .query(
                "SELECT patchset_id FROM patches WHERE message_id = ?",
                libsql::params![msg_id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            return self.get_patchset_details(id, page, limit).await;
        }

        Ok(None)
    }

    pub async fn get_message_body(&self, msg_id: &str) -> Result<Option<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT body, git_blob_hash, mailing_list FROM messages WHERE message_id = ?",
                libsql::params![msg_id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let body: Option<String> = row.get(0).ok();
            if let Some(b) = body
                && !b.is_empty()
            {
                return Ok(Some(b));
            }
            // Try git blob
            let hash: Option<String> = row.get(1).ok();
            let group: Option<String> = row.get(2).ok();

            if let (Some(_h), Some(_g)) = (hash, group) {
                // We don't have easy access to git_ops::read_blob here without repo path.
                // The DB does not know about repo path logic; it must be passed in.
                // The Reviewer service has the repo path.
                // Return None if the body is empty in the DB, and let the caller handle the blob if needed.
                // The body is needed for the base-commit.
                // The body is populated in the DB if it is small.
                // Sashiko stores the body in the DB unless it is a large patch.
                // See "body_to_store" logic in main.rs:
                // `if is_git_hash { ("", Some(hash)) } else { (body, None) }`
                // If it is from a git archive, the body is empty in the DB.
                return Ok(None);
            }
            Ok(None)
        } else {
            Ok(None)
        }
    }

    pub async fn new(settings: &DatabaseSettings) -> Result<Self> {
        info!(
            "Connecting to database at {}",
            crate::utils::redact_secret(&settings.url)
        );

        let db = if settings.url.starts_with("libsql://") || settings.url.starts_with("https://") {
            Builder::new_remote(settings.url.clone(), settings.token.clone())
                .build()
                .await?
        } else {
            Builder::new_local(&settings.url).build().await?
        };

        let conn = db.connect()?;

        // Enable WAL mode for better concurrency
        // PRAGMA journal_mode returns a row (the new mode), so we must use query() instead of execute()
        let _ = conn
            .query("PRAGMA journal_mode=WAL;", ())
            .await?
            .next()
            .await;
        let _ = conn
            .query("PRAGMA busy_timeout = 5000;", ())
            .await?
            .next()
            .await;

        Ok(Self { conn })
    }

    pub async fn migrate(&self) -> Result<()> {
        let schema = include_str!("schema.sql");
        self.conn.execute_batch(schema).await?;

        // Consolidate 'Applying' and 'In Review' states
        let _ = self
            .conn
            .execute(
                "UPDATE patchsets SET status = 'In Review' WHERE status = 'Applying'",
                (),
            )
            .await;
        let _ = self
            .conn
            .execute(
                "UPDATE reviews SET status = 'In Review' WHERE status = 'Applying'",
                (),
            )
            .await;

        // Manual migrations for existing tables
        let _ = self
            .try_add_column("messages", "to_recipients", "TEXT")
            .await;
        let _ = self
            .try_add_column("messages", "cc_recipients", "TEXT")
            .await;
        let _ = self
            .try_add_column("messages", "git_blob_hash", "TEXT")
            .await;
        let _ = self
            .try_add_column("messages", "mailing_list", "TEXT")
            .await;
        let _ = self
            .try_create_index(
                "idx_patchsets_cover_message_id",
                "patchsets",
                "cover_letter_message_id",
            )
            .await;
        let _ = self.try_add_column("patches", "status", "TEXT").await;
        let _ = self.try_add_column("patches", "apply_error", "TEXT").await;
        let _ = self.try_add_column("reviews", "provider", "TEXT").await;
        let _ = self
            .try_add_column("reviews", "prompts_git_hash", "TEXT")
            .await;
        let _ = self
            .try_add_column("reviews", "result_description", "TEXT")
            .await;
        let _ = self.try_add_column("reviews", "status", "TEXT").await;
        let _ = self.try_add_column("reviews", "logs", "TEXT").await;
        let _ = self.try_add_column("reviews", "patch_id", "INTEGER").await;
        let _ = self
            .try_add_column("reviews", "inline_review", "TEXT")
            .await;
        let _ = self
            .try_add_column("patchsets", "baseline_id", "INTEGER")
            .await;
        let _ = self
            .try_add_column("patchsets", "failed_reason", "TEXT")
            .await;
        let _ = self
            .try_add_column("patchsets", "skip_filters", "TEXT")
            .await;
        let _ = self
            .try_add_column("patchsets", "only_filters", "TEXT")
            .await;
        let _ = self
            .try_add_column("patchsets", "target_review_count", "INTEGER DEFAULT 1")
            .await;
        let _ = self.try_add_column("patchsets", "model_name", "TEXT").await;
        let _ = self
            .try_add_column("patchsets", "prompts_git_hash", "TEXT")
            .await;
        let _ = self
            .try_add_column("patchsets", "baseline_logs", "TEXT")
            .await;
        let _ = self.try_add_column("patchsets", "provider", "TEXT").await;

        let _ = self
            .conn
            .execute(
                "CREATE TABLE IF NOT EXISTS tool_usages (
                    id INTEGER PRIMARY KEY,
                    review_id INTEGER NOT NULL,
                    provider TEXT,
                    model TEXT,
                    tool_name TEXT,
                    arguments TEXT,
                    output_length INTEGER,
                    created_at INTEGER,
                    FOREIGN KEY(review_id) REFERENCES reviews(id)
                )",
                (),
            )
            .await;
        let _ = self
            .try_create_index("idx_tool_usages_review", "tool_usages", "review_id")
            .await;

        // Manual migration for messages_mailing_lists
        let _ = self
            .conn
            .execute(
                "CREATE TABLE IF NOT EXISTS messages_mailing_lists (
                    message_id INTEGER NOT NULL,
                    mailing_list_id INTEGER NOT NULL,
                    PRIMARY KEY (message_id, mailing_list_id),
                    FOREIGN KEY(message_id) REFERENCES messages(id) ON DELETE CASCADE,
                    FOREIGN KEY(mailing_list_id) REFERENCES mailing_lists(id) ON DELETE CASCADE
                )",
                (),
            )
            .await;

        // Backfill messages_mailing_lists from messages.mailing_list
        let _ = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO messages_mailing_lists (message_id, mailing_list_id)
                 SELECT m.id, ml.id
                 FROM messages m
                 JOIN mailing_lists ml ON m.mailing_list = ml.nntp_group
                 WHERE m.mailing_list IS NOT NULL",
                (),
            )
            .await;

        // Findings table migration
        let _ = self
            .try_add_column("findings", "severity_explanation", "TEXT")
            .await;
        // Ignore errors for these as they might fail on new DBs or if already migrated
        let _ = self
            .conn
            .execute("ALTER TABLE findings RENAME COLUMN message TO problem", ())
            .await;
        let _ = self
            .conn
            .execute("ALTER TABLE findings DROP COLUMN file_path", ())
            .await;
        let _ = self
            .conn
            .execute("ALTER TABLE findings DROP COLUMN line_number", ())
            .await;

        let _ = self.migrate_tool_usages().await;
        Ok(())
    }

    pub async fn get_mailing_list_id_by_name(&self, name: &str) -> Result<Option<i64>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM mailing_lists WHERE nntp_group = ?",
                libsql::params![name],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub async fn add_message_to_mailing_list(
        &self,
        message_id: i64,
        mailing_list_id: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO messages_mailing_lists (message_id, mailing_list_id) VALUES (?, ?)",
                libsql::params![message_id, mailing_list_id],
            )
            .await?;
        Ok(())
    }

    pub async fn get_mailing_lists(&self) -> Result<Vec<(String, String)>> {
        let mut rows = self
            .conn
            .query("SELECT name, nntp_group FROM mailing_lists", ())
            .await?;
        let mut lists = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            lists.push((row.get(0)?, row.get(1)?));
        }
        Ok(lists)
    }

    pub async fn get_pending_review_id(
        &self,
        patchset_id: i64,
        patch_id: Option<i64>,
    ) -> Result<Option<i64>> {
        let mut rows = match patch_id {
            Some(pid) => {
                self.conn.query("SELECT id FROM reviews WHERE patchset_id = ? AND patch_id = ? AND status = 'Pending' LIMIT 1", libsql::params![patchset_id, pid]).await?
            }
            None => {
                self.conn.query("SELECT id FROM reviews WHERE patchset_id = ? AND patch_id IS NULL AND status = 'Pending' LIMIT 1", libsql::params![patchset_id]).await?
            }
        };
        if let Ok(Some(row)) = rows.next().await {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub async fn create_review(
        &self,
        patchset_id: i64,
        patch_id: Option<i64>,
        provider: &str,
        model: &str,
        baseline_id: Option<i64>,
        prompts_hash: Option<&str>,
    ) -> Result<i64> {
        let mut rows = self
            .conn
            .query(
                "INSERT INTO reviews (patchset_id, patch_id, status, created_at, provider, model, baseline_id, prompts_hash)
             VALUES (?, ?, 'Pending', ?, ?, ?, ?, ?) RETURNING id",
                libsql::params![
                    patchset_id,
                    patch_id,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_secs() as i64,
                    provider,
                    model,
                    baseline_id,
                    prompts_hash
                ],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to get review ID"))
        }
    }

    pub async fn has_successful_review(
        &self,
        patchset_id: i64,
        patch_id: i64,
        baseline_id: Option<i64>,
    ) -> Result<bool> {
        Ok(self
            .count_successful_reviews(patchset_id, patch_id, baseline_id)
            .await?
            > 0)
    }

    pub async fn count_successful_reviews(
        &self,
        patchset_id: i64,
        patch_id: i64,
        _baseline_id: Option<i64>,
    ) -> Result<usize> {
        let mut rows = self.conn
            .query(
                "SELECT COUNT(*) FROM reviews WHERE patchset_id = ? AND patch_id = ? AND status = 'Reviewed'",
                libsql::params![patchset_id, patch_id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        } else {
            Ok(0)
        }
    }

    pub async fn has_failed_review(
        &self,
        patchset_id: i64,
        patch_id: i64,
        _baseline_id: Option<i64>,
    ) -> Result<bool> {
        let mut rows = self.conn
            .query(
                "SELECT 1 FROM reviews WHERE patchset_id = ? AND patch_id = ? AND status IN ('Failed', 'FailedToApply') AND interaction_id IS NULL",
                libsql::params![patchset_id, patch_id],
            )
            .await?;

        Ok(rows.next().await.ok().flatten().is_some())
    }

    pub async fn update_review_status(
        &self,
        review_id: i64,
        status: &str,
        logs: Option<&str>,
    ) -> Result<()> {
        if let Some(l) = logs {
            self.conn
                .execute(
                    "UPDATE reviews SET status = ?, logs = ? WHERE id = ?",
                    libsql::params![status, l, review_id],
                )
                .await?;
        } else {
            self.conn
                .execute(
                    "UPDATE reviews SET status = ? WHERE id = ?",
                    libsql::params![status, review_id],
                )
                .await?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn complete_review(
        &self,
        review_id: i64,
        status: &str,
        result: &str,
        summary: Option<&str>,
        interaction_id: Option<&str>,
        inline_review: Option<&str>,
        logs: Option<&str>,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE reviews SET status = ?, result_description = ?, summary = ?, interaction_id = ?, inline_review = ?, logs = ? WHERE id = ?",
                libsql::params![status, result, summary, interaction_id, inline_review, logs, review_id],
            )
            .await?;
        Ok(())
    }

    pub async fn create_ai_interaction(&self, params: AiInteractionParams<'_>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO ai_interactions (id, parent_interaction_id, workflow_id, provider, model, input_context, output_raw, tokens_in, tokens_out, tokens_cached, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            libsql::params![
                params.id,
                params.parent_id,
                params.workflow_id,
                params.provider,
                params.model,
                params.input,
                params.output,
                params.tokens_in,
                params.tokens_out,
                params.tokens_cached,
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64
            ],
        ).await?;
        Ok(())
    }

    pub async fn create_tool_usage(&self, usage: ToolUsage) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tool_usages (review_id, provider, model, tool_name, arguments, output_length, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            libsql::params![
                usage.review_id,
                usage.provider,
                usage.model,
                usage.tool_name,
                usage.arguments,
                usage.output_length,
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64
            ],
        ).await?;
        Ok(())
    }

    pub async fn migrate_tool_usages(&self) -> Result<()> {
        // 1. Check if we have logs to parse
        info!("Migration: Checking for tool usages to backfill...");
        let mut rows = self.conn.query("SELECT id, logs, provider, model FROM reviews WHERE status IN ('Reviewed', 'Failed') AND logs IS NOT NULL", ()).await?;

        while let Ok(Some(row)) = rows.next().await {
            let review_id: i64 = row.get(0)?;
            let logs: String = row.get(1)?;
            let provider: String = row.get(2).unwrap_or_else(|_| "unknown".to_string());
            let model: String = row.get(3).unwrap_or_else(|_| "unknown".to_string());

            // Check if already populated
            let count_rows = self
                .conn
                .query(
                    "SELECT count(*) FROM tool_usages WHERE review_id = ?",
                    libsql::params![review_id],
                )
                .await;
            if let Ok(mut c_rows) = count_rows
                && let Ok(Some(c_row)) = c_rows.next().await
            {
                let count: i64 = c_row.get(0)?;
                if count > 0 {
                    continue;
                }
            }

            // Parse logs (simple JSON array parsing)
            if let Ok(history) = serde_json::from_str::<Vec<serde_json::Value>>(&logs) {
                for item in history {
                    if let Some(parts) = item.get("parts").and_then(|p| p.as_array()) {
                        for part in parts {
                            // Check for function call
                            if let Some(call) = part.get("functionCall") {
                                let name = call["name"].as_str().unwrap_or("unknown");
                                let args = call["args"].to_string();
                                // We need to find the response to get the output length.
                                // But here we iterate linearly.
                                // Estimate or find the next part.
                                // Record usage without output length for now.
                                // Attempt to find the corresponding functionResponse in subsequent parts.
                                // The history interleaves them.

                                // Insert what we have for now.
                                let _ = self
                                    .create_tool_usage(ToolUsage {
                                        review_id,
                                        provider: provider.clone(),
                                        model: model.clone(),
                                        tool_name: name.to_string(),
                                        arguments: Some(args),
                                        output_length: 0, // Placeholder
                                    })
                                    .await;
                            }
                            // If we want exact output length, we need to match functionResponse.
                            // But that might be complex for this simple migration.
                            if let Some(_resp) = part.get("functionResponse") {
                                // Update the previous entry.
                                // Or just ignore output length for backfill.
                            }
                        }
                    }
                }
            }
        }
        info!("Migration: verified tool usages.");
        let _ = self.migrate_findings().await;
        Ok(())
    }

    pub async fn create_finding(&self, finding: Finding) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO findings (review_id, severity, severity_explanation, problem)
             VALUES (?, ?, ?, ?)",
                libsql::params![
                    finding.review_id,
                    finding.severity as i32,
                    finding.severity_explanation,
                    finding.problem,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn migrate_findings(&self) -> Result<()> {
        info!("Migration: Checking for findings to backfill...");
        // Select reviews that have AI output but maybe no findings in the new table
        let sql = "SELECT r.id, ai.output_raw 
                   FROM reviews r 
                   JOIN ai_interactions ai ON r.interaction_id = ai.id
                   WHERE r.status = 'Reviewed'";

        let mut rows = self.conn.query(sql, ()).await?;

        while let Ok(Some(row)) = rows.next().await {
            let review_id: i64 = row.get(0)?;
            let output_raw: String = row.get(1)?;

            // Check if we already have findings for this review
            let count_check = self
                .conn
                .query(
                    "SELECT count(*) FROM findings WHERE review_id = ?",
                    libsql::params![review_id],
                )
                .await;
            if let Ok(mut c_rows) = count_check
                && let Ok(Some(c_row)) = c_rows.next().await
            {
                let count: i64 = c_row.get(0)?;
                if count > 0 {
                    continue;
                }
            }

            // Parse JSON and insert findings
            if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&output_raw)
                && let Some(findings_arr) = json_val.get("findings").and_then(|f| f.as_array())
            {
                for f in findings_arr {
                    let severity_str = f["severity"].as_str().unwrap_or("Low");

                    // New format: problem, severity_explanation
                    // Old format: message
                    let problem = if let Some(p) = f.get("problem").and_then(|s| s.as_str()) {
                        p.to_string()
                    } else {
                        f["message"].as_str().unwrap_or("").to_string()
                    };

                    let severity_explanation = f
                        .get("severity_explanation")
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string());

                    let severity = Severity::from_str(severity_str);

                    let _ = self
                        .create_finding(Finding {
                            review_id,
                            severity,
                            severity_explanation,
                            problem,
                        })
                        .await;
                }
            }
        }
        info!("Migration: verified findings.");
        Ok(())
    }

    pub async fn get_timeline_stats(&self, subsystem_id: Option<i64>) -> Result<serde_json::Value> {
        let mut messages_data = Vec::new();

        if let Some(sid) = subsystem_id {
            let sql_msgs =
                "SELECT strftime('%Y-%m-%d', date, 'unixepoch') as day, count(*) FROM messages m
             JOIN messages_subsystems ms ON m.id = ms.message_id
             WHERE ms.subsystem_id = ?
             GROUP BY day ORDER BY day";
            let mut rows = self.conn.query(sql_msgs, libsql::params![sid]).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let count: i64 = row.get(1)?;
                    messages_data.push(json!({"day": day, "count": count}));
                }
            }
        } else {
            let sql_msgs = "SELECT strftime('%Y-%m-%d', date, 'unixepoch') as day, count(*) FROM messages GROUP BY day ORDER BY day";
            let mut rows = self.conn.query(sql_msgs, ()).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let count: i64 = row.get(1)?;
                    messages_data.push(json!({"day": day, "count": count}));
                }
            }
        }

        let mut patchsets_data = Vec::new();
        if let Some(sid) = subsystem_id {
            let sql = "SELECT strftime('%Y-%m-%d', date, 'unixepoch') as day, status, count(*) FROM patchsets p
             JOIN patchsets_subsystems ps ON p.id = ps.patchset_id
             WHERE ps.subsystem_id = ?
             GROUP BY day, status ORDER BY day";
            let mut rows = self.conn.query(sql, libsql::params![sid]).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let status: Option<String> = row.get(1).ok();
                    let count: i64 = row.get(2)?;
                    patchsets_data.push(
                        json!({"day": day, "status": status.unwrap_or_default(), "count": count}),
                    );
                }
            }
        } else {
            let sql = "SELECT strftime('%Y-%m-%d', date, 'unixepoch') as day, status, count(*) FROM patchsets GROUP BY day, status ORDER BY day";
            let mut rows = self.conn.query(sql, ()).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let status: Option<String> = row.get(1).ok();
                    let count: i64 = row.get(2)?;
                    patchsets_data.push(
                        json!({"day": day, "status": status.unwrap_or_default(), "count": count}),
                    );
                }
            }
        }

        // Patches stats (individual patches)
        let mut patches_data = Vec::new();
        if let Some(sid) = subsystem_id {
            let sql =
                "SELECT strftime('%Y-%m-%d', m.date, 'unixepoch') as day, count(*) FROM patches p
              JOIN messages m ON p.message_id = m.message_id
              JOIN patches_subsystems ps ON p.id = ps.patch_id
              WHERE ps.subsystem_id = ?
              GROUP BY day ORDER BY day";
            let mut rows = self.conn.query(sql, libsql::params![sid]).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let count: i64 = row.get(1)?;
                    patches_data.push(json!({"day": day, "count": count}));
                }
            }
        } else {
            let sql =
                "SELECT strftime('%Y-%m-%d', m.date, 'unixepoch') as day, count(*) FROM patches p
              JOIN messages m ON p.message_id = m.message_id
              GROUP BY day ORDER BY day";
            let mut rows = self.conn.query(sql, ()).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let count: i64 = row.get(1)?;
                    patches_data.push(json!({"day": day, "count": count}));
                }
            }
        }

        // Reviews stats (outcomes over time)
        let mut reviews_data = Vec::new();
        if let Some(sid) = subsystem_id {
            let sql = "SELECT 
                strftime('%Y-%m-%d', r.created_at, 'unixepoch') as day,
                r.status,
                COUNT(*) as count
            FROM reviews r
            JOIN patchsets_subsystems ps ON r.patchset_id = ps.patchset_id
            WHERE ps.subsystem_id = ?
            GROUP BY day, status
            ORDER BY day";
            let mut rows = self.conn.query(sql, libsql::params![sid]).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let status: String = row.get(1).unwrap_or_else(|_| "unknown".to_string());
                    let count: i64 = row.get(2)?;
                    reviews_data.push(json!({"day": day, "status": status, "count": count}));
                }
            }
        } else {
            let sql = "SELECT 
                strftime('%Y-%m-%d', r.created_at, 'unixepoch') as day,
                r.status,
                COUNT(*) as count
            FROM reviews r
            GROUP BY day, status
            ORDER BY day";
            let mut rows = self.conn.query(sql, ()).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let status: String = row.get(1).unwrap_or_else(|_| "unknown".to_string());
                    let count: i64 = row.get(2)?;
                    reviews_data.push(json!({"day": day, "status": status, "count": count}));
                }
            }
        }

        // Findings stats
        let mut findings_data = Vec::new();
        if let Some(sid) = subsystem_id {
            let sql = "SELECT 
                strftime('%Y-%m-%d', r.created_at, 'unixepoch') as day,
                CASE f.severity 
                    WHEN 1 THEN 'low' 
                    WHEN 2 THEN 'medium' 
                    WHEN 3 THEN 'high' 
                    WHEN 4 THEN 'critical' 
                    ELSE 'unknown' 
                END as severity,
                COUNT(*) as count
            FROM findings f
            JOIN reviews r ON f.review_id = r.id
            JOIN patchsets_subsystems ps ON r.patchset_id = ps.patchset_id
            WHERE ps.subsystem_id = ?
            GROUP BY day, severity
            ORDER BY day";
            let mut rows = self.conn.query(sql, libsql::params![sid]).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let severity: String = row.get(1).unwrap_or_else(|_| "unknown".to_string());
                    let count: i64 = row.get(2)?;
                    findings_data.push(json!({"day": day, "severity": severity, "count": count}));
                }
            }
        } else {
            let sql = "SELECT 
                strftime('%Y-%m-%d', r.created_at, 'unixepoch') as day,
                CASE f.severity 
                    WHEN 1 THEN 'low' 
                    WHEN 2 THEN 'medium' 
                    WHEN 3 THEN 'high' 
                    WHEN 4 THEN 'critical' 
                    ELSE 'unknown' 
                END as severity,
                COUNT(*) as count
            FROM findings f
            JOIN reviews r ON f.review_id = r.id
            GROUP BY day, severity
            ORDER BY day";
            let mut rows = self.conn.query(sql, ()).await?;
            while let Ok(Some(row)) = rows.next().await {
                if let Ok(day) = row.get::<String>(0) {
                    let severity: String = row.get(1).unwrap_or_else(|_| "unknown".to_string());
                    let count: i64 = row.get(2)?;
                    findings_data.push(json!({"day": day, "severity": severity, "count": count}));
                }
            }
        }

        Ok(json!({
            "messages": messages_data,
            "patchsets": patchsets_data,
            "patches": patches_data,
            "reviews": reviews_data,
            "findings": findings_data
        }))
    }

    pub async fn get_review_stats(&self) -> Result<serde_json::Value> {
        let sql = "SELECT 
            r.provider, 
            r.model, 
            r.status, 
            count(*),
            sum(COALESCE(ai.tokens_in, 0)),
            sum(COALESCE(ai.tokens_out, 0)),
            sum(COALESCE(ai.tokens_cached, 0))
        FROM reviews r
        LEFT JOIN ai_interactions ai ON r.interaction_id = ai.id
        GROUP BY r.provider, r.model, r.status";

        let mut rows = self.conn.query(sql, ()).await?;
        let mut stats = Vec::new();
        #[allow(clippy::similar_names)]
        while let Ok(Some(row)) = rows.next().await {
            let provider: Option<String> = row.get(0).ok();
            let model: Option<String> = row.get(1).ok();
            let status: Option<String> = row.get(2).ok();
            let count: i64 = row.get(3)?;
            let tokens_in: i64 = row.get(4).unwrap_or(0);
            let tokens_out: i64 = row.get(5).unwrap_or(0);
            let tokens_cached: i64 = row.get(6).unwrap_or(0);

            stats.push(json!({
                "provider": provider.unwrap_or_default(),
                "model": model.unwrap_or_default(),
                "status": status.unwrap_or_default(),
                "count": count,
                "tokens_in": tokens_in,
                "tokens_out": tokens_out,
                "tokens_cached": tokens_cached
            }));
        }
        Ok(json!(stats))
    }

    pub async fn get_tool_usage_stats(&self) -> Result<serde_json::Value> {
        let sql = "SELECT provider, model, tool_name, count(*), avg(output_length) FROM tool_usages GROUP BY provider, model, tool_name";
        let mut rows = self.conn.query(sql, ()).await?;
        let mut stats = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            let provider: Option<String> = row.get(0).ok();
            let model: Option<String> = row.get(1).ok();
            let tool_name: Option<String> = row.get(2).ok();
            let count: i64 = row.get(3)?;
            let avg_len: f64 = row.get(4).unwrap_or(0.0);
            stats.push(json!({
                "provider": provider.unwrap_or_default(),
                "model": model.unwrap_or_default(),
                "tool": tool_name.unwrap_or_default(),
                "count": count,
                "avg_output_length": avg_len
            }));
        }
        Ok(json!(stats))
    }

    pub async fn begin_transaction(&self) -> Result<()> {
        self.conn.execute("BEGIN IMMEDIATE", ()).await?;
        Ok(())
    }

    pub async fn commit_transaction(&self) -> Result<()> {
        self.conn.execute("COMMIT", ()).await?;
        Ok(())
    }

    async fn try_create_index(&self, index_name: &str, table: &str, column: &str) -> Result<()> {
        let sql = format!(
            "CREATE INDEX IF NOT EXISTS {} ON {}({})",
            index_name, table, column
        );
        if let Err(e) = self.conn.execute(&sql, ()).await {
            info!("Migration: Error creating index {}: {}", index_name, e);
        } else {
            info!("Migration: Ensured index {} exists", index_name);
        }
        Ok(())
    }

    async fn try_add_column(&self, table: &str, column: &str, type_def: &str) -> Result<()> {
        let sql = format!("ALTER TABLE {} ADD COLUMN {} {}", table, column, type_def);
        if let Err(_e) = self.conn.execute(&sql, ()).await {
            // Ignore error if column likely exists (duplicate column name)
            // info!("Migration: Column {} likely exists or error adding: {}", column, e);
        } else {
            info!("Migration: Added column {} to {}", column, table);
        }
        Ok(())
    }

    // People & Recipients
    pub async fn ensure_person(&self, name: Option<&str>, email: &str) -> Result<i64> {
        let email = email.trim();
        // Try to insert
        self.conn
            .execute(
                "INSERT OR IGNORE INTO people (name, email) VALUES (?, ?)",
                libsql::params![name, email],
            )
            .await?;

        // If a name is provided and the existing record has none, update it.
        // For now, keep it simple. Just get ID.
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM people WHERE email = ?",
                libsql::params![email],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to ensure person: {}", email))
        }
    }

    pub async fn add_message_recipient(
        &self,
        message_id: i64,
        person_id: i64,
        recipient_type: &str,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO messages_recipients (message_id, person_id, recipient_type) VALUES (?, ?, ?)",
                libsql::params![message_id, person_id, recipient_type],
            )
            .await?;
        Ok(())
    }

    // Subsystems
    pub async fn ensure_subsystem(&self, name: &str, mailing_list_address: &str) -> Result<i64> {
        // Try to insert
        self.conn
            .execute(
                "INSERT OR IGNORE INTO subsystems (name, mailing_list_address) VALUES (?, ?)",
                libsql::params![name, mailing_list_address],
            )
            .await?;

        // Get ID
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM subsystems WHERE mailing_list_address = ?",
                libsql::params![mailing_list_address],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            // Fallback: Get ID by name (Collision on name with different address)
            let mut rows = self
                .conn
                .query(
                    "SELECT id FROM subsystems WHERE name = ?",
                    libsql::params![name],
                )
                .await?;
            if let Ok(Some(row)) = rows.next().await {
                Ok(row.get(0)?)
            } else {
                Err(anyhow::anyhow!("Failed to ensure subsystem"))
            }
        }
    }

    pub async fn add_subsystem_to_message(
        &self,
        message_id_db: i64,
        subsystem_id: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO messages_subsystems (message_id, subsystem_id) VALUES (?, ?)",
                libsql::params![message_id_db, subsystem_id],
            )
            .await?;
        Ok(())
    }

    pub async fn add_subsystem_to_thread(&self, thread_id: i64, subsystem_id: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO threads_subsystems (thread_id, subsystem_id) VALUES (?, ?)",
                libsql::params![thread_id, subsystem_id],
            )
            .await?;
        Ok(())
    }

    pub async fn add_subsystem_to_patch(&self, patch_id: i64, subsystem_id: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO patches_subsystems (patch_id, subsystem_id) VALUES (?, ?)",
                libsql::params![patch_id, subsystem_id],
            )
            .await?;
        Ok(())
    }

    pub async fn add_subsystem_to_patchset(
        &self,
        patchset_id: i64,
        subsystem_id: i64,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO patchsets_subsystems (patchset_id, subsystem_id) VALUES (?, ?)",
                libsql::params![patchset_id, subsystem_id],
            )
            .await?;
        Ok(())
    }

    pub async fn get_message_id_by_msg_id(&self, msg_id: &str) -> Result<Option<i64>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM messages WHERE message_id = ?",
                libsql::params![msg_id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub async fn ensure_mailing_list(&self, name: &str, group: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO mailing_lists (name, nntp_group, last_article_num) VALUES (?, ?, 0)
                 ON CONFLICT(nntp_group) DO UPDATE SET name = excluded.name",
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
        let mut rows = self.conn
            .query(
                "INSERT INTO threads (root_message_id, subject, last_updated) VALUES (?, ?, ?) RETURNING id",
                libsql::params![root_message_id, subject, date],
            )
            .await?;

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
        let thread_id = self
            .create_thread(message_id, "(placeholder)", date)
            .await?;

        self.create_message(
            message_id,
            thread_id,
            None,
            "unknown",
            "(placeholder)",
            date,
            "",
            "",
            "",
            None,
            None,
        )
        .await?;

        Ok(thread_id)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_message(
        &self,
        message_id: &str,
        thread_id: i64,
        in_reply_to: Option<&str>,
        author: &str,
        subject: &str,
        date: i64,
        body: &str,
        to: &str,
        cc: &str,
        git_blob_hash: Option<&str>,
        mailing_list: Option<&str>,
    ) -> Result<()> {
        // Check for thread merge (Thread split resolution)
        if let Ok(Some(old_thread_id)) = self.get_thread_id_for_message(message_id).await
            && old_thread_id != thread_id
        {
            info!("Merging thread {} into {}", old_thread_id, thread_id);
            // 1. Move messages
            self.conn
                .execute(
                    "UPDATE messages SET thread_id = ? WHERE thread_id = ?",
                    libsql::params![thread_id, old_thread_id],
                )
                .await?;

            // 2. Move patchsets
            self.conn
                .execute(
                    "UPDATE patchsets SET thread_id = ? WHERE thread_id = ?",
                    libsql::params![thread_id, old_thread_id],
                )
                .await?;

            // 3. Merge subsystems
            self.conn
                .execute(
                    "UPDATE OR IGNORE threads_subsystems SET thread_id = ? WHERE thread_id = ?",
                    libsql::params![thread_id, old_thread_id],
                )
                .await?;
            // Delete any remaining (conflicting) subsystem mappings for the old thread
            self.conn
                .execute(
                    "DELETE FROM threads_subsystems WHERE thread_id = ?",
                    libsql::params![old_thread_id],
                )
                .await?;

            // 5. Delete old thread
            self.conn
                .execute(
                    "DELETE FROM threads WHERE id = ?",
                    libsql::params![old_thread_id],
                )
                .await?;
        }

        // Use INSERT OR REPLACE to handle updating placeholders.
        // We want to preserve thread_id if it was set by placeholder (which is correct).
        // Actually, if we are "creating" the real message now, we should overwrite the placeholder fields.
        // Ensure the same thread_id is kept if it exists.
        // The caller (main.rs) resolves thread_id before calling create_message.
        // If we found a placeholder, we use its thread_id.
        // So here we just upsert.

        // Blindly replacing might change the thread_id if a different one is passed.
        // But main.rs logic should ensure consistency.
        // Use INSERT OR REPLACE.
        self.conn.execute(
            "INSERT INTO messages (message_id, thread_id, in_reply_to, author, subject, date, body, to_recipients, cc_recipients, git_blob_hash, mailing_list) 
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(message_id) DO UPDATE SET
                thread_id=excluded.thread_id,
                in_reply_to=excluded.in_reply_to,
                author=excluded.author,
                subject=excluded.subject,
                date=excluded.date,
                body=excluded.body,
                to_recipients=excluded.to_recipients,
                cc_recipients=excluded.cc_recipients,
                git_blob_hash=excluded.git_blob_hash,
                mailing_list=excluded.mailing_list",
            libsql::params![message_id, thread_id, in_reply_to, author, subject, date, body, to, cc, git_blob_hash, mailing_list],
        ).await?;
        Ok(())
    }

    pub async fn create_baseline(
        &self,
        repo_url: Option<&str>,
        branch: Option<&str>,
        commit: Option<&str>,
    ) -> Result<i64> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM baselines WHERE repo_url IS ? AND branch IS ? AND last_known_commit IS ?",
                libsql::params![repo_url, branch, commit],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            return Ok(row.get(0)?);
        }

        let mut rows = self.conn
            .query(
                "INSERT INTO baselines (repo_url, branch, last_known_commit) VALUES (?, ?, ?) RETURNING id",
                libsql::params![repo_url, branch, commit],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to get baseline ID"))
        }
    }

    pub async fn get_baseline_commit(&self, id: i64) -> Result<Option<String>> {
        let mut rows = self
            .conn
            .query(
                "SELECT last_known_commit FROM baselines WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0).ok())
        } else {
            Ok(None)
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_patchset(
        &self,
        thread_id: i64,
        cover_letter_message_id: Option<&str>,
        message_id: &str,
        subject: &str,
        author: &str,
        date: i64,
        total_parts: u32,
        parser_version: i32,
        to: &str,
        cc: &str,
        version: Option<u32>,
        part_index: u32,
        baseline_id: Option<i64>,
        strict_author: bool,
        skip_filters: Option<&Vec<String>>,
        only_filters: Option<&Vec<String>>,
    ) -> Result<Option<i64>> {
        let skip_filters_json = skip_filters.map(|f| serde_json::to_string(f).unwrap_or_default());
        let only_filters_json = only_filters.map(|f| serde_json::to_string(f).unwrap_or_default());
        // 1. Try to find by cover_letter_message_id first (handles placeholders from API/Fetcher)
        let mut clid_candidates = Vec::new();
        if let Some(clid) = cover_letter_message_id {
            clid_candidates.push(clid.to_string());
        }
        // Fallback for single-patch git imports where placeholder is sha@sashiko.local
        // but the actual cover letter becomes the sha itself.
        clid_candidates.push(format!("{}@sashiko.local", message_id));

        for clid in clid_candidates {
            let mut rows = self
                .conn
                .query(
                    "SELECT id, date, author, subject, subject_index, total_parts FROM patchsets WHERE cover_letter_message_id = ?",
                    libsql::params![clid.clone()],
                )
                .await?;
            if let Ok(Some(row)) = rows.next().await {
                // Found it! Use this ID. We'll update its fields below.
                let id: i64 = row.get(0)?;
                let subject_index: u32 = row.get(4).unwrap_or(9999);
                let existing_total: u32 = row.get(5).unwrap_or(1);

                // Prevent downgrading a series to a singleton if we already have multiple parts.
                // This handles cases where a singleton root (1/1) overwrites a series (N/N) inferred from replies.
                let final_total = if total_parts == 1 && existing_total > 1 {
                    existing_total
                } else {
                    total_parts
                };

                // We proceed to update this record with the full metadata
                self.conn.execute(
                    "UPDATE patchsets SET thread_id = ?, author = ?, total_parts = ?, parser_version = ?, to_recipients = ?, cc_recipients = ? WHERE id = ?",
                    libsql::params![thread_id, author, final_total, parser_version, to, cc, id],
                ).await?;

                if let Some(real_clid) = cover_letter_message_id {
                    self.conn
                        .execute(
                            "UPDATE patchsets SET cover_letter_message_id = ? WHERE id = ?",
                            libsql::params![real_clid, id],
                        )
                        .await?;
                }

                if let Some(bid) = baseline_id {
                    self.conn
                        .execute(
                            "UPDATE patchsets SET baseline_id = ? WHERE id = ?",
                            libsql::params![bid, id],
                        )
                        .await?;
                }

                // Update subject if this is a better index (e.g. going from placeholder to real subject)
                if part_index < subject_index {
                    self.conn
                        .execute(
                            "UPDATE patchsets SET subject = ?, subject_index = ? WHERE id = ?",
                            libsql::params![subject, part_index, id],
                        )
                        .await?;
                }

                self.conn.execute(
                    "UPDATE patchsets SET status = 'Incomplete' WHERE id = ? AND status = 'Fetching'",
                    libsql::params![id],
                ).await?;

                self.conn.execute(
                    "UPDATE patchsets SET status = 'Pending' WHERE id = ? AND received_parts >= total_parts AND status IN ('Incomplete', 'Fetching')",
                    libsql::params![id],
                ).await?;

                return Ok(Some(id));
            }
        }

        // 2. Normal matching logic: Find candidate patchsets in this thread OR matching author/time
        // We expand the search window to finding ANY patchset by this author in the last 24h
        let window_start = date - 86400;
        let window_end = date + 86400;
        let mut rows = self
            .conn
            .query(
                "SELECT id, date, author, subject, subject_index, total_parts, received_parts, cover_letter_message_id, thread_id FROM patchsets 
                 WHERE thread_id = ? OR (author = ? AND date BETWEEN ? AND ?)",
                libsql::params![thread_id, author, window_start, window_end],
            )
            .await?;

        let mut matches = Vec::new();

        while let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            let existing_date: i64 = row.get(1)?;
            let existing_author: String = row.get(2)?;
            let existing_subject: String = row.get(3)?;
            let existing_subject_index: u32 = row.get(4).unwrap_or(9999);
            let existing_total: u32 = row.get(5).unwrap_or(1);
            let existing_received: u32 = row.get(6).unwrap_or(0);
            let existing_cover_id: Option<String> = row.get(7).ok();
            let existing_thread_id: Option<i64> = row.get(8).ok();

            // Check if this message is already part of this patchset (Duplicate processing)
            // 1. Check if it is the cover letter.
            let is_cover_duplicate = existing_cover_id.as_deref() == Some(message_id);

            // 2. Check if it is an existing patch.
            let is_patch_duplicate = if !is_cover_duplicate {
                let mut p_rows = self
                    .conn
                    .query(
                        "SELECT 1 FROM patches WHERE patchset_id = ? AND message_id = ?",
                        libsql::params![id, message_id],
                    )
                    .await?;
                p_rows.next().await.ok().flatten().is_some()
            } else {
                false
            };

            let is_duplicate = is_cover_duplicate || is_patch_duplicate;

            // If the patchset is already full, do not merge more patches into it,
            // UNLESS it is a duplicate of a message already in the set.
            // This prevents merging unrelated patchsets that happen to look similar (same author/size).
            if existing_received >= existing_total && !is_duplicate && part_index != 0 {
                continue;
            }

            // Parse version from existing subject
            let existing_version = crate::patch::parse_subject_version(&existing_subject);

            // Clean subjects for comparison
            let clean_new = crate::patch::clean_subject(subject);
            let clean_old = crate::patch::clean_subject(&existing_subject);

            // Check for index collision
            // If the patchset already contains a patch with this index (and different message_id), it's a collision.
            // This prevents merging [PATCH 1/2] Series A and [PATCH 1/2] Series B.
            let index_collision = if part_index == 0 {
                existing_cover_id.is_some()
                    && existing_cover_id.as_deref() != Some(message_id)
                    && existing_subject_index == 0
            } else {
                let mut p_rows = self
                    .conn
                    .query(
                        "SELECT 1 FROM patches WHERE patchset_id = ? AND part_index = ? AND message_id != ?",
                        libsql::params![id, part_index, message_id],
                    )
                    .await?;
                p_rows.next().await.ok().flatten().is_some()
            };

            // Matching logic:
            // 1. Author matches OR it's a multi-part series with matching total_parts (trusting thread context)
            //    BUT strict_author enforces strict author matching (for Email/NNTP).
            // 2. Time must be close (within 24 hours / 86400s)
            // 3. Total parts must match
            // 4. Versions must match (treating None as v1)
            // 5. For singletons (total=1), Subject must match (fuzzy) to avoid merging unrelated patches

            let v_new = version.unwrap_or(1);
            let v_old = existing_version.unwrap_or(1);
            let versions_compatible = v_new == v_old;

            let is_singleton = total_parts == 1;
            // For singletons, we require the subject to be somewhat similar to avoid merging unrelated patches.
            let subject_match = if is_singleton {
                if subject == existing_subject {
                    true
                } else {
                    // Allow merging 0/1 (cover) and 1/1 (patch) even if subjects differ
                    if (part_index == 0 && existing_subject_index == 1)
                        || (part_index == 1 && existing_subject_index == 0)
                    {
                        true
                    } else {
                        clean_new == clean_old
                    }
                }
            } else {
                // For series:
                // If we are replacing/matching the SAME index as the one that defined the patchset subject,
                // we require the subjects to match.
                // e.g. [PATCH 1/2] Series A vs [PATCH 1/2] Series B -> Mismatch.
                if part_index == existing_subject_index {
                    clean_new == clean_old
                } else {
                    true // For other parts (1/N vs 2/N), subjects differ naturally.
                }
            };

            // Relaxed author check logic
            let author_match = crate::patch::extract_email(&existing_author)
                == crate::patch::extract_email(author);
            let series_match = (total_parts > 1 && total_parts == existing_total)
                || existing_total == 1
                || total_parts == 1;

            let author_or_series_match = if strict_author {
                author_match
            } else {
                author_match || series_match
            };

            // Prefix matching (to separate different series from same author)
            let same_thread = existing_thread_id == Some(thread_id);
            let prefix_match = if same_thread {
                true // Trust thread
            } else {
                let new_prefixes = crate::patch::get_subject_prefixes(subject);
                let old_prefixes = crate::patch::get_subject_prefixes(&existing_subject);
                new_prefixes == old_prefixes
            };

            // Thread Enforcement: To prevent cross-thread "stealing" of patches for resends of the same series,
            // we strictly require multi-part series patches to belong to the same thread.
            let thread_compatible = same_thread || is_singleton;

            if author_or_series_match
                && (!strict_author || (date - existing_date).abs() < 86400)
                && (versions_compatible || same_thread)
                && (total_parts == existing_total || existing_total == 1 || total_parts == 1)
                && subject_match
                && prefix_match
                && thread_compatible
                && !index_collision
            {
                matches.push((id, existing_subject_index));
            }
        }

        if !matches.is_empty() {
            // Sort matches to pick the "best" one to keep (e.g. oldest ID or one with lowest subject index)
            // Let's keep the one with the lowest ID (created first)
            matches.sort_by_key(|k| k.0);

            let target_id = matches[0].0;
            let mut current_subject_index = matches[0].1;

            // If we have multiple matches, merge others into target_id
            for (merge_from_id, merge_subject_index) in matches.iter().skip(1) {
                let merge_from_id = *merge_from_id;
                info!("Merging patchset {} into {}", merge_from_id, target_id);

                // Reassign patches
                self.conn
                    .execute(
                        "UPDATE OR IGNORE patches SET patchset_id = ? WHERE patchset_id = ?",
                        libsql::params![target_id, merge_from_id],
                    )
                    .await?;

                // Reassign reviews
                self.conn
                    .execute(
                        "UPDATE reviews SET patchset_id = ? WHERE patchset_id = ?",
                        libsql::params![target_id, merge_from_id],
                    )
                    .await?;

                // Merge subsystems
                self.conn
                    .execute(
                        "INSERT OR IGNORE INTO patchsets_subsystems (patchset_id, subsystem_id)
                         SELECT ?, subsystem_id FROM patchsets_subsystems WHERE patchset_id = ?",
                        libsql::params![target_id, merge_from_id],
                    )
                    .await?;
                self.conn
                    .execute(
                        "DELETE FROM patchsets_subsystems WHERE patchset_id = ?",
                        libsql::params![merge_from_id],
                    )
                    .await?;

                // If the merged patchset had a better subject index, track it
                if *merge_subject_index < current_subject_index {
                    current_subject_index = *merge_subject_index;
                }

                // Delete the merged patchset
                self.conn
                    .execute(
                        "DELETE FROM patchsets WHERE id = ?",
                        libsql::params![merge_from_id],
                    )
                    .await?;
            }

            // Update the target patchset
            self.conn.execute(
                "UPDATE patchsets SET author = ?, total_parts = ?, parser_version = ?, to_recipients = ?, cc_recipients = ? WHERE id = ?",
                libsql::params![author, total_parts, parser_version, to, cc, target_id],
            ).await?;

            if skip_filters_json.is_some() || only_filters_json.is_some() {
                self.conn.execute(
                    "UPDATE patchsets SET skip_filters = COALESCE(?, skip_filters), only_filters = COALESCE(?, only_filters) WHERE id = ?",
                    libsql::params![skip_filters_json.clone(), only_filters_json.clone(), target_id],
                ).await?;
            }

            if let Some(bid) = baseline_id {
                self.conn
                    .execute(
                        "UPDATE patchsets SET baseline_id = ? WHERE id = ?",
                        libsql::params![bid, target_id],
                    )
                    .await?;
            }

            // Conditionally update subject
            // Note: We check against the best index found among all merged sets OR the new part_index
            if part_index < current_subject_index {
                self.conn
                    .execute(
                        "UPDATE patchsets SET subject = ?, subject_index = ? WHERE id = ?",
                        libsql::params![subject, part_index, target_id],
                    )
                    .await?;
            } else if matches.len() > 1 {
                // If we merged, we might need to update the subject index of the target to the best one we found.
                // But we don't have the subject string from the merged one easily available here.
                // However, the existing target subject is likely fine unless part_index is better.
                // Update subject_index to be correct if a better one was merged.
                // Actually, if matches[i].1 was better, we should have used its subject.
                // But that's complicated. Assuming the target (oldest) usually has the cover letter or we eventually find it.
                // Simplification: We only update if CURRENT patch is better.
                // If we merged a patchset that HAD the cover letter, we ideally want that subject.
                // But we lost it.
                // TODO: Optimize merge subject selection. For now, this is better than duplicates.
            }

            if let Some(clid) = cover_letter_message_id {
                self.conn
                    .execute(
                        "UPDATE patchsets SET cover_letter_message_id = ? WHERE id = ?",
                        libsql::params![clid, target_id],
                    )
                    .await?;
            }

            // Recalculate received parts for target (in case we merged)
            self.conn
            .execute(
                "UPDATE patchsets SET received_parts = (SELECT COUNT(*) FROM patches WHERE patchset_id = ?) WHERE id = ?",
                libsql::params![target_id, target_id],
            )
            .await?;

            self.conn.execute(
                "UPDATE patchsets SET status = 'Incomplete' WHERE id = ? AND status = 'Fetching'",
                libsql::params![target_id],
            ).await?;

            self.conn.execute(
                "UPDATE patchsets SET status = 'Pending' WHERE id = ? AND received_parts >= total_parts AND status IN ('Incomplete', 'Fetching')",
                libsql::params![target_id],
            ).await?;

            return Ok(Some(target_id));
        }

        // No match found, create new patchset
        let mut rows = self.conn
            .query(
                "INSERT INTO patchsets (thread_id, cover_letter_message_id, subject, author, date, total_parts, received_parts, status, parser_version, to_recipients, cc_recipients, subject_index, baseline_id, skip_filters, only_filters) 
                 VALUES (?, ?, ?, ?, ?, ?, 0, 'Incomplete', ?, ?, ?, ?, ?, ?, ?) RETURNING id",
                libsql::params![thread_id, cover_letter_message_id, subject, author, date, total_parts, parser_version, to, cc, part_index, baseline_id, skip_filters_json.clone(), only_filters_json.clone()],
            )
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
    ) -> Result<i64> {
        // Check if patch exists and get old patchset_id to fix counts if we steal it
        let old_patchset_id: Option<i64> = {
            let mut rows = self
                .conn
                .query(
                    "SELECT patchset_id FROM patches WHERE message_id = ?",
                    libsql::params![message_id],
                )
                .await?;
            if let Ok(Some(row)) = rows.next().await {
                Some(row.get(0)?)
            } else {
                None
            }
        };

        // Insert or Update (Move patch to new patchset if duplicate)
        self.conn.execute(
            "INSERT INTO patches (patchset_id, message_id, part_index, diff) VALUES (?, ?, ?, ?)
             ON CONFLICT(message_id) DO UPDATE SET
                patchset_id=excluded.patchset_id,
                part_index=excluded.part_index,
                diff=excluded.diff",
            libsql::params![patchset_id, message_id, part_index, diff]
        ).await?;

        // Update received_parts for the NEW patchset
        self.conn
            .execute(
                "UPDATE patchsets SET received_parts = (SELECT COUNT(*) FROM patches WHERE patchset_id = ?) WHERE id = ?",
                libsql::params![patchset_id, patchset_id],
            )
            .await?;

        // Update received_parts for the OLD patchset (if we moved it)
        if let Some(old_id) = old_patchset_id
            && old_id != patchset_id
        {
            self.conn
                        .execute(
                            "UPDATE patchsets SET received_parts = (SELECT COUNT(*) FROM patches WHERE patchset_id = ?) WHERE id = ?",
                            libsql::params![old_id, old_id],
                        )
                        .await?;
        }

        // Check if complete and update status
        // We transition from 'Incomplete' OR 'Fetching' to 'Pending' (ready for review)
        self.conn.execute(
            "UPDATE patchsets SET status = 'Pending' WHERE id = ? AND received_parts >= total_parts AND status IN ('Incomplete', 'Fetching')",
            libsql::params![patchset_id],
        ).await?;

        // Get the patch ID
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM patches WHERE message_id = ?",
                libsql::params![message_id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to get patch ID"))
        }
    }

    fn build_search(
        &self,
        query: Option<String>,
        mailing_list: Option<String>,
        target: &str,
    ) -> (String, Vec<String>) {
        let mut conditions = Vec::new();
        let mut params = Vec::new();

        // Always exclude placeholders
        conditions.push("subject != '(placeholder)'".to_string());

        if let Some(list) = mailing_list
            && !list.is_empty()
        {
            if target == "patchset" {
                // Filter patchsets where any patch OR the cover letter is in the mailing list
                // We use p.id to avoid ambiguity with joined tables (e.g. subsystems.id)
                conditions.push(
                    "p.id IN (
                        SELECT patchset_id FROM patches p2 
                        JOIN messages m ON p2.message_id = m.message_id 
                        JOIN messages_mailing_lists mml ON m.id = mml.message_id 
                        JOIN mailing_lists ml ON mml.mailing_list_id = ml.id 
                        WHERE ml.nntp_group = ?
                        UNION
                        SELECT ps.id FROM patchsets ps 
                        JOIN messages m ON ps.cover_letter_message_id = m.message_id 
                        JOIN messages_mailing_lists mml ON m.id = mml.message_id 
                        JOIN mailing_lists ml ON mml.mailing_list_id = ml.id 
                        WHERE ml.nntp_group = ?
                    )"
                    .to_string(),
                );
                params.push(list.clone());
                params.push(list);
            } else {
                // Filter messages
                conditions.push("id IN (SELECT message_id FROM messages_mailing_lists mml JOIN mailing_lists ml ON mml.mailing_list_id = ml.id WHERE ml.nntp_group = ?)".to_string());
                params.push(list);
            }
        }

        if let Some(q) = query {
            let q = q.trim();
            if !q.is_empty() {
                if let Some(val) = q.strip_prefix("author:") {
                    conditions.push("author LIKE ?".to_string());
                    params.push(format!("%{}%", val.trim()));
                } else if let Some(val) = q.strip_prefix("subject:") {
                    conditions.push("subject LIKE ?".to_string());
                    params.push(format!("%{}%", val.trim()));
                } else if let Some(val) = q.strip_prefix("date:") {
                    conditions.push("datetime(date, 'unixepoch') LIKE ?".to_string());
                    params.push(format!("%{}%", val.trim()));
                } else if let Some(val) = q.strip_prefix("subsystem:") {
                    let sub_query = if target == "patchset" {
                        "p.id IN (SELECT patchset_id FROM patchsets_subsystems ps JOIN subsystems s ON ps.subsystem_id = s.id WHERE s.name LIKE ?)"
                    } else {
                        "id IN (SELECT message_id FROM messages_subsystems ms JOIN subsystems s ON ms.subsystem_id = s.id WHERE s.name LIKE ?)"
                    };
                    conditions.push(sub_query.to_string());
                    params.push(format!("%{}%", val.trim()));
                } else {
                    conditions.push("(subject LIKE ? OR author LIKE ?)".to_string());
                    params.push(format!("%{}%", q));
                    params.push(format!("%{}%", q));
                }
            }
        }

        if conditions.is_empty() {
            (String::new(), vec![])
        } else {
            (format!("WHERE {}", conditions.join(" AND ")), params)
        }
    }

    pub async fn get_patchsets(
        &self,
        limit: usize,
        offset: usize,
        query: Option<String>,
        mailing_list: Option<String>,
    ) -> Result<Vec<PatchsetRow>> {
        let (where_clause, params) = self.build_search(query, mailing_list, "patchset");
        // We use p.* alias implicitely by using unqualified names in WHERE which is fine given no collisions.
        // But for clarity/safety we should alias in FROM.
        // build_search returns "WHERE author ...".

        let sql = format!(
            "SELECT p.id, p.subject, p.status, p.thread_id, p.author, p.date, p.cover_letter_message_id, p.total_parts, p.received_parts, GROUP_CONCAT(s.name, ','),
             COALESCE(f.low, 0), COALESCE(f.medium, 0), COALESCE(f.high, 0), COALESCE(f.critical, 0), p.baseline_id, p.failed_reason, p.target_review_count, p.skip_filters, p.only_filters
             FROM patchsets p
             LEFT JOIN patchsets_subsystems ps ON p.id = ps.patchset_id
             LEFT JOIN subsystems s ON ps.subsystem_id = s.id
             LEFT JOIN (
                SELECT r.patchset_id,
                    SUM(CASE WHEN f.severity = 1 THEN 1 ELSE 0 END) as low,
                    SUM(CASE WHEN f.severity = 2 THEN 1 ELSE 0 END) as medium,
                    SUM(CASE WHEN f.severity = 3 THEN 1 ELSE 0 END) as high,
                    SUM(CASE WHEN f.severity = 4 THEN 1 ELSE 0 END) as critical
                FROM reviews r
                JOIN findings f ON r.id = f.review_id
                WHERE r.status = 'Reviewed'
                GROUP BY r.patchset_id
             ) f ON p.id = f.patchset_id
             {} 
             GROUP BY p.id
             ORDER BY p.date DESC LIMIT ? OFFSET ?",
            where_clause
        );

        let mut args = Vec::new();
        for p in params {
            args.push(libsql::Value::Text(p));
        }
        args.push(libsql::Value::Integer(limit as i64));
        args.push(libsql::Value::Integer(offset as i64));

        let mut rows = self.conn.query(&sql, args).await?;

        let mut patchsets = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            let subsystems_str: Option<String> = row.get(9).ok();
            let subsystems = if let Some(s) = subsystems_str {
                s.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            } else {
                Vec::new()
            };

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
                subsystems,
                findings_low: row.get(10).ok(),
                findings_medium: row.get(11).ok(),
                findings_high: row.get(12).ok(),
                findings_critical: row.get(13).ok(),
                baseline_id: row.get(14).ok(),
                failed_reason: row.get(15).ok(),
                target_review_count: row.get(16).ok(),
                skip_filters: row.get(17).ok(),
                only_filters: row.get(18).ok(),
                model_name: None,
                prompts_git_hash: None,
                baseline_logs: None,
                provider: None,
            });
        }
        Ok(patchsets)
    }

    pub async fn get_messages(
        &self,
        limit: usize,
        offset: usize,
        query: Option<String>,
        mailing_list: Option<String>,
    ) -> Result<Vec<MessageRow>> {
        let (where_clause, params) = self.build_search(query, mailing_list, "message");
        let sql = format!(
            "SELECT id, message_id, thread_id, in_reply_to, author, subject, date, body, to_recipients, cc_recipients, git_blob_hash, mailing_list FROM messages {} ORDER BY date DESC LIMIT ? OFFSET ?",
            where_clause
        );

        let mut args = Vec::new();
        for p in params {
            args.push(libsql::Value::Text(p));
        }
        args.push(libsql::Value::Integer(limit as i64));
        args.push(libsql::Value::Integer(offset as i64));

        let mut rows = self.conn.query(&sql, args).await?;
        let mut messages = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            messages.push(MessageRow {
                id: row.get(0)?,
                message_id: row.get(1)?,
                thread_id: row.get(2).ok(),
                in_reply_to: row.get(3).ok(),
                author: row.get(4).ok(),
                subject: row.get(5).ok(),
                date: row.get(6).ok(),
                body: row.get(7).ok(),
                to: row.get(8).ok(),
                cc: row.get(9).ok(),
                git_blob_hash: row.get(10).ok(),
                mailing_list: row.get(11).ok(),
                diff: None,
                thread: None,
            });
        }
        Ok(messages)
    }

    pub async fn count_patchsets(
        &self,
        query: Option<String>,
        mailing_list: Option<String>,
    ) -> Result<usize> {
        let (where_clause, params) = self.build_search(query, mailing_list, "patchset");
        // We must alias patchsets as p because build_search uses p.id for filters
        let sql = format!("SELECT COUNT(*) FROM patchsets p {}", where_clause);

        let mut args = Vec::new();
        for p in params {
            args.push(libsql::Value::Text(p));
        }

        let mut rows = self.conn.query(&sql, args).await?;
        if let Ok(Some(row)) = rows.next().await {
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        } else {
            Ok(0)
        }
    }

    pub async fn count_pending_patches(&self) -> Result<usize> {
        let mut rows = self.conn.query(
            "SELECT COUNT(p.id) FROM patches p JOIN patchsets ps ON p.patchset_id = ps.id 
             WHERE ps.status IN ('Pending', 'In Review') AND p.status = 'Pending' 
             AND p.id NOT IN (SELECT patch_id FROM reviews WHERE status IN ('Pending', 'In Review', 'Applying') AND patch_id IS NOT NULL)",
            ()
        ).await?;
        if let Ok(Some(row)) = rows.next().await {
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        } else {
            Ok(0)
        }
    }

    pub async fn count_reviewing_patches(&self) -> Result<usize> {
        let mut rows = self.conn.query(
            "SELECT COUNT(DISTINCT patch_id) FROM reviews WHERE status IN ('Pending', 'In Review', 'Applying') AND patch_id IS NOT NULL",
            ()
        ).await?;
        if let Ok(Some(row)) = rows.next().await {
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        } else {
            Ok(0)
        }
    }

    pub async fn count_messages(
        &self,
        query: Option<String>,
        mailing_list: Option<String>,
    ) -> Result<usize> {
        let (where_clause, params) = self.build_search(query, mailing_list, "message");
        let sql = format!("SELECT COUNT(*) FROM messages {}", where_clause);

        let mut args = Vec::new();
        for p in params {
            args.push(libsql::Value::Text(p));
        }

        let mut rows = self.conn.query(&sql, args).await?;
        if let Ok(Some(row)) = rows.next().await {
            let count: i64 = row.get(0)?;
            Ok(count as usize)
        } else {
            Ok(0)
        }
    }

    pub async fn get_patchset_details(
        &self,
        id: i64,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT p.id, p.subject, p.status, p.to_recipients, p.cc_recipients,
                    p.author, p.date, p.cover_letter_message_id, p.thread_id,
                    p.total_parts, p.received_parts, p.failed_reason,
                    p.model_name, p.prompts_git_hash, p.baseline_logs, p.baseline_id, p.provider
                FROM patchsets p
                WHERE p.id = ?",
                libsql::params![id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let pid: i64 = row.get(0)?;
            let subject: Option<String> = row.get(1).ok();
            let status: Option<String> = row.get(2).ok();
            let to: Option<String> = row.get(3).ok();
            let cc: Option<String> = row.get(4).ok();
            let author: Option<String> = row.get(5).ok();
            let date: Option<i64> = row.get(6).ok();
            let mid: Option<String> = row.get(7).ok();
            let thread_id: Option<i64> = row.get(8).ok();
            let total_parts: Option<u32> = row.get(9).ok();
            let received_parts: Option<u32> = row.get(10).ok();
            let failed_reason: Option<String> = row.get(11).ok();
            let model_name: Option<String> = row.get(12).ok();
            let prompts_git_hash: Option<String> = row.get(13).ok();
            let baseline_logs: Option<String> = row.get(14).ok();
            let baseline_id: Option<i64> = row.get(15).ok();
            let provider: Option<String> = row.get(16).ok();
            // Fetch baseline details if needed
            let baseline = if let Some(bid) = baseline_id {
                let mut browse = self
                    .conn
                    .query(
                        "SELECT repo_url, branch, last_known_commit FROM baselines WHERE id = ?",
                        libsql::params![bid],
                    )
                    .await?;
                if let Ok(Some(brow)) = browse.next().await {
                    Some(serde_json::json!({
                       "repo_url": brow.get::<Option<String>>(0).ok(),
                       "branch": brow.get::<Option<String>>(1).ok(),
                       "commit": brow.get::<Option<String>>(2).ok(),
                    }))
                } else {
                    None
                }
            } else {
                None
            };

            // Calculate pagination
            let limit_val = limit.unwrap_or(50);
            let page_val = page.unwrap_or(1);
            let offset_val = limit_val * (page_val.saturating_sub(1));

            // Fetch subsystems
            let mut subsystems = Vec::new();
            let mut sub_rows = self
                .conn
                .query(
                    "SELECT s.name FROM subsystems s
                 JOIN patchsets_subsystems ps ON s.id = ps.subsystem_id
                 WHERE ps.patchset_id = ?",
                    libsql::params![pid],
                )
                .await?;
            while let Ok(Some(row)) = sub_rows.next().await {
                subsystems.push(row.get::<String>(0)?);
            }

            // Count total patches
            let mut total_patches = 0;
            let mut count_rows = self
                .conn
                .query(
                    "SELECT COUNT(*) FROM patches WHERE patchset_id = ?",
                    libsql::params![pid],
                )
                .await?;
            if let Ok(Some(row)) = count_rows.next().await {
                total_patches = row.get::<i64>(0)?;
            }

            // Fetch patches with subject and msg_db_id
            let mut patches = Vec::new();
            let mut patch_ids = Vec::new();
            let mut patch_rows = self
                .conn
                .query(
                    "SELECT p.id, p.message_id, p.part_index, m.id, m.subject, p.status, p.apply_error, 
                            eo.status as email_status, eo.to_addresses, eo.cc_addresses
                 FROM patches p
                 LEFT JOIN messages m ON p.message_id = m.message_id
                 LEFT JOIN email_outbox eo ON eo.patch_id = p.id
                 WHERE p.patchset_id = ? 
                 ORDER BY p.part_index ASC
                 LIMIT ? OFFSET ?",
                    libsql::params![pid, limit_val, offset_val],
                )
                .await?;
            #[allow(clippy::similar_names)]
            while let Ok(Some(p)) = patch_rows.next().await {
                let p_id: i64 = p.get(0)?;
                patch_ids.push(p_id);
                patches.push(serde_json::json!({
                    "id": p_id,
                    "message_id": p.get::<String>(1)?,
                    "part_index": p.get::<Option<i64>>(2).ok(),
                    "msg_db_id": p.get::<Option<i64>>(3).ok(),
                    "subject": p.get::<Option<String>>(4).ok(),
                    "status": p.get::<Option<String>>(5).ok(),
                    "apply_error": p.get::<Option<String>>(6).ok(),
                    "email_status": p.get::<Option<String>>(7).ok(),
                    "email_to": p.get::<Option<String>>(8).ok(),
                    "email_cc": p.get::<Option<String>>(9).ok(),
                }));
            }

            // Fetch reviews
            let mut reviews = Vec::new();
            let mut in_clause = patch_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            if in_clause.is_empty() {
                in_clause = "-1".to_string(); // Fallback so SQL doesn't error
            }
            let query_str = format!(
                "SELECT r.summary, r.created_at, ai.input_context, ai.output_raw, 
                        r.result_description, r.status, r.inline_review, r.logs, ai.tokens_in, ai.tokens_out, r.patch_id, r.id, ai.tokens_cached
                 FROM reviews r
                 LEFT JOIN ai_interactions ai ON r.interaction_id = ai.id
                 WHERE r.patchset_id = ? AND (r.patch_id IS NULL OR r.patch_id IN ({}))
                 ORDER BY r.created_at ASC", in_clause);

            let mut params = vec![libsql::Value::Integer(pid)];
            for &pid_val in &patch_ids {
                params.push(libsql::Value::Integer(pid_val));
            }

            let mut rev_rows = self.conn.query(&query_str, params).await?;

            while let Ok(Some(r)) = rev_rows.next().await {
                reviews.push(serde_json::json!({
                    "summary": r.get::<Option<String>>(0).ok(),
                    "created_at": r.get::<Option<i64>>(1).ok(),
                    "output": r.get::<Option<String>>(3).ok(),
                    "result": r.get::<Option<String>>(4).ok(),
                    "status": r.get::<Option<String>>(5).ok(),
                    "inline_review": r.get::<Option<String>>(6).ok(),
                    "logs": r.get::<Option<String>>(7).ok(),
                    "tokens_in": r.get::<Option<u32>>(8).ok(),
                    "tokens_out": r.get::<Option<u32>>(9).ok(),
                    "patch_id": r.get::<Option<i64>>(10).ok(),
                    "id": r.get::<i64>(11).ok(),
                    "tokens_cached": r.get::<Option<u32>>(12).ok(),
                    "model": model_name.clone(),
                    "provider": provider.clone(),
                    "prompts_hash": prompts_git_hash.clone(),
                    "baseline": baseline.clone()
                }));
            }

            // Fetch thread messages
            let mut messages = Vec::new();
            if let Some(tid) = thread_id {
                let mut msg_rows = self.conn.query(
                    "SELECT id, message_id, author, date, subject, in_reply_to FROM messages WHERE thread_id = ? AND subject != '(placeholder)' ORDER BY date ASC",
                    libsql::params![tid]
                ).await?;
                while let Ok(Some(m)) = msg_rows.next().await {
                    messages.push(serde_json::json!({
                        "id": m.get::<i64>(0)?,
                        "message_id": m.get::<String>(1)?,
                        "author": m.get::<Option<String>>(2).ok(),
                        "date": m.get::<Option<i64>>(3).ok(),
                        "subject": m.get::<Option<String>>(4).ok(),
                        "in_reply_to": m.get::<Option<String>>(5).ok(),
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
                "failed_reason": failed_reason,
                "to": to,
                "cc": cc,
                "total_parts": total_parts,
                "total_patches_in_db": total_patches,
                "page": page_val,
                "limit": limit_val,
                "received_parts": received_parts,
                "reviews": reviews,
                "patches": patches,
                "thread": messages,
                "subsystems": subsystems,
                "model_name": model_name,
                "prompts_git_hash": prompts_git_hash,
                "baseline_logs": baseline_logs,
                "baseline": baseline,
                "provider": provider
            })))
        } else {
            Ok(None)
        }
    }

    pub async fn get_patchset_summary(
        &self,
        id: i64,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT p.id, p.subject, p.status, p.to_recipients, p.cc_recipients,
                    p.author, p.date, p.cover_letter_message_id, p.thread_id,
                    p.total_parts, p.received_parts, p.failed_reason,
                    p.model_name, p.prompts_git_hash, p.baseline_logs, p.baseline_id, p.provider
                FROM patchsets p
                WHERE p.id = ?",
                libsql::params![id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let pid: i64 = row.get(0)?;
            let subject: Option<String> = row.get(1).ok();
            let status: Option<String> = row.get(2).ok();
            let to: Option<String> = row.get(3).ok();
            let cc: Option<String> = row.get(4).ok();
            let author: Option<String> = row.get(5).ok();
            let date: Option<i64> = row.get(6).ok();
            let mid: Option<String> = row.get(7).ok();
            let thread_id: Option<i64> = row.get(8).ok();
            let total_parts: Option<u32> = row.get(9).ok();
            let received_parts: Option<u32> = row.get(10).ok();
            let failed_reason: Option<String> = row.get(11).ok();
            let model_name: Option<String> = row.get(12).ok();
            let prompts_git_hash: Option<String> = row.get(13).ok();
            let baseline_logs: Option<String> = row.get(14).ok();
            let baseline_id: Option<i64> = row.get(15).ok();
            let provider: Option<String> = row.get(16).ok();
            let baseline = if let Some(bid) = baseline_id {
                let mut browse = self
                    .conn
                    .query(
                        "SELECT repo_url, branch, last_known_commit FROM baselines WHERE id = ?",
                        libsql::params![bid],
                    )
                    .await?;
                if let Ok(Some(brow)) = browse.next().await {
                    Some(serde_json::json!({
                       "repo_url": brow.get::<Option<String>>(0).ok(),
                       "branch": brow.get::<Option<String>>(1).ok(),
                       "commit": brow.get::<Option<String>>(2).ok(),
                    }))
                } else {
                    None
                }
            } else {
                None
            };

            let limit_val = limit.unwrap_or(50);
            let page_val = page.unwrap_or(1);
            let offset_val = limit_val * (page_val.saturating_sub(1));

            let mut subsystems = Vec::new();
            let mut sub_rows = self
                .conn
                .query(
                    "SELECT s.name FROM subsystems s
                 JOIN patchsets_subsystems ps ON s.id = ps.subsystem_id
                 WHERE ps.patchset_id = ?",
                    libsql::params![pid],
                )
                .await?;
            while let Ok(Some(row)) = sub_rows.next().await {
                subsystems.push(row.get::<String>(0)?);
            }

            let mut total_patches = 0;
            let mut count_rows = self
                .conn
                .query(
                    "SELECT COUNT(*) FROM patches WHERE patchset_id = ?",
                    libsql::params![pid],
                )
                .await?;
            if let Ok(Some(row)) = count_rows.next().await {
                total_patches = row.get::<i64>(0)?;
            }

            let mut patches = Vec::new();
            let mut patch_ids = Vec::new();
            let mut patch_rows = self
                .conn
                .query(
                    "SELECT p.id, p.message_id, p.part_index, m.id, m.subject, p.status, p.apply_error, 
                            eo.status as email_status, eo.to_addresses, eo.cc_addresses
                 FROM patches p
                 LEFT JOIN messages m ON p.message_id = m.message_id
                 LEFT JOIN email_outbox eo ON eo.patch_id = p.id
                 WHERE p.patchset_id = ? 
                 ORDER BY p.part_index ASC
                 LIMIT ? OFFSET ?",
                    libsql::params![pid, limit_val, offset_val],
                )
                .await?;

            #[allow(clippy::similar_names)]
            while let Ok(Some(p)) = patch_rows.next().await {
                let p_id: i64 = p.get(0)?;
                patch_ids.push(p_id);
                patches.push(serde_json::json!({
                    "id": p_id,
                    "message_id": p.get::<String>(1)?,
                    "part_index": p.get::<Option<i64>>(2).ok(),
                    "msg_db_id": p.get::<Option<i64>>(3).ok(),
                    "subject": p.get::<Option<String>>(4).ok(),
                    "status": p.get::<Option<String>>(5).ok(),
                    "apply_error": p.get::<Option<String>>(6).ok(),
                    "email_status": p.get::<Option<String>>(7).ok(),
                    "email_to": p.get::<Option<String>>(8).ok(),
                    "email_cc": p.get::<Option<String>>(9).ok(),
                }));
            }

            let mut reviews = Vec::new();
            let mut in_clause = patch_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            if in_clause.is_empty() {
                in_clause = "-1".to_string();
            }
            let query_str = format!(
                "SELECT r.summary, r.created_at, ai.output_raw, 
                        r.result_description, r.status, r.inline_review, ai.tokens_in, ai.tokens_out, r.patch_id, r.id, ai.tokens_cached
                 FROM reviews r
                 LEFT JOIN ai_interactions ai ON r.interaction_id = ai.id
                 WHERE r.patchset_id = ? AND (r.patch_id IS NULL OR r.patch_id IN ({}))
                 ORDER BY r.created_at ASC", in_clause);

            let mut params = vec![libsql::Value::Integer(pid)];
            for &pid_val in &patch_ids {
                params.push(libsql::Value::Integer(pid_val));
            }

            let mut rev_rows = self.conn.query(&query_str, params).await?;

            while let Ok(Some(r)) = rev_rows.next().await {
                reviews.push(serde_json::json!({
                    "summary": r.get::<Option<String>>(0).ok(),
                    "created_at": r.get::<Option<i64>>(1).ok(),
                    "output": r.get::<Option<String>>(2).ok(),
                    "result": r.get::<Option<String>>(3).ok(),
                    "status": r.get::<Option<String>>(4).ok(),
                    "inline_review": r.get::<Option<String>>(5).ok(),
                    "tokens_in": r.get::<Option<u32>>(6).ok(),
                    "tokens_out": r.get::<Option<u32>>(7).ok(),
                    "patch_id": r.get::<Option<i64>>(8).ok(),
                    "id": r.get::<i64>(9).ok(),
                    "tokens_cached": r.get::<Option<u32>>(10).ok(),
                    "model": model_name.clone(),
                    "provider": provider.clone(),
                    "prompts_hash": prompts_git_hash.clone(),
                    "baseline": baseline.clone()
                }));
            }

            let mut messages = Vec::new();
            if let Some(tid) = thread_id {
                let mut msg_rows = self.conn.query(
                    "SELECT id, message_id, author, date, subject, in_reply_to FROM messages WHERE thread_id = ? AND subject != '(placeholder)' ORDER BY date ASC",
                    libsql::params![tid]
                ).await?;
                while let Ok(Some(m)) = msg_rows.next().await {
                    messages.push(serde_json::json!({
                        "id": m.get::<i64>(0)?,
                        "message_id": m.get::<String>(1)?,
                        "author": m.get::<Option<String>>(2).ok(),
                        "date": m.get::<Option<i64>>(3).ok(),
                        "subject": m.get::<Option<String>>(4).ok(),
                        "in_reply_to": m.get::<Option<String>>(5).ok(),
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
                "failed_reason": failed_reason,
                "to": to,
                "cc": cc,
                "total_parts": total_parts,
                "total_patches_in_db": total_patches,
                "page": page_val,
                "limit": limit_val,
                "received_parts": received_parts,
                "reviews": reviews,
                "patches": patches,
                "thread": messages,
                "subsystems": subsystems,
                "model_name": model_name,
                "prompts_git_hash": prompts_git_hash,
                "baseline_logs": baseline_logs,
                "baseline": baseline,
                "provider": provider
            })))
        } else {
            Ok(None)
        }
    }

    pub async fn get_patchset_summary_by_msgid(
        &self,
        msg_id: &str,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM patchsets WHERE cover_letter_message_id = ?",
                libsql::params![msg_id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            return self.get_patchset_summary(id, page, limit).await;
        }

        let mut rows = self
            .conn
            .query(
                "SELECT patchset_id FROM patches WHERE message_id = ?",
                libsql::params![msg_id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            return self.get_patchset_summary(id, page, limit).await;
        }

        Ok(None)
    }

    pub async fn get_review_details(&self, id: i64) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT r.id, r.model, r.summary, r.created_at, ai.input_context, ai.output_raw, 
                        b.repo_url, b.branch, b.last_known_commit,
                        r.provider, r.prompts_git_hash, r.result_description,
                        r.status, r.inline_review, r.logs, ai.tokens_in, ai.tokens_out, r.patch_id, ai.tokens_cached
             FROM reviews r
             LEFT JOIN ai_interactions ai ON r.interaction_id = ai.id
             LEFT JOIN baselines b ON r.baseline_id = b.id
             WHERE r.id = ?",
                libsql::params![id],
            )
            .await?;

        if let Ok(Some(r)) = rows.next().await {
            Ok(Some(serde_json::json!({
                "id": r.get::<i64>(0)?,
                "model": r.get::<Option<String>>(1).ok(),
                "summary": r.get::<Option<String>>(2).ok(),
                "created_at": r.get::<Option<i64>>(3).ok(),
                "input": r.get::<Option<String>>(4).ok(),
                "output": r.get::<Option<String>>(5).ok(),
                "baseline": {
                    "repo_url": r.get::<Option<String>>(6).ok(),
                    "branch": r.get::<Option<String>>(7).ok(),
                    "commit": r.get::<Option<String>>(8).ok(),
                },
                "provider": r.get::<Option<String>>(9).ok(),
                "prompts_hash": r.get::<Option<String>>(10).ok(),
                "result": r.get::<Option<String>>(11).ok(),
                "status": r.get::<Option<String>>(12).ok(),
                "inline_review": r.get::<Option<String>>(13).ok(),
                "logs": r.get::<Option<String>>(14).ok(),
                "tokens_in": r.get::<Option<u32>>(15).ok(),
                "tokens_out": r.get::<Option<u32>>(16).ok(),
                "patch_id": r.get::<Option<i64>>(17).ok(),
                "tokens_cached": r.get::<Option<u32>>(18).ok(),
            })))
        } else {
            Ok(None)
        }
    }

    pub async fn get_latest_review_for_patchset(
        &self,
        patchset_id: i64,
    ) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT id FROM reviews WHERE patchset_id = ? ORDER BY created_at DESC LIMIT 1",
                libsql::params![patchset_id],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            self.get_review_details(id).await
        } else {
            Ok(None)
        }
    }

    pub async fn get_patch_diffs(
        &self,
        patchset_id: i64,
    ) -> Result<Vec<(i64, i64, String, String, String, i64, String)>> {
        let mut rows = self
            .conn
            .query(
                "SELECT p.id, p.part_index, p.diff, m.subject, m.author, m.date, m.message_id 
             FROM patches p 
             JOIN messages m ON p.message_id = m.message_id 
             WHERE p.patchset_id = ? 
             ORDER BY p.part_index ASC",
                libsql::params![patchset_id],
            )
            .await?;

        let mut diffs = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            let index: i64 = row.get(1).unwrap_or(0);
            let diff: String = row.get(2)?;
            let subject: String = row.get(3).unwrap_or_default();
            let author: String = row.get(4).unwrap_or_default();
            let date: i64 = row.get(5).unwrap_or(0);
            let message_id: String = row.get(6)?;
            diffs.push((id, index, diff, subject, author, date, message_id));
        }
        Ok(diffs)
    }

    pub async fn get_pending_patchsets(&self, limit: usize) -> Result<Vec<PatchsetRow>> {
        let mut rows = self.conn.query(
            "SELECT id, subject, status, thread_id, author, date, cover_letter_message_id, total_parts, received_parts, baseline_id, failed_reason, target_review_count, skip_filters, only_filters
             FROM patchsets WHERE status = 'Pending' ORDER BY date ASC LIMIT ?",
            libsql::params![limit as i64],
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
                subsystems: Vec::new(),
                findings_low: None,
                findings_medium: None,
                findings_high: None,
                findings_critical: None,
                baseline_id: row.get(9).ok(),
                failed_reason: row.get(10).ok(),
                target_review_count: row.get(11).ok(),
                skip_filters: row.get(12).ok(),
                only_filters: row.get(13).ok(),
                model_name: None,
                prompts_git_hash: None,
                baseline_logs: None,
                provider: None,
            });
        }
        Ok(patchsets)
    }

    pub async fn update_patchset_status(&self, id: i64, status: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchsets SET status = ? WHERE id = ?",
                libsql::params![status, id],
            )
            .await?;
        Ok(())
    }

    pub async fn update_patch_status(&self, patch_id: i64, status: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patches SET status = ? WHERE id = ?",
                libsql::params![status, patch_id],
            )
            .await?;
        Ok(())
    }

    pub async fn restart_failed_reviews(&self) -> Result<u64> {
        let count = self.conn.execute(
            "UPDATE patchsets SET status = 'Pending', failed_reason = NULL WHERE status = 'Failed'",
            libsql::params![],
        ).await?;
        Ok(count)
    }

    pub async fn rerun_patchset(&self, id: i64) -> Result<()> {
        // 1. Reset patchset status to Pending
        self.conn
            .execute(
                "UPDATE patchsets SET status = 'Pending' WHERE id = ?",
                libsql::params![id],
            )
            .await?;

        // 2. Increment target_review_count
        self.conn
            .execute(
                "UPDATE patchsets SET target_review_count = COALESCE(target_review_count, 1) + 1 WHERE id = ?",
                libsql::params![id],
            )
            .await?;

        Ok(())
    }

    pub async fn rerun_patch(&self, patchset_id: i64, _patch_id: i64) -> Result<()> {
        // NOTE: Currently we only support re-running the entire patchset to trigger more reviews.
        // Even if the user requested a specific patch, we increment the set's target count
        // to allow the reviewer service to proceed.
        self.rerun_patchset(patchset_id).await
    }

    pub async fn create_fetching_patchset(
        &self,
        article_id: &str,
        subject: &str,
        skip_filters: Option<&Vec<String>>,
        only_filters: Option<&Vec<String>>,
    ) -> Result<i64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;

        let root_msg_id = if article_id.contains('@') {
            article_id.to_string()
        } else {
            format!("{}@sashiko.local", article_id)
        };

        let clid_candidates = vec![article_id.to_string(), root_msg_id.clone()];

        let skip_filters_json = skip_filters.map(|f| serde_json::to_string(f).unwrap_or_default());
        let only_filters_json = only_filters.map(|f| serde_json::to_string(f).unwrap_or_default());

        // 1. Check if it already exists
        for clid in clid_candidates {
            let mut rows = self
                .conn
                .query(
                    "SELECT id, status FROM patchsets WHERE cover_letter_message_id = ?",
                    libsql::params![clid.clone()],
                )
                .await?;

            if let Ok(Some(row)) = rows.next().await {
                let id: i64 = row.get(0)?;
                let status: String = row.get(1).unwrap_or_default();

                // Only reset to Fetching if it failed or is currently fetching.
                // We don't want to reset if it is already Incomplete, Pending, or Reviewed.
                if status == "Failed" || status == "Fetching" {
                    self.conn.execute(
                        "UPDATE patchsets SET status = 'Fetching', failed_reason = NULL, skip_filters = ?, only_filters = ? WHERE id = ?",
                        libsql::params![skip_filters_json.clone(), only_filters_json.clone(), id]
                    ).await?;
                }
                return Ok(id);
            }
        }

        // 2. Ensure a placeholder thread and message exist to satisfy Foreign Key constraints
        let thread_id = self.ensure_thread_for_message(&root_msg_id, now).await?;

        // 3. Create the fetching patchset
        let mut rows = self.conn
            .query(
                "INSERT INTO patchsets (thread_id, cover_letter_message_id, subject, status, date, skip_filters, only_filters) 
                     VALUES (?, ?, ?, 'Fetching', ?, ?, ?) RETURNING id",
                libsql::params![thread_id, root_msg_id, subject, now, skip_filters_json, only_filters_json],
            )
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to get patchset ID"))
        }
    }
    pub async fn update_patchset_error(&self, article_id: &str, error: &str) -> Result<()> {
        let root_msg_id = if article_id.contains('@') {
            article_id.to_string()
        } else {
            format!("{}@sashiko.local", article_id)
        };
        self.conn
            .execute(
                "UPDATE patchsets SET status = 'Failed', failed_reason = ? WHERE cover_letter_message_id = ?",
                libsql::params![error, root_msg_id],
            )
            .await?;
        Ok(())
    }

    pub async fn update_patchset_baseline_info(
        &self,
        id: i64,
        baseline_id: Option<i64>,
        model_name: Option<&str>,
        prompts_hash: Option<&str>,
        logs: Option<&str>,
        provider: Option<&str>,
    ) -> Result<()> {
        self.conn
            .execute(
                "UPDATE patchsets SET baseline_id = ?, model_name = ?, prompts_git_hash = ?, baseline_logs = ?, provider = ? WHERE id = ?",
                libsql::params![baseline_id, model_name, prompts_hash, logs, provider, id],
            )
            .await?;
        Ok(())
    }

    pub async fn update_patch_application_status(
        &self,
        patchset_id: i64,
        part_index: i64,
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE patches SET status = ?, apply_error = ? WHERE patchset_id = ? AND part_index = ?",
            libsql::params![status, error, patchset_id, part_index],
        ).await?;
        Ok(())
    }

    pub async fn reset_reviewing_status(&self) -> Result<u64> {
        let status_pending = ReviewStatus::Pending.as_str();
        // Reset Patchsets
        let count_ps = self
            .conn
            .execute(
                format!(
                    "UPDATE patchsets SET status = '{}' WHERE status IN ('In Review', 'Reviewing')",
                    status_pending
                )
                .as_str(),
                (),
            )
            .await?;

        // Reset Reviews
        let count_rev = self
            .conn
            .execute(
                format!(
                    "UPDATE reviews SET status = '{}' WHERE status = 'In Review'",
                    status_pending
                )
                .as_str(),
                (),
            )
            .await?;

        Ok(count_ps + count_rev)
    }

    pub async fn get_patchset_counts_by_status(
        &self,
    ) -> Result<std::collections::HashMap<String, usize>> {
        let mut rows = self
            .conn
            .query("SELECT status, COUNT(*) FROM patchsets GROUP BY status", ())
            .await?;

        let mut counts = std::collections::HashMap::new();
        while let Ok(Some(row)) = rows.next().await {
            let status: Option<String> = row.get(0).ok();
            let count: i64 = row.get(1)?;
            let status_key = status.unwrap_or_else(|| "Unknown".to_string());
            counts.insert(status_key, count as usize);
        }
        Ok(counts)
    }
}

impl Database {
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_email_outbox(
        &self,
        patch_id: i64,
        status: &str,
        to_addresses: &str,
        cc_addresses: &str,
        subject: &str,
        in_reply_to: &str,
        references_hdr: &str,
        body: &str,
    ) -> Result<()> {
        // Prevent duplicate emails for the same patch
        let mut rows = self
            .conn
            .query(
                "SELECT 1 FROM email_outbox WHERE patch_id = ?",
                libsql::params![patch_id],
            )
            .await?;

        if let Ok(Some(_)) = rows.next().await {
            tracing::info!(
                "Email outbox entry already exists for patch_id {}, skipping to prevent duplicates.",
                patch_id
            );
            return Ok(());
        }

        let created_at = chrono::Utc::now().timestamp();
        self.conn.execute(
            "INSERT INTO email_outbox (patch_id, status, to_addresses, cc_addresses, subject, in_reply_to, references_hdr, body, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            libsql::params![
                patch_id,
                status,
                to_addresses,
                cc_addresses,
                subject,
                in_reply_to,
                references_hdr,
                body,
                created_at,
            ],
        ).await?;
        Ok(())
    }

    pub async fn lock_pending_email(&self) -> Result<Option<EmailOutboxRow>> {
        let now = chrono::Utc::now().timestamp();
        let mut rows = self.conn.query(
            "UPDATE email_outbox 
             SET status = 'Sending', locked_at = ? 
             WHERE id = (SELECT id FROM email_outbox WHERE status = 'Pending' LIMIT 1)
             RETURNING id, patch_id, status, to_addresses, cc_addresses, subject, in_reply_to, references_hdr, body, locked_at, error_log, created_at",
            libsql::params![now]
        ).await?;

        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            let patch_id: i64 = row.get(1)?;
            let status: String = row.get(2)?;
            let to_addresses: String = row.get(3)?;
            let cc_addresses: String = row.get(4)?;
            let subject: String = row.get(5)?;
            let in_reply_to: String = row.get(6)?;
            let references_hdr: String = row.get(7)?;
            let body: String = row.get(8)?;
            let locked_at: Option<i64> = row.get(9).ok();
            let error_log: Option<String> = row.get(10).ok();
            let created_at: i64 = row.get(11)?;

            Ok(Some(EmailOutboxRow {
                id,
                patch_id,
                status,
                to_addresses,
                cc_addresses,
                subject,
                in_reply_to,
                references_hdr,
                body,
                locked_at,
                error_log,
                created_at,
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn mark_email_sent(&self, id: i64) -> Result<()> {
        self.conn
            .execute(
                "UPDATE email_outbox SET status = 'Sent', locked_at = NULL WHERE id = ?",
                libsql::params![id],
            )
            .await?;
        Ok(())
    }

    pub async fn mark_email_failed(&self, id: i64, error_log: &str) -> Result<()> {
        self.conn.execute("UPDATE email_outbox SET status = 'Failed', error_log = ?, locked_at = NULL WHERE id = ?", libsql::params![error_log.to_string(), id]).await?;
        Ok(())
    }

    pub async fn sweep_ghost_emails(&self) -> Result<u64> {
        let ten_mins_ago = chrono::Utc::now().timestamp() - 600;
        let count = self.conn.execute(
            "UPDATE email_outbox SET status = 'Pending', locked_at = NULL WHERE status = 'Sending' AND locked_at < ?",
            libsql::params![ten_mins_ago]
        ).await?;
        Ok(count)
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
        db.create_message(
            "msg1", thread_id, None, "Author A", "Patch 1", 1000, "", "", "", None, None,
        )
        .await
        .unwrap();
        let ps1 = db
            .create_patchset(
                thread_id,
                None,
                "msg1",
                "Patch 1",
                "Author A",
                1000,
                2,
                1,
                "to",
                "cc",
                Some(1),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(ps1.is_some());

        // 2. Add Cover Letter (index 0)
        // Should return same ID and update subject to "Cover Letter"
        db.create_message(
            "root",
            thread_id,
            None,
            "Author A",
            "Cover Letter",
            1005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps1_update = db
            .create_patchset(
                thread_id,
                Some("root"),
                "root",
                "Cover Letter",
                "Author A",
                1005,
                2,
                1,
                "to",
                "cc",
                Some(1),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(ps1, ps1_update);

        let list = db.get_patchsets(1, 0, None, None).await.unwrap();
        assert_eq!(list[0].subject.as_deref(), Some("Cover Letter"));

        // 3. Add Patch 2 (index 2)
        // Should NOT update subject (index 2 > index 0)
        db.create_message(
            "msg2", thread_id, None, "Author A", "Patch 2", 1006, "", "", "", None, None,
        )
        .await
        .unwrap();
        db.create_patchset(
            thread_id,
            None,
            "msg2",
            "Patch 2",
            "Author A",
            1006,
            2,
            1,
            "to",
            "cc",
            Some(1),
            2,
            None,
            true,
            None,
            None,
        )
        .await
        .unwrap();

        let list = db.get_patchsets(1, 0, None, None).await.unwrap();
        assert_eq!(list[0].subject.as_deref(), Some("Cover Letter"));

        // 4. Create NEW patchset in same thread (Author B, Time 1000 - same time but diff author)
        // With relaxed logic, this SHOULD merge if total_parts match (assuming same series).
        let ps3 = db
            .create_patchset(
                thread_id,
                None,
                "msg_other",
                "Other Author",
                "Author B",
                1000,
                2,
                1,
                "to",
                "cc",
                Some(1),
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(ps3, ps1, "Different author in same series should merge");

        // 5. Create NEW patchset v2 (Author A, Time 1002 - close time, but v2)
        // Under new logic "Implicit matches Explicit", this SHOULD merge with ps1 (Implicit)
        // because time/author/total match.
        let ps_v2 = db
            .create_patchset(
                thread_id,
                None,
                "msg_v2",
                "[PATCH v2] Patchset 1",
                "Author B",
                1002,
                2,
                1,
                "to",
                "cc",
                Some(2),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap();
        assert_ne!(
            ps1, ps_v2,
            "Implicit v1 should NOT merge with v2 even if time/author match"
        );

        // 7. Test Merging: Create disjoint patchsets then bridge them
        let t_merge = db
            .create_thread("root_merge", "Merge Test", 10000)
            .await
            .unwrap();

        // PS A (Time 10000)
        db.create_message(
            "m1", t_merge, None, "Merger", "P1", 10000, "", "", "", None, None,
        )
        .await
        .unwrap();
        let psa = db
            .create_patchset(
                t_merge,
                None,
                "m1",
                "Series",
                "Merger",
                10000,
                3,
                1,
                "",
                "",
                Some(1),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // PS B (Time 200000) - 190000s diff > 86400s limit -> New PS
        db.create_message(
            "m2", t_merge, None, "Merger", "P3", 200000, "", "", "", None, None,
        )
        .await
        .unwrap();
        let psb = db
            .create_patchset(
                t_merge,
                None,
                "m2",
                "Series",
                "Merger",
                200000,
                3,
                1,
                "",
                "",
                Some(1),
                3,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_ne!(psa, psb);

        // PS C (Time 100000) - 90000s diff from A (>86400), 100000s diff from B (>86400)
        // Wait, if C is > 86400 from both, it won't match either!
        // We need C to match BOTH.
        // A=10000. B=200000. Gap=190000.
        // If we want C to bridge, C needs to be within 86400 of A AND within 86400 of B.
        // But 190000 > 86400 * 2 (172800).
        // So it's IMPOSSIBLE to bridge with ONE message if they are that far apart!
        // We need A and B to be < 2 * 86400 apart.
        // Let's set B = 10000 + 100000 = 110000.
        // Diff = 100000. > 86400. So disjoint.
        // C = 10000 + 50000 = 60000.
        // Diff(A, C) = 50000 < 86400. Match A.
        // Diff(B, C) = 110000 - 60000 = 50000 < 86400. Match B.
        // So C bridges A and B.

        db.create_message(
            "m2_fixed", t_merge, None, "Merger", "P3_fixed", 120000, "", "", "", None, None,
        )
        .await
        .unwrap(); // 120000. Diff 110000 > 86400.
        let psb_fixed = db
            .create_patchset(
                t_merge,
                None,
                "m2_fixed",
                "Series",
                "Merger",
                120000,
                3,
                1,
                "",
                "",
                Some(1),
                3,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        assert_ne!(psa, psb_fixed);

        // PS C (Time 65000)
        // Diff(A, C) = 55000 < 86400.
        // Diff(B, C) = 120000 - 65000 = 55000 < 86400.
        db.create_message(
            "m3", t_merge, None, "Merger", "P2", 65000, "", "", "", None, None,
        )
        .await
        .unwrap();
        let psc = db
            .create_patchset(
                t_merge,
                None,
                "m3",
                "Series",
                "Merger",
                65000,
                3,
                1,
                "",
                "",
                Some(1),
                2,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(psc, psa);
    }

    #[tokio::test]
    async fn test_five_patch_series_merging() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_5", "Five Patch Series", 20000)
            .await
            .unwrap();
        let author = "Series Author <author@example.com>";

        // Patches arrive in order: 1/5, 0/5, 2/5, 4/5, 3/5
        let indices = [1, 0, 2, 4, 3];
        let mut patchset_ids = Vec::new();

        for (i, &idx) in indices.iter().enumerate() {
            let msg_id = format!("msg_{}", idx);
            let subject = format!("[PATCH {}/5] Feature part {}", idx, idx);
            let time = 20000 + (i as i64 * 10); // 10s apart

            db.create_message(
                &msg_id, thread_id, None, author, &subject, time, "", "", "", None, None,
            )
            .await
            .unwrap();
            let ps_id = db
                .create_patchset(
                    thread_id,
                    if idx == 0 { Some(&msg_id) } else { None },
                    &msg_id,
                    &subject,
                    author,
                    time,
                    5,
                    1,
                    "to",
                    "cc",
                    None,
                    idx as u32,
                    None,
                    true,
                    None,
                    None,
                )
                .await
                .unwrap()
                .unwrap();

            patchset_ids.push(ps_id);
        }

        // All IDs should be the same
        let first_id = patchset_ids[0];
        for id in patchset_ids {
            assert_eq!(
                id, first_id,
                "All parts of the same series should share the same patchset ID"
            );
        }

        // Verify the final subject is the cover letter (index 0)
        let list = db.get_patchsets(1, 0, None, None).await.unwrap();
        assert_eq!(
            list[0].subject.as_deref(),
            Some("[PATCH 0/5] Feature part 0")
        );
    }

    #[tokio::test]
    async fn test_patchset_status_transition() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_status", "Status Test", 60000)
            .await
            .unwrap();
        let author = "Status Author <status@example.com>";

        // 1. Create patchset with 2 parts. received=0 initially (cover letter doesn't count as received part in DB logic usually, but here we insert it)
        // Wait, create_patchset creates the set. create_patch updates received count.
        // We call create_patchset first.
        let ps_id = db
            .create_patchset(
                thread_id,
                None,
                "msg_status",
                "Status Test",
                author,
                60000,
                2,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Check initial status
        let list = db.get_patchsets(1, 0, None, None).await.unwrap();
        assert_eq!(list[0].status.as_deref(), Some("Incomplete"));

        // 2. Add Patch 1. received=1. Total=2. Status should be Incomplete.
        db.create_message(
            "msg_1", thread_id, None, author, "Part 1", 60005, "", "", "", None, None,
        )
        .await
        .unwrap();
        db.create_patch(ps_id, "msg_1", 1, "diff").await.unwrap();
        let list = db.get_patchsets(1, 0, None, None).await.unwrap();
        assert_eq!(list[0].status.as_deref(), Some("Incomplete"));

        // 3. Add Patch 2. received=2. Total=2. Status should transition to Pending.
        db.create_message(
            "msg_2", thread_id, None, author, "Part 2", 60010, "", "", "", None, None,
        )
        .await
        .unwrap();
        db.create_patch(ps_id, "msg_2", 2, "diff").await.unwrap();
        let list = db.get_patchsets(1, 0, None, None).await.unwrap();
        assert_eq!(list[0].status.as_deref(), Some("Pending"));
    }

    #[tokio::test]
    async fn test_implicit_version_mismatch_should_merge() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_v6", "Version 6 Series", 30000)
            .await
            .unwrap();
        let author = "Author V6 <v6@example.com>";

        // Case: Cover letter has v6, but patches don't say v6 (implicitly v1).
        // If the user forgot to version patches, they should NOT merge with strict version checking.
        // This prevents merging v1 patches into v6 series if timestamps overlap.

        // 1. Cover letter: [PATCH 00/33 v6] -> v6
        db.create_message(
            "msg_00",
            thread_id,
            None,
            author,
            "[PATCH 00/33 v6] Cover",
            30000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_cover = db
            .create_patchset(
                thread_id,
                Some("msg_00"),
                "msg_00",
                "[PATCH 00/33 v6] Cover",
                author,
                30000,
                33,
                1,
                "",
                "",
                Some(6),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. Patch 1: [PATCH 01/33] -> v1 (implicit). Pass None.
        db.create_message(
            "msg_01",
            thread_id,
            None,
            author,
            "[PATCH 01/33] Part 1",
            30005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_p1 = db
            .create_patchset(
                thread_id,
                None,
                "msg_01",
                "[PATCH 01/33] Part 1",
                author,
                30005,
                33,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Relaxed checking: Should merge because same thread
        assert_eq!(
            ps_cover, ps_p1,
            "Should merge explicit v6 cover with implicit v1 patch if in same thread"
        );
    }

    #[tokio::test]
    async fn test_unrelated_singletons_no_merge() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_single", "Singletons", 60000)
            .await
            .unwrap();
        let author = "Author S <s@example.com>";

        // Patch A
        db.create_message(
            "msg_a",
            thread_id,
            None,
            author,
            "[PATCH] Fix A",
            60000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_a = db
            .create_patchset(
                thread_id,
                None,
                "msg_a",
                "[PATCH] Fix A",
                author,
                60000,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Patch B (Close time, same author, implicit version, total=1)
        db.create_message(
            "msg_b",
            thread_id,
            None,
            author,
            "[PATCH] Fix B",
            60005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_b = db
            .create_patchset(
                thread_id,
                None,
                "msg_b",
                "[PATCH] Fix B",
                author,
                60005,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_ne!(
            ps_a, ps_b,
            "Should NOT merge unrelated singletons even if author/time match"
        );
    }

    #[tokio::test]
    async fn test_singleton_cover_patch_merge() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_1of1", "Singleton Series", 60000)
            .await
            .unwrap();
        let author = "Author 1of1 <1@example.com>";

        // Cover: [PATCH 0/1] Subject A
        db.create_message(
            "msg_0",
            thread_id,
            None,
            author,
            "[PATCH 0/1] Subject A",
            60000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_0 = db
            .create_patchset(
                thread_id,
                Some("msg_0"),
                "msg_0",
                "[PATCH 0/1] Subject A",
                author,
                60000,
                1,
                1,
                "",
                "",
                None,
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Patch: [PATCH 1/1] Subject B (Different subject)
        db.create_message(
            "msg_1",
            thread_id,
            None,
            author,
            "[PATCH 1/1] Subject B",
            60005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_1 = db
            .create_patchset(
                thread_id,
                None,
                "msg_1",
                "[PATCH 1/1] Subject B",
                author,
                60005,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            ps_0, ps_1,
            "Should merge 0/1 and 1/1 even if subjects differ"
        );
    }

    #[tokio::test]
    async fn test_version_mismatch_no_merge() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_diff_ver", "Version Mismatch", 40000)
            .await
            .unwrap();
        let author = "Author Diff <diff@example.com>";

        // v5
        db.create_message(
            "msg_v5",
            thread_id,
            None,
            author,
            "[PATCH v5 1/2] Part 1",
            40000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_v5 = db
            .create_patchset(
                thread_id,
                None,
                "msg_v5",
                "[PATCH v5 1/2] Part 1",
                author,
                40000,
                2,
                1,
                "",
                "",
                Some(5),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Add patch to trigger index collision logic
        db.create_patch(ps_v5, "msg_v5", 1, "diff").await.unwrap();

        // v6 (Close time)
        db.create_message(
            "msg_v6",
            thread_id,
            None,
            author,
            "[PATCH v6 1/2] Part 1",
            40010,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_v6 = db
            .create_patchset(
                thread_id,
                None,
                "msg_v6",
                "[PATCH v6 1/2] Part 1",
                author,
                40010,
                2,
                1,
                "",
                "",
                Some(6),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_ne!(
            ps_v5, ps_v6,
            "Should NOT merge different explicit versions (v5 vs v6)"
        );
    }

    #[tokio::test]
    async fn test_v3_series_fragmentation() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_v3", "v3 Series", 50000)
            .await
            .unwrap();
        let author = "Author V3 <v3@example.com>";

        // 1. [PATCH v3 0/2] (Cover)
        db.create_message(
            "v3_0",
            thread_id,
            None,
            author,
            "[PATCH v3 0/2] Cover",
            50000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_0 = db
            .create_patchset(
                thread_id,
                Some("v3_0"),
                "v3_0",
                "[PATCH v3 0/2] Cover",
                author,
                50000,
                2,
                1,
                "",
                "",
                Some(3),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. [PATCH v3 1/2]
        db.create_message(
            "v3_1",
            thread_id,
            None,
            author,
            "[PATCH v3 1/2] Part 1",
            50005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_1 = db
            .create_patchset(
                thread_id,
                None,
                "v3_1",
                "[PATCH v3 1/2] Part 1",
                author,
                50005,
                2,
                1,
                "",
                "",
                Some(3),
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 3. [PATCH v3 2/2]
        db.create_message(
            "v3_2",
            thread_id,
            None,
            author,
            "[PATCH v3 2/2] Part 2",
            50010,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_2 = db
            .create_patchset(
                thread_id,
                None,
                "v3_2",
                "[PATCH v3 2/2] Part 2",
                author,
                50010,
                2,
                1,
                "",
                "",
                Some(3),
                2,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(ps_0, ps_1, "Patch 1 should merge with Cover");
        assert_eq!(ps_0, ps_2, "Patch 2 should merge with Cover");
    }

    #[tokio::test]
    async fn test_merge_with_confusing_version_in_subject() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_confusing", "Confusing Versions", 80000)
            .await
            .unwrap();
        let author = "Confused Author <confused@example.com>";

        // 1. [PATCH v3 00/17] (v3)
        db.create_message(
            "msg_v3_conf_00",
            thread_id,
            None,
            author,
            "[PATCH v3 00/17] Cover",
            80000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps_cover = db
            .create_patchset(
                thread_id,
                Some("msg_v3_conf_00"),
                "msg_v3_conf_00",
                "[PATCH v3 00/17] Cover",
                author,
                80000,
                17,
                1,
                "",
                "",
                Some(3),
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. [PATCH 01/17] Support v2 hardware. Treat as implicit version (None), NOT v2.
        db.create_message(
            "msg_conf_01",
            thread_id,
            None,
            author,
            "[PATCH 01/17] Support v2 hardware",
            80005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        // Here we simulate the parser extracting "2" from "v2" if it's aggressive
        // But `create_patchset` takes the *parsed* version.
        // If we want to simulate the BUG, we must pass what `parse_email` WOULD pass.
        // `parse_email` uses `parse_subject_version`.
        // Let's check what `parse_subject_version` does for this string.
        let subject = "[PATCH v3 01/17] Support v2 hardware";
        let parsed_ver = crate::patch::parse_subject_version(subject);

        let ps_part1 = db
            .create_patchset(
                thread_id,
                None,
                "msg_conf_01",
                subject,
                author,
                80005,
                17,
                1,
                "",
                "",
                parsed_ver, // Pass the result of the potentially buggy parser
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            ps_cover, ps_part1,
            "Should merge because subject implies v3 (and ignores v2 in text)"
        );
    }

    #[tokio::test]
    async fn test_merge_patchsets_with_dependencies() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_deps", "Dependencies Test", 90000)
            .await
            .unwrap();
        let author = "Deps Author <deps@example.com>";

        // 1. Create first patchset part [PATCH 1/2]
        db.create_message(
            "msg_deps_1",
            thread_id,
            None,
            author,
            "[PATCH 1/2] Part 1",
            90000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps1 = db
            .create_patchset(
                thread_id,
                None,
                "msg_deps_1",
                "[PATCH 1/2] Part 1",
                author,
                90000,
                2,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. Add dependencies to ps1 (Review, Tag, Subsystem)
        let review_id = db
            .create_review(ps1, None, "gemini", "test-model", None, None)
            .await
            .unwrap();

        let sub_id = db
            .ensure_subsystem("test_sub", "test@example.com")
            .await
            .unwrap();
        db.add_subsystem_to_patchset(ps1, sub_id).await.unwrap();

        // 3. Create second patchset part [PATCH 2/2] -> Should merge into ps1 (or ps1 into ps2, but we keep oldest ID so ps2 into ps1)
        // ps1 ID should be preserved because it was created first.
        db.create_message(
            "msg_deps_2",
            thread_id,
            None,
            author,
            "[PATCH 2/2] Part 2",
            90005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps2 = db
            .create_patchset(
                thread_id,
                None,
                "msg_deps_2",
                "[PATCH 2/2] Part 2",
                author,
                90005, // Close enough
                2,
                1,
                "",
                "",
                None,
                2,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(ps1, ps2, "Patchsets should have merged");

        // 4. Verify dependencies moved
        // Check review
        let mut rows = db
            .conn
            .query(
                "SELECT patchset_id FROM reviews WHERE id = ?",
                libsql::params![review_id],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let review_ps_id: i64 = row.get(0).unwrap();
        assert_eq!(review_ps_id, ps1);

        // Check subsystem
        let mut rows = db
            .conn
            .query(
                "SELECT count(*) FROM patchsets_subsystems WHERE patchset_id = ?",
                libsql::params![ps1],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let count: i64 = row.get(0).unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn test_create_ai_interaction_with_cached_tokens() {
        let db = setup_db().await;

        // Create interaction
        let params = AiInteractionParams {
            id: "test_id",
            parent_id: None,
            workflow_id: None,
            provider: "test_provider",
            model: "test_model",
            input: "input",
            output: "output",
            tokens_in: 100,
            tokens_out: 50,
            tokens_cached: 25,
        };

        db.create_ai_interaction(params).await.unwrap();

        // Verify via raw query since there is no direct get_ai_interaction method exposed
        // (get_review_details joins it, but requires a review)

        let mut rows = db
            .conn
            .query(
                "SELECT tokens_cached FROM ai_interactions WHERE id = 'test_id'",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let cached: u32 = row.get(0).unwrap();

        assert_eq!(cached, 25);
    }

    #[tokio::test]
    async fn test_has_failed_review_logic() {
        let db = setup_db().await;

        // Setup patchset
        let thread_id = db.create_thread("root", "Subject", 100).await.unwrap();
        db.create_message(
            "msg1", thread_id, None, "Author", "Subject", 100, "", "", "", None, None,
        )
        .await
        .unwrap();
        let ps_id = db
            .create_patchset(
                thread_id,
                Some("msg1"),
                "msg1",
                "Subject",
                "Author",
                100,
                1,
                1,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let patch_id = db.create_patch(ps_id, "msg1", 1, "diff").await.unwrap();

        // 1. Initial State: No reviews
        assert!(!db.has_failed_review(ps_id, patch_id, None).await.unwrap());

        // 2. Failed Review (No interaction) -> Should be detected
        let review_id = db
            .create_review(ps_id, Some(patch_id), "gemini", "test-model", None, None)
            .await
            .unwrap();
        db.update_review_status(review_id, "FailedToApply", None)
            .await
            .unwrap();

        assert!(db.has_failed_review(ps_id, patch_id, None).await.unwrap());

        // 3. Status "Failed" (No interaction) -> Should be detected
        db.update_review_status(review_id, "Failed", None)
            .await
            .unwrap();
        assert!(db.has_failed_review(ps_id, patch_id, None).await.unwrap());

        // 4. Status "Reviewed" (Success) -> Should NOT be detected
        db.update_review_status(review_id, "Reviewed", None)
            .await
            .unwrap();
        assert!(!db.has_failed_review(ps_id, patch_id, None).await.unwrap());

        // 5. Status "Failed" WITH interaction_id -> Should NOT be detected (reached AI)
        // Revert to Failed first
        db.update_review_status(review_id, "Failed", None)
            .await
            .unwrap();

        // Create interaction first to satisfy FK
        db.create_ai_interaction(AiInteractionParams {
            id: "int_id",
            parent_id: None,
            workflow_id: None,
            provider: "p",
            model: "m",
            input: "",
            output: "",
            tokens_in: 0,
            tokens_out: 0,
            tokens_cached: 0,
        })
        .await
        .unwrap();

        // Set interaction_id
        db.complete_review(
            review_id,
            "Failed",
            "desc",
            None,
            Some("int_id"),
            None,
            None,
        )
        .await
        .unwrap();

        assert!(!db.has_failed_review(ps_id, patch_id, None).await.unwrap());
    }

    #[tokio::test]
    async fn test_cross_thread_no_merge() {
        let db = setup_db().await;

        // 1. Create Thread A and Patchset A (1/2)
        let t1 = db
            .create_thread("root1", "Subject 1/2", 1000)
            .await
            .unwrap();
        db.create_message(
            "msg1",
            t1,
            None,
            "Author",
            "[PATCH 1/2] Series",
            1000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let ps1 = db
            .create_patchset(
                t1,
                None,
                "msg1",
                "[PATCH 1/2] Series",
                "Author",
                1000,
                2,
                0,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. Create Thread B and Patchset B (2/2) - Same Author, Close Time, Different Thread
        let t2 = db
            .create_thread("root2", "Subject 2/2", 1005)
            .await
            .unwrap(); // 5 seconds later
        db.create_message(
            "msg2",
            t2,
            None,
            "Author",
            "[PATCH 2/2] Series",
            1005,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let ps2 = db
            .create_patchset(
                t2,
                None,
                "msg2",
                "[PATCH 2/2] Series",
                "Author",
                1005,
                2,
                0,
                "",
                "",
                None,
                2,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 3. Assert they DID NOT merge (ps2 should NOT equal ps1)
        assert_ne!(
            ps1, ps2,
            "Patchsets from different threads should NOT merge even if author/time match"
        );

        // 4. Verify total patches count or received parts
        db.create_patch(ps1, "msg1", 1, "").await.unwrap();
        db.create_patch(ps2, "msg2", 2, "").await.unwrap();

        let details1 = db
            .get_patchset_details(ps1, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(details1["received_parts"], 1);
        let details2 = db
            .get_patchset_details(ps2, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(details2["received_parts"], 1);
    }

    #[tokio::test]
    async fn test_duplicate_ingestion_on_full_patchset() {
        let db = setup_db().await;

        // 1. Create Patchset (1/1)
        let t1 = db.create_thread("root1", "Subject", 1000).await.unwrap();
        let msg_id = "msg1";

        db.create_message(
            msg_id,
            t1,
            None,
            "Author",
            "[PATCH 1/1] Subject",
            1000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let ps1 = db
            .create_patchset(
                t1,
                None,
                msg_id,
                "[PATCH 1/1] Subject",
                "Author",
                1000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 2. Add patch so it becomes full
        db.create_patch(ps1, msg_id, 1, "diff").await.unwrap();

        let details = db
            .get_patchset_details(ps1, None, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(details["received_parts"], 1);
        assert_eq!(details["total_parts"], 1);

        // 3. Try to ingest the SAME patch again
        // It matches the existing patchset (Author/Time/Thread).
        // It IS full (1/1).
        // But it IS a duplicate (msg_id matches).
        // So it SHOULD merge.
        let ps2 = db
            .create_patchset(
                t1,
                None,
                msg_id,
                "[PATCH 1/1] Subject",
                "Author",
                1000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            ps1, ps2,
            "Should merge duplicate into existing patchset even if full"
        );

        // 4. Try to ingest a NEW patch (different ID) that looks like it belongs
        // This simulates a collision or a separate series with same metadata.
        // It should NOT merge because the set is full and it's NOT a duplicate.
        let msg_id_new = "msg_new";
        db.create_message(
            msg_id_new,
            t1,
            None,
            "Author",
            "[PATCH 1/1] Subject",
            1000,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();

        let ps3 = db
            .create_patchset(
                t1,
                None,
                msg_id_new,
                "[PATCH 1/1] Subject",
                "Author",
                1000,
                1,
                0,
                "",
                "",
                None,
                1,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        assert_ne!(
            ps1, ps3,
            "Should create NEW patchset for non-duplicate when full"
        );
    }

    #[tokio::test]
    async fn test_mailing_list_filtering() {
        let db = setup_db().await;

        // 1. Setup lists
        db.ensure_mailing_list("List A", "list-a").await.unwrap();
        db.ensure_mailing_list("List B", "list-b").await.unwrap();
        let id_a = db
            .get_mailing_list_id_by_name("list-a")
            .await
            .unwrap()
            .unwrap();
        let id_b = db
            .get_mailing_list_id_by_name("list-b")
            .await
            .unwrap()
            .unwrap();

        // 2. Create threads
        let t_a = db.create_thread("root_a", "Subject A", 100).await.unwrap();
        let t_b = db.create_thread("root_b", "Subject B", 100).await.unwrap();

        // 3. Create Message A (in List A)
        db.create_message(
            "msg_a",
            t_a,
            None,
            "Author",
            "Subject A",
            100,
            "",
            "",
            "",
            None,
            Some("list-a"),
        )
        .await
        .unwrap();
        let msg_a_id = db.get_message_id_by_msg_id("msg_a").await.unwrap().unwrap();
        db.add_message_to_mailing_list(msg_a_id, id_a)
            .await
            .unwrap();

        // 4. Create Message B (in List B)
        db.create_message(
            "msg_b",
            t_b,
            None,
            "Author",
            "Subject B",
            100,
            "",
            "",
            "",
            None,
            Some("list-b"),
        )
        .await
        .unwrap();
        let msg_b_id = db.get_message_id_by_msg_id("msg_b").await.unwrap().unwrap();
        db.add_message_to_mailing_list(msg_b_id, id_b)
            .await
            .unwrap();

        // 5. Create Patchsets
        // Patchset A linked to msg_a (as cover letter)
        let ps_a = db
            .create_patchset(
                t_a,
                Some("msg_a"),
                "msg_a",
                "Subject A",
                "Author",
                100,
                1,
                1,
                "",
                "",
                None,
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // Patchset B linked to msg_b (as cover letter)
        let ps_b = db
            .create_patchset(
                t_b,
                Some("msg_b"),
                "msg_b",
                "Subject B",
                "Author",
                100,
                1,
                1,
                "",
                "",
                None,
                0,
                None,
                true,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        // 6. Test filtering messages
        let msgs_a = db
            .get_messages(10, 0, None, Some("list-a".to_string()))
            .await
            .unwrap();
        assert_eq!(msgs_a.len(), 1);
        assert_eq!(msgs_a[0].message_id, "msg_a");

        let msgs_b = db
            .get_messages(10, 0, None, Some("list-b".to_string()))
            .await
            .unwrap();
        assert_eq!(msgs_b.len(), 1);
        assert_eq!(msgs_b[0].message_id, "msg_b");

        // 7. Add patch to ps_a to make it pass the CURRENT logic (patches only)
        // db.create_message(
        //     "patch_a_1", t_a, None, "Author", "Patch A 1", 101, "", "", "", None, Some("list-a")
        // ).await.unwrap();
        // let p_a_1_id = db.get_message_id_by_msg_id("patch_a_1").await.unwrap().unwrap();
        // db.add_message_to_mailing_list(p_a_1_id, id_a).await.unwrap();
        // db.create_patch(ps_a, "patch_a_1", 1, "").await.unwrap();

        // Now ps_a has a patch in list-a.
        // UPDATE: We commented out the patch creation above.
        // ps_a only has a cover letter in list-a.
        // The UNION query should find it.
        let psets_a = db
            .get_patchsets(10, 0, None, Some("list-a".to_string()))
            .await
            .unwrap();
        assert_eq!(psets_a.len(), 1);
        assert_eq!(psets_a[0].id, ps_a);

        let psets_b = db
            .get_patchsets(10, 0, None, Some("list-a".to_string()))
            .await
            .unwrap();
        let found_b = psets_b.iter().any(|p| p.id == ps_b);
        assert!(!found_b);
    }
}
