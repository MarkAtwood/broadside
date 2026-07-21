use anyhow::Context;
use fieldwork_db::db::Pool;

use crate::id::gen_int_id;

/// Create a post from plain text or HTML content.
pub async fn create(
    pool: &Pool,
    persona_id: i64,
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


    // FEP post abstract: auto-generate for long posts.
    let abstract_text = if content_text.chars().count() > 500 {
        let first_sentence_end = content_text.find(". ")
            .or_else(|| content_text.find(".\n"))
            .map(|i| i + 1)
            .unwrap_or_else(|| content_text.len().min(200));
        let end = first_sentence_end.min(200);
        let truncated: String = content_text.chars().take(end).collect();
        if truncated.len() < content_text.len() {
            Some(format!("{truncated}..."))
        } else {
            Some(truncated)
        }
    } else {
        None
    };

    let post = fieldwork_db::posts_db::PostRow {
        id,
        user_id,
        persona_id,
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
        abstract_text,
    };
    fieldwork_db::posts_db::create_post(pool, &post)
        .await
        .with_context(|| format!("inserting post for persona {persona_id}"))?;

    if let Some(sref) = source_ref {
        crate::db_extras::insert_post_meta(pool, id, sref).await?;
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
    pool: &Pool,
    persona_id: i64,
    limit: i64,
    offset: i64,
) -> anyhow::Result<Vec<PostRow>> {
    let rows = crate::db_extras::list_posts_with_meta(pool, persona_id, limit, offset).await?;
    Ok(rows
        .into_iter()
        .map(|r| PostRow {
            id: r.id,
            persona_id: r.persona_id,
            content_html: r.content_html,
            content: r.content,
            created_at: r.created_at,
            source_ref: r.source_ref,
        })
        .collect())
}

/// Count total posts for a persona.
pub async fn count_for_persona(pool: &Pool, persona_id: i64) -> anyhow::Result<i64> {

    let count = fieldwork_db::posts_db::posts_count(pool, persona_id).await?;
    Ok(count)
}

#[derive(Debug)]
pub struct PostRow {
    pub id: String,
    pub persona_id: i64,
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
