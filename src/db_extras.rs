//! Broadside-specific database wrappers.
//!
//! Centralizes all raw sqlx::query calls for tables and columns that are
//! broadside-specific (not in fieldwork). Business logic calls these wrappers
//! instead of using sqlx::query directly.

use anyhow::Context;
use fieldwork_db::db::sqlx;
use fieldwork_db::db::Pool;

// --- broadside_post_meta ---

/// Insert a source_ref for a post (used for feed dedup).
pub async fn insert_post_meta(
    pool: &Pool,
    post_id: i64,
    source_ref: &str,
) -> anyhow::Result<()> {
    match pool {
        Pool::Sqlite(sq) => {
            sqlx::query("INSERT INTO broadside_post_meta (post_id, source_ref) VALUES (?, ?)")
                .bind(post_id)
                .bind(source_ref)
                .execute(sq)
                .await
                .with_context(|| format!("inserting source_ref for post {post_id}"))?;
        }
    }
    Ok(())
}

/// Insert a source_ref for a post, ignoring duplicates (used for feed dedup).
pub async fn insert_post_meta_ignore(pool: &Pool, post_id: i64, source_ref: &str) {
    match pool {
        Pool::Sqlite(sq) => {
            let _ = sqlx::query(
                "INSERT OR IGNORE INTO broadside_post_meta (post_id, source_ref) VALUES (?, ?)",
            )
            .bind(post_id)
            .bind(source_ref)
            .execute(sq)
            .await;
        }
    }
}

/// Check if a source_ref already exists in broadside_post_meta.
pub async fn source_ref_exists(pool: &Pool, source_ref: &str) -> anyhow::Result<bool> {
    match pool {
        Pool::Sqlite(sq) => {
            let (count,) = sqlx::query_as::<_, (i64,)>(
                "SELECT COUNT(*) FROM broadside_post_meta WHERE source_ref = ?",
            )
            .bind(source_ref)
            .fetch_one(sq)
            .await?;
            Ok(count > 0)
        }
    }
}

/// Row type for posts joined with broadside_post_meta.
#[derive(Debug)]
pub struct PostWithMeta {
    pub id: String,
    pub persona_id: i64,
    pub content_html: String,
    pub content: String,
    pub created_at: i64,
    pub source_ref: Option<String>,
}

impl<'r> sqlx::FromRow<'r, sqlx::sqlite::SqliteRow> for PostWithMeta {
    fn from_row(row: &'r sqlx::sqlite::SqliteRow) -> sqlx::Result<Self> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            persona_id: row.try_get("persona_id")?,
            content_html: row.try_get("content_html")?,
            content: row.try_get("content")?,
            created_at: row.try_get("created_at")?,
            source_ref: row.try_get("source_ref")?,
        })
    }
}

/// List posts for a persona, joined with broadside_post_meta for source_ref.
pub async fn list_posts_with_meta(
    pool: &Pool,
    persona_id: i64,
    limit: i64,
    offset: i64,
) -> anyhow::Result<Vec<PostWithMeta>> {
    match pool {
        Pool::Sqlite(sq) => {
            let rows = sqlx::query_as::<_, PostWithMeta>(
                "SELECT CAST(p.id AS TEXT) AS id, p.persona_id, p.content_html, p.content, p.created_at, m.source_ref \
                 FROM posts p \
                 LEFT JOIN broadside_post_meta m ON m.post_id = p.id \
                 WHERE p.persona_id = ? ORDER BY p.created_at DESC LIMIT ? OFFSET ?",
            )
            .bind(persona_id)
            .bind(limit)
            .bind(offset)
            .fetch_all(sq)
            .await
            .context("listing posts with meta")?;
            Ok(rows)
        }
    }
}

// --- feed_state ---

