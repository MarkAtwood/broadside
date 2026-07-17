use std::net::TcpListener;
use std::sync::Arc;

/// Spin up a broadside server in-process on a random port for testing.
async fn test_server() -> (String, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().to_str().unwrap();

    // Initialize the database
    broadside::db::init_data_dir(data_dir).await.unwrap();

    // Create a test persona
    let pool = broadside::db::connect(tmp.path()).await.unwrap();
    broadside::persona::add(&pool, "test", Some("Test Account"))
        .await
        .unwrap();

    // Create a post
    let persona_id = broadside::persona::get_id(&pool, "test").await.unwrap();
    broadside::post::create(
        &pool,
        &persona_id,
        "<p>Hello fediverse!</p>",
        "Hello fediverse!",
        Some("test-post-1"),
    )
    .await
    .unwrap();

    // Find a free port
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let base_url = format!("http://127.0.0.1:{port}");

    let state = Arc::new(broadside::server::AppState {
        pool,
        domain: format!("127.0.0.1:{port}"),
        webhook_keys: std::collections::HashMap::new(),
        http_client: reqwest::Client::new(),
        inbox_limiter: std::sync::Arc::new(broadside::ratelimit::RateLimiter::new(1000, 60)),
    });

    let app = broadside::server::router(state);
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .unwrap();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Give the server a moment to start
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (base_url, tmp)
}

#[tokio::test]
async fn test_webfinger_discovery() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    // Domain includes port in test mode
    let port = base_url.rsplit(':').next().unwrap();
    let domain = format!("127.0.0.1:{port}");

    let resp = client
        .get(format!(
            "{base_url}/.well-known/webfinger?resource=acct:test@{domain}"
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200, "webfinger should return 200");

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["links"][0]["href"]
        .as_str()
        .unwrap()
        .contains("/users/test"));
}

#[tokio::test]
async fn test_webfinger_unknown_user() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    let port = base_url.rsplit(':').next().unwrap();
    let domain = format!("127.0.0.1:{port}");

    let resp = client
        .get(format!(
            "{base_url}/.well-known/webfinger?resource=acct:nobody@{domain}"
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn test_actor_document() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base_url}/users/test"))
        .header("Accept", "application/activity+json")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "Person");
    assert_eq!(body["preferredUsername"], "test");
    assert!(body["publicKey"]["publicKeyPem"]
        .as_str()
        .unwrap()
        .contains("BEGIN PUBLIC KEY"));
    assert!(body["inbox"].as_str().unwrap().contains("/inbox"));
    assert!(body["outbox"].as_str().unwrap().contains("/outbox"));
}

#[tokio::test]
async fn test_actor_unknown() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base_url}/users/nobody"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn test_outbox() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    // Collection root
    let resp = client
        .get(format!("{base_url}/users/test/outbox"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "OrderedCollection");
    assert_eq!(body["totalItems"], 1);

    // First page
    let resp = client
        .get(format!("{base_url}/users/test/outbox?page=1"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "OrderedCollectionPage");
    let items = body["orderedItems"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "Create");
    assert_eq!(items[0]["object"]["type"], "Note");
    assert!(items[0]["object"]["content"]
        .as_str()
        .unwrap()
        .contains("Hello fediverse!"));
}

#[tokio::test]
async fn test_followers_collection() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base_url}/users/test/followers"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["type"], "OrderedCollection");
    assert_eq!(body["totalItems"], 0);
}

#[tokio::test]
async fn test_nodeinfo() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    // Discovery
    let resp = client
        .get(format!("{base_url}/.well-known/nodeinfo"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["links"][0]["href"]
        .as_str()
        .unwrap()
        .contains("/nodeinfo/2.0"));

    // NodeInfo document
    let resp = client
        .get(format!("{base_url}/nodeinfo/2.0"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["software"]["name"], "broadside");
    assert_eq!(body["protocols"][0], "activitypub");
}

#[tokio::test]
async fn test_health() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base_url}/health"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["personas"], 1);
}

#[tokio::test]
async fn test_inbox_invalid_json() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base_url}/users/test/inbox"))
        .header("Content-Type", "application/activity+json")
        .body("not json")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn test_inbox_discard_unknown_activity() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    let activity = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Like",
        "actor": "https://remote.example/users/alice",
        "object": format!("{base_url}/users/test/statuses/123")
    });

    let resp = client
        .post(format!("{base_url}/users/test/inbox"))
        .header("Content-Type", "application/activity+json")
        .json(&activity)
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        202,
        "unknown activities should be accepted and discarded"
    );
}

#[tokio::test]
async fn test_shared_inbox() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    let activity = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Announce",
        "actor": "https://remote.example/users/bob",
        "object": "https://remote.example/statuses/456"
    });

    let resp = client
        .post(format!("{base_url}/inbox"))
        .header("Content-Type", "application/activity+json")
        .json(&activity)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 202);
}

#[tokio::test]
async fn test_post_dedup_via_source_ref() {
    let (base_url, _tmp) = test_server().await;
    let _ = base_url; // server not needed for this test

    let tmp = tempfile::tempdir().unwrap();
    broadside::db::init_data_dir(tmp.path().to_str().unwrap())
        .await
        .unwrap();
    let pool = broadside::db::connect(tmp.path()).await.unwrap();
    broadside::persona::add(&pool, "dedup", None).await.unwrap();
    let pid = broadside::persona::get_id(&pool, "dedup").await.unwrap();

    let id1 = broadside::post::create(&pool, &pid, "<p>a</p>", "a", Some("ref-1"))
        .await
        .unwrap();
    assert!(!id1.is_empty());

    // Same source_ref should fail with UNIQUE constraint
    let result = broadside::post::create(&pool, &pid, "<p>b</p>", "b", Some("ref-1")).await;
    assert!(result.is_err(), "duplicate source_ref should be rejected");
}
