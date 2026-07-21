//! PeerTube-compatible API v1 endpoints for broadside.
//!
//! Broadside is a one-way publisher with no user authentication,
//! so only read-only endpoints are served: listing videos, channels,
//! and comments. Upload/create operations are not available.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
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
#[allow(dead_code)]
struct ListVideosQuery {
    #[serde(default = "default_limit")]
    count: i64,
    #[serde(default)]
    start: i64,
    #[serde(default)]
    sort: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct ListChannelVideosQuery {
    #[serde(default = "default_limit")]
    count: i64,
    #[serde(default)]
    start: i64,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct ListChannelsQuery {
    #[serde(default = "default_limit")]
    count: i64,
    #[serde(default)]
    start: i64,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct JobsQuery {
    #[serde(default = "default_limit")]
    count: i64,
    #[serde(default)]
    start: i64,
}

fn default_limit() -> i64 {
    15
}

fn now_iso() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

fn epoch_to_iso(epoch: i64) -> String {
    chrono::DateTime::from_timestamp(epoch, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_else(|| now_iso())
}

// ---------------------------------------------------------------------------
// JSON builders
// ---------------------------------------------------------------------------

fn persona_to_channel(
    p: &fieldwork_db::persona_db::PersonaRow,
    domain: &str,
    follower_count: i64,
) -> serde_json::Value {
    serde_json::json!({
        "id": p.id,
        "name": p.username,
        "displayName": p.display_name,
        "description": p.bio,
        "url": format!("https://{}/video-channels/{}", domain, p.username),
        "host": domain,
        "followersCount": follower_count,
        "followingCount": 0,
        "isLocal": true,
        "createdAt": epoch_to_iso(p.created_at),
        "updatedAt": epoch_to_iso(p.created_at),
        "ownerAccount": {
            "id": p.id,
            "name": p.username,
            "displayName": p.display_name,
            "host": domain,
            "url": format!("https://{}/users/{}", domain, p.username),
        }
    })
}

fn post_to_video(
    post: &fieldwork_db::posts_db::PostRow,
    persona: &fieldwork_db::persona_db::PersonaRow,
    domain: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": post.id,
        "uuid": format!("{:016x}", post.id),
        "name": if post.spoiler_text.is_empty() { "Untitled" } else { &post.spoiler_text },
        "description": post.content_html,
        "isLocal": true,
        "duration": 0,
        "views": 0,
        "likes": 0,
        "dislikes": 0,
        "nsfw": false,
        "state": { "id": 1, "label": "Published" },
        "publishedAt": epoch_to_iso(post.created_at),
        "createdAt": epoch_to_iso(post.created_at),
        "updatedAt": epoch_to_iso(post.created_at),
        "url": format!("https://{}/users/{}/statuses/{}", domain, persona.username, post.id),
        "channel": {
            "id": persona.id,
            "name": persona.username,
            "displayName": persona.display_name,
            "host": domain,
        },
        "account": {
            "id": persona.id,
            "name": persona.username,
            "displayName": persona.display_name,
            "host": domain,
        },
        "files": [],
        "streamingPlaylists": [],
    })
}

// ---------------------------------------------------------------------------
// GET /api/v1/videos
// ---------------------------------------------------------------------------

async fn list_videos(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListVideosQuery>,
) -> impl IntoResponse {
    let limit = query.count.clamp(1, 100);

    // ponytail: broadside has no dedicated video table. Serve posts as
    // PeerTube "videos" for compatibility. Ceiling: add a videos table
    // when actual video upload support is needed.
    let personas = fieldwork_db::persona_db::list_personas(&state.pool)
        .await
        .unwrap_or_default();

    let mut videos = Vec::new();
    for p in &personas {
        let posts = fieldwork_db::posts_db::posts_by_persona(
            &state.pool,
            p.id,
            limit,
            None,
        )
        .await
        .unwrap_or_default();

        for post in &posts {
            videos.push(post_to_video(post, p, &state.domain));
        }
    }

    videos.truncate(limit as usize);

    Json(serde_json::json!({
        "total": videos.len(),
        "data": videos,
    }))
}

// ---------------------------------------------------------------------------
// GET /api/v1/videos/{id}
// ---------------------------------------------------------------------------

async fn get_video(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let post = match fieldwork_db::posts_db::get_post(&state.pool, id).await {
        Ok(Some(p)) => p,
        Ok(None) => return (StatusCode::NOT_FOUND, "video not found").into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let persona = match fieldwork_db::persona_db::get_persona_by_id(
        &state.pool,
        post.persona_id,
    )
    .await
    {
        Ok(Some(p)) => p,
        _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    Json(post_to_video(&post, &persona, &state.domain)).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/video-channels
// ---------------------------------------------------------------------------

async fn list_channels(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListChannelsQuery>,
) -> impl IntoResponse {
    let limit = query.count.clamp(1, 100);

    let personas = fieldwork_db::persona_db::list_personas(&state.pool)
        .await
        .unwrap_or_default();

    let mut channels = Vec::with_capacity(personas.len());
    for p in &personas {
        let follower_count = fieldwork_db::followers_db::follower_count(&state.pool, p.id)
            .await
            .unwrap_or(0);
        channels.push(persona_to_channel(p, &state.domain, follower_count));
    }

    channels.truncate(limit as usize);

    Json(serde_json::json!({
        "total": channels.len(),
        "data": channels,
    }))
}

// ---------------------------------------------------------------------------
// GET /api/v1/video-channels/{name}
// ---------------------------------------------------------------------------

async fn get_channel(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let persona = match fieldwork_db::persona_db::get_persona_by_username(
        &state.pool,
        &name,
    )
    .await
    {
        Ok(Some(p)) => p,
        Ok(None) => return (StatusCode::NOT_FOUND, "channel not found").into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let follower_count = fieldwork_db::followers_db::follower_count(&state.pool, persona.id)
        .await
        .unwrap_or(0);

    Json(persona_to_channel(&persona, &state.domain, follower_count)).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/video-channels/{name}/videos
// ---------------------------------------------------------------------------

async fn list_channel_videos(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(query): Query<ListChannelVideosQuery>,
) -> impl IntoResponse {
    let limit = query.count.clamp(1, 100);

    let persona = match fieldwork_db::persona_db::get_persona_by_username(
        &state.pool,
        &name,
    )
    .await
    {
        Ok(Some(p)) => p,
        Ok(None) => return (StatusCode::NOT_FOUND, "channel not found").into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let posts = fieldwork_db::posts_db::posts_by_persona(
        &state.pool,
        persona.id,
        limit,
        None,
    )
    .await
    .unwrap_or_default();

    let videos: Vec<serde_json::Value> = posts
        .iter()
        .map(|p| post_to_video(p, &persona, &state.domain))
        .collect();

    Json(serde_json::json!({
        "total": videos.len(),
        "data": videos,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/videos/{id}/comment-threads
// ---------------------------------------------------------------------------

async fn get_comment_threads(
    State(_state): State<Arc<AppState>>,
    Path(_id): Path<i64>,
) -> impl IntoResponse {
    // ponytail: broadside has no comment storage. Return empty.
    // Ceiling: implement when comment DB layer exists.
    Json(serde_json::json!({
        "total": 0,
        "data": [],
    }))
}

// ---------------------------------------------------------------------------
// GET /api/v1/jobs/{state} — admin only, broadside returns empty
// ---------------------------------------------------------------------------

async fn list_jobs(
    State(_state): State<Arc<AppState>>,
    Path(_job_state): Path<String>,
    Query(_query): Query<JobsQuery>,
) -> impl IntoResponse {
    Json(serde_json::json!({
        "total": 0,
        "data": [],
    }))
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/videos", get(list_videos))
        .route("/api/v1/videos/{id}", get(get_video))
        .route("/api/v1/videos/{id}/comment-threads", get(get_comment_threads))
        .route("/api/v1/video-channels", get(list_channels))
        .route("/api/v1/video-channels/{name}", get(get_channel))
        .route("/api/v1/video-channels/{name}/videos", get(list_channel_videos))
        .route("/api/v1/jobs/{state}", get(list_jobs))
}
