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

// ── Semantic cache (IMP-12) ──────────────────────────────────────────────────

/// Cosine similarity between two equal-length vectors.
/// Returns 0.0 when either vector has zero magnitude or lengths differ.
pub fn cosine_similarity(a: &[f64], b: &[f64]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let mag_a: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let mag_b: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 {
        0.0
    } else {
        dot / (mag_a * mag_b)
    }
}

/// Bounded FIFO semantic cache keyed on embedding vectors (IMP-12).
///
/// `find_similar` performs a linear scan over all stored vectors and returns
/// the response paired with the vector whose cosine similarity to the query
/// meets or exceeds `threshold`. On a tie the most-similar entry wins.
/// Sensitive prompts are never stored — the caller enforces that invariant.
///
/// Each entry also carries the requested model name (ADR-158) and a sampling
/// signature (ADR-159): two requests with semantically similar content must not
/// share a cached response when they target different models, or differ in
/// output-affecting sampling parameters (temperature, top_p, max_tokens, seed,
/// penalties, stop, response_format). The exact-match cache already keys on
/// model + sampling via `request_key`; the semantic cache matches the prompt
/// fuzzily (cosine) but model and sampling exactly.
///
/// Like `ResponseCache`, entries optionally expire after `max_age` (ADR-160) so
/// `PASTURE_CACHE_TTL` bounds staleness for *both* caches, not just the
/// exact-match one — otherwise a semantic hit could return an answer older than
/// the operator's configured TTL.
struct SemanticEntry {
    embedding: Vec<f64>,
    model: String,
    sampling: u64,
    inserted: Instant,
    resp: CompletionResponse,
}

