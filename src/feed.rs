use anyhow::Context;
use sqlx::SqlitePool;

use crate::config::FeedConfig;
use crate::sanitize;

/// Wrap a raw SqlitePool in fieldwork's Pool enum for shared module calls.
fn fw_pool(pool: &SqlitePool) -> fieldwork::db::Pool {
    fieldwork::db::Pool::Sqlite(pool.clone())
}

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
            // escape_html_attr produces a safe href; sanitize_html is defense-in-depth
            // in case future changes alter the template structure.
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

        // Use INSERT OR IGNORE for dedup via broadside_post_meta.source_ref UNIQUE constraint
        let id = crate::id::gen_int_id();
        let now = chrono::Utc::now().timestamp();
        let user_id = crate::persona::get_operator_user_id(pool).await?;
        let ap_id = format!("urn:broadside:post:{id}");
        let id_str = id.to_string();

        // Remaining SQL: broadside_post_meta is a broadside-specific table for dedup.
        let exists = sqlx::query_as::<_, (i64,)>(
            "SELECT COUNT(*) FROM broadside_post_meta WHERE source_ref = ?",
        )
        .bind(&entry_id)
        .fetch_one(pool)
        .await?;

        if exists.0 > 0 {
            continue;
        }

        let fwp = fw_pool(pool);
        let post = fieldwork::posts_db::PostRow {
            id,
            user_id: user_id.clone(),
            persona_id: persona_id.clone(),
            ap_id,
            in_reply_to_id: None,
            in_reply_to_uri: None,
            boost_of_id: None,
            boost_of_uri: None,
            content: text.clone(),
            content_html: html.clone(),
            spoiler_text: String::new(),
            visibility: "public".to_string(),
            sensitive: false,
            language: None,
            context_url: None,
            created_at: now,
            edited_at: None,
            deleted_at: None,
            deleted_reason: None,
        };
        let post_ok = fieldwork::posts_db::create_post(&fwp, &post).await.is_ok();

        // Remaining SQL: broadside_post_meta is a broadside-specific table.
        let _ = sqlx::query(
            "INSERT OR IGNORE INTO broadside_post_meta (post_id, source_ref) VALUES (?, ?)",
        )
        .bind(id)
        .bind(&entry_id)
        .execute(pool)
        .await;

        if post_ok {
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
                            .map(|m| m.as_str())
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
                                pool, &id_str, url_str, data_dir, "", client,
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
                id_str.clone(),
                html.clone(),
                data_dir.to_str().unwrap_or(".").to_string(),
                client.clone(),
                domain.to_string(),
            );

            if let Err(e) = crate::delivery::fan_out(pool, &id_str, &persona_id).await {
                tracing::error!(post_id = %id_str, error = %e, "fan_out failed for new post");
            }
            new_count += 1;
            newest_id = Some(entry_id);
        }
    }

    // Remaining SQL: feed_state is a broadside-specific table with no fieldwork equivalent.
    if let Some(ref nid) = newest_id {
        let now = chrono::Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO feed_state (feed_url, persona_id, last_seen_id, last_poll) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT(feed_url) DO UPDATE SET last_seen_id = ?, last_poll = ?",
        )
        .bind(&config.url)
        .bind(&persona_id)
        .bind(nid)
        .bind(now)
        .bind(nid)
        .bind(now)
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
