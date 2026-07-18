use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::Arc;

use crate::config::Config;

/// Shared application state.
pub struct AppState {
    pub pool: SqlitePool,
    pub domain: String,
    pub data_dir: String,
    pub webhook_keys: std::collections::HashMap<String, String>,
    pub http_client: reqwest::Client,
    pub inbox_limiter: std::sync::Arc<crate::ratelimit::RateLimiter>,
    pub actor_cache: crate::actor_cache::ActorKeyCache,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/.well-known/webfinger", get(webfinger))
        .route("/.well-known/nodeinfo", get(nodeinfo_discovery))
        .route("/nodeinfo/2.0", get(nodeinfo))
        .route("/users/{username}", get(actor))
        .route("/users/{username}/outbox", get(outbox))
        .route("/users/{username}/followers", get(followers_collection))
        .route("/users/{username}/inbox", post(inbox))
        .route("/inbox", post(shared_inbox))
        .route("/hook/{persona}", post(crate::webhook::handle_webhook))
        .route("/health", get(health))
        // Body size limit: 256KB for all POST endpoints (inbox and webhook)
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
        .with_state(state)
}

pub async fn serve(config: &Config) -> anyhow::Result<()> {
    let pool = crate::db::connect(std::path::Path::new(&config.server.data_dir)).await?;
    let domain = config.server.domain.clone();

    let webhook_keys: std::collections::HashMap<String, String> = config
        .webhook
        .iter()
        .map(|w| (w.persona.clone(), w.key.clone()))
        .collect();

    // Disable automatic redirect following for outbound requests (SSRF defense)
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    // 60 requests per minute per IP on inbox endpoints
    let inbox_limiter = std::sync::Arc::new(crate::ratelimit::RateLimiter::new(60, 60));

    let actor_cache = crate::actor_cache::ActorKeyCache::new(http_client.clone());

    let state = Arc::new(AppState {
        pool: pool.clone(),
        domain: domain.clone(),
        data_dir: config.server.data_dir.clone(),
        webhook_keys,
        http_client,
        inbox_limiter: inbox_limiter.clone(),
        actor_cache,
    });

    // Start delivery worker
    tokio::spawn(crate::delivery::run_worker(pool.clone(), domain.clone()));

    // Periodic rate limiter prune (every 10 minutes)
    let limiter_clone = inbox_limiter.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            limiter_clone.prune().await;
        }
    });

    // Start feed pollers
    let data_dir_path = std::path::PathBuf::from(&config.server.data_dir);
    for feed_config in &config.feed {
        tokio::spawn(crate::feed::run_poller(
            pool.clone(),
            feed_config.clone(),
            domain.clone(),
            data_dir_path.clone(),
        ));
    }

    // Start directory watcher
    if let Some(watch_config) = &config.watch {
        tokio::spawn(crate::watch::run_watcher(
            pool.clone(),
            watch_config.clone(),
            domain.clone(),
        ));
    }

    let app = router(state);
    let listener = tokio::net::TcpListener::bind(&config.server.bind).await?;
    tracing::info!(bind = %config.server.bind, domain = %config.server.domain, "starting server");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("server shut down gracefully");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received Ctrl+C, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}

// --- WebFinger ---

#[derive(Deserialize)]
struct WebfingerQuery {
    resource: String,
}

#[derive(Serialize)]
struct WebfingerResponse {
    subject: String,
    links: Vec<WebfingerLink>,
}

#[derive(Serialize)]
struct WebfingerLink {
    rel: String,
    #[serde(rename = "type")]
    link_type: String,
    href: String,
}

async fn webfinger(
    State(state): State<Arc<AppState>>,
    Query(query): Query<WebfingerQuery>,
) -> impl IntoResponse {
    let prefix = "acct:";
    let acct = if let Some(acct) = query.resource.strip_prefix(prefix) {
        acct
    } else {
        return (StatusCode::BAD_REQUEST, "resource must start with acct:").into_response();
    };

    let (username, domain) = match acct.split_once('@') {
        Some(pair) => pair,
        None => return (StatusCode::BAD_REQUEST, "invalid acct URI").into_response(),
    };

    if domain != state.domain {
        return (StatusCode::NOT_FOUND, "unknown domain").into_response();
    }

    let exists = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM personas WHERE username = ?")
        .bind(username)
        .fetch_one(&state.pool)
        .await;

    match exists {
        Ok((0,)) | Err(_) => (StatusCode::NOT_FOUND, "unknown user").into_response(),
        Ok(_) => {
            let resp = WebfingerResponse {
                subject: query.resource.clone(),
                links: vec![WebfingerLink {
                    rel: "self".to_string(),
                    link_type: "application/activity+json".to_string(),
                    href: format!("https://{}/users/{}", state.domain, username),
                }],
            };
            Json(resp).into_response()
        }
    }
}

