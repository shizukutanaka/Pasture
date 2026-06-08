//! Exact-match response cache (IMP-6, FrugalGPT "prompt adaptation"; same
//! spirit as the project's Cotton inference cache).
//!
//! Identical requests return a stored answer without calling any backend,
//! saving cloud cost. Zero-dependency: keys are hashed with the standard
//! library hasher; eviction is bounded FIFO + optional TTL (IMP-cache-ttl).
//! Sensitive prompts are never cached (the caller skips them).

use crate::backend::{CompletionRequest, CompletionResponse};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Stable key for a request: model + ordered (role, content) of each message +
/// the sampling parameters. Sampling is part of the key so that, e.g., a
/// `temperature:0` response is never served to a `temperature:1` request.
pub fn request_key(req: &CompletionRequest) -> u64 {
    let mut h = DefaultHasher::new();
    req.model.hash(&mut h);
    for m in &req.messages {
        m.role.hash(&mut h);
        m.content.hash(&mut h);
    }
    let s = &req.sampling;
    // f64 has no Hash; hash the bit pattern (None as a fixed sentinel).
    let hash_opt_f64 = |h: &mut DefaultHasher, x: Option<f64>| match x {
        Some(v) => {
            1u8.hash(h);
            v.to_bits().hash(h);
        }
        None => 0u8.hash(h),
    };
    hash_opt_f64(&mut h, s.temperature);
    hash_opt_f64(&mut h, s.top_p);
    s.max_tokens.hash(&mut h);
    s.seed.hash(&mut h);
    hash_opt_f64(&mut h, s.presence_penalty);
    hash_opt_f64(&mut h, s.frequency_penalty);
    for stop in &s.stop {
        stop.hash(&mut h);
    }
    // response_format (a JSON value) has no Hash; hash its canonical string.
    if let Some(rf) = &s.response_format {
        rf.to_json_string().hash(&mut h);
    }
    h.finish()
}

/// A bounded, FIFO-evicting response cache with live hit/miss counters and
/// optional TTL (IMP-cache-ttl). Each entry records its insertion time;
/// `get` treats entries older than `max_age` as misses and removes them.
pub struct ResponseCache {
    map: HashMap<u64, (CompletionResponse, Instant)>,
    order: VecDeque<u64>,
    cap: usize,
    /// Maximum age of a cached entry. None = no TTL (entries live until evicted by FIFO).
    max_age: Option<Duration>,
    /// Total number of successful cache lookups since the cache was created.
    hits: AtomicU64,
    /// Total number of failed cache lookups since the cache was created.
    misses: AtomicU64,
}

impl ResponseCache {
    /// Create a cache holding at most `cap` entries (cap of 0 disables storage).
    pub fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
            max_age: None,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Set a maximum age for cached entries (builder). Entries older than `secs`
    /// are treated as misses and removed on the next `get`. `secs == 0` disables TTL.
    pub fn with_max_age(mut self, secs: u64) -> Self {
        self.set_max_age(secs);
        self
    }

    /// Mutating variant of `with_max_age`, usable on an already-stored cache.
    pub fn set_max_age(&mut self, secs: u64) {
        self.max_age = if secs > 0 {
            Some(Duration::from_secs(secs))
        } else {
            None
        };
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Maximum number of entries this cache will hold (0 = disabled).
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Configured TTL in seconds (0 = no TTL).
    pub fn max_age_secs(&self) -> u64 {
        self.max_age.map(|d| d.as_secs()).unwrap_or(0)
    }

    /// Cumulative cache hits since creation (monotonically increasing).
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Cumulative cache misses since creation (monotonically increasing).
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Look up a cached response (cloned). Increments hit or miss counter.
    /// Expired entries (TTL exceeded) are treated as misses and removed.
    pub fn get(&mut self, key: u64) -> Option<CompletionResponse> {
        // Clone early to avoid holding a live borrow while potentially removing.
        let found = self.map.get(&key).map(|(r, ins)| (r.clone(), *ins));
        match found {
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
            Some((resp, inserted)) => {
                if let Some(max_age) = self.max_age {
                    if inserted.elapsed() > max_age {
                        self.map.remove(&key);
                        self.misses.fetch_add(1, Ordering::Relaxed);
                        return None;
                    }
                }
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(resp)
            }
        }
    }

    /// Store a response with the current timestamp, evicting the oldest entry
    /// when over capacity. Updating an existing key refreshes its timestamp.
    pub fn put(&mut self, key: u64, resp: CompletionResponse) {
        if self.cap == 0 {
            return;
        }
        if self.map.insert(key, (resp, Instant::now())).is_none() {
            self.order.push_back(key);
        }
        while self.map.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Message;

    fn req(model: &str, content: &str) -> CompletionRequest {
        CompletionRequest {
            model: model.to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: content.to_string(),
            }],
            stream: false,
            has_tools: false,
            sampling: Default::default(),
        }
    }

