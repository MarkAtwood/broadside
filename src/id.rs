use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Generate a snowflake-style string ID for tables with TEXT PRIMARY KEY.
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

/// Generate a numeric snowflake-style ID for tables with INTEGER PRIMARY KEY.
///
/// Uses millisecond timestamp shifted left by 10 bits + sequence counter (mod 1024).
/// Produces unique i64 values that sort chronologically.
pub fn gen_int_id() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_millis() as i64;
    let seq = SEQUENCE.fetch_add(1, Ordering::SeqCst) & 0x3FF; // 10 bits
    (millis << 10) | (seq as i64)
}
