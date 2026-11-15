//! Idempotency-Key response store (IMP-74, ported from RekAI).
//!
//! A client that retries a `POST /v1/chat/completions` after a timeout
//! cannot tell whether the first attempt already ran — and already cost
//! money. With an `Idempotency-Key` header, the first successful response
//! is stored here and a duplicate request replays it byte-for-byte instead
//! of running the completion (and its cost) a second time.
//!
//! The store is deliberately small and honest about its limits: in-memory
//! only (a restart forgets every key), bounded with FIFO eviction, and it
//! stores successes only — an error is never replayed, so a retry after a
//! failure still gets a real attempt. Keys are opaque client-chosen
//! strings; the proxy treats the header value as the whole key.

use std::collections::{HashMap, VecDeque};

/// Default capacity: enough for a burst of client retries, small enough
/// that stored response bodies stay a minor memory cost.
pub const DEFAULT_CAPACITY: usize = 1024;

pub struct IdempotencyStore {
    capacity: usize,
    entries: HashMap<String, String>,
    order: VecDeque<String>,
}

impl IdempotencyStore {
    pub fn new(capacity: usize) -> Self {
        IdempotencyStore {
            capacity: capacity.max(1),
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// The stored response body for `key`, if a success was recorded.
    pub fn get(&self, key: &str) -> Option<String> {
        self.entries.get(key).cloned()
    }

    /// Record a successful response body under `key`. Re-recording an
    /// existing key refreshes its body but keeps its eviction position.
    pub fn put(&mut self, key: &str, body: String) {
        if !self.entries.contains_key(key) {
            self.order.push_back(key.to_string());
            while self.order.len() > self.capacity {
                if let Some(oldest) = self.order.pop_front() {
                    self.entries.remove(&oldest);
                }
            }
        }
        self.entries.insert(key.to_string(), body);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for IdempotencyStore {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_put_get_roundtrip() {
        let mut s = IdempotencyStore::new(4);
        assert_eq!(s.get("k1"), None);
        s.put("k1", "body-1".to_string());
        assert_eq!(s.get("k1"), Some("body-1".to_string()));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn test_fifo_eviction() {
        let mut s = IdempotencyStore::new(2);
        s.put("a", "1".to_string());
        s.put("b", "2".to_string());
        s.put("c", "3".to_string());
        assert_eq!(s.get("a"), None, "oldest evicted first");
        assert_eq!(s.get("b"), Some("2".to_string()));
        assert_eq!(s.get("c"), Some("3".to_string()));
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn test_reput_refreshes_body_not_position() {
        let mut s = IdempotencyStore::new(2);
        s.put("a", "1".to_string());
        s.put("b", "2".to_string());
        s.put("a", "1-new".to_string());
        s.put("c", "3".to_string());
        // "a" kept its original (oldest) position, so it is evicted first.
        assert_eq!(s.get("a"), None);
        assert_eq!(s.get("b"), Some("2".to_string()));
    }

    #[test]
    fn test_capacity_floor_of_one() {
        let mut s = IdempotencyStore::new(0);
        s.put("a", "1".to_string());
        s.put("b", "2".to_string());
        assert_eq!(s.len(), 1);
        assert_eq!(s.get("b"), Some("2".to_string()));
    }
}
