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
async fn test_merge_different_versions_should_fail() {
    common::setup_tracing();
    let db = setup_db().await;

    // 1. Create Thread
    let t1 = db.create_thread("root1", "Subject", 1000).await.unwrap();

    // Create messages
    db.create_message(
        "msg1",
        t1,
        None,
        "Author",
        "[PATCH] Fix something",
        1000,
        "body",
        "",
        "",
        None,
        None,
    )
    .await
    .unwrap();
    db.create_message(
        "msg2",
        t1,
        None,
        "Author",
        "[PATCH v2] Fix something",
        1010,
        "body",
        "",
        "",
        None,
        None,
    )
    .await
    .unwrap();

    // 2. Create Patchset v1 (Implicit)
    // [PATCH] Fix something
    // version: None
    let ps1 = db
        .create_patchset(
            t1,
            None,
            "msg1",
            "[PATCH] Fix something",
            "Author",
            1000,
            1, // total parts
            0,
            "",
            "",
            None, // version
            1,    // index
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();

    let _ = db.create_patch(ps1, "msg1", 1, "diff").await.unwrap();

    // 3. Create Patchset v2 (Explicit)
    // [PATCH v2] Fix something
    // Same author, very close time (10s later)
    // version: Some(2)
    let ps2 = db
        .create_patchset(
            t1,
            None,
            "msg2",
            "[PATCH v2] Fix something",
            "Author",
            1010,
            1, // total parts
            0,
            "",
            "",
            Some(2), // version
            1,       // index
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();

    // 4. Assert they are DIFFERENT (should NOT merge)
    assert_ne!(
        ps1, ps2,
        "Patchsets with different versions (v1 vs v2) should NOT merge even if close in time"
    );
}

#[tokio::test]
async fn test_merge_same_versions_should_merge() {
    let db = setup_db().await;

    // 1. Create Thread
    let t1 = db.create_thread("root2", "Subject", 2000).await.unwrap();

    // 2. Create Patchset v2 (Explicit)
    // [PATCH v2] Fix something
    let ps1 = db
        .create_patchset(
            t1,
            None,
            "msg3",
            "[PATCH v2] Fix something",
            "Author",
            2000,
            1,
            0,
            "",
            "",
            Some(2),
            1,
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();

    // 3. Create Patchset v2 (Explicit) - Resend or part of same series
    // [PATCH v2] Fix something
    let ps2 = db
        .create_patchset(
            t1,
            None,
            "msg4",
            "[PATCH v2] Fix something",
            "Author",
            2010,
            1,
            0,
            "",
            "",
            Some(2),
            1,
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();

    // 4. Assert they MERGED
    assert_eq!(
        ps1, ps2,
        "Patchsets with SAME version (v2 vs v2) SHOULD merge"
    );
}

#[tokio::test]
async fn test_merge_different_versions_series_should_fail() {
    let db = setup_db().await;

    // 1. Create Thread
    let t1 = db
        .create_thread("root3", "Subject Series", 3000)
        .await
        .unwrap();

    db.create_message(
        "msg5",
        t1,
        None,
        "Author",
        "[PATCH 1/2] Fix something",
        3000,
        "body",
        "",
        "",
        None,
        None,
    )
    .await
    .unwrap();
    db.create_message(
        "msg6",
        t1,
        None,
        "Author",
        "[PATCH v2 1/2] Fix something",
        3010,
        "body",
        "",
        "",
        None,
        None,
    )
    .await
    .unwrap();

    // 2. Create Patchset v1 (Implicit) - Part 1/2
    // [PATCH 1/2] Fix something
    let ps1 = db
        .create_patchset(
            t1,
            None,
            "msg5",
            "[PATCH 1/2] Fix something",
            "Author",
            3000,
            2, // total parts > 1
            0,
            "",
            "",
            None, // version
            1,    // index
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();

    let _ = db.create_patch(ps1, "msg5", 1, "diff").await.unwrap();

    // 3. Create Patchset v2 (Explicit) - Part 1/2
    // [PATCH v2 1/2] Fix something
    // Same author, close time
    let ps2 = db
        .create_patchset(
            t1,
            None,
            "msg6",
            "[PATCH v2 1/2] Fix something",
            "Author",
            3010,
            2, // total parts > 1
            0,
            "",
            "",
            Some(2), // version
            1,       // index
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();

    // 4. Assert they are DIFFERENT (should NOT merge)
    assert_ne!(
        ps1, ps2,
        "Patchsets (Series) with different versions (v1 vs v2) should NOT merge"
    );
}