// --- NodeInfo ---

async fn nodeinfo_discovery(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "links": [{
            "rel": "http://nodeinfo.diaspora.software/ns/schema/2.0",
            "href": format!("https://{}/nodeinfo/2.0", state.domain)
        }]
    }))
}

async fn nodeinfo(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let user_count = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM personas")
        .fetch_one(&state.pool)
        .await
        .map(|(c,)| c)
        .unwrap_or(0);

    let post_count = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM posts")
        .fetch_one(&state.pool)
        .await
        .map(|(c,)| c)
        .unwrap_or(0);

    Json(serde_json::json!({
        "version": "2.0",
        "software": {
            "name": "broadside",
            "version": env!("CARGO_PKG_VERSION")
        },
        "protocols": ["activitypub"],
        "usage": {
            "users": { "total": user_count },
            "localPosts": post_count
        },
        "openRegistrations": false
    }))
}

// --- Actor ---

async fn actor(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
) -> impl IntoResponse {
    let row = sqlx::query_as::<_, (String, String, String, String, String)>(
        "SELECT id, username, display_name, bio, public_key FROM personas WHERE username = ?",
    )
    .bind(&username)
    .fetch_optional(&state.pool)
    .await;

    let (_id, username, display_name, bio, public_key) = match row {
        Ok(Some(r)) => r,
        _ => return (StatusCode::NOT_FOUND, "unknown user").into_response(),
    };

    let actor_uri = format!("https://{}/users/{}", state.domain, username);

    let doc = serde_json::json!({
        "@context": [
            "https://www.w3.org/ns/activitystreams",
            "https://w3id.org/security/v1"
        ],
        "id": actor_uri,
        "type": "Person",
        "preferredUsername": username,
        "name": display_name,
        "summary": bio,
        "inbox": format!("{}/inbox", actor_uri),
        "outbox": format!("{}/outbox", actor_uri),
        "followers": format!("{}/followers", actor_uri),
        "url": actor_uri,
        "publicKey": {
            "id": format!("{}#main-key", actor_uri),
            "owner": actor_uri,
            "publicKeyPem": public_key
        }
    });

    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "application/activity+json",
        )],
        Json(doc),
    )
        .into_response()
}

// --- Outbox ---

#[derive(Deserialize)]
struct PaginationQuery {
    page: Option<u32>,
}

async fn outbox(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
    Query(query): Query<PaginationQuery>,
) -> impl IntoResponse {
    let persona_id = match crate::persona::get_id(&state.pool, &username).await {
        Ok(id) => id,
        Err(_) => return (StatusCode::NOT_FOUND, "unknown user").into_response(),
    };

    let total = crate::post::count_for_persona(&state.pool, &persona_id)
        .await
        .unwrap_or(0);

    let outbox_uri = format!("https://{}/users/{}/outbox", state.domain, username);

    if query.page.is_none() {
        let doc = serde_json::json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": outbox_uri,
            "type": "OrderedCollection",
            "totalItems": total,
            "first": format!("{}?page=1", outbox_uri)
        });
        return (
            [(
                axum::http::header::CONTENT_TYPE,
                "application/activity+json",
            )],
            Json(doc),
        )
            .into_response();
    }

    let page = query.page.unwrap_or(1).max(1);
    let per_page: i64 = 20;
    let offset = (page as i64 - 1) * per_page;

    let posts = crate::post::list_for_persona(&state.pool, &persona_id, per_page, offset)
        .await
        .unwrap_or_default();

    let actor_uri = format!("https://{}/users/{}", state.domain, username);
    let items: Vec<serde_json::Value> = posts
        .iter()
        .map(|p| {
            let post_uri = format!("{}/statuses/{}", actor_uri, p.id);
            serde_json::json!({
                "id": format!("{}/activity", post_uri),
                "type": "Create",
                "actor": actor_uri,
                "published": p.published_at,
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
                "cc": [format!("{}/followers", actor_uri)],
                "object": {
                    "id": post_uri,
                    "type": "Note",
                    "attributedTo": actor_uri,
                    "content": p.content_html,
                    "published": p.published_at,
                    "to": ["https://www.w3.org/ns/activitystreams#Public"],
                    "cc": [format!("{}/followers", actor_uri)],
                }
            })
        })
        .collect();

    let doc = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}?page={}", outbox_uri, page),
        "type": "OrderedCollectionPage",
        "partOf": outbox_uri,
        "orderedItems": items
    });

    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/activity+json",
        )],
        Json(doc),
    )
        .into_response()
}

