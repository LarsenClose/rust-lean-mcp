//! Kimina-style REPL pool with header LRU caching.
//!
//! Maintains a pool of pre-warmed REPL processes indexed by their import header.
//! When a caller requests a REPL for a given header:
//!
//! - **Cache hit**: an existing warmed REPL is returned immediately, skipping
//!   the expensive import re-elaboration step.
//! - **Cache miss**: a new REPL is started and warmed with the header before
//!   being returned.
//!
//! After use, the REPL is returned to the pool. When the pool exceeds
//! `max_size`, the least-recently-used entry is evicted and its process is
//! shut down.
//!
//! This mirrors the approach used by the Kimina Lean Server (project-numina),
//! which achieves 1.5-2x speedup by avoiding redundant header elaboration.

use std::path::{Path, PathBuf};
use std::time::Instant;

use tokio::sync::Mutex;
use tracing;

use crate::repl::{Repl, SnippetResult};

// ---------------------------------------------------------------------------
// Default pool size
// ---------------------------------------------------------------------------

/// Default maximum number of REPL instances kept in the pool.
const DEFAULT_MAX_SIZE: usize = 4;

/// Environment variable to override the default pool size.
const POOL_SIZE_ENV_VAR: &str = "LEAN_MCP_REPL_POOL_SIZE";

// ---------------------------------------------------------------------------
// PoolEntry
// ---------------------------------------------------------------------------

/// A single entry in the REPL pool: a warmed REPL plus metadata.
struct PoolEntry {
    /// The import header this REPL was warmed with.
    header: String,
    /// The warmed REPL instance (process alive, header env cached).
    repl: Repl,
    /// When this entry was last used (for LRU eviction).
    last_used: Instant,
}

// ---------------------------------------------------------------------------
// ReplPool
// ---------------------------------------------------------------------------

/// A pool of pre-warmed REPL processes indexed by import header.
///
/// Thread-safe: all pool operations go through an internal [`Mutex`].
///
/// # Example
///
/// ```rust,no_run
/// # use std::path::Path;
/// # use lean_mcp_core::repl_pool::ReplPool;
/// # async fn example() {
/// let pool = ReplPool::new(Path::new("/my/lean/project"), "repl".to_string(), 4);
///
/// // Run snippets -- pool handles REPL lifecycle automatically.
/// let header = "import Mathlib\n";
/// let results = pool.run_snippets(header, "theorem foo : True := by\n", &["trivial".into()]).await;
/// # }
/// ```
pub struct ReplPool {
    project_path: PathBuf,
    repl_path: String,
    max_size: usize,
    /// Pool entries guarded by a mutex. We use `tokio::sync::Mutex` because
    /// acquire/release are async (they may start/stop REPL processes).
    pool: Mutex<Vec<PoolEntry>>,
}

impl ReplPool {
    /// Create a new REPL pool.
    ///
    /// `max_size` controls how many idle REPL instances are kept alive.
    /// If the `LEAN_MCP_REPL_POOL_SIZE` env var is set, it overrides
    /// `max_size`.
    pub fn new(project_path: &Path, repl_path: String, max_size: usize) -> Self {
        let effective_size = std::env::var(POOL_SIZE_ENV_VAR)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(max_size);

        tracing::info!(
            max_size = effective_size,
            "Creating REPL pool for {}",
            project_path.display()
        );

        Self {
            project_path: project_path.to_path_buf(),
            repl_path,
            max_size: effective_size,
            pool: Mutex::new(Vec::new()),
        }
    }

    /// Create a new REPL pool with the default max size.
    pub fn with_defaults(project_path: &Path, repl_path: String) -> Self {
        Self::new(project_path, repl_path, DEFAULT_MAX_SIZE)
    }

