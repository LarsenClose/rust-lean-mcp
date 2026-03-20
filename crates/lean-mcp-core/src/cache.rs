//! LRU-bounded cache for LSP responses keyed by file content hash + position.
//!
//! Agents in an edit-check-edit loop frequently re-query the same file and
//! position. When the file content has not changed between queries, the LSP
//! response is identical, so we can serve it from cache and skip the round-trip.
//!
//! # Design
//!
//! * **Content hash** — `DefaultHasher` (SipHash-based) over file text.
//!   Fast, deterministic within a process, and collision-resistant enough for
//!   cache keys.
//! * **TTL** — Every entry carries a creation timestamp. Reads that find an
//!   expired entry treat it as a miss and remove it.
//! * **LRU eviction** — When `max_entries` is reached on insert, the oldest
//!   entry (by creation time) is evicted.
//! * **Concurrency** — `tokio::sync::RwLock` allows many concurrent readers;
//!   writes take an exclusive lock only for the duration of the HashMap
//!   mutation.
//! * **Method isolation** — The cache key includes the LSP method name so that
//!   goal state, hover info, and diagnostics for the same position are stored
//!   independently.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

/// Cache key: content hash of the file, cursor position, and LSP method.
#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct CacheKey {
    pub content_hash: u64,
    pub line: u32,
    pub column: u32,
    pub method: String,
}

impl CacheKey {
    /// Create a new cache key.
    pub fn new(content_hash: u64, line: u32, column: u32, method: impl Into<String>) -> Self {
        Self {
            content_hash,
            line,
            column,
            method: method.into(),
        }
    }
}

/// Cached LSP response with metadata for TTL and LRU ordering.
#[derive(Debug, Clone)]
struct CacheEntry {
    value: serde_json::Value,
    created: Instant,
}

/// LRU-bounded, TTL-aware cache for LSP responses.
///
/// Thread-safe via `Arc<RwLock<…>>` — designed for `tokio` async contexts
/// where reads vastly outnumber writes.
#[derive(Clone)]
pub struct LspCache {
    entries: Arc<RwLock<HashMap<CacheKey, CacheEntry>>>,
    max_entries: usize,
    ttl: Duration,
}

impl LspCache {
    /// Create an empty cache with the given capacity and TTL.
    ///
    /// * `max_entries` — maximum number of entries before LRU eviction.
    /// * `ttl` — time-to-live for each entry; expired entries are treated as
    ///   misses.
    pub fn new(max_entries: usize, ttl: Duration) -> Self {
        Self {
            entries: Arc::new(RwLock::new(HashMap::new())),
            max_entries,
            ttl,
        }
    }

    /// Look up a cached response.
    ///
    /// Returns `None` if the key is absent or the entry has expired.
    /// Expired entries are lazily removed on the next write.
    pub async fn get(&self, key: &CacheKey) -> Option<serde_json::Value> {
        let entries = self.entries.read().await;
        if let Some(entry) = entries.get(key) {
            if entry.created.elapsed() < self.ttl {
                return Some(entry.value.clone());
            }
        }
        None
    }

    /// Store a response in the cache.
    ///
    /// If the cache is at capacity, the oldest entry (by creation time) is
    /// evicted first. Expired entries are also purged opportunistically.
    pub async fn put(&self, key: CacheKey, value: serde_json::Value) {
        let mut entries = self.entries.write().await;

        // Purge expired entries first.
        entries.retain(|_, e| e.created.elapsed() < self.ttl);

        // If still at capacity, evict the oldest entry.
        if entries.len() >= self.max_entries && !entries.contains_key(&key) {
            if let Some(oldest_key) = entries
                .iter()
                .min_by_key(|(_, e)| e.created)
                .map(|(k, _)| k.clone())
            {
                entries.remove(&oldest_key);
            }
        }

        entries.insert(
            key,
            CacheEntry {
                value,
                created: Instant::now(),
            },
        );
    }

    /// Invalidate all entries whose `content_hash` matches the given hash.
    ///
    /// Call this when a file's content changes to ensure stale results are
    /// never served.
    pub async fn invalidate_content(&self, content_hash: u64) {
        let mut entries = self.entries.write().await;
        entries.retain(|k, _| k.content_hash != content_hash);
    }

    /// Remove all entries.
    pub async fn clear(&self) {
        let mut entries = self.entries.write().await;
        entries.clear();
    }

    /// Return the current number of (possibly expired) entries.
    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    /// Return `true` if the cache contains no entries.
    pub async fn is_empty(&self) -> bool {
        self.entries.read().await.is_empty()
    }

