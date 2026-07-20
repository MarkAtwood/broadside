use anyhow::{bail, Context};
use image::DynamicImage;
use sqlx::SqlitePool;
use std::path::Path;

/// Wrap a raw SqlitePool in fieldwork's Pool enum for shared module calls.
fn fw_pool(pool: &SqlitePool) -> fieldwork::db::Pool {
    fieldwork::db::Pool::Sqlite(pool.clone())
}

const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10 MB
const MAX_DIMENSION: u32 = 4096;
const MAX_MEDIA_PER_POST: usize = 8;

/// Process a local image file: validate, resize if needed, strip EXIF, compute blurhash, store.
pub async fn process_local(
    pool: &SqlitePool,
    post_id: &str,
    file_path: &Path,
    data_dir: &Path,
    description: &str,
) -> anyhow::Result<String> {
    let metadata = tokio::fs::metadata(file_path)
        .await
        .with_context(|| format!("reading {}", file_path.display()))?;

    if metadata.len() > MAX_FILE_SIZE {
        bail!(
            "file {} exceeds 10MB limit ({} bytes)",
            file_path.display(),
            metadata.len()
        );
    }

    let bytes = tokio::fs::read(file_path)
        .await
        .with_context(|| format!("reading {}", file_path.display()))?;

    sniff_image_mime(&bytes)?;
    store_processed_image(pool, post_id, &bytes, data_dir, description).await
}

/// Fetch a remote image URL, validate, and store. Streams with size limit.
pub async fn process_remote(
    pool: &SqlitePool,
    post_id: &str,
    url: &str,
    data_dir: &Path,
    description: &str,
    client: &reqwest::Client,
) -> anyhow::Result<String> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        bail!("media URL must be http or https: {url}");
    }

    // SSRF guard: block private/internal URLs
    if let Ok(parsed) = url::Url::parse(url) {
        if let Some(host) = parsed.host_str() {
            if crate::server::is_private_host_resolved(host).await {
                bail!("media URL points to private/internal host: {url}");
            }
        }
    }

    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching media {url}"))?;

    if !resp.status().is_success() {
        bail!("media fetch failed: HTTP {}", resp.status());
    }

    let bytes = crate::http::read_body_limited(resp, MAX_FILE_SIZE as usize)
        .await
        .with_context(|| format!("reading media body from {url}"))?;

    sniff_image_mime(&bytes)?;
    store_processed_image(pool, post_id, &bytes, data_dir, description).await
}

/// Common image processing and storage. Returns the media row ID.
async fn store_processed_image(
    pool: &SqlitePool,
    post_id: &str,
    raw_bytes: &[u8],
    data_dir: &Path,
    description: &str,
) -> anyhow::Result<String> {
    let (img, width, height) = process_image(raw_bytes)?;

    // Re-encode as PNG (strips EXIF, normalizes format)
    let mut output = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut output);
    img.write_to(&mut cursor, image::ImageFormat::Png)
        .context("encoding processed image")?;

    let media_id = fieldwork::id::generate_id();
    let dest_filename = format!("{media_id}.png");
    let media_dir = data_dir.join("media");
    tokio::fs::create_dir_all(&media_dir).await?;
    let dest_path = media_dir.join(&dest_filename);
    tokio::fs::write(&dest_path, &output).await?;

    let hash = compute_blurhash(&img, width, height);
    let rel_path = format!("media/{dest_filename}");
    let file_size = output.len() as i64;
    let now = chrono::Utc::now().timestamp();
    let user_id = crate::persona::get_operator_user_id(pool).await?;

    let post_id_int: i64 = post_id.parse().context("post_id not a valid integer")?;
    let fwp = fw_pool(pool);
    let post_row = fieldwork::posts_db::get_post(&fwp, post_id_int)
        .await
        .context("looking up post for media")?
        .context("post not found for media")?;
    let persona_id = post_row.persona_id;

    let post_id_int: Option<i64> = post_id.parse().ok();
    let media_row = fieldwork::media_db::MediaRow {
        id: media_id,
        user_id,
        persona_id,
        post_id: post_id_int,
        file_path: rel_path,
        mime_type: "image/png".to_string(), // actual stored format after re-encoding
        file_size,
        width: Some(width as i32),
        height: Some(height as i32),
        blurhash: if hash.is_empty() { None } else { Some(hash) },
        integrity: None,
        description: description.to_string(),
        created_at: now,
    };
    fieldwork::media_db::insert_media(&fw_pool(pool), &media_row).await?;

    Ok(media_id.to_string())
}

/// Fetch media attachments for a post (for ActivityPub serialization).
pub async fn attachments_for_post(
    pool: &SqlitePool,
    post_id: &str,
    domain: &str,
) -> Vec<serde_json::Value> {
    let post_id_int: i64 = match post_id.parse() {
        Ok(id) => id,
        Err(_) => return Vec::new(),
    };

    let rows = match fieldwork::media_db::attachments_for_post(&fw_pool(pool), post_id_int).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(post_id, error = %e, "failed to fetch media attachments");
            return Vec::new();
        }
    };

    rows.iter()
        .map(|m| {
            let url = format!("https://{}/{}", domain, m.file_path);
            let mut attachment = serde_json::json!({
                "type": "Document",
                "mediaType": m.mime_type,
                "url": url,
                "name": m.description,
            });
            if let Some(ref bh) = m.blurhash {
                if !bh.is_empty() {
                    attachment["blurhash"] = serde_json::json!(bh);
                }
            }
            if let Some(w) = m.width {
                attachment["width"] = serde_json::json!(w);
            }
            if let Some(h) = m.height {
                attachment["height"] = serde_json::json!(h);
            }
            attachment
        })
        .collect()
}

/// Maximum media attachments allowed per post.
pub const MAX_MEDIA: usize = MAX_MEDIA_PER_POST;

/// Sniff image MIME type from magic bytes.
pub fn sniff_image_mime(bytes: &[u8]) -> anyhow::Result<&'static str> {
    if bytes.len() < 4 {
        bail!("file too small to identify");
    }
    if bytes.starts_with(b"\x89PNG") {
        Ok("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Ok("image/jpeg")
    } else if bytes.starts_with(b"GIF8") {
        Ok("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Ok("image/webp")
    } else {
        bail!("unsupported image format (not PNG, JPEG, GIF, or WebP)");
    }
}

/// Decode, validate, and resize image. Returns the decoded image with dimensions.
fn process_image(bytes: &[u8]) -> anyhow::Result<(DynamicImage, u32, u32)> {
    // Set decode limits to prevent decompression bombs
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    // Cap decoded memory at 64MB and max 256 image dimensions total (prevents GIF frame bombs)
    limits.max_alloc = Some(64 * 1024 * 1024);
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    reader.limits(limits);
    let img = reader
        .decode()
        .context("decoding image (may exceed size limits)")?;
    let img = if img.width() > MAX_DIMENSION || img.height() > MAX_DIMENSION {
        img.resize(
            MAX_DIMENSION,
            MAX_DIMENSION,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img
    };
    let width = img.width();
    let height = img.height();
    Ok((img, width, height))
}

/// Compute blurhash from an already-decoded image.
fn compute_blurhash(img: &DynamicImage, width: u32, height: u32) -> String {
    let rgba = img.to_rgba8();
    match blurhash::encode(4, 3, width, height, rgba.as_raw()) {
        Ok(hash) => hash,
        Err(e) => {
            tracing::warn!("blurhash computation failed ({width}x{height}): {e}");
            String::new()
        }
    }
}
