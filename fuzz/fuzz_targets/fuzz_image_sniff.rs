#![no_main]
use libfuzzer_sys::fuzz_target;

// Fuzz the image MIME type sniffer with arbitrary bytes.
// Should never panic — only Ok or Err.
fuzz_target!(|data: &[u8]| {
    let _ = broadside::media::sniff_image_mime(data);
});
