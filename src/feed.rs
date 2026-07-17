use anyhow::Context;
use sqlx::SqlitePool;

use crate::config::FeedConfig;
use crate::sanitize;

const MAX_CONTENT_LEN: usize = 5000;

/// Truncate a string at a UTF-8 safe boundary.
fn truncate_utf8(s: &mut String, max_len: usize) {
    if s.len() <= max_len {
        return;
    }
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// Poll a single feed and create posts for new entries.
pub async fn poll_feed(
    pool: &SqlitePool,
    config: &FeedConfig,
    _domain: &str,
    client: &reqwest::Client,
) -> anyhow::Result<u32> {
    let persona_id = crate::persona::get_id(pool, &config.persona).await?;

    let body = client
        .get(&config.url)
        .send()
        .await
        .with_context(|| format!("fetching feed {}", config.url))?
        .bytes()
        .await?;

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
            let link_html = format!(r#"<p><a href="{url}">{url}</a></p>"#);
            html.push_str(&sanitize::sanitize_html(&link_html));
        }

        if html.len() > MAX_CONTENT_LEN {
            truncate_utf8(&mut html, MAX_CONTENT_LEN);
            // Re-sanitize truncated HTML to close any open tags
            html = sanitize::sanitize_html(&html);
            if let Some(ref url) = link {
                let link_html = format!(r#"<a href="{url}">read more</a>"#);
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
            crate::delivery::fan_out(pool, &id, &persona_id).await?;
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
pub async fn run_poller(pool: SqlitePool, config: FeedConfig, domain: String) {
    let interval = config.interval();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    loop {
        match poll_feed(&pool, &config, &domain, &client).await {
            Ok(n) if n > 0 => tracing::info!(feed = %config.url, new = n, "feed poll complete"),
            Ok(_) => {}
            Err(e) => tracing::error!(feed = %config.url, error = %e, "feed poll failed"),
        }
        tokio::time::sleep(interval).await;
    }
}

/// One-shot poll of all configured feeds.
pub async fn poll_all(pool: &SqlitePool, feeds: &[FeedConfig], domain: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();
    for feed in feeds {
        match poll_feed(pool, feed, domain, &client).await {
            Ok(n) => println!("{}: {} new posts", feed.url, n),
            Err(e) => eprintln!("{}: error: {e}", feed.url),
        }
    }
    Ok(())
}
