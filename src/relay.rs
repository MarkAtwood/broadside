use anyhow::{bail, Context};
use sqlx::SqlitePool;
use std::sync::Arc;

use crate::id::gen_id;
use crate::server::SsrfSafeResolver;
use crate::signatures;

/// Add a relay subscription. Fetches the relay actor to discover its inbox,
/// then sends a Follow activity and stores the subscription as pending.
pub async fn add(
    pool: &SqlitePool,
    relay_url: &str,
    domain: &str,
    persona: &str,
) -> anyhow::Result<()> {
    if !relay_url.starts_with("https://") {
        bail!("relay URL must use https");
    }

    // SSRF guard: reject relay URLs pointing to private/internal hosts
    let relay_host = url::Url::parse(relay_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()));
    match relay_host {
        Some(ref h) if crate::server::is_private_host_resolved(h).await => {
            bail!("relay URL resolves to private/internal host");
        }
        None => bail!("relay URL has no valid host"),
        _ => {}
    }

    // Check if already subscribed
    let existing = sqlx::query_as::<_, (String,)>("SELECT status FROM relays WHERE actor_uri = ?")
        .bind(relay_url)
        .fetch_optional(pool)
        .await?;

    if let Some((status,)) = existing {
        bail!("relay already registered (status: {status})");
    }

    // ponytail: new Client per CLI invocation; CLI runs once and exits, no reuse benefit.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .dns_resolver(Arc::new(SsrfSafeResolver))
        .build()?;
    let resp = client
        .get(relay_url)
        .header("Accept", "application/activity+json")
        .send()
        .await
        .context("fetching relay actor")?;

    let body = crate::http::read_body_limited(resp, 65536)
        .await
        .context("reading relay actor document")?;

    let actor: serde_json::Value =
        serde_json::from_slice(&body).context("parsing relay actor document")?;

    let inbox_uri = actor["inbox"]
        .as_str()
        .context("relay actor has no inbox field")?
        .to_string();

    if !inbox_uri.starts_with("https://") {
        bail!("relay inbox must be https");
    }

    // SSRF guard: reject inbox URIs pointing to private/internal hosts
    let inbox_host = url::Url::parse(&inbox_uri)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()));
    match inbox_host {
        Some(ref h) if crate::server::is_private_host_resolved(h).await => {
            bail!("relay inbox URI resolves to private/internal host");
        }
        None => bail!("relay inbox URI has no valid host"),
        _ => {}
    }

    // Store the relay subscription (including persona used for signing)
    let id = gen_id();
    sqlx::query(
        "INSERT INTO relays (id, actor_uri, inbox_uri, status, persona) VALUES (?, ?, ?, 'pending', ?)",
    )
    .bind(&id)
    .bind(relay_url)
    .bind(&inbox_uri)
    .bind(persona)
    .execute(pool)
    .await
    .context("storing relay subscription")?;

    // Send Follow activity to the relay
    let actor_uri = format!("https://{domain}/users/{persona}");
    let follow_id = format!("{actor_uri}/relay-follow/{id}");
    let activity = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": follow_id,
        "type": "Follow",
        "actor": actor_uri,
        "object": relay_url,
    });

    let body_bytes = serde_json::to_vec(&activity)?;
    let private_key = crate::persona::get_private_key(pool, persona).await?;
    let key_id = format!("{actor_uri}#main-key");
    let parsed_inbox = url::Url::parse(&inbox_uri);
    let inbox_domain = parsed_inbox
        .as_ref()
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default();
    let inbox_path = parsed_inbox
        .as_ref()
        .map(|u| u.path().to_string())
        .unwrap_or_else(|_| "/inbox".to_string());

    let sig_headers = signatures::sign_request(
        &private_key,
        &key_id,
        &inbox_path,
        &inbox_domain,
        &body_bytes,
    )?;

    let resp = client
        .post(&inbox_uri)
        .headers(sig_headers)
        .header("Content-Type", "application/activity+json")
        .body(body_bytes)
        .send()
        .await
        .context("sending Follow to relay")?;

    let status = resp.status();
    if status.is_success() || status.as_u16() == 202 {
        println!("Follow sent to {relay_url} (status: {status})");
        println!("Subscription pending — will activate when relay sends Accept.");
    } else {
        let body = resp.text().await.unwrap_or_default();
        bail!("relay returned {status}: {body}");
    }

    Ok(())
}