// --- Followers collection ---

async fn followers_collection(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
) -> impl IntoResponse {
    let persona_id = match crate::persona::get_id(&state.pool, &username).await {
        Ok(id) => id,
        Err(_) => return (StatusCode::NOT_FOUND, "unknown user").into_response(),
    };

    let (count,) =
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM followers WHERE persona_id = ?")
            .bind(&persona_id)
            .fetch_one(&state.pool)
            .await
            .unwrap_or((0,));

    let doc = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("https://{}/users/{}/followers", state.domain, username),
        "type": "OrderedCollection",
        "totalItems": count
    });

    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/activity+json",
        )],
        Json(doc),
    )
        .into_response()
}

// --- Inbox ---

async fn inbox(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    handle_inbox(&state, Some(&username), &headers, &body).await
}

async fn shared_inbox(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    handle_inbox(&state, None, &headers, &body).await
}

async fn handle_inbox(
    state: &AppState,
    _username: Option<&str>,
    headers: &HeaderMap,
    body: &str,
) -> impl IntoResponse {
    // Rate limit by X-Real-IP (must be set by reverse proxy).
    // WARNING: without a reverse proxy, this header is attacker-controlled.
    let client_ip = headers
        .get("x-real-ip")
        .or_else(|| headers.get("x-forwarded-for"))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .unwrap_or("unknown")
        .trim()
        .to_string();

    if !state.inbox_limiter.try_acquire(&client_ip).await {
        return StatusCode::TOO_MANY_REQUESTS;
    }

    // REQUIRE Signature header — reject unsigned requests.
    let sig_header = match headers.get("signature").and_then(|v| v.to_str().ok()) {
        Some(h) => h.to_string(),
        None => {
            tracing::debug!("rejecting unsigned inbox request");
            return StatusCode::UNAUTHORIZED;
        }
    };

    // Extract keyId from Signature header
    let key_id = match extract_key_id_from_sig(&sig_header) {
        Some(k) => k,
        None => return StatusCode::BAD_REQUEST,
    };

    let actor_uri = key_id.split('#').next().unwrap_or(&key_id).to_string();

    // Reconstruct the request path
    let path = if let Some(uname) = _username {
        format!("/users/{uname}/inbox")
    } else {
        "/inbox".to_string()
    };

    // Fetch actor key — fail closed if unreachable
    let public_key_pem = match state.actor_cache.get_public_key(&actor_uri).await {
        Ok((pem, _)) => pem,
        Err(e) => {
            tracing::warn!(actor = %actor_uri, error = %e, "cannot fetch actor key");
            return StatusCode::UNAUTHORIZED;
        }
    };

    // Verify signature
    if crate::signatures::verify_signature(&public_key_pem, &sig_header, "post", &path, headers)
        .is_err()
    {
        // Retry once after cache invalidation (key rotation)
        state.actor_cache.invalidate(&actor_uri).await;
        match state.actor_cache.get_public_key(&actor_uri).await {
            Ok((fresh_key, _)) => {
                if crate::signatures::verify_signature(
                    &fresh_key,
                    &sig_header,
                    "post",
                    &path,
                    headers,
                )
                .is_err()
                {
                    tracing::warn!(actor = %actor_uri, "signature verification failed");
                    return StatusCode::UNAUTHORIZED;
                }
            }
            Err(_) => return StatusCode::UNAUTHORIZED,
        }
    }

    // Verify Digest header — REQUIRED.
    // The Digest header cryptographically binds the body to the signature
    // (when 'digest' is in the signed headers). Without it, body substitution is trivial.
    let digest_header = match headers.get("digest").and_then(|v| v.to_str().ok()) {
        Some(d) => d.to_string(),
        None => {
            tracing::debug!("rejecting request without Digest header");
            return StatusCode::BAD_REQUEST;
        }
    };
    if let Some(expected_b64) = digest_header.strip_prefix("SHA-256=") {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine;
        use sha2::Digest;
        let actual = sha2::Sha256::digest(body.as_bytes());
        let actual_b64 = B64.encode(actual);
        if actual_b64 != expected_b64 {
            tracing::warn!("Digest mismatch");
            return StatusCode::BAD_REQUEST;
        }
    } else {
        return StatusCode::BAD_REQUEST;
    }

    // Verify Date header freshness — REQUIRED.
    // Date must be present and within 5 minutes to prevent replay attacks.
    let date_str = match headers.get("date").and_then(|v| v.to_str().ok()) {
        Some(d) => d.to_string(),
        None => {
            tracing::debug!("rejecting request without Date header");
            return StatusCode::BAD_REQUEST;
        }
    };
    if let Ok(date) = chrono::DateTime::parse_from_rfc2822(&date_str) {
        let age = chrono::Utc::now().signed_duration_since(date);
        if age.num_seconds().unsigned_abs() > 300 {
            tracing::debug!("rejecting stale request (age: {}s)", age.num_seconds());
            return StatusCode::UNAUTHORIZED;
        }
    } else {
        return StatusCode::BAD_REQUEST;
    }

    // Parse activity
    let activity: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST,
    };

    let activity_type = activity["type"].as_str().unwrap_or("");

    // Verify the activity's actor matches the signature's keyId actor
    let activity_actor = activity["actor"].as_str().unwrap_or("");
    if !activity_actor.is_empty() && activity_actor != actor_uri {
        tracing::warn!(
            sig_actor = %actor_uri,
            activity_actor,
            "actor mismatch between signature and activity"
        );
        return StatusCode::UNAUTHORIZED;
    }

    match activity_type {
        "Follow" => {
            let follower_actor = match activity["actor"].as_str() {
                Some(a) if !a.is_empty() => a,
                _ => return StatusCode::BAD_REQUEST,
            };
            let followed = match activity["object"].as_str() {
                Some(o) => o,
                None => return StatusCode::BAD_REQUEST,
            };

            // Validate the followed URI belongs to this server
            let expected_prefix = format!("https://{}/users/", state.domain);
            let username = match followed.strip_prefix(&expected_prefix) {
                Some(u) if !u.is_empty() && !u.contains('/') => u,
                _ => return StatusCode::BAD_REQUEST,
            };

            let persona_id = match crate::persona::get_id(&state.pool, username).await {
                Ok(id) => id,
                Err(_) => return StatusCode::NOT_FOUND,
            };

            // SSRF guard: only fetch public https URLs
            if !follower_actor.starts_with("https://") {
                return StatusCode::BAD_REQUEST;
            }
            if let Ok(parsed) = url::Url::parse(follower_actor) {
                if let Some(host) = parsed.host_str() {
                    if is_private_host(host) {
                        return StatusCode::BAD_REQUEST;
                    }
                }
            }

            // Fetch the follower's actor document to get their inbox
            let actor_doc = match state
                .http_client
                .get(follower_actor)
                .header("Accept", "application/activity+json")
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    let body = match resp.bytes().await {
                        Ok(b) if b.len() <= 65536 => b,
                        Ok(b) => {
                            tracing::warn!(size = b.len(), "actor document too large");
                            return StatusCode::ACCEPTED;
                        }
                        Err(_) => return StatusCode::ACCEPTED,
                    };
                    match serde_json::from_slice::<serde_json::Value>(&body) {
                        Ok(v) => v,
                        Err(_) => return StatusCode::ACCEPTED,
                    }
                }
                _ => return StatusCode::ACCEPTED,
            };

            let inbox_uri = actor_doc["inbox"].as_str().unwrap_or("").to_string();
            // Validate shared inbox URI — same SSRF rules
            let shared_inbox_uri = actor_doc["endpoints"]["sharedInbox"]
                .as_str()
                .filter(|s| s.starts_with("https://"))
                .filter(|s| {
                    url::Url::parse(s)
                        .ok()
                        .and_then(|u| u.host_str().map(|h| !is_private_host(h)))
                        .unwrap_or(false)
                })
                .map(|s| s.to_string());

            if inbox_uri.is_empty() || !inbox_uri.starts_with("https://") {
                return StatusCode::ACCEPTED;
            }
            if let Ok(parsed) = url::Url::parse(&inbox_uri) {
                if let Some(host) = parsed.host_str() {
                    if is_private_host(host) {
                        return StatusCode::ACCEPTED;
                    }
                }
            }

            // Insert follower
            let follower_id = crate::id::gen_id();
            if let Err(e) = sqlx::query(
                "INSERT OR IGNORE INTO followers \
                 (id, persona_id, actor_uri, inbox_uri, shared_inbox_uri) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(&follower_id)
            .bind(&persona_id)
            .bind(follower_actor)
            .bind(&inbox_uri)
            .bind(&shared_inbox_uri)
            .execute(&state.pool)
            .await
            {
                tracing::error!(error = %e, follower = follower_actor, "failed to insert follower");
            }

            tracing::info!(
                follower = follower_actor,
                persona = username,
                "accepted follow"
            );

            // Send Accept in a background task
            let accept = serde_json::json!({
                "@context": "https://www.w3.org/ns/activitystreams",
                "id": format!("https://{}/users/{}#accept/{}", state.domain, username, follower_id),
                "type": "Accept",
                "actor": format!("https://{}/users/{}", state.domain, username),
                "object": activity
            });

            let pool = state.pool.clone();
            let client = state.http_client.clone();
            let domain = state.domain.clone();
            let inbox = inbox_uri.clone();
            let uname = username.to_string();

            tokio::spawn(async move {
                let accept_body = match serde_json::to_vec(&accept) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!(error = %e, "failed to serialize Accept activity");
                        return;
                    }
                };
                let actor_uri = format!("https://{}/users/{}", domain, uname);
                let key_id = format!("{actor_uri}#main-key");

                let private_key = match crate::persona::get_private_key(&pool, &uname).await {
                    Ok(k) => k,
                    Err(_) => return,
                };

                let target_path = url::Url::parse(&inbox)
                    .map(|u| u.path().to_string())
                    .unwrap_or_else(|_| "/inbox".to_string());
                let inbox_domain = url::Url::parse(&inbox)
                    .ok()
                    .and_then(|u| u.host_str().map(|h| h.to_string()))
                    .unwrap_or_default();

                if let Ok(sig_headers) = crate::signatures::sign_request(
                    &private_key,
                    &key_id,
                    &target_path,
                    &inbox_domain,
                    &accept_body,
                ) {
                    if let Err(e) = client
                        .post(&inbox)
                        .headers(sig_headers)
                        .header("Content-Type", "application/activity+json")
                        .body(accept_body)
                        .send()
                        .await
                    {
                        tracing::error!(error = %e, inbox = %inbox, "failed to send Accept");
                    }
                }
            });

            StatusCode::ACCEPTED
        }
        "Undo" => {
            // Signature is verified above — the actor field matches the signer.
            let inner_type = activity["object"]["type"].as_str().unwrap_or("");
            if inner_type == "Follow" {
                // Only delete followers where actor_uri matches the SIGNED actor
                // (not the inner object's actor, which could differ)
                if !actor_uri.is_empty() {
                    match sqlx::query("DELETE FROM followers WHERE actor_uri = ?")
                        .bind(&actor_uri)
                        .execute(&state.pool)
                        .await
                    {
                        Ok(r) => {
                            if r.rows_affected() > 0 {
                                tracing::info!(
                                    follower = %actor_uri,
                                    "removed follower (Undo Follow)"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, follower = %actor_uri, "failed to remove follower")
                        }
                    }
                }
            }
            StatusCode::ACCEPTED
        }
        _ => {
            tracing::debug!(activity_type, "discarding inbound activity");
            StatusCode::ACCEPTED
        }
    }
}

