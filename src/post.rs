use anyhow::Context;
use sqlx::SqlitePool;

use crate::id::gen_int_id;

/// Wrap a raw SqlitePool in fieldwork's Pool enum for shared module calls.
fn fw_pool(pool: &SqlitePool) -> fieldwork::db::Pool {
    fieldwork::db::Pool::Sqlite(pool.clone())
}

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

    let fwp = fw_pool(pool);
    let post = fieldwork::posts_db::PostRow {
        id,
        user_id,
        persona_id: persona_id.to_string(),
        ap_id,
        in_reply_to_id: None,
        in_reply_to_uri: None,
        boost_of_id: None,
        boost_of_uri: None,
        content: content_text.to_string(),
        content_html: content_html.to_string(),
        spoiler_text: String::new(),
        visibility: "public".to_string(),
        sensitive: false,
        language: None,
        context_url: None,
        created_at: now,
        edited_at: None,
        deleted_at: None,
        deleted_reason: None,
    };
    fieldwork::posts_db::create_post(&fwp, &post)
        .await
        .with_context(|| format!("inserting post for persona {persona_id}"))?;

    // Remaining SQL: broadside_post_meta is a broadside-specific table with no fieldwork equivalent.
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
/// Remaining SQL: joins broadside_post_meta (broadside-specific table) for source_ref.
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
    let fwp = fw_pool(pool);
    let count = fieldwork::posts_db::posts_count(&fwp, persona_id).await?;
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
