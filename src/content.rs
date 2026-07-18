use regex::Regex;
use std::sync::LazyLock;

/// Hashtag pattern: #word at start or after whitespace/tag boundary
static HASHTAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[\s>])#([a-zA-Z][a-zA-Z0-9_]*)").unwrap());

/// Mention pattern: @user@domain at start or after whitespace/tag boundary
static MENTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[\s>])@([a-zA-Z0-9_]+)@([a-zA-Z0-9][a-zA-Z0-9.\-]+[a-zA-Z0-9])").unwrap()
});

/// Bare URL pattern
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://[^\s<>\x22')\]]+[a-zA-Z0-9/]").unwrap());

/// A tag entry for the ActivityPub Note object.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Tag {
    #[serde(rename = "type")]
    pub tag_type: String,
    pub href: String,
    pub name: String,
}

/// Extract hashtags, mentions, and bare URLs from HTML content.
/// Returns processed HTML with links and a tag array for the AP Note.
///
/// `domain` is this server's domain (for hashtag hrefs).
pub fn process_content(html: &str, domain: &str) -> (String, Vec<Tag>) {
    let mut tags = Vec::new();
    let mut result = html.to_string();

    // Auto-link bare URLs (skip if already inside an <a> tag)
    let url_matches: Vec<String> = URL_RE
        .find_iter(&result)
        .map(|m| m.as_str().to_string())
        .collect();
    for url in &url_matches {
        if let Some(pos) = result.find(url.as_str()) {
            let before = &result[..pos];
            // Don't link if already inside an href attribute
            if !before.ends_with("href=\"") && !before.ends_with("href='") {
                let linked = format!(
                    r#"<a href="{url}" rel="nofollow noopener noreferrer" target="_blank">{url}</a>"#
                );
                result = format!("{}{}{}", &result[..pos], linked, &result[pos + url.len()..]);
            }
        }
    }

    // Process hashtags
    // We need to match on the original html (not result, which may have links now)
    // but apply to result. Use the original html for finding hashtag text.
    let hashtag_matches: Vec<(String, String)> = HASHTAG_RE
        .captures_iter(html)
        .map(|cap| {
            let name = cap.get(1).unwrap().as_str().to_string();
            let hashtag_text = format!("#{name}");
            (hashtag_text, name)
        })
        .collect();

    for (hashtag_text, name) in &hashtag_matches {
        let lower = name.to_lowercase();
        let href = format!("https://{domain}/tags/{lower}");
        let linked = format!(r#"<a href="{href}" class="mention hashtag" rel="tag">#{name}</a>"#);
        if let Some(pos) = result.find(hashtag_text.as_str()) {
            // Make sure we're not inside an existing <a> tag
            let before = &result[..pos];
            if !before.ends_with("href=\"") && !before.contains("<a ") || before.contains("</a>") {
                result = format!(
                    "{}{}{}",
                    &result[..pos],
                    linked,
                    &result[pos + hashtag_text.len()..]
                );
            }
        }

        if !tags.iter().any(|t: &Tag| t.name == format!("#{lower}")) {
            tags.push(Tag {
                tag_type: "Hashtag".to_string(),
                href,
                name: format!("#{lower}"),
            });
        }
    }

    // Process mentions
    let mention_matches: Vec<(String, String, String)> = MENTION_RE
        .captures_iter(html)
        .map(|cap| {
            let user = cap.get(1).unwrap().as_str().to_string();
            let host = cap.get(2).unwrap().as_str().to_string();
            let mention_text = format!("@{user}@{host}");
            (mention_text, user, host)
        })
        .collect();

    for (mention_text, user, host) in &mention_matches {
        let href = format!("https://{host}/@{user}");
        let actor_href = format!("https://{host}/users/{user}");
        let linked = format!(
            r#"<span class="h-card"><a href="{href}" class="u-url mention">@<span>{user}</span></a></span>"#
        );
        if let Some(pos) = result.find(mention_text.as_str()) {
            result = format!(
                "{}{}{}",
                &result[..pos],
                linked,
                &result[pos + mention_text.len()..]
            );
        }

        let mention_name = format!("@{user}@{host}");
        if !tags.iter().any(|t: &Tag| t.name == mention_name) {
            tags.push(Tag {
                tag_type: "Mention".to_string(),
                href: actor_href,
                name: mention_name,
            });
        }
    }

    (result, tags)
}
