use ammonia::Builder;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

static SANITIZE_BUILDER: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let tags: HashSet<&'static str> = [
        "p",
        "br",
        "a",
        "span",
        "em",
        "strong",
        "del",
        "blockquote",
        "code",
        "pre",
        "ul",
        "ol",
        "li",
    ]
    .into_iter()
    .collect();

    let mut attrs: HashMap<&'static str, HashSet<&'static str>> = HashMap::new();
    // ammonia 4.x manages rel= on <a> tags internally (adds noopener noreferrer)
    attrs.insert("a", ["href"].into_iter().collect());

    let mut b = Builder::new();
    b.tags(tags).tag_attributes(attrs);
    b
});

/// Sanitize HTML to the Mastodon-compatible allowlist.
pub fn sanitize_html(html: &str) -> String {
    SANITIZE_BUILDER.clean(html).to_string()
}

/// Render markdown to sanitized HTML.
pub fn markdown_to_html(markdown: &str) -> String {
    let parser = pulldown_cmark::Parser::new(markdown);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    sanitize_html(&html)
}

static TEXT_BUILDER: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let mut b = Builder::new();
    b.tags(std::collections::HashSet::new());
    b
});

/// Strip HTML tags to get plain text.
pub fn html_to_text(html: &str) -> String {
    TEXT_BUILDER.clean(html).to_string()
}

/// Truncate a string at a UTF-8 safe boundary.
pub fn truncate_utf8(s: &mut String, max_len: usize) {
    if s.len() <= max_len {
        return;
    }
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

/// Escape a string for safe interpolation in double-quoted HTML attributes.
pub fn escape_html_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
