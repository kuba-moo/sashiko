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
use sashiko::settings::{DatabaseSettings, Settings};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// Build a minimal [`Settings`] for integration tests.
///
/// The `read_only` flag and `embargo_bypass_tokens` on `server` are set from the
/// parameters; all other fields use harmless defaults that don't require
/// external resources.
fn test_settings(read_only: bool, bypass_tokens: &[&str]) -> Settings {
    let tokens = bypass_tokens
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(", ");
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
embargo_bypass_tokens = [{tokens}]

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
    spawn_test_server_with_tokens(read_only, &[]).await
}

/// Like [`spawn_test_server`], with embargo-bypass tokens configured.
async fn spawn_test_server_with_tokens(read_only: bool, bypass_tokens: &[&str]) -> TestServer {
    let db_settings = DatabaseSettings {
        url: ":memory:".to_string(),
        token: String::new(),
    };
    let db = Arc::new(Database::new(&db_settings).await.unwrap());
    db.migrate().await.unwrap();

    let (event_tx, event_rx) = mpsc::channel::<Event>(100);
    let (fetch_tx, _fetch_rx) = mpsc::channel::<FetchRequest>(100);

    let settings = Arc::new(test_settings(read_only, bypass_tokens));
    let app = build_router(
        settings,
        Arc::clone(&db),
        event_tx,
        fetch_tx,
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

#[tokio::test]
#[ignore]
async fn test_stats_reviews_endpoint() {
    let server = spawn_test_server(false).await;

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

    // Insert 1005 reviews.
    // 5 Failed first, then 1000 Reviewed.
    server.db.begin_transaction().await.unwrap();
    for i in 1..=5 {
        server.db.conn.execute(
            &format!("INSERT INTO reviews (id, patchset_id, status, created_at) VALUES ({}, 1, 'Failed', {})", i, 1234567890 + i),
            ()
        ).await.unwrap();
    }
    for i in 6..=1005 {
        let int_id = format!("int-{}", i);
        server.db.conn.execute(
            &format!("INSERT INTO ai_interactions (id, tokens_in, tokens_out, tokens_cached) VALUES ('{}', 10, 20, 5)", int_id),
            ()
        ).await.unwrap();

        server.db.conn.execute(
            &format!("INSERT INTO reviews (id, patchset_id, status, interaction_id, created_at) VALUES ({}, 1, 'Reviewed', '{}', {})", i, int_id, 1234567890 + i),
            ()
        ).await.unwrap();
    }
    server.db.commit_transaction().await.unwrap();

    let resp = reqwest::get(format!("{}/api/stats/reviews", server.base_url))
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
}

#[tokio::test]
#[ignore]
async fn test_stats_tools_endpoint() {
    let server = spawn_test_server(false).await;

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

    server.db.begin_transaction().await.unwrap();
    for i in 1..=1005 {
        server.db.conn.execute(
            &format!("INSERT INTO reviews (id, patchset_id, status, created_at) VALUES ({}, 1, 'Reviewed', {})", i, 1234567890 + i),
            ()
        ).await.unwrap();
    }

    // Tool usages for reviews 1..5 (should be excluded)
    for i in 1..=5 {
        server.db.conn.execute(
            &format!("INSERT INTO tool_usages (review_id, tool_name, output_length) VALUES ({}, 'old_tool', 100)", i),
            ()
        ).await.unwrap();
    }

    // Tool usages for reviews 6..10 (should be included)
    for i in 6..=10 {
        server.db.conn.execute(
            &format!("INSERT INTO tool_usages (review_id, tool_name, output_length) VALUES ({}, 'new_tool', 200)", i),
            ()
        ).await.unwrap();
    }
    server.db.commit_transaction().await.unwrap();

    let resp = reqwest::get(format!("{}/api/stats/tools", server.base_url))
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
#[ignore]
async fn test_redirect_www_to_non_www() {
    let server = spawn_test_server(false).await;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let resp = client
        .get(&server.base_url)
        .header("Host", "www.sashiko.dev")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 308);
    assert_eq!(
        resp.headers().get("Location").unwrap().to_str().unwrap(),
        "https://sashiko.dev/"
    );
}

#[tokio::test]
#[ignore]
async fn test_redirect_www_to_non_www_with_path_and_query() {
    let server = spawn_test_server(false).await;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let resp = client
        .get(format!("{}/api/stats?foo=bar", server.base_url))
        .header("Host", "www.sashiko.dev")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 308);
    assert_eq!(
        resp.headers().get("Location").unwrap().to_str().unwrap(),
        "https://sashiko.dev/api/stats?foo=bar"
    );
}

#[tokio::test]
#[ignore]
async fn test_review_endpoint_returns_logs() {
    let server = spawn_test_server(false).await;

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

    let logs_json = "[{\"role\":\"user\",\"content\":\"test prompt\"}]";
    server
        .db
        .conn
        .execute(
            &format!("INSERT INTO reviews (id, patchset_id, status, logs, created_at) VALUES (1, 1, 'Reviewed', '{}', 1234567890)", logs_json),
            (),
        )
        .await
        .unwrap();

    let resp = reqwest::get(format!("{}/api/review?id=1", server.base_url))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["logs"].as_str().unwrap(), logs_json);
}

