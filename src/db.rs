use crate::ReviewStatus;
use crate::settings::DatabaseSettings;
use anyhow::Result;
use libsql::Builder;
use serde::Serialize;
use serde_json::json;
use tracing::info;

pub struct Database {
    pub conn: libsql::Connection,
}

#[derive(Debug, Serialize)]
pub struct Subsystem {
    pub id: i64,
    pub name: String,
    pub mailing_list_address: String,
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
    pub subsystems: Vec<String>,
    pub findings_low: Option<i64>,
    pub findings_medium: Option<i64>,
    pub findings_high: Option<i64>,
    pub findings_critical: Option<i64>,
    pub baseline_id: Option<i64>,
}

#[derive(Debug, Serialize)]
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
}

pub struct ReviewExperimentParams<'a> {
    pub patchset_id: i64,
    pub provider: &'a str,
    pub model: &'a str,
    pub prompts_hash: Option<&'a str>,
    pub baseline_id: Option<i64>,
    pub result_description: &'a str,
    pub interaction_id: Option<&'a str>,
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

#[derive(Debug, Serialize)]
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
    pub file_path: String,
    pub line_number: i64,
    pub severity: Severity,
    pub message: String,
    pub suggestion: Option<String>,
}

impl Database {
    pub async fn get_message_details(&self, id: i64) -> Result<Option<MessageRow>> {
        let mut rows = self.conn.query(
            "SELECT id, message_id, thread_id, in_reply_to, author, subject, date, body, to_recipients, cc_recipients, git_blob_hash, mailing_list FROM messages WHERE id = ?",
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

            Ok(Some(MessageRow {
                id: row.get(0)?,
                message_id: row.get(1)?,
                thread_id,
                in_reply_to: row.get(3).ok(),
                author: row.get(4).ok(),
                subject: row.get(5).ok(),
                date: row.get(6).ok(),
                body: row.get(7).ok(),
                to: row.get(8).ok(),
                cc: row.get(9).ok(),
                git_blob_hash: row.get(10).ok(),
                mailing_list: row.get(11).ok(),
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
            return self.get_patchset_details(id).await;
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
            return self.get_patchset_details(id).await;
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
            if let Some(b) = body {
                if !b.is_empty() {
                    return Ok(Some(b));
                }
            }
            // Try git blob
            let hash: Option<String> = row.get(1).ok();
            let group: Option<String> = row.get(2).ok();

            if let (Some(_h), Some(_g)) = (hash, group) {
                // We don't have easy access to git_ops::read_blob here without repo path.
                // Assuming db shouldn't know about repo path logic or we pass it?
                // Reviewer service has repo path.
                // Let's just return None if body is empty in DB, and let caller handle blob if needed.
                // But for base-commit, we need the body.
                // Ideally body is populated in DB if small?
                // Sashiko stores body in DB unless it's a huge patch?
                // "body_to_store" in main.rs logic:
                // `if is_git_hash { ("", Some(hash)) } else { (body, None) }`
                // So if it's from git archive, body is empty in DB.
                return Ok(None);
            }
            Ok(None)
        } else {
            Ok(None)
        }
    }

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

        let _ = self.migrate_tool_usages().await;
        Ok(())
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
        self.conn
            .execute(
                "INSERT INTO reviews (patchset_id, patch_id, provider, model_name, prompts_git_hash, baseline_id, status, created_at)
             VALUES (?, ?, ?, ?, ?, ?, 'Pending', ?)",
                libsql::params![
                    patchset_id,
                    patch_id,
                    provider,
                    model,
                    prompts_hash,
                    baseline_id,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_secs() as i64
                ],
            )
            .await?;

        let mut rows = self
            .conn
            .query("SELECT last_insert_rowid()", libsql::params![])
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
        let mut rows = if let Some(bid) = baseline_id {
            self.conn
                .query(
                    "SELECT 1 FROM reviews WHERE patchset_id = ? AND patch_id = ? AND baseline_id = ? AND status = 'Reviewed'",
                    libsql::params![patchset_id, patch_id, bid],
                )
                .await?
        } else {
            self.conn
                .query(
                    "SELECT 1 FROM reviews WHERE patchset_id = ? AND patch_id = ? AND baseline_id IS NULL AND status = 'Reviewed'",
                    libsql::params![patchset_id, patch_id],
                )
                .await?
        };

        Ok(rows.next().await.ok().flatten().is_some())
    }

    pub async fn has_failed_review(
        &self,
        patchset_id: i64,
        patch_id: i64,
        baseline_id: Option<i64>,
    ) -> Result<bool> {
        let mut rows = if let Some(bid) = baseline_id {
            self.conn
                .query(
                    "SELECT 1 FROM reviews WHERE patchset_id = ? AND patch_id = ? AND baseline_id = ? AND status IN ('Failed', 'FailedToApply') AND interaction_id IS NULL",
                    libsql::params![patchset_id, patch_id, bid],
                )
                .await?
        } else {
            self.conn
                .query(
                    "SELECT 1 FROM reviews WHERE patchset_id = ? AND patch_id = ? AND baseline_id IS NULL AND status IN ('Failed', 'FailedToApply') AND interaction_id IS NULL",
                    libsql::params![patchset_id, patch_id],
                )
                .await?
        };

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

    pub async fn create_review_experiment(&self, params: ReviewExperimentParams<'_>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO reviews (patchset_id, provider, model_name, prompts_git_hash, baseline_id, result_description, interaction_id, created_at, status)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'Finished')",
            libsql::params![
                params.patchset_id,
                params.provider,
                params.model,
                params.prompts_hash,
                params.baseline_id,
                params.result_description,
                params.interaction_id,
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64
            ],
        ).await?;
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
        let mut rows = self.conn.query("SELECT id, logs, provider, model_name FROM reviews WHERE status IN ('Reviewed', 'Failed') AND logs IS NOT NULL", ()).await?;

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
            if let Ok(mut c_rows) = count_rows {
                if let Ok(Some(c_row)) = c_rows.next().await {
                    let count: i64 = c_row.get(0)?;
                    if count > 0 {
                        continue;
                    }
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
                                // We need to find the response to get output length.
                                // But here we iterate linearly.
                                // Let's just estimate or find next part?
                                // Simplification: just record usage without output length for now?
                                // Or try to find corresponding functionResponse in next parts/items?
                                // actually the history interleaves them.

                                // For now, let's insert what we have.
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
                                // We could update the previous entry?
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
        self.conn.execute(
            "INSERT INTO findings (review_id, file_path, line_number, severity, message, suggestion)
             VALUES (?, ?, ?, ?, ?, ?)",
            libsql::params![
                finding.review_id,
                finding.file_path,
                finding.line_number,
                finding.severity as i32,
                finding.message,
                finding.suggestion,
            ],
        ).await?;
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
            if let Ok(mut c_rows) = count_check {
                if let Ok(Some(c_row)) = c_rows.next().await {
                    let count: i64 = c_row.get(0)?;
                    if count > 0 {
                        continue;
                    }
                }
            }

            // Parse JSON and insert findings
            if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&output_raw) {
                if let Some(findings_arr) = json_val.get("findings").and_then(|f| f.as_array()) {
                    for f in findings_arr {
                        let file_path = f["file"].as_str().unwrap_or("unknown").to_string();
                        let line_number = f["line"].as_i64().unwrap_or(0);
                        let severity_str = f["severity"].as_str().unwrap_or("Low");
                        let message = f["message"].as_str().unwrap_or("").to_string();
                        let suggestion = f["suggestion"].as_str().map(|s| s.to_string());

                        let severity = Severity::from_str(severity_str);

                        let _ = self
                            .create_finding(Finding {
                                review_id,
                                file_path,
                                line_number,
                                severity,
                                message,
                                suggestion,
                            })
                            .await;
                    }
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
            "findings": findings_data
        }))
    }

    pub async fn get_review_stats(&self) -> Result<serde_json::Value> {
        let sql = "SELECT 
            r.provider, 
            r.model_name, 
            r.status, 
            count(*),
            sum(COALESCE(ai.tokens_in, 0)),
            sum(COALESCE(ai.tokens_out, 0)),
            sum(COALESCE(ai.tokens_cached, 0))
        FROM reviews r
        LEFT JOIN ai_interactions ai ON r.interaction_id = ai.id
        GROUP BY r.provider, r.model_name, r.status";

        let mut rows = self.conn.query(sql, ()).await?;
        let mut stats = Vec::new();
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

    // Tags
    #[allow(dead_code)]
    pub async fn ensure_tag(&self, name: &str) -> Result<i64> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO tags (name) VALUES (?)",
                libsql::params![name],
            )
            .await?;

        let mut rows = self
            .conn
            .query("SELECT id FROM tags WHERE name = ?", libsql::params![name])
            .await?;

        if let Ok(Some(row)) = rows.next().await {
            Ok(row.get(0)?)
        } else {
            Err(anyhow::anyhow!("Failed to ensure tag"))
        }
    }

    #[allow(dead_code)]
    pub async fn add_tag_to_message(&self, message_id: i64, tag_id: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO messages_tags (message_id, tag_id) VALUES (?, ?)",
                libsql::params![message_id, tag_id],
            )
            .await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn add_tag_to_thread(&self, thread_id: i64, tag_id: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO threads_tags (thread_id, tag_id) VALUES (?, ?)",
                libsql::params![thread_id, tag_id],
            )
            .await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn add_tag_to_patch(&self, patch_id: i64, tag_id: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO patches_tags (patch_id, tag_id) VALUES (?, ?)",
                libsql::params![patch_id, tag_id],
            )
            .await?;
        Ok(())
    }

