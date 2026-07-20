use anyhow::Context;
use fieldwork_db::db::Pool;
use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
use rsa::rand_core::OsRng;
use rsa::RsaPrivateKey;

use crate::id::gen_int_id;

/// Get the single operator user_id. Broadside is single-user; this returns the first user.
pub async fn get_operator_user_id(pool: &Pool) -> anyhow::Result<i64> {

    let users = fieldwork_db::tenant_db::list_users(pool)
        .await
        .context("no operator user found — database may not be initialized")?;
    users
        .into_iter()
        .next()
        .map(|u| u.id)
        .context("no operator user found — database may not be initialized")
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
    // private_pem is Zeroizing<String> — we must convert to String for DB storage.
    // The Zeroizing wrapper zeros the original on drop after this conversion.
    Ok((private_pem.to_string(), public_pem))
}

/// Create a new persona with a fresh RSA keypair and Ed25519 recovery keypair.
pub async fn add(
    pool: &Pool,
    username: &str,
    display_name: Option<&str>,
) -> anyhow::Result<()> {
    if !crate::server::is_valid_username(username) {
        anyhow::bail!(
            "invalid username '{username}': must be 1-64 chars, ASCII alphanumeric, underscore, or hyphen"
        );
    }
    let (private_pem, public_pem) = generate_keypair()?;
    let id = gen_int_id();
    let display = display_name.unwrap_or(username);
    let now = chrono::Utc::now().timestamp();
    let user_id = get_operator_user_id(pool).await?;

    // Generate Ed25519 recovery keypair for DID identity
    let (recovery_private, recovery_public) = crate::did::generate_recovery_keypair();
    let did_key = crate::did::ed25519_to_did_key(&recovery_public);
    let recovery_pubkey_hex = crate::did::hex_encode(&recovery_public);
    let recovery_phrase = crate::did::private_key_to_mnemonic(&recovery_private);
    // recovery_private is Zeroizing<[u8; 32]> — auto-zeroized on drop


    let row = fieldwork_db::persona_db::PersonaRow {
        id,
        user_id,
        username: username.to_string(),
        display_name: display.to_string(),
        bio: String::new(),
        bio_html: String::new(),
        private_key_pem: private_pem,
        public_key_pem: public_pem,
        avatar_media_id: None,
        header_media_id: None,
        is_locked: false,
        discoverable: true,
        bot: false,
        did_web: Some(did_key.clone()),
        fields_json: "[]".to_string(),
        created_at: now,
        last_status_at: None,
    };
    fieldwork_db::persona_db::create_persona(pool, &row)
        .await
        .with_context(|| format!("inserting persona {username}"))?;

    crate::db_extras::set_persona_did(pool, id, &did_key, &recovery_pubkey_hex)
        .await
        .with_context(|| format!("setting DID for persona {username}"))?;

    println!("Created persona @{username} (id: {id})");
    eprintln!("DID: {did_key}");
    eprintln!();
    eprintln!("RECOVERY PHRASE (save this — it will not be shown again):");
    eprintln!("{recovery_phrase}");
    Ok(())
}

/// List all personas with follower counts.
pub async fn list(pool: &Pool) -> anyhow::Result<()> {

    let rows = fieldwork_db::persona_db::list_personas(pool)
        .await
        .context("listing personas")?;

    if rows.is_empty() {
        println!("No personas configured.");
        return Ok(());
    }

    for row in &rows {
        let followers = fieldwork_db::followers_db::follower_count(pool, row.id)
            .await
            .unwrap_or(0);
        println!(
            "@{} ({}) — {} followers [id: {}]",
            row.username, row.display_name, followers, row.id
        );
    }
    Ok(())
}

/// Update a persona's display name, bio, avatar, or header.
pub async fn update(
    pool: &Pool,
    username: &str,
    display_name: Option<&str>,
    bio: Option<&str>,
    avatar: Option<&str>,
    header: Option<&str>,
) -> anyhow::Result<()> {
    if display_name.is_none() && bio.is_none() && avatar.is_none() && header.is_none() {
        anyhow::bail!("nothing to update — specify --display-name, --bio, --avatar, or --header");
    }


    let persona_id = get_id(pool, username).await?;

    // Use fieldwork for display_name and bio
    if display_name.is_some() || bio.is_some() {
        fieldwork_db::persona_db::update_persona_profile(
            pool,
            persona_id,
            display_name,
            bio,
            None,
        )
        .await
        .with_context(|| format!("updating profile for @{username}"))?;
    }

    if avatar.is_some() || header.is_some() {
        crate::db_extras::update_persona_media(pool, persona_id, avatar, header).await?;
    }

    println!("Updated persona @{username}");
    Ok(())
}

/// Look up a persona's private key PEM by username.
pub async fn get_private_key(pool: &Pool, username: &str) -> anyhow::Result<String> {

    let row = fieldwork_db::persona_db::get_persona_by_username(pool, username)
        .await
        .with_context(|| format!("persona @{username} not found"))?
        .with_context(|| format!("persona @{username} not found"))?;
    Ok(row.private_key_pem)
}

/// Look up a persona's public key PEM by username.
pub async fn get_public_key(pool: &Pool, username: &str) -> anyhow::Result<String> {

    let row = fieldwork_db::persona_db::get_persona_by_username(pool, username)
        .await
        .with_context(|| format!("persona @{username} not found"))?
        .with_context(|| format!("persona @{username} not found"))?;
    Ok(row.public_key_pem)
}

/// Look up a persona's ID by username.
pub async fn get_id(pool: &Pool, username: &str) -> anyhow::Result<i64> {

    let row = fieldwork_db::persona_db::get_persona_by_username(pool, username)
        .await
        .with_context(|| format!("persona @{username} not found"))?
        .with_context(|| format!("persona @{username} not found"))?;
    Ok(row.id)
}
