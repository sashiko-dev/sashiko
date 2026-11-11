//! Integration tests that spin up an in-memory HTTP server on an ephemeral port
//! and exercise the REST API.

use std::net::SocketAddr;
use std::sync::Arc;

use sashiko::db::Database;
use sashiko::events::Event;
use sashiko::fetcher::FetchRequest;
use sashiko::server::{ServerOptions, build_router};
use sashiko::settings::{DatabaseSettings, Settings};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// Build a minimal [`Settings`] for integration tests.
///
/// The `read_only` flag on `server` is set from the parameter; all other
/// fields use harmless defaults that don't require external resources.
fn test_settings(read_only: bool) -> Settings {
    let toml = format!(
        r#"
[database]
url = ":memory:"
token = ""

[nntp]
server = "localhost"
port = 119

[mailing_lists]
track = []

[ai]
provider = "gemini"
model = "test"

[server]
host = "127.0.0.1"
port = 0
read_only = {read_only}

[git]
repository_path = "."

[review]
concurrency = 1
worktree_dir = "/tmp/sashiko-test-trees"
timeout_seconds = 60
"#
    );
    let cfg = config::Config::builder()
        .add_source(config::File::from_str(&toml, config::FileFormat::Toml))
        .build()
        .expect("test settings parse");
    cfg.try_deserialize::<Settings>()
        .expect("test settings deserialize")
}

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
    spawn_test_server_with_settings(test_settings(read_only)).await
}

