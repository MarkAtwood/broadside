use anyhow::Context;
use fieldwork_db::db::sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use std::path::Path;
use std::str::FromStr;

/// Broadside-specific extra migrations (run after the canonical fieldwork chain).
const BROADSIDE_EXTRAS: &[fieldwork_db::db::Migration] = &[
    fieldwork_db::db::Migration {
        version: 100,
        description: "broadside: add broadside_post_meta and persona DID columns",
        sql_sqlite: r#"
CREATE TABLE IF NOT EXISTS broadside_post_meta (
    post_id  INTEGER PRIMARY KEY REFERENCES posts(id) ON DELETE CASCADE,
    source_ref TEXT UNIQUE
);
"#,
        sql_postgres: r#"
CREATE TABLE IF NOT EXISTS broadside_post_meta (
    post_id  BIGINT PRIMARY KEY REFERENCES posts(id) ON DELETE CASCADE,
    source_ref TEXT UNIQUE
);
"#,
    },
    fieldwork_db::db::Migration {
        version: 101,
        description: "broadside: add per-persona DID columns",
        sql_sqlite: "__ADD_COLUMN_IF_NOT_EXISTS:personas:did_key:TEXT",
        sql_postgres: "ALTER TABLE personas ADD COLUMN IF NOT EXISTS did_key TEXT;",
    },
    fieldwork_db::db::Migration {
        version: 102,
        description: "broadside: add per-persona recovery_pubkey column",
        sql_sqlite: "__ADD_COLUMN_IF_NOT_EXISTS:personas:recovery_pubkey:TEXT",
        sql_postgres: "ALTER TABLE personas ADD COLUMN IF NOT EXISTS recovery_pubkey TEXT;",
    },
];

pub async fn connect(data_dir: &Path) -> anyhow::Result<fieldwork_db::db::Pool> {
    let db_path = data_dir.join("broadside.db");
    let db_str = db_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("data dir path contains invalid UTF-8"))?;
    let options = SqliteConnectOptions::from_str(db_str)?
        .journal_mode(SqliteJournalMode::Wal)
        .create_if_missing(true)
        .foreign_keys(true);
    let sq_pool = fieldwork_db::db::sqlx::SqlitePool::connect_with(options).await?;
    let pool = fieldwork_db::db::Pool::Sqlite(sq_pool);

    fieldwork_db::db::migrate_full(&pool, Some(&fieldwork_db::db::LEGACY_BROADSIDE), BROADSIDE_EXTRAS)
        .await
        .context("Failed to run database migrations")?;

    Ok(pool)
}

pub async fn init_data_dir(data_dir: &Path) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(data_dir).await?;
    tokio::fs::create_dir_all(data_dir.join("media")).await?;

    let pool = connect(data_dir).await?;

    // Ensure a default operator user exists (for fresh databases)
    let now = chrono::Utc::now().timestamp();
    // Only insert if no users exist yet (idempotent)
    if fieldwork_db::tenant_db::list_users(&pool).await.map(|v| v.is_empty()).unwrap_or(true) {
        let _ = fieldwork_db::tenant_db::create_user(
            &pool,
            1000000000000i64,
            "admin@localhost",
            None,
            "admin",
            now,
        )
        .await;
    }

    match &pool {
        fieldwork_db::db::Pool::Sqlite(sq) => sq.close().await,
    }

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
