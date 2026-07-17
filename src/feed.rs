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
) -> anyhow::Result<u32> {
    let persona_id = crate::persona::get_id(pool, &config.persona).await?;

    let client = reqwest::Client::new();
    let body = client
        .get(&config.url)
        .send()
        .await
        .with_context(|| format!("fetching feed {}", config.url))?
        .bytes()
        .await?;

    let feed = feed_rs::parser::parse(&body[..])
        .with_context(|| format!("parsing feed {}", config.url))?;

    // Get last seen ID
    let last_seen = sqlx::query_as::<_, (Option<String>,)>(
        "SELECT last_seen_id FROM feed_state WHERE feed_url = ?",
    )
    .bind(&config.url)
    .fetch_optional(pool)
    .await?
    .and_then(|(id,)| id);

    let mut new_count = 0u32;
    let mut newest_id: Option<String> = None;

    // Process entries in reverse order (oldest first) so newest_id ends up correct
    let entries: Vec<_> = feed.entries.into_iter().rev().collect();

    for entry in &entries {
        let entry_id = entry.id.clone();

        // Skip entries we've already seen
        if let Some(ref seen) = last_seen {
            if &entry_id == seen {
                break;
            }
        }

        // Build content
        let title = entry.title.as_ref().map(|t| t.content.clone());
        let body_html = entry
            .content
            .as_ref()
            .and_then(|c| c.body.clone())
            .or_else(|| entry.summary.as_ref().map(|s| s.content.clone()))
            .unwrap_or_default();

        let link = entry
            .links
            .first()
            .map(|l| l.href.clone());

        let mut html = String::new();
        if let Some(ref t) = title {
            html.push_str(&format!("<p><strong>{}</strong></p>", ammonia::clean(t)));
        }
        html.push_str(&sanitize::sanitize_html(&body_html));

        if let Some(ref url) = link {
            html.push_str(&format!(
                r#"<p><a href="{url}">{url}</a></p>"#
            ));
        }

        // Truncate if too long
        if html.len() > MAX_CONTENT_LEN {
            html.truncate(MAX_CONTENT_LEN);
            if let Some(ref url) = link {
                html.push_str(&format!(
                    r#"… <a href="{url}">read more</a>"#
                ));
            }
        }

        let text = sanitize::html_to_text(&html);

        // Insert post (dedup on source_ref)
        match crate::post::create(pool, &persona_id, &html, &text, Some(&entry_id)).await {
            Ok(post_id) => {
                crate::delivery::fan_out(pool, &post_id, &persona_id).await?;
                new_count += 1;
                newest_id = Some(entry_id);
            }
            Err(e) => {
                // UNIQUE constraint = already seen, skip
                if e.to_string().contains("UNIQUE") {
                    continue;
                }
                return Err(e);
            }
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
    loop {
        match poll_feed(&pool, &config, &domain).await {
            Ok(n) if n > 0 => tracing::info!(feed = %config.url, new = n, "feed poll complete"),
            Ok(_) => {}
            Err(e) => tracing::error!(feed = %config.url, error = %e, "feed poll failed"),
        }
        tokio::time::sleep(interval).await;
    }
}

/// One-shot poll of all configured feeds.
pub async fn poll_all(pool: &SqlitePool, feeds: &[FeedConfig], domain: &str) -> anyhow::Result<()> {
    for feed in feeds {
        match poll_feed(pool, feed, domain).await {
            Ok(n) => println!("{}: {} new posts", feed.url, n),
            Err(e) => eprintln!("{}: error: {e}", feed.url),
        }
    }
    Ok(())
}
