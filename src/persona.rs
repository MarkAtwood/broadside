use anyhow::Context;
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::rand_core::OsRng;
use rsa::RsaPrivateKey;
use sqlx::SqlitePool;

use crate::id::gen_id;

/// Generate an RSA 2048 keypair, returning (private_pem, public_pem).
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

/// Create a new persona with a fresh RSA keypair.
pub async fn add(
    pool: &SqlitePool,
    username: &str,
    display_name: Option<&str>,
) -> anyhow::Result<()> {
    let (private_pem, public_pem) = generate_keypair()?;
    let id = gen_id();
    let display = display_name.unwrap_or(username);

    sqlx::query(
        "INSERT INTO personas (id, username, display_name, private_key, public_key) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(username)
    .bind(display)
    .bind(&private_pem)
    .bind(&public_pem)
    .execute(pool)
    .await
    .with_context(|| format!("inserting persona {username}"))?;

    println!("Created persona @{username} (id: {id})");
    Ok(())
}

/// List all personas with follower counts.
pub async fn list(pool: &SqlitePool) -> anyhow::Result<()> {
    let rows = sqlx::query_as::<_, (String, String, String, i64)>(
        "SELECT p.id, p.username, p.display_name, COUNT(f.id) as follower_count \
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

    // Verify persona exists first
    let (count,) = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM personas WHERE username = ?")
        .bind(username)
        .fetch_one(pool)
        .await?;
    if count == 0 {
        anyhow::bail!("persona @{username} not found");
    }

    if let Some(name) = display_name {
        sqlx::query("UPDATE personas SET display_name = ? WHERE username = ?")
            .bind(name)
            .bind(username)
            .execute(pool)
            .await?;
    }
    if let Some(bio_text) = bio {
        sqlx::query("UPDATE personas SET bio = ? WHERE username = ?")
            .bind(bio_text)
            .bind(username)
            .execute(pool)
            .await?;
    }
    if let Some(avatar_path) = avatar {
        sqlx::query("UPDATE personas SET avatar_path = ? WHERE username = ?")
            .bind(avatar_path)
            .bind(username)
            .execute(pool)
            .await?;
    }
    if let Some(header_path) = header {
        sqlx::query("UPDATE personas SET header_path = ? WHERE username = ?")
            .bind(header_path)
            .bind(username)
            .execute(pool)
            .await?;
    }

    println!("Updated persona @{username}");
    Ok(())
}

/// Look up a persona's private key PEM by username.
pub async fn get_private_key(pool: &SqlitePool, username: &str) -> anyhow::Result<String> {
    let (key,) =
        sqlx::query_as::<_, (String,)>("SELECT private_key FROM personas WHERE username = ?")
            .bind(username)
            .fetch_one(pool)
            .await
            .with_context(|| format!("persona @{username} not found"))?;
    Ok(key)
}

/// Look up a persona's public key PEM by username.
pub async fn get_public_key(pool: &SqlitePool, username: &str) -> anyhow::Result<String> {
    let (key,) =
        sqlx::query_as::<_, (String,)>("SELECT public_key FROM personas WHERE username = ?")
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
