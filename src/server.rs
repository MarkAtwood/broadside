use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::fmt::Write;
use std::sync::Arc;

use crate::config::Config;

/// Base CSS shared by index and profile pages. Uses CSS custom properties for theming.
const BASE_CSS: &str = "\
:root { --text: #1d1d1f; --muted: #6e6e73; --bg: #fff; --card: #f5f5f7; --border: #d2d2d7; --link: #0066cc; }\
* { box-sizing: border-box; margin: 0; padding: 0; }\
body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif; \
color: var(--text); background: var(--bg); line-height: 1.6; }\
main { max-width: 640px; margin: 0 auto; padding: 2rem 1.5rem; }\
h1 { font-size: 1.75rem; font-weight: 600; margin-bottom: 0.5rem; }\
a { color: var(--link); text-decoration: none; }\
a:hover { text-decoration: underline; }\
@media (prefers-color-scheme: dark) { \
:root { --text: #f5f5f7; --muted: #98989d; --bg: #1d1d1f; --card: #2c2c2e; --border: #3a3a3c; --link: #2997ff; }\
body { background: var(--bg); color: var(--text); } }";

/// Shared application state passed to all route handlers.
pub struct AppState {
    pub pool: SqlitePool,
    pub domain: String,
    pub data_dir: String,
    pub webhook_keys: std::collections::HashMap<String, String>,
    pub http_client: reqwest::Client,
    pub inbox_limiter: std::sync::Arc<crate::ratelimit::RateLimiter>,
    pub actor_cache: crate::actor_cache::ActorKeyCache,
    pub extra_css: String,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/.well-known/webfinger", get(webfinger))
        .route("/.well-known/nodeinfo", get(nodeinfo_discovery))
        .route("/nodeinfo/2.0", get(nodeinfo))
        .route("/users/{username}", get(actor))
        .route("/users/{username}/outbox", get(outbox))
        .route("/users/{username}/followers", get(followers_collection))
        .route("/users/{username}/following", get(following_collection))
        .route("/users/{username}/inbox", post(inbox))
        .route("/inbox", post(shared_inbox))
        .route("/hook/{persona}", post(crate::webhook::handle_webhook))
        .route("/", get(index))
        .route("/health", get(health))
        // Body size limit: 256KB for all POST endpoints (inbox and webhook)
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
        .layer(axum::middleware::from_fn(security_headers))
        .with_state(state)
}

async fn security_headers(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert("X-Content-Type-Options", "nosniff".parse().unwrap());
    h.insert("X-Frame-Options", "DENY".parse().unwrap());
    h.insert("Referrer-Policy", "same-origin".parse().unwrap());
    h.insert(
        "Content-Security-Policy",
        "default-src 'none'; style-src 'unsafe-inline'; img-src https: data:; frame-ancestors 'none'"
            .parse()
            .unwrap(),
    );
    // Default to no-store; handlers override with public caching where appropriate
    h.entry("Cache-Control")
        .or_insert("no-store".parse().unwrap());
    resp
}

pub async fn serve(config: &Config) -> anyhow::Result<()> {
    // Warn about insecure file permissions on sensitive files
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let data_dir = std::path::Path::new(&config.server.data_dir);
        for name in &["broadside.db", "config.toml"] {
            let path = data_dir.join(name);
            if let Ok(meta) = std::fs::metadata(&path) {
                if meta.mode() & 0o077 != 0 {
                    tracing::warn!(
                        file = %path.display(),
                        mode = format!("{:o}", meta.mode() & 0o777),
                        "sensitive file is readable by other users — recommend chmod 600"
                    );
                }
            }
        }
    }

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

    let theme_css = crate::theme::load_theme_css(&config.server.theme_tokens_path);
    let custom_css = if config.server.custom_css_path.is_empty() {
        String::new()
    } else {
        std::fs::read_to_string(&config.server.custom_css_path).unwrap_or_default()
    };
    // Strip </style> (case-insensitive) to prevent style tag breakout from operator-supplied CSS
    let extra_css = regex::RegexBuilder::new("</style>")
        .case_insensitive(true)
        .build()
        .unwrap()
        .replace_all(&format!("{theme_css}{custom_css}"), "")
        .into_owned();

    let state = Arc::new(AppState {
        pool: pool.clone(),
        domain: domain.clone(),
        data_dir: config.server.data_dir.clone(),
        webhook_keys,
        http_client,
        inbox_limiter: inbox_limiter.clone(),
        actor_cache,
        extra_css,
    });

    // Start delivery worker
    tokio::spawn(crate::delivery::run_worker(pool.clone(), domain.clone()));

    // Periodic census registration (the-federation.info)
    {
        let census_domain = domain.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(7 * 24 * 3600)).await;
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap();
            loop {
                let url = format!("https://the-federation.info/register/{census_domain}");
                match client.get(&url).send().await {
                    Ok(resp) => tracing::info!(
                        status = %resp.status(),
                        "census registration ping to the-federation.info"
                    ),
                    Err(e) => tracing::debug!("census ping failed (non-fatal): {e}"),
                }
                tokio::time::sleep(std::time::Duration::from_secs(7 * 24 * 3600)).await;
            }
        });
    }

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
    if query.resource.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing resource").into_response();
    }
    let prefix = "acct:";
    let Some(acct) = query.resource.strip_prefix(prefix) else {
        return (StatusCode::NOT_FOUND, "resource not found").into_response();
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
            (
                [
                    (axum::http::header::CONTENT_TYPE, "application/jrd+json"),
                    (axum::http::header::CACHE_CONTROL, "public, max-age=300"),
                    (axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
                ],
                Json(resp),
            )
                .into_response()
        }
    }
}