// ── Embargo Tests ────────────────────────────────────────────────────────

/// Far enough out that the handler's real clock always sees it as pending.
const FUTURE_EMBARGO: i64 = 4_102_444_800; // 2100-01-01

/// Seeds one embargoed, reviewed patchset with a cover letter and one patch.
async fn seed_embargoed_patchset(db: &Arc<Database>, embargo_until: Option<i64>) {
    let until = match embargo_until {
        Some(until) => until.to_string(),
        None => "NULL".to_string(),
    };
    // Messages first: patchsets and patches both carry a foreign key into them.
    db.conn
        .execute(
            "INSERT INTO messages (message_id, subject, author, date) VALUES \
               ('cover@example.com', '[PATCH 0/1] test series', 'Author <a@b.com>', 1234567890), \
               ('part1@example.com', '[PATCH 1/1] test patch', 'Author <a@b.com>', 1234567890)",
            (),
        )
        .await
        .unwrap();
    db.conn
        .execute(
            &format!(
                "INSERT INTO patchsets \
                   (id, status, subject, author, date, cover_letter_message_id, embargo_until) \
                 VALUES (1, 'Reviewed', '[PATCH 0/1] test series', 'Author <a@b.com>', \
                         1234567890, 'cover@example.com', {until})"
            ),
            (),
        )
        .await
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO patches (id, patchset_id, message_id, part_index) \
             VALUES (1, 1, 'part1@example.com', 1)",
            (),
        )
        .await
        .unwrap();
}

async fn stored_embargo_until(db: &Arc<Database>, id: i64) -> Option<i64> {
    let mut rows = db
        .conn
        .query(
            "SELECT embargo_until FROM patchsets WHERE id = ?",
            libsql::params![id],
        )
        .await
        .unwrap();
    rows.next()
        .await
        .unwrap()
        .unwrap()
        .get::<Option<i64>>(0)
        .unwrap()
}

#[tokio::test]
#[ignore]
async fn test_lift_embargo_by_patch_message_id() {
    let server = spawn_test_server(false).await;
    seed_embargoed_patchset(&server.db, Some(FUTURE_EMBARGO)).await;

    // A message-ID from inside the series, not the cover letter: that is what
    // an operator has at hand when reading the list.
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "{}/api/patchset/embargo/lift?id=part1@example.com",
            server.base_url
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "lifted");
    assert_eq!(body["id"], 1);
    assert_eq!(body["patchset_status"], "Reviewed");
    assert_eq!(body["was_embargoed_until"], FUTURE_EMBARGO);

    // Due, but still set: the release path only finds rows that hold a
    // non-NULL release time.
    let until = stored_embargo_until(&server.db, 1).await.unwrap();
    assert!(until < FUTURE_EMBARGO);
}

#[tokio::test]
#[ignore]
async fn test_lift_embargo_reports_unembargoed_patchset() {
    let server = spawn_test_server(false).await;
    seed_embargoed_patchset(&server.db, None).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "{}/api/patchset/embargo/lift?id=1",
            server.base_url
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "not_modified");
    assert!(body["reason"].as_str().unwrap().contains("not embargoed"));
    assert_eq!(stored_embargo_until(&server.db, 1).await, None);
}

#[tokio::test]
#[ignore]
async fn test_lift_embargo_unknown_patchset_is_not_found() {
    let server = spawn_test_server(false).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "{}/api/patchset/embargo/lift?id=absent@example.com",
            server.base_url
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
}

#[tokio::test]
#[ignore]
async fn test_lift_embargo_rejected_in_read_only_mode() {
    let server = spawn_test_server(/* read_only */ true).await;
    seed_embargoed_patchset(&server.db, Some(FUTURE_EMBARGO)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "{}/api/patchset/embargo/lift?id=1",
            server.base_url
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 403);
    assert_eq!(
        stored_embargo_until(&server.db, 1).await,
        Some(FUTURE_EMBARGO)
    );
}

