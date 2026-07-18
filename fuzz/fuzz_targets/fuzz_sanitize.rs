#![no_main]
use libfuzzer_sys::fuzz_target;

// Fuzz the HTML sanitizer with arbitrary HTML input.
// Looking for panics or cases where dangerous content survives sanitization.
fuzz_target!(|data: &str| {
    let result = broadside::sanitize::sanitize_html(data);
    // Verify no script tags survive
    let lower = result.to_lowercase();
    assert!(
        !lower.contains("<script"),
        "script tag survived sanitization: {result}"
    );
    // Check that javascript: doesn't appear in href attributes
    assert!(
        !lower.contains("href=\"javascript:"),
        "javascript: URI in href survived: {result}"
    );

    // Also fuzz markdown → HTML → sanitize pipeline
    let md_result = broadside::sanitize::markdown_to_html(data);
    let md_lower = md_result.to_lowercase();
    assert!(
        !md_lower.contains("<script"),
        "script tag in markdown output: {md_result}"
    );
});