// --- Health ---

async fn health(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let persona_count = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM personas")
        .fetch_one(&state.pool)
        .await
        .map(|(c,)| c)
        .unwrap_or(0);

    let pending =
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM delivery_queue WHERE status = 'pending'")
            .fetch_one(&state.pool)
            .await
            .map(|(c,)| c)
            .unwrap_or(0);

    Json(serde_json::json!({
        "status": "ok",
        "personas": persona_count,
        "pending_deliveries": pending
    }))
}

/// Check if a hostname resolves to a private/loopback/link-local address.
pub fn is_private_host(host: &str) -> bool {
    use std::net::IpAddr;
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => {
                v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_broadcast()
                    || v4.is_unspecified()
                    || v4.octets()[0] == 169 && v4.octets()[1] == 254
            }
            IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
                    || v6.to_ipv4_mapped().is_some_and(|v4| {
                        v4.is_loopback()
                            || v4.is_private()
                            || v4.is_link_local()
                            || v4.octets()[0] == 169 && v4.octets()[1] == 254
                    })
            }
        };
    }
    host == "localhost" || host.ends_with(".local") || host.ends_with(".internal")
}

/// Extract keyId from a Signature header value using proper quoted-string parsing.
fn extract_key_id_from_sig(sig_header: &str) -> Option<String> {
    let mut in_quotes = false;
    let mut escape_next = false;
    let mut current = String::new();
    let mut parts = Vec::new();

    for c in sig_header.chars() {
        if escape_next {
            current.push(c);
            escape_next = false;
            continue;
        }
        match c {
            '\\' if in_quotes => {
                escape_next = true;
                current.push(c);
            }
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            ',' if !in_quotes => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }

    for part in &parts {
        let trimmed = part.trim();
        if let Some(val) = trimmed.strip_prefix("keyId=\"") {
            return Some(val.strip_suffix('"').unwrap_or(val).to_string());
        }
    }
    None
}
