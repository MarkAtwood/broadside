use anyhow::Context;
use sqlx::SqlitePool;

use crate::id::gen_id;

/// Create a post from plain text or HTML content.
pub async fn create(
    pool: &SqlitePool,
    persona_id: &str,
    content_html: &str,
    content_text: &str,
    source_ref: Option<&str>,
) -> anyhow::Result<String> {
    let id = gen_id();

    sqlx::query(
        "INSERT INTO posts (id, persona_id, content_html, content_text, source_ref) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(persona_id)
    .bind(content_html)
    .bind(content_text)
    .bind(source_ref)
    .execute(pool)
    .await
    .with_context(|| format!("inserting post for persona {persona_id}"))?;

    tracing::info!(post_id = %id, persona_id, "created post");
    Ok(id)
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
        "SELECT id, persona_id, content_html, content_text, published_at, source_ref \
         FROM posts WHERE persona_id = ? ORDER BY published_at DESC LIMIT ? OFFSET ?",
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

#[derive(sqlx::FromRow)]
pub struct PostRow {
    pub id: String,
    pub persona_id: String,
    pub content_html: String,
    pub content_text: String,
    pub published_at: String,
    pub source_ref: Option<String>,
}
