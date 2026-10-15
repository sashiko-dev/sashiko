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

//! PostgreSQL connection ownership for daemon storage.

use anyhow::{Context, Result};
use postgres_native_tls::MakeTlsConnector;
use tokio_postgres::Client;

/// A connected PostgreSQL client with its protocol driver running in Tokio.
pub struct PostgresStorage {
    client: Client,
}

impl PostgresStorage {
    /// Connect using the platform's trusted certificate authorities.
    pub async fn connect(url: &str) -> Result<Self> {
        let connector = native_tls::TlsConnector::new().context("create PostgreSQL TLS client")?;
        Self::connect_with_tls(url, connector).await
    }

    async fn connect_with_tls(url: &str, connector: native_tls::TlsConnector) -> Result<Self> {
        let (client, connection) = tokio_postgres::connect(url, MakeTlsConnector::new(connector))
            .await
            .context("connect to PostgreSQL")?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::warn!("PostgreSQL connection closed: {error}");
            }
        });
        Ok(Self { client })
    }

    /// Return the client for backend-specific migration and query code.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Create or upgrade the shared bug catalog under a migration lock.
    pub async fn migrate_shared_bugs(&mut self) -> Result<()> {
        let tx = self.client.transaction().await?;
        tx.query_one("SELECT pg_advisory_xact_lock($1)", &[&0x53415348494B4Fi64])
            .await?;
        tx.batch_execute(
            "CREATE TABLE IF NOT EXISTS sashiko_schema_migrations (
                scope TEXT NOT NULL,
                version INTEGER NOT NULL,
                applied_at BIGINT NOT NULL,
                PRIMARY KEY (scope, version)
            )",
        )
        .await?;
        let row = tx
            .query_opt(
                "SELECT max(version) FROM sashiko_schema_migrations WHERE scope = 'shared_bugs'",
                &[],
            )
            .await?;
        let version = row
            .and_then(|row| row.get::<_, Option<i32>>(0))
            .unwrap_or(0);
        if version < 1 {
            tx.batch_execute(include_str!("migrations/postgresql/001_shared_bugs.sql"))
                .await?;
            tx.execute(
                "INSERT INTO sashiko_schema_migrations (scope, version, applied_at)
                 VALUES ('shared_bugs', 1, extract(epoch from now())::bigint)",
                &[],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_to_configured_test_database() -> Result<()> {
        let Ok(url_file) = std::env::var("SASHIKO_TEST_POSTGRES_URL_FILE") else {
            return Ok(());
        };
        let url = std::fs::read_to_string(url_file)?;
        let mut builder = native_tls::TlsConnector::builder();
        // The disposable lab server uses an untrusted certificate. Production
        // connections use platform trust through PostgresStorage::connect.
        builder.danger_accept_invalid_certs(true);
        builder.danger_accept_invalid_hostnames(true);
        let mut storage = PostgresStorage::connect_with_tls(url.trim(), builder.build()?).await?;
        let row = storage
            .client()
            .query_one("SELECT current_database()", &[])
            .await?;
        let database: String = row.get(0);
        assert_eq!(database, "sashiko_agent_test_20261014");
        storage.migrate_shared_bugs().await?;
        storage.migrate_shared_bugs().await?;
        let row = storage
            .client()
            .query_one(
                "SELECT count(*) FROM sashiko_schema_migrations WHERE scope = 'shared_bugs'",
                &[],
            )
            .await?;
        assert_eq!(row.get::<_, i64>(0), 1);
        Ok(())
    }
}
