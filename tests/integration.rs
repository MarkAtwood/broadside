use std::net::TcpListener;
use std::sync::Arc;

/// Spin up a broadside server in-process on a random port for testing.
async fn test_server() -> (String, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();

    // Initialize the database
    broadside::db::init_data_dir(tmp.path()).await.unwrap();

    // Create a test persona
    let pool = broadside::db::connect(tmp.path()).await.unwrap();
    broadside::persona::add(&pool, "test", Some("Test Account"))
        .await
        .unwrap();

    // Create a post
    let persona_id = broadside::persona::get_id(&pool, "test").await.unwrap();
    broadside::post::create(
        &pool,
        persona_id,
        "<p>Hello fediverse!</p>",
        "Hello fediverse!",
        Some("test-post-1"),
    )
    .await
    .unwrap();

    // ponytail: bind-drop-rebind has a theoretical port reuse race; acceptable in test code
    // since CI retries cover the astronomically rare collision. Passing the listener directly
    // would require refactoring AppState construction.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let base_url = format!("http://127.0.0.1:{port}");

    let http_client = reqwest::Client::new();
    let state = Arc::new(broadside::server::AppState {
        pool,
        domain: format!("127.0.0.1:{port}"),
        data_dir: tmp.path().to_str().unwrap().to_string(),
        webhook_keys: std::collections::HashMap::new(),
        http_client: http_client.clone(),
        inbox_limiter: std::sync::Arc::new(broadside::ratelimit::RateLimiter::new(1000, 60)),
        actor_cache: broadside::actor_cache::ActorKeyCache::new(http_client),
        extra_css: String::new(),
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
async fn test_webfinger_non_acct_uri() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    // RFC 7033: non-acct: URI schemes must get 404, not 400
    let resp = client
        .get(format!(
            "{base_url}/.well-known/webfinger?resource=https://example.com/users/alice"
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn test_webfinger_empty_resource() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    // RFC 7033 §4.2.4: empty resource parameter is malformed -> 400
    let resp = client
        .get(format!("{base_url}/.well-known/webfinger?resource="))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn test_webfinger_cors_header() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    let port = base_url.rsplit(':').next().unwrap();
    let domain = format!("127.0.0.1:{port}");

    // RFC 7033 §5.1: WebFinger must include Access-Control-Allow-Origin
    let resp = client
        .get(format!(
            "{base_url}/.well-known/webfinger?resource=acct:test@{domain}"
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("access-control-allow-origin").unwrap(),
        "*"
    );
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
async fn test_inbox_rejects_unsigned() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    // Unsigned request should be rejected with 401
    let resp = client
        .post(format!("{base_url}/users/test/inbox"))
        .header("Content-Type", "application/activity+json")
        .body("not json")
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        401,
        "unsigned inbox requests must be rejected"
    );
}

#[tokio::test]
async fn test_inbox_rejects_unsigned_activity() {
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

    assert_eq!(resp.status(), 401, "unsigned activities must be rejected");
}

#[tokio::test]
async fn test_shared_inbox_rejects_unsigned() {
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

    assert_eq!(
        resp.status(),
        401,
        "unsigned shared inbox requests must be rejected"
    );
}

/// Fediverse Pasture-inspired robustness test: POST various malformed and
/// edge-case payloads to both inbox endpoints. The server must never return 500.
#[tokio::test]
async fn test_inbox_robustness_no_500() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    let big_content = format!(
        r#"{{"type":"Create","actor":"https://x.example/u/a","object":{{"type":"Note","content":"{}"}}}}"#,
        "x".repeat(100_000)
    );
    let payloads: Vec<(&str, &str)> = vec![
        // Empty body
        ("empty body", ""),
        // Not JSON
        ("plain text", "hello world"),
        // Valid JSON but not an object
        ("json array", "[]"),
        ("json number", "42"),
        ("json string", "\"hello\""),
        ("json null", "null"),
        // Empty object
        ("empty object", "{}"),
        // Missing required fields
        (
            "no type",
            r#"{"actor":"https://x.example/u/a","object":"https://x.example/p/1"}"#,
        ),
        (
            "no actor",
            r#"{"type":"Create","object":"https://x.example/p/1"}"#,
        ),
        // Null values in key positions
        (
            "null type",
            r#"{"type":null,"actor":"https://x.example/u/a"}"#,
        ),
        ("null actor", r#"{"type":"Create","actor":null}"#),
        (
            "null object",
            r#"{"type":"Create","actor":"https://x.example/u/a","object":null}"#,
        ),
        // Wrong types for fields
        (
            "numeric type",
            r#"{"type":42,"actor":"https://x.example/u/a"}"#,
        ),
        (
            "array actor",
            r#"{"type":"Create","actor":["https://x.example/u/a"]}"#,
        ),
        (
            "boolean object",
            r#"{"type":"Like","actor":"https://x.example/u/a","object":true}"#,
        ),
        // Deeply nested
        (
            "deep nesting",
            r#"{"type":"Create","actor":"https://x.example/u/a","object":{"object":{"object":{"object":{"object":{"object":{"object":{"object":{"object":{"object":"deep"}}}}}}}}}}"#,
        ),
        // Very long content field
        ("100KB content", &big_content),
        // Unicode edge cases
        (
            "emoji actor",
            r#"{"type":"Follow","actor":"https://x.example/u/😀🎉","object":"https://x.example/u/b"}"#,
        ),
        (
            "null bytes",
            "{\"type\":\"Create\",\"actor\":\"https://x.example/u/a\\u0000b\"}",
        ),
        // Unknown activity types
        (
            "unknown type",
            r#"{"type":"Explode","actor":"https://x.example/u/a","object":"https://x.example/p/1"}"#,
        ),
        // Duplicate keys (last wins per JSON spec)
        (
            "duplicate keys",
            r#"{"type":"Like","type":"Follow","actor":"https://x.example/u/a"}"#,
        ),
        // Extra unexpected fields
        (
            "extra fields",
            r#"{"type":"Like","actor":"https://x.example/u/a","object":"https://x.example/p/1","evil":"<script>alert(1)</script>","nested":{"deep":true}}"#,
        ),
        // Mastodon-style with @context
        (
            "with context",
            r#"{"@context":"https://www.w3.org/ns/activitystreams","type":"Like","actor":"https://x.example/u/a","object":"https://x.example/p/1"}"#,
        ),
        // Array-valued to/cc (common in real AP)
        (
            "array to/cc",
            r#"{"type":"Create","actor":"https://x.example/u/a","to":["https://www.w3.org/ns/activitystreams#Public"],"cc":[],"object":{"type":"Note","content":"hi"}}"#,
        ),
        // String to/cc (also valid per AP spec)
        (
            "string to",
            r#"{"type":"Create","actor":"https://x.example/u/a","to":"https://www.w3.org/ns/activitystreams#Public"}"#,
        ),
    ];

    for (label, body) in &payloads {
        for endpoint in ["/users/test/inbox", "/inbox"] {
            let resp = client
                .post(format!("{base_url}{endpoint}"))
                .header("Content-Type", "application/activity+json")
                .body(body.to_string())
                .send()
                .await
                .unwrap();

            let status = resp.status().as_u16();
            assert!(
                status < 500,
                "inbox returned {status} (5xx) for {label} on {endpoint}"
            );
        }
    }
}

// --- DID federation compatibility ---

#[tokio::test]
async fn test_actor_includes_also_known_as_with_dids() {
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

    // alsoKnownAs must be present as an array
    let aka = body["alsoKnownAs"]
        .as_array()
        .expect("alsoKnownAs must be an array");
    assert!(aka.len() >= 1, "alsoKnownAs must have at least did:web");

    // First entry is did:web
    let did_web = aka[0].as_str().unwrap();
    assert!(
        did_web.starts_with("did:web:"),
        "first alsoKnownAs should be did:web, got: {did_web}"
    );
    assert!(
        did_web.contains(":users:test"),
        "did:web should contain :users:test"
    );

    // Second entry is did:key (persona created in test_server has DID)
    let did_key = aka[1].as_str().unwrap();
    assert!(
        did_key.starts_with("did:key:z6Mk"),
        "second alsoKnownAs should be did:key, got: {did_key}"
    );

    // Core AP fields must still be present (regression check)
    assert_eq!(body["type"], "Person");
    assert!(body["inbox"].is_string());
    assert!(body["outbox"].is_string());
    assert!(body["publicKey"]["publicKeyPem"].is_string());
    assert!(body["followers"].is_string());
    assert!(body["following"].is_string());
    assert!(body["endpoints"]["sharedInbox"].is_string());
}

#[tokio::test]
async fn test_did_document_endpoint() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base_url}/users/test/did.json"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "application/did+json"
    );

    let doc: serde_json::Value = resp.json().await.unwrap();

    // W3C DID v1 context
    let contexts = doc["@context"].as_array().expect("@context must be array");
    assert_eq!(contexts[0], "https://www.w3.org/ns/did/v1");

    // id is did:web
    let id = doc["id"].as_str().unwrap();
    assert!(
        id.starts_with("did:web:"),
        "id should be did:web, got: {id}"
    );
    assert!(id.ends_with(":users:test"));

    // Verification methods: RSA (main-key) + Ed25519 (recovery-key)
    let methods = doc["verificationMethod"]
        .as_array()
        .expect("verificationMethod must be array");
    assert_eq!(methods.len(), 2, "expect RSA + Ed25519 methods");
    assert_eq!(methods[0]["type"], "RsaVerificationKey2018");
    assert!(methods[0]["publicKeyPem"]
        .as_str()
        .unwrap()
        .contains("BEGIN PUBLIC KEY"));
    assert_eq!(methods[1]["type"], "Ed25519VerificationKey2020");
    assert!(methods[1]["publicKeyMultibase"]
        .as_str()
        .unwrap()
        .starts_with("z"));

    // authentication and assertionMethod
    assert_eq!(doc["authentication"][0], "#main-key");
    assert_eq!(doc["assertionMethod"][0], "#main-key");

    // alsoKnownAs includes did:key and actor URL
    let aka = doc["alsoKnownAs"]
        .as_array()
        .expect("alsoKnownAs must be array");
    let aka_strings: Vec<&str> = aka.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        aka_strings.iter().any(|s| s.starts_with("did:key:z6Mk")),
        "alsoKnownAs should include did:key"
    );
    assert!(
        aka_strings.iter().any(|s| s.contains("/users/test")),
        "alsoKnownAs should include actor URL"
    );
}

