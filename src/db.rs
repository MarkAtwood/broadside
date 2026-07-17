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

CREATE TABLE IF NOT EXISTS feed_state (
    feed_url     TEXT PRIMARY KEY,
    persona_id   TEXT NOT NULL REFERENCES personas(id),
    last_seen_id TEXT,
    last_poll    TEXT
);
"#;

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
    Ok(pool)
}

pub async fn init_data_dir(data_dir: &str) -> anyhow::Result<()> {
    let path = Path::new(data_dir);
    tokio::fs::create_dir_all(path).await?;
    tokio::fs::create_dir_all(path.join("media")).await?;

    let pool = connect(path).await?;
    sqlx::raw_sql(SCHEMA).execute(&pool).await?;
    pool.close().await;

    let config_path = path.join("config.toml");
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
