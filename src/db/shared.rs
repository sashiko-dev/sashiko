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

pub(super) struct SharedCatalog {
    pub settings: crate::settings::StorageSettings,
    pub lock: tokio::sync::Mutex<()>,
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
            let remote = Self::postgres(&shared.settings).await?;
            let guard = remote.conn.dedup_lock().await?;
            self.sync_shared_bugs().await?;
            Ok(guard)
        } else {
            self.conn.dedup_lock().await
        }
    }

    pub fn has_shared_bug_catalog(&self) -> bool {
        self.shared.is_some()
    }

    pub(super) async fn initialize_shared_catalog(&self) -> Result<()> {
        let tx = self.begin_transaction().await?;
        tx.execute_batch(include_str!("../migrations/014_shared_bug_mirror.sql"))
            .await?;
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
        tx.execute("INSERT INTO shared_bug_sync (bug_id, origin, local_version, dirty)
            SELECT id, (SELECT value FROM meta WHERE key='shared_instance'), 1, 1 FROM bugs WHERE true
            ON CONFLICT(bug_id) DO NOTHING", ()).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Pending publications remain durable until PostgreSQL acknowledges them.
    pub async fn shared_bug_pending_ids(&self) -> Result<BTreeSet<i64>> {
        if self.shared.is_none() {
            return Ok(BTreeSet::new());
        }
        let mut rows = self
            .conn
            .query("SELECT bug_id FROM shared_bug_sync WHERE dirty=1", ())
            .await?;
        let mut ids = BTreeSet::new();
        while let Some(row) = rows.next().await? {
            ids.insert(row.get(0)?);
        }
        Ok(ids)
    }

    /// Producer-local IDs in foreign evidence cannot identify mirror reviews.
    pub(super) async fn foreign_shared_events(&self, bug_ids: &[i64]) -> Result<BTreeSet<i64>> {
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

    /// Publish local mutations and import catalog updates. Retries are idempotent.
    pub async fn sync_shared_bugs(&self) -> Result<()> {
        let Some(shared) = &self.shared else {
            return Ok(());
        };
        let _guard = shared.lock.lock().await;
        let remote = Self::postgres(&shared.settings).await?;
        let project = self.shared_meta("project").await?;
        let instance = self.shared_meta("shared_instance").await?;
        let tx = remote.begin_transaction().await?;
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
        let mut rows = tx
            .query(
                "SELECT value FROM shared_catalog_meta WHERE key='identity'",
                (),
            )
            .await?;
        let identity = rows
            .next()
            .await?
            .context("missing catalog identity")?
            .get::<String>(0)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS shared_bug_records (
            project TEXT NOT NULL, bugid TEXT NOT NULL,
            sequence BIGINT GENERATED ALWAYS AS IDENTITY,
            payload TEXT NOT NULL, PRIMARY KEY(project,bugid));
            CREATE INDEX IF NOT EXISTS shared_bug_records_sequence ON shared_bug_records(project,sequence)").await?;
        tx.commit().await?;
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

        for id in self.shared_bug_pending_ids().await?.into_iter() {
            let (local, base, version) = self.shared_snapshot(id, &instance).await?;
            let tx = remote.begin_transaction().await?;
            let mut rows = tx
                .query(
                    "SELECT payload FROM shared_bug_records WHERE project=? AND bugid=?",
                    libsql::params![project.as_str(), local.bugid.as_str()],
                )
                .await?;
            let prior: Option<Record> = rows
                .next()
                .await?
                .map(|r| r.get::<String>(0))
                .transpose()?
                .map(|s| serde_json::from_str(&s))
                .transpose()?;
            let merged = merge_record(base.as_ref(), &local, prior.as_ref(), version, &instance)?;
            if prior.as_ref() != Some(&merged) {
                tx.execute(
                    "INSERT INTO shared_bug_records (project,bugid,payload) VALUES (?,?,?)
                    ON CONFLICT(project,bugid) DO UPDATE SET payload=excluded.payload,
                    sequence=DEFAULT",
                    libsql::params![
                        project.as_str(),
                        merged.bugid.as_str(),
                        serde_json::to_string(&merged)?
                    ],
                )
                .await?;
            }
            tx.commit().await?;
            self.shared_accept(&merged, Some(version), &instance)
                .await?;
        }

        let mut cursor: i64 = self.shared_meta("shared_cursor").await?.parse()?;
        let mut rows = remote
            .conn
            .query(
                "SELECT coalesce(max(sequence),0) FROM shared_bug_records WHERE project=?",
                [project.as_str()],
            )
            .await?;
        let high_water = rows
            .next()
            .await?
            .context("missing catalog sequence")?
            .get::<i64>(0)?;
        while cursor < high_water {
            let mut rows=remote.conn.query("SELECT sequence,payload FROM shared_bug_records WHERE project=? AND sequence>? AND sequence<=? ORDER BY sequence LIMIT 200",libsql::params![project.as_str(),cursor,high_water]).await?;
            let mut updates = Vec::new();
            let mut last = cursor;
            while let Some(row) = rows.next().await? {
                last = row.get(0)?;
                updates.push(serde_json::from_str::<Record>(&row.get::<String>(1)?)?);
            }
            if updates.is_empty() {
                break;
            }
            updates.sort_by_key(|r| r.triage.duplicate_of.is_some());
            for record in updates {
                self.shared_import_targets(&remote, &record, &project, &instance)
                    .await?;
                self.shared_accept(&record, None, &instance).await?;
            }
            self.execute(
                "UPDATE meta SET value=? WHERE key='shared_cursor'",
                [last.to_string()],
            )
            .await?;
            cursor = last;
        }
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
            target = record.triage.duplicate_of.clone();
            missing.push(record);
        }
        for record in missing.into_iter().rev() {
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
        let project = self.shared_meta("project").await?;
        anyhow::ensure!(
            record.bugid.starts_with(&format!("{project}-")) && !record.origin.is_empty(),
            "shared catalog project or origin mismatch"
        );
        anyhow::ensure!(
            (record.triage.status == BugLifecycleStatus::Duplicate)
                == record.triage.duplicate_of.is_some(),
            "invalid shared bug triage for {}",
            record.bugid
        );
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
            anyhow::ensure!(
                target != &record.bugid,
                "shared bug cannot duplicate itself"
            );
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
            created_at=?,updated_at=?,audit_author='system',audit_tool='shared_catalog',audit_model=NULL WHERE id=?",
            libsql::params![record.title.as_str(),record.triage.status.as_str(),duplicate,record.analysis.state.as_str(),
            record.assignment.assignee.clone(),record.assignment.assigned_at,record.analysis.severity,
            record.analysis.verified.clone(),record.analysis.introduced.clone(),record.analysis.fixed.clone(),
            record.created_at,record.updated_at,id]).await?;
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
                tx.execute("UPDATE bug_enrichments SET content=?, data_json=?, logs=? WHERE id=? AND bug_id=?",libsql::params![
                    event.content.as_deref().map(crate::compression::compress_string_if_needed).unwrap_or(libsql::Value::Null),
                    event.data_json.as_ref().map(serde_json::to_string).transpose()?,
                    event.logs.as_deref().map(crate::compression::compress_string_if_needed).unwrap_or(libsql::Value::Null),event_id,id]).await?;
            } else {
                let event_id = db
                    .insert_bug_enrichment(id, &event.new_enrichment())
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
        let field = conflict.field;
        let key = format!("conflict:{instance}:{}:{version}:{field}", local.bugid);
        result.events.entry(key).or_insert_with(|| Event {
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

    #[tokio::test]
    async fn imported_evidence_keeps_raw_input_without_foreign_review_links() -> Result<()> {
        let mut db = Database::new(&crate::settings::DatabaseSettings {
            url: ":memory:".into(),
            token: String::new(),
        })
        .await?;
        db.shared = Some(Arc::new(SharedCatalog {
            settings: Default::default(),
            lock: tokio::sync::Mutex::new(()),
        }));
        db.migrate().await?;
        db.ensure_project_stamp(crate::project::ProjectId::Sashiko)
            .await?;
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
        db.shared = Some(Arc::new(SharedCatalog {
            settings: Default::default(),
            lock: tokio::sync::Mutex::new(()),
        }));
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
