//! Funkwhale-compatible API endpoints for broadside (read-only).

use crate::server::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use fieldwork::funkwhale_api::*;
use std::sync::Arc;

async fn list_tracks(
    State(state): State<Arc<AppState>>,
    Query(params): Query<PaginationParams>,
) -> Json<PaginatedResponse<TrackResponse>> {
    let (limit, offset) = params.to_limit_offset();
    let tracks = fieldwork_db::audio_db::list_public_tracks(&state.pool, limit, offset)
        .await
        .unwrap_or_default();
    let results: Vec<_> = tracks.iter().map(|t| t.into()).collect();
    Json(PaginatedResponse {
        count: results.len(),
        results,
    })
}

async fn get_track_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    match fieldwork_db::audio_db::get_track(&state.pool, id).await {
        Ok(Some(t)) => Json(TrackResponse::from(&t)).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn list_albums(
    State(state): State<Arc<AppState>>,
    Query(params): Query<PaginationParams>,
) -> Json<PaginatedResponse<AlbumResponse>> {
    let (limit, offset) = params.to_limit_offset();
    let albums = fieldwork_db::audio_db::list_albums(&state.pool, limit, offset)
        .await
        .unwrap_or_default();
    let results: Vec<_> = albums
        .iter()
        .map(|(album, artist, count)| AlbumResponse {
            album: album.clone(),
            artist: artist.clone(),
            track_count: *count,
        })
        .collect();
    Json(PaginatedResponse {
        count: results.len(),
        results,
    })
}

async fn list_channels(
    State(state): State<Arc<AppState>>,
    Query(params): Query<PaginationParams>,
) -> Json<PaginatedResponse<ChannelResponse>> {
    let (limit, offset) = params.to_limit_offset();
    let channels = fieldwork_db::audio_db::list_all_audio_channels(&state.pool, limit, offset)
        .await
        .unwrap_or_default();
    let results: Vec<_> = channels.iter().map(|c| c.into()).collect();
    Json(PaginatedResponse {
        count: results.len(),
        results,
    })
}

async fn list_playlists(
    State(state): State<Arc<AppState>>,
    Query(params): Query<PaginationParams>,
) -> Json<PaginatedResponse<PlaylistResponse>> {
    let (limit, offset) = params.to_limit_offset();
    let playlists = fieldwork_db::audio_db::list_audio_playlists(&state.pool, limit, offset)
        .await
        .unwrap_or_default();
    let results: Vec<_> = playlists
        .iter()
        .map(|(id, user_id, title, desc, vis, created_at)| PlaylistResponse {
            id: *id,
            user_id: *user_id,
            title: title.clone(),
            description: desc.clone(),
            visibility: vis.clone(),
            created_at: *created_at,
        })
        .collect();
    Json(PaginatedResponse {
        count: results.len(),
        results,
    })
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(TRACKS_PATH, get(list_tracks))
        .route(TRACK_PATH, get(get_track_handler))
        .route(ALBUMS_PATH, get(list_albums))
        .route(CHANNELS_PATH, get(list_channels))
        .route(PLAYLISTS_PATH, get(list_playlists))
}