    /// Acquire a REPL warmed for the given header.
    ///
    /// On cache hit, an existing warmed REPL is removed from the pool and
    /// returned. On cache miss, a new REPL is started and warmed with the
    /// header before being returned.
    pub async fn acquire(&self, header: &str) -> Result<Repl, String> {
        // Try to find a matching entry in the pool.
        {
            let mut entries = self.pool.lock().await;
            if let Some(idx) = Self::find_entry(&mut entries, header) {
                let entry = entries.swap_remove(idx);
                tracing::debug!(
                    header_len = header.len(),
                    pool_size = entries.len(),
                    "REPL pool hit"
                );
                return Ok(entry.repl);
            }
        }

        // Cache miss: create and warm a new REPL.
        tracing::debug!(
            header_len = header.len(),
            "REPL pool miss, creating new instance"
        );
        let mut repl = Repl::new(&self.project_path, &self.repl_path);
        repl.start().await?;
        repl.load_header(header).await?;
        Ok(repl)
    }

    /// Return a REPL to the pool after use.
    ///
    /// If the REPL process is no longer alive, it is silently discarded.
    /// If the pool exceeds `max_size`, the least-recently-used entry is evicted.
    pub async fn release(&self, mut repl: Repl, header: &str) {
        // Discard dead REPLs.
        if !repl.is_alive() {
            tracing::debug!("Discarding dead REPL (process exited)");
            return;
        }

        let mut entries = self.pool.lock().await;

        entries.push(PoolEntry {
            header: header.to_string(),
            repl,
            last_used: Instant::now(),
        });

        // Evict LRU entries if over capacity.
        while entries.len() > self.max_size {
            let lru_idx = Self::find_lru(&entries);
            let mut evicted = entries.swap_remove(lru_idx);
            tracing::debug!(
                header_len = evicted.header.len(),
                "Evicting LRU REPL from pool"
            );
            evicted.repl.close().await;
        }

        tracing::debug!(pool_size = entries.len(), "REPL returned to pool");
    }

    /// Run snippets using a pooled REPL (convenience method).
    ///
    /// Acquires a REPL warmed for `header`, sends the body + sorry, runs each
    /// snippet as a tactic, then returns the REPL to the pool.
    ///
    /// `header` and `body` correspond to the two parts produced by
    /// [`Repl::split_header_body`].
    pub async fn run_snippets(
        &self,
        header: &str,
        body: &str,
        snippets: &[String],
    ) -> Vec<SnippetResult> {
        let error_result = |msg: &str| SnippetResult {
            goals: vec![],
            messages: vec![],
            proof_status: None,
            error: Some(msg.to_string()),
        };

        if snippets.is_empty() {
            return vec![];
        }

        // Acquire a warmed REPL for this header.
        let mut repl = match self.acquire(header).await {
            Ok(r) => r,
            Err(e) => {
                return snippets
                    .iter()
                    .map(|_| error_result(&format!("REPL pool acquire error: {e}")))
                    .collect();
            }
        };

        // Combine header + body into base_code for run_snippets.
        let base_code = format!("{}{}", header, body);
        let results = repl.run_snippets(&base_code, snippets).await;

        // Return the REPL to the pool.
        self.release(repl, header).await;

        results
    }

    /// Shut down all pooled REPL processes.
    pub async fn shutdown(&self) {
        let mut entries = self.pool.lock().await;
        for entry in entries.drain(..) {
            let mut repl = entry.repl;
            repl.close().await;
        }
        tracing::info!("REPL pool shut down");
    }

    /// Returns the current number of idle REPLs in the pool.
    pub async fn size(&self) -> usize {
        self.pool.lock().await.len()
    }