// ── Token-authorized embargo lift ───────────────────────────────────────
//
// Every server here is spawned with `allow_all_submit` true, so a token
// endpoint that fell back to the origin check the way the localhost one does
// would answer 200 to the unauthorized cases below rather than 401.

const BYPASS_TOKEN: &str = "s3cret-lift-token";

#[tokio::test]
#[ignore]
async fn test_token_lift_embargo_with_bearer_token() {
    let server = spawn_test_server_with_tokens(false, &[BYPASS_TOKEN]).await;
    seed_embargoed_patchset(&server.db, Some(FUTURE_EMBARGO)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "{}/api/patchset/embargo/lift-token?id=part1@example.com",
            server.base_url
        ))
        .bearer_auth(BYPASS_TOKEN)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "lifted");
    assert_eq!(body["id"], 1);
    assert_eq!(body["patchset_status"], "Reviewed");
    assert_eq!(body["was_embargoed_until"], FUTURE_EMBARGO);

    let until = stored_embargo_until(&server.db, 1).await.unwrap();
    assert!(until < FUTURE_EMBARGO);
}

#[tokio::test]
#[ignore]
async fn test_token_lift_embargo_with_query_token() {
    let server = spawn_test_server_with_tokens(false, &[BYPASS_TOKEN]).await;
    seed_embargoed_patchset(&server.db, Some(FUTURE_EMBARGO)).await;

    // `?token=` is the other way the read paths take a token, so it has to
    // authorize this too — a link handed to somebody is how the token gets
    // into a browser in the first place.
    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "{}/api/patchset/embargo/lift-token?id=1&token={BYPASS_TOKEN}",
            server.base_url
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "lifted");
    assert!(stored_embargo_until(&server.db, 1).await.unwrap() < FUTURE_EMBARGO);
}

#[tokio::test]
#[ignore]
async fn test_token_lift_embargo_without_token_is_unauthorized() {
    let server = spawn_test_server_with_tokens(false, &[BYPASS_TOKEN]).await;
    seed_embargoed_patchset(&server.db, Some(FUTURE_EMBARGO)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "{}/api/patchset/embargo/lift-token?id=1",
            server.base_url
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
    assert_eq!(
        stored_embargo_until(&server.db, 1).await,
        Some(FUTURE_EMBARGO)
    );
}

#[tokio::test]
#[ignore]
async fn test_token_lift_embargo_with_wrong_token_is_unauthorized() {
    let server = spawn_test_server_with_tokens(false, &[BYPASS_TOKEN]).await;
    seed_embargoed_patchset(&server.db, Some(FUTURE_EMBARGO)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "{}/api/patchset/embargo/lift-token?id=1",
            server.base_url
        ))
        .bearer_auth("not-the-token")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
    assert_eq!(
        stored_embargo_until(&server.db, 1).await,
        Some(FUTURE_EMBARGO)
    );
}

#[tokio::test]
#[ignore]
async fn test_token_lift_embargo_unauthorized_when_no_tokens_configured() {
    // An instance that configures no token has nothing to authorize with, so
    // this route must refuse everyone rather than let a request past for want
    // of anything to compare it against.
    let server = spawn_test_server_with_tokens(false, &[]).await;
    seed_embargoed_patchset(&server.db, Some(FUTURE_EMBARGO)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "{}/api/patchset/embargo/lift-token?id=1",
            server.base_url
        ))
        .bearer_auth(BYPASS_TOKEN)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 401);
    assert_eq!(
        stored_embargo_until(&server.db, 1).await,
        Some(FUTURE_EMBARGO)
    );
}

#[tokio::test]
#[ignore]
async fn test_token_lift_embargo_rejected_in_read_only_mode() {
    let server = spawn_test_server_with_tokens(/* read_only */ true, &[BYPASS_TOKEN]).await;
    seed_embargoed_patchset(&server.db, Some(FUTURE_EMBARGO)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "{}/api/patchset/embargo/lift-token?id=1",
            server.base_url
        ))
        .bearer_auth(BYPASS_TOKEN)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 403);
    assert_eq!(
        stored_embargo_until(&server.db, 1).await,
        Some(FUTURE_EMBARGO)
    );
}

// ── Redaction of cross-reviewed embargoed patchsets ──────────────────────

