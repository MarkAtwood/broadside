use anyhow::Context;
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::rand_core::OsRng;
use rsa::RsaPrivateKey;
use sqlx::SqlitePool;

use crate::id::gen_id;

/// Get the single operator user_id. Broadside is single-user; this returns the first user.
pub async fn get_operator_user_id(pool: &SqlitePool) -> anyhow::Result<String> {
    let (id,) = sqlx::query_as::<_, (String,)>("SELECT id FROM users LIMIT 1")
        .fetch_one(pool)
        .await
        .context("no operator user found — database may not be initialized")?;
    Ok(id)
}

/// Generate an RSA 2048 keypair, returning (private_pem, public_pem).
// ponytail: 2048 matches Mastodon's choice; 4096 would double signing time for marginal benefit
// in this context.
fn generate_keypair() -> anyhow::Result<(String, String)> {
    let private_key =
        RsaPrivateKey::new(&mut OsRng, 2048).context("generating RSA 2048 keypair")?;
    let private_pem = private_key
        .to_pkcs8_pem(LineEnding::LF)
        .context("encoding private key to PEM")?;
    let public_pem = private_key
        .to_public_key()
        .to_public_key_pem(LineEnding::LF)
        .context("encoding public key to PEM")?;
    // private_pem is Zeroizing<String> — we must convert to String for SQLite storage.
    // The Zeroizing wrapper zeros the original on drop after this conversion.
    Ok((private_pem.to_string(), public_pem))
}

/// Create a new persona with a fresh RSA keypair and Ed25519 recovery keypair.
pub async fn add(
    pool: &SqlitePool,
    username: &str,
    display_name: Option<&str>,
) -> anyhow::Result<()> {
    if !crate::server::is_valid_username(username) {
        anyhow::bail!(
            "invalid username '{username}': must be 1-64 chars, ASCII alphanumeric, underscore, or hyphen"
        );
    }
    let (private_pem, public_pem) = generate_keypair()?;
    let id = gen_id();
    let display = display_name.unwrap_or(username);
    let now = chrono::Utc::now().timestamp();
    let user_id = get_operator_user_id(pool).await?;

    // Generate Ed25519 recovery keypair for DID identity
    let (recovery_private, recovery_public) = crate::did::generate_recovery_keypair();
    let did_key = crate::did::ed25519_to_did_key(&recovery_public);
    let recovery_pubkey_hex = crate::did::hex_encode(&recovery_public);
    let recovery_phrase = crate::did::private_key_to_mnemonic(&recovery_private);
    // recovery_private is Zeroizing<[u8; 32]> — auto-zeroized on drop

    sqlx::query(
        "INSERT INTO personas (id, user_id, username, display_name, private_key_pem, public_key_pem, did_key, recovery_pubkey, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&user_id)
    .bind(username)
    .bind(display)
    .bind(&private_pem)
    .bind(&public_pem)
    .bind(&did_key)
    .bind(&recovery_pubkey_hex)
    .bind(now)
    .execute(pool)
    .await
    .with_context(|| format!("inserting persona {username}"))?;

    println!("Created persona @{username} (id: {id})");
    eprintln!("DID: {did_key}");
    eprintln!();
    eprintln!("RECOVERY PHRASE (save this — it will not be shown again):");
    eprintln!("{recovery_phrase}");
    Ok(())
}

/// List all personas with follower counts.
pub async fn list(pool: &SqlitePool) -> anyhow::Result<()> {
    let rows = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT p.id, p.username, p.display_name, COUNT(f.remote_account_id) as follower_count \
         FROM personas p LEFT JOIN followers f ON f.persona_id = p.id \
         GROUP BY p.id ORDER BY p.username",
    )
    .fetch_all(pool)
    .await
    .context("listing personas")?;

    if rows.is_empty() {
        println!("No personas configured.");
        return Ok(());
    }

    for (id, username, display_name, followers) in &rows {
        println!("@{username} ({display_name}) — {followers} followers [id: {id}]");
    }
    Ok(())
}

/// Update a persona's display name, bio, avatar, or header.
pub async fn update(
    pool: &SqlitePool,
    username: &str,
    display_name: Option<&str>,
    bio: Option<&str>,
    avatar: Option<&str>,
    header: Option<&str>,
) -> anyhow::Result<()> {
    if display_name.is_none() && bio.is_none() && avatar.is_none() && header.is_none() {
        anyhow::bail!("nothing to update — specify --display-name, --bio, --avatar, or --header");
    }

    let mut fields: Vec<(&str, &str)> = Vec::new();
    if let Some(v) = display_name {
        fields.push(("display_name = ?", v));
    }
    if let Some(v) = bio {
        fields.push(("bio = ?", v));
    }
    if let Some(v) = avatar {
        fields.push(("avatar_media_id = ?", v));
    }
    if let Some(v) = header {
        fields.push(("header_media_id = ?", v));
    }

    let set_clause: Vec<&str> = fields.iter().map(|(clause, _)| *clause).collect();
    let sql = format!(
        "UPDATE personas SET {} WHERE username = ?",
        set_clause.join(", ")
    );
    let mut q = sqlx::query(&sql);
    for (_, value) in &fields {
        q = q.bind(*value);
    }
    q = q.bind(username);
    let result = q.execute(pool).await?;

    if result.rows_affected() == 0 {
        anyhow::bail!("persona @{username} not found");
    }

    println!("Updated persona @{username}");
    Ok(())
}

/// Look up a persona's private key PEM by username.
pub async fn get_private_key(pool: &SqlitePool, username: &str) -> anyhow::Result<String> {
    let (key,) =
        sqlx::query_as::<_, (String,)>("SELECT private_key_pem FROM personas WHERE username = ?")
            .bind(username)
            .fetch_one(pool)
            .await
            .with_context(|| format!("persona @{username} not found"))?;
    Ok(key)
}

/// Look up a persona's public key PEM by username.
pub async fn get_public_key(pool: &SqlitePool, username: &str) -> anyhow::Result<String> {
    let (key,) =
        sqlx::query_as::<_, (String,)>("SELECT public_key_pem FROM personas WHERE username = ?")
            .bind(username)
            .fetch_one(pool)
            .await
            .with_context(|| format!("persona @{username} not found"))?;
    Ok(key)
}

/// Look up a persona's ID by username.
pub async fn get_id(pool: &SqlitePool, username: &str) -> anyhow::Result<String> {
    let (id,) = sqlx::query_as::<_, (String,)>("SELECT id FROM personas WHERE username = ?")
        .bind(username)
        .fetch_one(pool)
        .await
        .with_context(|| format!("persona @{username} not found"))?;
    Ok(id)
}