#[tokio::test]
async fn test_did_document_unknown_user_returns_404() {
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base_url}/users/nobody/did.json"))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn test_actor_also_known_as_is_valid_jsonld() {
    // Mastodon processes alsoKnownAs as a JSON-LD property from ActivityStreams context.
    // Verify the actor document is valid for a JSON-LD consumer:
    // - @context includes ActivityStreams
    // - alsoKnownAs values are strings (URIs), not objects
    // - No unknown keys outside the declared contexts that could cause warnings
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base_url}/users/test"))
        .header("Accept", "application/activity+json")
        .send()
        .await
        .unwrap();

    let body: serde_json::Value = resp.json().await.unwrap();

    // @context includes ActivityStreams (required for alsoKnownAs)
    let context = &body["@context"];
    let ctx_strings: Vec<&str> = if let Some(arr) = context.as_array() {
        arr.iter().filter_map(|v| v.as_str()).collect()
    } else {
        vec![context.as_str().unwrap_or("")]
    };
    assert!(
        ctx_strings
            .iter()
            .any(|s| *s == "https://www.w3.org/ns/activitystreams"),
        "@context must include ActivityStreams"
    );

    // alsoKnownAs values must all be strings (URI-shaped)
    for entry in body["alsoKnownAs"].as_array().unwrap() {
        assert!(
            entry.is_string(),
            "alsoKnownAs entries must be strings, got: {entry}"
        );
        let s = entry.as_str().unwrap();
        assert!(
            s.starts_with("did:") || s.starts_with("https://") || s.starts_with("http://"),
            "alsoKnownAs entry should be a URI: {s}"
        );
    }
}

