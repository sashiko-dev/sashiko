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
    pub message_id: String,
    pub subject: Option<String>,
    pub author: Option<String>,
    pub date: Option<i64>,
    pub status: Option<String>,
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

    pub async fn message_exists(&self, message_id: &str) -> Result<bool> {
        let mut rows = self
            .conn
            .query(
                "SELECT 1 FROM patchsets WHERE message_id = ?",
                libsql::params![message_id],
            )
            .await?;
        Ok(rows.next().await?.is_some())
    }

    pub async fn create_patchset(
        &self,
        message_id: &str,
        subject: &str,
        author: &str,
        date: i64,
        total_parts: u32,
    ) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO patchsets (message_id, subject, author, date, total_parts, received_parts, status) VALUES (?, ?, ?, ?, ?, 1, 'Pending') ON CONFLICT(message_id) DO UPDATE SET received_parts = received_parts + 1",
                libsql::params![message_id, subject, author, date, total_parts],
            )
            .await?;

        let mut rows = self
            .conn
            .query(
                "SELECT id FROM patchsets WHERE message_id = ?",
                libsql::params![message_id],
            )
            .await?;
        if let Ok(Some(row)) = rows.next().await {
            let id: i64 = row.get(0)?;
            Ok(id)
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
        body: &str,
        diff: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO patches (patchset_id, message_id, part_index, body, diff) VALUES (?, ?, ?, ?, ?)",
            libsql::params![patchset_id, message_id, part_index, body, diff]
        ).await?;
        Ok(())
    }

    pub async fn get_patchsets(&self, limit: usize, offset: usize) -> Result<Vec<PatchsetRow>> {
        let mut rows = self.conn.query(
            "SELECT id, message_id, subject, author, date, status FROM patchsets ORDER BY date DESC LIMIT ? OFFSET ?",
            libsql::params![limit as i64, offset as i64],
        ).await?;

        let mut patchsets = Vec::new();
        while let Ok(Some(row)) = rows.next().await {
            patchsets.push(PatchsetRow {
                id: row.get(0)?,
                message_id: row.get(1)?,
                subject: row.get(2).ok(),
                author: row.get(3).ok(),
                date: row.get(4).ok(),
                status: row.get(5).ok(),
            });
        }
        Ok(patchsets)
    }
}