// --- NodeInfo ---

async fn nodeinfo_discovery(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    (
        [(axum::http::header::CACHE_CONTROL, "public, max-age=3600")],
        Json(serde_json::json!({
            "links": [{
                "rel": "http://nodeinfo.diaspora.software/ns/schema/2.0",
                "href": format!("https://{}/nodeinfo/2.0", state.domain)
            }]
        })),
    )
}

async fn nodeinfo(State(state): State<Arc<AppState>>) -> impl IntoResponse {
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

    let doc = serde_json::json!({
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
    });
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/json; profile=\"http://nodeinfo.diaspora.software/ns/schema/2.0#\"",
            ),
            (axum::http::header::CACHE_CONTROL, "public, max-age=300"),
        ],
        Json(doc),
    )
}

// --- Actor ---

async fn actor(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Content negotiation: if the client prefers HTML, serve a profile page
    let wants_html = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/html") && !v.contains("application/activity+json"))
        .unwrap_or(false);

    if wants_html {
        return serve_profile_html(&state, &username).await;
    }
    let row = sqlx::query_as::<_, (String, String, String, String, String, Option<String>, Option<String>, String, String)>(
        "SELECT id, username, display_name, bio, public_key, avatar_path, header_path, created_at, metadata \
         FROM personas WHERE username = ?",
    )
    .bind(&username)
    .fetch_optional(&state.pool)
    .await;

    let (
        _id,
        username,
        display_name,
        bio,
        public_key,
        avatar_path,
        header_path,
        created_at,
        metadata_json,
    ) = match row {
        Ok(Some(r)) => r,
        _ => return (StatusCode::NOT_FOUND, "unknown user").into_response(),
    };

    let actor_uri = format!("https://{}/users/{}", state.domain, username);

    let mut doc = serde_json::json!({
        "@context": [
            "https://www.w3.org/ns/activitystreams",
            "https://w3id.org/security/v1"
        ],
        "id": actor_uri,
        "type": "Person",
        "preferredUsername": username,
        "name": display_name,
        "summary": bio,
        "published": created_at,
        "inbox": format!("{}/inbox", actor_uri),
        "outbox": format!("{}/outbox", actor_uri),
        "followers": format!("{}/followers", actor_uri),
        "following": format!("{}/following", actor_uri),
        "url": actor_uri,
        "discoverable": true,
        "manuallyApprovesFollowers": false,
        "endpoints": {
            "sharedInbox": format!("https://{}/inbox", state.domain)
        },
        "publicKey": {
            "id": format!("{}#main-key", actor_uri),
            "owner": actor_uri,
            "publicKeyPem": public_key
        }
    });

    if let Some(ref avatar) = avatar_path {
        doc["icon"] = serde_json::json!({
            "type": "Image",
            "mediaType": "image/png",
            "url": format!("https://{}/{}", state.domain, avatar)
        });
    }
    if let Some(ref header) = header_path {
        doc["image"] = serde_json::json!({
            "type": "Image",
            "mediaType": "image/png",
            "url": format!("https://{}/{}", state.domain, header)
        });
    }

    // Profile metadata fields (e.g., "Website", "GitHub")
    // Stored as JSON array of {"name": "...", "value": "..."} objects
    if let Ok(fields) = serde_json::from_str::<Vec<serde_json::Value>>(&metadata_json) {
        if !fields.is_empty() {
            let attachments: Vec<serde_json::Value> = fields
                .iter()
                .map(|f| {
                    serde_json::json!({
                        "type": "PropertyValue",
                        "name": f["name"],
                        "value": f["value"]
                    })
                })
                .collect();
            doc["attachment"] = serde_json::json!(attachments);
        }
    }

    (
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/activity+json",
            ),
            (axum::http::header::CACHE_CONTROL, "public, max-age=300"),
            (axum::http::header::VARY, "Accept"),
        ],
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
            [
                (
                    axum::http::header::CONTENT_TYPE,
                    "application/activity+json",
                ),
                (axum::http::header::CACHE_CONTROL, "public, max-age=60"),
            ],
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
    let mut items: Vec<serde_json::Value> = Vec::with_capacity(posts.len());
    for p in &posts {
        let post_uri = format!("{}/statuses/{}", actor_uri, p.id);
        let (processed_html, tags) =
            crate::content::process_content(&p.content_html, &state.domain);
        let tag_json: Vec<serde_json::Value> = tags
            .iter()
            .map(|t| serde_json::to_value(t).unwrap_or_default())
            .collect();
        let attachments =
            crate::media::attachments_for_post(&state.pool, &p.id, &state.domain).await;
        items.push(serde_json::json!({
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
                "content": processed_html,
                "published": p.published_at,
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
                "cc": [format!("{}/followers", actor_uri)],
                "tag": tag_json,
                "attachment": attachments,
            }
        }));
    }

    let doc = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}?page={}", outbox_uri, page),
        "type": "OrderedCollectionPage",
        "partOf": outbox_uri,
        "orderedItems": items
    });

    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/activity+json",
            ),
            (axum::http::header::CACHE_CONTROL, "public, max-age=60"),
        ],
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
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/activity+json",
            ),
            (axum::http::header::CACHE_CONTROL, "public, max-age=300"),
        ],
        Json(doc),
    )
        .into_response()
}

