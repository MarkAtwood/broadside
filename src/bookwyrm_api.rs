//! Bookwyrm-compatible API endpoints for broadside (read-only).

use crate::server::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use fieldwork::bookwyrm_api::*;
use std::sync::Arc;

fn book_row_to_response(b: &fieldwork_db::books_db::BookRow) -> BookResponse {
    BookResponse {
        id: b.id,
        title: b.title.clone(),
        author: b.author.clone(),
        isbn: b.isbn.clone(),
        isbn13: b.isbn13.clone(),
        openlibrary_id: b.openlibrary_id.clone(),
        cover_url: b.cover_url.clone(),
        description: b.description.clone(),
        pages: b.pages,
        published_year: b.published_year,
        language: b.language.clone(),
        created_at: b.created_at,
    }
}

fn review_row_to_response(r: &fieldwork_db::books_db::ReviewRow) -> ReviewResponse {
    ReviewResponse {
        id: r.id,
        user_id: r.user_id,
        persona_id: r.persona_id,
        book_id: r.book_id,
        content: r.content.clone(),
        content_html: r.content_html.clone(),
        rating: r.rating,
        spoiler: r.spoiler,
        ap_id: r.ap_id.clone(),
        created_at: r.created_at,
    }
}

async fn search_books(
    State(state): State<Arc<AppState>>,
    Query(params): Query<BookSearchParams>,
) -> Json<Vec<BookResponse>> {
    let limit = params.limit.clamp(1, 100);
    let books = fieldwork_db::books_db::search_books(&state.pool, &params.q, limit)
        .await
        .unwrap_or_default();
    Json(books.iter().map(book_row_to_response).collect())
}

async fn get_book(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    match fieldwork_db::books_db::get_book(&state.pool, id).await {
        Ok(Some(b)) => Json(book_row_to_response(&b)).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn book_reviews(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> Json<Vec<ReviewResponse>> {
    let reviews = fieldwork_db::books_db::reviews_for_book(&state.pool, id, 50)
        .await
        .unwrap_or_default();
    Json(reviews.iter().map(review_row_to_response).collect())
}

async fn user_reading(
    State(state): State<Arc<AppState>>,
    Path(user_id): Path<i64>,
) -> Json<UserReadingResponse> {
    let (to_read, reading, read) = fieldwork_db::books_db::reading_stats(&state.pool, user_id)
        .await
        .unwrap_or((0, 0, 0));
    let reviews = fieldwork_db::books_db::reviews_by_user(&state.pool, user_id, 10)
        .await
        .unwrap_or_default();
    Json(UserReadingResponse {
        shelves: ShelfSummaryResponse {
            to_read,
            reading,
            read,
        },
        recent_reviews: reviews.iter().map(review_row_to_response).collect(),
    })
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(BOOKS_PATH, get(search_books))
        .route(BOOK_PATH, get(get_book))
        .route(BOOK_REVIEWS_PATH, get(book_reviews))
        .route(USER_READING_PATH, get(user_reading))
}