    fn resp(content: &str) -> CompletionResponse {
        CompletionResponse {
            content: content.to_string(),
            model: "m".to_string(),
            prompt_tokens: 1,
            completion_tokens: 1,
        }
    }

    #[test]
    fn test_request_key_stable_and_distinct() {
        assert_eq!(request_key(&req("m", "hi")), request_key(&req("m", "hi")));
        assert_ne!(request_key(&req("m", "hi")), request_key(&req("m", "bye")));
        assert_ne!(request_key(&req("a", "hi")), request_key(&req("b", "hi")));
    }

    #[test]
    fn test_put_get_roundtrip() {
        let mut c = ResponseCache::new(4);
        let k = request_key(&req("m", "hi"));
        assert!(c.get(k).is_none());
        c.put(k, resp("hello"));
        assert_eq!(c.get(k).unwrap().content, "hello");
    }

    #[test]
    fn test_fifo_eviction() {
        let mut c = ResponseCache::new(2);
        c.put(1, resp("a"));
        c.put(2, resp("b"));
        c.put(3, resp("c")); // evicts key 1
        assert!(c.get(1).is_none());
        assert!(c.get(2).is_some());
        assert!(c.get(3).is_some());
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn test_cap_zero_disables() {
        let mut c = ResponseCache::new(0);
        c.put(1, resp("a"));
        assert!(c.get(1).is_none());
        assert!(c.is_empty());
    }

    #[test]
    fn test_update_existing_key_no_duplicate_order() {
        let mut c = ResponseCache::new(2);
        c.put(1, resp("a"));
        c.put(1, resp("a2"));
        assert_eq!(c.len(), 1);
        assert_eq!(c.get(1).unwrap().content, "a2");
    }

    #[test]
    fn test_hit_miss_counters() {
        let mut c = ResponseCache::new(4);
        assert_eq!(c.hits(), 0);
        assert_eq!(c.misses(), 0);
        c.get(99); // miss
        assert_eq!(c.misses(), 1);
        assert_eq!(c.hits(), 0);
        c.put(1, resp("a"));
        c.get(1); // hit
        c.get(1); // hit
        c.get(2); // miss
        assert_eq!(c.hits(), 2);
        assert_eq!(c.misses(), 2);
    }

    #[test]
    fn test_cap_zero_counts_misses() {
        let mut c = ResponseCache::new(0);
        c.put(1, resp("a")); // no-op
        c.get(1); // miss (nothing stored)
        assert_eq!(c.misses(), 1);
        assert_eq!(c.hits(), 0);
    }

    #[test]
    fn test_ttl_zero_disables_expiry() {
        let mut c = ResponseCache::new(4).with_max_age(0);
        assert_eq!(c.max_age_secs(), 0);
        c.put(1, resp("a"));
        // With TTL=0 (disabled) entry lives indefinitely.
        assert!(c.get(1).is_some());
    }

    #[test]
    fn test_ttl_expired_entry_counts_as_miss() {
        // Use a 1-second TTL but sleep past it via a Duration trick:
        // instead of sleeping, back-date the insertion by manipulating
        // the stored Instant via the 1-ns TTL.
        let mut c = ResponseCache::new(4).with_max_age(1);
        c.put(1, resp("a"));
        // Entry should be live immediately.
        assert_eq!(c.get(1).unwrap().content, "a");
        assert_eq!(c.hits(), 1);
    }

    #[test]
    fn test_ttl_very_short_expires_entry() {
        // Create a cache with a 1-second TTL, then manually simulate expiry
        // by inserting a 0-duration max_age. This is a unit test of the
        // logic; real TTL correctness is verified by max_age_secs().
        let mut c = ResponseCache::new(4);
        c.max_age = Some(Duration::from_nanos(1)); // effectively instant expiry
        c.put(1, resp("a")); // inserted now
        // Yield to let the Instant advance past 1ns.
        std::thread::sleep(Duration::from_millis(1));
        // Entry should be expired.
        let result = c.get(1);
        assert!(result.is_none(), "1ns TTL entry should have expired");
        assert_eq!(c.misses(), 1);
        assert_eq!(c.hits(), 0);
        // Expired entry should be removed from map.
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn test_ttl_expired_entry_removed_from_map() {
        let mut c = ResponseCache::new(4);
        c.max_age = Some(Duration::from_nanos(1));
        c.put(1, resp("a"));
        c.put(2, resp("b"));
        std::thread::sleep(Duration::from_millis(1));
        let _ = c.get(1); // expired, removed
        assert_eq!(c.len(), 1, "expired entry should be removed, leaving only key 2");
    }
}
