use anyhow::{bail, Context};
use sqlx::SqlitePool;
use std::sync::Arc;

use crate::server::SsrfSafeResolver;
use crate::signatures;

/// Wrap a raw SqlitePool in fieldwork's Pool enum for shared module calls.
fn fw_pool(pool: &SqlitePool) -> fieldwork::db::Pool {
    fieldwork::db::Pool::Sqlite(pool.clone())
}

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
    let fwp = fw_pool(pool);
    if let Some(existing) = fieldwork::relay::find_by_actor(&fwp, relay_url).await? {
        bail!("relay already registered (state: {})", existing.state);
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

    let inbox_url = actor["inbox"]
        .as_str()
        .context("relay actor has no inbox field")?
        .to_string();

    if !inbox_url.starts_with("https://") {
        bail!("relay inbox must be https");
    }

    // SSRF guard: reject inbox URIs pointing to private/internal hosts
    let inbox_host = url::Url::parse(&inbox_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()));
    match inbox_host {
        Some(ref h) if crate::server::is_private_host_resolved(h).await => {
            bail!("relay inbox URI resolves to private/internal host");
        }
        None => bail!("relay inbox URI has no valid host"),
        _ => {}
    }

    // Resolve persona username to persona_id for the FK
    let persona_id = crate::persona::get_id(pool, persona).await?;

    // Build the follow_id URI before inserting (needed for the Follow activity)
    let actor_uri = format!("https://{domain}/users/{persona}");
    // Use a temporary ID for the follow_id URI; subscribe() generates the actual row ID
    let temp_follow_id = format!("{actor_uri}/relay-follow/pending");

    // Store the relay subscription
    let id = fieldwork::relay::subscribe(&fwp, relay_url, &inbox_url, Some(&persona_id), &temp_follow_id)
        .await
        .context("storing relay subscription")?;

    // Remaining SQL: follow_id update has no fieldwork equivalent.
    // fieldwork::relay provides subscribe/accept/reject/unsubscribe but not field-level updates.
    let follow_id = format!("{actor_uri}/relay-follow/{id}");
    sqlx::query("UPDATE relays SET follow_id = ? WHERE id = ?")
        .bind(&follow_id)
        .bind(id)
        .execute(pool)
        .await
        .context("updating relay follow_id")?;
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
    let parsed_inbox = url::Url::parse(&inbox_url);
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
        .post(&inbox_url)
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
pub async fn remove(
    pool: &SqlitePool,
    relay_url: &str,
    domain: &str,
    persona_override: Option<&str>,
) -> anyhow::Result<()> {
    let fwp = fw_pool(pool);
    let relay_row = fieldwork::relay::find_by_actor(&fwp, relay_url)
        .await?
        .context(format!("relay not found: {relay_url}"))?;

    let relay_id = relay_row.id;
    let inbox_url = relay_row.inbox_url;

    // Resolve persona: use override username, or look up stored persona_id's username
    let persona_username = if let Some(p) = persona_override {
        p.to_string()
    } else if let Some(ref pid) = relay_row.persona_id {
        let persona = fieldwork::persona_db::get_persona_by_id(&fwp, pid)
            .await
            .context("looking up persona for relay")?
            .context("persona not found for relay")?;
        persona.username
    } else {
        bail!("no persona stored for this relay; pass --persona explicitly");
    };

    // Send Undo{Follow} to the relay
    let actor_uri = format!("https://{domain}/users/{persona_username}");
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
    let private_key = crate::persona::get_private_key(pool, &persona_username).await?;
    let key_id = format!("{actor_uri}#main-key");
    let parsed_inbox = url::Url::parse(&inbox_url);
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
        .post(&inbox_url)
        .headers(sig_headers)
        .header("Content-Type", "application/activity+json")
        .body(body_bytes)
        .send()
        .await;

    // Delete regardless of Undo delivery success
    fieldwork::relay::unsubscribe(&fwp, relay_id).await?;

    println!("Removed relay: {relay_url}");
    Ok(())
}

/// List all relay subscriptions.
pub async fn list(pool: &SqlitePool) -> anyhow::Result<()> {
    // Remaining SQL: list all relays (any state) has no fieldwork equivalent.
    // fieldwork::relay provides get_accepted (accepted only) and find_by_actor (single lookup).
    let rows = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT actor_uri, inbox_url, state, created_at FROM relays ORDER BY created_at",
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        println!("No relay subscriptions.");
        return Ok(());
    }

    println!("Relay subscriptions ({}):", rows.len());
    for (actor_uri, inbox_url, state, created_at) in &rows {
        let marker = match state.as_str() {
            "accepted" => "✓",
            "pending" => "◐",
            "rejected" => "✗",
            _ => "?",
        };
        let created_str = chrono::DateTime::from_timestamp(*created_at, 0)
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| format!("{created_at}"));
        println!("  {marker} {actor_uri}");
        println!("    inbox: {inbox_url}");
        println!("    state: {state}  since: {created_str}");
    }
    Ok(())
}

/// Activate a relay subscription (called when we receive an Accept from the relay).
pub async fn activate(pool: &SqlitePool, relay_actor_uri: &str) -> anyhow::Result<bool> {
    let fwp = fw_pool(pool);
    let relay = match fieldwork::relay::find_by_actor(&fwp, relay_actor_uri).await? {
        Some(r) if r.state == fieldwork::relay::RelayState::Pending => r,
        _ => return Ok(false),
    };
    fieldwork::relay::accept(&fwp, relay.id).await?;
    Ok(true)
}
