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

    // Light mode from "color" group
    if let Some(colors) = doc.get("color").and_then(|v| v.as_object()) {
        let mut vars = String::new();
        for (name, token) in colors {
            if let (Some(var), Some(value)) = (
                token_to_var(name),
                token.get("$value").and_then(|v| v.as_str()),
            ) {
                vars.push_str(&format!("{var}:{value};"));
            }
        }
        if !vars.is_empty() {
            css.push_str(&format!(":root{{{vars}}}"));
        }
    }

    // Dark mode from "color-dark" group
    if let Some(colors) = doc.get("color-dark").and_then(|v| v.as_object()) {
        let mut vars = String::new();
        for (name, token) in colors {
            if let (Some(var), Some(value)) = (
                token_to_var(name),
                token.get("$value").and_then(|v| v.as_str()),
            ) {
                vars.push_str(&format!("{var}:{value};"));
            }
        }
        if !vars.is_empty() {
            css.push_str(&format!(
                "@media(prefers-color-scheme:dark){{:root{{{vars}}}}}"
            ));
        }
    }

    css
}
