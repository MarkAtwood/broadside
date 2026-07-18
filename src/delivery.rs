use anyhow::Context;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::id::gen_id;
use crate::signatures;

/// Retry delays by attempt number (0-indexed after the first immediate try).
const RETRY_DELAYS: &[Duration] = &[
    Duration::from_secs(60),    // attempt 2
    Duration::from_secs(300),   // attempt 3
    Duration::from_secs(1800),  // attempt 4
    Duration::from_secs(7200),  // attempt 5
    Duration::from_secs(28800), // attempt 6
];
const MAX_ATTEMPTS: u32 = 7;

/// Per-domain circuit breaker state.
struct CircuitBreaker {
    failures: HashMap<String, (u32, Instant)>,
}

impl CircuitBreaker {
    fn new() -> Self {
        Self {
            failures: HashMap::new(),
        }
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
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

/// Fan out a post to all followers' inboxes.
pub async fn fan_out(pool: &SqlitePool, post_id: &str, persona_id: &str) -> anyhow::Result<u64> {
    let rows = sqlx::query_as::<_, (String, Option<String>)>(
        "SELECT inbox_uri, shared_inbox_uri FROM followers WHERE persona_id = ?",
    )
    .bind(persona_id)
    .fetch_all(pool)
    .await
    .context("querying followers for fan-out")?;

    let mut seen = HashSet::new();
    let mut queued = 0u64;

    for (inbox_uri, shared_inbox_uri) in &rows {
        let target = shared_inbox_uri.as_deref().unwrap_or(inbox_uri.as_str());
        if !seen.insert(target.to_string()) {
            continue;
        }

        let id = gen_id();
        sqlx::query("INSERT INTO delivery_queue (id, post_id, inbox_uri) VALUES (?, ?, ?)")
            .bind(&id)
            .bind(post_id)
            .bind(target)
            .execute(pool)
            .await?;
        queued += 1;
    }

    tracing::info!(post_id, queued, "fan-out complete");
    Ok(queued)
}

/// Background delivery worker. Runs as a tokio task.
pub async fn run_worker(pool: SqlitePool, domain: String) {
    let breaker = Arc::new(Mutex::new(CircuitBreaker::new()));
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
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let rows = sqlx::query_as::<_, (String, String, String, u32)>(
        "SELECT dq.id, dq.post_id, dq.inbox_uri, dq.attempts \
         FROM delivery_queue dq \
         WHERE dq.status = 'pending' AND dq.next_retry <= ? \
         ORDER BY dq.next_retry ASC LIMIT 50",
    )
    .bind(&now)
    .fetch_all(pool)
    .await?;

    let mut processed = 0u32;

    for (delivery_id, post_id, inbox_uri, attempts) in rows {
        // Re-validate inbox URI at delivery time (defense against DNS rebinding)
        if !inbox_uri.starts_with("https://") {
            mark_dead(pool, &delivery_id, "inbox_uri not https").await?;
            processed += 1;
            continue;
        }
        let inbox_domain = extract_domain(&inbox_uri);
        if crate::server::is_private_host_resolved(&inbox_domain).await {
            mark_dead(pool, &delivery_id, "inbox_uri resolves to private host").await?;
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

        let post = sqlx::query_as::<_, (String, String, String)>(
            "SELECT p.content_html, p.published_at, pe.username \
             FROM posts p JOIN personas pe ON pe.id = p.persona_id \
             WHERE p.id = ?",
        )
        .bind(&post_id)
        .fetch_optional(pool)
        .await?;

        let (content_html, published_at, username) = match post {
            Some(p) => p,
            None => {
                mark_dead(pool, &delivery_id, "post not found").await?;
                processed += 1;
                continue;
            }
        };

        let actor_uri = format!("https://{domain}/users/{username}");
        let post_uri = format!("{actor_uri}/statuses/{post_id}");

        // Process content for hashtags, mentions, and URL auto-linking
        let (processed_html, tags) = crate::content::process_content(&content_html, domain);
        let tag_json: Vec<serde_json::Value> = tags
            .iter()
            .map(|t| serde_json::to_value(t).unwrap_or_default())
            .collect();

        let attachments = crate::media::attachments_for_post(pool, &post_id, domain).await;

        let activity = serde_json::json!({
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

        let body = serde_json::to_vec(&activity)?;

        let private_key = crate::persona::get_private_key(pool, &username).await?;
        let key_id = format!("{actor_uri}#main-key");
        let target_path = url::Url::parse(&inbox_uri)
            .map(|u| u.path().to_string())
            .unwrap_or_else(|_| "/inbox".to_string());

        let sig_headers = match signatures::sign_request(
            &private_key,
            &key_id,
            &target_path,
            &inbox_domain,
            &body,
        ) {
            Ok(h) => h,
            Err(_e) => {
                tracing::error!("failed to sign request for {}", inbox_uri);
                mark_dead(pool, &delivery_id, "signing failed").await?;
                processed += 1;
                continue;
            }
        };

        let result = client
            .post(&inbox_uri)
            .headers(sig_headers)
            .header("Content-Type", "application/activity+json")
            .body(body)
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() || resp.status().as_u16() == 202 => {
                sqlx::query("DELETE FROM delivery_queue WHERE id = ?")
                    .bind(&delivery_id)
                    .execute(pool)
                    .await?;
                breaker.lock().await.record_success(&inbox_domain);
                tracing::debug!(inbox = inbox_uri, "delivered");
            }
            Ok(resp) if resp.status().as_u16() == 410 => {
                // Get the persona_id from the post so we only delete that persona's followers
                let persona_id =
                    sqlx::query_as::<_, (String,)>("SELECT persona_id FROM posts WHERE id = ?")
                        .bind(&post_id)
                        .fetch_optional(pool)
                        .await?
                        .map(|(pid,)| pid);

                sqlx::query("DELETE FROM delivery_queue WHERE id = ?")
                    .bind(&delivery_id)
                    .execute(pool)
                    .await?;

                let removed = if let Some(pid) = &persona_id {
                    sqlx::query(
                        "DELETE FROM followers WHERE persona_id = ? AND (inbox_uri = ? OR shared_inbox_uri = ?)",
                    )
                    .bind(pid)
                    .bind(&inbox_uri)
                    .bind(&inbox_uri)
                    .execute(pool)
                    .await?
                } else {
                    sqlx::query("DELETE FROM followers WHERE inbox_uri = ? OR shared_inbox_uri = ?")
                        .bind(&inbox_uri)
                        .bind(&inbox_uri)
                        .execute(pool)
                        .await?
                };
                tracing::info!(
                    inbox = inbox_uri,
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
    attempts: u32,
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
    let next_retry = chrono::Utc::now()
        + chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::hours(8));
    let next_retry_str = next_retry.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    sqlx::query(
        "UPDATE delivery_queue SET attempts = ?, next_retry = ?, last_error = ? WHERE id = ?",
    )
    .bind(next_attempt as i32)
    .bind(&next_retry_str)
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
    sqlx::query("UPDATE delivery_queue SET status = 'dead', last_error = ? WHERE id = ?")
        .bind(error)
        .bind(delivery_id)
        .execute(pool)
        .await?;
    tracing::warn!(delivery_id, error, "delivery dead-lettered");
    Ok(())
}

fn extract_domain(uri: &str) -> String {
    url::Url::parse(uri)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default()
}

/// CLI: inspect the delivery queue.
pub async fn inspect(pool: &SqlitePool) -> anyhow::Result<()> {
    let pending = sqlx::query_as::<_, (String, String, i32, String)>(
        "SELECT id, inbox_uri, attempts, next_retry \
         FROM delivery_queue WHERE status = 'pending' ORDER BY next_retry LIMIT 50",
    )
    .fetch_all(pool)
    .await?;

    let dead = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT id, inbox_uri, last_error \
         FROM delivery_queue WHERE status = 'dead' ORDER BY id LIMIT 50",
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
            println!("  {id}  → {inbox}  attempts={attempts}  next={next}");
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
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let result = sqlx::query(
        "UPDATE delivery_queue SET status = 'pending', attempts = 0, next_retry = ? WHERE status = 'dead'",
    )
    .bind(&now)
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
    let (pending,) =
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM delivery_queue WHERE status = 'pending'")
            .fetch_one(pool)
            .await?;
    let (dead,) =
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM delivery_queue WHERE status = 'dead'")
            .fetch_one(pool)
            .await?;

    println!("Pending: {pending}");
    println!("Dead:    {dead}");
    Ok(())
}
