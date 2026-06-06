//! Exact-match response cache (IMP-6, FrugalGPT "prompt adaptation"; same
//! spirit as the project's Cotton inference cache).
//!
//! Identical requests return a stored answer without calling any backend,
//! saving cloud cost. Zero-dependency: keys are hashed with the standard
//! library hasher; eviction is bounded FIFO. Sensitive prompts are never
//! cached (the caller skips them).

use crate::backend::{CompletionRequest, CompletionResponse};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

/// Stable key for a request: model + ordered (role, content) of each message.
pub fn request_key(req: &CompletionRequest) -> u64 {
    let mut h = DefaultHasher::new();
    req.model.hash(&mut h);
    for m in &req.messages {
        m.role.hash(&mut h);
        m.content.hash(&mut h);
    }
    h.finish()
}

/// A bounded, FIFO-evicting response cache.
pub struct ResponseCache {
    map: HashMap<u64, CompletionResponse>,
    order: VecDeque<u64>,
    cap: usize,
}

impl ResponseCache {
    /// Create a cache holding at most `cap` entries (cap of 0 disables storage).
    pub fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Look up a cached response (cloned).
    pub fn get(&self, key: u64) -> Option<CompletionResponse> {
        self.map.get(&key).cloned()
    }

    /// Store a response, evicting the oldest entry when over capacity.
    pub fn put(&mut self, key: u64, resp: CompletionResponse) {
        if self.cap == 0 {
            return;
        }
        if self.map.insert(key, resp).is_none() {
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
}
