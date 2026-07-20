use anyhow::Context;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::id::gen_int_id;
use crate::signatures;

/// Retry delays by attempt number (0-indexed after the first immediate try).
const RETRY_DELAYS: &[Duration] = &[
    Duration::from_secs(60),    // attempt 2
    Duration::from_secs(300),   // attempt 3
    Duration::from_secs(1800),  // attempt 4
    Duration::from_secs(7200),  // attempt 5
    Duration::from_secs(28800), // attempt 6
];
const MAX_ATTEMPTS: i32 = 7;

/// Per-domain circuit breaker state.
// ponytail: In-memory only — state is lost on restart. Acceptable for a single-process server:
// burst retries after restart are bounded by MAX_ATTEMPTS and exponential backoff anyway.
// Upgrade path: persist trip counts in SQLite if multi-process deployment is needed.
#[derive(Default)]
struct CircuitBreaker {
    failures: HashMap<String, (u32, Instant)>,
}

impl CircuitBreaker {
    fn record_failure(&mut self, domain: &str) {
        let entry = self
            .failures
            .entry(domain.to_string())
            .or_insert((0, Instant::now()));
        entry.0 += 1;
        entry.1 = Instant::now();
    }

    fn record_success(&mut self, domain: &str) {
        self.failures.remove(domain);
    }

    /// Returns true if the domain is tripped (10+ consecutive failures within the last hour).
    /// After the 1-hour cooldown, resets the counter so the domain gets a fair retry window.
    fn is_tripped(&mut self, domain: &str) -> bool {
        if let Some((count, last)) = self.failures.get(domain) {
            if *count >= 10 {
                if last.elapsed() < Duration::from_secs(3600) {
                    return true;
                }
                // Cooldown expired — reset so domain gets a fresh window
                self.failures.remove(domain);
            }
        }
        false
    }
}

