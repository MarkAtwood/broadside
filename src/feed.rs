use anyhow::Context;
use sqlx::SqlitePool;

use crate::config::FeedConfig;
use crate::sanitize;

const MAX_CONTENT_LEN: usize = 5000;

/// Poll a single feed and create posts for new entries.
pub async fn poll_feed(
    pool: &SqlitePool,
    config: &FeedConfig,
    domain: &str,
    client: &reqwest::Client,
    data_dir: &std::path::Path,
) -> anyhow::Result<u32> {
    let persona_id = crate::persona::get_id(pool, &config.persona).await?;

    let resp = client
        .get(&config.url)
        .send()
        .await
        .with_context(|| format!("fetching feed {}", config.url))?;

    const MAX_FEED_SIZE: usize = 5 * 1024 * 1024; // 5 MB
    let body = crate::http::read_body_limited(resp, MAX_FEED_SIZE)
        .await
        .with_context(|| format!("reading feed {}", config.url))?;

    let feed = feed_rs::parser::parse(&body[..])
        .with_context(|| format!("parsing feed {}", config.url))?;

    let mut new_count = 0u32;
    let mut newest_id: Option<String> = None;

    // Process all entries — dedup via INSERT OR IGNORE + source_ref UNIQUE constraint.
    for entry in &feed.entries {
        let entry_id = entry.id.clone();

        let title = entry.title.as_ref().map(|t| t.content.clone());
        let body_html = entry
            .content
            .as_ref()
            .and_then(|c| c.body.clone())
            .or_else(|| entry.summary.as_ref().map(|s| s.content.clone()))
            .unwrap_or_default();

        // Only allow http/https link URLs — reject javascript: and other schemes
        let link = entry.links.first().and_then(|l| {
            if l.href.starts_with("https://") || l.href.starts_with("http://") {
                Some(l.href.clone())
            } else {
                None
            }
        });

        let mut html = String::new();
        if let Some(ref t) = title {
            html.push_str(&format!("<p><strong>{}</strong></p>", ammonia::clean(t)));
        }
        html.push_str(&sanitize::sanitize_html(&body_html));

        if let Some(ref url) = link {
            // Use ammonia to produce a safe <a> tag (handles entity encoding)
            let escaped_url = crate::sanitize::escape_html_attr(url);
            let link_html = format!(r#"<p><a href="{escaped_url}">{escaped_url}</a></p>"#);
            html.push_str(&sanitize::sanitize_html(&link_html));
        }

        if html.len() > MAX_CONTENT_LEN {
            crate::sanitize::truncate_utf8(&mut html, MAX_CONTENT_LEN);
            // Re-sanitize truncated HTML to close any open tags
            html = sanitize::sanitize_html(&html);
            if let Some(ref url) = link {
                let escaped_url = crate::sanitize::escape_html_attr(url);
                let link_html = format!(r#"<a href="{escaped_url}">read more</a>"#);
                html.push_str(&sanitize::sanitize_html(&link_html));
            }
        }

        let text = sanitize::html_to_text(&html);

        // Use INSERT OR IGNORE for dedup — avoids fragile string matching on error messages
        let id = crate::id::gen_id();
        let result = sqlx::query(
            "INSERT OR IGNORE INTO posts (id, persona_id, content_html, content_text, source_ref) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&persona_id)
        .bind(&html)
        .bind(&text)
        .bind(&entry_id)
        .execute(pool)
        .await?;

        if result.rows_affected() > 0 {
            // Attach enclosure images as media (capped to prevent abuse)
            let max_media = crate::media::MAX_MEDIA;
            let mut media_count = 0usize;
            'media_loop: for media_link in &entry.media {
                for content in &media_link.content {
                    if media_count >= max_media {
                        break 'media_loop;
                    }
                    if let Some(ref url_val) = content.url {
                        let url_str = url_val.as_str();
                        let mime = content
                            .content_type
                            .as_ref()
                            .map(|m| m.essence().to_string())
                            .unwrap_or_default();
                        if mime.starts_with("image/")
                            || url_str.ends_with(".jpg")
                            || url_str.ends_with(".jpeg")
                            || url_str.ends_with(".png")
                            || url_str.ends_with(".gif")
                            || url_str.ends_with(".webp")
                        {
                            // process_remote has its own SSRF guard
                            match crate::media::process_remote(
                                pool, &id, url_str, data_dir, "", client,
                            )
                            .await
                            {
                                Ok(_) => media_count += 1,
                                Err(e) => {
                                    tracing::warn!(url = url_str, error = %e, "failed to fetch feed media")
                                }
                            }
                        }
                    }
                }
            }

            // Spawn background card fetch for link previews
            crate::card::spawn_fetch(
                pool.clone(),
                id.clone(),
                html.clone(),
                data_dir.to_str().unwrap_or(".").to_string(),
                client.clone(),
                domain.to_string(),
            );

            if let Err(e) = crate::delivery::fan_out(pool, &id, &persona_id).await {
                tracing::error!(post_id = %id, error = %e, "fan_out failed for new post");
            }
            new_count += 1;
            newest_id = Some(entry_id);
        }
    }

    // Update feed state
    if let Some(ref nid) = newest_id {
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        sqlx::query(
            "INSERT INTO feed_state (feed_url, persona_id, last_seen_id, last_poll) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(feed_url) DO UPDATE SET last_seen_id = ?, last_poll = ?",
        )
        .bind(&config.url)
        .bind(&persona_id)
        .bind(nid)
        .bind(&now)
        .bind(nid)
        .bind(&now)
        .execute(pool)
        .await?;
    }

    if new_count > 0 {
        tracing::info!(feed = %config.url, new_count, "polled feed");
    }
    Ok(new_count)
}

/// Background feed poller. Runs as a tokio task for each configured feed.
pub async fn run_poller(
    pool: SqlitePool,
    config: FeedConfig,
    domain: String,
    data_dir: std::path::PathBuf,
) {
    let interval = config.interval();
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "failed to build HTTP client, feed poller exiting");
            return;
        }
    };

    // ponytail: poll-then-sleep means first poll is immediate on startup — intentional so new
    // posts appear without waiting a full interval after deploy/restart.
    loop {
        match poll_feed(&pool, &config, &domain, &client, &data_dir).await {
            Ok(n) if n > 0 => tracing::info!(feed = %config.url, new = n, "feed poll complete"),
            Ok(_) => {}
            Err(e) => tracing::error!(feed = %config.url, error = %e, "feed poll failed"),
        }
        tokio::time::sleep(interval).await;
    }
}

/// One-shot poll of all configured feeds.
pub async fn poll_all(
    pool: &SqlitePool,
    feeds: &[FeedConfig],
    domain: &str,
    data_dir: &std::path::Path,
) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    for feed in feeds {
        match poll_feed(pool, feed, domain, &client, data_dir).await {
            Ok(n) => println!("{}: {} new posts", feed.url, n),
            Err(e) => eprintln!("{}: error: {e}", feed.url),
        }
    }
    Ok(())
}
