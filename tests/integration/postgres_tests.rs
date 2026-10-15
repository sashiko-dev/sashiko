use anyhow::{Context, Result};
use futures::FutureExt;
use sashiko::db::{BugLifecycleStatus, Database, ListBugsParams, NewBug, NewBugEnrichment};
use sashiko::settings::StorageSettings;
use serde_json::json;

fn test_settings() -> Result<sashiko::settings::Settings> {
    Ok(toml::from_str(
        "[ai]\nprovider='mock'\nmodel='test'\n[review]\nconcurrency=1\n",
    )?)
}

#[tokio::test]
#[ignore = "requires SASHIKO_TEST_POSTGRES_URL_FILE pointing to a disposable PostgreSQL database"]
async fn postgres_daemon_storage_roundtrip() -> Result<()> {
    let file = std::env::var("SASHIKO_TEST_POSTGRES_URL_FILE")
        .context("set the PostgreSQL test URL file")?;
    let schema = format!("test_{}", uuid::Uuid::new_v4().simple());
    let settings = StorageSettings {
        postgres_url: Some(std::fs::read_to_string(file)?.trim().to_string()),
        postgres_schema: schema.clone(),
        postgres_tls_require_only: true,
        ..Default::default()
    };
    let mut configured = test_settings()?;
    configured.storage = settings;
    configured.storage.mode = sashiko::settings::StorageMode::Postgresql;
    // A configured PostgreSQL client must ignore the legacy libSQL URL.
    configured.database.url = ":memory:".into();
    let db = Database::configured(&configured).await?;
    let result = std::panic::AssertUnwindSafe(async {
        exercise_daemon(&db).await?;
        exercise_connection_recovery(&db, &configured.storage).await?;
        exercise_schema_isolation(&db, &configured.storage).await
    })
    .catch_unwind()
    .await;
    db.execute_batch(&format!(
        "DROP SCHEMA IF EXISTS \"{schema}_independent\" CASCADE;
        DROP SCHEMA \"{schema}\" CASCADE"
    ))
    .await?;
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

async fn exercise_schema_isolation(db: &Database, settings: &StorageSettings) -> Result<()> {
    let mut settings = settings.clone();
    settings.postgres_schema.push_str("_independent");
    let independent = Database::postgres(&settings).await?;
    let tx = db.begin_transaction().await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        independent.conn.query("SELECT 1", ()),
    )
    .await
    .context("a transaction blocked an independent schema")??;
    tx.rollback().await?;
    let guard = db.lock_bug_dedup().await?;
    let independent_guard = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        independent.lock_bug_dedup(),
    )
    .await
    .context("deduplication blocked an independent schema")??;
    drop(independent_guard);
    drop(guard);
    Ok(())
}

async fn postgres_control(settings: &StorageSettings) -> Result<tokio_postgres::Client> {
    let mut config: tokio_postgres::Config = settings.postgres_url.as_ref().unwrap().parse()?;
    config.ssl_mode(tokio_postgres::config::SslMode::Require);
    let tls = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .danger_accept_invalid_hostnames(true)
        .build()?;
    let (client, driver) = config
        .connect(postgres_native_tls::MakeTlsConnector::new(tls))
        .await?;
    tokio::spawn(async move {
        let _ = driver.await;
    });
    Ok(client)
}

async fn backend_pid(conn: &sashiko::storage::Connection) -> Result<i32> {
    let mut rows = conn.query("SELECT pg_backend_pid()", ()).await?;
    Ok(i32::try_from(
        rows.next().await?.context("backend PID")?.get::<i64>(0)?,
    )?)
}

