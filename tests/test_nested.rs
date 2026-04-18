use sashiko::db::Database;
use sashiko::settings::DatabaseSettings;

#[tokio::test]
async fn test_get_completed_reviews_for_release_nested() {
    let settings = DatabaseSettings {
        url: "file:test_nested.db?mode=rwc".to_string(),
        token: "".to_string(),
    };
    let _ = std::fs::remove_file("test_nested.db");
    let db = Database::new(&settings).await.unwrap();
    db.migrate().await.unwrap();

    db.conn
        .execute(
            "INSERT INTO patchsets (id, status) VALUES (1, 'Reviewing')",
            (),
        )
        .await
        .unwrap();
    db.conn
        .execute("INSERT INTO messages (message_id) VALUES ('<msg1>')", ())
        .await
        .unwrap();
    db.conn.execute("INSERT INTO patches (id, patchset_id, message_id, part_index) VALUES (1, 1, '<msg1>', 1)", ()).await.unwrap();
    db.conn.execute(
        "INSERT INTO reviews (id, patchset_id, patch_id, status, inline_review, summary) VALUES (1, 1, 1, 'Reviewed', 'Inline', 'Summary')",
        (),
    ).await.unwrap();
    db.conn.execute(
        "INSERT INTO findings (review_id, severity, problem, severity_explanation) VALUES (1, 3, 'Problem 1', 'Explanation 1')",
        (),
    ).await.unwrap();

    // With 2 reviews, maybe the nested query breaks the outer statement state?
    db.conn.execute(
        "INSERT INTO reviews (id, patchset_id, patch_id, status, inline_review, summary) VALUES (2, 1, 1, 'Reviewed', 'Inline2', 'Summary2')",
        (),
    ).await.unwrap();

    let reviews = db.get_completed_reviews_for_release(1).await.unwrap();
    assert_eq!(reviews.len(), 2);
}