// --- Following collection (always empty — broadside is one-way) ---

async fn following_collection(
    State(state): State<Arc<AppState>>,
    Path(username): Path<String>,
) -> impl IntoResponse {
    if crate::persona::get_id(&state.pool, &username)
        .await
        .is_err()
    {
        return (StatusCode::NOT_FOUND, "unknown user").into_response();
    }

    let doc = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("https://{}/users/{}/following", state.domain, username),
        "type": "OrderedCollection",
        "totalItems": 0
    });

    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/activity+json",
            ),
            (axum::http::header::CACHE_CONTROL, "public, max-age=3600"),
        ],
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

    // Parse Signature header and extract keyId, signed headers list
    let sig_parts = match crate::signatures::parse_signature_header(&sig_header) {
        Ok(p) => p,
        Err(_) => return StatusCode::BAD_REQUEST,
    };

    // Require digest and date in the signed headers to prevent body substitution and replay
    let signed_header_names: Vec<&str> = sig_parts.headers.split_whitespace().collect();
    if !signed_header_names.contains(&"digest") {
        tracing::debug!("rejecting signature that does not cover digest header");
        return StatusCode::BAD_REQUEST;
    }
    if !signed_header_names.contains(&"date") {
        tracing::debug!("rejecting signature that does not cover date header");
        return StatusCode::BAD_REQUEST;
    }

    let actor_uri = sig_parts
        .key_id
        .split('#')
        .next()
        .unwrap_or(&sig_parts.key_id)
        .to_string();

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

    // Parse activity (serde_json >= 1.0.111 enforces 128-level recursion limit)
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
                    if is_private_host_resolved(host).await {
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
                    let body = match crate::http::read_body_limited(resp, 65536).await {
                        Ok(b) => b,
                        Err(e) => {
                            tracing::warn!("actor document fetch failed: {e}");
                            return StatusCode::ACCEPTED;
                        }
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
            let shared_inbox_uri = match actor_doc["endpoints"]["sharedInbox"]
                .as_str()
                .filter(|s| s.starts_with("https://"))
            {
                Some(uri) => {
                    let host = url::Url::parse(uri)
                        .ok()
                        .and_then(|u| u.host_str().map(|h| h.to_string()));
                    match host {
                        Some(h) if !is_private_host_resolved(&h).await => Some(uri.to_string()),
                        _ => None,
                    }
                }
                None => None,
            };

            if inbox_uri.is_empty() || !inbox_uri.starts_with("https://") {
                return StatusCode::ACCEPTED;
            }
            if let Ok(parsed) = url::Url::parse(&inbox_uri) {
                if let Some(host) = parsed.host_str() {
                    if is_private_host_resolved(host).await {
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
        "Accept" => {
            // A remote actor accepted our Follow — check if it's a relay activation
            if let Some(relay_actor) = activity["actor"].as_str() {
                if crate::relay::activate(&state.pool, relay_actor)
                    .await
                    .unwrap_or(false)
                {
                    tracing::info!(relay = %relay_actor, "relay subscription activated");
                }
            }
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

// --- Index ---

async fn index(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT username, display_name FROM personas ORDER BY username",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    let mut personas_html = String::new();
    for (username, display_name) in &rows {
        let _ = write!(
            personas_html,
            r#"<li><a href="/users/{u}"><strong>{dn}</strong> <span>@{u}@{domain}</span></a></li>"#,
            u = ammonia::clean(username),
            dn = ammonia::clean(display_name),
            domain = state.domain,
        );
    }

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>{domain} — Broadside</title>
    <style>{base_css}
        p {{ color: var(--muted); margin-bottom: 1.5rem; }}
        ul {{ list-style: none; }}
        li {{ padding: 0.75rem 0; border-bottom: 1px solid var(--border); }}
        li a {{ text-decoration: none; color: var(--text); display: block; }}
        li a:hover {{ color: var(--link); }}
        li span {{ color: var(--muted); font-size: 0.9rem; margin-left: 0.5rem; }}
        footer {{ margin-top: 2rem; color: var(--muted); font-size: 0.8rem; }}
    </style>
    <style>{extra_css}</style>
</head>
<body>
    <main>
        <h1>Broadside</h1>
        <p>ActivityPub broadcast server on {domain}</p>
        <ul>{personas_html}</ul>
        <footer>Powered by <a href="https://github.com">Broadside</a> v{version}</footer>
    </main>
</body>
</html>"#,
        base_css = BASE_CSS,
        domain = state.domain,
        personas_html = personas_html,
        extra_css = state.extra_css,
        version = env!("CARGO_PKG_VERSION"),
    );

    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=60"),
        ],
        html,
    )
        .into_response()
}

/// Serve a simple HTML profile page for browser visitors.
async fn serve_profile_html(state: &AppState, username: &str) -> axum::response::Response {
    let row = sqlx::query_as::<_, (String, String, String, String)>(
        "SELECT display_name, bio, created_at, metadata FROM personas WHERE username = ?",
    )
    .bind(username)
    .fetch_optional(&state.pool)
    .await;

    let (display_name, bio, created_at, metadata_json) = match row {
        Ok(Some(r)) => r,
        _ => return (StatusCode::NOT_FOUND, "unknown user").into_response(),
    };

    let persona_id = match crate::persona::get_id(&state.pool, username).await {
        Ok(id) => id,
        Err(_) => return (StatusCode::NOT_FOUND, "unknown user").into_response(),
    };

    let posts = crate::post::list_for_persona(&state.pool, &persona_id, 20, 0)
        .await
        .unwrap_or_default();

    let actor_uri = format!("https://{}/users/{}", state.domain, username);

    // Build metadata fields HTML (Mastodon-style table)
    let mut fields_html = String::new();
    if let Ok(fields) = serde_json::from_str::<Vec<serde_json::Value>>(&metadata_json) {
        if !fields.is_empty() {
            fields_html.push_str("<table class=\"fields\">");
            for f in &fields {
                let name = f["name"].as_str().unwrap_or("");
                let value = f["value"].as_str().unwrap_or("");
                let value_html = if value.starts_with("https://") || value.starts_with("http://") {
                    format!(
                        r#"<a href="{v}" rel="nofollow noopener noreferrer">{v}</a>"#,
                        v = ammonia::clean(value)
                    )
                } else {
                    ammonia::clean(value).to_string()
                };
                let _ = write!(
                    fields_html,
                    "<tr><th>{}</th><td>{}</td></tr>",
                    ammonia::clean(name),
                    value_html
                );
            }
            fields_html.push_str("</table>");
        }
    }

    // Build posts HTML
    let mut posts_html = String::new();
    for p in &posts {
        let (processed, _) = crate::content::process_content(&p.content_html, &state.domain);
        let date_display = p.published_at.get(..10).unwrap_or(&p.published_at);
        let _ = write!(
            posts_html,
            r#"<article class="post">
                <div class="post-content">{content}</div>
                <footer><time datetime="{ts}">{date}</time></footer>
            </article>"#,
            content = processed,
            ts = p.published_at,
            date = date_display,
        );
    }

    let bio_html = if bio.is_empty() {
        String::new()
    } else {
        format!("<div class=\"bio\">{}</div>", ammonia::clean(&bio))
    };

    let follower_count =
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM followers WHERE persona_id = ?")
            .bind(&persona_id)
            .fetch_one(&state.pool)
            .await
            .map(|(c,)| c)
            .unwrap_or(0);

    let post_count = crate::post::count_for_persona(&state.pool, &persona_id)
        .await
        .unwrap_or(0);

    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>@{username}@{domain} — {display_name}</title>
    <link rel="alternate" type="application/activity+json" href="{actor_uri}">
    <style>{base_css}
        h1 {{ margin-bottom: 0.15rem; }}
        h2 {{ font-size: 1.1rem; font-weight: 600; color: var(--muted); text-transform: uppercase;
              letter-spacing: 0.05em; margin: 1.5rem 0 0.75rem; }}
        .handle {{ color: var(--muted); font-size: 0.95rem; margin-bottom: 1rem; }}
        .bio {{ margin-bottom: 1rem; }}
        table {{ width: 100%; border-collapse: collapse; margin-bottom: 1rem; }}
        th, td {{ padding: 0.6rem 0.8rem; text-align: left; border-bottom: 1px solid var(--border); }}
        th {{ background: var(--card); color: var(--muted); font-size: 0.8rem; font-weight: 600;
             text-transform: uppercase; letter-spacing: 0.04em; width: 30%; }}
        td {{ font-size: 0.95rem; }}
        .meta {{ color: var(--muted); font-size: 0.85rem; margin-bottom: 1.5rem; }}
        hr {{ border: none; border-top: 1px solid var(--border); margin: 1.5rem 0; }}
        article {{ padding: 1rem 0; border-bottom: 1px solid var(--border); }}
        article p {{ margin-bottom: 0.5rem; }}
        article time {{ color: var(--muted); font-size: 0.8rem; }}
    </style>
    <style>{extra_css}</style>
</head>
<body>
    <main>
        <h1>{display_name}</h1>
        <p class="handle">@{username}@{domain}</p>
        {bio_html}
        {fields_html}
        <p class="meta">{post_count} posts · {follower_count} followers · Joined {created_at}</p>
        <hr>
        <h2>Posts</h2>
        {posts_html}
    </main>
</body>
</html>"#,
        base_css = BASE_CSS,
        username = ammonia::clean(username),
        domain = state.domain,
        display_name = ammonia::clean(&display_name),
        actor_uri = actor_uri,
        extra_css = state.extra_css,
        bio_html = bio_html,
        fields_html = fields_html,
        post_count = post_count,
        follower_count = follower_count,
        created_at = created_at.get(..10).unwrap_or(&created_at),
        posts_html = posts_html,
    );

    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "public, max-age=60"),
            (axum::http::header::VARY, "Accept"),
        ],
        html,
    )
        .into_response()
}

/// Check if an IP address is private/loopback/link-local.
fn is_private_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_loopback() || v4.is_private() || v4.is_link_local())
        }
    }
}

/// Check if a hostname string is a known private/loopback name or IP literal.
pub fn is_private_host(host: &str) -> bool {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return is_private_ip(ip);
    }
    host == "localhost" || host.ends_with(".local") || host.ends_with(".internal")
}

/// Check if a hostname resolves to private/internal addresses via DNS.
/// Fails closed: returns true (blocked) if DNS resolution fails.
pub async fn is_private_host_resolved(host: &str) -> bool {
    if is_private_host(host) {
        return true;
    }
    match tokio::net::lookup_host(format!("{host}:443")).await {
        Ok(mut addrs) => addrs.any(|addr| is_private_ip(addr.ip())),
        Err(_) => true,
    }
}

/// Validate that a persona username contains only safe characters for ActivityPub URIs.
pub fn is_valid_username(username: &str) -> bool {
    !username.is_empty()
        && username.len() <= 64
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
