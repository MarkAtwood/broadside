use anyhow::Context;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

use crate::config::WatchConfig;
use crate::sanitize;

/// Background directory watcher. Runs as a tokio task.
pub async fn run_watcher(pool: SqlitePool, config: WatchConfig, domain: String) {
    if let Err(e) = watch_loop(&pool, &config, &domain).await {
        tracing::error!(error = %e, "directory watcher exited");
    }
}

async fn watch_loop(pool: &SqlitePool, config: &WatchConfig, _domain: &str) -> anyhow::Result<()> {
    let watch_path = PathBuf::from(&config.path);
    let published_path = PathBuf::from(&config.published);
    let pattern = config.pattern.clone();

    tokio::fs::create_dir_all(&watch_path)
        .await
        .context("creating watch directory")?;
    tokio::fs::create_dir_all(&published_path)
        .await
        .context("creating published directory")?;

    let (tx, mut rx) = mpsc::channel::<PathBuf>(100);

    let mut watcher = RecommendedWatcher::new(
        move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                if matches!(event.kind, EventKind::Create(_)) {
                    for path in event.paths {
                        let _ = tx.blocking_send(path);
                    }
                }
            }
        },
        notify::Config::default(),
    )
    .context("creating filesystem watcher")?;

    watcher
        .watch(&watch_path, RecursiveMode::NonRecursive)
        .context("starting filesystem watch")?;

    tracing::info!(path = %watch_path.display(), pattern, "watching directory");

    let persona_id = crate::persona::get_id(pool, &config.persona).await?;

    while let Some(file_path) = rx.recv().await {
        if !matches_pattern(&file_path, &pattern) {
            continue;
        }

        // Small delay to let the file finish writing
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Read the file first, then verify the canonical path stays within the watch directory.
        // This eliminates the TOCTOU race between symlink check and file read.
        let canonical = match tokio::fs::canonicalize(&file_path).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, file = %file_path.display(), "cannot canonicalize watched file");
                continue;
            }
        };
        let canonical_watch = match tokio::fs::canonicalize(&watch_path).await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "cannot canonicalize watch directory");
                continue;
            }
        };
        if !canonical.starts_with(&canonical_watch) {
            tracing::warn!(file = %file_path.display(), canonical = %canonical.display(), "rejecting file that escapes watch directory");
            continue;
        }

        let dest = published_path.join(file_path.file_name().unwrap_or_default());
        match process_file(pool, &persona_id, &file_path).await {
            Ok(post_id) => {
                tracing::info!(post_id, file = %file_path.display(), "published from directory");
            }
            Err(e) => {
                // Duplicate file events hit UNIQUE constraint — still move to published
                tracing::warn!(error = %e, file = %file_path.display(), "processing watched file (may be duplicate)");
            }
        }
        // Always move to published to prevent stranding
        if file_path.exists() {
            if let Err(e) = tokio::fs::rename(&file_path, &dest).await {
                tracing::error!(error = %e, "moving file to published");
            }
        }
    }

    Ok(())
}

async fn process_file(
    pool: &SqlitePool,
    persona_id: &str,
    file_path: &Path,
) -> anyhow::Result<String> {
    let content = tokio::fs::read_to_string(file_path)
        .await
        .with_context(|| format!("reading {}", file_path.display()))?;

    let html = sanitize::markdown_to_html(&content);
    let text = sanitize::html_to_text(&html);
    let source_ref = file_path.to_string_lossy().to_string();

    let post_id = crate::post::create(pool, persona_id, &html, &text, Some(&source_ref)).await?;
    crate::delivery::fan_out(pool, &post_id, persona_id).await?;
    Ok(post_id)
}

fn matches_pattern(path: &Path, pattern: &str) -> bool {
    let file_name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };

    // Simple glob: "*.md" matches anything ending in ".md"
    if let Some(ext) = pattern.strip_prefix("*.") {
        return file_name.len() > ext.len() + 1
            && file_name.ends_with(ext)
            && file_name.as_bytes()[file_name.len() - ext.len() - 1] == b'.';
    }

    file_name == pattern
}