async fn exercise_connection_recovery(db: &Database, settings: &StorageSettings) -> Result<()> {
    let control = postgres_control(settings).await?;
    let original = backend_pid(&db.conn).await?;
    let tx = db.begin_transaction().await?;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            db.conn.query("SELECT 1", ()),
        )
        .await
        .is_err()
    );
    tx.rollback().await?;
    // Canceling a regular operation must discard its uncertain protocol state.
    assert_ne!(backend_pid(&db.conn).await?, original);
    let prior = backend_pid(&db.conn).await?;
    control
        .query_one("SELECT pg_terminate_backend($1,5000)", &[&prior])
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Ok(current) = backend_pid(&db.conn).await {
                assert_ne!(current, prior);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("regular PostgreSQL session did not recover")?;

    // Dedicated transactions must fail after losing their session. Reopening
    // here would execute a write outside the original transaction.
    let tx = db.begin_transaction().await?;
    let pid = backend_pid(&tx).await?;
    control
        .query_one("SELECT pg_terminate_backend($1,5000)", &[&pid])
        .await?;
    assert!(
        tx.execute("UPDATE bugs SET title='uncommitted reconnect write'", ())
            .await
            .is_err()
    );
    drop(tx);
    let (bugs, _) = db.list_bugs(ListBugsParams::default()).await?;
    assert!(
        bugs.iter()
            .all(|bug| bug.title != "uncommitted reconnect write")
    );

    // A lost lock session must also stay failed rather than silently replacing
    // the connection with one that does not own the advisory lock.
    let guard = db
        .lock_bug_dedup()
        .await?
        .context("PostgreSQL dedup guard")?;
    let pid = backend_pid(&guard).await?;
    control
        .query_one("SELECT pg_terminate_backend($1,5000)", &[&pid])
        .await?;
    assert!(backend_pid(&guard).await.is_err());
    drop(guard);
    let guard = db.lock_bug_dedup().await?;
    drop(guard);
    Ok(())
}