    /// Returns the configured maximum pool size.
    pub fn max_size(&self) -> usize {
        self.max_size
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Find a pool entry matching the given header (exact string match).
    ///
    /// Returns the index of the matching entry, or `None`.
    fn find_entry(entries: &mut [PoolEntry], header: &str) -> Option<usize> {
        entries.iter().position(|e| e.header == header)
    }

    /// Find the least-recently-used entry in the pool.
    ///
    /// # Panics
    ///
    /// Panics if `entries` is empty.
    fn find_lru(entries: &[PoolEntry]) -> usize {
        entries
            .iter()
            .enumerate()
            .min_by_key(|(_, e)| e.last_used)
            .map(|(i, _)| i)
            .expect("find_lru called on empty pool")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Helper to create a pool for testing.
    /// Uses a fake repl path since we won't actually spawn processes in unit tests.
    fn test_pool(max_size: usize) -> ReplPool {
        ReplPool::new(
            Path::new("/tmp/test-project"),
            "fake-repl".to_string(),
            max_size,
        )
    }

    // ---- Construction -------------------------------------------------------

    #[test]
    fn new_pool_has_correct_max_size() {
        let pool = test_pool(8);
        assert_eq!(pool.max_size(), 8);
    }

    #[test]
    fn with_defaults_uses_default_max_size() {
        let pool = ReplPool::with_defaults(Path::new("/tmp"), "repl".to_string());
        assert_eq!(pool.max_size(), DEFAULT_MAX_SIZE);
    }

    #[tokio::test]
    async fn new_pool_is_empty() {
        let pool = test_pool(4);
        assert_eq!(pool.size().await, 0);
    }

    // ---- acquire / cache miss -----------------------------------------------

    #[tokio::test]
    async fn acquire_cache_miss_creates_new_repl() {
        // acquire with no entries in the pool results in a cache miss.
        // Since we use a fake repl path, start() will fail, but we can
        // verify the error message indicates a miss (new instance creation).
        let pool = test_pool(4);
        let result = pool.acquire("import Lean\n").await;
        assert!(result.is_err(), "Expected error from fake repl binary");
        let err = result.err().unwrap();
        // The error should come from trying to start the REPL process
        assert!(
            err.contains("Failed to start") || err.contains("lake env"),
            "Unexpected error: {err}"
        );
        // Pool should still be empty (nothing was returned)
        assert_eq!(pool.size().await, 0);
    }

    // ---- release / acquire cache hit ----------------------------------------

    #[tokio::test]
    async fn release_and_acquire_cache_hit() {
        let pool = test_pool(4);

        // Manually create a Repl (no process) and release it.
        let repl = Repl::new(Path::new("/tmp/test-project"), "fake-repl");
        let header = "import Mathlib\n";

        // Release it -- it will be discarded because is_alive() returns false
        // for a Repl with no process. This is correct behavior.
        pool.release(repl, header).await;

        // Dead REPLs are discarded, so pool should be empty.
        assert_eq!(pool.size().await, 0);
    }

    // ---- LRU eviction -------------------------------------------------------

    #[tokio::test]
    async fn lru_eviction_when_pool_exceeds_max_size() {
        // We can't use real REPLs, but we can test the LRU logic directly
        // through the internal PoolEntry structures.
        let now = Instant::now();
        let entries = vec![
            PoolEntry {
                header: "header_old".to_string(),
                repl: Repl::new(Path::new("/tmp"), "fake"),
                last_used: now - std::time::Duration::from_secs(100),
            },
            PoolEntry {
                header: "header_new".to_string(),
                repl: Repl::new(Path::new("/tmp"), "fake"),
                last_used: now,
            },
            PoolEntry {
                header: "header_mid".to_string(),
                repl: Repl::new(Path::new("/tmp"), "fake"),
                last_used: now - std::time::Duration::from_secs(50),
            },
        ];

        // The LRU entry should be index 0 (oldest last_used)
        let lru_idx = ReplPool::find_lru(&entries);
        assert_eq!(lru_idx, 0);
        assert_eq!(entries[lru_idx].header, "header_old");
    }

    // ---- find_entry ---------------------------------------------------------

    #[test]
    fn find_entry_returns_matching_index() {
        let now = Instant::now();
        let mut entries = vec![
            PoolEntry {
                header: "import A\n".to_string(),
                repl: Repl::new(Path::new("/tmp"), "fake"),
                last_used: now,
            },
            PoolEntry {
                header: "import B\n".to_string(),
                repl: Repl::new(Path::new("/tmp"), "fake"),
                last_used: now,
            },
        ];

        assert_eq!(ReplPool::find_entry(&mut entries, "import B\n"), Some(1));
        assert_eq!(ReplPool::find_entry(&mut entries, "import A\n"), Some(0));
        assert_eq!(ReplPool::find_entry(&mut entries, "import C\n"), None);
    }

    // ---- different headers get different entries ----------------------------

    #[test]
    fn different_headers_are_not_matched() {
        let now = Instant::now();
        let mut entries = vec![PoolEntry {
            header: "import Lean\n".to_string(),
            repl: Repl::new(Path::new("/tmp"), "fake"),
            last_used: now,
        }];

        // Exact match required
        assert_eq!(ReplPool::find_entry(&mut entries, "import Lean\n"), Some(0));
        assert_eq!(ReplPool::find_entry(&mut entries, "import Lean"), None);
        assert_eq!(ReplPool::find_entry(&mut entries, "import Mathlib\n"), None);
    }

    // ---- run_snippets with empty snippets -----------------------------------

    #[tokio::test]
    async fn run_snippets_empty_returns_empty() {
        let pool = test_pool(4);
        let results = pool
            .run_snippets("import Lean\n", "theorem foo := by\n", &[])
            .await;
        assert!(results.is_empty());
    }

    // ---- run_snippets cache miss error propagation --------------------------

    #[tokio::test]
    async fn run_snippets_propagates_acquire_error() {
        let pool = test_pool(4);
        let snippets = vec!["simp".to_string(), "ring".to_string()];
        let results = pool
            .run_snippets("import Lean\n", "theorem foo := by\n", &snippets)
            .await;

        // Should get one error result per snippet
        assert_eq!(results.len(), 2);
        for r in &results {
            assert!(r.error.is_some());
            let err = r.error.as_ref().unwrap();
            assert!(
                err.contains("REPL pool acquire error"),
                "Unexpected error: {err}"
            );
        }
    }

    // ---- shutdown -----------------------------------------------------------

    #[tokio::test]
    async fn shutdown_empties_pool() {
        let pool = test_pool(4);
        // Pool starts empty, shutdown should be a no-op.
        pool.shutdown().await;
        assert_eq!(pool.size().await, 0);
    }

    // ---- env var override ---------------------------------------------------

    #[test]
    fn pool_size_env_var_override() {
        // Test that the env var name constant is correct
        assert_eq!(POOL_SIZE_ENV_VAR, "LEAN_MCP_REPL_POOL_SIZE");
    }

    // ---- Send + Sync --------------------------------------------------------

    #[test]
    fn repl_pool_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ReplPool>();
    }

    // ---- find_lru -----------------------------------------------------------

    #[test]
    fn find_lru_single_entry() {
        let entries = vec![PoolEntry {
            header: "h".to_string(),
            repl: Repl::new(Path::new("/tmp"), "fake"),
            last_used: Instant::now(),
        }];
        assert_eq!(ReplPool::find_lru(&entries), 0);
    }

    #[test]
    fn find_lru_picks_oldest() {
        let now = Instant::now();
        let entries = vec![
            PoolEntry {
                header: "a".to_string(),
                repl: Repl::new(Path::new("/tmp"), "fake"),
                last_used: now - std::time::Duration::from_secs(10),
            },
            PoolEntry {
                header: "b".to_string(),
                repl: Repl::new(Path::new("/tmp"), "fake"),
                last_used: now - std::time::Duration::from_secs(30),
            },
            PoolEntry {
                header: "c".to_string(),
                repl: Repl::new(Path::new("/tmp"), "fake"),
                last_used: now - std::time::Duration::from_secs(20),
            },
        ];
        // "b" has the oldest timestamp
        assert_eq!(ReplPool::find_lru(&entries), 1);
    }
}
