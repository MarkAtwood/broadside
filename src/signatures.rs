// Thin wrappers around cavage-httpsig. Broadside's original inline implementation
// is replaced by the shared crate; these wrappers preserve the old call-site API
// (path + host strings instead of url::Url) so callers don't need restructuring.

pub use cavage_httpsig::SignatureParts;

/// Sign an outbound HTTP POST request.
///
/// Wrapper: converts (path, host) pair to a url::Url for cavage_httpsig::sign_post.
pub fn sign_request(
    private_key_pem: &str,
    key_id: &str,
    target_path: &str,
    host: &str,
    body: &[u8],
) -> anyhow::Result<reqwest::header::HeaderMap> {
    let url: url::Url = format!("https://{host}{target_path}")
        .parse()
        .map_err(|e| anyhow::anyhow!("building target URL: {e}"))?;
    let headers = cavage_httpsig::sign_post(private_key_pem, key_id, &url, body)
        .map_err(|e| anyhow::anyhow!("signing request: {e}"))?;
    Ok(headers)
}

/// Verify an HTTP Signature on an inbound request.
///
/// Wrapper: converts cavage_httpsig::VerifyError to anyhow::Error.
pub fn verify_signature(
    public_key_pem: &str,
    signature_header: &str,
    method: &str,
    path: &str,
    headers: &reqwest::header::HeaderMap,
) -> anyhow::Result<()> {
    cavage_httpsig::verify(public_key_pem, signature_header, method, path, headers)
        .map_err(|e| anyhow::anyhow!("signature verification failed: {e}"))?;
    Ok(())
}

/// Parse a Signature header value into its components.
///
/// Wrapper: converts cavage_httpsig::VerifyError to anyhow::Error.
pub fn parse_signature_header(header: &str) -> anyhow::Result<SignatureParts> {
    cavage_httpsig::parse_signature_header(header).map_err(|e| anyhow::anyhow!("{e}"))
}