/// Remove a relay subscription. Sends Undo{Follow} and deletes the record.
/// Uses the persona stored at subscription time; falls back to `persona_override` if provided.
pub async fn remove(
    pool: &SqlitePool,
    relay_url: &str,
    domain: &str,
    persona_override: Option<&str>,
) -> anyhow::Result<()> {
    let row = sqlx::query_as::<_, (String, String, String)>(
        "SELECT id, inbox_uri, persona FROM relays WHERE actor_uri = ?",
    )
    .bind(relay_url)
    .fetch_optional(pool)
    .await?;

    let (relay_id, inbox_uri, stored_persona) = match row {
        Some(r) => r,
        None => bail!("relay not found: {relay_url}"),
    };

    let persona = match persona_override {
        Some(p) => p.to_string(),
        None if !stored_persona.is_empty() => stored_persona,
        _ => bail!("no persona stored for this relay; pass --persona explicitly"),
    };

    // Send Undo{Follow} to the relay
    let actor_uri = format!("https://{domain}/users/{persona}");
    let follow_id = format!("{actor_uri}/relay-follow/{relay_id}");
    let activity = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{follow_id}/undo"),
        "type": "Undo",
        "actor": &actor_uri,
        "object": {
            "id": follow_id,
            "type": "Follow",
            "actor": &actor_uri,
            "object": relay_url,
        }
    });

    let body_bytes = serde_json::to_vec(&activity)?;
    let private_key = crate::persona::get_private_key(pool, &persona).await?;
    let key_id = format!("{actor_uri}#main-key");
    let parsed_inbox = url::Url::parse(&inbox_uri);
    let inbox_domain = parsed_inbox
        .as_ref()
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default();
    let inbox_path = parsed_inbox
        .as_ref()
        .map(|u| u.path().to_string())
        .unwrap_or_else(|_| "/inbox".to_string());

    let sig_headers = signatures::sign_request(
        &private_key,
        &key_id,
        &inbox_path,
        &inbox_domain,
        &body_bytes,
    )?;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .dns_resolver(Arc::new(SsrfSafeResolver))
        .build()?;
    let _ = client
        .post(&inbox_uri)
        .headers(sig_headers)
        .header("Content-Type", "application/activity+json")
        .body(body_bytes)
        .send()
        .await;

    // Delete regardless of Undo delivery success
    sqlx::query("DELETE FROM relays WHERE id = ?")
        .bind(&relay_id)
        .execute(pool)
        .await?;

    println!("Removed relay: {relay_url}");
    Ok(())
}

/// List all relay subscriptions.
pub async fn list(pool: &SqlitePool) -> anyhow::Result<()> {
    let rows = sqlx::query_as::<_, (String, String, String, String)>(
        "SELECT actor_uri, inbox_uri, status, created_at FROM relays ORDER BY created_at",
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        println!("No relay subscriptions.");
        return Ok(());
    }

    println!("Relay subscriptions ({}):", rows.len());
    for (actor_uri, inbox_uri, status, created_at) in &rows {
        let marker = match status.as_str() {
            "active" => "✓",
            "pending" => "◐",
            "rejected" => "✗",
            _ => "?",
        };
        println!("  {marker} {actor_uri}");
        println!("    inbox: {inbox_uri}");
        println!("    status: {status}  since: {created_at}");
    }
    Ok(())
}

/// Activate a relay subscription (called when we receive an Accept from the relay).
pub async fn activate(pool: &SqlitePool, relay_actor_uri: &str) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE relays SET status = 'active' WHERE actor_uri = ? AND status = 'pending'",
    )
    .bind(relay_actor_uri)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() > 0)
}