    /// Compute a content hash for a file's text.
    ///
    /// Uses `DefaultHasher` (SipHash-1-3) which is fast and
    /// collision-resistant enough for in-process cache keys.
    pub fn hash_content(content: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut hasher);
        hasher.finish()
    }
}

impl std::fmt::Debug for LspCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspCache")
            .field("max_entries", &self.max_entries)
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- Helpers ------------------------------------------------------------

    fn make_key(hash: u64, line: u32, col: u32, method: &str) -> CacheKey {
        CacheKey::new(hash, line, col, method)
    }

    fn short_ttl_cache(max: usize) -> LspCache {
        LspCache::new(max, Duration::from_millis(200))
    }

    fn long_ttl_cache(max: usize) -> LspCache {
        LspCache::new(max, Duration::from_secs(300))
    }

    // -- 1. Basic get/put ---------------------------------------------------

    #[tokio::test]
    async fn put_then_get_returns_value() {
        let cache = long_ttl_cache(16);
        let key = make_key(1, 10, 5, "textDocument/hover");
        let val = json!({"contents": "Nat"});

        cache.put(key.clone(), val.clone()).await;
        let got = cache.get(&key).await;

        assert_eq!(got, Some(val));
    }

    // -- 2. Cache miss ------------------------------------------------------

    #[tokio::test]
    async fn get_missing_key_returns_none() {
        let cache = long_ttl_cache(16);
        let key = make_key(99, 1, 1, "textDocument/hover");

        assert_eq!(cache.get(&key).await, None);
    }

    // -- 3. TTL expiry ------------------------------------------------------

    #[tokio::test]
    async fn expired_entry_returns_none() {
        let cache = short_ttl_cache(16); // 200 ms TTL
        let key = make_key(1, 1, 1, "goal");
        cache.put(key.clone(), json!("hello")).await;

        // Wait for expiry.
        tokio::time::sleep(Duration::from_millis(250)).await;

        assert_eq!(cache.get(&key).await, None);
    }

    // -- 4. TTL not yet expired returns hit ----------------------------------

    #[tokio::test]
    async fn non_expired_entry_returns_value() {
        let cache = LspCache::new(16, Duration::from_secs(10));
        let key = make_key(1, 1, 1, "goal");
        let val = json!(42);
        cache.put(key.clone(), val.clone()).await;

        assert_eq!(cache.get(&key).await, Some(val));
    }

    // -- 5. Invalidation by content hash ------------------------------------

    #[tokio::test]
    async fn invalidate_content_removes_matching_entries() {
        let cache = long_ttl_cache(16);
        let hash = 0xABCD;

        // Insert two entries with the same content hash but different positions.
        let k1 = make_key(hash, 1, 1, "goal");
        let k2 = make_key(hash, 5, 3, "hover");
        // And one entry with a different hash.
        let k3 = make_key(0xFFFF, 1, 1, "goal");

        cache.put(k1.clone(), json!("a")).await;
        cache.put(k2.clone(), json!("b")).await;
        cache.put(k3.clone(), json!("c")).await;

        cache.invalidate_content(hash).await;

        assert_eq!(cache.get(&k1).await, None);
        assert_eq!(cache.get(&k2).await, None);
        // The unrelated entry survives.
        assert_eq!(cache.get(&k3).await, Some(json!("c")));
    }

    // -- 6. Clear -----------------------------------------------------------

    #[tokio::test]
    async fn clear_removes_all_entries() {
        let cache = long_ttl_cache(16);
        for i in 0..5 {
            cache.put(make_key(i, 1, 1, "goal"), json!(i)).await;
        }
        assert_eq!(cache.len().await, 5);

        cache.clear().await;

        assert!(cache.is_empty().await);
    }

    // -- 7. LRU eviction ----------------------------------------------------

    #[tokio::test]
    async fn evicts_oldest_when_at_capacity() {
        let cache = long_ttl_cache(3);

        let k1 = make_key(1, 1, 1, "goal");
        let k2 = make_key(2, 1, 1, "goal");
        let k3 = make_key(3, 1, 1, "goal");

        cache.put(k1.clone(), json!("first")).await;
        // Small delay so creation times are ordered.
        tokio::time::sleep(Duration::from_millis(5)).await;
        cache.put(k2.clone(), json!("second")).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        cache.put(k3.clone(), json!("third")).await;

        assert_eq!(cache.len().await, 3);

        // Inserting a 4th entry should evict the oldest (k1).
        let k4 = make_key(4, 1, 1, "goal");
        cache.put(k4.clone(), json!("fourth")).await;

        assert_eq!(cache.len().await, 3);
        assert_eq!(cache.get(&k1).await, None, "oldest entry should be evicted");
        assert_eq!(cache.get(&k4).await, Some(json!("fourth")));
    }

    // -- 8. Overwrite existing key ------------------------------------------

    #[tokio::test]
    async fn overwrite_updates_value_and_refreshes_timestamp() {
        let cache = long_ttl_cache(16);
        let key = make_key(1, 1, 1, "hover");

        cache.put(key.clone(), json!("old")).await;
        cache.put(key.clone(), json!("new")).await;

        assert_eq!(cache.get(&key).await, Some(json!("new")));
        assert_eq!(cache.len().await, 1);
    }

    // -- 9. Method isolation ------------------------------------------------

    #[tokio::test]
    async fn different_methods_same_position_are_independent() {
        let cache = long_ttl_cache(16);
        let hash = 0x1234;

        let goal_key = make_key(hash, 10, 5, "goal");
        let hover_key = make_key(hash, 10, 5, "hover");

        cache.put(goal_key.clone(), json!("goal_result")).await;
        cache.put(hover_key.clone(), json!("hover_result")).await;

        assert_eq!(cache.get(&goal_key).await, Some(json!("goal_result")));
        assert_eq!(cache.get(&hover_key).await, Some(json!("hover_result")));
    }

    // -- 10. Content hash determinism ----------------------------------------

    #[test]
    fn hash_content_is_deterministic() {
        let content = "theorem foo : 1 + 1 = 2 := by omega";
        let h1 = LspCache::hash_content(content);
        let h2 = LspCache::hash_content(content);
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_content_differs_for_different_input() {
        let h1 = LspCache::hash_content("theorem foo : True := trivial");
        let h2 = LspCache::hash_content("theorem bar : True := trivial");
        assert_ne!(h1, h2);
    }

    // -- 11. Expired entries purged on put -----------------------------------

    #[tokio::test]
    async fn expired_entries_purged_on_put() {
        let cache = short_ttl_cache(16); // 200 ms TTL
        let k1 = make_key(1, 1, 1, "goal");
        cache.put(k1.clone(), json!("old")).await;

        // Wait for k1 to expire.
        tokio::time::sleep(Duration::from_millis(250)).await;

        // Insert a new entry — the expired k1 should be purged.
        let k2 = make_key(2, 1, 1, "goal");
        cache.put(k2.clone(), json!("new")).await;

        assert_eq!(cache.len().await, 1);
        assert_eq!(cache.get(&k1).await, None);
        assert_eq!(cache.get(&k2).await, Some(json!("new")));
    }

    // -- 12. Concurrent reads and writes ------------------------------------

    #[tokio::test]
    async fn concurrent_reads_and_writes() {
        let cache = long_ttl_cache(256);
        let cache_ref = cache.clone();

        // Spawn many concurrent writers.
        let mut handles = Vec::new();
        for i in 0u64..50 {
            let c = cache_ref.clone();
            handles.push(tokio::spawn(async move {
                c.put(make_key(i, 1, 1, "goal"), json!(i)).await;
            }));
        }

        // Spawn concurrent readers.
        for i in 0u64..50 {
            let c = cache_ref.clone();
            handles.push(tokio::spawn(async move {
                // May or may not see the value depending on write ordering.
                let _ = c.get(&make_key(i, 1, 1, "goal")).await;
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        // All 50 writes should have landed (capacity is 256).
        assert_eq!(cache.len().await, 50);
    }

    // -- 13. Clone shares state ---------------------------------------------

    #[tokio::test]
    async fn clone_shares_underlying_state() {
        let cache = long_ttl_cache(16);
        let clone = cache.clone();

        let key = make_key(42, 1, 1, "hover");
        cache.put(key.clone(), json!("shared")).await;

        assert_eq!(clone.get(&key).await, Some(json!("shared")));
    }

    // -- 14. Overwrite does not count as extra entry -------------------------

    #[tokio::test]
    async fn overwrite_does_not_grow_size() {
        let cache = long_ttl_cache(2);
        let key = make_key(1, 1, 1, "goal");

        cache.put(key.clone(), json!("v1")).await;
        cache.put(key.clone(), json!("v2")).await;
        cache.put(key.clone(), json!("v3")).await;

        assert_eq!(cache.len().await, 1);
        assert_eq!(cache.get(&key).await, Some(json!("v3")));
    }

    // -- 15. Debug impl does not panic --------------------------------------

    #[test]
    fn debug_impl_works() {
        let cache = long_ttl_cache(16);
        let dbg = format!("{cache:?}");
        assert!(dbg.contains("LspCache"));
        assert!(dbg.contains("max_entries"));
    }

    // -- 16. Empty cache is_empty -------------------------------------------

    #[tokio::test]
    async fn new_cache_is_empty() {
        let cache = long_ttl_cache(16);
        assert!(cache.is_empty().await);
        assert_eq!(cache.len().await, 0);
    }
}
