use sashiko::db::Database;
use sashiko::settings::DatabaseSettings;

#[tokio::test]
async fn test_get_message_details_by_msgid() {
    let settings = DatabaseSettings {
        url: ":memory:".to_string(),
        token: "".to_string(),
    };
    let db = Database::new(&settings).await.unwrap();
    db.migrate().await.unwrap();

    db.conn.execute(
        "INSERT INTO messages (message_id, thread_id, in_reply_to, author, subject, date, body, to_recipients, cc_recipients, git_blob_hash, mailing_list)
         VALUES ('<test1>', NULL, NULL, 'Author', 'Subject', 12345, 'Body', 'To', 'Cc', 'Hash', 'ML')",
        (),
    ).await.unwrap();

    let details = db.get_message_details_by_msgid("<test1>").await.unwrap();
    assert!(details.is_some());
    let details = details.unwrap();
    assert_eq!(details.message_id, "<test1>");
}

#[tokio::test]
async fn test_get_completed_reviews_for_release() {
    let settings = DatabaseSettings {
        url: ":memory:".to_string(),
        token: "".to_string(),
    };
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
    db.conn
        .execute(
            "INSERT INTO reviews (id, patchset_id, patch_id, status, inline_review, summary)
         VALUES (1, 1, 1, 'Reviewed', 'Inline', 'Summary')",
            (),
        )
        .await
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO findings (review_id, severity, problem, severity_explanation)
         VALUES (1, 3, 'Problem 1', 'Explanation 1')",
            (),
        )
        .await
        .unwrap();

    let reviews = db.get_completed_reviews_for_release(1).await.unwrap();
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].summary, "Summary");
    assert_eq!(reviews[0].findings.len(), 1);
}
