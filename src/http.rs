use anyhow::{bail, Context, Result};

/// Read a response body with a hard streaming byte cap.
/// Aborts as soon as accumulated bytes exceed `max_bytes` — never buffers more than that.
pub async fn read_body_limited(mut resp: reqwest::Response, max_bytes: usize) -> Result<Vec<u8>> {
    // Fast-reject via Content-Length if available (advisory but cheap)
    if let Some(len) = resp.content_length() {
        if len > max_bytes as u64 {
            bail!("response Content-Length {len} exceeds {max_bytes} byte limit");
        }
    }

    let mut buf = Vec::with_capacity(max_bytes.min(65536));
    while let Some(chunk) = resp.chunk().await.context("reading response chunk")? {
        if buf.len() + chunk.len() > max_bytes {
            bail!(
                "response body exceeds {} byte limit (read {} + chunk {})",
                max_bytes,
                buf.len(),
                chunk.len()
            );
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}