async fn exercise_daemon(db: &Database) -> Result<()> {
    db.migrate().await.context("migrate PostgreSQL")?;
    db.migrate().await.context("repeat PostgreSQL migration")?;
    db.ensure_project_stamp(sashiko::project::ProjectId::Sashiko)
        .await?;
    let thread = db.create_thread("pg-root", "PostgreSQL test", 1000).await?;
    db.create_message(
        "pg-message",
        thread,
        None,
        "Tester",
        "PostgreSQL test",
        1000,
        "body",
        "",
        "",
        None,
        None,
    )
    .await?;
    let patchset = db
        .create_patchset(
            thread,
            None,
            "pg-message",
            "PostgreSQL test",
            "Tester",
            1000,
            1,
            1,
            "",
            "",
            None,
            0,
            None,
            false,
            None,
            None,
        )
        .await?
        .context("create patchset")?;
    let patch = db
        .create_patch(patchset, "pg-message", 1, &"diff".repeat(1000))
        .await?;
    let review = db
        .create_review(patchset, Some(patch), "mock", "test", None, None)
        .await?;
    let new_bug: NewBug = serde_json::from_value(json!({
        "bugid": format!("sashiko-{}", uuid::Uuid::new_v4()),
        "title": "storage: candidate transaction",
        "discovered_in_patchset_id": patchset,
        "discovered_in_patch_id": patch,
        "subsystems": [{"name": "ÄTHERNET DRIVER", "source": "maintainers_section"}]
    }))?;
    let bug = db
        .create_bug_with_enrichment(
            &new_bug,
            Some(&NewBugEnrichment {
                kind: "candidate".into(),
                tool: "integration-test".into(),
                content: Some("evidence".repeat(500)),
                data_json: Some(json!({"proof": "stored"})),
                ..Default::default()
            }),
        )
        .await?;
    let loaded = db.get_bug(bug).await?.context("stored bug")?;
    assert_eq!(loaded.bugid, new_bug.bugid);
    assert!(loaded.enrichments.iter().any(|e| e.kind == "candidate" && e.content.as_deref() == Some(&"evidence".repeat(500))));
    for (scope, expected) in [("ÄTHERNET driver", 1), ("äTHERNET driver", 0)] {
        let scope = vec![scope.to_string()];
        let (_, total) = db
            .list_bugs(ListBugsParams {
                visibility: sashiko::db::BugVisibility::Sections(&scope),
                ..Default::default()
            })
            .await?;
        assert_eq!(total, expected);
        assert_eq!(
            db.get_subsystems_bug_counts(
                Some(BugLifecycleStatus::New),
                sashiko::db::BugVisibility::Sections(&scope)
            )
            .await?
            .len(),
            expected
        );
    }
    let claim = db
        .claim_pending_bug("postgres-test", 60, 3)
        .await?
        .context("claim pending bug")?;
    assert_eq!(claim.bugid, new_bug.bugid);
    assert!(
        db.claim_pending_bug("second-worker", 60, 3)
            .await?
            .is_none()
    );
    db.link_review_to_bug(review, bug, true).await?;
    db.link_review_to_bug(review, bug, false).await?;
    assert!(db.list_bugs_for_review(review).await?[0].1);
    db.set_bug_lifecycle_status(bug, BugLifecycleStatus::Open)
        .await?;
    db.add_bug_enrichment(
        bug,
        &NewBugEnrichment {
            kind: "severity_calibration".into(),
            data_json: Some(json!({"severity":"High"})),
            ..Default::default()
        },
    )
    .await?;
    assert_eq!(
        db.get_bug(bug).await?.unwrap().severity(),
        sashiko::db::Severity::High
    );
    db.add_bug_enrichment(
        bug,
        &NewBugEnrichment {
            kind: "verification".into(),
            data_json: Some(json!({"is_valid":true,"verified_on_sha":"abc123"})),
            ..Default::default()
        },
    )
    .await?;
    let duplicate: NewBug = serde_json::from_value(
        json!({"bugid":format!("sashiko-{}",uuid::Uuid::new_v4()),"title":"duplicate"}),
    )?;
    let duplicate_id = db.create_bug(&duplicate).await?;
    db.link_review_to_bug(review, duplicate_id, true).await?;
    assert!(
        db.mark_bug_as_duplicate(sashiko::db::MarkDuplicateBugParams {
            ephemeral_id: duplicate_id,
            canonical_id: bug,
            reasoning: "same cause",
            ..Default::default()
        })
        .await?
    );
    let (bugs, total) = db
        .list_bugs(ListBugsParams {
            visibility: sashiko::db::BugVisibility::Unrestricted,
            ..Default::default()
        })
        .await?;
    assert_eq!(total, 2);
    assert!(bugs.iter().any(|bug| bug.bugid == new_bug.bugid));
    db.get_patchset_details(patchset, None, None)
        .await?
        .context("patchset details")?;
    db.get_review_stats().await?;
    db.get_tool_usage_stats().await?;
    assert_eq!(db.get_patchsets(10, 0, None, None).await?.len(), 1);
    db.get_timeline_stats(None).await?;
    db.list_all_bugs_for_vector_search().await?;
    db.create_ai_interaction(sashiko::db::AiInteractionParams {
        id: "pg-ai",
        parent_id: None,
        workflow_id: None,
        provider: "mock",
        model: "test",
        input: &"input".repeat(1000),
        output: "output",
        tokens_in: 123,
        tokens_out: 45,
        tokens_cached: 6,
    })
    .await?;
    db.complete_review(review, "Reviewed", "done", None, Some("pg-ai"), None, None)
        .await?;
    let stats = db.get_review_stats().await?;
    assert_eq!(stats["reviews"][0]["tokens_in"], 123);
    let tx = db.begin_transaction().await?;
    tx.execute("UPDATE bugs SET title = 'rolled back' WHERE id = ?", [bug])
        .await?;
    tx.rollback().await?;
    assert_ne!(db.get_bug(bug).await?.unwrap().title, "rolled back");
    Ok(())
}

