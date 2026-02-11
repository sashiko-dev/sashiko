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
async fn test_singleton_root_overwrite_prevention() {
    common::setup_tracing();
    let db = setup_db().await;
    let root_id = "root_msg";
    // Ensure thread and placeholder message exist
    let t1 = db.ensure_thread_for_message(root_id, 1000).await.unwrap();

    // Create message msg_2 (Part 2)
    db.create_message(
        "msg_2",
        t1,
        Some(root_id),
        "Author",
        "[PATCH 2/3] Part 2",
        1010,
        "body",
        "",
        "",
        None,
        None,
    )
    .await
    .unwrap();

    // 1. Ingest Part 2/3 first (Reply to Root)
    // This creates the patchset with inferred total=3, cover=root_id
    let ps_id = db
        .create_patchset(
            t1,
            Some(root_id), // Ingestor infers this from In-Reply-To
            "msg_2",
            "[PATCH 2/3] Part 2",
            "Author",
            1010,
            3,
            0,
            "",
            "",
            None,
            2,
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();

    // Add the patch
    db.create_patch(ps_id, "msg_2", 2, "diff").await.unwrap();

    // 2. Ingest Root (Singleton 1/1)
    // It has the ID `root_id`.
    // It claims total=1.
    // It should merge into ps_id (because of cover_letter_message_id match).
    // BUT it should NOT downgrade total_parts to 1.
    let ps_id_root = db
        .create_patchset(
            t1,
            Some(root_id), // It is its own cover? Or passed as such.
            root_id,
            "[PATCH] Singleton Root",
            "Author",
            1000, // Arrived earlier physically, but processed later
            1,    // Claims 1/1
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

    assert_eq!(ps_id, ps_id_root, "Root should merge into existing set");

    // Add root patch
    db.create_patch(ps_id, root_id, 1, "diff").await.unwrap();

    // 3. Verify Total Parts
    let details = db.get_patchset_details(ps_id).await.unwrap().unwrap();
    let total = details["total_parts"].as_u64().unwrap();
    let received = details["received_parts"].as_u64().unwrap();

    assert_eq!(received, 2, "Should have 2 patches");
    assert_eq!(total, 3, "Total parts should remain 3, not downgraded to 1");
}
