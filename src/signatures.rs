use anyhow::Context;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chrono::Utc;
use reqwest::header::HeaderMap;
use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey};
use rsa::sha2::Sha256;
use rsa::signature::{SignatureEncoding, Signer, Verifier};
use rsa::{RsaPrivateKey, RsaPublicKey};

/// Sign an outbound HTTP POST request.
///
/// Returns headers to add to the request: `Date`, `Digest`, `Signature`.
/// Follows the HTTP Signatures spec (draft-cavage-http-signatures) as
/// implemented by Mastodon.
pub fn sign_request(
    private_key_pem: &str,
    key_id: &str,
    target: &str,
    host: &str,
    body: &[u8],
) -> anyhow::Result<HeaderMap> {
    // ponytail: PEM is re-parsed on every call (~1ms). Delivery is IO-bound (HTTP round-trip
    // dominates), so this is negligible. Upgrade path: accept a pre-parsed key or cache in
    // the delivery worker's post_cache.
    let private_key =
        RsaPrivateKey::from_pkcs8_pem(private_key_pem).context("parsing private key PEM")?;
    let signing_key = rsa::pkcs1v15::SigningKey::<Sha256>::new(private_key);

    let date = Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let digest = format!("SHA-256={}", B64.encode(sha256_digest(body)));

    let signed_string =
        format!("(request-target): post {target}\nhost: {host}\ndate: {date}\ndigest: {digest}");

    let signature = signing_key.sign(signed_string.as_bytes());
    let sig_b64 = B64.encode(signature.to_bytes());

    let sig_header = format!(
        r#"keyId="{key_id}",algorithm="rsa-sha256",headers="(request-target) host date digest",signature="{sig_b64}""#
    );

    let mut headers = HeaderMap::new();
    headers.insert("Date", date.parse()?);
    headers.insert("Digest", digest.parse()?);
    headers.insert("Signature", sig_header.parse()?);

    Ok(headers)
}

/// Verify an HTTP Signature on an inbound request.
///
/// `public_key_pem`: the sender's public key (fetched from their actor document).
/// `signature_header`: the raw `Signature` header value.
/// `method`, `path`: the request method and path.
/// `headers`: the request headers (used to reconstruct the signed string).
pub fn verify_signature(
    public_key_pem: &str,
    signature_header: &str,
    method: &str,
    path: &str,
    headers: &HeaderMap,
) -> anyhow::Result<()> {
    let parts = parse_signature_header(signature_header)?;

    let signed_headers = parts.headers.split_whitespace();
    let mut lines = Vec::new();
    for h in signed_headers {
        if h == "(request-target)" {
            lines.push(format!(
                "(request-target): {} {}",
                method.to_lowercase(),
                path
            ));
        } else {
            let val = headers
                .get(h)
                .with_context(|| format!("missing header {h:?} referenced in signature"))?
                .to_str()
                .with_context(|| format!("non-ascii header {h:?}"))?;
            lines.push(format!("{h}: {val}"));
        }
    }
    let signed_string = lines.join("\n");

    let public_key =
        RsaPublicKey::from_public_key_pem(public_key_pem).context("parsing public key PEM")?;
    let verifying_key = rsa::pkcs1v15::VerifyingKey::<Sha256>::new(public_key);

    let sig_bytes = B64
        .decode(&parts.signature)
        .context("decoding signature base64")?;
    let signature = rsa::pkcs1v15::Signature::try_from(sig_bytes.as_slice())
        .context("parsing signature bytes")?;

    verifying_key
        .verify(signed_string.as_bytes(), &signature)
        .context("signature verification failed")?;

    Ok(())
}

/// Parsed components of an HTTP Signature header.
// ponytail: fields are owned Strings because they are returned from parse_signature_header
// and the input header string does not live long enough to borrow from. Ceiling: use
// Cow<'_, str> if profiling shows excessive allocation on the hot verify path.
pub struct SignatureParts {
    pub key_id: String,
    pub headers: String,
    pub signature: String,
}

impl std::fmt::Debug for SignatureParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignatureParts")
            .field("key_id", &self.key_id)
            .field("headers", &self.headers)
            .field("signature", &"[REDACTED]")
            .finish()
    }
}

/// Parse a Signature header value into its components.
///
/// Format: `keyId="...",algorithm="...",headers="...",signature="..."`
pub fn parse_signature_header(header: &str) -> anyhow::Result<SignatureParts> {
    let mut key_id_val = None;
    let mut headers_val = None;
    let mut signature_val = None;

    for part in split_signature_params(header) {
        let part = part.trim();
        if let Some(inner) = part.strip_prefix("keyId=\"") {
            let val = inner
                .strip_suffix('"')
                .context("keyId value missing closing quote in Signature header")?;
            key_id_val = Some(val.to_string());
        } else if let Some(inner) = part.strip_prefix("headers=\"") {
            let val = inner
                .strip_suffix('"')
                .context("headers value missing closing quote in Signature header")?;
            headers_val = Some(val.to_string());
        } else if let Some(inner) = part.strip_prefix("signature=\"") {
            let val = inner
                .strip_suffix('"')
                .context("signature value missing closing quote in Signature header")?;
            signature_val = Some(val.to_string());
        }
    }

    Ok(SignatureParts {
        key_id: key_id_val.context("missing keyId= in Signature header")?,
        headers: headers_val.context("missing headers= in Signature header")?,
        signature: signature_val.context("missing signature= in Signature header")?,
    })
}

/// Split signature header params, respecting quoted values and backslash escapes.
// ponytail: returns Vec<String> not Vec<&str> because backslash-escape handling builds new strings.
fn split_signature_params(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escape_next = false;

    for c in s.chars() {
        if escape_next {
            current.push(c);
            escape_next = false;
            continue;
        }
        match c {
            '\\' if in_quotes => {
                escape_next = true;
                current.push(c);
            }
            '"' => {
                in_quotes = !in_quotes;
                current.push(c);
            }
            ',' if !in_quotes => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn sha256_digest(data: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}