/// Upsert feed polling state.
pub async fn upsert_feed_state(
    pool: &Pool,
    feed_url: &str,
    persona_id: i64,
    last_seen_id: &str,
    last_poll: i64,
) -> anyhow::Result<()> {
    match pool {
        Pool::Sqlite(sq) => {
            sqlx::query(
                "INSERT INTO feed_state (feed_url, persona_id, last_seen_id, last_poll) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT(feed_url) DO UPDATE SET last_seen_id = ?, last_poll = ?",
            )
            .bind(feed_url)
            .bind(persona_id)
            .bind(last_seen_id)
            .bind(last_poll)
            .bind(last_seen_id)
            .bind(last_poll)
            .execute(sq)
            .await?;
        }
    }
    Ok(())
}

// --- persona DID columns (broadside migrations 101-102) ---

/// Set did_key and recovery_pubkey for a persona.
pub async fn set_persona_did(
    pool: &Pool,
    persona_id: i64,
    did_key: &str,
    recovery_pubkey_hex: &str,
) -> anyhow::Result<()> {
    match pool {
        Pool::Sqlite(sq) => {
            sqlx::query("UPDATE personas SET did_key = ?, recovery_pubkey = ? WHERE id = ?")
                .bind(did_key)
                .bind(recovery_pubkey_hex)
                .bind(persona_id)
                .execute(sq)
                .await
                .with_context(|| format!("setting DID for persona {persona_id}"))?;
        }
    }
    Ok(())
}

/// Get did_key for a persona by username. Returns None if persona not found or did_key is NULL.
pub async fn get_did_key_by_username(
    pool: &Pool,
    username: &str,
) -> anyhow::Result<Option<String>> {
    match pool {
        Pool::Sqlite(sq) => {
            let row = sqlx::query_as::<_, (Option<String>,)>(
                "SELECT did_key FROM personas WHERE username = ?",
            )
            .bind(username)
            .fetch_optional(sq)
            .await?;
            Ok(row.and_then(|r| r.0))
        }
    }
}

/// Get did_key and recovery_pubkey for a persona by username.
pub async fn get_did_and_recovery_by_username(
    pool: &Pool,
    username: &str,
) -> anyhow::Result<(Option<String>, Option<String>)> {
    match pool {
        Pool::Sqlite(sq) => {
            let row = sqlx::query_as::<_, (Option<String>, Option<String>)>(
                "SELECT did_key, recovery_pubkey FROM personas WHERE username = ?",
            )
            .bind(username)
            .fetch_optional(sq)
            .await?;
            Ok(row.unwrap_or((None, None)))
        }
    }
}

/// Find a persona by did_key. Returns (id, username) if found.
pub async fn find_persona_by_did(
    pool: &Pool,
    did_key: &str,
) -> anyhow::Result<Option<(i64, String)>> {
    match pool {
        Pool::Sqlite(sq) => {
            let row = sqlx::query_as::<_, (i64, String)>(
                "SELECT id, username FROM personas WHERE did_key = ?",
            )
            .bind(did_key)
            .fetch_optional(sq)
            .await
            .context("looking up persona by DID")?;
            Ok(row)
        }
    }
}

/// List personas that lack a did_key. Returns (id, username) pairs.
pub async fn list_personas_without_did(
    pool: &Pool,
) -> anyhow::Result<Vec<(i64, String)>> {
    match pool {
        Pool::Sqlite(sq) => {
            let rows = sqlx::query_as::<_, (i64, String)>(
                "SELECT id, username FROM personas WHERE did_key IS NULL",
            )
            .fetch_all(sq)
            .await
            .context("querying personas without DID")?;
            Ok(rows)
        }
    }
}

/// Update fields_json for a persona by username.
pub async fn update_fields_json(
    pool: &Pool,
    username: &str,
    fields_json: &str,
) -> anyhow::Result<()> {
    match pool {
        Pool::Sqlite(sq) => {
            sqlx::query("UPDATE personas SET fields_json = ? WHERE username = ?")
                .bind(fields_json)
                .bind(username)
                .execute(sq)
                .await?;
        }
    }
    Ok(())
}

