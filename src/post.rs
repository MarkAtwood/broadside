use anyhow::Context;
use sqlx::SqlitePool;

use crate::id::gen_int_id;

/// Create a post from plain text or HTML content.
pub async fn create(
    pool: &SqlitePool,
    persona_id: &str,
    content_html: &str,
    content_text: &str,
    source_ref: Option<&str>,
) -> anyhow::Result<String> {
    let id = gen_int_id();
    let now = chrono::Utc::now().timestamp();
    let user_id = crate::persona::get_operator_user_id(pool).await?;
    // ponytail: ap_id is constructed at insert time from the id. The real AP URI
    // uses the domain, but we don't have it here. Use a placeholder that the
    // delivery layer overwrites with the full URI when building the activity.
    let ap_id = format!("urn:broadside:post:{id}");
    let id_str = id.to_string();

    sqlx::query(
        "INSERT INTO posts (id, user_id, persona_id, ap_id, content, content_html, visibility, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, 'public', ?)",
    )
    .bind(id)
    .bind(&user_id)
    .bind(persona_id)
    .bind(&ap_id)
    .bind(content_text)
    .bind(content_html)
    .bind(now)
    .execute(pool)
    .await
    .with_context(|| format!("inserting post for persona {persona_id}"))?;

    // Store source_ref in broadside_post_meta if provided.
    // Uses plain INSERT (not OR IGNORE) so duplicate source_ref triggers a UNIQUE error.
    if let Some(sref) = source_ref {
        sqlx::query(
            "INSERT INTO broadside_post_meta (post_id, source_ref) VALUES (?, ?)",
        )
        .bind(id)
        .bind(sref)
        .execute(pool)
        .await
        .with_context(|| format!("inserting source_ref for post {id}"))?;
    }

    tracing::info!(post_id = %id, persona_id, "created post");
    Ok(id_str)
}

/// Wrap plain text in a paragraph tag.
pub fn text_to_html(text: &str) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<p>{}</p>",
        escaped.replace("\n\n", "</p><p>").replace('\n', "<br>")
    )
}

/// Fetch recent posts for a persona, newest first.
pub async fn list_for_persona(
    pool: &SqlitePool,
    persona_id: &str,
    limit: i64,
    offset: i64,
) -> anyhow::Result<Vec<PostRow>> {
    let rows = sqlx::query_as::<_, PostRow>(
        "SELECT CAST(p.id AS TEXT) AS id, p.persona_id, p.content_html, p.content, p.created_at, m.source_ref \
         FROM posts p \
         LEFT JOIN broadside_post_meta m ON m.post_id = p.id \
         WHERE p.persona_id = ? ORDER BY p.created_at DESC LIMIT ? OFFSET ?",
    )
    .bind(persona_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
    .context("listing posts")?;
    Ok(rows)
}

/// Count total posts for a persona.
pub async fn count_for_persona(pool: &SqlitePool, persona_id: &str) -> anyhow::Result<i64> {
    let (count,) = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM posts WHERE persona_id = ?")
        .bind(persona_id)
        .fetch_one(pool)
        .await?;
    Ok(count)
}

#[derive(Debug, sqlx::FromRow)]
pub struct PostRow {
    pub id: String,
    pub persona_id: String,
    pub content_html: String,
    pub content: String,
    pub created_at: i64,
    pub source_ref: Option<String>,
}

impl PostRow {
    /// Format created_at epoch seconds as ISO 8601 for ActivityPub output.
    pub fn published_at_iso(&self) -> String {
        chrono::DateTime::from_timestamp(self.created_at, 0)
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_else(|| format!("{}", self.created_at))
    }
}
