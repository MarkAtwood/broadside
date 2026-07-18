use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Generate a snowflake-style ID: millisecond timestamp + sequence counter.
///
/// Format: "{millis}-{seq}" as text. Roughly chronological; sequence counter
/// uses `Relaxed` ordering so cross-thread IDs within the same millisecond
/// may not be strictly monotonic. Good enough for a single-binary server.
pub fn gen_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_millis();
    let seq = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{millis}-{seq:06}")
}