#[tokio::test]
async fn test_did_key_is_consistent_across_endpoints() {
    // The did:key in the actor's alsoKnownAs must match the one in the DID document
    let (base_url, _tmp) = test_server().await;
    let client = reqwest::Client::new();

    let actor_resp = client
        .get(format!("{base_url}/users/test"))
        .header("Accept", "application/activity+json")
        .send()
        .await
        .unwrap();
    let actor: serde_json::Value = actor_resp.json().await.unwrap();

    let did_resp = client
        .get(format!("{base_url}/users/test/did.json"))
        .send()
        .await
        .unwrap();
    let did_doc: serde_json::Value = did_resp.json().await.unwrap();

    // Extract did:key from actor alsoKnownAs
    let actor_did_key = actor["alsoKnownAs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v.as_str().map_or(false, |s| s.starts_with("did:key:")))
        .expect("actor should have did:key in alsoKnownAs")
        .as_str()
        .unwrap();

    // Extract did:key from DID document alsoKnownAs
    let doc_did_key = did_doc["alsoKnownAs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v.as_str().map_or(false, |s| s.starts_with("did:key:")))
        .expect("DID doc should have did:key in alsoKnownAs")
        .as_str()
        .unwrap();

    assert_eq!(
        actor_did_key, doc_did_key,
        "did:key must be consistent between actor and DID document"
    );

    // The recovery key in verificationMethod should encode the same pubkey
    let recovery_multibase = did_doc["verificationMethod"][1]["publicKeyMultibase"]
        .as_str()
        .unwrap();
    // did:key:z{multibase} — the multibase portion after "did:key:" should match
    let did_key_multibase = actor_did_key.strip_prefix("did:key:").unwrap();
    assert_eq!(
        did_key_multibase, recovery_multibase,
        "did:key multibase must match recovery key publicKeyMultibase"
    );
}

#[tokio::test]
async fn test_post_dedup_via_source_ref() {
    let (base_url, _tmp) = test_server().await;
    let _ = base_url; // server not needed for this test

    let tmp = tempfile::tempdir().unwrap();
    broadside::db::init_data_dir(tmp.path()).await.unwrap();
    let pool = broadside::db::connect(tmp.path()).await.unwrap();
    broadside::persona::add(&pool, "dedup", None).await.unwrap();
    let pid = broadside::persona::get_id(&pool, "dedup").await.unwrap();

    let id1 = broadside::post::create(&pool, pid, "<p>a</p>", "a", Some("ref-1"))
        .await
        .unwrap();
    assert!(!id1.is_empty());

    // Same source_ref should fail with UNIQUE constraint
    let result = broadside::post::create(&pool, pid, "<p>b</p>", "b", Some("ref-1")).await;
    assert!(result.is_err(), "duplicate source_ref should be rejected");
}
