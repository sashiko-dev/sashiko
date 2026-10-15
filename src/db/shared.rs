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

//! Durable, versioned synchronization of a local bug mirror with PostgreSQL.

use super::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

const SYNC_BATCH: usize = 100;
const CATALOG_TIMEOUT: Duration = Duration::from_secs(300);
const MIRROR_SCHEMA_VERSION: u32 = 2;

/// Whether committed catalog updates are available for deduplication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharedSyncStatus {
    /// All changes through the captured catalog sequence have been imported.
    Ready,
    /// Another bounded pass must import remaining committed records.
    Importing,
}

pub(super) struct SharedCatalog {
    pub settings: crate::settings::StorageSettings,
    pub lock: tokio::sync::Mutex<()>,
    remote: tokio::sync::OnceCell<Database>,
}

impl SharedCatalog {
    pub(super) fn new(settings: crate::settings::StorageSettings) -> Self {
        Self {
            settings,
            lock: tokio::sync::Mutex::new(()),
            remote: tokio::sync::OnceCell::new(),
        }
    }

    async fn remote(&self) -> Result<&Database> {
        self.remote
            .get_or_try_init(|| async {
                let remote = Database::postgres(&self.settings).await?;
                remote.initialize_remote_catalog().await?;
                Ok::<_, anyhow::Error>(remote)
            })
            .await
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Shared bug catalog is unavailable")]
struct SharedCatalogUnavailable;

#[derive(Debug, thiserror::Error)]
#[error("Shared catalog has more records to import")]
struct SharedCatalogNotReady;

fn catalog_operation<T>(
    timeout: Duration,
    operation: impl std::future::Future<Output = Result<T>>,
) -> impl std::future::Future<Output = Result<T>> {
    let operation = Box::pin(operation);
    async move {
        let result = match tokio::time::timeout(timeout, operation).await {
            Ok(result) => result,
            Err(error) => Err(error.into()),
        };
        result.map_err(catalog_error)
    }
}

async fn next_queue_position(conn: &crate::storage::Connection) -> Result<i64> {
    let mut rows = conn
        .query(
            "UPDATE meta SET value=CAST(value AS INTEGER)+1
        WHERE key='shared_queue_sequence' RETURNING CAST(value AS INTEGER)",
            (),
        )
        .await?;
    Ok(rows
        .next()
        .await?
        .context("missing shared queue sequence")?
        .get(0)?)
}

fn retryable_postgres_error(error: &tokio_postgres::Error) -> bool {
    use tokio_postgres::error::SqlState;
    error.is_closed()
        || error.code().is_some_and(|code| {
            code.code().starts_with("08")
                || [
                    SqlState::ADMIN_SHUTDOWN,
                    SqlState::CRASH_SHUTDOWN,
                    SqlState::CANNOT_CONNECT_NOW,
                    SqlState::QUERY_CANCELED,
                    SqlState::TOO_MANY_CONNECTIONS,
                    SqlState::T_R_SERIALIZATION_FAILURE,
                    SqlState::T_R_DEADLOCK_DETECTED,
                ]
                .contains(code)
        })
}

fn catalog_error(error: anyhow::Error) -> anyhow::Error {
    if error.is::<SharedCatalogUnavailable>() {
        return error;
    }
    let retryable = error.chain().any(|cause| {
        if let Some(error) = cause.downcast_ref::<crate::storage::Error>() {
            match error {
                crate::storage::Error::Canceled => return true,
                crate::storage::Error::Postgres(error) => return retryable_postgres_error(error),
                _ => {}
            }
        }
        if let Some(error) = cause.downcast_ref::<tokio_postgres::Error>()
            && retryable_postgres_error(error)
        {
            return true;
        }
        if cause.is::<tokio::time::error::Elapsed>() || cause.is::<SharedCatalogNotReady>() {
            return true;
        }
        cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::NetworkDown
                    | std::io::ErrorKind::NetworkUnreachable
                    | std::io::ErrorKind::HostUnreachable
            )
        })
    });
    if retryable {
        error.context(SharedCatalogUnavailable)
    } else {
        error
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
struct Record {
    bugid: String,
    origin: String,
    title: String,
    triage: Triage,
    assignment: Assignment,
    analysis: Analysis,
    reporter: String,
    reported_at: i64,
    discovered_in_commit: Option<String>,
    source_ref: Option<String>,
    created_at: i64,
    updated_at: i64,
    subsystems: BTreeMap<String, SubsystemSource>,
    events: BTreeMap<String, Event>,
    vectors: BTreeMap<String, String>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
struct Triage {
    status: BugLifecycleStatus,
    duplicate_of: Option<String>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
struct Assignment {
    assignee: Option<String>,
    assigned_at: Option<i64>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
struct Analysis {
    state: BugPipelineState,
    severity: i64,
    verified: Option<String>,
    introduced: Option<String>,
    fixed: Option<String>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
struct Event {
    kind: String,
    tool: String,
    model: Option<String>,
    author: Option<String>,
    created_at: i64,
    content: Option<String>,
    data_json: Option<serde_json::Value>,
    tokens_in: Option<usize>,
    tokens_out: Option<usize>,
    tokens_cached: Option<usize>,
    logs: Option<String>,
}

struct MergeConflict {
    field: String,
    local: Option<serde_json::Value>,
    shared: Option<serde_json::Value>,
}

enum DuplicateChain {
    Valid,
    Unpublished,
    Cycle,
}

enum CatalogTarget {
    Missing,
    Canonical,
    Duplicate(String),
}

async fn catalog_target(
    conn: &crate::storage::Connection,
    project: &str,
    bugid: &str,
) -> Result<CatalogTarget> {
    let mut rows = conn
        .query(
            "SELECT payload::jsonb #>> '{triage,duplicate_of}'
        FROM shared_bug_records WHERE project=? AND bugid=?",
            libsql::params![project, bugid],
        )
        .await?;
    Ok(match rows.next().await? {
        None => CatalogTarget::Missing,
        Some(row) => match row.get::<Option<String>>(0)? {
            None => CatalogTarget::Canonical,
            Some(target) => CatalogTarget::Duplicate(target),
        },
    })
}

async fn shared_duplicate_chain(
    conn: &crate::storage::Connection,
    project: &str,
    record: &Record,
) -> Result<DuplicateChain> {
    let mut target = record.triage.duplicate_of.clone();
    let mut visited = BTreeSet::from([record.bugid.clone()]);
    while let Some(bugid) = target {
        if !visited.insert(bugid.clone()) {
            return Ok(DuplicateChain::Cycle);
        }
        target = match catalog_target(conn, project, &bugid).await? {
            CatalogTarget::Missing => return Ok(DuplicateChain::Unpublished),
            CatalogTarget::Canonical => None,
            CatalogTarget::Duplicate(target) => Some(target),
        };
    }
    Ok(DuplicateChain::Valid)
}

impl Record {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.bugid.is_empty() && !self.origin.is_empty(),
            "shared catalog bug identity or origin is missing"
        );
        anyhow::ensure!(
            (self.triage.status == BugLifecycleStatus::Duplicate)
                == self.triage.duplicate_of.is_some(),
            "invalid shared bug triage for {}",
            self.bugid
        );
        anyhow::ensure!(
            self.triage.duplicate_of.as_ref() != Some(&self.bugid),
            "shared bug cannot duplicate itself"
        );
        Ok(())
    }

    fn add_conflict(&mut self, conflict: MergeConflict, version: i64, instance: &str) {
        let field = conflict.field;
        let key = format!("conflict:{instance}:{}:{version}:{field}", self.bugid);
        self.events.entry(key).or_insert_with(|| Event {
            kind: "sync_conflict".into(),
            tool: "shared_catalog".into(),
            model: None,
            author: Some("system".into()),
            created_at: chrono::Utc::now().timestamp(),
            content: Some(format!(
                "Concurrent changes to {field}; kept the shared catalog value"
            )),
            data_json: Some(json!({"field":field,"local_version":version,"proposed":conflict.local,"kept":conflict.shared})),
            tokens_in: None,
            tokens_out: None,
            tokens_cached: None,
            logs: None,
        });
    }
}

impl Event {
    fn new_enrichment(&self) -> NewBugEnrichment {
        NewBugEnrichment {
            kind: self.kind.clone(),
            tool: self.tool.clone(),
            model: self.model.clone(),
            author: self.author.clone(),
            created_at: self.created_at,
            content: self.content.clone(),
            data_json: self.data_json.clone(),
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            tokens_cached: self.tokens_cached,
            logs: self.logs.clone(),
        }
    }
}

impl Database {
    pub async fn lock_bug_dedup(&self) -> Result<Option<crate::storage::Connection>> {
        if let Some(shared) = &self.shared {
            catalog_operation(CATALOG_TIMEOUT, async {
                let remote = shared.remote().await?;
                let guard = remote.conn.dedup_lock().await?;
                if self.sync_shared_bugs().await? == SharedSyncStatus::Importing {
                    return Err(SharedCatalogNotReady.into());
                }
                Ok(guard)
            })
            .await
        } else {
            self.conn.dedup_lock().await
        }
    }

    pub(crate) fn is_shared_catalog_failure(error: &anyhow::Error) -> bool {
        error.is::<SharedCatalogUnavailable>()
    }

    /// A committed outcome remains valid when publication must be retried.
    pub(crate) async fn publish_shared_bug_outcome(&self, bugid: &str) {
        if let Err(error) =
            catalog_operation(CATALOG_TIMEOUT, self.sync_shared_bugs_inner(Some(bugid))).await
        {
            tracing::warn!(
                "Bug {bugid} outcome is committed; catalog publication remains pending: {error:#}"
            );
        }
    }

    /// Return a claimed analysis to the queue without charging a catalog outage.
    pub(crate) async fn defer_bug_analysis_for_catalog(&self, id: i64, error: &str) -> Result<()> {
        let tx = self.begin_bug_write(id).await?;
        tx.execute(
            "UPDATE bugs SET
             attempt_count=CASE WHEN pipeline_state='running' AND attempt_count>0
                                THEN attempt_count-1 ELSE attempt_count END,
             pipeline_state=CASE WHEN pipeline_state='running' THEN 'pending' ELSE pipeline_state END,
             locked_by=NULL,lease_expires_at=NULL,last_error=?,updated_at=?,
             audit_author=?,audit_tool=?,audit_model=? WHERE id=?",
            libsql::params![error,chrono::Utc::now().timestamp(),self.bug_actor.as_str(),
                self.bug_tool.as_str(),self.bug_model.clone(),id],
        ).await?;
        tx.commit().await?;
        Ok(())
    }

    pub fn has_shared_bug_catalog(&self) -> bool {
        self.shared.is_some()
    }

    async fn initialize_remote_catalog(&self) -> Result<()> {
        let tx = self.begin_transaction().await?;
        let mut rows=tx.query("SELECT count(*) FROM information_schema.tables WHERE table_schema=current_schema() AND table_name='reviews'",()).await?;
        anyhow::ensure!(
            rows.next()
                .await?
                .context("missing schema check")?
                .get::<i64>(0)?
                == 0,
            "Use a separate PostgreSQL schema for a shared bug catalog and a full daemon database"
        );
        tx.execute_batch("CREATE TABLE IF NOT EXISTS shared_catalog_meta (key TEXT PRIMARY KEY,value TEXT NOT NULL)").await?;
        tx.execute(
            "INSERT INTO shared_catalog_meta VALUES ('identity',?) ON CONFLICT(key) DO NOTHING",
            [uuid::Uuid::new_v4().to_string()],
        )
        .await?;
        tx.execute(
            "INSERT INTO shared_catalog_meta VALUES ('format','1') ON CONFLICT(key) DO NOTHING",
            (),
        )
        .await?;
        let mut format = tx
            .query(
                "SELECT value FROM shared_catalog_meta WHERE key='format'",
                (),
            )
            .await?;
        anyhow::ensure!(
            format
                .next()
                .await?
                .context("missing catalog format")?
                .get::<String>(0)?
                == "1",
            "Unsupported shared catalog format"
        );
        tx.execute_batch("CREATE TABLE IF NOT EXISTS shared_bug_records (
            project TEXT NOT NULL, bugid TEXT NOT NULL,
            sequence BIGINT GENERATED ALWAYS AS IDENTITY,
            payload TEXT NOT NULL, PRIMARY KEY(project,bugid));
            CREATE INDEX IF NOT EXISTS shared_bug_records_sequence ON shared_bug_records(project,sequence)").await?;
        tx.commit().await?;
        Ok(())
    }

    pub(super) async fn initialize_shared_catalog(&self) -> Result<()> {
        let tx = self.begin_transaction().await?;
        let (version, was_enabled) = {
            let mut rows = tx.query(
                "SELECT key,value FROM meta WHERE key IN ('shared_schema_version','shared_enabled')", (),
            ).await?;
            let mut version = 0u32;
            let mut enabled = false;
            while let Some(row) = rows.next().await? {
                match row.get::<String>(0)?.as_str() {
                    "shared_schema_version" => version = row.get::<String>(1)?.parse()?,
                    "shared_enabled" => enabled = row.get::<String>(1)? == "1",
                    _ => {}
                }
            }
            (version, enabled)
        };
        anyhow::ensure!(
            version <= MIRROR_SCHEMA_VERSION,
            "Unsupported shared mirror schema version {version}"
        );
        if version < 1 {
            tx.execute_batch(include_str!("../migrations/shared_catalog/001_mirror.sql"))
                .await?;
        }
        if version < 2 {
            tx.execute_batch(include_str!("../migrations/shared_catalog/002_queue.sql"))
                .await?;
        }
        if version < MIRROR_SCHEMA_VERSION {
            tx.execute_batch(include_str!("../migrations/shared_catalog/triggers.sql"))
                .await?;
            tx.execute(
                "INSERT INTO meta (key,value) VALUES ('shared_schema_version',?)
                ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [MIRROR_SCHEMA_VERSION.to_string()],
            )
            .await?;
        }
        tx.execute("INSERT INTO meta (key,value) VALUES ('shared_queue_sequence','0') ON CONFLICT(key) DO NOTHING", ()).await?;
        tx.execute(
            "INSERT INTO meta (key,value) VALUES ('shared_instance',?) ON CONFLICT(key) DO NOTHING",
            [uuid::Uuid::new_v4().to_string()],
        )
        .await?;
        tx.execute("INSERT INTO meta (key,value) VALUES ('shared_importing','0') ON CONFLICT(key) DO NOTHING", ()).await?;
        tx.execute(
            "INSERT INTO meta (key,value) VALUES ('shared_cursor','0') ON CONFLICT(key) DO NOTHING",
            (),
        )
        .await?;
        tx.execute(
            "INSERT INTO meta (key,value) VALUES ('shared_enabled','1')
            ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            (),
        )
        .await?;
        let conflict = if was_enabled {
            "DO NOTHING"
        } else {
            "DO UPDATE SET local_version=local_version+1,dirty=1,queue_order=excluded.queue_order"
        };
        let position = next_queue_position(&tx).await?;
        tx.execute(&format!("INSERT INTO shared_bug_sync (bug_id, origin, local_version, dirty,queue_order)
            SELECT id, (SELECT value FROM meta WHERE key='shared_instance'), 1, 1, {position} FROM bugs WHERE true
            ON CONFLICT(bug_id) {conflict}"), ()).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Pending publications remain durable until PostgreSQL acknowledges them.
    pub async fn shared_bug_pending_ids(&self) -> Result<BTreeSet<i64>> {
        self.shared_pending_ids(None).await
    }

    /// Look up pending state only for the bugs included in an API response.
    pub async fn shared_bug_pending_ids_for(&self, bug_ids: &[i64]) -> Result<BTreeSet<i64>> {
        self.shared_pending_ids(Some(bug_ids)).await
    }

    async fn shared_pending_ids(&self, scope: Option<&[i64]>) -> Result<BTreeSet<i64>> {
        if self.shared.is_none() || scope.is_some_and(<[i64]>::is_empty) {
            return Ok(BTreeSet::new());
        }
        let mut sql = "SELECT s.bug_id FROM shared_bug_sync s WHERE s.dirty=1".to_string();
        let mut params = Vec::<libsql::Value>::new();
        if let Some(scope) = scope {
            sql.push_str(" AND s.bug_id IN (SELECT value FROM json_each(?))");
            params.push(serde_json::to_string(scope)?.into());
        }
        let mut rows = self.conn.query(&sql, params).await?;
        let mut ids = BTreeSet::new();
        while let Some(row) = rows.next().await? {
            ids.insert(row.get(0)?);
        }
        Ok(ids)
    }

    /// Producer-local IDs in foreign evidence cannot identify mirror reviews.
    pub(crate) async fn foreign_shared_events(&self, bug_ids: &[i64]) -> Result<BTreeSet<i64>> {
        if self.shared.is_none() || bug_ids.is_empty() {
            return Ok(BTreeSet::new());
        }
        let prefix = format!("{}:%", self.shared_meta("shared_instance").await?);
        let mut rows = self.conn.query(
            "SELECT e.id FROM bug_enrichments e JOIN shared_bug_events s ON s.enrichment_id=e.id
             WHERE e.bug_id IN (SELECT value FROM json_each(?)) AND s.event_key NOT LIKE ?",
            libsql::params![serde_json::to_string(bug_ids)?, prefix],
        ).await?;
        let mut foreign = BTreeSet::new();
        while let Some(row) = rows.next().await? {
            foreign.insert(row.get(0)?);
        }
        Ok(foreign)
    }

    /// Publish and import bounded batches. Pending local publications can remain
    /// queued; the result describes whether committed catalog changes are imported.
    pub async fn sync_shared_bugs(&self) -> Result<SharedSyncStatus> {
        catalog_operation(CATALOG_TIMEOUT, self.sync_shared_bugs_inner(None)).await
    }

    async fn sync_shared_bugs_inner(&self, priority: Option<&str>) -> Result<SharedSyncStatus> {
        let Some(shared) = &self.shared else {
            return Ok(SharedSyncStatus::Ready);
        };
        let _guard = shared.lock.lock().await;
        let remote = shared.remote().await?;
        let project = self.shared_meta("project").await?;
        let instance = self.shared_meta("shared_instance").await?;
        let mut rows = remote.conn.query(
            "SELECT i.value,f.value FROM shared_catalog_meta i JOIN shared_catalog_meta f ON f.key='format' WHERE i.key='identity'", (),
        ).await?;
        let row = rows.next().await?.context("missing catalog metadata")?;
        let identity: String = row.get(0)?;
        anyhow::ensure!(
            row.get::<String>(1)? == "1",
            "Unsupported shared catalog format"
        );
        let local = self.begin_transaction().await?;
        local.execute("INSERT INTO meta (key,value) VALUES ('shared_catalog_id',?) ON CONFLICT(key) DO NOTHING",[identity.as_str()]).await?;
        let mut rows = local
            .query("SELECT value FROM meta WHERE key='shared_catalog_id'", ())
            .await?;
        anyhow::ensure!(
            rows.next()
                .await?
                .context("missing local catalog identity")?
                .get::<String>(0)?
                == identity,
            "Shared catalog identity changed; use a separate local database for another catalog"
        );
        drop(rows);
        local.commit().await?;

        for id in self.shared_publication_ids(priority).await? {
            self.shared_publish_record(remote, id, &project, &instance)
                .await?;
            self.shared_note_attempt(id).await?;
        }
        let limit = if priority.is_some() { 0 } else { SYNC_BATCH };
        let importing = self
            .shared_import_batch(remote, &project, &instance, limit)
            .await?;
        Ok(if importing {
            SharedSyncStatus::Importing
        } else {
            SharedSyncStatus::Ready
        })
    }

    async fn shared_publish_record(
        &self,
        remote: &Database,
        id: i64,
        project: &str,
        instance: &str,
    ) -> Result<()> {
        let (local, base, version) = self.shared_snapshot(id, instance).await?;
        let tx = remote.begin_transaction().await?;
        let mut rows = tx
            .query(
                "SELECT payload FROM shared_bug_records WHERE project=? AND bugid=?",
                libsql::params![project, local.bugid.as_str()],
            )
            .await?;
        let prior: Option<Record> = rows
            .next()
            .await?
            .map(|r| r.get::<String>(0))
            .transpose()?
            .map(|s| serde_json::from_str(&s))
            .transpose()?;
        anyhow::ensure!(
            prior
                .as_ref()
                .is_none_or(|prior| prior.bugid == local.bugid),
            "catalog record identity mismatch"
        );
        let mut merged = merge_record(base.as_ref(), &local, prior.as_ref(), version, instance)?;
        merged.validate()?;
        // Validate relations under the catalog write lock. Independent
        // local decisions can otherwise form a cross-instance cycle.
        match shared_duplicate_chain(&tx, project, &merged).await? {
            DuplicateChain::Valid => {}
            DuplicateChain::Unpublished => {
                tx.rollback().await?;
                return Ok(());
            }
            DuplicateChain::Cycle => {
                let prior = prior
                    .as_ref()
                    .context("shared duplicate cycle without a prior record")?;
                let conflict = MergeConflict {
                    field: "triage".into(),
                    local: Some(serde_json::to_value(&merged.triage)?),
                    shared: Some(serde_json::to_value(&prior.triage)?),
                };
                merged.triage = prior.triage.clone();
                merged.analysis = prior.analysis.clone();
                merged.add_conflict(conflict, version, instance);
            }
        }
        if prior.as_ref() != Some(&merged) {
            tx.execute(
                "INSERT INTO shared_bug_records (project,bugid,payload) VALUES (?,?,?)
                ON CONFLICT(project,bugid) DO UPDATE SET payload=excluded.payload,
                sequence=DEFAULT",
                libsql::params![
                    project,
                    merged.bugid.as_str(),
                    serde_json::to_string(&merged)?
                ],
            )
            .await?;
        }
        tx.commit().await?;
        self.shared_import_targets(remote, &merged, project, instance)
            .await?;
        self.shared_accept(&merged, Some(version), instance).await?;
        Ok(())
    }

    async fn shared_import_batch(
        &self,
        remote: &Database,
        project: &str,
        instance: &str,
        limit: usize,
    ) -> Result<bool> {
        let mut cursor: i64 = self.shared_meta("shared_cursor").await?.parse()?;
        let mut rows = remote
            .conn
            .query(
                "SELECT coalesce(max(sequence),0) FROM shared_bug_records WHERE project=?",
                [project],
            )
            .await?;
        let high_water = rows
            .next()
            .await?
            .context("missing catalog sequence")?
            .get::<i64>(0)?;
        for _ in 0..limit {
            if cursor >= high_water {
                break;
            }
            let mut rows = remote.conn.query("SELECT sequence,payload,bugid FROM shared_bug_records WHERE project=? AND sequence>? AND sequence<=? ORDER BY sequence LIMIT 1", libsql::params![project,cursor,high_water]).await?;
            if let Some(row) = rows.next().await? {
                let next: i64 = row.get(0)?;
                let record: Record = serde_json::from_str(&row.get::<String>(1)?)?;
                anyhow::ensure!(
                    record.bugid == row.get::<String>(2)?,
                    "catalog record identity mismatch"
                );
                self.shared_import_targets(remote, &record, project, instance)
                    .await?;
                self.shared_accept(&record, None, instance).await?;
                cursor = next;
            } else {
                // Rows can move beyond the captured high water during this pass.
                cursor = high_water;
            }
            self.execute(
                "UPDATE meta SET value=? WHERE key='shared_cursor'",
                [cursor.to_string()],
            )
            .await?;
        }
        Ok(cursor < high_water)
    }

    async fn shared_publication_ids(&self, priority: Option<&str>) -> Result<Vec<i64>> {
        if let Some(bugid) = priority {
            let mut rows = self
                .conn
                .query("SELECT id,duplicate_of_id FROM bugs WHERE bugid=?", [bugid])
                .await?;
            let Some(row) = rows.next().await? else {
                return Ok(Vec::new());
            };
            let id: i64 = row.get(0)?;
            let mut ids: Vec<i64> = row.get::<Option<i64>>(1)?.into_iter().collect();
            ids.push(id);
            return Ok(ids);
        }
        let mut rows = self.conn.query("SELECT bug_id FROM shared_bug_sync WHERE dirty=1 ORDER BY queue_order,bug_id LIMIT ?", [SYNC_BATCH as i64]).await?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next().await? {
            ids.push(row.get(0)?);
        }
        Ok(ids)
    }

    async fn shared_note_attempt(&self, id: i64) -> Result<()> {
        let tx = self.begin_transaction().await?;
        let dirty = {
            let mut rows = tx
                .query(
                    "SELECT 1 FROM shared_bug_sync WHERE bug_id=? AND dirty=1",
                    [id],
                )
                .await?;
            rows.next().await?.is_some()
        };
        if dirty {
            let position = next_queue_position(&tx).await?;
            tx.execute(
                "UPDATE shared_bug_sync SET queue_order=? WHERE bug_id=?",
                libsql::params![position, id],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn shared_import_targets(
        &self,
        remote: &Database,
        record: &Record,
        project: &str,
        instance: &str,
    ) -> Result<()> {
        let mut target = record.triage.duplicate_of.clone();
        let mut visited = BTreeSet::from([record.bugid.clone()]);
        let mut missing = Vec::new();
        while let Some(bugid) = target {
            anyhow::ensure!(
                visited.insert(bugid.clone()),
                "shared catalog duplicate cycle at {bugid}"
            );
            if self.get_bug_by_bugid(&bugid).await?.is_some() {
                break;
            }
            target = match catalog_target(&remote.conn, project, &bugid).await? {
                CatalogTarget::Missing => {
                    anyhow::bail!("shared canonical bug {bugid} is not published yet")
                }
                CatalogTarget::Canonical => None,
                CatalogTarget::Duplicate(target) => Some(target),
            };
            missing.push(bugid);
        }
        for bugid in missing.into_iter().rev() {
            let mut rows = remote
                .conn
                .query(
                    "SELECT payload FROM shared_bug_records WHERE project=? AND bugid=?",
                    libsql::params![project, bugid.as_str()],
                )
                .await?;
            let row = rows
                .next()
                .await?
                .with_context(|| format!("shared canonical bug {bugid} is not published yet"))?;
            let record: Record = serde_json::from_str(&row.get::<String>(0)?)?;
            anyhow::ensure!(record.bugid == bugid, "catalog record identity mismatch");
            self.shared_accept(&record, None, instance).await?;
        }
        Ok(())
    }

    async fn shared_meta(&self, key: &str) -> Result<String> {
        let mut rows = self
            .conn
            .query("SELECT value FROM meta WHERE key=?", [key])
            .await?;
        rows.next()
            .await?
            .context("shared catalog metadata missing")?
            .get::<String>(0)
            .map_err(Into::into)
    }

    async fn shared_snapshot(
        &self,
        id: i64,
        instance: &str,
    ) -> Result<(Record, Option<Record>, i64)> {
        let tx = self.begin_transaction().await?;
        let db = self.with_connection((*tx).clone());
        let bug = db
            .get_bug(id)
            .await?
            .context("queued shared bug is missing")?;
        let mut rows = tx
            .query(
                "SELECT origin,local_version,base_json FROM shared_bug_sync WHERE bug_id=?",
                [id],
            )
            .await?;
        let row = rows.next().await?.context("missing publication state")?;
        let origin: String = row.get(0)?;
        let version = row.get(1)?;
        let base = row
            .get::<Option<String>>(2)?
            .map(|s| serde_json::from_str(&s))
            .transpose()?;
        let mut events = BTreeMap::new();
        let mut rows = tx.query("SELECT id,bug_id,kind,tool,model,author,created_at,content,data_json,tokens_in,tokens_out,tokens_cached,logs FROM bug_enrichments WHERE bug_id=? ORDER BY created_at,id",[id]).await?;
        while let Some(row) = rows.next().await? {
            let e = Self::parse_bug_enrichment_row(&row)?;
            tx.execute("INSERT INTO shared_bug_events (enrichment_id,event_key) VALUES (?,?) ON CONFLICT(enrichment_id) DO NOTHING",libsql::params![e.id,format!("{instance}:{:020}",e.id)]).await?;
            let mut keys = tx
                .query(
                    "SELECT event_key FROM shared_bug_events WHERE enrichment_id=?",
                    [e.id],
                )
                .await?;
            let key = keys
                .next()
                .await?
                .context("missing shared event key")?
                .get::<String>(0)?;
            events.insert(
                key,
                Event {
                    kind: e.kind,
                    tool: e.tool,
                    model: e.model,
                    author: e.author,
                    created_at: e.created_at,
                    content: e.content,
                    data_json: e.data_json,
                    tokens_in: e.tokens_in,
                    tokens_out: e.tokens_out,
                    tokens_cached: e.tokens_cached,
                    logs: e.logs,
                },
            );
        }
        let mut subsystems = BTreeMap::new();
        let mut rows = tx
            .query(
                "SELECT subsystem,source FROM bug_subsystems WHERE bug_id=?",
                [id],
            )
            .await?;
        while let Some(row) = rows.next().await? {
            subsystems.insert(
                row.get(0)?,
                serde_json::from_value(json!(row.get::<String>(1)?))?,
            );
        }
        let mut vectors = BTreeMap::new();
        let mut rows = tx
            .query(
                "SELECT model,vector_json FROM bug_vectors WHERE bug_id=?",
                [id],
            )
            .await?;
        while let Some(row) = rows.next().await? {
            vectors.insert(row.get(0)?, row.get(1)?);
        }
        let mut rows=tx.query("SELECT severity_int,verified_on_sha,introducing_commit_sha,fixed_in_commit_sha,(SELECT target.bugid FROM bugs target WHERE target.id=bugs.duplicate_of_id) FROM bugs WHERE id=?",[id]).await?;
        let row = rows
            .next()
            .await?
            .context("missing shared bug projection")?;
        let record = Record {
            bugid: bug.bugid,
            origin,
            title: bug.title,
            triage: Triage {
                status: bug.lifecycle_status,
                duplicate_of: row.get(4)?,
            },
            assignment: Assignment {
                assignee: bug.assignee,
                assigned_at: bug.assigned_at,
            },
            analysis: Analysis {
                state: bug.pipeline_state,
                severity: row.get(0)?,
                verified: row.get(1)?,
                introduced: row.get(2)?,
                fixed: row.get(3)?,
            },
            reporter: bug.reporter,
            reported_at: bug.reported_at,
            discovered_in_commit: bug.discovered_in_commit,
            source_ref: bug.source_ref,
            created_at: bug.created_at,
            updated_at: bug.updated_at,
            subsystems,
            events,
            vectors,
        };
        tx.commit().await?;
        Ok((record, base, version))
    }

    async fn shared_accept(
        &self,
        record: &Record,
        expected: Option<i64>,
        instance: &str,
    ) -> Result<()> {
        record.validate()?;
        let tx = self.begin_transaction().await?;
        let db = self.with_connection((*tx).clone());
        let existing = db.get_bug_by_bugid(&record.bugid).await?;
        if let Some(bug) = &existing {
            let mut rows = tx
                .query(
                    "SELECT local_version,dirty FROM shared_bug_sync WHERE bug_id=?",
                    [bug.id],
                )
                .await?;
            if let Some(row) = rows.next().await? {
                let version: i64 = row.get(0)?;
                let dirty: i64 = row.get(1)?;
                if dirty != 0 && expected != Some(version) {
                    // Keep the last accepted base until the current local
                    // version is merged. Advancing it here would turn remote
                    // changes into apparent local edits on the next retry.
                    tx.commit().await?;
                    return Ok(());
                }
            }
        }
        let duplicate = if let Some(target) = &record.triage.duplicate_of {
            Some(
                db.get_bug_by_bugid(target)
                    .await?
                    .with_context(|| {
                        format!("shared canonical bug {target} is not synchronized yet")
                    })?
                    .id,
            )
        } else {
            None
        };
        if let (Some(bug), Some(target)) = (&existing, duplicate) {
            let cycle = {
                let mut rows = tx.query("WITH RECURSIVE chain(id,duplicate_of_id) AS (
                    SELECT id,duplicate_of_id FROM bugs WHERE id=?
                    UNION SELECT b.id,b.duplicate_of_id FROM bugs b JOIN chain c ON b.id=c.duplicate_of_id
                ) SELECT 1 FROM chain WHERE id=? LIMIT 1", libsql::params![target,bug.id]).await?;
                rows.next().await?.is_some()
            };
            if cycle {
                // Revisit the incoming relation after the conflicting local
                // target is reconciled. Keep a valid local canonical meanwhile.
                let position = next_queue_position(&tx).await?;
                tx.execute("UPDATE shared_bug_sync SET dirty=1,local_version=local_version+1,queue_order=? WHERE bug_id=?", libsql::params![position,bug.id]).await?;
                tx.commit().await?;
                return Ok(());
            }
        }
        tx.execute("UPDATE meta SET value='1' WHERE key='shared_importing'", ())
            .await?;
        let id = if let Some(bug) = existing {
            bug.id
        } else {
            db.insert_bug(&NewBug {
                bugid: record.bugid.clone(),
                title: record.title.clone(),
                lifecycle_status: record.triage.status,
                pipeline_state: record.analysis.state,
                reporter: record.reporter.clone(),
                reported_at: record.reported_at,
                assignee: record.assignment.assignee.clone(),
                duplicate_of_id: duplicate,
                discovered_in_patchset_id: None,
                discovered_in_patch_id: None,
                discovered_in_commit: record.discovered_in_commit.clone(),
                source_ref: record.source_ref.clone(),
                vector_json: None,
                subsystems: Vec::new(),
            })
            .await?
        };
        tx.execute("UPDATE bugs SET title=?, lifecycle_status=?,duplicate_of_id=?,pipeline_state=?,
            assignee=?,assigned_at=?,severity_int=?,verified_on_sha=?,introducing_commit_sha=?,fixed_in_commit_sha=?,
            reporter=?,reported_at=?,discovered_in_commit=?,source_ref=?,created_at=?,updated_at=?,audit_author='system',audit_tool='shared_catalog',audit_model=NULL WHERE id=?",
            libsql::params![record.title.as_str(),record.triage.status.as_str(),duplicate,record.analysis.state.as_str(),
            record.assignment.assignee.clone(),record.assignment.assigned_at,record.analysis.severity,
            record.analysis.verified.clone(),record.analysis.introduced.clone(),record.analysis.fixed.clone(),
            record.reporter.as_str(),record.reported_at,record.discovered_in_commit.clone(),record.source_ref.clone(),record.created_at,record.updated_at,id]).await?;
        tx.execute("DELETE FROM bug_subsystems WHERE bug_id=?", [id])
            .await?;
        for (name, source) in &record.subsystems {
            tx.execute(
                "INSERT INTO bug_subsystems (bug_id,subsystem,source) VALUES (?,?,?)",
                libsql::params![id, name.as_str(), source.as_str()],
            )
            .await?;
        }
        let mut events: Vec<_> = record.events.iter().collect();
        events.sort_by_key(|(key, event)| (event.created_at, *key));
        for (key, event) in events {
            let mut rows = tx
                .query(
                    "SELECT enrichment_id FROM shared_bug_events WHERE event_key=?",
                    [key.as_str()],
                )
                .await?;
            if let Some(row) = rows.next().await? {
                let event_id: i64 = row.get(0)?;
                tx.execute("UPDATE bug_enrichments SET kind=?,tool=?,model=?,author=?,created_at=?,tokens_in=?,tokens_out=?,tokens_cached=?,content=?,data_json=?,logs=? WHERE id=? AND bug_id=?",libsql::params![
                    event.kind.as_str(),event.tool.as_str(),event.model.clone(),event.author.clone(),event.created_at,
                    event.tokens_in.map(|v|v as i64),event.tokens_out.map(|v|v as i64),event.tokens_cached.map(|v|v as i64),
                    event.content.as_deref().map(crate::compression::compress_string_if_needed).unwrap_or(libsql::Value::Null),
                    event.data_json.as_ref().map(serde_json::to_string).transpose()?,
                    event.logs.as_deref().map(crate::compression::compress_string_if_needed).unwrap_or(libsql::Value::Null),event_id,id]).await?;
            } else {
                let event_id = db
                    .insert_bug_enrichment_with_metadata(
                        id,
                        &event.new_enrichment(),
                        EnrichmentMetadata::Preserve,
                    )
                    .await?;
                tx.execute(
                    "INSERT INTO shared_bug_events (enrichment_id,event_key) VALUES (?,?)",
                    libsql::params![event_id, key.as_str()],
                )
                .await?;
            }
        }
        // Imports retain the original writers' audit events. Audit rows emitted
        // by replaying those same changes locally are redundant.
        tx.execute("DELETE FROM bug_enrichments WHERE bug_id=? AND id NOT IN (SELECT enrichment_id FROM shared_bug_events)",[id]).await?;
        tx.execute("DELETE FROM bug_vectors WHERE bug_id=?", [id])
            .await?;
        for (model, vector) in &record.vectors {
            tx.execute(
                "INSERT INTO bug_vectors (bug_id,model,vector_json,created_at) VALUES (?,?,?,?)",
                libsql::params![id, model.as_str(), vector.as_str(), record.updated_at],
            )
            .await?;
        }
        tx.execute("INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty,base_json) VALUES (?,?,0,0,?)
            ON CONFLICT(bug_id) DO UPDATE SET origin=excluded.origin,dirty=0,base_json=excluded.base_json",libsql::params![id,record.origin.as_str(),serde_json::to_string(record)?]).await?;
        tx.execute("UPDATE meta SET value='0' WHERE key='shared_importing'", ())
            .await?;
        tx.commit().await?;
        // Remote reports retain their producing instance as the analysis owner.
        // Review and patch foreign keys are never imported from another instance.
        let _ = instance;
        Ok(())
    }
}

fn merge_record(
    base: Option<&Record>,
    local: &Record,
    remote: Option<&Record>,
    version: i64,
    instance: &str,
) -> Result<Record> {
    let Some(remote) = remote else {
        return Ok(local.clone());
    };
    let mut conflicts = Vec::new();
    let base = base.map(serde_json::to_value).transpose()?;
    let local_value = serde_json::to_value(local)?;
    let remote_value = serde_json::to_value(remote)?;
    let merged = merge_value(
        base.as_ref(),
        Some(&local_value),
        Some(&remote_value),
        "",
        &mut conflicts,
    )
    .context("shared record disappeared")?;
    let mut result: Record = serde_json::from_value(merged)?;
    result.created_at = local.created_at.min(remote.created_at);
    result.updated_at = local.updated_at.max(remote.updated_at);
    for conflict in conflicts {
        result.add_conflict(conflict, version, instance);
    }
    Ok(result)
}

fn merge_value(
    base: Option<&serde_json::Value>,
    local: Option<&serde_json::Value>,
    remote: Option<&serde_json::Value>,
    path: &str,
    conflicts: &mut Vec<MergeConflict>,
) -> Option<serde_json::Value> {
    if local == base || local == remote {
        return remote.cloned();
    }
    if remote == base {
        return local.cloned();
    }
    if let (Some(serde_json::Value::Object(local)), Some(serde_json::Value::Object(remote))) =
        (local, remote)
        && !matches!(path, "triage" | "assignment" | "analysis")
    {
        let base = base.and_then(|v| v.as_object());
        let keys: BTreeSet<_> = local
            .keys()
            .chain(remote.keys())
            .chain(base.into_iter().flat_map(|m| m.keys()))
            .collect();
        let mut result = serde_json::Map::new();
        for key in keys {
            let next = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            if let Some(value) = merge_value(
                base.and_then(|m| m.get(key)),
                local.get(key),
                remote.get(key),
                &next,
                conflicts,
            ) {
                result.insert(key.clone(), value);
            }
        }
        return Some(serde_json::Value::Object(result));
    }
    if !matches!(path, "updated_at" | "created_at") {
        conflicts.push(MergeConflict {
            field: path.to_string(),
            local: local.cloned(),
            shared: remote.cloned(),
        });
    }
    remote.cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn local_mirror() -> Result<Database> {
        let mut db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".into(),
            token: String::new(),
        })
        .await?;
        db.shared = Some(Arc::new(SharedCatalog::new(Default::default())));
        db.migrate().await?;
        db.ensure_project_stamp(crate::project::ProjectId::Sashiko)
            .await?;
        Ok(db)
    }

    async fn local_report(db: &Database, title: &str) -> Result<i64> {
        let bug: NewBug = serde_json::from_value(json!({
            "bugid": format!("sashiko-{}", uuid::Uuid::new_v4()), "title": title
        }))?;
        db.create_bug(&bug).await
    }

    async fn acknowledge_local(db: &Database, id: i64) -> Result<()> {
        let instance = db.shared_meta("shared_instance").await?;
        let (record, _, version) = db.shared_snapshot(id, &instance).await?;
        db.shared_accept(&record, Some(version), &instance).await
    }

    #[tokio::test]
    async fn publication_batches_are_bounded_and_deferred_reports_do_not_starve_targets()
    -> Result<()> {
        let db = local_mirror().await?;
        let mut ids = Vec::new();
        for _ in 0..SYNC_BATCH + 5 {
            ids.push(local_report(&db, "queued").await?);
        }
        let first = db.shared_publication_ids(None).await?;
        assert_eq!(first.len(), SYNC_BATCH);
        for id in first {
            db.shared_note_attempt(id).await?;
        }
        let next = db.shared_publication_ids(None).await?;
        assert_eq!(&next[..5], &ids[SYNC_BATCH..]);
        let last = *ids.last().unwrap();
        let bug = db.get_bug(last).await?.unwrap();
        assert_eq!(
            db.shared_publication_ids(Some(&bug.bugid)).await?,
            vec![last]
        );
        Ok(())
    }

    #[tokio::test]
    async fn sync_deadline_covers_waiting_for_the_local_sync_mutex() -> Result<()> {
        let db = local_mirror().await?;
        let guard = db.shared.as_ref().unwrap().lock.lock().await;
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            catalog_operation(Duration::from_millis(10), db.sync_shared_bugs_inner(None)),
        )
        .await?;
        assert!(Database::is_shared_catalog_failure(
            &result.err().context("deadline")?
        ));
        drop(guard);
        assert!(db.shared.as_ref().unwrap().lock.try_lock().is_ok());
        Ok(())
    }

    #[tokio::test]
    async fn pending_local_triage_cannot_form_a_cycle_during_import() -> Result<()> {
        let db = local_mirror().await?;
        let left = local_report(&db, "left").await?;
        let right = local_report(&db, "right").await?;
        acknowledge_local(&db, left).await?;
        acknowledge_local(&db, right).await?;
        let instance = db.shared_meta("shared_instance").await?;
        let (mut incoming, _, _) = db.shared_snapshot(left, &instance).await?;
        let (canonical_right, _, _) = db.shared_snapshot(right, &instance).await?;
        db.mark_bug_as_duplicate(MarkDuplicateBugParams {
            ephemeral_id: right,
            canonical_id: left,
            reasoning: "pending local decision",
            ..Default::default()
        })
        .await?;
        incoming.triage = Triage {
            status: BugLifecycleStatus::Duplicate,
            duplicate_of: Some(canonical_right.bugid.clone()),
        };
        incoming.analysis.state = BugPipelineState::Succeeded;
        db.shared_accept(&incoming, None, &instance).await?;
        assert!(db.get_bug(left).await?.unwrap().duplicate_of_id.is_none());
        assert!(db.shared_bug_pending_ids().await?.contains(&left));
        let (_, _, version) = db.shared_snapshot(right, &instance).await?;
        db.shared_accept(&canonical_right, Some(version), &instance)
            .await?;
        let (_, _, version) = db.shared_snapshot(left, &instance).await?;
        db.shared_accept(&incoming, Some(version), &instance)
            .await?;
        assert_eq!(
            db.get_bug(left).await?.unwrap().duplicate_of_id,
            Some(right)
        );
        assert!(db.get_bug(right).await?.unwrap().duplicate_of_id.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn imports_preserve_metadata_without_inheriting_the_receiving_actor() -> Result<()> {
        let db = local_mirror().await?;
        let source = local_report(&db, "original report").await?;
        let instance = db.shared_meta("shared_instance").await?;
        let (mut record, _, _) = db.shared_snapshot(source, &instance).await?;
        record.bugid = format!("sashiko-{}", uuid::Uuid::new_v4());
        record.origin = uuid::Uuid::new_v4().to_string();
        record.reporter.clear();
        record.reported_at = 0;
        record.discovered_in_commit = Some("original-commit".into());
        record.source_ref = Some("original-reference".into());
        record.events = record
            .events
            .into_iter()
            .map(|(key, mut event)| {
                event.author = None;
                event.model = None;
                event.created_at = 0;
                (key.replacen(&instance, &record.origin, 1), event)
            })
            .collect();
        let receiver =
            db.with_bug_actor("receiver", "receiver-tool", Some("receiver-model".into()));
        for updated in [false, true] {
            if updated {
                record.reporter = "original-author".into();
                record.reported_at = 123;
                for event in record.events.values_mut() {
                    event.author = Some("peer-author".into());
                    event.model = Some("peer-model".into());
                    event.created_at = 12;
                    event.tokens_in = Some(42);
                }
            }
            receiver.shared_accept(&record, None, &instance).await?;
            let id = db.get_bug_by_bugid(&record.bugid).await?.unwrap().id;
            let (actual, _, _) = db.shared_snapshot(id, &instance).await?;
            assert_eq!(
                serde_json::to_value(actual)?,
                serde_json::to_value(&record)?
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn queue_migration_preserves_an_existing_version_one_mirror() -> Result<()> {
        let mut db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".into(),
            token: String::new(),
        })
        .await?;
        db.migrate().await?;
        let id = local_report(&db, "preexisting mirror").await?;
        db.conn
            .execute_batch(include_str!("../migrations/shared_catalog/001_mirror.sql"))
            .await?;
        let instance = uuid::Uuid::new_v4().to_string();
        db.execute("INSERT INTO meta (key,value) VALUES ('shared_schema_version','1'),('shared_enabled','1'),('shared_instance',?)", [instance.as_str()]).await?;
        db.execute(
            "INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty) VALUES (?,?,1,1)",
            libsql::params![id, instance.as_str()],
        )
        .await?;
        db.shared = Some(Arc::new(SharedCatalog::new(Default::default())));
        db.migrate().await?;
        db.ensure_project_stamp(crate::project::ProjectId::Sashiko)
            .await?;
        assert_eq!(
            db.shared_meta("shared_schema_version").await?,
            MIRROR_SCHEMA_VERSION.to_string()
        );
        assert_eq!(db.shared_meta("shared_instance").await?, instance);
        assert_eq!(db.shared_publication_ids(None).await?, vec![id]);
        Ok(())
    }

    #[tokio::test]
    async fn lease_and_housekeeping_updates_do_not_republish_reports() -> Result<()> {
        let db = local_mirror().await?;
        let id = local_report(&db, "bookkeeping").await?;
        db.claim_pending_bug("owner", 300, 3)
            .await?
            .context("claim")?;
        acknowledge_local(&db, id).await?;
        assert!(db.shared_bug_pending_ids().await?.is_empty());
        assert!(db.renew_bug_lease(id, "owner", 300).await?);
        assert!(db.shared_bug_pending_ids().await?.is_empty());
        db.execute("UPDATE bugs SET lifecycle_status='open',pipeline_state='succeeded',locked_by=NULL,lease_expires_at=NULL WHERE id=?", [id]).await?;
        acknowledge_local(&db, id).await?;
        assert_eq!(db.touch_open_bugs_fix_check_batch(&[id]).await?, 1);
        assert!(db.shared_bug_pending_ids().await?.is_empty());
        db.update_bug_title(id, "meaningful edit").await?;
        assert!(db.shared_bug_pending_ids().await?.contains(&id));
        Ok(())
    }

    #[tokio::test]
    async fn disabling_and_reenabling_catalog_tracking_preserves_local_edits() -> Result<()> {
        let mut db = local_mirror().await?;
        let id = local_report(&db, "before disable").await?;
        acknowledge_local(&db, id).await?;
        let shared = db.shared.take().context("shared mode")?;
        db.migrate().await?;
        db.update_bug_title(id, "edited in libSQL mode").await?;
        let mut rows = db
            .conn
            .query("SELECT dirty FROM shared_bug_sync WHERE bug_id=?", [id])
            .await?;
        assert_eq!(rows.next().await?.unwrap().get::<i64>(0)?, 0);
        drop(rows);
        db.shared = Some(shared);
        db.migrate().await?;
        assert!(db.shared_bug_pending_ids().await?.contains(&id));
        assert_eq!(
            db.get_bug(id).await?.unwrap().title,
            "edited in libSQL mode"
        );
        assert_eq!(
            db.shared_meta("shared_schema_version").await?,
            MIRROR_SCHEMA_VERSION.to_string()
        );
        db.execute(
            "UPDATE meta SET value='999' WHERE key='shared_schema_version'",
            (),
        )
        .await?;
        assert!(db.migrate().await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn fix_checks_and_lease_recovery_skip_foreign_mirrors() -> Result<()> {
        let db = local_mirror().await?;
        let own = local_report(&db, "origin-owned analysis").await?;
        db.execute("UPDATE bugs SET lifecycle_status='open',pipeline_state='succeeded',verified_on_sha='old' WHERE id=?", [own]).await?;
        let instance = db.shared_meta("shared_instance").await?;
        let (mut remote, _, _) = db.shared_snapshot(own, &instance).await?;
        remote.bugid = format!("sashiko-{}", uuid::Uuid::new_v4());
        remote.origin = uuid::Uuid::new_v4().to_string();
        remote.events.clear();
        db.shared_accept(&remote, None, &instance).await?;
        let foreign = db
            .get_bug_by_bugid(&remote.bugid)
            .await?
            .context("foreign mirror")?
            .id;
        let candidates = db.list_open_bugs_for_fix_check("new", None, 50).await?;
        assert_eq!(
            candidates.iter().map(|bug| bug.id).collect::<Vec<_>>(),
            vec![own]
        );
        assert!(
            db.claim_specific_open_bug_for_fix_check(foreign, "new", "owner", 300)
                .await?
                .is_none()
        );
        assert_eq!(
            db.claim_open_bug_for_fix_check("new", "owner", 300)
                .await?
                .context("owned fix check")?
                .id,
            own
        );
        assert!(
            db.claim_open_bug_for_fix_check("new", "owner", 300)
                .await?
                .is_none()
        );
        for id in [own, foreign] {
            db.execute("UPDATE bugs SET pipeline_state='running',attempt_count=3,locked_by=NULL,lease_expires_at=NULL WHERE id=?", [id]).await?;
        }
        assert_eq!(db.recover_stale_running_bugs().await?, 1);
        assert_eq!(
            db.get_bug(foreign).await?.unwrap().pipeline_state,
            BugPipelineState::Running
        );
        assert_eq!(db.abandon_exhausted_bugs(3).await?, 1);
        assert_eq!(
            db.get_bug(own).await?.unwrap().pipeline_state,
            BugPipelineState::Abandoned
        );
        assert_eq!(
            db.get_bug(foreign).await?.unwrap().pipeline_state,
            BugPipelineState::Running
        );
        Ok(())
    }

    #[tokio::test]
    async fn pending_state_is_limited_to_requested_bugs() -> Result<()> {
        let db = local_mirror().await?;
        let mut ids = Vec::new();
        for title in ["requested", "unrelated"] {
            let bug: NewBug = serde_json::from_value(json!({
                "bugid": format!("sashiko-{}", uuid::Uuid::new_v4()), "title": title
            }))?;
            ids.push(db.create_bug(&bug).await?);
        }
        assert_eq!(db.shared_bug_pending_ids().await?.len(), 2);
        assert_eq!(
            db.shared_bug_pending_ids_for(&[ids[0], -1]).await?,
            BTreeSet::from([ids[0]])
        );
        assert!(db.shared_bug_pending_ids_for(&[]).await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn catalog_failure_defers_claim_without_spending_attempts() -> Result<()> {
        let mut db = local_mirror().await?;
        Arc::get_mut(db.shared.as_mut().context("shared mode")?)
            .context("exclusive test configuration")?
            .settings
            .postgres_url = Some("postgresql://test@127.0.0.1:1/unavailable".into());
        let bug: NewBug = serde_json::from_value(json!({
            "bugid": format!("sashiko-{}", uuid::Uuid::new_v4()), "title": "catalog unavailable"
        }))?;
        let id = db.create_bug(&bug).await?;
        db.claim_pending_bug("owner", 300, 3)
            .await?
            .context("claim")?;
        let owner = db.with_bug_claim(id, "owner");
        let error = owner
            .lock_bug_dedup()
            .await
            .err()
            .context("catalog must be unavailable")?;
        assert!(Database::is_shared_catalog_failure(&error));
        assert!(
            db.with_bug_claim(id, "stale")
                .defer_bug_analysis_for_catalog(id, "outage")
                .await
                .is_err()
        );
        owner.defer_bug_analysis_for_catalog(id, "outage").await?;
        assert_eq!(
            db.get_bug(id).await?.unwrap().pipeline_state,
            BugPipelineState::Pending
        );
        assert!(!db.renew_bug_lease(id, "owner", 300).await?);
        let mut rows = db
            .conn
            .query("SELECT attempt_count FROM bugs WHERE id=?", [id])
            .await?;
        assert_eq!(rows.next().await?.unwrap().get::<i64>(0)?, 0);
        Ok(())
    }

    #[tokio::test]
    async fn catalog_configuration_errors_are_not_refunded_as_outages() -> Result<()> {
        let db = local_mirror().await?;
        let error = db
            .lock_bug_dedup()
            .await
            .err()
            .context("missing configuration")?;
        assert!(!Database::is_shared_catalog_failure(&error));
        for kind in [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::TimedOut,
        ] {
            assert!(Database::is_shared_catalog_failure(&catalog_error(
                std::io::Error::from(kind).into()
            )));
        }
        assert!(!Database::is_shared_catalog_failure(&catalog_error(
            anyhow::anyhow!("Unsupported shared catalog format")
        )));
        assert!(!Database::is_shared_catalog_failure(&catalog_error(
            std::io::Error::from(std::io::ErrorKind::NotFound).into()
        )));
        Ok(())
    }

    #[tokio::test]
    async fn failed_outcome_publication_keeps_success_and_releases_claim() -> Result<()> {
        let db = local_mirror().await?;
        let bug: NewBug = serde_json::from_value(json!({
            "bugid": format!("sashiko-{}", uuid::Uuid::new_v4()), "title": "committed outcome"
        }))?;
        let id = db.create_bug(&bug).await?;
        db.claim_pending_bug("owner", 300, 3)
            .await?
            .context("claim")?;
        let owner = db.with_bug_claim(id, "owner");
        owner
            .update_bug_outcome(
                id,
                UpdateBugOutcomeParams {
                    lifecycle_status: BugLifecycleStatus::Open,
                    inline_review: "saved report",
                    ..Default::default()
                },
            )
            .await?;
        owner.publish_shared_bug_outcome(&bug.bugid).await;
        owner.release_bug_lease(id).await?;
        let saved = db.get_bug(id).await?.unwrap();
        assert_eq!(saved.pipeline_state, BugPipelineState::Succeeded);
        assert_eq!(saved.lifecycle_status, BugLifecycleStatus::Open);
        assert_eq!(saved.inline_review(), "saved report");
        assert!(db.shared_bug_pending_ids_for(&[id]).await?.contains(&id));
        assert!(!db.renew_bug_lease(id, "owner", 300).await?);
        Ok(())
    }

    #[tokio::test]
    async fn imported_evidence_keeps_raw_input_without_foreign_review_links() -> Result<()> {
        let db = local_mirror().await?;
        let instance = db.shared_meta("shared_instance").await?;
        let bug: NewBug = serde_json::from_value(json!({
            "bugid": format!("sashiko-{}", uuid::Uuid::new_v4()), "title": "local discovery"
        }))?;
        let payload = json!({"patchset_id": 123, "patch_id": 456, "review_id": 789, "reasoning": "original evidence"});
        let id = db
            .create_bug_with_enrichment(
                &bug,
                Some(&NewBugEnrichment {
                    kind: "candidate".into(),
                    data_json: Some(payload.clone()),
                    ..Default::default()
                }),
            )
            .await?;
        let (mut record, _, _) = db.shared_snapshot(id, &instance).await?;
        let local = db.get_bug(id).await?.context("local discovery")?;
        let evidence = db.bug_evidence(&[local]).await?;
        assert_eq!(evidence["discoveries"][0]["patchset_id"], 123);
        assert_eq!(evidence["discoveries"][0]["patch_id"], 456);
        record.bugid = format!("sashiko-{}", uuid::Uuid::new_v4());
        record.origin = uuid::Uuid::new_v4().to_string();
        record.events = record
            .events
            .into_iter()
            .map(|(key, event)| (key.replacen(&instance, &record.origin, 1), event))
            .collect();
        db.shared_accept(&record, None, &instance).await?;
        let imported = db
            .get_bug_by_bugid(&record.bugid)
            .await?
            .context("imported discovery")?;
        assert_eq!(
            imported
                .enrichments
                .iter()
                .find(|e| e.kind == "candidate")
                .and_then(|e| e.data_json.as_ref()),
            Some(&payload)
        );
        let evidence = db.bug_evidence(&[imported]).await?;
        assert!(evidence["discoveries"][0]["patchset_id"].is_null());
        assert!(evidence["discoveries"][0]["patch_id"].is_null());
        Ok(())
    }

    #[tokio::test]
    async fn acknowledgement_keeps_merge_base_after_concurrent_local_edit() -> Result<()> {
        let mut db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".into(),
            token: String::new(),
        })
        .await?;
        db.shared = Some(Arc::new(SharedCatalog::new(Default::default())));
        db.migrate().await?;
        db.ensure_project_stamp(crate::project::ProjectId::Sashiko)
            .await?;
        let instance = db.shared_meta("shared_instance").await?;
        let bug: NewBug = serde_json::from_value(json!({
            "bugid": format!("sashiko-{}", uuid::Uuid::new_v4()),
            "title": "original title", "lifecycle_status": "open"
        }))?;
        let id = db.create_bug(&bug).await?;
        let (base, _, version) = db.shared_snapshot(id, &instance).await?;
        db.shared_accept(&base, Some(version), &instance).await?;
        db.set_bug_lifecycle_status(id, BugLifecycleStatus::Dismissed)
            .await?;
        let (local, base, version) = db.shared_snapshot(id, &instance).await?;
        let mut remote = base.clone().context("accepted catalog base")?;
        remote.triage.status = BugLifecycleStatus::Fixed;
        remote.title = "catalog title".into();
        let merged = merge_record(base.as_ref(), &local, Some(&remote), version, &instance)?;

        // Another local write commits while publication is awaiting its ack.
        db.add_bug_enrichment(
            id,
            &NewBugEnrichment {
                kind: "comment".into(),
                content: Some("concurrent evidence".into()),
                ..Default::default()
            },
        )
        .await?;
        db.shared_accept(&merged, Some(version), &instance).await?;
        let (pending, retry_base, retry_version) = db.shared_snapshot(id, &instance).await?;
        assert!(db.shared_bug_pending_ids().await?.contains(&id));
        assert!(retry_base == base);
        let retry = merge_record(
            retry_base.as_ref(),
            &pending,
            Some(&merged),
            retry_version,
            &instance,
        )?;
        assert_eq!(retry.triage.status, BugLifecycleStatus::Fixed);
        assert_eq!(retry.title, "catalog title");
        assert!(
            retry
                .events
                .values()
                .any(|e| e.content.as_deref() == Some("concurrent evidence"))
        );
        db.shared_accept(&retry, Some(retry_version), &instance)
            .await?;
        assert!(db.shared_bug_pending_ids().await?.is_empty());
        Ok(())
    }
}
