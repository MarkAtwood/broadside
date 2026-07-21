//! Lemmy-compatible API v3 endpoints for broadside.
//!
//! Broadside is a one-way publisher with no user authentication,
//! so only read-only endpoints are served: listing communities, posts,
//! comments, and site info. Create/vote/follow operations are not available.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use std::sync::Arc;

use crate::server::AppState;
use fieldwork::util::{epoch_to_iso, now_iso};

// ---------------------------------------------------------------------------
// Query params
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ListCommunitiesQuery {
    #[serde(default = "default_limit")]
    limit: i64,
}

#[derive(Deserialize)]
struct GetCommunityQuery {
    #[serde(default)]
    id: Option<i64>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct ListPostsQuery {
    #[serde(default)]
    community_id: Option<i64>,
    #[serde(default)]
    community_name: Option<String>,
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    page: Option<i64>,
    #[serde(default)]
    sort: Option<String>,
}

#[derive(Deserialize)]
struct GetPostQuery {
    id: i64,
}

#[derive(Deserialize)]
struct ListCommentsQuery {
    #[serde(default)]
    post_id: Option<i64>,
    #[serde(default)]
    sort: Option<String>,
}

fn default_limit() -> i64 {
    20
}

// ---------------------------------------------------------------------------
// JSON builders
// ---------------------------------------------------------------------------

fn community_to_json(
    c: &fieldwork_db::communities_db::CommunityRow,
    persona: &fieldwork_db::persona_db::PersonaRow,
    member_count: i64,
    domain: &str,
) -> serde_json::Value {
    serde_json::json!({
        "community": {
            "id": persona.id,
            "name": persona.username,
            "title": c.title,
            "description": c.description,
            "removed": false,
            "published": epoch_to_iso(c.created_at),
            "deleted": false,
            "nsfw": c.nsfw,
            "actor_id": format!("https://{}/users/{}", domain, persona.username),
            "local": true,
            "icon": null,
            "banner": null,
            "hidden": false,
            "posting_restricted_to_mods": c.posting_restricted,
            "instance_id": 1,
        },
        "subscribed": "NotSubscribed",
        "blocked": false,
        "counts": {
            "id": persona.id,
            "community_id": persona.id,
            "subscribers": member_count,
            "posts": 0,
            "comments": 0,
            "published": epoch_to_iso(c.created_at),
            "users_active_day": 0,
            "users_active_week": 0,
            "users_active_month": 0,
            "users_active_half_year": 0,
        },
    })
}

fn post_to_json(
    p: &fieldwork_db::communities_db::CommunityPostRow,
    author: &fieldwork_db::persona_db::PersonaRow,
    community_persona: &fieldwork_db::persona_db::PersonaRow,
    score: i64,
    domain: &str,
) -> serde_json::Value {
    let upvotes = score.max(0);
    let downvotes = (-score).max(0);
    serde_json::json!({
        "post": {
            "id": p.id,
            "name": p.title,
            "body": p.body,
            "creator_id": p.author_persona_id,
            "community_id": p.community_id,
            "removed": p.removed,
            "locked": p.locked,
            "published": epoch_to_iso(p.created_at),
            "updated": p.updated_at.map(epoch_to_iso),
            "deleted": false,
            "nsfw": false,
            "ap_id": p.ap_id,
            "local": true,
            "url": p.url,
            "featured_community": p.pinned,
            "featured_local": false,
        },
        "creator": {
            "id": author.id,
            "name": author.username,
            "display_name": author.display_name,
            "actor_id": format!("https://{}/users/{}", domain, author.username),
            "local": true,
            "deleted": false,
            "bot_account": false,
        },
        "community": {
            "id": community_persona.id,
            "name": community_persona.username,
            "title": community_persona.display_name,
            "actor_id": format!("https://{}/users/{}", domain, community_persona.username),
            "local": true,
        },
        "counts": {
            "id": p.id,
            "post_id": p.id,
            "comments": 0,
            "score": score,
            "upvotes": upvotes,
            "downvotes": downvotes,
            "published": epoch_to_iso(p.created_at),
        },
        "subscribed": "NotSubscribed",
        "saved": false,
        "read": false,
        "creator_blocked": false,
        "unread_comments": 0,
    })
}

fn comment_to_json(
    c: &fieldwork_db::communities_db::CommentRow,
    author: &fieldwork_db::persona_db::PersonaRow,
    score: i64,
    domain: &str,
) -> serde_json::Value {
    let upvotes = score.max(0);
    let downvotes = (-score).max(0);
    // Build a Lemmy-style path from parent_comment_id
    let path = match c.parent_comment_id {
        Some(pid) => format!("0.{}.{}", pid, c.id),
        None => format!("0.{}", c.id),
    };
    serde_json::json!({
        "comment": {
            "id": c.id,
            "creator_id": c.author_persona_id,
            "post_id": c.post_id,
            "content": c.content,
            "removed": c.removed,
            "published": epoch_to_iso(c.created_at),
            "updated": c.updated_at.map(epoch_to_iso),
            "deleted": false,
            "ap_id": c.ap_id,
            "local": true,
            "path": path,
            "distinguished": false,
        },
        "creator": {
            "id": author.id,
            "name": author.username,
            "display_name": author.display_name,
            "actor_id": format!("https://{}/users/{}", domain, author.username),
            "local": true,
            "deleted": false,
            "bot_account": false,
        },
        "post": {
            "id": c.post_id,
        },
        "counts": {
            "id": c.id,
            "comment_id": c.id,
            "score": score,
            "upvotes": upvotes,
            "downvotes": downvotes,
            "child_count": 0,
        },
        "subscribed": "NotSubscribed",
        "saved": false,
        "creator_blocked": false,
    })
}

// ---------------------------------------------------------------------------
// GET /api/v3/site
// ---------------------------------------------------------------------------

async fn get_site(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let personas = fieldwork_db::persona_db::list_personas(&state.pool)
        .await
        .unwrap_or_default();
    let user_count = personas.len() as i64;

    let mut post_count = 0i64;
    for p in &personas {
        post_count += fieldwork_db::posts_db::posts_count(&state.pool, p.id)
            .await
            .unwrap_or(0);
    }

    let community_count = fieldwork_db::communities_db::list_communities(&state.pool, 1000)
        .await
        .map(|v| v.len() as i64)
        .unwrap_or(0);

    Json(serde_json::json!({
        "site_view": {
            "site": {
                "id": 1,
                "name": format!("Broadside ({})", state.domain),
                "description": "ActivityPub broadcast server",
                "published": now_iso(),
                "updated": null,
                "actor_id": format!("https://{}", state.domain),
                "instance_id": 1,
            },
            "local_site": {
                "id": 1,
                "site_id": 1,
                "enable_downvotes": false,
                "enable_nsfw": false,
                "community_creation_admin_only": true,
                "require_email_verification": false,
                "registration_mode": "Closed",
                "published": now_iso(),
            },
            "local_site_rate_limit": {
                "id": 1,
                "local_site_id": 1,
            },
            "counts": {
                "id": 1,
                "site_id": 1,
                "users": user_count,
                "posts": post_count,
                "comments": 0,
                "communities": community_count,
                "users_active_day": 0,
                "users_active_week": 0,
                "users_active_month": 0,
                "users_active_half_year": 0,
            },
        },
        "admins": [],
        "version": env!("CARGO_PKG_VERSION"),
        "all_languages": [],
        "discussion_languages": [],
        "taglines": [],
    }))
}

// ---------------------------------------------------------------------------
// GET /api/v3/community/list
// ---------------------------------------------------------------------------

async fn list_communities(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListCommunitiesQuery>,
) -> impl IntoResponse {
    let limit = query.limit.clamp(1, 50);

    let communities = fieldwork_db::communities_db::list_communities(&state.pool, limit)
        .await
        .unwrap_or_default();

    let mut views = Vec::with_capacity(communities.len());
    for c in &communities {
        let persona = match fieldwork_db::persona_db::get_persona_by_id(
            &state.pool,
            c.persona_id,
        )
        .await
        {
            Ok(Some(p)) => p,
            _ => continue,
        };
        let members = fieldwork_db::communities_db::member_count(&state.pool, c.persona_id)
            .await
            .unwrap_or(0);
        views.push(community_to_json(c, &persona, members, &state.domain));
    }

    Json(serde_json::json!({ "communities": views }))
}

// ---------------------------------------------------------------------------
// GET /api/v3/community
// ---------------------------------------------------------------------------

async fn get_community(
    State(state): State<Arc<AppState>>,
    Query(query): Query<GetCommunityQuery>,
) -> impl IntoResponse {
    let persona_id = if let Some(id) = query.id {
        id
    } else if let Some(ref name) = query.name {
        match fieldwork_db::persona_db::get_persona_by_username(&state.pool, name).await {
            Ok(Some(p)) => p.id,
            _ => return (StatusCode::NOT_FOUND, "community not found").into_response(),
        }
    } else {
        return (StatusCode::BAD_REQUEST, "id or name required").into_response();
    };

    let community = match fieldwork_db::communities_db::get_community(&state.pool, persona_id).await
    {
        Ok(Some(c)) => c,
        Ok(None) => return (StatusCode::NOT_FOUND, "community not found").into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let persona = match fieldwork_db::persona_db::get_persona_by_id(&state.pool, persona_id).await {
        Ok(Some(p)) => p,
        _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let members = fieldwork_db::communities_db::member_count(&state.pool, persona_id)
        .await
        .unwrap_or(0);

    Json(serde_json::json!({
        "community_view": community_to_json(&community, &persona, members, &state.domain),
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v3/post/list
// ---------------------------------------------------------------------------

async fn list_posts(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListPostsQuery>,
) -> impl IntoResponse {
    let limit = query.limit.clamp(1, 50);
    let page = query.page.unwrap_or(1).max(1);
    let offset = (page - 1) * limit;
    let sort = query.sort.as_deref().unwrap_or("new");

    let community_id = if let Some(id) = query.community_id {
        Some(id)
    } else if let Some(ref name) = query.community_name {
        fieldwork_db::persona_db::get_persona_by_username(&state.pool, name)
            .await
            .ok()
            .flatten()
            .map(|p| p.id)
    } else {
        None
    };

    let community_id = match community_id {
        Some(id) => id,
        None => return Json(serde_json::json!({ "posts": [] })).into_response(),
    };

    let posts = fieldwork_db::communities_db::list_posts(
        &state.pool,
        community_id,
        sort,
        limit,
        offset,
    )
    .await
    .unwrap_or_default();

    let community_persona =
        match fieldwork_db::persona_db::get_persona_by_id(&state.pool, community_id).await {
            Ok(Some(p)) => p,
            _ => return Json(serde_json::json!({ "posts": [] })).into_response(),
        };

    let mut views = Vec::with_capacity(posts.len());
    for p in &posts {
        let author = match fieldwork_db::persona_db::get_persona_by_id(
            &state.pool,
            p.author_persona_id,
        )
        .await
        {
            Ok(Some(a)) => a,
            _ => continue,
        };
        let score = fieldwork_db::communities_db::post_score(&state.pool, p.id)
            .await
            .unwrap_or(0);
        views.push(post_to_json(p, &author, &community_persona, score, &state.domain));
    }

    Json(serde_json::json!({ "posts": views })).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v3/post
// ---------------------------------------------------------------------------

async fn get_post(
    State(state): State<Arc<AppState>>,
    Query(query): Query<GetPostQuery>,
) -> impl IntoResponse {
    let post = match fieldwork_db::communities_db::get_post(&state.pool, query.id).await {
        Ok(Some(p)) => p,
        Ok(None) => return (StatusCode::NOT_FOUND, "post not found").into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let author = match fieldwork_db::persona_db::get_persona_by_id(
        &state.pool,
        post.author_persona_id,
    )
    .await
    {
        Ok(Some(a)) => a,
        _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let community_persona = match fieldwork_db::persona_db::get_persona_by_id(
        &state.pool,
        post.community_id,
    )
    .await
    {
        Ok(Some(p)) => p,
        _ => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let score = fieldwork_db::communities_db::post_score(&state.pool, post.id)
        .await
        .unwrap_or(0);

    let comments = fieldwork_db::communities_db::get_comments(&state.pool, query.id, "new")
        .await
        .unwrap_or_default();

    let mut comment_views = Vec::with_capacity(comments.len());
    for c in &comments {
        let c_author = match fieldwork_db::persona_db::get_persona_by_id(
            &state.pool,
            c.author_persona_id,
        )
        .await
        {
            Ok(Some(a)) => a,
            _ => continue,
        };
        let c_score = fieldwork_db::communities_db::comment_score(&state.pool, c.id)
            .await
            .unwrap_or(0);
        comment_views.push(comment_to_json(c, &c_author, c_score, &state.domain));
    }

    Json(serde_json::json!({
        "post_view": post_to_json(&post, &author, &community_persona, score, &state.domain),
        "comments": comment_views,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v3/comment/list
// ---------------------------------------------------------------------------

async fn list_comments(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListCommentsQuery>,
) -> impl IntoResponse {
    let sort = query.sort.as_deref().unwrap_or("new");

    let comments = if let Some(post_id) = query.post_id {
        fieldwork_db::communities_db::get_comments(&state.pool, post_id, sort)
            .await
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut views = Vec::with_capacity(comments.len());
    for c in &comments {
        let author = match fieldwork_db::persona_db::get_persona_by_id(
            &state.pool,
            c.author_persona_id,
        )
        .await
        {
            Ok(Some(a)) => a,
            _ => continue,
        };
        let score = fieldwork_db::communities_db::comment_score(&state.pool, c.id)
            .await
            .unwrap_or(0);
        views.push(comment_to_json(c, &author, score, &state.domain));
    }

    Json(serde_json::json!({ "comments": views }))
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v3/site", get(get_site))
        .route("/api/v3/community/list", get(list_communities))
        .route("/api/v3/community", get(get_community))
        .route("/api/v3/post/list", get(list_posts))
        .route("/api/v3/post", get(get_post))
        .route("/api/v3/comment/list", get(list_comments))
}
