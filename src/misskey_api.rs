//! Misskey-compatible API endpoints for broadside.
//!
//! Broadside is a one-way publisher with no user authentication,
//! so only read-only endpoints are served: notes/show, notes/local-timeline,
//! and users/show. Write operations (create, reactions) are not available.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::server::AppState;

// ---------------------------------------------------------------------------
// Request types (Misskey sends everything as POST JSON)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[allow(dead_code)]
struct NoteShowRequest {
    i: Option<String>,
    #[serde(rename = "noteId")]
    note_id: String,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct TimelineRequest {
    i: Option<String>,
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(rename = "sinceId")]
    since_id: Option<String>,
    #[serde(rename = "untilId")]
    until_id: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct UserShowRequest {
    i: Option<String>,
    #[serde(rename = "userId")]
    user_id: Option<String>,
    username: Option<String>,
}

fn default_limit() -> i64 {
    20
}

// ---------------------------------------------------------------------------
// JSON builders
// ---------------------------------------------------------------------------

fn epoch_to_misskey_date(epoch: i64) -> String {
    chrono::DateTime::from_timestamp(epoch, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_default()
}

fn post_to_note(
    post: &fieldwork_db::posts_db::PostRow,
    persona: &fieldwork_db::persona_db::PersonaRow,
    domain: &str,
) -> Value {
    json!({
        "id": post.id.to_string(),
        "createdAt": epoch_to_misskey_date(post.created_at),
        "text": post.content,
        "cw": if post.spoiler_text.is_empty() { None } else { Some(&post.spoiler_text) },
        "visibility": misskey_visibility(&post.visibility),
        "uri": format!("https://{domain}/users/{}/statuses/{}", persona.username, post.id),
        "url": format!("https://{domain}/users/{}/statuses/{}", persona.username, post.id),
        "user": persona_to_misskey_user(persona, domain),
        "reactions": {},
        "myReaction": null,
        "replyId": post.in_reply_to_id.map(|id| id.to_string()),
        "renoteId": post.boost_of_id.map(|id| id.to_string()),
    })
}

fn persona_to_misskey_user(
    p: &fieldwork_db::persona_db::PersonaRow,
    domain: &str,
) -> Value {
    json!({
        "id": p.id.to_string(),
        "username": p.username,
        "name": p.display_name,
        "host": null,
        "description": p.bio,
        "isBot": p.bot,
        "isLocked": p.is_locked,
        "url": format!("https://{domain}/@{}", p.username),
        "avatarUrl": null,
        "bannerUrl": null,
    })
}

fn misskey_visibility(mastodon_vis: &str) -> &str {
    match mastodon_vis {
        "public" => "public",
        "unlisted" => "home",
        "private" => "followers",
        "direct" => "specified",
        _ => "public",
    }
}

// ---------------------------------------------------------------------------
// POST /api/notes/show
// ---------------------------------------------------------------------------

async fn notes_show(
    State(state): State<Arc<AppState>>,
    Json(body): Json<NoteShowRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let note_id: i64 = body.note_id.parse().map_err(|_| {
        (StatusCode::BAD_REQUEST, Json(json!({"error": {"message": "Invalid noteId"}})))
    })?;

    let post = fieldwork_db::posts_db::get_post(&state.pool, note_id)
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"message": "Database error"}}))))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, Json(json!({"error": {"message": "Note not found"}}))))?;

    if post.deleted_at.is_some() {
        return Err((StatusCode::NOT_FOUND, Json(json!({"error": {"message": "Note not found"}}))));
    }

    let persona = fieldwork_db::persona_db::get_persona_by_id(&state.pool, post.persona_id)
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"message": "Database error"}}))))?
        .ok_or_else(|| (StatusCode::NOT_FOUND, Json(json!({"error": {"message": "User not found"}}))))?;

    let reactions = fieldwork_db::reactions_db::reactions_for_post(&state.pool, post.id)
        .await
        .unwrap_or_default();
    let mut note = post_to_note(&post, &persona, &state.domain);
    let reactions_map: serde_json::Map<String, Value> = reactions
        .into_iter()
        .map(|(emoji, count)| (emoji, json!(count)))
        .collect();
    note["reactions"] = Value::Object(reactions_map);

    Ok(Json(note))
}

// ---------------------------------------------------------------------------
// POST /api/notes/local-timeline
// ---------------------------------------------------------------------------

async fn notes_local_timeline(
    State(state): State<Arc<AppState>>,
    Json(body): Json<TimelineRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let limit = body.limit.clamp(1, 40);
    let max_id = body.until_id.as_deref().and_then(|s| s.parse::<i64>().ok());

    let posts = fieldwork_db::timeline_db::public_timeline(&state.pool, limit, max_id)
        .await
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"message": "Database error"}}))))?;

    let mut notes = Vec::with_capacity(posts.len());
    for post in &posts {
        if post.deleted_at.is_some() {
            continue;
        }
        let persona = match fieldwork_db::persona_db::get_persona_by_id(&state.pool, post.persona_id).await {
            Ok(Some(p)) => p,
            _ => continue,
        };
        notes.push(post_to_note(post, &persona, &state.domain));
    }

    Ok(Json(json!(notes)))
}

// ---------------------------------------------------------------------------
// POST /api/users/show
// ---------------------------------------------------------------------------

async fn users_show(
    State(state): State<Arc<AppState>>,
    Json(body): Json<UserShowRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let persona = if let Some(uid) = &body.user_id {
        let id: i64 = uid.parse().map_err(|_| {
            (StatusCode::BAD_REQUEST, Json(json!({"error": {"message": "Invalid userId"}})))
        })?;
        fieldwork_db::persona_db::get_persona_by_id(&state.pool, id)
            .await
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"message": "Database error"}}))))?
    } else if let Some(username) = &body.username {
        fieldwork_db::persona_db::get_persona_by_username(&state.pool, username)
            .await
            .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"message": "Database error"}}))))?
    } else {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": {"message": "userId or username required"}}))));
    };

    let persona = persona.ok_or_else(|| {
        (StatusCode::NOT_FOUND, Json(json!({"error": {"message": "User not found"}})))
    })?;

    Ok(Json(persona_to_misskey_user(&persona, &state.domain)))
}

// ---------------------------------------------------------------------------
// Routes (read-only for broadside)
// ---------------------------------------------------------------------------

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/notes/show", post(notes_show))
        .route("/api/notes/local-timeline", post(notes_local_timeline))
        .route("/api/users/show", post(users_show))
}
