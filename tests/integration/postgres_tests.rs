use anyhow::{Context, Result};
use futures::FutureExt;
use sashiko::db::{BugLifecycleStatus, Database, ListBugsParams, NewBug, NewBugEnrichment};
use sashiko::settings::StorageSettings;
use serde_json::json;

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
    let db = Database::postgres(&settings).await?;
    let result = std::panic::AssertUnwindSafe(exercise_daemon(&db))
        .catch_unwind()
        .await;
    db.execute_batch(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .await?;
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
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
        "discovered_in_patch_id": patch
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
