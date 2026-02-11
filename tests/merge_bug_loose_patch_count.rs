use sashiko::db::Database;
use sashiko::patch::parse_email;
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
async fn test_merge_loose_patch_count() {
    common::setup_tracing();
    let db = setup_db().await;

    // Simulate loose patch subject
    let raw_1 = b"Message-ID: <msg1>
Subject: [PATCH] 1/2: Part 1
From: Author <author@example.com>
Date: Mon, 1 Jan 2024 10:00:00 -0000

Diff...";
    let (meta1, _) = parse_email(raw_1).unwrap();

    // Verify parsing logic works (Fix verification)
    assert_eq!(meta1.index, 1);
    assert_eq!(meta1.total, 2);

    let t1 = db.create_thread("msg1", "Part 1", 1000).await.unwrap();

    // 1. Ingest Part 1
    let ps1 = db
        .create_patchset(
            t1,
            None,
            "msg1",
            &meta1.subject,
            &meta1.author,
            meta1.date,
            meta1.total,
            0,
            "",
            "",
            None,
            meta1.index,
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();

    // 2. Ingest Part 2
    let raw_2 = b"Message-ID: <msg2>
Subject: [PATCH] 2/2: Part 2
From: Author <author@example.com>
Date: Mon, 1 Jan 2024 10:00:10 -0000

Diff...";
    let (meta2, _) = parse_email(raw_2).unwrap();

    assert_eq!(meta2.index, 2);
    assert_eq!(meta2.total, 2);

    let ps2 = db
        .create_patchset(
            t1,
            None,
            "msg2",
            &meta2.subject,
            &meta2.author,
            meta2.date,
            meta2.total,
            0,
            "",
            "",
            None,
            meta2.index,
            None,
            true,
        )
        .await
        .unwrap()
        .unwrap();

    // 3. Assert they MERGED
    assert_eq!(
        ps1, ps2,
        "Patchsets should merge given correct total count parsing"
    );
}
