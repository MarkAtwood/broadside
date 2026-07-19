use anyhow::Context;
use sqlx::SqlitePool;
use std::path::Path;

use crate::id::gen_id;

/// Parsed OpenGraph / Twitter Card metadata.
#[derive(Debug, Default)]
pub struct CardMeta {
    pub url: String,
    pub title: String,
    pub description: String,
    pub image_url: String,
    pub site_name: String,
    pub card_type: String,
}

/// Extract the first https URL from HTML content.
pub fn extract_first_url(html: &str) -> Option<String> {
    // Match href="https://..." or bare https:// URLs in text
    static URL_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"https://[^\s"'<>\])]+"#).unwrap());
    URL_RE.find(html).map(|m| m.as_str().to_string())
}

/// Parse OG/Twitter meta tags from HTML bytes (only scans first 100KB).
pub fn parse_og_meta(html: &[u8], url: &str) -> CardMeta {
    let scan_limit = html.len().min(102400);
    let text = String::from_utf8_lossy(&html[..scan_limit]);

    let mut meta = CardMeta {
        url: url.to_string(),
        card_type: "link".to_string(),
        ..Default::default()
    };

    // Simple meta tag scanner — no full DOM parse needed
    let regexes = meta_tag_regexes();
    // Collect matches from both orderings: property-first and content-first
    let mut matches: Vec<(&str, &str)> = Vec::new();
    for cap in regexes[0].captures_iter(&text) {
        let property = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let content = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        matches.push((property, content));
    }
    for cap in regexes[1].captures_iter(&text) {
        // Reversed: content is group 1, property/name is group 2
        let content = cap.get(1).map(|m| m.as_str()).unwrap_or("");
        let property = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        matches.push((property, content));
    }
    for (property, content) in matches {
        match property {
            "og:title" | "twitter:title" => {
                if meta.title.is_empty() {
                    meta.title = html_decode(content);
                }
            }
            "og:description" | "twitter:description" => {
                if meta.description.is_empty() {
                    meta.description = html_decode(content);
                }
            }
            "og:image" | "twitter:image" | "twitter:image:src" => {
                if meta.image_url.is_empty() {
                    meta.image_url = html_decode(content);
                }
            }
            "og:site_name" => {
                if meta.site_name.is_empty() {
                    meta.site_name = html_decode(content);
                }
            }
            "og:type" => {
                let t = content.to_lowercase();
                if t.contains("video") {
                    meta.card_type = "video".to_string();
                } else if t.contains("photo") || t == "image" {
                    meta.card_type = "photo".to_string();
                }
            }
            _ => {}
        }
    }

    // Fallback: extract <title> if no og:title
    if meta.title.is_empty() {
        if let Some(cap) = title_re().captures(&text) {
            if let Some(m) = cap.get(1) {
                meta.title = html_decode(m.as_str());
            }
        }
    }

    meta
}

fn meta_tag_regexes() -> &'static [regex::Regex; 2] {
    static RES: std::sync::LazyLock<[regex::Regex; 2]> = std::sync::LazyLock::new(|| {
        [
            // property/name before content
            regex::Regex::new(
                r#"(?i)<meta\s[^>]*(?:property|name)\s*=\s*["']([^"']+)["'][^>]*content\s*=\s*["']([^"']*)["'][^>]*/?\s*>"#,
            ).unwrap(),
            // content before property/name (reversed order)
            regex::Regex::new(
                r#"(?i)<meta\s[^>]*content\s*=\s*["']([^"']*)["'][^>]*(?:property|name)\s*=\s*["']([^"']+)["'][^>]*/?\s*>"#,
            ).unwrap(),
        ]
    });
    &RES
}

fn title_re() -> &'static regex::Regex {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)<title[^>]*>([^<]+)</title>").unwrap());
    &RE
}

fn html_decode(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
}

