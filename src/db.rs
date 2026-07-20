use anyhow::Context;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool};
use std::path::Path;
use std::str::FromStr;

/// Broadside-specific extra migrations (run after the canonical fieldwork chain).
const BROADSIDE_EXTRAS: &[fieldwork::db::Migration] = &[
    fieldwork::db::Migration {
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
    fieldwork::db::Migration {
        version: 101,
        description: "broadside: add per-persona DID columns",
        sql_sqlite: "__ADD_COLUMN_IF_NOT_EXISTS:personas:did_key:TEXT",
        sql_postgres: "ALTER TABLE personas ADD COLUMN IF NOT EXISTS did_key TEXT;",
    },
    fieldwork::db::Migration {
        version: 102,
        description: "broadside: add per-persona recovery_pubkey column",
        sql_sqlite: "__ADD_COLUMN_IF_NOT_EXISTS:personas:recovery_pubkey:TEXT",
        sql_postgres: "ALTER TABLE personas ADD COLUMN IF NOT EXISTS recovery_pubkey TEXT;",
    },
];

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

    let fw_pool = fieldwork::db::Pool::Sqlite(pool.clone());
    fieldwork::db::migrate_full(&fw_pool, Some(&fieldwork::db::LEGACY_BROADSIDE), BROADSIDE_EXTRAS)
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
    let fw_pool = fieldwork::db::Pool::Sqlite(pool.clone());
    // Only insert if no users exist yet (idempotent)
    if fieldwork::tenant_db::list_users(&fw_pool).await.map(|v| v.is_empty()).unwrap_or(true) {
        let _ = fieldwork::tenant_db::create_user(
            &fw_pool,
            1000000000000i64,
            "admin@localhost",
            None,
            "admin",
            now,
        )
        .await;
    }

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
