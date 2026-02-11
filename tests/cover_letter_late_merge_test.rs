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
async fn test_cover_letter_merges_into_full_patchset() {
    common::setup_tracing();
    let db = setup_db().await;

    // 1. Create Thread
    let t1 = db.create_thread("root1", "Subject", 1000).await.unwrap();

    // 2. Create Patch 1/2
    let patch1_msg_id = "msg_patch_1";
    db.create_message(
        patch1_msg_id,
        t1,
        None,
        "Author",
        "[PATCH 1/2] Fix something",
        1000,
        "body",
        "",
        "",
        None,
        None,
    )
    .await
    .unwrap();

    let ps_id = db
        .create_patchset(
            t1,
            None,
            patch1_msg_id,
            "[PATCH 1/2] Fix something",
            "Author",
            1000,
            2, // total parts
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

    db.create_patch(ps_id, patch1_msg_id, 1, "diff1")
        .await
        .unwrap();

    // 3. Create Patch 2/2
    let patch2_msg_id = "msg_patch_2";
    db.create_message(
        patch2_msg_id,
        t1,
        None,
        "Author",
        "[PATCH 2/2] Fix something",
        1005,
        "body",
        "",
        "",
        None,
        None,
    )
    .await
    .unwrap();

    let ps2_id = db
        .create_patchset(
            t1,
            None,
            patch2_msg_id,
            "[PATCH 2/2] Fix something",
            "Author",
            1005,
            2, // total parts
            0,
            "",
            "",
            None, // version
            2,    // index
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();

    assert_eq!(ps_id, ps2_id, "Patches should merge");
    db.create_patch(ps_id, patch2_msg_id, 2, "diff2")
        .await
        .unwrap();

    // Verify it is full
    let details = db.get_patchset_details(ps_id).await.unwrap().unwrap();
    assert_eq!(details["received_parts"].as_u64(), Some(2));
    assert_eq!(details["total_parts"].as_u64(), Some(2));

    // 4. Create Cover Letter [PATCH 0/2]
    // Should merge into existing patchset even though it is full
    let cover_msg_id = "msg_cover_0";
    db.create_message(
        cover_msg_id,
        t1,
        None,
        "Author",
        "[PATCH 0/2] Fix something",
        1010,
        "body",
        "",
        "",
        None,
        None,
    )
    .await
    .unwrap();

    let ps_cover_id = db
        .create_patchset(
            t1,
            Some(cover_msg_id), // cover letter id
            cover_msg_id,
            "[PATCH 0/2] Fix something",
            "Author",
            1010, // 10s later
            2,    // total parts
            0,
            "",
            "",
            None, // version
            0,    // index
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();

    // 5. Assert they MERGED
    assert_eq!(
        ps_id, ps_cover_id,
        "Cover letter should merge into existing full patchset"
    );

    // Verify cover letter ID was updated
    let details_updated = db.get_patchset_details(ps_id).await.unwrap().unwrap();
    assert_eq!(details_updated["message_id"].as_str(), Some(cover_msg_id));
}