/// Fan out a post to all followers' inboxes and active relay inboxes.
///
/// Stores a broadside-specific JSON stub `{"post_id":"..."}` in activity_json.
/// The delivery worker expands this into a full Create activity at send time.
pub async fn fan_out(pool: &SqlitePool, post_id: &str, persona_id: &str) -> anyhow::Result<u64> {
    // Query follower inboxes via remote_accounts JOIN
    let rows = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT ra.inbox_url, ra.shared_inbox_url \
         FROM followers f \
         JOIN remote_accounts ra ON ra.id = f.remote_account_id \
         WHERE f.persona_id = ?",
    )
    .bind(persona_id)
    .fetch_all(pool)
    .await
    .context("querying followers for fan-out")?;

    let now = chrono::Utc::now().timestamp();
    // ponytail: store post_id in activity_json as a broadside-specific stub.
    // The delivery worker expands this into the full Create activity at send time
    // (needs domain which isn't available here).
    let stub = serde_json::json!({"post_id": post_id}).to_string();

    // ponytail: .to_string() calls below allocate into HashSet<String> for dedup ownership;
    // unavoidable since `target` is a borrow from the loop iteration.
    let mut seen = HashSet::new();
    let mut queued = 0u64;

    for (inbox_url, shared_inbox_url) in &rows {
        let target = shared_inbox_url.as_deref().unwrap_or(inbox_url.as_str());
        if !seen.insert(target.to_string()) {
            continue;
        }

        let id = gen_int_id();
        sqlx::query(
            "INSERT INTO delivery_queue (id, target_inbox, sender_persona_id, activity_json, next_attempt_at, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(target)
        .bind(persona_id)
        .bind(&stub)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await?;
        queued += 1;
    }

    // Also deliver to all active relay inboxes
    let relays =
        sqlx::query_as::<_, (String,)>("SELECT inbox_url FROM relays WHERE state = 'active'")
            .fetch_all(pool)
            .await
            .context("querying relays for fan-out")?;

    for (relay_inbox,) in &relays {
        if !seen.insert(relay_inbox.clone()) {
            continue;
        }
        let id = gen_int_id();
        sqlx::query(
            "INSERT INTO delivery_queue (id, target_inbox, sender_persona_id, activity_json, next_attempt_at, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(relay_inbox)
        .bind(persona_id)
        .bind(&stub)
        .bind(now)
        .bind(now)
        .execute(pool)
        .await?;
        queued += 1;
    }

    tracing::info!(post_id, queued, "fan-out complete");
    Ok(queued)
}

/// Background delivery worker. Runs as a tokio task.
pub async fn run_worker(pool: SqlitePool, domain: String) {
    let breaker = Arc::new(Mutex::new(CircuitBreaker::default()));
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "failed to build HTTP client, delivery worker exiting");
            return;
        }
    };

    loop {
        match process_batch(&pool, &domain, &client, &breaker).await {
            Ok(0) => tokio::time::sleep(Duration::from_secs(5)).await,
            Ok(n) => tracing::debug!(processed = n, "delivery batch"),
            Err(e) => {
                tracing::error!(error = %e, "delivery worker error");
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

async fn process_batch(
    pool: &SqlitePool,
    domain: &str,
    client: &reqwest::Client,
    breaker: &Arc<Mutex<CircuitBreaker>>,
) -> anyhow::Result<u32> {
    let now = chrono::Utc::now().timestamp();

    // Canonical schema: pending = delivered_at IS NULL AND dead_at IS NULL
    let rows = sqlx::query_as::<_, (i64, String, String, String, i32)>(
        "SELECT dq.id, dq.sender_persona_id, dq.target_inbox, dq.activity_json, dq.attempts \
         FROM delivery_queue dq \
         WHERE dq.delivered_at IS NULL AND dq.dead_at IS NULL AND dq.next_attempt_at <= ? \
         ORDER BY dq.next_attempt_at ASC LIMIT 50",
    )
    .bind(now)
    .fetch_all(pool)
    .await?;

    let mut processed = 0u32;
    // Cache per-post: (serialized_body, private_key, actor_uri)
    // ponytail: body is .clone()'d per inbox send. Bodies are typically <10KB JSON;
    // memcpy cost is negligible vs the HTTP roundtrip (~100ms+). Upgrade path: Arc<Vec<u8>>.
    let mut post_cache: HashMap<String, (Vec<u8>, String, String)> = HashMap::new();

    for (delivery_id_int, sender_persona_id, target_inbox, activity_json, attempts) in rows {
        let delivery_id = delivery_id_int.to_string();
        // Re-validate inbox URI at delivery time (defense against DNS rebinding)
        if !target_inbox.starts_with("https://") {
            mark_dead(pool, &delivery_id, "target_inbox not https").await?;
            processed += 1;
            continue;
        }
        let inbox_domain = extract_domain(&target_inbox);
        if crate::server::is_private_host_resolved(&inbox_domain).await {
            mark_dead(pool, &delivery_id, "target_inbox resolves to private host").await?;
            processed += 1;
            continue;
        }

        {
            let mut br = breaker.lock().await;
            if br.is_tripped(&inbox_domain) {
                tracing::debug!(domain = inbox_domain, "circuit breaker tripped, skipping");
                continue;
            }
        }

        // Extract post_id from the activity_json stub ({"post_id":"..."})
        let post_id = match serde_json::from_str::<serde_json::Value>(&activity_json)
            .ok()
            .and_then(|v| v["post_id"].as_str().map(|s| s.to_string()))
        {
            Some(pid) => pid,
            None => {
                mark_dead(pool, &delivery_id, "missing post_id in activity_json").await?;
                processed += 1;
                continue;
            }
        };

        let post = sqlx::query_as::<_, (String, String, i64, String)>(
            "SELECT p.id, p.content_html, p.created_at, pe.username \
             FROM posts p JOIN personas pe ON pe.id = p.persona_id \
             WHERE p.id = ?",
        )
        .bind(&post_id)
        .fetch_optional(pool)
        .await?;

        let (post_id, content_html, created_at_epoch, username) = match post {
            Some(p) => p,
            None => {
                mark_dead(pool, &delivery_id, "post not found").await?;
                processed += 1;
                continue;
            }
        };

        // Build activity body, cache per post
        let cache_key = post_id.clone();
        if !post_cache.contains_key(&cache_key) {
            let published_at = chrono::DateTime::from_timestamp(created_at_epoch, 0)
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_else(|| format!("{created_at_epoch}"));

            let actor_uri = format!("https://{domain}/users/{username}");
            let post_uri = format!("{actor_uri}/statuses/{post_id}");
            let (processed_html, tags) = crate::content::process_content(&content_html, domain);
            let plain_text = crate::sanitize::html_to_text(&processed_html);
            let detected_lang = crate::content::detect_language(&plain_text);
            // ponytail: Tag is a simple flat struct — serialization is infallible.
            let mut tag_json: Vec<serde_json::Value> = tags
                .iter()
                .map(|t| serde_json::to_value(t).expect("Tag serialization is infallible"))
                .collect();
            // FEP-e232: add Link tags for any quoted AP post URLs
            let quote_links = crate::content::detect_quote_links(&processed_html);
            tag_json.extend(quote_links.iter().cloned());
            let attachments = crate::media::attachments_for_post(pool, &post_id, domain).await;
            let card = crate::card::get_card_for_post(pool, &post_id, domain).await;

            let mut activity = serde_json::json!({
                "@context": "https://www.w3.org/ns/activitystreams",
                "id": format!("{post_uri}/activity"),
                "type": "Create",
                "actor": &actor_uri,
                "published": published_at,
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
                "cc": [format!("{actor_uri}/followers")],
                "object": {
                    "id": post_uri,
                    "type": "Note",
                    "attributedTo": &actor_uri,
                    "content": processed_html,
                    "published": published_at,
                    "to": ["https://www.w3.org/ns/activitystreams#Public"],
                    "cc": [format!("{actor_uri}/followers")],
                    "tag": tag_json,
                    "attachment": attachments,
                }
            });

            // Only include contentMap when language was detected (avoid null field)
            if let Some(lang) = detected_lang {
                activity["object"]["contentMap"] = serde_json::json!({lang: &processed_html});
            }

            // FEP-e232: add quoteUrl for Misskey/Pleroma compat
            if let Some(first_quote) = quote_links.first() {
                if let Some(href) = first_quote["href"].as_str() {
                    activity["object"]["quoteUrl"] = serde_json::Value::String(href.to_string());
                }
            }

            // Add card as preview link if available
            if let Some(card_val) = card {
                activity["object"]["preview"] = card_val;
            }

            let body = serde_json::to_vec(&activity)?;
            let private_key = crate::persona::get_private_key(pool, &username).await?;
            post_cache.insert(cache_key.clone(), (body, private_key, actor_uri));
        }

        let (body, private_key, actor_uri) = post_cache.get(&cache_key).expect("just inserted");
        let key_id = format!("{actor_uri}#main-key");
        let target_path = url::Url::parse(&target_inbox)
            .map(|u| u.path().to_string())
            .unwrap_or_else(|_| "/inbox".to_string());

        let sig_headers =
            match signatures::sign_request(private_key, &key_id, &target_path, &inbox_domain, body)
            {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!(inbox = %target_inbox, error = %e, "failed to sign request");
                    mark_dead(pool, &delivery_id, "signing failed").await?;
                    processed += 1;
                    continue;
                }
            };

        let result = client
            .post(&target_inbox)
            .headers(sig_headers)
            .header("Content-Type", "application/activity+json")
            .body(body.clone())
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                let delivered_at = chrono::Utc::now().timestamp();
                sqlx::query("UPDATE delivery_queue SET delivered_at = ? WHERE id = ?")
                    .bind(delivered_at)
                    .bind(&delivery_id)
                    .execute(pool)
                    .await?;
                breaker.lock().await.record_success(&inbox_domain);
                tracing::debug!(inbox = target_inbox, "delivered");
            }
            Ok(resp) if resp.status().as_u16() == 410 => {
                // 410 Gone — remove followers pointing to this inbox
                let delivered_at = chrono::Utc::now().timestamp();
                sqlx::query("UPDATE delivery_queue SET delivered_at = ? WHERE id = ?")
                    .bind(delivered_at)
                    .bind(&delivery_id)
                    .execute(pool)
                    .await?;

                // Remove followers whose remote_account points at this inbox
                let removed = sqlx::query(
                    "DELETE FROM followers WHERE remote_account_id IN \
                     (SELECT id FROM remote_accounts WHERE inbox_url = ? OR shared_inbox_url = ?) \
                     AND persona_id = ?",
                )
                .bind(&target_inbox)
                .bind(&target_inbox)
                .bind(&sender_persona_id)
                .execute(pool)
                .await?;

                tracing::info!(
                    inbox = target_inbox,
                    removed = removed.rows_affected(),
                    "410 Gone — removed followers"
                );
            }
            Ok(resp) => {
                let status = resp.status();
                let err_msg = format!("HTTP {status}");
                handle_retry(pool, &delivery_id, attempts, &err_msg).await?;
                breaker.lock().await.record_failure(&inbox_domain);
            }
            Err(e) => {
                handle_retry(pool, &delivery_id, attempts, &e.to_string()).await?;
                breaker.lock().await.record_failure(&inbox_domain);
            }
        }

        processed += 1;
    }

    Ok(processed)
}

