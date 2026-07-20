use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::Mutex;

/// Simple per-IP token bucket rate limiter.
// ponytail: single Mutex over the entire HashMap. At current inbox load (tens of req/s)
// this is not a bottleneck. Ceiling: replace with DashMap or sharded locks if profiling
// shows contention (typically only needed above ~10k req/s on many-core hardware).
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
    capacity: u32,
    refill_rate: f64,
}

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    /// Create a rate limiter allowing `capacity` requests per `window_secs`.
    pub fn new(capacity: u32, window_secs: u64) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            capacity,
            refill_rate: capacity as f64 / window_secs as f64,
        }
    }

    /// Try to consume one token for the given key. Returns false if rate limited.
    pub async fn try_acquire(&self, key: &str) -> bool {
        let mut buckets = self.buckets.lock().await;
        let now = Instant::now();

        // ponytail: entry() requires an owned key, allocating a String on every call even
        // for existing entries. Ceiling: use raw_entry (nightly) or switch to DashMap which
        // accepts &str via the Equivalent trait.
        let bucket = buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: self.capacity as f64,
            last_refill: now,
        });

        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.refill_rate).min(self.capacity as f64);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Prune stale entries older than 1 hour.
    pub async fn prune(&self) {
        let mut buckets = self.buckets.lock().await;
        let now = Instant::now();
        buckets.retain(|_, b| now.duration_since(b.last_refill).as_secs() < 3600);
    }
}
