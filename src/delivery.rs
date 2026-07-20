use anyhow::Context;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::signatures;

/// Wrap a raw SqlitePool in fieldwork's Pool enum for shared module calls.
fn fw_pool(pool: &SqlitePool) -> fieldwork::db::Pool {
    fieldwork::db::Pool::Sqlite(pool.clone())
}

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
    let now = chrono::Utc::now().timestamp();
    let fwp = fw_pool(pool);

    // Query follower inboxes (already deduplicated by shared_inbox preference)
    let inboxes = fieldwork::followers_db::follower_inboxes(&fwp, persona_id)
        .await
        .context("querying followers for fan-out")?;

    // ponytail: store post_id in activity_json as a broadside-specific stub.
    // The delivery worker expands this into the full Create activity at send time
    // (needs domain which isn't available here).
    let stub = serde_json::json!({"post_id": post_id}).to_string();

    let mut seen = HashSet::new();
    let mut queued = 0u64;

    for target in &inboxes {
        if !seen.insert(target.clone()) {
            continue;
        }

        fieldwork::delivery_db::enqueue(&fwp, target, persona_id, &stub, now).await?;
        queued += 1;
    }

    // Also deliver to all accepted relay inboxes
    let relays = fieldwork::relay::get_accepted(&fwp)
        .await
        .context("querying relays for fan-out")?;

    for relay in &relays {
        if !seen.insert(relay.inbox_url.clone()) {
            continue;
        }
        fieldwork::delivery_db::enqueue(&fwp, &relay.inbox_url, persona_id, &stub, now).await?;
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
    let fwp = fw_pool(pool);

    let jobs = fieldwork::delivery_db::fetch_pending(&fwp, 50, now).await?;

    let mut processed = 0u32;
    // Cache per-post: (serialized_body, private_key, actor_uri)
    // ponytail: body is .clone()'d per inbox send. Bodies are typically <10KB JSON;
    // memcpy cost is negligible vs the HTTP roundtrip (~100ms+). Upgrade path: Arc<Vec<u8>>.
    let mut post_cache: HashMap<String, (Vec<u8>, String, String)> = HashMap::new();

    for job in jobs {
        let delivery_id = job.id;
        let sender_persona_id = &job.sender_persona_id;
        let target_inbox = &job.target_inbox;
        let activity_json = &job.activity_json;
        let attempts = job.attempts;
        // Re-validate inbox URI at delivery time (defense against DNS rebinding)
        if !target_inbox.starts_with("https://") {
            fieldwork::delivery_db::mark_dead(&fwp, delivery_id, "target_inbox not https", now).await?;
            tracing::warn!(delivery_id, error = "target_inbox not https", "delivery dead-lettered");
            processed += 1;
            continue;
        }
        let inbox_domain = extract_domain(target_inbox);
        if crate::server::is_private_host_resolved(&inbox_domain).await {
            fieldwork::delivery_db::mark_dead(&fwp, delivery_id, "target_inbox resolves to private host", now).await?;
            tracing::warn!(delivery_id, error = "target_inbox resolves to private host", "delivery dead-lettered");
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
        let post_id = match serde_json::from_str::<serde_json::Value>(activity_json)
            .ok()
            .and_then(|v| v["post_id"].as_str().map(|s| s.to_string()))
        {
            Some(pid) => pid,
            None => {
                fieldwork::delivery_db::mark_dead(&fwp, delivery_id, "missing post_id in activity_json", now).await?;
                tracing::warn!(delivery_id, error = "missing post_id in activity_json", "delivery dead-lettered");
                processed += 1;
                continue;
            }
        };

        let post_id_int: i64 = match post_id.parse() {
            Ok(v) => v,
            Err(_) => {
                fieldwork::delivery_db::mark_dead(&fwp, delivery_id, "invalid post_id", now).await?;
                processed += 1;
                continue;
            }
        };
        let fw_post = fieldwork::posts_db::get_post(&fwp, post_id_int).await?;
        let (post_id, content_html, created_at_epoch, username) = match fw_post {
            Some(p) => {
                let persona = fieldwork::persona_db::get_persona_by_id(&fwp, &p.persona_id).await?;
                let uname = persona.map(|r| r.username).unwrap_or_default();
                (p.id.to_string(), p.content_html, p.created_at, uname)
            }
            None => {
                fieldwork::delivery_db::mark_dead(&fwp, delivery_id, "post not found", now).await?;
                tracing::warn!(delivery_id, error = "post not found", "delivery dead-lettered");
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
        let target_path = url::Url::parse(target_inbox)
            .map(|u| u.path().to_string())
            .unwrap_or_else(|_| "/inbox".to_string());

        let sig_headers =
            match signatures::sign_request(private_key, &key_id, &target_path, &inbox_domain, body)
            {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!(inbox = %target_inbox, error = %e, "failed to sign request");
                    fieldwork::delivery_db::mark_dead(&fwp, delivery_id, "signing failed", now).await?;
                    tracing::warn!(delivery_id, error = "signing failed", "delivery dead-lettered");
                    processed += 1;
                    continue;
                }
            };

        let result = client
            .post(target_inbox.as_str())
            .headers(sig_headers)
            .header("Content-Type", "application/activity+json")
            .body(body.clone())
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                let delivered_at = chrono::Utc::now().timestamp();
                fieldwork::delivery_db::mark_delivered(&fwp, delivery_id, delivered_at).await?;
                breaker.lock().await.record_success(&inbox_domain);
                tracing::debug!(inbox = %target_inbox, "delivered");
            }
            Ok(resp) if resp.status().as_u16() == 410 => {
                // 410 Gone — mark delivered, then remove followers pointing to this inbox
                let delivered_at = chrono::Utc::now().timestamp();
                fieldwork::delivery_db::mark_delivered(&fwp, delivery_id, delivered_at).await?;

                let removed = crate::db_extras::remove_followers_by_inbox(
                    pool, target_inbox, sender_persona_id,
                )
                .await?;

                tracing::info!(
                    inbox = %target_inbox,
                    removed,
                    "410 Gone — removed followers"
                );
            }
            Ok(resp) => {
                let status = resp.status();
                let err_msg = format!("HTTP {status}");
                let retry_now = chrono::Utc::now().timestamp();
                fieldwork::delivery_db::schedule_retry(&fwp, delivery_id, &err_msg, retry_now).await?;
                tracing::warn!(delivery_id, attempt = attempts + 1, error = %err_msg, "delivery failed, will retry");
                breaker.lock().await.record_failure(&inbox_domain);
            }
            Err(e) => {
                let err_msg = e.to_string();
                let retry_now = chrono::Utc::now().timestamp();
                fieldwork::delivery_db::schedule_retry(&fwp, delivery_id, &err_msg, retry_now).await?;
                tracing::warn!(delivery_id, attempt = attempts + 1, error = %err_msg, "delivery failed, will retry");
                breaker.lock().await.record_failure(&inbox_domain);
            }
        }

        processed += 1;
    }

    Ok(processed)
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
    let fwp = fw_pool(pool);
    // Fetch pending jobs with a far-future timestamp to get all pending items
    let pending_jobs = fieldwork::delivery_db::fetch_pending(&fwp, 50, i64::MAX).await?;

    let dead = crate::db_extras::delivery_list_dead(pool).await?;

    if pending_jobs.is_empty() && dead.is_empty() {
        println!("Queue is empty.");
        return Ok(());
    }

    if !pending_jobs.is_empty() {
        println!("Pending ({}):", pending_jobs.len());
        for job in &pending_jobs {
            let next_str = chrono::DateTime::from_timestamp(job.next_attempt_at, 0)
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_else(|| format!("{}", job.next_attempt_at));
            println!("  {}  → {}  attempts={}  next={next_str}", job.id, job.target_inbox, job.attempts);
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
    let count = crate::db_extras::delivery_retry_all_dead(pool, now).await?;
    println!("Retrying {count} dead-lettered deliveries.");
    Ok(())
}

/// CLI: delivery stats.
pub async fn stats(pool: &SqlitePool) -> anyhow::Result<()> {
    let pending = crate::db_extras::delivery_count_pending(pool).await?;
    let dead = crate::db_extras::delivery_count_dead(pool).await?;

    println!("Pending: {pending}");
    println!("Dead:    {dead}");
    Ok(())
}
