use anyhow::Context;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const CACHE_TTL: Duration = Duration::from_secs(86400); // 24 hours
const MAX_ENTRIES: usize = 10000;

/// Cached actor public key and key ID.
struct CachedActor {
    public_key_pem: String,
    key_id: String,
    fetched_at: Instant,
}

/// In-memory cache for remote actor public keys.
pub struct ActorKeyCache {
    entries: Mutex<HashMap<String, CachedActor>>,
    client: reqwest::Client,
}

impl ActorKeyCache {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            client,
        }
    }

    /// Get the public key PEM for a remote actor. Fetches and caches if not present.
    pub async fn get_public_key(&self, actor_uri: &str) -> anyhow::Result<(String, String)> {
        // Check cache
        {
            let entries = self.entries.lock().await;
            if let Some(cached) = entries.get(actor_uri) {
                if cached.fetched_at.elapsed() < CACHE_TTL {
                    return Ok((cached.public_key_pem.clone(), cached.key_id.clone()));
                }
            }
        }

        // SSRF guard: only fetch public https URLs
        if !actor_uri.starts_with("https://") {
            anyhow::bail!("actor URI must be https: {actor_uri}");
        }
        if let Ok(parsed) = url::Url::parse(actor_uri) {
            if let Some(host) = parsed.host_str() {
                if crate::server::is_private_host_resolved(host).await {
                    anyhow::bail!("actor URI points to private host: {actor_uri}");
                }
            }
        }

        // Fetch actor document with body size limit (64KB is sufficient for any actor doc)
        let resp = self
            .client
            .get(actor_uri)
            .header("Accept", "application/activity+json")
            .send()
            .await
            .with_context(|| format!("fetching actor {actor_uri}"))?;

        let body = crate::http::read_body_limited(resp, 65536)
            .await
            .with_context(|| format!("reading actor document from {actor_uri}"))?;
        let actor_doc: serde_json::Value = serde_json::from_slice(&body)
            .with_context(|| format!("parsing actor document from {actor_uri}"))?;

        let public_key_pem = actor_doc["publicKey"]["publicKeyPem"]
            .as_str()
            .with_context(|| format!("no publicKeyPem in actor document from {actor_uri}"))?
            .to_string();

        // Validate that the key belongs to the actor we requested.
        // Require owner field — reject actor documents that omit it.
        let key_owner = actor_doc["publicKey"]["owner"]
            .as_str()
            .with_context(|| format!("no publicKey.owner in actor document from {actor_uri}"))?;
        if key_owner != actor_uri {
            anyhow::bail!("publicKey.owner mismatch: expected {actor_uri}, got {key_owner}");
        }

        let key_id = actor_doc["publicKey"]["id"]
            .as_str()
            .unwrap_or(actor_uri)
            .to_string();

        // Cache it
        {
            let mut entries = self.entries.lock().await;
            // Evict if too full: clear half the cache to amortise eviction cost
            if entries.len() >= MAX_ENTRIES {
                // ponytail: half-clear is O(n/2) amortised over n/2 subsequent inserts = O(1) per insert
                entries.retain(|_, v| v.fetched_at.elapsed() < CACHE_TTL / 2);
                if entries.len() >= MAX_ENTRIES {
                    // All entries are fresh; drop half arbitrarily
                    let keep: std::collections::HashSet<String> =
                        entries.keys().take(MAX_ENTRIES / 2).cloned().collect();
                    entries.retain(|k, _| keep.contains(k));
                }
            }
            entries.insert(
                actor_uri.to_string(),
                CachedActor {
                    public_key_pem: public_key_pem.clone(),
                    key_id: key_id.clone(),
                    fetched_at: Instant::now(),
                },
            );
        }

        Ok((public_key_pem, key_id))
    }

    /// Invalidate a cached key (e.g., on signature verification failure for key rotation).
    pub async fn invalidate(&self, actor_uri: &str) {
        let mut entries = self.entries.lock().await;
        entries.remove(actor_uri);
    }
}
