use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Generate a snowflake-style ID: millisecond timestamp + sequence counter.
///
/// Format: "{millis}-{seq}" as text. Roughly chronological; sequence counter
/// uses SeqCst ordering so concurrent callers never produce duplicate IDs.
pub fn gen_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_millis();
    let seq = SEQUENCE.fetch_add(1, Ordering::SeqCst);
    format!("{millis}-{seq:06}")
}
