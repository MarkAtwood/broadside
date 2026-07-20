/// Token name to CSS variable mapping.
fn token_to_var(name: &str) -> Option<&'static str> {
    match name {
        "primary" => Some("--link"),
        "background" => Some("--bg"),
        "surface" => Some("--card"),
        "text" => Some("--text"),
        "muted" => Some("--muted"),
        "border" => Some("--border"),
        _ => None,
    }
}

/// Reject CSS values that could inject rules or exfiltrate data.
fn is_safe_css_value(s: &str) -> bool {
    s.len() <= 100
        && !s.contains('}')
        && !s.contains(';')
        && !s.contains("url(")
        && !s.contains("expression(")
        && !s.contains("@import")
        && !s.contains("javascript:")
}

/// Extract CSS variable declarations from a design tokens color group.
fn vars_from_group(group: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut vars = String::new();
    for (name, token) in group {
        if let (Some(var), Some(value)) = (
            token_to_var(name),
            token.get("$value").and_then(|v| v.as_str()),
        ) {
            if is_safe_css_value(value) {
                vars.push_str(&format!("{var}:{value};"));
            } else {
                tracing::warn!(token = name, "rejecting unsafe CSS token value");
            }
        }
    }
    vars
}

/// Load a W3C Design Tokens JSON file and return CSS custom property overrides.
pub fn load_theme_css(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(path, "failed to load theme tokens: {e}");
            return String::new();
        }
    };
    let doc: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(path, "failed to parse theme tokens JSON: {e}");
            return String::new();
        }
    };

    let mut css = String::new();

    if let Some(colors) = doc.get("color").and_then(|v| v.as_object()) {
        let vars = vars_from_group(colors);
        if !vars.is_empty() {
            css.push_str(&format!(":root{{{vars}}}"));
        }
    }

    if let Some(colors) = doc.get("color-dark").and_then(|v| v.as_object()) {
        let vars = vars_from_group(colors);
        if !vars.is_empty() {
            css.push_str(&format!(
                "@media(prefers-color-scheme:dark){{:root{{{vars}}}}}"
            ));
        }
    }

    css
}