async fn spawn_test_server_with_settings(settings: Settings) -> TestServer {
    let db_settings = DatabaseSettings {
        url: ":memory:".to_string(),
        token: String::new(),
    };
    let db = Arc::new(Database::new(&db_settings).await.unwrap());
    db.migrate().await.unwrap();

    let (event_tx, event_rx) = mpsc::channel::<Event>(100);
    let (fetch_tx, _fetch_rx) = mpsc::channel::<FetchRequest>(100);

    let settings = Arc::new(settings);
    let app = build_router(
        settings,
        Arc::clone(&db),
        event_tx,
        fetch_tx,
        ServerOptions {
            allow_all_submit: true,
            dry_run: true,
            ..Default::default()
        },
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

#[tokio::test]
async fn test_prometheus_metrics_disabled_by_default() {
    let server = spawn_test_server(false).await;
    let response = reqwest::get(format!("{}/metrics", server.base_url))
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn test_prometheus_metrics_enabled() {
    let mut settings = test_settings(true);
    settings.server.prometheus_metrics_enabled = true;
    let server = spawn_test_server_with_settings(settings).await;
    let response = reqwest::get(format!("{}/metrics", server.base_url))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "text/plain; version=0.0.4; charset=utf-8"
    );
    let body = response.text().await.unwrap();
    let stats: serde_json::Value = reqwest::get(format!("{}/api/stats", server.base_url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for (metric, key) in [
        ("sashiko_pending_patches", "pending"),
        ("sashiko_reviewing_patches", "reviewing"),
        ("sashiko_messages", "messages"),
        ("sashiko_patchsets", "patchsets"),
        ("sashiko_repo_packs", "repo_packs"),
        ("sashiko_repo_pack_bytes", "repo_pack_bytes"),
    ] {
        assert!(
            body.lines()
                .any(|line| line.starts_with(&format!("# HELP {metric} ")))
        );
        assert!(
            body.lines()
                .any(|line| line == format!("# TYPE {metric} gauge"))
        );
        assert!(
            body.lines()
                .any(|line| line == format!("{metric} {}", stats[key]))
        );
    }
    for metric in [
        "sashiko_reviews_completed",
        "sashiko_reviews_failed",
        "sashiko_recent_bugs_analyzed",
        "sashiko_recent_bug_enrichment_stages",
        "sashiko_recent_bug_tokens_in",
        "sashiko_recent_bug_tokens_out",
        "sashiko_recent_bug_tokens_cached",
    ] {
        assert!(body.contains(&format!("# HELP {metric} ")));
        assert!(body.contains(&format!("# TYPE {metric} gauge\n{metric} 0\n")));
    }
    for metric in [
        "sashiko_recent_reviews",
        "sashiko_recent_review_tokens_in",
        "sashiko_recent_review_tokens_out",
        "sashiko_recent_review_tokens_cached",
    ] {
        assert!(body.contains(&format!("# HELP {metric} ")));
        assert!(body.contains(&format!("# TYPE {metric} gauge\n")));
        assert!(!body.contains(&format!("{metric}{{")));
    }
    assert!(body.ends_with('\n'));
}

#[tokio::test]
async fn test_prometheus_review_totals_share_json_cache() {
    let mut settings = test_settings(true);
    settings.server.prometheus_metrics_enabled = true;
    let server = spawn_test_server_with_settings(settings).await;
    server
        .db
        .conn
        .execute(
            "INSERT INTO patchsets (id, status, subject, author, date) \
             VALUES (1, 'Pending', 'Test', 'Author <a@b.com>', 1234567890)",
            (),
        )
        .await
        .unwrap();
    server
        .db
        .conn
        .execute(
            "INSERT INTO reviews (patchset_id, status, created_at) VALUES \
             (1, 'Reviewed', 1234567890), (1, 'Failed', 1234567890), \
             (1, 'Error', 1234567890), (1, 'Apply Failed', 1234567890), \
             (1, 'Pending', 1234567890), (1, 'In Review', 1234567890)",
            (),
        )
        .await
        .unwrap();

    let stats: serde_json::Value = reqwest::get(format!("{}/api/stats/reviews", server.base_url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stats["total_reviews"], 4);
    assert_eq!(stats["total_failures"], 3);

    // Reruns lower the database totals, but both endpoints share the cached snapshot.
    server
        .db
        .conn
        .execute("UPDATE reviews SET status = 'Pending'", ())
        .await
        .unwrap();
    let response = reqwest::get(format!("{}/metrics", server.base_url))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    for (metric, key) in [
        ("sashiko_reviews_completed", "total_reviews"),
        ("sashiko_reviews_failed", "total_failures"),
    ] {
        assert!(body.contains(&format!("# TYPE {metric} gauge\n{metric} {}\n", stats[key])));
    }
}

#[tokio::test]
async fn test_prometheus_metrics_fails_when_review_stats_unavailable() {
    let mut settings = test_settings(true);
    settings.server.prometheus_metrics_enabled = true;
    let server = spawn_test_server_with_settings(settings).await;
    server
        .db
        .conn
        .execute("DROP TABLE reviews", ())
        .await
        .unwrap();

    let response = reqwest::get(format!("{}/metrics", server.base_url))
        .await
        .unwrap();
    assert_eq!(response.status(), 500);
}

#[tokio::test]
async fn test_prometheus_recent_stats_and_label_escaping() {
    let mut settings = test_settings(true);
    settings.server.prometheus_metrics_enabled = true;
    let server = spawn_test_server_with_settings(settings).await;
    for sql in [
        "INSERT INTO patchsets (id, status, subject, author, date) \
         VALUES (1, 'Pending', 'Test', 'Author <a@b.com>', 1234567890)",
        "INSERT INTO ai_interactions (id, tokens_in, tokens_out, tokens_cached) \
         VALUES ('metrics-test', 10, 20, 5)",
        "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 1001) \
         INSERT INTO bugs (id, bugid, title, reporter, reported_at, created_at, updated_at) \
         SELECT x, 'sashiko-test-' || x, 'Test', 'test@example.org', 1, 1, 1 FROM cnt",
        "INSERT INTO bug_enrichments \
         (bug_id, kind, tool, created_at, tokens_in, tokens_out, tokens_cached) VALUES \
         (1, 'analysis', 'ai', 1, 999, 999, 999), \
         (2, 'analysis', 'ai', 1, 10, 20, 5), \
         (2, 'analysis', 'ai', 2, 7, 8, 2), \
         (3, 'analysis', 'ai', 1, NULL, NULL, NULL)",
    ] {
        server.db.conn.execute(sql, ()).await.unwrap();
    }
    server
        .db
        .conn
        .execute(
            "INSERT INTO reviews \
             (patchset_id, provider, model, status, interaction_id, created_at) \
             VALUES (1, ?1, ?2, ?3, 'metrics-test', 1234567890)",
            libsql::params!["test\"provider", "test\\model", "Reviewed\n\"\\status"],
        )
        .await
        .unwrap();

    let response = reqwest::get(format!("{}/metrics", server.base_url))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    let labels = r#"provider="test\"provider",model="test\\model",status="Reviewed\n\"\\status""#;
    for (name, value) in [
        ("sashiko_recent_reviews", 1),
        ("sashiko_recent_review_tokens_in", 10),
        ("sashiko_recent_review_tokens_out", 20),
        ("sashiko_recent_review_tokens_cached", 5),
    ] {
        assert!(
            body.lines()
                .any(|line| line == format!("{name}{{{labels}}} {value}")),
            "missing {name} with escaped labels in {body}"
        );
    }
    for (name, value) in [
        ("sashiko_recent_bugs_analyzed", 1),
        ("sashiko_recent_bug_enrichment_stages", 2),
        ("sashiko_recent_bug_tokens_in", 17),
        ("sashiko_recent_bug_tokens_out", 28),
        ("sashiko_recent_bug_tokens_cached", 7),
    ] {
        assert!(body.contains(&format!("# TYPE {name} gauge\n{name} {value}\n")));
    }
}

// ── Smoke Tests ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_empty_db_smoke_endpoints() {
    let server = spawn_test_server(false).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/api/stats", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert!(body["version"].is_string());

    for endpoint in ["patchsets", "messages"] {
        let resp = client
            .get(format!("{}/api/{endpoint}", server.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["total"], 0);
        assert!(body["items"].as_array().unwrap().is_empty());

        let page20 = client
            .get(format!("{}/api/{endpoint}?page=20", server.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(page20.status(), 200);

        let page21 = client
            .get(format!("{}/api/{endpoint}?page=21", server.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(
            page21.status(),
            403,
            "{endpoint}: unauthenticated deep pagination"
        );
    }

    let resp = client
        .get(format!("{}/api/lists", server.base_url))
        .send()
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
async fn test_submit_inject_validation_and_acceptance() {
    let mut server = spawn_test_server(false).await;
    let client = reqwest::Client::new();

    // 1. Reject payloads without a valid mbox header.
    let resp = client
        .post(format!("{}/api/submit", server.base_url))
        .json(&serde_json::json!({
            "type": "inject",
            "raw": "this is not an mbox",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // 2. Reject baselines that are not 40-hex-digit commit IDs.
    for baseline in [
        "v6.7-rc2",
        "origin/master",
        "1234567",
        "--upload-pack=sh",
        "1234567890ABCDEF1234567890ABCDEF12345678",
        "1234567890abcdef1234567890abcdef12345678\nbase-commit: x",
    ] {
        let resp = client
            .post(format!("{}/api/submit", server.base_url))
            .json(&serde_json::json!({
                "type": "inject",
                "raw": SAMPLE_MBOX,
                "base_commit": baseline,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "baseline {baseline:?} was accepted");
    }

    // 3. Accept a valid inject request and enqueue Event::RawMboxSubmitted.
    let resp = client
        .post(format!("{}/api/submit", server.base_url))
        .json(&serde_json::json!({
            "type": "inject",
            "raw": SAMPLE_MBOX,
            "base_commit": "1234567890abcdef1234567890abcdef12345678",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "accepted");

    let event = server
        .event_rx
        .try_recv()
        .expect("expected an event on the channel");
    match event {
        Event::RawMboxSubmitted { raw, baseline, .. } => {
            assert!(raw.contains("[PATCH] mm/slub"));
            assert_eq!(
                baseline.as_deref(),
                Some("1234567890abcdef1234567890abcdef12345678")
            );
        }
        other => panic!("expected RawMboxSubmitted, got {other:?}"),
    }
}

#[tokio::test]
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

// ── Database-Backed Query Tests ─────────────────────────────────────────

#[tokio::test]
async fn test_patchset_message_and_review_query_endpoints() {
    let server = spawn_test_server(false).await;
    let client = reqwest::Client::new();

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
            "INSERT INTO messages (message_id, subject, author, date, body) \
             VALUES ('<integ-1@example.com>', 'Test Subject', 'Author <a@b.com>', 1234567890, 'Test body')",
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

    let logs_json = "[{\"role\":\"user\",\"content\":\"test prompt\"}]";
    server
        .db
        .conn
        .execute(
            &format!(
                "INSERT INTO reviews (id, patchset_id, status, logs, created_at) \
                 VALUES (1, 1, 'Reviewed', '{logs_json}', 1234567890)"
            ),
            (),
        )
        .await
        .unwrap();

    let resp = client
        .get(format!("{}/api/patchsets", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    let resp = client
        .get(format!(
            "{}/api/message?id=<integ-1@example.com>",
            server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["subject"], "Test Subject");
    assert_eq!(body["body"], "Test body");

    let resp = client
        .get(format!("{}/api/review?id=1", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["logs"].as_str().unwrap(), logs_json);
}

/// The patchset page asks for its page size as limit=. Through the real
/// router, the size reaches the query on both patchset routes, per_page wins
/// when both are named, the size is bounded at 100, a value that is not a
/// number falls back to the default, and a repeated key is refused as a
/// repeated per_page always was.
#[tokio::test]
async fn test_patchset_page_size_reaches_the_query() {
    let server = spawn_test_server(false).await;
    let client = reqwest::Client::new();

    let tx = server.db.begin_immediate_transaction().await.unwrap();
    tx.execute(
        "INSERT INTO patchsets (id, status, subject, author, date, total_parts, received_parts) \
         VALUES (1, 'Pending', '[PATCH 0/120] big series', 'Author <a@b.com>', 1234567890, 120, 120)",
        (),
    )
    .await
    .unwrap();
    tx.execute(
        "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 120)
         INSERT INTO messages (message_id, subject, author, date, body)
         SELECT '<page-' || x || '@example.com>', 'part ' || x, 'Author <a@b.com>', 1234567890, ''
         FROM cnt",
        (),
    )
    .await
    .unwrap();
    tx.execute(
        "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 120)
         INSERT INTO patches (id, patchset_id, message_id, part_index)
         SELECT x, 1, '<page-' || x || '@example.com>', x FROM cnt",
        (),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let get = |route: &str, query: &str| {
        let url = format!(
            "{}/api/{route}?id=%3Cpage-1%40example.com%3E&{query}",
            server.base_url
        );
        let client = client.clone();
        async move { client.get(url).send().await.unwrap() }
    };
    for route in ["patch", "patchset"] {
        // Sequential numeric patchset IDs are rejected for unauthenticated callers.
        let forbidden = client
            .get(format!("{}/api/{route}?id=1", server.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(
            forbidden.status(),
            403,
            "{route}: unauthenticated numeric id"
        );

        for (query, expected) in [
            ("limit=7", 7),
            ("per_page=7", 7),
            ("per_page=9&limit=7", 9),
            ("limit=4294967295", 100),
            ("limit=abc", 50),
            ("page=1", 50),
        ] {
            let resp = get(route, query).await;
            assert_eq!(resp.status(), 200, "{route}?{query}");
            let body: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(body["limit"], expected, "{route}?{query}");
            // The rows, not only the echo: the size is what bounds the query.
            let patches = body["patches"].as_array().map(Vec::len);
            assert_eq!(patches, Some(expected), "{route}?{query} patches");
        }
        let resp = get(route, "limit=7&limit=9").await;
        assert_eq!(resp.status(), 400, "{route}: a repeated limit");
    }
}

#[tokio::test]
async fn test_stats_reviews_and_tools_endpoints() {
    let mut settings = test_settings(false);
    settings.server.prometheus_metrics_enabled = true;
    let server = spawn_test_server_with_settings(settings).await;
    let client = reqwest::Client::new();

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

    // Insert 1005 reviews (5 Failed first, then 1000 Reviewed) and tool usages in bulk.
    let tx = server.db.begin_immediate_transaction().await.unwrap();
    tx.execute(
        "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 5)
         INSERT INTO reviews (id, patchset_id, status, created_at)
         SELECT x, 1, 'Failed', 1234567890 + x FROM cnt",
        (),
    )
    .await
    .unwrap();
    tx.execute(
        "WITH RECURSIVE cnt(x) AS (SELECT 6 UNION ALL SELECT x + 1 FROM cnt WHERE x < 1005)
         INSERT INTO ai_interactions (id, tokens_in, tokens_out, tokens_cached)
         SELECT 'int-' || x, 10, 20, 5 FROM cnt",
        (),
    )
    .await
    .unwrap();
    tx.execute(
        "WITH RECURSIVE cnt(x) AS (SELECT 6 UNION ALL SELECT x + 1 FROM cnt WHERE x < 1005)
         INSERT INTO reviews (id, patchset_id, status, interaction_id, created_at)
         SELECT x, 1, 'Reviewed', 'int-' || x, 1234567890 + x FROM cnt",
        (),
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO tool_usages (review_id, tool_name, output_length)
         SELECT id, CASE WHEN id <= 5 THEN 'old_tool' ELSE 'new_tool' END,
                CASE WHEN id <= 5 THEN 100 ELSE 200 END
         FROM reviews WHERE id <= 10",
        (),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let resp = client
        .get(format!("{}/api/stats/reviews", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["total_reviews"], 1005);
    assert_eq!(body["total_failures"], 5);
    let reviews = body["reviews"].as_array().unwrap();
    assert_eq!(reviews.len(), 1);
    let group = &reviews[0];
    assert_eq!(group["status"], "Reviewed");
    assert_eq!(group["count"], 1000);
    assert_eq!(group["tokens_in"], 10000);
    assert_eq!(group["tokens_out"], 20000);
    assert_eq!(group["tokens_cached"], 5000);

    let metrics = client
        .get(format!("{}/metrics", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(metrics.status(), 200);
    let metrics = metrics.text().await.unwrap();
    for (name, value) in [
        ("sashiko_recent_reviews", 1000),
        ("sashiko_recent_review_tokens_in", 10000),
        ("sashiko_recent_review_tokens_out", 20000),
        ("sashiko_recent_review_tokens_cached", 5000),
    ] {
        assert!(metrics.contains(&format!(
            "{name}{{provider=\"\",model=\"\",status=\"Reviewed\"}} {value}\n"
        )));
    }
    assert!(!metrics.contains("status=\"Failed\""));
    assert!(metrics.contains("\nsashiko_reviews_failed 5\n"));

    let resp = client
        .get(format!("{}/api/stats/tools", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let tools = body.as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["tool"], "new_tool");
    assert_eq!(tools[0]["count"], 5);
    assert_eq!(tools[0]["avg_output_length"], 200.0);
}

// ── Redirect Tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_redirect_www_to_non_www() {
    let server = spawn_test_server(false).await;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    for (path, expected_location) in [
        ("", "https://sashiko.dev/"),
        (
            "/api/stats?foo=bar",
            "https://sashiko.dev/api/stats?foo=bar",
        ),
    ] {
        let resp = client
            .get(format!("{}{path}", server.base_url))
            .header("Host", "www.sashiko.dev")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 308);
        assert_eq!(
            resp.headers().get("Location").unwrap().to_str().unwrap(),
            expected_location
        );
    }
}