#[tokio::test]
#[ignore = "requires SASHIKO_TEST_POSTGRES_URL_FILE pointing to a disposable PostgreSQL database"]
async fn postgres_shared_catalog_between_instances() -> Result<()> {
    let file = std::env::var("SASHIKO_TEST_POSTGRES_URL_FILE")?;
    let directory = tempfile::tempdir()?;
    let mut settings = test_settings()?;
    settings.storage.mode = sashiko::settings::StorageMode::LibsqlSharedBugs;
    settings.storage.postgres_url = Some(std::fs::read_to_string(file)?.trim().to_string());
    settings.storage.postgres_schema = format!("test_{}", uuid::Uuid::new_v4().simple());
    settings.storage.postgres_tls_require_only = true;
    settings.database.url = directory
        .path()
        .join("first.db")
        .to_string_lossy()
        .to_string();
    let first = Database::configured(&settings).await?;
    first.migrate().await?;
    first
        .ensure_project_stamp(sashiko::project::ProjectId::Sashiko)
        .await?;
    let mut second_settings = settings.clone();
    let mut url = reqwest::Url::parse(second_settings.storage.postgres_url.as_deref().unwrap())?;
    url.query_pairs_mut().append_pair(
        "application_name",
        &format!("{}_mirror", settings.storage.postgres_schema),
    );
    second_settings.storage.postgres_url = Some(url.into());
    second_settings.database.url = directory
        .path()
        .join("second.db")
        .to_string_lossy()
        .to_string();
    let second = Database::configured(&second_settings).await?;
    second.migrate().await?;
    second
        .ensure_project_stamp(sashiko::project::ProjectId::Sashiko)
        .await?;
    let result = std::panic::AssertUnwindSafe(exercise_shared(&first, &second, &settings))
        .catch_unwind()
        .await;
    let cleanup = Database::postgres(&settings.storage).await?;
    cleanup
        .execute_batch(&format!(
            "DROP SCHEMA \"{}\" CASCADE",
            settings.storage.postgres_schema
        ))
        .await?;
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

async fn exercise_shared(
    first: &Database,
    second: &Database,
    settings: &sashiko::settings::Settings,
) -> Result<()> {
    let local_only: NewBug = serde_json::from_value(
        json!({"bugid":format!("sashiko-{}",uuid::Uuid::new_v4()),"title":"second instance","pipeline_state":"succeeded","lifecycle_status":"open"}),
    )?;
    second.create_bug(&local_only).await?;
    let bug: NewBug = serde_json::from_value(
        json!({"bugid":format!("sashiko-{}",uuid::Uuid::new_v4()),"title":"shared candidate"}),
    )?;
    let id = first
        .create_bug_with_enrichment(
            &bug,
            Some(&NewBugEnrichment {
                kind: "candidate".into(),
                content: Some("original evidence".into()),
                ..Default::default()
            }),
        )
        .await?;
    assert!(first.shared_bug_pending_ids().await?.contains(&id));
    let legacy: NewBug = serde_json::from_value(json!({
        "bugid": format!("linux-{}", uuid::Uuid::new_v4()),
        "title": "legacy report from another project",
        "pipeline_state": "succeeded", "lifecycle_status": "open"
    }))?;
    let legacy_id = first.create_bug(&legacy).await?;
    assert!(
        first
            .shared_bug_pending_ids_for(&[legacy_id])
            .await?
            .contains(&legacy_id)
    );
    first.sync_shared_bugs().await?;
    assert!(first.shared_bug_pending_ids().await?.is_empty());
    second.sync_shared_bugs().await?;
    assert!(first.get_bug(legacy_id).await?.is_some());
    assert!(second.get_bug_by_bugid(&legacy.bugid).await?.is_some());
    let mirror = second
        .get_bug_by_bugid(&bug.bugid)
        .await?
        .context("shared mirror")?;
    assert_ne!(mirror.id, id);
    assert!(
        mirror
            .enrichments
            .iter()
            .any(|e| e.content.as_deref() == Some("original evidence"))
    );
    assert!(
        second
            .claim_pending_bug("foreign-worker", 60, 3)
            .await?
            .is_none()
    );
    first
        .set_bug_lifecycle_status(id, BugLifecycleStatus::Open)
        .await?;
    first.sync_shared_bugs().await?;
    second.sync_shared_bugs().await?;
    first
        .set_bug_lifecycle_status(id, BugLifecycleStatus::Fixed)
        .await?;
    second
        .set_bug_lifecycle_status(mirror.id, BugLifecycleStatus::Dismissed)
        .await?;
    first.sync_shared_bugs().await?;
    second.sync_shared_bugs().await?;
    let updated = second.get_bug_by_bugid(&bug.bugid).await?.unwrap();
    assert_eq!(updated.lifecycle_status, BugLifecycleStatus::Fixed);
    assert!(
        updated
            .enrichments
            .iter()
            .any(|e| e.kind == "sync_conflict")
    );
    first.sync_shared_bugs().await?;
    let count = first.get_bug(id).await?.unwrap().enrichments.len();
    first.sync_shared_bugs().await?;
    assert_eq!(first.get_bug(id).await?.unwrap().enrichments.len(), count);
    let duplicate: NewBug = serde_json::from_value(
        json!({"bugid":format!("sashiko-{}",uuid::Uuid::new_v4()),"title":"duplicate report"}),
    )?;
    let duplicate_id = first.create_bug(&duplicate).await?;
    first
        .mark_bug_as_duplicate(sashiko::db::MarkDuplicateBugParams {
            ephemeral_id: duplicate_id,
            canonical_id: id,
            reasoning: "same public bug",
            ..Default::default()
        })
        .await?;
    first.sync_shared_bugs().await?;
    second.sync_shared_bugs().await?;
    let imported_duplicate = second.get_bug_by_bugid(&duplicate.bugid).await?.unwrap();
    assert_eq!(imported_duplicate.duplicate_of_id, Some(mirror.id));
    assert_shared_lock_exclusion(first, second, &settings.storage).await?;
    // A local bug write commits while the catalog is unreachable, then a new
    // daemon handle publishes the durable outbox after connectivity is restored.
    let mut offline = settings.clone();
    offline.database.url = std::path::Path::new(&settings.database.url)
        .with_file_name("offline.db")
        .to_string_lossy()
        .to_string();
    offline.storage.postgres_url = Some("postgresql://localhost:1/unavailable".into());
    let queued = Database::configured(&offline).await?;
    queued.migrate().await?;
    queued
        .ensure_project_stamp(sashiko::project::ProjectId::Sashiko)
        .await?;
    let pending: NewBug = serde_json::from_value(
        json!({"bugid":format!("sashiko-{}",uuid::Uuid::new_v4()),"title":"offline candidate"}),
    )?;
    queued.create_bug(&pending).await?;
    assert!(queued.sync_shared_bugs().await.is_err());
    drop(queued);
    offline.storage = settings.storage.clone();
    let resumed = Database::configured(&offline).await?;
    resumed.migrate().await?;
    resumed.sync_shared_bugs().await?;
    second.sync_shared_bugs().await?;
    assert!(second.get_bug_by_bugid(&pending.bugid).await?.is_some());
    exercise_shared_duplicate_races(first, second).await?;
    Ok(())
}

async fn assert_shared_lock_exclusion(
    first: &Database,
    second: &Database,
    settings: &StorageSettings,
) -> Result<()> {
    let guard = first.lock_bug_dedup().await?.context("held dedup guard")?;
    let pid = backend_pid(&guard).await?;
    let control = postgres_control(settings).await?;
    let second_name = format!("{}_mirror", settings.postgres_schema);
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 2] = [&pid, &second_name];
    let waiter = second.lock_bug_dedup();
    tokio::pin!(waiter);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            tokio::select! {
                acquired = &mut waiter => {
                    acquired?;
                    anyhow::bail!("second dedup guard acquired before the first was released");
                }
                row = control.query_one(
                    "SELECT EXISTS (
                       SELECT 1 FROM pg_catalog.pg_locks held
                       JOIN pg_catalog.pg_locks waiting
                         USING (locktype,database,classid,objid,objsubid)
                       JOIN pg_catalog.pg_stat_activity client ON client.pid=waiting.pid
                       WHERE held.pid=$1 AND held.locktype='advisory'
                         AND client.application_name=$2
                         AND held.granted AND NOT waiting.granted)", &params,
                ) => {
                    if row?.get::<_, bool>(0) {
                        return Ok::<_, anyhow::Error>(());
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("second connection never entered the server lock wait")??;
    drop(guard);
    let guard = tokio::time::timeout(std::time::Duration::from_secs(10), waiter)
        .await
        .context("second dedup guard did not acquire after release")??;
    drop(guard);
    Ok(())
}

async fn shared_report(db: &Database, title: &str) -> Result<(i64, String)> {
    let bugid = format!("sashiko-{}", uuid::Uuid::new_v4());
    let bug: NewBug = serde_json::from_value(json!({
        "bugid": bugid, "title": title,
        "pipeline_state": "succeeded", "lifecycle_status": "open"
    }))?;
    Ok((db.create_bug(&bug).await?, bugid))
}

async fn exercise_shared_duplicate_races(first: &Database, second: &Database) -> Result<()> {
    let (source, source_public) = shared_report(first, "acknowledgement source").await?;
    first.sync_shared_bugs().await?;
    second.sync_shared_bugs().await?;
    let source_mirror = second.get_bug_by_bugid(&source_public).await?.unwrap();
    let (target, target_public) = shared_report(second, "new remote target").await?;
    second.sync_shared_bugs().await?;
    second
        .mark_bug_as_duplicate(sashiko::db::MarkDuplicateBugParams {
            ephemeral_id: source_mirror.id,
            canonical_id: target,
            reasoning: "new shared target",
            ..Default::default()
        })
        .await?;
    second.sync_shared_bugs().await?;
    first
        .add_bug_enrichment(
            source,
            &NewBugEnrichment {
                kind: "comment".into(),
                content: Some("concurrent source comment".into()),
                ..Default::default()
            },
        )
        .await?;
    // Publishing the dirty source merges a target this mirror has not pulled.
    first.sync_shared_bugs().await?;
    let imported_target = first.get_bug_by_bugid(&target_public).await?.unwrap();
    let loaded = first.get_bug(source).await?.unwrap();
    assert_eq!(loaded.duplicate_of_id, Some(imported_target.id));
    assert!(
        loaded
            .enrichments
            .iter()
            .any(|e| e.content.as_deref() == Some("concurrent source comment"))
    );

    let (left, left_public) = shared_report(first, "left concurrent decision").await?;
    let (right, right_public) = shared_report(first, "right concurrent decision").await?;
    first.sync_shared_bugs().await?;
    second.sync_shared_bugs().await?;
    let mirror_left = second.get_bug_by_bugid(&left_public).await?.unwrap();
    let mirror_right = second.get_bug_by_bugid(&right_public).await?.unwrap();
    first
        .mark_bug_as_duplicate(sashiko::db::MarkDuplicateBugParams {
            ephemeral_id: left,
            canonical_id: right,
            reasoning: "first decision",
            ..Default::default()
        })
        .await?;
    second
        .mark_bug_as_duplicate(sashiko::db::MarkDuplicateBugParams {
            ephemeral_id: mirror_right.id,
            canonical_id: mirror_left.id,
            reasoning: "reciprocal decision",
            ..Default::default()
        })
        .await?;
    first.sync_shared_bugs().await?;
    second.sync_shared_bugs().await?;
    first.sync_shared_bugs().await?;
    for db in [first, second] {
        let left = db.get_bug_by_bugid(&left_public).await?.unwrap();
        let right = db.get_bug_by_bugid(&right_public).await?.unwrap();
        assert_eq!(left.duplicate_of_id, Some(right.id));
        assert_eq!(right.lifecycle_status, BugLifecycleStatus::Open);
        assert!(right.duplicate_of_id.is_none());
        assert!(
            right
                .enrichments
                .iter()
                .any(|event| event.kind == "sync_conflict")
        );
    }
    Ok(())
}
