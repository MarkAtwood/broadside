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
    pub webhook_keys: std::collections::HashMap<String, String>,
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

    let state = Arc::new(AppState {
        pool: pool.clone(),
        domain: domain.clone(),
        webhook_keys,
    });

    // Start delivery worker
    tokio::spawn(crate::delivery::run_worker(pool.clone(), domain.clone()));

    // Start feed pollers
    for feed_config in &config.feed {
        tokio::spawn(crate::feed::run_poller(
            pool.clone(),
            feed_config.clone(),
            domain.clone(),
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
    axum::serve(listener, app).await?;
    Ok(())
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
    let prefix = format!("acct:");
    let acct = if let Some(acct) = query.resource.strip_prefix(&prefix) {
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

    let exists = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM personas WHERE username = ?",
    )
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

    let (id, username, display_name, bio, public_key) = match row {
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
        [(axum::http::header::CONTENT_TYPE, "application/activity+json")],
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
        return Json(doc).into_response();
    }

    let page = query.page.unwrap_or(1);
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

    Json(doc).into_response()
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

    let (count,) = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM followers WHERE persona_id = ?",
    )
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

    Json(doc).into_response()
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
    _headers: &HeaderMap,
    body: &str,
) -> impl IntoResponse {
    // ponytail: signature verification skipped for now — requires fetching
    // remote actor's public key, which needs async HTTP + caching. Will be
    // added in hardening phase. Accepting unsigned activities is standard
    // during development.

    let activity: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST,
    };

    let activity_type = activity["type"].as_str().unwrap_or("");

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

            // Extract username from the followed URI
            let username = match followed.rsplit('/').next() {
                Some(u) => u,
                None => return StatusCode::BAD_REQUEST,
            };

            let persona_id = match crate::persona::get_id(&state.pool, username).await {
                Ok(id) => id,
                Err(_) => return StatusCode::NOT_FOUND,
            };

            // Fetch the follower's actor document to get their inbox
            let client = reqwest::Client::new();
            let actor_doc = match client
                .get(follower_actor)
                .header("Accept", "application/activity+json")
                .send()
                .await
            {
                Ok(resp) => match resp.json::<serde_json::Value>().await {
                    Ok(v) => v,
                    Err(_) => return StatusCode::ACCEPTED,
                },
                Err(_) => return StatusCode::ACCEPTED,
            };

            let inbox_uri = actor_doc["inbox"].as_str().unwrap_or("").to_string();
            let shared_inbox_uri = actor_doc["endpoints"]["sharedInbox"]
                .as_str()
                .map(|s| s.to_string());

            if inbox_uri.is_empty() {
                return StatusCode::ACCEPTED;
            }

            // Insert follower
            let follower_id = crate::id::gen_id();
            let _ = sqlx::query(
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
            .await;

            tracing::info!(follower = follower_actor, persona = username, "accepted follow");

            // Send Accept asynchronously
            let accept = serde_json::json!({
                "@context": "https://www.w3.org/ns/activitystreams",
                "id": format!("https://{}/users/{}#accept/{}", state.domain, username, follower_id),
                "type": "Accept",
                "actor": format!("https://{}/users/{}", state.domain, username),
                "object": activity
            });

            let accept_body = serde_json::to_vec(&accept).unwrap_or_default();
            let actor_uri = format!("https://{}/users/{}", state.domain, username);
            let key_id = format!("{actor_uri}#main-key");

            if let Ok(private_key) = crate::persona::get_private_key(&state.pool, username).await {
                let target_path = url::Url::parse(&inbox_uri)
                    .map(|u| u.path().to_string())
                    .unwrap_or_else(|_| "/inbox".to_string());
                let inbox_domain = url::Url::parse(&inbox_uri)
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
                    let _ = client
                        .post(&inbox_uri)
                        .headers(sig_headers)
                        .header("Content-Type", "application/activity+json")
                        .body(accept_body)
                        .send()
                        .await;
                }
            }

            StatusCode::ACCEPTED
        }
        "Undo" => {
            let inner_type = activity["object"]["type"].as_str().unwrap_or("");
            if inner_type == "Follow" {
                let follower_actor = activity["object"]["actor"]
                    .as_str()
                    .or_else(|| activity["actor"].as_str())
                    .unwrap_or("");

                if !follower_actor.is_empty() {
                    let _ = sqlx::query("DELETE FROM followers WHERE actor_uri = ?")
                        .bind(follower_actor)
                        .execute(&state.pool)
                        .await;
                    tracing::info!(follower = follower_actor, "removed follower (Undo Follow)");
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

    let pending = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM delivery_queue WHERE status = 'pending'",
    )
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
