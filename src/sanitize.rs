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

/// Strip HTML tags to get plain text.
pub fn html_to_text(html: &str) -> String {
    // ponytail: ammonia with no tags allowed strips everything to text
    Builder::new()
        .tags(std::collections::HashSet::new())
        .clean(html)
        .to_string()
}
