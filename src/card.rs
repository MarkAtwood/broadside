use anyhow::Context;
use fieldwork::db::sqlx::SqlitePool;
use std::path::Path;

/// Wrap a raw SqlitePool in fieldwork's Pool enum for shared module calls.
fn fw_pool(pool: &SqlitePool) -> fieldwork::db::Pool {
    fieldwork::db::Pool::Sqlite(pool.clone())
}

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

/// Extract the first https URL from HTML content, skipping URLs inside href/src attributes.
pub fn extract_first_url(html: &str) -> Option<&str> {
    // Negative lookbehind via manual scan: skip matches preceded by href=" or src="
    static URL_RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"https://[^\s"'<>\])]+"#).unwrap());
    for m in URL_RE.find_iter(html) {
        let start = m.start();
        // Check if preceded by href=" or src=" (6 chars max lookbehind)
        let prefix = &html[..start];
        if prefix.ends_with(r#"href=""#) || prefix.ends_with(r#"src=""#) {
            continue;
        }
        return Some(m.as_str());
    }
    None
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
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let (replacement, consumed) = if rest.starts_with("&amp;") {
            ("&", 5)
        } else if rest.starts_with("&lt;") {
            ("<", 4)
        } else if rest.starts_with("&gt;") {
            (">", 4)
        } else if rest.starts_with("&quot;") {
            ("\"", 6)
        } else if rest.starts_with("&#39;") {
            ("'", 5)
        } else if rest.starts_with("&#x27;") {
            ("'", 6)
        } else {
            ("", 0)
        };
        if consumed > 0 {
            out.push_str(replacement);
            rest = &rest[consumed..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

/// Fetch OG metadata for a URL and store the card in the database.
/// Designed to be called from a spawned background task.
pub async fn fetch_and_store(
    pool: &SqlitePool,
    post_id: &str,
    url: &str,
    _data_dir: &Path,
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

    // Sanitize remote-supplied metadata: strip HTML and cap field lengths
    let mut title = crate::sanitize::html_to_text(&meta.title);
    let mut description = crate::sanitize::html_to_text(&meta.description);
    let mut site_name = crate::sanitize::html_to_text(&meta.site_name);
    crate::sanitize::truncate_utf8(&mut title, 512);
    crate::sanitize::truncate_utf8(&mut description, 2048);
    crate::sanitize::truncate_utf8(&mut site_name, 256);

    let now = chrono::Utc::now().timestamp();
    let fwp = fw_pool(pool);
    let image = if meta.image_url.starts_with("https://") {
        Some(meta.image_url.clone())
    } else {
        None
    };
    let card_row = fieldwork::cards_db::CardRow {
        id: 0, // ignored by upsert_card
        url: url.to_string(),
        card_type: meta.card_type.clone(),
        title,
        description,
        image_url: image,
        author_name: String::new(),
        author_url: String::new(),
        provider_name: site_name,
        provider_url: String::new(),
        html: String::new(),
        width: 0,
        height: 0,
        fetched_at: now,
        failed: false,
    };
    fieldwork::cards_db::upsert_card(&fwp, &card_row)
        .await
        .context("inserting link_card")?;

    let post_id_int: i64 = post_id
        .parse()
        .context("post_id is not a valid integer")?;
    fieldwork::cards_db::link_card_to_post(&fwp, post_id_int, url)
        .await
        .context("linking post to card")?;

    tracing::debug!(post_id, url, title = %meta.title, "card fetched");
    Ok(())
}

/// Get the card for a post (if any), for inclusion in AP Note objects.
pub async fn get_card_for_post(
    pool: &SqlitePool,
    post_id: &str,
    _domain: &str,
) -> Option<serde_json::Value> {
    let post_id_int: i64 = post_id.parse().ok()?;
    let fwp = fw_pool(pool);
    let cards = fieldwork::cards_db::cards_for_post(&fwp, post_id_int).await.ok()?;
    let row = cards.into_iter().next()?;

    let mut card = serde_json::json!({
        "type": "Link",
        "href": row.url,
        "name": row.title,
    });

    if !row.description.is_empty() {
        card["summary"] = serde_json::Value::String(row.description);
    }
    if !row.provider_name.is_empty() {
        card["attributedTo"] = serde_json::Value::String(row.provider_name);
    }
    if let Some(ref img) = row.image_url {
        if !img.is_empty() {
            card["icon"] = serde_json::json!({
                "type": "Image",
                "url": img,
            });
        }
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
        Some(u) => u.to_string(),
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
