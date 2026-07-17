use ammonia::Builder;
use std::collections::HashSet;

/// Sanitize HTML to the Mastodon-compatible allowlist.
pub fn sanitize_html(html: &str) -> String {
    let tags: HashSet<&str> = [
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

    let mut attrs = std::collections::HashMap::new();
    let a_attrs: HashSet<&str> = ["href", "rel"].into_iter().collect();
    attrs.insert("a", a_attrs);

    Builder::new()
        .tags(tags)
        .tag_attributes(attrs)
        .clean(html)
        .to_string()
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