async fn handle_retry(
    pool: &SqlitePool,
    delivery_id: &str,
    attempts: i32,
    error: &str,
) -> anyhow::Result<()> {
    let next_attempt = attempts + 1;
    if next_attempt >= MAX_ATTEMPTS {
        mark_dead(pool, delivery_id, error).await?;
        return Ok(());
    }

    let delay = RETRY_DELAYS
        .get(attempts as usize)
        .copied()
        .unwrap_or(Duration::from_secs(28800));
    let next_attempt_at = chrono::Utc::now().timestamp() + delay.as_secs() as i64;

    sqlx::query(
        "UPDATE delivery_queue SET attempts = ?, next_attempt_at = ?, last_error = ? WHERE id = ?",
    )
    .bind(next_attempt)
    .bind(next_attempt_at)
    .bind(error)
    .bind(delivery_id)
    .execute(pool)
    .await?;

    tracing::warn!(
        delivery_id,
        attempt = next_attempt,
        error,
        "delivery failed, will retry"
    );
    Ok(())
}

async fn mark_dead(pool: &SqlitePool, delivery_id: &str, error: &str) -> anyhow::Result<()> {
    let dead_at = chrono::Utc::now().timestamp();
    sqlx::query("UPDATE delivery_queue SET dead_at = ?, last_error = ? WHERE id = ?")
        .bind(dead_at)
        .bind(error)
        .bind(delivery_id)
        .execute(pool)
        .await?;
    tracing::warn!(delivery_id, error, "delivery dead-lettered");
    Ok(())
}