    #[allow(dead_code)]
    pub async fn add_tag_to_patchset(&self, patchset_id: i64, tag_id: i64) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO patchsets_tags (patchset_id, tag_id) VALUES (?, ?)",
                libsql::params![patchset_id, tag_id],
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
        if let Ok(Some(old_thread_id)) = self.get_thread_id_for_message(message_id).await {
            if old_thread_id != thread_id {
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

                // 4. Merge tags
                self.conn
                    .execute(
                        "UPDATE OR IGNORE threads_tags SET thread_id = ? WHERE thread_id = ?",
                        libsql::params![thread_id, old_thread_id],
                    )
                    .await?;
                // Delete any remaining (conflicting) tag mappings for the old thread
                self.conn
                    .execute(
                        "DELETE FROM threads_tags WHERE thread_id = ?",
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
        }

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
    ) -> Result<Option<i64>> {
        // Find candidate patchsets in this thread
        let mut rows = self
            .conn
            .query(
                "SELECT id, date, author, subject, subject_index, total_parts FROM patchsets WHERE thread_id = ?",
                libsql::params![thread_id],
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

            // Parse version from existing subject
            let existing_version = crate::patch::parse_subject_version(&existing_subject);

            // Matching logic:
            // 1. Author matches OR it's a multi-part series with matching total_parts (trusting thread context)
            //    BUT strict_author enforces strict author matching (for Email/NNTP).
            // 2. Time must be close (within 24 hours / 86400s)
            // 3. Total parts must match
            // 4. Versions must match OR one is unspecified (None)
            // 5. For singletons (total=1), Subject must match (fuzzy) to avoid merging unrelated patches
            
            let versions_compatible = match (version, existing_version) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            };

            let is_singleton = total_parts == 1;
            // For singletons, we require the subject to be somewhat similar to avoid merging unrelated patches.
            let subject_match = if is_singleton {
                if subject == existing_subject {
                    true
                } else {
                    // Allow merging 0/1 (cover) and 1/1 (patch) even if subjects differ
                    (part_index == 0 && existing_subject_index == 1)
                        || (part_index == 1 && existing_subject_index == 0)
                }
            } else {
                true // For series, we rely on 1/N, 2/N pattern and author/time.
            };

            // Relaxed author check logic
            let author_match = existing_author == author;
            let series_match = total_parts > 1 && total_parts == existing_total;

            let author_or_series_match = if strict_author {
                author_match
            } else {
                author_match || series_match
            };

            if author_or_series_match
                && (!strict_author || (date - existing_date).abs() < 86400)
                && versions_compatible
                && total_parts == existing_total
                && subject_match
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

                // Merge tags
                self.conn
                    .execute(
                        "INSERT OR IGNORE INTO patchsets_tags (patchset_id, tag_id)
                         SELECT ?, tag_id FROM patchsets_tags WHERE patchset_id = ?",
                        libsql::params![target_id, merge_from_id],
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
                // If we merged, we might need to update the subject index of the target to the best one we found
                // But we don't have the subject string from the merged one easily available here.
                // However, the existing target subject is likely fine unless part_index is better.
                // We just update subject_index to be correct if we merged a better one?
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

            return Ok(Some(target_id));
        }

        // No match found, create new patchset
        self.conn
            .execute(
                "INSERT INTO patchsets (thread_id, cover_letter_message_id, subject, author, date, total_parts, received_parts, status, parser_version, to_recipients, cc_recipients, subject_index, baseline_id) 
                 VALUES (?, ?, ?, ?, ?, ?, 0, 'Incomplete', ?, ?, ?, ?, ?)",
                libsql::params![thread_id, cover_letter_message_id, subject, author, date, total_parts, parser_version, to, cc, part_index, baseline_id],
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
        if let Some(old_id) = old_patchset_id {
            if old_id != patchset_id {
                self.conn
                        .execute(
                            "UPDATE patchsets SET received_parts = (SELECT COUNT(*) FROM patches WHERE patchset_id = ?) WHERE id = ?",
                            libsql::params![old_id, old_id],
                        )
                        .await?;
            }
        }

        // Check if complete and update status
        // We only transition from 'Incomplete' to 'Pending' (ready for review)
        self.conn.execute(
            "UPDATE patchsets SET status = 'Pending' WHERE id = ? AND received_parts >= total_parts AND status = 'Incomplete'",
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

    fn build_search(&self, query: Option<String>, target: &str) -> (String, Vec<String>) {
        let mut conditions = Vec::new();
        let mut params = Vec::new();

        // Always exclude placeholders
        conditions.push("subject != '(placeholder)'".to_string());

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
                        "id IN (SELECT patchset_id FROM patchsets_subsystems ps JOIN subsystems s ON ps.subsystem_id = s.id WHERE s.name LIKE ?)"
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
    ) -> Result<Vec<PatchsetRow>> {
        let (where_clause, params) = self.build_search(query, "patchset");
        // We use p.* alias implicitely by using unqualified names in WHERE which is fine given no collisions.
        // But for clarity/safety we should alias in FROM.
        // build_search returns "WHERE author ...".

        let sql = format!(
            "SELECT p.id, p.subject, p.status, p.thread_id, p.author, p.date, p.cover_letter_message_id, p.total_parts, p.received_parts, GROUP_CONCAT(s.name, ','),
             COALESCE(f.low, 0), COALESCE(f.medium, 0), COALESCE(f.high, 0), COALESCE(f.critical, 0)
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
                baseline_id: None, // We don't fetch it in this query yet, or we can add it. For now None is safe.
            });
        }
        Ok(patchsets)
    }

    pub async fn get_messages(
        &self,
        limit: usize,
        offset: usize,
        query: Option<String>,
    ) -> Result<Vec<MessageRow>> {
        let (where_clause, params) = self.build_search(query, "message");
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
                thread: None,
            });
        }
        Ok(messages)
    }

    pub async fn count_patchsets(&self, query: Option<String>) -> Result<usize> {
        let (where_clause, params) = self.build_search(query, "patchset");
        let sql = format!("SELECT COUNT(*) FROM patchsets {}", where_clause);

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

    pub async fn count_messages(&self, query: Option<String>) -> Result<usize> {
        let (where_clause, params) = self.build_search(query, "message");
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

    pub async fn get_patchset_details(&self, id: i64) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT p.id, p.subject, p.status, p.to_recipients, p.cc_recipients, 
                    p.author, p.date, p.cover_letter_message_id, p.thread_id,
                    p.total_parts, p.received_parts
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

            // Fetch reviews
            let mut reviews = Vec::new();
            let mut rev_rows = self
                .conn
                .query(
                    "SELECT r.model_name, r.summary, r.created_at, ai.input_context, ai.output_raw, 
                            b.repo_url, b.branch, b.last_known_commit,
                            r.provider, r.prompts_git_hash, r.result_description,
                            r.status, r.inline_review, r.logs, ai.tokens_in, ai.tokens_out, r.patch_id, r.id, ai.tokens_cached
                 FROM reviews r
                 LEFT JOIN ai_interactions ai ON r.interaction_id = ai.id
                 LEFT JOIN baselines b ON r.baseline_id = b.id
                 WHERE r.patchset_id = ?
                 ORDER BY r.created_at ASC",
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
                    "baseline": {
                        "repo_url": r.get::<Option<String>>(5).ok(),
                        "branch": r.get::<Option<String>>(6).ok(),
                        "commit": r.get::<Option<String>>(7).ok(),
                    },
                    "provider": r.get::<Option<String>>(8).ok(),
                    "prompts_hash": r.get::<Option<String>>(9).ok(),
                    "result": r.get::<Option<String>>(10).ok(),
                    "status": r.get::<Option<String>>(11).ok(),
                    "inline_review": r.get::<Option<String>>(12).ok(),
                    "logs": r.get::<Option<String>>(13).ok(),
                    "tokens_in": r.get::<Option<u32>>(14).ok(),
                    "tokens_out": r.get::<Option<u32>>(15).ok(),
                    "patch_id": r.get::<Option<i64>>(16).ok(),
                    "id": r.get::<i64>(17).ok(),
                    "tokens_cached": r.get::<Option<u32>>(18).ok(),
                }));
            }

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

            // Fetch patches with subject and msg_db_id
            let mut patches = Vec::new();
            let mut patch_rows = self
                .conn
                .query(
                    "SELECT p.id, p.message_id, p.part_index, m.id, m.subject 
                 FROM patches p
                 LEFT JOIN messages m ON p.message_id = m.message_id
                 WHERE p.patchset_id = ? 
                 ORDER BY p.part_index ASC",
                    libsql::params![pid],
                )
                .await?;
            while let Ok(Some(p)) = patch_rows.next().await {
                patches.push(serde_json::json!({
                    "id": p.get::<i64>(0)?,
                    "message_id": p.get::<String>(1)?,
                    "part_index": p.get::<Option<i64>>(2).ok(),
                    "msg_db_id": p.get::<Option<i64>>(3).ok(),
                    "subject": p.get::<Option<String>>(4).ok(),
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
                "to": to,
                "cc": cc,
                "total_parts": total_parts,
                "received_parts": received_parts,
                "reviews": reviews,
                "patches": patches,
                "thread": messages,
                "subsystems": subsystems
            })))
        } else {
            Ok(None)
        }
    }

    pub async fn get_review_details(&self, id: i64) -> Result<Option<serde_json::Value>> {
        let mut rows = self
            .conn
            .query(
                "SELECT r.id, r.model_name, r.summary, r.created_at, ai.input_context, ai.output_raw, 
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

    pub async fn get_patch_diffs(
        &self,
        patchset_id: i64,
    ) -> Result<Vec<(i64, i64, String, String, String, i64)>> {
        let mut rows = self
            .conn
            .query(
                "SELECT p.id, p.part_index, p.diff, m.subject, m.author, m.date 
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
            diffs.push((id, index, diff, subject, author, date));
        }
        Ok(diffs)
    }

    pub async fn get_pending_patchsets(&self, limit: usize) -> Result<Vec<PatchsetRow>> {
        let mut rows = self.conn.query(
            "SELECT id, subject, status, thread_id, author, date, cover_letter_message_id, total_parts, received_parts, baseline_id 
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
                format!("UPDATE patchsets SET status = '{}' WHERE status IN ('Applying', 'In Review', 'Reviewing')", status_pending).as_str(),
                (),
            )
            .await?;

        // Reset Reviews
        let count_rev = self
            .conn
            .execute(
                format!(
                    "UPDATE reviews SET status = '{}' WHERE status IN ('Applying', 'In Review')",
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
                "Patch 1",
                "Author A",
                1000,
                2,
                1,
                "to",
                "cc",
                Some(1),
                1,
                None, true
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
                "Cover Letter",
                "Author A",
                1005,
                2,
                1,
                "to",
                "cc",
                Some(1),
                0,
                None, true
            )
            .await
            .unwrap();
        assert_eq!(ps1, ps1_update);

        let list = db.get_patchsets(1, 0, None).await.unwrap();
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
            "Patch 2",
            "Author A",
            1006,
            2,
            1,
            "to",
            "cc",
            Some(1),
            2,
            None, true
        )
        .await
        .unwrap();

        let list = db.get_patchsets(1, 0, None).await.unwrap();
        assert_eq!(list[0].subject.as_deref(), Some("Cover Letter"));

        // 4. Create NEW patchset in same thread (Author B, Time 1000 - same time but diff author)
        // With relaxed logic, this SHOULD merge if total_parts match (assuming same series).
        let ps3 = db
            .create_patchset(
                thread_id,
                None,
                "Other Author",
                "Author B",
                1000,
                2,
                1,
                "to",
                "cc",
                Some(1),
                0,
                None, false
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
                "[PATCH v2] Patchset 1",
                "Author B",
                1002,
                2,
                1,
                "to",
                "cc",
                Some(2),
                0,
                None, true
            )
            .await
            .unwrap();
        assert_eq!(
            ps1, ps_v2,
            "Implicit v1 should merge with v2 if time/author match"
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
                "Series",
                "Merger",
                10000,
                3,
                1,
                "",
                "",
                Some(1),
                1,
                None, true
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
                "Series",
                "Merger",
                200000,
                3,
                1,
                "",
                "",
                Some(1),
                3,
                None, true
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
                "Series",
                "Merger",
                120000,
                3,
                1,
                "",
                "",
                Some(1),
                3,
                None, true
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
                "Series",
                "Merger",
                65000,
                3,
                1,
                "",
                "",
                Some(1),
                2,
                None, true
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
                    &subject,
                    author,
                    time,
                    5,
                    1,
                    "to",
                    "cc",
                    None,
                    idx as u32,
                    None, true
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
        let list = db.get_patchsets(1, 0, None).await.unwrap();
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
                "Status Test",
                author,
                60000,
                2,
                1,
                "",
                "",
                None,
                1,
                None, true
            )
            .await
            .unwrap()
            .unwrap();

        // Check initial status
        let list = db.get_patchsets(1, 0, None).await.unwrap();
        assert_eq!(list[0].status.as_deref(), Some("Incomplete"));

        // 2. Add Patch 1. received=1. Total=2. Status should be Incomplete.
        db.create_message(
            "msg_1", thread_id, None, author, "Part 1", 60005, "", "", "", None, None,
        )
        .await
        .unwrap();
        db.create_patch(ps_id, "msg_1", 1, "diff").await.unwrap();
        let list = db.get_patchsets(1, 0, None).await.unwrap();
        assert_eq!(list[0].status.as_deref(), Some("Incomplete"));

        // 3. Add Patch 2. received=2. Total=2. Status should transition to Pending.
        db.create_message(
            "msg_2", thread_id, None, author, "Part 2", 60010, "", "", "", None, None,
        )
        .await
        .unwrap();
        db.create_patch(ps_id, "msg_2", 2, "diff").await.unwrap();
        let list = db.get_patchsets(1, 0, None).await.unwrap();
        assert_eq!(list[0].status.as_deref(), Some("Pending"));
    }

    #[tokio::test]
    async fn test_implicit_version_merging() {
        let db = setup_db().await;
        let thread_id = db
            .create_thread("root_v6", "Version 6 Series", 30000)
            .await
            .unwrap();
        let author = "Author V6 <v6@example.com>";

        // Case: Cover letter has v6, but patches don't say v6 (implicitly v1?)
        // If the user forgot to version patches, they should still merge if time/author match.
        // However, strict version check prevents this if one is v6 and other is v1.
        // But the prompt says "They should be merged".
        // This implies loose version matching if one side is v1 (default)?

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
                "[PATCH 00/33 v6] Cover",
                author,
                30000,
                33,
                1,
                "",
                "",
                Some(6),
                0,
                None, true
            )
            .await
            .unwrap()
            .unwrap();

        // 2. Patch 1: [PATCH 01/33] -> v1 (implicit) -> Pass None
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
                "[PATCH 01/33] Part 1",
                author,
                30005,
                33,
                1,
                "",
                "",
                None,
                1,
                None, true
            )
            .await
            .unwrap()
            .unwrap();

        // With strict checking, this might fail (assert_eq will panic if not merged).
        // If it fails, we need to relax the check in `create_patchset`.
        assert_eq!(
            ps_cover, ps_p1,
            "Should merge explicit v6 cover with implicit v1 patch if context matches"
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
                "[PATCH] Fix A",
                author,
                60000,
                1,
                1,
                "",
                "",
                None,
                1,
                None, true
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
                "[PATCH] Fix B",
                author,
                60005,
                1,
                1,
                "",
                "",
                None,
                1,
                None, true
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
                "[PATCH 0/1] Subject A",
                author,
                60000,
                1,
                1,
                "",
                "",
                None,
                0,
                None, true
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
                "[PATCH 1/1] Subject B",
                author,
                60005,
                1,
                1,
                "",
                "",
                None,
                1,
                None, true
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
                "[PATCH v5 1/2] Part 1",
                author,
                40000,
                2,
                1,
                "",
                "",
                Some(5),
                1,
                None, true
            )
            .await
            .unwrap()
            .unwrap();

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
                "[PATCH v6 1/2] Part 1",
                author,
                40010,
                2,
                1,
                "",
                "",
                Some(6),
                1,
                None, true
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
                "[PATCH v3 0/2] Cover",
                author,
                50000,
                2,
                1,
                "",
                "",
                Some(3),
                0,
                None, true
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
                "[PATCH v3 1/2] Part 1",
                author,
                50005,
                2,
                1,
                "",
                "",
                Some(3),
                1,
                None, true
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
                "[PATCH v3 2/2] Part 2",
                author,
                50010,
                2,
                1,
                "",
                "",
                Some(3),
                2,
                None, true
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

        // 1. [PATCH v3 00/17] -> v3
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
                "[PATCH v3 00/17] Cover",
                author,
                80000,
                17,
                1,
                "",
                "",
                Some(3),
                0,
                None, true
            )
            .await
            .unwrap()
            .unwrap();

        // 2. [PATCH 01/17] Support v2 hardware -> Should treat as implicit version (None), NOT v2
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
        let subject = "[PATCH 01/17] Support v2 hardware";
        let parsed_ver = crate::patch::parse_subject_version(subject);

        let ps_part1 = db
            .create_patchset(
                thread_id, None, subject, author, 80005, 17, 1, "", "",
                parsed_ver, // Pass the result of the potentially buggy parser
                1,
                None, true
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            ps_cover, ps_part1,
            "Should merge even if subject contains 'v2'"
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
                "[PATCH 1/2] Part 1",
                author,
                90000,
                2,
                1,
                "",
                "",
                None,
                1,
                None, true
            )
            .await
            .unwrap()
            .unwrap();

        // 2. Add dependencies to ps1 (Review, Tag, Subsystem)
        let review_id = db
            .create_review(ps1, None, "test_provider", "test_model", None, None)
            .await
            .unwrap();

        let tag_id = db.ensure_tag("test_tag").await.unwrap();
        db.conn
            .execute(
                "INSERT INTO patchsets_tags (patchset_id, tag_id) VALUES (?, ?)",
                libsql::params![ps1, tag_id],
            )
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
                "[PATCH 2/2] Part 2",
                author,
                90005, // Close enough
                2,
                1,
                "",
                "",
                None,
                2,
                None, true
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

        // Check tag
        let mut rows = db
            .conn
            .query(
                "SELECT count(*) FROM patchsets_tags WHERE patchset_id = ?",
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
        db.create_message("msg1", thread_id, None, "Author", "Subject", 100, "", "", "", None, None).await.unwrap();
        let ps_id = db.create_patchset(thread_id, Some("msg1"), "Subject", "Author", 100, 1, 1, "", "", None, 1, None, true).await.unwrap().unwrap();
        let patch_id = db.create_patch(ps_id, "msg1", 1, "diff").await.unwrap();

        // 1. Initial State: No reviews
        assert!(!db.has_failed_review(ps_id, patch_id, None).await.unwrap());

        // 2. Failed Review (No interaction) -> Should be detected
        let review_id = db.create_review(ps_id, Some(patch_id), "p", "m", None, None).await.unwrap();
        db.update_review_status(review_id, "FailedToApply", None).await.unwrap();
        
        assert!(db.has_failed_review(ps_id, patch_id, None).await.unwrap());

        // 3. Status "Failed" (No interaction) -> Should be detected
        db.update_review_status(review_id, "Failed", None).await.unwrap();
        assert!(db.has_failed_review(ps_id, patch_id, None).await.unwrap());

        // 4. Status "Reviewed" (Success) -> Should NOT be detected
        db.update_review_status(review_id, "Reviewed", None).await.unwrap();
        assert!(!db.has_failed_review(ps_id, patch_id, None).await.unwrap());

        // 5. Status "Failed" WITH interaction_id -> Should NOT be detected (reached AI)
        // Revert to Failed first
        db.update_review_status(review_id, "Failed", None).await.unwrap();
        
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
        }).await.unwrap();

        // Set interaction_id
        db.complete_review(review_id, "Failed", "desc", None, Some("int_id"), None, None).await.unwrap();
        
        assert!(!db.has_failed_review(ps_id, patch_id, None).await.unwrap());
    }
}