/// Merges a peer's finding into the seeded patchset the way the reviewer does:
/// a completed cross-review job, a `findings` row tagged with it, and the
/// spliced comment block in both `inline_review` and the stored model output.
async fn seed_imported_remote_finding(db: &Arc<Database>) {
    db.conn
        .execute(
            "INSERT INTO ai_interactions (id, input_context, output_raw) \
             VALUES ('local', 'ctx', \
                     '{\"review\":{\"findings\":[{\"problem\":\"Peer found a leak\"}]}}')",
            (),
        )
        .await
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO reviews \
               (id, patchset_id, patch_id, interaction_id, status, created_at, inline_review) \
             VALUES (1, 1, 1, 'local', 'Reviewed', 1234567890, \
                     'local comment\n\n[Finding: peer-abc123] Peer found a leak')",
            (),
        )
        .await
        .unwrap();

    db.enqueue_cross_reviews(
        1,
        &[("peer".to_string(), "https://peer.example".to_string())],
        1000,
    )
    .await
    .unwrap();
    let job = db
        .claim_due_cross_reviews(1000, 1)
        .await
        .unwrap()
        .pop()
        .unwrap();
    db.conn
        .execute(
            "INSERT INTO findings \
               (review_id, severity, problem, preexisting, cross_review_job_id, \
                external_finding_id) \
             VALUES (1, 3, 'Peer found a leak', 0, ?, 'remote-1')",
            libsql::params![job.id],
        )
        .await
        .unwrap();
    db.finish_cross_review_job(&job, "complete", 1100, None)
        .await
        .unwrap();
}

/// Cross-review runs during the local embargo now, so the merged report exists
/// while the patchset is still withheld. The existing embargo redaction has to
/// cover the imported half of it, not just the locally generated half.
#[tokio::test]
#[ignore]
async fn test_embargoed_patchset_hides_imported_remote_finding() {
    let server = spawn_test_server_with_tokens(false, &[BYPASS_TOKEN]).await;
    seed_embargoed_patchset(&server.db, Some(FUTURE_EMBARGO)).await;
    seed_imported_remote_finding(&server.db).await;

    let anonymous: serde_json::Value = reqwest::get(format!("{}/api/patch?id=1", server.base_url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(anonymous["status"], "Embargoed");
    assert!(anonymous["reviews"].as_array().unwrap().is_empty());
    let serialized = anonymous.to_string();
    assert!(!serialized.contains("Peer found a leak"), "{serialized}");
    assert!(!serialized.contains("peer-abc123"), "{serialized}");
    // Even the bare fact that a peer weighed in stays behind the embargo: a
    // "Cross-reviewed" badge next to a withheld review only invites the question
    // of what the peer found.
    assert!(anonymous.get("cross_review").is_none(), "{serialized}");

    let bypassed: serde_json::Value = reqwest::get(format!(
        "{}/api/patch?id=1&token={BYPASS_TOKEN}",
        server.base_url
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(bypassed["status"], "Reviewed");
    let reviews = bypassed["reviews"].as_array().unwrap();
    assert_eq!(reviews.len(), 1);
    assert!(
        reviews[0]["inline_review"]
            .as_str()
            .unwrap()
            .contains("[Finding: peer-abc123]")
    );
    assert!(
        reviews[0]["output"]
            .as_str()
            .unwrap()
            .contains("Peer found a leak")
    );
    assert_eq!(bypassed["cross_review"]["status"], "complete");
    assert_eq!(bypassed["cross_review"]["sources"][0]["name"], "peer");
}

/// The list query carries the cross-review rollup as its own columns, which the
/// per-patchset handler's status gate does not reach.
#[tokio::test]
#[ignore]
async fn test_embargoed_patchset_list_hides_cross_review_metadata() {
    let server = spawn_test_server_with_tokens(false, &[BYPASS_TOKEN]).await;
    seed_embargoed_patchset(&server.db, Some(FUTURE_EMBARGO)).await;
    seed_imported_remote_finding(&server.db).await;

    // `q` is set so the request bypasses the shared homepage cache either way,
    // making the two responses differ only by the token.
    let anonymous: serde_json::Value =
        reqwest::get(format!("{}/api/patchsets?q=test", server.base_url))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    let row = &anonymous["items"][0];
    assert_eq!(row["status"], "Embargoed");
    assert!(row["cross_review_status"].is_null(), "{row}");
    assert!(row["cross_reviewed_at"].is_null(), "{row}");
    assert_eq!(row["findings_high"], 0);

    let bypassed: serde_json::Value = reqwest::get(format!(
        "{}/api/patchsets?q=test&token={BYPASS_TOKEN}",
        server.base_url
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let row = &bypassed["items"][0];
    assert_eq!(row["status"], "Reviewed");
    assert_eq!(row["cross_review_status"], "complete");
    assert_eq!(row["cross_reviewed_at"], 1100);
    assert_eq!(row["findings_high"], 1);
}