// ponytail: Allocates one String per delivery call. Dwarfed by the HTTP request cost (~100ms+).
// Upgrade path: return Cow<str> or inline the URL parse at call sites.
fn extract_domain(uri: &str) -> String {
    url::Url::parse(uri)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default()
}

/// CLI: inspect the delivery queue.
pub async fn inspect(pool: &SqlitePool) -> anyhow::Result<()> {
    let pending = sqlx::query_as::<_, (String, String, i32, i64)>(
        "SELECT CAST(id AS TEXT), target_inbox, attempts, next_attempt_at \
         FROM delivery_queue WHERE delivered_at IS NULL AND dead_at IS NULL \
         ORDER BY next_attempt_at LIMIT 50",
    )
    .fetch_all(pool)
    .await?;

    let dead = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT CAST(id AS TEXT), target_inbox, last_error \
         FROM delivery_queue WHERE dead_at IS NOT NULL ORDER BY id LIMIT 50",
    )
    .fetch_all(pool)
    .await?;

    if pending.is_empty() && dead.is_empty() {
        println!("Queue is empty.");
        return Ok(());
    }

    if !pending.is_empty() {
        println!("Pending ({}):", pending.len());
        for (id, inbox, attempts, next) in &pending {
            let next_str = chrono::DateTime::from_timestamp(*next, 0)
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_else(|| format!("{next}"));
            println!("  {id}  → {inbox}  attempts={attempts}  next={next_str}");
        }
    }

    if !dead.is_empty() {
        println!("Dead-lettered ({}):", dead.len());
        for (id, inbox, error) in &dead {
            println!(
                "  {id}  → {inbox}  error={}",
                error.as_deref().unwrap_or("?")
            );
        }
    }

    Ok(())
}

/// CLI: retry all dead-lettered deliveries.
pub async fn retry_dead(pool: &SqlitePool) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp();
    let result = sqlx::query(
        "UPDATE delivery_queue SET dead_at = NULL, last_error = NULL, attempts = 0, next_attempt_at = ? \
         WHERE dead_at IS NOT NULL",
    )
    .bind(now)
    .execute(pool)
    .await?;
    println!(
        "Retrying {} dead-lettered deliveries.",
        result.rows_affected()
    );
    Ok(())
}

/// CLI: delivery stats.
pub async fn stats(pool: &SqlitePool) -> anyhow::Result<()> {
    let (pending,) = sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM delivery_queue WHERE delivered_at IS NULL AND dead_at IS NULL",
    )
    .fetch_one(pool)
    .await?;
    let (dead,) =
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM delivery_queue WHERE dead_at IS NOT NULL")
            .fetch_one(pool)
            .await?;

    println!("Pending: {pending}");
    println!("Dead:    {dead}");
    Ok(())
}
