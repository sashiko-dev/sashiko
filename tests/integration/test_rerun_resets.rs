use sashiko::db::Database;
use sashiko::settings::DatabaseSettings;

#[tokio::test]
async fn test_rerun_resets_patch_statuses() {
    let settings = DatabaseSettings {
        url: ":memory:".to_string(),
        token: "".to_string(),
    };
    let db = Database::new(&settings).await.unwrap();
    db.migrate().await.unwrap();

    db.conn
        .execute(
            "INSERT INTO patchsets (id, status) VALUES (1, 'Reviewed')",
            (),
        )
        .await
        .unwrap();
    db.conn
        .execute("INSERT INTO messages (message_id) VALUES ('<msg1>')", ())
        .await
        .unwrap();
    db.conn
        .execute("INSERT INTO messages (message_id) VALUES ('<msg2>')", ())
        .await
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO patches (id, patchset_id, message_id, part_index, status, apply_error) \
             VALUES (1, 1, '<msg1>', 1, 'Failed', 'could not apply')",
            (),
        )
        .await
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO patches (id, patchset_id, message_id, part_index, status, apply_error) \
             VALUES (2, 1, '<msg2>', 2, 'Reviewed', NULL)",
            (),
        )
        .await
        .unwrap();

    db.rerun_patchset(1).await.unwrap();

    // Verify patchset status is reset
    let mut rows = db
        .conn
        .query("SELECT status FROM patchsets WHERE id = 1", ())
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    let status: String = row.get(0).unwrap();
    assert_eq!(status, "Pending");

    // Verify all patch statuses are reset and apply_error cleared
    let mut rows = db
        .conn
        .query(
            "SELECT status, apply_error FROM patches WHERE patchset_id = 1 ORDER BY id",
            (),
        )
        .await
        .unwrap();

    let row = rows.next().await.unwrap().unwrap();
    let status: String = row.get(0).unwrap();
    let apply_error: Option<String> = row.get(1).ok();
    assert_eq!(status, "Pending");
    assert!(apply_error.is_none(), "apply_error should be cleared");

    let row = rows.next().await.unwrap().unwrap();
    let status: String = row.get(0).unwrap();
    assert_eq!(status, "Pending");
}
