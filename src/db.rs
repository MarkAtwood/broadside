use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool};
use std::path::Path;
use std::str::FromStr;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS personas (
    id          TEXT PRIMARY KEY,
    username    TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL DEFAULT '',
    bio         TEXT NOT NULL DEFAULT '',
    avatar_path TEXT,
    header_path TEXT,
    metadata    TEXT NOT NULL DEFAULT '[]',
    private_key TEXT NOT NULL,
    public_key  TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS followers (
    id              TEXT PRIMARY KEY,
    persona_id      TEXT NOT NULL REFERENCES personas(id),
    actor_uri       TEXT NOT NULL,
    inbox_uri       TEXT NOT NULL,
    shared_inbox_uri TEXT,
    followed_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE(persona_id, actor_uri)
);

CREATE TABLE IF NOT EXISTS posts (
    id            TEXT PRIMARY KEY,
    persona_id    TEXT NOT NULL REFERENCES personas(id),
    content_html  TEXT NOT NULL,
    content_text  TEXT NOT NULL,
    published_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    source_ref    TEXT,
    UNIQUE(persona_id, source_ref)
);

CREATE TABLE IF NOT EXISTS post_media (
    id          TEXT PRIMARY KEY,
    post_id     TEXT NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    file_path   TEXT NOT NULL,
    mime_type   TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    blurhash    TEXT NOT NULL DEFAULT '',
    width       INTEGER,
    height      INTEGER
);

-- ponytail: status is stringly-typed ('pending'/'failed'/'delivered'). An enum would be
-- safer but requires a custom sqlx Type impl and migration. Ceiling: add a CHECK constraint
-- or migrate to an integer status code if the set of states grows beyond 3.
CREATE TABLE IF NOT EXISTS delivery_queue (
    id          TEXT PRIMARY KEY,
    post_id     TEXT NOT NULL REFERENCES posts(id),
    inbox_uri   TEXT NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    next_retry  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    status      TEXT NOT NULL DEFAULT 'pending',
    last_error  TEXT
);

CREATE INDEX IF NOT EXISTS idx_delivery_pending ON delivery_queue(status, next_retry);

CREATE TABLE IF NOT EXISTS relays (
    id          TEXT PRIMARY KEY,
    actor_uri   TEXT NOT NULL UNIQUE,
    inbox_uri   TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE TABLE IF NOT EXISTS feed_state (
    feed_url     TEXT PRIMARY KEY,
    persona_id   TEXT NOT NULL REFERENCES personas(id),
    last_seen_id TEXT,
    last_poll    TEXT
);

CREATE TABLE IF NOT EXISTS cards (
    id          TEXT PRIMARY KEY,
    post_id     TEXT NOT NULL UNIQUE REFERENCES posts(id) ON DELETE CASCADE,
    url         TEXT NOT NULL,
    title       TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    image_url   TEXT NOT NULL DEFAULT '',
    image_path  TEXT NOT NULL DEFAULT '',
    site_name   TEXT NOT NULL DEFAULT '',
    card_type   TEXT NOT NULL DEFAULT 'link',
    fetched_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
"#;

/// Current schema version. Bump this when adding migrations.
const CURRENT_SCHEMA_VERSION: i64 = 3;

/// Migrations to apply for each version bump. Index 0 = migration from version 0 to 1, etc.
/// Version 0 is the initial schema (SCHEMA constant above).
/// Add ALTER TABLE statements here for future versions.
const MIGRATIONS: &[&str] = &[
    // Version 0 -> 1: initial schema, no migration needed (handled by CREATE TABLE IF NOT EXISTS)
    "",
    // Version 1 -> 2: link preview cards table
    "CREATE TABLE IF NOT EXISTS cards (
        id          TEXT PRIMARY KEY,
        post_id     TEXT NOT NULL UNIQUE REFERENCES posts(id) ON DELETE CASCADE,
        url         TEXT NOT NULL,
        title       TEXT NOT NULL DEFAULT '',
        description TEXT NOT NULL DEFAULT '',
        image_url   TEXT NOT NULL DEFAULT '',
        image_path  TEXT NOT NULL DEFAULT '',
        site_name   TEXT NOT NULL DEFAULT '',
        card_type   TEXT NOT NULL DEFAULT 'link',
        fetched_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
    );",
    // Version 2 -> 3: store persona used when subscribing to a relay
    "ALTER TABLE relays ADD COLUMN persona TEXT NOT NULL DEFAULT '';",
];

async fn ensure_migrations(pool: &SqlitePool) -> anyhow::Result<()> {
    // Create metadata table for tracking schema version
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
    )
    .execute(pool)
    .await?;

    // Get current version
    let row: Option<(String,)> =
        sqlx::query_as("SELECT value FROM schema_meta WHERE key = 'schema_version'")
            .fetch_optional(pool)
            .await?;

    let version: i64 = match row {
        Some((v,)) => v
            .parse()
            .map_err(|e| anyhow::anyhow!("corrupt schema_version '{}': {}", v, e))?,
        None => {
            // First run — base schema already applied, set version
            sqlx::query("INSERT INTO schema_meta (key, value) VALUES ('schema_version', ?)")
                .bind(CURRENT_SCHEMA_VERSION.to_string())
                .execute(pool)
                .await?;
            return Ok(());
        }
    };

    if version >= CURRENT_SCHEMA_VERSION {
        return Ok(());
    }

    // Apply migrations sequentially
    for v in version..CURRENT_SCHEMA_VERSION {
        let idx = v as usize;
        if idx < MIGRATIONS.len() && !MIGRATIONS[idx].is_empty() {
            sqlx::raw_sql(MIGRATIONS[idx]).execute(pool).await?;
        }
    }

    // Update stored version
    sqlx::query("UPDATE schema_meta SET value = ? WHERE key = 'schema_version'")
        .bind(CURRENT_SCHEMA_VERSION.to_string())
        .execute(pool)
        .await?;

    Ok(())
}

pub async fn connect(data_dir: &Path) -> anyhow::Result<SqlitePool> {
    let db_path = data_dir.join("broadside.db");
    let db_str = db_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("data dir path contains invalid UTF-8"))?;
    let options = SqliteConnectOptions::from_str(db_str)?
        .journal_mode(SqliteJournalMode::Wal)
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePool::connect_with(options).await?;
    // Ensure schema exists (CREATE TABLE IF NOT EXISTS is idempotent)
    sqlx::raw_sql(SCHEMA).execute(&pool).await?;
    // Apply any pending migrations
    ensure_migrations(&pool).await?;
    Ok(pool)
}

pub async fn init_data_dir(data_dir: &Path) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(data_dir).await?;
    tokio::fs::create_dir_all(data_dir.join("media")).await?;

    let pool = connect(data_dir).await?;
    pool.close().await;

    let config_path = data_dir.join("config.toml");
    if !config_path.exists() {
        tokio::fs::write(
            &config_path,
            r#"[server]
bind = "127.0.0.1:3000"
domain = "example.com"
data_dir = "."
"#,
        )
        .await?;
    }

    Ok(())
}
