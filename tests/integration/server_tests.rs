//! Integration tests that spin up a real HTTP server and exercise the API.
//!
//! These tests are marked `#[ignore]` so they only run via `make integration-test`
//! (i.e. `cargo test --release -- --ignored`). They are included in the tag-release
//! CI workflow but skipped during normal `make test` / PR checks.

use std::net::SocketAddr;
use std::sync::Arc;

use sashiko::api::build_router;
use sashiko::db::Database;
use sashiko::events::Event;
use sashiko::fetcher::FetchRequest;
use sashiko::settings::DatabaseSettings;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// A running test server instance with its base URL and background handles.
struct TestServer {
    /// Base URL including the OS-assigned port, e.g. `http://127.0.0.1:12345`.
    base_url: String,
    /// Shared database handle — tests can insert fixture data directly.
    db: Arc<Database>,
    /// Event receiver — tests can drain submitted events from the channel.
    event_rx: mpsc::Receiver<Event>,
}

/// Spawn a real axum server on a random port with an in-memory database.
///
/// The server runs in a background tokio task and is dropped when the
/// [`TestServer`] goes out of scope (the task is detached, so cleanup is
/// automatic when the tokio runtime shuts down).
async fn spawn_test_server(read_only: bool) -> TestServer {
    let db_settings = DatabaseSettings {
        url: ":memory:".to_string(),
        token: String::new(),
    };
    let db = Arc::new(Database::new(&db_settings).await.unwrap());
    db.migrate().await.unwrap();

    let (event_tx, event_rx) = mpsc::channel::<Event>(100);
    let (fetch_tx, _fetch_rx) = mpsc::channel::<FetchRequest>(100);

    let app = build_router(
        Arc::clone(&db),
        event_tx,
        fetch_tx,
        read_only,
        /* allow_all_submit */ true,
        /* smtp_enabled */ false,
        /* dry_run */ true,
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");

    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    TestServer {
        base_url,
        db,
        event_rx,
    }
}

// ── Smoke Tests ─────────────────────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn test_stats_endpoint_returns_ok() {
    let server = spawn_test_server(false).await;
    let resp = reqwest::get(format!("{}/api/stats", server.base_url))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert!(body["version"].is_string());
}

#[tokio::test]
#[ignore]
async fn test_patchsets_empty_on_fresh_db() {
    let server = spawn_test_server(false).await;
    let resp = reqwest::get(format!("{}/api/patchsets", server.base_url))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["total"], 0);
    assert!(body["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
#[ignore]
async fn test_messages_empty_on_fresh_db() {
    let server = spawn_test_server(false).await;
    let resp = reqwest::get(format!("{}/api/messages", server.base_url))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["total"], 0);
    assert!(body["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
#[ignore]
async fn test_lists_empty_on_fresh_db() {
    let server = spawn_test_server(false).await;
    let resp = reqwest::get(format!("{}/api/lists", server.base_url))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body.as_array().unwrap().is_empty());
}

// ── Submit / Inject Tests ───────────────────────────────────────────────

/// A minimal mbox-formatted kernel patch for testing ingestion.
const SAMPLE_MBOX: &str = "\
From dummy@example.com Thu May 14 12:00:00 2026
From: Test Author <test@example.com>
Date: Thu, 14 May 2026 12:00:00 +0000
Subject: [PATCH] mm/slub: fix object count in partial slab
Message-Id: <test-integration-1@example.com>

Fix an off-by-one in the partial slab object count that could lead
to an incorrect freelist walk under memory pressure.

---
 mm/slub.c | 2 +-
 1 file changed, 1 insertion(+), 1 deletion(-)

diff --git a/mm/slub.c b/mm/slub.c
index 1a2b3c4d5e6f..7a8b9c0d1e2f 100644
--- a/mm/slub.c
+++ b/mm/slub.c
@@ -100,7 +100,7 @@ static int count_partial_objects(struct kmem_cache_node *n)
 \tstruct slab *slab;
 \tint count = 0;
 
-\tlist_for_each_entry(slab, &n->partial, slab_list)
+\tlist_for_each_entry(slab, &n->partial, slab_list) {
 \t\tcount += slab->objects - slab->inuse;
 \t}
 
-- 
2.40.0
";

#[tokio::test]
#[ignore]
async fn test_submit_inject_accepted() {
    let mut server = spawn_test_server(false).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/submit", server.base_url))
        .json(&serde_json::json!({
            "type": "inject",
            "raw": SAMPLE_MBOX,
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "accepted");

    // The server should have enqueued an Event::RawMboxSubmitted on the channel.
    let event = server
        .event_rx
        .try_recv()
        .expect("expected an event on the channel");

    match event {
        Event::RawMboxSubmitted { raw, .. } => {
            assert!(raw.contains("[PATCH] mm/slub"));
        }
        other => panic!("expected RawMboxSubmitted, got {other:?}"),
    }
}

#[tokio::test]
#[ignore]
async fn test_submit_rejected_in_read_only_mode() {
    let server = spawn_test_server(/* read_only */ true).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/submit", server.base_url))
        .json(&serde_json::json!({
            "type": "inject",
            "raw": SAMPLE_MBOX,
        }))
        .send()
        .await
        .unwrap();

    // read_only mode should reject POST requests.
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
#[ignore]
async fn test_submit_rejects_empty_mbox() {
    let server = spawn_test_server(false).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/submit", server.base_url))
        .json(&serde_json::json!({
            "type": "inject",
            "raw": "this is not an mbox",
        }))
        .send()
        .await
        .unwrap();

    // The server should reject payloads without a valid mbox header.
    assert_eq!(resp.status(), 400);
}

// ── Database-Backed Query Tests ─────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn test_patchsets_returned_after_insert() {
    let server = spawn_test_server(false).await;

    // Insert a patchset directly via the DB so we can query it via HTTP.
    // The patchsets table requires subject/author/date for get_patchsets to return rows.
    server
        .db
        .conn
        .execute(
            "INSERT INTO patchsets (id, status, subject, author, date) \
             VALUES (1, 'Pending', '[PATCH] test patch', 'Author <a@b.com>', 1234567890)",
            (),
        )
        .await
        .unwrap();

    server
        .db
        .conn
        .execute(
            "INSERT INTO messages (message_id, subject, author, date) \
             VALUES ('<integ-1@example.com>', '[PATCH] test patch', 'Author <a@b.com>', 1234567890)",
            (),
        )
        .await
        .unwrap();

    server
        .db
        .conn
        .execute(
            "INSERT INTO patches (id, patchset_id, message_id, part_index) \
             VALUES (1, 1, '<integ-1@example.com>', 1)",
            (),
        )
        .await
        .unwrap();

    let resp = reqwest::get(format!("{}/api/patchsets", server.base_url))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["total"], 1);

    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
}

#[tokio::test]
#[ignore]
async fn test_message_details_via_api() {
    let server = spawn_test_server(false).await;

    server
        .db
        .conn
        .execute(
            "INSERT INTO messages (message_id, subject, author, date, body) \
             VALUES ('<detail-1@example.com>', 'Test Subject', 'Author <a@b.com>', 1234567890, 'Test body')",
            (),
        )
        .await
        .unwrap();

    let resp = reqwest::get(format!(
        "{}/api/message?id=<detail-1@example.com>",
        server.base_url
    ))
    .await
    .unwrap();

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["subject"], "Test Subject");
    assert_eq!(body["body"], "Test body");
}
