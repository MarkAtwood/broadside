#![no_main]
use libfuzzer_sys::fuzz_target;

// Fuzz the Signature header parser with arbitrary input.
// Looking for panics, infinite loops, or OOM from malformed headers.
fuzz_target!(|data: &str| {
    // The signature parser is internal, so we test via verify_signature
    // which calls parse_signature_header and split_signature_params.
    // We don't care about the result — just that it doesn't panic.
    let headers = reqwest::header::HeaderMap::new();
    let _ = broadside::signatures::verify_signature(
        "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA\n-----END PUBLIC KEY-----",
        data,
        "post",
        "/inbox",
        &headers,
    );
});