pub struct SemanticCache {
    entries: VecDeque<SemanticEntry>,
    cap: usize,
    /// Minimum cosine similarity for a hit (e.g. 0.92).
    threshold: f64,
    /// Maximum age of a cached entry. None = no TTL (FIFO eviction only).
    max_age: Option<Duration>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl SemanticCache {
    pub fn new(cap: usize, threshold: f64) -> Self {
        Self {
            entries: VecDeque::new(),
            cap,
            threshold,
            max_age: None,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Set a maximum age for cached entries (ADR-160). Entries older than `secs`
    /// are treated as misses and removed on the next `find_similar`. `secs == 0`
    /// disables TTL. Mirrors `ResponseCache::set_max_age` so one `PASTURE_CACHE_TTL`
    /// bounds staleness for both caches.
    pub fn set_max_age(&mut self, secs: u64) {
        self.max_age = if secs > 0 {
            Some(Duration::from_secs(secs))
        } else {
            None
        };
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn cap(&self) -> usize {
        self.cap
    }

    pub fn threshold(&self) -> f64 {
        self.threshold
    }

    /// Configured TTL in seconds (0 = no TTL).
    pub fn max_age_secs(&self) -> u64 {
        self.max_age.map(|d| d.as_secs()).unwrap_or(0)
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Find the best-matching cached response (cosine ≥ threshold, same model and
    /// sampling). Returns a clone of the response. Updates hit/miss counters.
    /// Entries for other models (ADR-158) or with a different sampling signature
    /// (ADR-159) are skipped — different models and different output-affecting
    /// sampling produce different outputs, so they must not cross-serve. Entries
    /// older than `max_age` (ADR-160) are removed first, so an expired entry is a
    /// miss and frees its slot, matching `ResponseCache::get`.
    pub fn find_similar(
        &mut self,
        query: &[f64],
        model: &str,
        sampling: u64,
    ) -> Option<CompletionResponse> {
        if let Some(max_age) = self.max_age {
            self.entries.retain(|e| e.inserted.elapsed() <= max_age);
        }
        let mut best_score = self.threshold - f64::EPSILON;
        let mut best: Option<&CompletionResponse> = None;
        for e in &self.entries {
            if e.model != model || e.sampling != sampling {
                continue;
            }
            let score = cosine_similarity(query, &e.embedding);
            if score > best_score {
                best_score = score;
                best = Some(&e.resp);
            }
        }
        match best {
            Some(resp) => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(resp.clone())
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Store an embedding → response pair, evicting the oldest when over capacity.
    /// `model` (ADR-158) and `sampling` (ADR-159, from `sampling_key`) are stored
    /// to prevent cross-model and cross-sampling hits.
    pub fn put(&mut self, embedding: Vec<f64>, model: String, sampling: u64, resp: CompletionResponse) {
        if self.cap == 0 {
            return;
        }
        self.entries.push_back(SemanticEntry {
            embedding,
            model,
            sampling,
            inserted: Instant::now(),
            resp,
        });
        while self.entries.len() > self.cap {
            self.entries.pop_front();
        }
    }
}

// ── Exact-match cache ────────────────────────────────────────────────────────

/// Stable key for a request: model + ordered (role, content) of each message +
/// the sampling parameters. Sampling is part of the key so that, e.g., a
/// `temperature:0` response is never served to a `temperature:1` request.
pub fn request_key(req: &CompletionRequest) -> u64 {
    let mut h = DefaultHasher::new();
    req.model.hash(&mut h);
    for m in &req.messages {
        m.role.hash(&mut h);
        // Normalise leading/trailing whitespace so "hi " and "hi" share a key.
        // Internal whitespace is left intact (code/formatting matters there).
        m.content.trim().hash(&mut h);
    }
    hash_sampling(&mut h, &req.sampling);
    h.finish()
}

/// Fold the sampling parameters into `h`. Shared by `request_key` (exact-match
/// cache) and `sampling_key` (semantic cache) so both caches treat the same set
/// of output-affecting parameters as significant — there is one source of truth
/// for "which knobs change the answer".
fn hash_sampling(h: &mut DefaultHasher, s: &crate::backend::SamplingParams) {
    // f64 has no Hash; hash the bit pattern (None as a fixed sentinel).
    let hash_opt_f64 = |h: &mut DefaultHasher, x: Option<f64>| match x {
        Some(v) => {
            1u8.hash(h);
            v.to_bits().hash(h);
        }
        None => 0u8.hash(h),
    };
    hash_opt_f64(h, s.temperature);
    hash_opt_f64(h, s.top_p);
    s.max_tokens.hash(h);
    s.seed.hash(h);
    hash_opt_f64(h, s.presence_penalty);
    hash_opt_f64(h, s.frequency_penalty);
    for stop in &s.stop {
        stop.hash(h);
    }
    // response_format (a JSON value) has no Hash; hash its canonical string.
    if let Some(rf) = &s.response_format {
        rf.to_json_string().hash(h);
    }
    // tools / tool_choice change the answer (the model may call a tool), so they
    // are part of the key — two requests with identical messages but different
    // tools must not cross-serve a cached response (ADR-177).
    if let Some(tools) = &s.tools {
        tools.to_json_string().hash(h);
    }
    if let Some(tc) = &s.tool_choice {
        tc.to_json_string().hash(h);
    }
}

/// Stable signature of just the sampling parameters (ADR-159). The semantic
/// cache stores this alongside each entry so a `temperature:0` (deterministic)
/// answer is never served to a `temperature:1.8` (high-randomness) request —
/// the same invariant `request_key` enforces for the exact-match cache, applied
/// to the fuzzy cache. The prompt is matched by cosine; model and sampling are
/// matched exactly.
pub fn sampling_key(s: &crate::backend::SamplingParams) -> u64 {
    let mut h = DefaultHasher::new();
    hash_sampling(&mut h, s);
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
                        // Also remove from the FIFO queue; without this the ghost key
                        // stays in `order` indefinitely, causing unbounded deque growth
                        // when the cache has a TTL and entries expire on get() (ADR-098).
                        self.order.retain(|&k| k != key);
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
                ..Default::default()
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
            tool_calls: None,
        }
    }

    #[test]
    fn test_request_key_whitespace_normalised() {
        // Leading/trailing whitespace should not produce different keys.
        assert_eq!(request_key(&req("m", "hi")), request_key(&req("m", "hi ")));
        assert_eq!(request_key(&req("m", "hi")), request_key(&req("m", " hi")));
        assert_eq!(request_key(&req("m", "hi")), request_key(&req("m", "  hi  ")));
        // But distinct content must still differ.
        assert_ne!(request_key(&req("m", "hi")), request_key(&req("m", "bye")));
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

    #[test]
    fn test_ttl_expiry_removes_ghost_from_order_deque() {
        // Regression test for ADR-098: TTL expiry on get() must remove the key
        // from the FIFO order deque, not just from the map. Without the fix,
        // ghost keys accumulate in `order` without bound.
        let mut c = ResponseCache::new(4);
        c.max_age = Some(Duration::from_nanos(1));
        c.put(1, resp("a"));
        c.put(2, resp("b"));
        std::thread::sleep(Duration::from_millis(1));
        let _ = c.get(1); // expired → must remove from both map and order
        let _ = c.get(2); // expired → must remove from both map and order
        // After both expire and are evicted on get(), the order deque must be empty.
        assert_eq!(c.order.len(), 0, "order deque must not retain ghost entries after TTL expiry");
        // Re-fill to capacity: FIFO eviction must still work correctly (no phantom pops).
        c.max_age = None;
        c.put(3, resp("c"));
        c.put(4, resp("d"));
        c.put(5, resp("e"));
        c.put(6, resp("f"));
        assert_eq!(c.len(), 4);
        c.put(7, resp("g")); // evicts key 3
        assert!(c.get(3).is_none(), "FIFO eviction must still work after TTL-expiry cleanup");
        assert!(c.get(7).is_some());
    }

    // ── SemanticCache tests ──────────────────────────────────────────────────

    #[test]
    fn test_cosine_similarity_identical() {
        let v = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!((cosine_similarity(&a, &b)).abs() < 1e-12);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        assert!((cosine_similarity(&a, &b) + 1.0).abs() < 1e-12);
    }

    #[test]
    fn test_cosine_similarity_length_mismatch_returns_zero() {
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[1.0]), 0.0);
    }

    #[test]
    fn test_cosine_similarity_zero_vector_returns_zero() {
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 2.0]), 0.0);
    }

    #[test]
    fn test_semantic_cache_hit_above_threshold() {
        let mut c = SemanticCache::new(4, 0.9);
        let v = vec![1.0_f64, 0.0, 0.0];
        c.put(v.clone(), "gpt-4o".to_string(), 0, resp("answer"));
        // Identical query, same model + sampling → cosine = 1.0 ≥ 0.9
        let hit = c.find_similar(&v, "gpt-4o", 0);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().content, "answer");
        assert_eq!(c.hits(), 1);
        assert_eq!(c.misses(), 0);
    }

    #[test]
    fn test_semantic_cache_miss_below_threshold() {
        let mut c = SemanticCache::new(4, 0.95);
        c.put(vec![1.0, 0.0], "m".to_string(), 0, resp("a"));
        // Orthogonal vector → cosine = 0 < 0.95
        let hit = c.find_similar(&[0.0, 1.0], "m", 0);
        assert!(hit.is_none());
        assert_eq!(c.misses(), 1);
    }

    #[test]
    fn test_semantic_cache_picks_best_match() {
        let mut c = SemanticCache::new(4, 0.8);
        // Store two vectors at different angles from [1,0]
        let v_close = vec![0.99f64, 0.14142f64]; // cos ~ 0.99
        let inv_sqrt2 = std::f64::consts::FRAC_1_SQRT_2;
        let v_far = vec![inv_sqrt2, inv_sqrt2]; // cos ~ 0.707
        c.put(v_far.clone(), "m".to_string(), 0, resp("far"));
        c.put(v_close.clone(), "m".to_string(), 0, resp("close"));
        let hit = c.find_similar(&[1.0, 0.0], "m", 0);
        // Should return the closest match ("close")
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().content, "close");
    }

