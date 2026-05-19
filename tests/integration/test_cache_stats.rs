use sashiko::db::{Database, ReviewCacheStats};
use sashiko::settings::DatabaseSettings;

#[tokio::test]
async fn test_complete_review_with_cache_stats() {
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
    db.conn
        .execute(
            "INSERT INTO patches (id, patchset_id, message_id, part_index) VALUES (1, 1, '<msg1>', 1)",
            (),
        )
        .await
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO reviews (id, patchset_id, patch_id, status) VALUES (1, 1, 1, 'In Review')",
            (),
        )
        .await
        .unwrap();

    let cache_stats = ReviewCacheStats {
        hits: 5,
        misses: 2,
        tokens_saved: 10_000,
        tokens_stored: 3_000,
    };

    db.complete_review(
        1,
        "Reviewed",
        "Review completed",
        Some("Test summary"),
        None,
        None,
        None,
        Some(&cache_stats),
    )
    .await
    .unwrap();

    let mut rows = db
        .conn
        .query(
            "SELECT cache_hits, cache_misses, cache_tokens_saved, cache_tokens_stored FROM reviews WHERE id = 1",
            (),
        )
        .await
        .unwrap();

    let row = rows.next().await.unwrap().unwrap();
    assert_eq!(row.get::<i64>(0).unwrap(), 5);
    assert_eq!(row.get::<i64>(1).unwrap(), 2);
    assert_eq!(row.get::<i64>(2).unwrap(), 10_000);
    assert_eq!(row.get::<i64>(3).unwrap(), 3_000);
}

#[tokio::test]
async fn test_complete_review_without_cache_stats() {
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
    db.conn
        .execute(
            "INSERT INTO patches (id, patchset_id, message_id, part_index) VALUES (1, 1, '<msg1>', 1)",
            (),
        )
        .await
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO reviews (id, patchset_id, patch_id, status) VALUES (1, 1, 1, 'In Review')",
            (),
        )
        .await
        .unwrap();

    db.complete_review(
        1,
        "Reviewed",
        "Review completed",
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let mut rows = db
        .conn
        .query(
            "SELECT cache_hits, cache_misses FROM reviews WHERE id = 1",
            (),
        )
        .await
        .unwrap();

    let row = rows.next().await.unwrap().unwrap();
    let hits: Option<i64> = row.get(0).ok();
    let misses: Option<i64> = row.get(1).ok();
    assert!(
        hits.is_none(),
        "cache_hits should be NULL when no stats provided"
    );
    assert!(
        misses.is_none(),
        "cache_misses should be NULL when no stats provided"
    );
}

#[tokio::test]
async fn test_cache_stats_in_patchset_summary() {
    let settings = DatabaseSettings {
        url: ":memory:".to_string(),
        token: "".to_string(),
    };
    let db = Database::new(&settings).await.unwrap();
    db.migrate().await.unwrap();

    db.conn
        .execute(
            "INSERT INTO patchsets (id, status, subject, date) VALUES (1, 'Reviewed', 'test', 1000)",
            (),
        )
        .await
        .unwrap();
    db.conn
        .execute("INSERT INTO messages (message_id) VALUES ('<msg1>')", ())
        .await
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO patches (id, patchset_id, message_id, part_index) VALUES (1, 1, '<msg1>', 1)",
            (),
        )
        .await
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO reviews (id, patchset_id, patch_id, status) VALUES (1, 1, 1, 'In Review')",
            (),
        )
        .await
        .unwrap();

    let cache_stats = ReviewCacheStats {
        hits: 42,
        misses: 7,
        tokens_saved: 50_000,
        tokens_stored: 8_000,
    };

    db.complete_review(
        1,
        "Reviewed",
        "Review completed",
        Some("Test summary"),
        None,
        None,
        None,
        Some(&cache_stats),
    )
    .await
    .unwrap();

    let result = db.get_patchset_summary(1, None, None).await.unwrap();
    assert!(result.is_some(), "patchset summary should exist");
    let data = result.unwrap();

    let reviews = data["reviews"].as_array().expect("reviews should be array");
    assert!(!reviews.is_empty(), "should have at least one review");

    let review = &reviews[0];
    assert_eq!(review["cache_hits"], 42, "cache_hits mismatch");
    assert_eq!(review["cache_misses"], 7, "cache_misses mismatch");
    assert_eq!(
        review["cache_tokens_saved"], 50_000,
        "cache_tokens_saved mismatch"
    );
    assert_eq!(
        review["cache_tokens_stored"], 8_000,
        "cache_tokens_stored mismatch"
    );
}