/// Fetch OG metadata for a URL and store the card in the database.
/// Designed to be called from a spawned background task.
pub async fn fetch_and_store(
    pool: &SqlitePool,
    post_id: &str,
    url: &str,
    data_dir: &Path,
    client: &reqwest::Client,
    domain: &str,
) -> anyhow::Result<()> {
    // SSRF guard
    let parsed = url::Url::parse(url)?;
    if let Some(host) = parsed.host_str() {
        if crate::server::is_private_host_resolved(host).await {
            anyhow::bail!("card URL resolves to private host: {url}");
        }
    }

    // Fetch with size limit (1MB for HTML)
    let resp = client
        .get(url)
        .header("User-Agent", format!("Broadside/0.2 (+https://{domain})"))
        .send()
        .await
        .context("fetching card URL")?;

    if !resp.status().is_success() {
        anyhow::bail!("card fetch returned {}", resp.status());
    }

    let body = crate::http::read_body_limited(resp, 1024 * 1024)
        .await
        .context("reading card body")?;

    let meta = parse_og_meta(&body, url);

    // Skip if we got nothing useful
    if meta.title.is_empty() && meta.description.is_empty() {
        return Ok(());
    }

    // Download and cache the preview image if present
    let image_path = if !meta.image_url.is_empty() {
        fetch_card_image(client, &meta.image_url, data_dir, domain)
            .await
            .unwrap_or_default()
    } else {
        String::new()
    };

    let id = gen_id();
    sqlx::query(
        "INSERT OR REPLACE INTO cards (id, post_id, url, title, description, image_url, image_path, site_name, card_type) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(post_id)
    .bind(url)
    .bind(&meta.title)
    .bind(&meta.description)
    .bind(&meta.image_url)
    .bind(&image_path)
    .bind(&meta.site_name)
    .bind(&meta.card_type)
    .execute(pool)
    .await
    .context("inserting card")?;

    tracing::debug!(post_id, url, title = %meta.title, "card fetched");
    Ok(())
}

/// Download and resize a card preview image. Returns the stored file path (relative to data_dir/media),
/// or None on failure.
async fn fetch_card_image(
    client: &reqwest::Client,
    image_url: &str,
    data_dir: &Path,
    domain: &str,
) -> Option<String> {
    if !image_url.starts_with("https://") {
        return None;
    }

    let result = async {
        let parsed = url::Url::parse(image_url)?;
        if let Some(host) = parsed.host_str() {
            if crate::server::is_private_host_resolved(host).await {
                anyhow::bail!("card image URL resolves to private host");
            }
        }

        let resp = client
            .get(image_url)
            .header("User-Agent", format!("Broadside/0.2 (+https://{domain})"))
            .send()
            .await?;

        if !resp.status().is_success() {
            anyhow::bail!("image fetch returned {}", resp.status());
        }

        let bytes = crate::http::read_body_limited(resp, 5 * 1024 * 1024).await?;

        // Decode and resize to max 800x418 (standard OG image ratio)
        let img = image::load_from_memory(&bytes).context("decoding card image")?;
        let resized = if img.width() > 800 || img.height() > 418 {
            img.resize(800, 418, image::imageops::FilterType::Lanczos3)
        } else {
            img
        };

        let filename = format!("card-{}.jpg", gen_id());
        let out_path = data_dir.join("media").join(&filename);
        resized
            .to_rgb8()
            .save_with_format(&out_path, image::ImageFormat::Jpeg)
            .context("saving card image")?;

        Ok::<String, anyhow::Error>(filename)
    }
    .await;

    match result {
        Ok(path) => Some(path),
        Err(e) => {
            tracing::debug!(image_url, error = %e, "card image fetch failed");
            None
        }
    }
}

/// Get the card for a post (if any), for inclusion in AP Note objects.
pub async fn get_card_for_post(
    pool: &SqlitePool,
    post_id: &str,
    domain: &str,
) -> Option<serde_json::Value> {
    let row = sqlx::query_as::<_, (String, String, String, String, String, String)>(
        "SELECT url, title, description, image_path, site_name, card_type FROM cards WHERE post_id = ?",
    )
    .bind(post_id)
    .fetch_optional(pool)
    .await
    .ok()??;

    let (url, title, description, image_path, site_name, _card_type) = row;

    let mut card = serde_json::json!({
        "type": "Link",
        "href": url,
        "name": title,
    });

    if !description.is_empty() {
        card["summary"] = serde_json::Value::String(description);
    }
    if !site_name.is_empty() {
        card["attributedTo"] = serde_json::Value::String(site_name);
    }
    if !image_path.is_empty() {
        card["icon"] = serde_json::json!({
            "type": "Image",
            "url": format!("https://{domain}/media/{image_path}"),
        });
    }

    Some(card)
}

/// Spawn a background task to fetch the link preview card for a post.
/// Extracts the first URL from `content_html` and fetches its OG metadata.
// ponytail: owned Strings are required here — spawned tasks need 'static ownership,
// so callers must clone into these parameters. No way around it with tokio::spawn.
pub fn spawn_fetch(
    pool: SqlitePool,
    post_id: String,
    content_html: String,
    data_dir: String,
    client: reqwest::Client,
    domain: String,
) {
    let url = match extract_first_url(&content_html) {
        Some(u) => u,
        None => return,
    };

    tokio::spawn(async move {
        if let Err(e) = fetch_and_store(
            &pool,
            &post_id,
            &url,
            Path::new(&data_dir),
            &client,
            &domain,
        )
        .await
        {
            tracing::debug!(post_id = %post_id, url = %url, error = %e, "card fetch failed");
        }
    });
}