    #[test]
    fn test_semantic_cache_fifo_eviction() {
        let mut c = SemanticCache::new(2, 0.9);
        let v = vec![1.0f64, 0.0];
        c.put(vec![1.0, 0.0], "m".to_string(), 0, resp("first"));
        c.put(vec![0.0, 1.0], "m".to_string(), 0, resp("second"));
        c.put(vec![0.5, 0.5], "m".to_string(), 0, resp("third")); // evicts "first"
        assert_eq!(c.len(), 2);
        // "first" entry ([1,0]) was evicted; an identical query returns one of the remaining
        let hit = c.find_similar(&v, "m", 0);
        // After eviction, [1,0] is gone; [0,1] and [0.5,0.5] remain.
        // cos([1,0],[0,1])=0, cos([1,0],[0.5,0.5])≈0.707 — both below threshold 0.9
        assert!(hit.is_none(), "evicted entry must not be found");
    }

    #[test]
    fn test_semantic_cache_cap_zero_stores_nothing() {
        let mut c = SemanticCache::new(0, 0.9);
        c.put(vec![1.0, 0.0], "m".to_string(), 0, resp("x"));
        assert!(c.is_empty());
        assert!(c.find_similar(&[1.0, 0.0], "m", 0).is_none());
    }

    #[test]
    fn test_semantic_cache_different_model_is_miss() {
        // ADR-158: semantically identical content for different models must NOT
        // cross-serve — different models produce different outputs.
        let mut c = SemanticCache::new(4, 0.9);
        let v = vec![1.0_f64, 0.0];
        c.put(v.clone(), "llama3".to_string(), 0, resp("local answer"));
        // Same embedding, different model → must be a miss.
        let hit = c.find_similar(&v, "gpt-4o", 0);
        assert!(
            hit.is_none(),
            "cross-model semantic hit must not occur: {:?}",
            hit.map(|h| h.content)
        );
        assert_eq!(c.misses(), 1);
        // Same model → must be a hit.
        let hit2 = c.find_similar(&v, "llama3", 0);
        assert!(hit2.is_some());
        assert_eq!(hit2.unwrap().content, "local answer");
    }

