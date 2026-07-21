//! Pixelfed-compatible API endpoints for broadside.
//!
//! Broadside has no user authentication, so only the public discover
//! endpoints are served. Collection CRUD is not available.

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use std::sync::Arc;

use crate::server::AppState;

// ---------------------------------------------------------------------------
// Query params
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DiscoverQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default = "default_range")]
    range: String,
}

fn default_limit() -> i64 {
    20
}

fn default_range() -> String {
    "daily".into()
}

fn range_to_since(range: &str) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let duration = match range {
        "weekly" => 7 * 24 * 3600,
        "monthly" => 30 * 24 * 3600,
        _ => 24 * 3600, // daily
    };
    now - duration
}

// ---------------------------------------------------------------------------
// GET /api/pixelfed/v1/discover/posts
// ---------------------------------------------------------------------------

async fn discover_posts(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DiscoverQuery>,
) -> impl IntoResponse {
    let limit = query.limit.clamp(1, 40);
    let since = range_to_since(&query.range);

    let post_ids = fieldwork_db::trending_db::trending_posts(&state.pool, limit, since)
        .await
        .unwrap_or_default();

    let mut statuses = Vec::with_capacity(post_ids.len());
    for post_id in post_ids {
        let post = match fieldwork_db::posts_db::get_post(&state.pool, post_id).await {
            Ok(Some(p)) if p.visibility == "public" => p,
            _ => continue,
        };
        let persona = match fieldwork_db::persona_db::get_persona_by_id(
            &state.pool,
            post.persona_id,
        )
        .await
        {
            Ok(Some(p)) => p,
            _ => continue,
        };

        let created_at = chrono::DateTime::from_timestamp(post.created_at, 0)
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_default();

        let actor_uri = format!("https://{}/users/{}", state.domain, persona.username);
        statuses.push(serde_json::json!({
            "id": post.id.to_string(),
            "created_at": created_at,
            "visibility": post.visibility,
            "content": post.content_html,
            "uri": format!("{}/statuses/{}", actor_uri, post.id),
            "url": format!("{}/statuses/{}", actor_uri, post.id),
            "account": {
                "id": persona.id.to_string(),
                "username": persona.username,
                "acct": persona.username,
                "display_name": persona.display_name,
                "url": actor_uri,
            },
            "media_attachments": [],
            "mentions": [],
            "tags": [],
            "emojis": [],
            "replies_count": 0,
            "reblogs_count": 0,
            "favourites_count": 0,
        }));
    }

    Json(serde_json::Value::Array(statuses))
}

// ---------------------------------------------------------------------------
// GET /api/pixelfed/v1/discover/hashtags
// ---------------------------------------------------------------------------

async fn discover_hashtags(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DiscoverQuery>,
) -> impl IntoResponse {
    let limit = query.limit.clamp(1, 40);
    let since = range_to_since(&query.range);

    let tags = fieldwork_db::trending_db::trending_tags(&state.pool, limit, since)
        .await
        .unwrap_or_default();

    let day_str = (since as u64).to_string();
    let results: Vec<serde_json::Value> = tags
        .into_iter()
        .map(|(name, count)| {
            serde_json::json!({
                "name": name,
                "url": format!("https://{}/tags/{}", state.domain, name),
                "history": [
                    { "day": day_str, "uses": count.to_string(), "accounts": count.to_string() }
                ]
            })
        })
        .collect();

    Json(serde_json::Value::Array(results))
}

// ---------------------------------------------------------------------------
// GET /api/pixelfed/v1/discover/accounts
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AccountsQuery {
    #[serde(default = "default_limit")]
    limit: i64,
}

async fn discover_accounts(
    State(state): State<Arc<AppState>>,
    Query(query): Query<AccountsQuery>,
) -> impl IntoResponse {
    let limit = query.limit.clamp(1, 40);

    let personas = fieldwork_db::persona_db::list_personas(&state.pool)
        .await
        .unwrap_or_default();

    // Sort by follower count descending, take limit
    let mut persona_counts: Vec<(fieldwork_db::persona_db::PersonaRow, i64)> =
        Vec::with_capacity(personas.len());
    for p in personas {
        let count = fieldwork_db::followers_db::follower_count(&state.pool, p.id)
            .await
            .unwrap_or(0);
        persona_counts.push((p, count));
    }
    persona_counts.sort_by(|a, b| b.1.cmp(&a.1));
    persona_counts.truncate(limit as usize);

    let results: Vec<serde_json::Value> = persona_counts
        .into_iter()
        .map(|(p, followers_count)| {
            let actor_uri = format!("https://{}/users/{}", state.domain, p.username);
            serde_json::json!({
                "id": p.id.to_string(),
                "username": p.username,
                "acct": p.username,
                "display_name": p.display_name,
                "url": actor_uri,
                "followers_count": followers_count,
                "following_count": 0,
                "statuses_count": 0,
                "note": p.bio,
                "avatar": format!("https://{}/avatars/original/missing.png", state.domain),
                "header": format!("https://{}/headers/original/missing.png", state.domain),
            })
        })
        .collect();

    Json(serde_json::Value::Array(results))
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/pixelfed/v1/discover/posts", get(discover_posts))
        .route(
            "/api/pixelfed/v1/discover/hashtags",
            get(discover_hashtags),
        )
        .route(
            "/api/pixelfed/v1/discover/accounts",
            get(discover_accounts),
        )
}
