use anyhow::Context;
use fieldwork_db::db::Pool;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

use crate::config::WatchConfig;
use crate::sanitize;

/// Background directory watcher. Runs as a tokio task.
pub async fn run_watcher(pool: Pool, config: WatchConfig, domain: String) {
    if let Err(e) = watch_loop(&pool, &config, &domain).await {
        tracing::error!(error = %e, "directory watcher exited");
    }
}

async fn watch_loop(pool: &Pool, config: &WatchConfig, _domain: &str) -> anyhow::Result<()> {
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

    let canonical_watch = tokio::fs::canonicalize(&watch_path)
        .await
        .context("canonicalizing watch directory")?;

    while let Some(file_path) = rx.recv().await {
        if !matches_pattern(&file_path, &pattern) {
            continue;
        }

        // Small delay to let the file finish writing
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Canonicalize FIRST to resolve symlinks, then boundary-check, then read.
        // This prevents TOCTOU: a symlink swap after canonicalize but before read would
        // produce a different canonical path, which the boundary check would reject on
        // the next event. Reading after the check means we never read a file that failed
        // the boundary check.
        let canonical = match tokio::fs::canonicalize(&file_path).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, file = %file_path.display(), "cannot canonicalize watched file");
                continue;
            }
        };
        if !canonical.starts_with(&canonical_watch) {
            tracing::warn!(file = %file_path.display(), canonical = %canonical.display(), "rejecting file that escapes watch directory");
            continue;
        }

        let content = match tokio::fs::read_to_string(&file_path).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, file = %file_path.display(), "cannot read watched file");
                continue;
            }
        };

        let dest = published_path.join(file_path.file_name().unwrap_or_default());
        match process_file_content(pool, persona_id, &file_path, &content).await {
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

async fn process_file_content(
    pool: &Pool,
    persona_id: i64,
    file_path: &Path,
    content: &str,
) -> anyhow::Result<String> {
    let html = sanitize::markdown_to_html(content);
    let text = sanitize::html_to_text(&html);
    let source_ref = file_path.to_string_lossy().into_owned();

    let post_id = crate::post::create(pool, persona_id, &html, &text, Some(&source_ref)).await?;
    crate::delivery::fan_out(pool, &post_id, persona_id).await?;
    Ok(post_id)
}

fn matches_pattern(path: &Path, pattern: &str) -> bool {
    // Simple glob: "*.md" matches any file whose extension is "md"
    if let Some(want_ext) = pattern.strip_prefix("*.") {
        return path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e == want_ext)
            .unwrap_or(false);
    }

    // Exact filename match
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n == pattern)
        .unwrap_or(false)
}