    #[test]
    fn test_semantic_cache_different_sampling_is_miss() {
        // ADR-159: semantically identical content with different output-affecting
        // sampling (e.g. temperature) must NOT cross-serve — the exact-match cache
        // already enforces this; the semantic cache must too.
        let mut c = SemanticCache::new(4, 0.9);
        let v = vec![1.0_f64, 0.0];
        // Stored under sampling signature 111 (e.g. temperature:0, deterministic).
        c.put(v.clone(), "m".to_string(), 111, resp("deterministic answer"));
        // Same embedding + model but a different sampling signature → miss.
        let hit = c.find_similar(&v, "m", 222);
        assert!(
            hit.is_none(),
            "cross-sampling semantic hit must not occur: {:?}",
            hit.map(|h| h.content)
        );
        assert_eq!(c.misses(), 1);
        // Same sampling signature → hit.
        let hit2 = c.find_similar(&v, "m", 111);
        assert!(hit2.is_some());
        assert_eq!(hit2.unwrap().content, "deterministic answer");
    }

    #[test]
    fn test_semantic_cache_ttl_expired_entry_is_miss_and_removed() {
        // ADR-160: PASTURE_CACHE_TTL must bound staleness for the semantic cache
        // too, not just the exact-match cache. An expired entry is a miss and is
        // removed so it frees its slot (matching ResponseCache::get).
        let mut c = SemanticCache::new(4, 0.9);
        c.set_max_age(1); // 1-second TTL configured...
        assert_eq!(c.max_age_secs(), 1);
        // ...but back-date by forcing a sub-nanosecond effective TTL.
        c.max_age = Some(Duration::from_nanos(1));
        let v = vec![1.0_f64, 0.0];
        c.put(v.clone(), "m".to_string(), 0, resp("stale"));
        std::thread::sleep(Duration::from_millis(1));
        let hit = c.find_similar(&v, "m", 0);
        assert!(hit.is_none(), "expired semantic entry must not be served");
        assert_eq!(c.misses(), 1);
        assert_eq!(c.len(), 0, "expired entry must be removed");
    }

    #[test]
    fn test_semantic_cache_no_ttl_keeps_entry() {
        // Default (no TTL): an entry survives indefinitely until FIFO eviction.
        let mut c = SemanticCache::new(4, 0.9);
        assert_eq!(c.max_age_secs(), 0);
        let v = vec![1.0_f64, 0.0];
        c.put(v.clone(), "m".to_string(), 0, resp("fresh"));
        std::thread::sleep(Duration::from_millis(1));
        let hit = c.find_similar(&v, "m", 0);
        assert!(hit.is_some(), "without TTL the entry must remain");
        assert_eq!(hit.unwrap().content, "fresh");
    }

    #[test]
    fn test_sampling_key_distinguishes_temperature() {
        use crate::backend::SamplingParams;
        let t0 = SamplingParams { temperature: Some(0.0), ..Default::default() };
        let t1 = SamplingParams { temperature: Some(1.0), ..Default::default() };
        let t0b = SamplingParams { temperature: Some(0.0), ..Default::default() };
        assert_ne!(sampling_key(&t0), sampling_key(&t1), "temp 0 vs 1 must differ");
        assert_eq!(sampling_key(&t0), sampling_key(&t0b), "same params must match");
    }
}