/// Update avatar_media_id and/or header_media_id for a persona.
pub async fn update_persona_media(
    pool: &Pool,
    persona_id: i64,
    avatar: Option<&str>,
    header: Option<&str>,
) -> anyhow::Result<()> {
    let mut set_parts: Vec<String> = Vec::new();
    let mut values: Vec<&str> = Vec::new();
    if let Some(v) = avatar {
        set_parts.push("avatar_media_id = ?".to_string());
        values.push(v);
    }
    if let Some(v) = header {
        set_parts.push("header_media_id = ?".to_string());
        values.push(v);
    }
    if set_parts.is_empty() {
        return Ok(());
    }
    let sql = format!(
        "UPDATE personas SET {} WHERE id = ?",
        set_parts.join(", ")
    );
    match pool {
        Pool::Sqlite(sq) => {
            let mut q = sqlx::query(&sql);
            for v in &values {
                q = q.bind(*v);
            }
            q = q.bind(persona_id);
            q.execute(sq).await?;
        }
    }
    Ok(())
}


/// List dead-lettered deliveries (up to 50).
pub async fn delivery_list_dead(
    pool: &Pool,
) -> anyhow::Result<Vec<(String, String, Option<String>)>> {
    match pool {
        Pool::Sqlite(sq) => {
            let rows = sqlx::query_as::<_, (String, String, Option<String>)>(
                "SELECT CAST(id AS TEXT), target_inbox, last_error \
                 FROM delivery_queue WHERE dead_at IS NOT NULL ORDER BY id LIMIT 50",
            )
            .fetch_all(sq)
            .await?;
            Ok(rows)
        }
    }
}


// --- 410 Gone follower removal ---

/// Remove followers whose remote_account has the given inbox URL, scoped to a persona.
/// Used when a 410 Gone response is received during delivery.
pub async fn remove_followers_by_inbox(
    pool: &Pool,
    inbox_url: &str,
    persona_id: i64,
) -> anyhow::Result<u64> {
    match pool {
        Pool::Sqlite(sq) => {
            let result = sqlx::query(
                "DELETE FROM followers WHERE remote_account_id IN \
                 (SELECT id FROM remote_accounts WHERE inbox_url = ? OR shared_inbox_url = ?) \
                 AND persona_id = ?",
            )
            .bind(inbox_url)
            .bind(inbox_url)
            .bind(persona_id)
            .execute(sq)
            .await?;
            Ok(result.rows_affected())
        }
    }
}

// --- follower list ---

/// List followers for a persona with actor_uri and accepted_at date.
pub async fn list_followers_with_dates(
    pool: &Pool,
    persona_id: i64,
) -> anyhow::Result<Vec<(String, i64)>> {
    match pool {
        Pool::Sqlite(sq) => {
            let rows = sqlx::query_as::<_, (String, i64)>(
                "SELECT ra.actor_uri, f.accepted_at \
                 FROM followers f \
                 JOIN remote_accounts ra ON ra.id = f.remote_account_id \
                 WHERE f.persona_id = ? ORDER BY f.accepted_at",
            )
            .bind(persona_id)
            .fetch_all(sq)
            .await?;
            Ok(rows)
        }
    }
}

// --- relay ---

/// Update the follow_id for a relay.
pub async fn relay_update_follow_id(
    pool: &Pool,
    relay_id: i64,
    follow_id: &str,
) -> anyhow::Result<()> {
    match pool {
        Pool::Sqlite(sq) => {
            sqlx::query("UPDATE relays SET follow_id = ? WHERE id = ?")
                .bind(follow_id)
                .bind(relay_id)
                .execute(sq)
                .await
                .context("updating relay follow_id")?;
        }
    }
    Ok(())
}

/// List all relay subscriptions (any state).
pub async fn relay_list_all(
    pool: &Pool,
) -> anyhow::Result<Vec<(String, String, String, i64)>> {
    match pool {
        Pool::Sqlite(sq) => {
            let rows = sqlx::query_as::<_, (String, String, String, i64)>(
                "SELECT actor_uri, inbox_url, state, created_at FROM relays ORDER BY created_at",
            )
            .fetch_all(sq)
            .await?;
            Ok(rows)
        }
    }
}
