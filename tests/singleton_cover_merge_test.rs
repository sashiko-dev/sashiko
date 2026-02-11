use sashiko::db::Database;
use sashiko::settings::DatabaseSettings;
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

mod common;

#[tokio::test]
async fn test_cover_letter_merges_into_singleton() {
    common::setup_tracing();
    let db = setup_db().await;
    let msg_1 = "msg_1";
    let msg_0 = "msg_0";

    // Ensure thread exists
    let t1 = db.ensure_thread_for_message(msg_1, 1000).await.unwrap();

    // 1. Ingest Patch 1/1
    db.create_message(
        msg_1,
        t1,
        None,
        "Author",
        "[PATCH 1/1] Patch",
        1000,
        "body",
        "",
        "",
        None,
        None,
    )
    .await
    .unwrap();

    // cover_letter_id is itself because it's 1/1
    let ps_id_1 = db
        .create_patchset(
            t1,
            Some(msg_1),
            msg_1,
            "[PATCH 1/1] Patch",
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
        )
        .await
        .unwrap()
        .unwrap();

    db.create_patch(ps_id_1, msg_1, 1, "diff").await.unwrap();

    // Verify state
    let d1 = db.get_patchset_details(ps_id_1).await.unwrap().unwrap();
    // subject_index is not exposed in details JSON, so we skip checking it.
    assert_eq!(d1["message_id"].as_str(), Some(msg_1));

    // 2. Ingest Cover Letter 0/1
    db.create_message(
        msg_0,
        t1,
        None,
        "Author",
        "[PATCH 0/1] Cover",
        1005,
        "body",
        "",
        "",
        None,
        None,
    )
    .await
    .unwrap();

    // It claims to be cover letter (index 0).
    // It should find ps_id_1 and merge into it.
    let ps_id_0 = db
        .create_patchset(
            t1,
            Some(msg_0),
            msg_0,
            "[PATCH 0/1] Cover",
            "Author",
            1005,
            1,
            0,
            "",
            "",
            None,
            0,
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        ps_id_1, ps_id_0,
        "Cover letter should merge into existing patchset"
    );

    // Verify state
    let d2 = db.get_patchset_details(ps_id_1).await.unwrap().unwrap();
    // subject_index is not exposed in details JSON, so we skip checking it.
    assert_eq!(d2["message_id"].as_str(), Some(msg_0));
}
