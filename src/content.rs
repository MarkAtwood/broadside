use regex::Regex;
use std::sync::LazyLock;

/// Hashtag pattern: #word at start or after whitespace/tag boundary
static HASHTAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[\s>])#([a-zA-Z][a-zA-Z0-9_]*)").expect("hashtag regex"));

/// Mention pattern: @user@domain at start or after whitespace/tag boundary
static MENTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[\s>])@([a-zA-Z0-9_]+)@([a-zA-Z0-9][a-zA-Z0-9.\-]+[a-zA-Z0-9])")
        .expect("mention regex")
});

/// Bare URL pattern
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://[^\s<>\x22')\]]+[a-zA-Z0-9/]").expect("url regex"));

/// Matches existing <a ...>...</a> spans to skip during linkification.
static LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<a\s[^>]*>[\s\S]*?</a>").expect("link regex"));

/// ActivityPub tag type.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
#[non_exhaustive]
pub enum TagType {
    Hashtag,
    Mention,
}

/// A tag entry for the ActivityPub Note object.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Tag {
    #[serde(rename = "type")]
    pub tag_type: TagType,
    pub href: String,
    pub name: String,
}

/// Extract hashtags, mentions, and bare URLs from HTML content.
/// Returns processed HTML with links and a tag array for the AP Note.
///
/// Uses position-aware replacement: collects all matches from the original
/// HTML, skips anything inside existing `<a>` tags, then applies replacements
/// right-to-left so byte offsets stay valid.
pub fn process_content(html: &str, domain: &str) -> (String, Vec<Tag>) {
    let mut tags = Vec::new();

    // Find existing <a> tag spans to skip
    let skip_ranges: Vec<(usize, usize)> = LINK_RE
        .find_iter(html)
        .map(|m| (m.start(), m.end()))
        .collect();

    let in_link = |pos: usize| -> bool {
        // Binary search: find the last range whose start <= pos, then check if pos < end.
        let idx = skip_ranges.partition_point(|(start, _)| *start <= pos);
        idx > 0 && pos < skip_ranges[idx - 1].1
    };

    // Collect all replacements from the ORIGINAL html (positions are stable)
    let mut replacements: Vec<(usize, usize, String)> = Vec::new();

    // Bare URLs
    for m in URL_RE.find_iter(html) {
        if in_link(m.start()) {
            continue;
        }
        let url = m.as_str();
        let linked = format!(
            r#"<a href="{url}" rel="nofollow noopener noreferrer" target="_blank">{url}</a>"#
        );
        replacements.push((m.start(), m.end(), linked));
    }

    // Hashtags
    for cap in HASHTAG_RE.captures_iter(html) {
        let name_match = cap.get(1).expect("hashtag capture group");
        // The # is one byte before the captured name
        let hash_pos = name_match.start() - 1;
        if in_link(hash_pos) {
            continue;
        }
        let name = name_match.as_str();
        let lower = name.to_lowercase();
        let href = format!("https://{domain}/tags/{lower}");
        let linked = format!(r#"<a href="{href}" class="mention hashtag" rel="tag">#{name}</a>"#);
        replacements.push((hash_pos, name_match.end(), linked));

        if !tags.iter().any(|t: &Tag| t.name == format!("#{lower}")) {
            tags.push(Tag {
                tag_type: TagType::Hashtag,
                href,
                name: format!("#{lower}"),
            });
        }
    }

    // Mentions
    for cap in MENTION_RE.captures_iter(html) {
        let user_match = cap.get(1).expect("mention user capture");
        let host_match = cap.get(2).expect("mention host capture");
        // The @ is one byte before the user capture
        let at_pos = user_match.start() - 1;
        if in_link(at_pos) {
            continue;
        }
        let user = user_match.as_str();
        let host = host_match.as_str();
        let href = format!("https://{host}/@{user}");
        let actor_href = format!("https://{host}/users/{user}");
        let linked = format!(
            r#"<span class="h-card"><a href="{href}" class="u-url mention">@<span>{user}</span></a></span>"#
        );
        replacements.push((at_pos, host_match.end(), linked));

        let mention_name = format!("@{user}@{host}");
        if !tags.iter().any(|t: &Tag| t.name == mention_name) {
            tags.push(Tag {
                tag_type: TagType::Mention,
                href: actor_href,
                name: mention_name,
            });
        }
    }

    // Sort by start position descending, then apply right-to-left
    replacements.sort_by_key(|r| std::cmp::Reverse(r.0));

    // Remove overlapping replacements (keep the one with the earliest start)
    let mut filtered: Vec<(usize, usize, String)> = Vec::with_capacity(replacements.len());
    for r in replacements {
        if filtered.last().map_or(true, |prev| r.1 <= prev.0) {
            filtered.push(r);
        }
    }

    let mut result = html.to_string();
    for (start, end, replacement) in filtered {
        result.replace_range(start..end, &replacement);
    }

    (result, tags)
}

/// Detect FEP-e232 quote post URLs (ActivityPub post URLs) in HTML content.
/// Returns Link tag objects suitable for the Note's `tag` array.
/// Excludes URLs that appear inside `<a>` tags (those are regular links, not quotes).
pub fn detect_quote_links(html: &str) -> Vec<serde_json::Value> {
    // Match typical AP post URLs: https://domain/users/username/statuses/id
    // Also matches: https://domain/@user/id (Mastodon shortform)
    static QUOTE_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"https://[a-zA-Z0-9.\-]+/(?:users/[a-zA-Z0-9_]+/statuses/[a-zA-Z0-9\-]+|@[a-zA-Z0-9_]+/[0-9]+)"#)
            .expect("quote URL regex")
    });

    // Build skip ranges for existing <a> tags
    let skip_ranges: Vec<(usize, usize)> = LINK_RE
        .find_iter(html)
        .map(|m| (m.start(), m.end()))
        .collect();

    let in_link = |pos: usize| -> bool {
        let idx = skip_ranges.partition_point(|(start, _)| *start <= pos);
        idx > 0 && pos < skip_ranges[idx - 1].1
    };

    let mut links = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for m in QUOTE_RE.find_iter(html) {
        if in_link(m.start()) {
            continue;
        }
        let url = m.as_str();
        if seen.insert(url) {
            links.push(serde_json::json!({
                "type": "Link",
                "href": url,
                "mediaType": "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\""
            }));
        }
    }
    links
}

/// Detect the language of plain text content. Returns an ISO 639-1 code (e.g. "en", "de")
/// if confidence is high enough, or None.
pub fn detect_language(text: &str) -> Option<&'static str> {
    if text.len() < 20 {
        return None; // Too short for reliable detection
    }
    let info = whatlang::detect(text)?;
    if info.confidence() < 0.8 {
        return None;
    }
    Some(info.lang().code())
}
