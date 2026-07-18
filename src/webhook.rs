use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;
use subtle::ConstantTimeEq;

use crate::sanitize;
use crate::server::AppState;

#[derive(Deserialize)]
pub struct WebhookQuery {
    key: String,
}

#[derive(Deserialize)]
pub struct WebhookPayload {
    content: String,
    #[serde(default = "default_content_type")]
    content_type: String,
    // ponytail: media fetch not yet implemented — tracked in broadside-to5p.5
    #[serde(default)]
    pub media: Vec<WebhookMedia>,
}

#[derive(Deserialize)]
pub struct WebhookMedia {
    pub url: String,
    #[serde(default)]
    pub description: String,
}

fn default_content_type() -> String {
    "text/plain".to_string()
}

pub async fn handle_webhook(
    State(state): State<Arc<AppState>>,
    Path(persona_name): Path<String>,
    Query(query): Query<WebhookQuery>,
    Json(payload): Json<WebhookPayload>,
) -> impl IntoResponse {
    let keys = &state.webhook_keys;
    match keys.get(&persona_name) {
        Some(expected_key) => {
            // Constant-time comparison — hash both keys to fixed length first
            // to avoid leaking key length via early-exit
            use sha2::Digest;
            let expected_hash = sha2::Sha256::digest(expected_key.as_bytes());
            let provided_hash = sha2::Sha256::digest(query.key.as_bytes());
            if expected_hash.ct_eq(&provided_hash).unwrap_u8() != 1 {
                return (StatusCode::UNAUTHORIZED, "invalid key").into_response();
            }
        }
        None => {
            return (
                StatusCode::NOT_FOUND,
                "no webhook configured for this persona",
            )
                .into_response()
        }
    }

    let persona_id = match crate::persona::get_id(&state.pool, &persona_name).await {
        Ok(id) => id,
        Err(_) => return (StatusCode::NOT_FOUND, "unknown persona").into_response(),
    };

    let html = match payload.content_type.as_str() {
        "text/markdown" => sanitize::markdown_to_html(&payload.content),
        "text/html" => sanitize::sanitize_html(&payload.content),
        _ => crate::post::text_to_html(&payload.content),
    };
    let text = sanitize::html_to_text(&html);

    let post_id = match crate::post::create(&state.pool, &persona_id, &html, &text, None).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!(error = %e, "webhook post creation failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "post creation failed").into_response();
        }
    };

    // Fetch and attach media (capped)
    let data_dir = std::path::Path::new(&state.data_dir);
    let max_media = crate::media::max_media_per_post();
    for media_item in payload.media.iter().take(max_media) {
        if let Err(e) = crate::media::process_remote(
            &state.pool,
            &post_id,
            &media_item.url,
            data_dir,
            &media_item.description,
            &state.http_client,
        )
        .await
        {
            tracing::warn!(url = %media_item.url, error = %e, "failed to fetch webhook media, skipping");
        }
    }

    match crate::delivery::fan_out(&state.pool, &post_id, &persona_id).await {
        Ok(queued) => {
            tracing::info!(
                persona = persona_name,
                post_id,
                queued,
                "webhook post created"
            );
            (
                StatusCode::CREATED,
                Json(serde_json::json!({"post_id": post_id, "queued": queued})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "webhook fan-out failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "fan-out failed").into_response()
        }
    }
}
