//! WriteFreely-compatible API endpoints for broadside (read-only).

use crate::server::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use fieldwork::writefreely_api::*;
use std::sync::Arc;

fn epoch_to_iso(epoch: i64) -> String {
    chrono::DateTime::from_timestamp(epoch, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

fn article_to_post_response(a: &fieldwork_db::articles_db::ArticleRow) -> PostResponse {
    let collection = a.collection_alias.as_ref().map(|alias| CollectionRef {
        alias: alias.clone(),
        title: String::new(),
    });
    PostResponse {
        id: a.id.to_string(),
        slug: a.slug.clone(),
        token: None,
        title: a.title.clone(),
        body: a.body.clone(),
        font: Some(a.font.clone()),
        lang: a.language.clone(),
        rtl: if a.rtl { Some(true) } else { None },
        created: epoch_to_iso(a.created_at),
        updated: a.updated_at.map(epoch_to_iso),
        views: a.views,
        collection,
    }
}

fn collection_to_response(c: &fieldwork_db::articles_db::CollectionRow) -> CollectionResponse {
    CollectionResponse {
        alias: c.alias.clone(),
        title: c.title.clone(),
        description: c.description.clone(),
        style_sheet: if c.style_sheet.is_empty() { None } else { Some(c.style_sheet.clone()) },
        public: c.visibility == "public",
    }
}

fn render_markdown(md: &str) -> String {
    let cleaned = ammonia::clean(md);
    format!("<p>{}</p>", cleaned.replace("\n\n", "</p><p>").replace('\n', "<br>"))
}

async fn get_post(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    match fieldwork_db::articles_db::get_article(&state.pool, id).await {
        Ok(Some(a)) => {
            fieldwork_db::articles_db::increment_views(&state.pool, id).await.ok();
            Json(WfResponse { code: 200, data: article_to_post_response(&a) }).into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn get_collection(
    State(state): State<Arc<AppState>>,
    Path(alias): Path<String>,
) -> impl IntoResponse {
    match fieldwork_db::articles_db::get_collection(&state.pool, &alias).await {
        Ok(Some(c)) => Json(WfResponse { code: 200, data: collection_to_response(&c) }).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn collection_posts(
    State(state): State<Arc<AppState>>,
    Path(alias): Path<String>,
    Query(query): Query<CollectionPostsQuery>,
) -> Json<WfResponse<Vec<PostResponse>>> {
    let page = query.page.max(1);
    let offset = ((page - 1) as i64) * 20;
    let articles = fieldwork_db::articles_db::list_collection_articles(&state.pool, &alias, 20, offset)
        .await.unwrap_or_default();
    let posts: Vec<_> = articles.iter().map(article_to_post_response).collect();
    Json(WfResponse { code: 200, data: posts })
}

async fn get_collection_post(
    State(state): State<Arc<AppState>>,
    Path((alias, slug)): Path<(String, String)>,
) -> impl IntoResponse {
    match fieldwork_db::articles_db::get_article_by_slug(&state.pool, &alias, &slug).await {
        Ok(Some(a)) => {
            fieldwork_db::articles_db::increment_views(&state.pool, a.id).await.ok();
            Json(WfResponse { code: 200, data: article_to_post_response(&a) }).into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn render_md(Json(body): Json<MarkdownRequest>) -> Json<WfResponse<MarkdownResponse>> {
    let html = render_markdown(&body.raw_body);
    Json(WfResponse { code: 200, data: MarkdownResponse { body: html } })
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(POST_PATH, get(get_post))
        .route(COLLECTION_PATH, get(get_collection))
        .route(COLLECTION_POSTS_PATH, get(collection_posts))
        .route(COLLECTION_POST_PATH, get(get_collection_post))
        .route(MARKDOWN_PATH, post(render_md))
}
