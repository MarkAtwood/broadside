use anyhow::{bail, Context};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub feed: Vec<FeedConfig>,
    #[serde(default)]
    pub webhook: Vec<WebhookConfig>,
    pub watch: Option<WatchConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    pub domain: String,
    pub data_dir: String,
    #[serde(default)]
    pub theme_tokens_path: String,
    #[serde(default)]
    pub custom_css_path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeedConfig {
    pub persona: String,
    pub url: String,
    pub poll_interval: String,
}

#[derive(Deserialize)]
pub struct WebhookConfig {
    pub persona: String,
    pub key: String,
}

impl std::fmt::Debug for WebhookConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookConfig")
            .field("persona", &self.persona)
            .field("key", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WatchConfig {
    pub persona: String,
    pub path: String,
    pub published: String,
    pub pattern: String,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("reading config from {}", path.display()))?;
        let config: Config = toml::from_str(&contents)
            .with_context(|| format!("parsing config from {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> anyhow::Result<()> {
        for (i, feed) in self.feed.iter().enumerate() {
            parse_duration(&feed.poll_interval)
                .with_context(|| format!("feed[{i}].poll_interval"))?;
        }
        if let Some(watch) = &self.watch {
            let path = Path::new(&watch.path);
            if !path.is_absolute() {
                bail!("watch.path must be absolute, got: {}", watch.path);
            }
            let published = Path::new(&watch.published);
            if !published.is_absolute() {
                bail!("watch.published must be absolute, got: {}", watch.published);
            }
        }
        Ok(())
    }
}

impl FeedConfig {
    pub fn interval(&self) -> Duration {
        parse_duration(&self.poll_interval).expect("validated on load")
    }
}

/// Parse a human-friendly duration string like "15m", "2h", "30s", "1d".
fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty duration string");
    }
    let (digits, suffix) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = digits
        .parse()
        .with_context(|| format!("invalid duration number in {s:?}"))?;
    let multiplier: u64 = match suffix.trim() {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86400,
        "" => bail!("duration {s:?} missing unit (s/m/h/d)"),
        other => bail!("unknown duration unit {other:?} in {s:?}"),
    };
    let secs = n
        .checked_mul(multiplier)
        .with_context(|| format!("duration {s:?} overflows"))?;
    Ok(Duration::from_secs(secs))
}
