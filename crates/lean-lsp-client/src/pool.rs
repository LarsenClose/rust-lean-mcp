//! Auto-scaling pool of LSP client instances for concurrent request dispatch.
//!
//! [`LspClientPool`] wraps multiple [`LspClient`] instances backed by separate
//! `lake serve` processes. It implements the [`LspClient`] trait itself, making
//! it transparent to callers — tool handlers don't know they're talking to a
//! pool.
//!
//! # Routing
//!
//! Requests are routed using **file-affinity with least-loaded fallback**:
//! 1. If a file has been opened on a specific instance, subsequent requests
//!    for that file prefer that instance (avoids re-elaboration cost).
//! 2. If the preferred instance is saturated, the least-loaded instance is
//!    chosen instead.
//!
//! # Auto-scaling
//!
//! The pool starts with a single instance and grows on demand when all
//! existing instances are busy. Growth is capped at `max_instances`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{RwLock, Semaphore};
use tracing::{debug, info, warn};

use crate::client::{LspClient, LspClientError};

/// In-flight request threshold above which an instance is considered saturated
/// for affinity routing purposes.
const SATURATION_THRESHOLD: usize = 2;

/// Type alias for the async spawner function that creates new LSP client instances.
pub type ClientSpawner = Box<
    dyn Fn()
            -> Pin<Box<dyn std::future::Future<Output = Result<Arc<dyn LspClient>, String>> + Send>>
        + Send
        + Sync,
>;

/// A single LSP client instance within the pool, with in-flight tracking.
struct PooledInstance {
    client: Arc<dyn LspClient>,
    in_flight: AtomicUsize,
}

/// RAII guard that decrements the in-flight counter when dropped.
struct InstanceGuard {
    instance: Arc<PooledInstance>,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        self.instance.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

impl InstanceGuard {
    fn client(&self) -> &dyn LspClient {
        self.instance.client.as_ref()
    }
}

/// Auto-scaling pool of LSP client instances.
///
/// Implements [`LspClient`] by routing each request to the best available
/// instance based on file affinity and load.
pub struct LspClientPool {
    project_path: PathBuf,
    instances: Arc<RwLock<Vec<Arc<PooledInstance>>>>,
    /// Maps relative file paths to the index of the instance that has them open.
    file_affinity: Arc<RwLock<HashMap<String, usize>>>,
    max_instances: usize,
    /// Semaphore with 1 permit — gates concurrent instance spawning.
    spawn_gate: Arc<Semaphore>,
    /// Factory function for creating new LSP client instances.
    spawner: Arc<ClientSpawner>,
}

impl std::fmt::Debug for LspClientPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspClientPool")
            .field("project_path", &self.project_path)
            .field("max_instances", &self.max_instances)
            .finish()
    }
}

impl LspClientPool {
    /// Create a new pool with a single initial instance.
    ///
    /// The `spawner` is called to create additional instances on demand.
    /// The `initial_client` is the first (already-connected) instance.
    pub fn new(
        project_path: PathBuf,
        initial_client: Arc<dyn LspClient>,
        max_instances: usize,
        spawner: ClientSpawner,
    ) -> Self {
        let instance = Arc::new(PooledInstance {
            client: initial_client,
            in_flight: AtomicUsize::new(0),
        });
        Self {
            project_path,
            instances: Arc::new(RwLock::new(vec![instance])),
            file_affinity: Arc::new(RwLock::new(HashMap::new())),
            max_instances: max_instances.max(1),
            spawn_gate: Arc::new(Semaphore::new(1)),
            spawner: Arc::new(spawner),
        }
    }

    /// Acquire an instance for a request, returning a guard that tracks in-flight count.
    ///
    /// Routing logic:
    /// 1. File affinity: prefer the instance that has the file open
    /// 2. Least-loaded fallback: pick the instance with fewest in-flight requests
    /// 3. Scale-up: if all instances are busy, spawn a new one (async, non-blocking)
    async fn acquire(&self, relative_path: Option<&str>) -> Result<InstanceGuard, LspClientError> {
        let instances = self.instances.read().await;

        // 1. File affinity check
        if let Some(path) = relative_path {
            let affinity = self.file_affinity.read().await;
            if let Some(&idx) = affinity.get(path) {
                if idx < instances.len() {
                    let inst = &instances[idx];
                    let load = inst.in_flight.load(Ordering::Relaxed);
                    if load < SATURATION_THRESHOLD {
                        inst.in_flight.fetch_add(1, Ordering::Relaxed);
                        debug!(
                            routing = "affinity_hit",
                            instance = idx,
                            load,
                            file = path,
                            "pool_acquire"
                        );
                        return Ok(InstanceGuard {
                            instance: inst.clone(),
                        });
                    }
                    debug!(
                        routing = "affinity_saturated",
                        instance = idx,
                        load,
                        file = path,
                        "pool_acquire"
                    );
                }
            }
        }

        // 2. Least-loaded
        let (idx, min_load) = instances
            .iter()
            .enumerate()
            .min_by_key(|(_, inst)| inst.in_flight.load(Ordering::Relaxed))
            .map(|(i, inst)| (i, inst.in_flight.load(Ordering::Relaxed)))
            .expect("pool must have at least one instance");

        debug!(
            routing = "least_loaded",
            instance = idx,
            load = min_load,
            pool_size = instances.len(),
            "pool_acquire"
        );

        instances[idx].in_flight.fetch_add(1, Ordering::Relaxed);
        let guard = InstanceGuard {
            instance: instances[idx].clone(),
        };

        let current_len = instances.len();
        drop(instances); // release read lock before potentially spawning

        // 3. Scale up in background if all instances are busy
        if min_load >= 1 && current_len < self.max_instances {
            let instances = Arc::clone(&self.instances);
            let spawn_gate = Arc::clone(&self.spawn_gate);
            let spawner = Arc::clone(&self.spawner);
            let max = self.max_instances;

            tokio::spawn(async move {
                Self::maybe_spawn_instance(instances, spawn_gate, spawner, max).await;
            });
        }

        Ok(guard)
    }

    /// Attempt to spawn a new instance if under the cap.
    ///
    /// Runs in a background task. Uses a semaphore so only one spawn happens
    /// at a time — if another spawn is in progress, returns immediately.
    async fn maybe_spawn_instance(
        instances: Arc<RwLock<Vec<Arc<PooledInstance>>>>,
        spawn_gate: Arc<Semaphore>,
        spawner: Arc<ClientSpawner>,
        max_instances: usize,
    ) {
        let permit = match spawn_gate.try_acquire() {
            Ok(p) => p,
            Err(_) => return, // another spawn in progress
        };

        // Double-check under permit
        let current_len = instances.read().await.len();
        if current_len >= max_instances {
            drop(permit);
            return;
        }

        info!(
            "Pool scaling up: spawning instance {} (max {})",
            current_len, max_instances
        );

        match spawner().await {
            Ok(client) => {
                let instance = Arc::new(PooledInstance {
                    client,
                    in_flight: AtomicUsize::new(0),
                });
                let mut inst_vec = instances.write().await;
                inst_vec.push(instance);
                info!("Pool now has {} instances", inst_vec.len());
            }
            Err(e) => {
                warn!("Failed to spawn new pool instance: {e}");
            }
        }

        drop(permit);
    }

    /// Update file affinity when a file is opened on an instance.
    async fn set_affinity(&self, relative_path: &str, instance: &Arc<PooledInstance>) {
        let instances = self.instances.read().await;
        if let Some(idx) = instances
            .iter()
            .position(|inst| Arc::ptr_eq(&inst.client, &instance.client))
        {
            let mut affinity = self.file_affinity.write().await;
            affinity.insert(relative_path.to_string(), idx);
            debug!(file = relative_path, instance = idx, "affinity set");
        }
    }

    /// Clear affinity for files associated with a given instance index.
    ///
    /// Used when an instance is removed from the pool (e.g., on failure).
    #[allow(dead_code)]
    async fn clear_affinity_for(&self, idx: usize) {
        let mut affinity = self.file_affinity.write().await;
        affinity.retain(|_, v| *v != idx);
    }

    // -- Pool stats for observability --

    /// Number of instances currently in the pool.
    pub async fn instance_count(&self) -> usize {
        self.instances.read().await.len()
    }

    /// Maximum number of instances allowed.
    pub fn max_instances(&self) -> usize {
        self.max_instances
    }

    /// Per-instance in-flight counts.
    pub async fn in_flight_counts(&self) -> Vec<usize> {
        self.instances
            .read()
            .await
            .iter()
            .map(|inst| inst.in_flight.load(Ordering::Relaxed))
            .collect()
    }

    /// Number of file-affinity entries tracked.
    pub async fn affinity_entry_count(&self) -> usize {
        self.file_affinity.read().await.len()
    }
}

#[async_trait]
impl LspClient for LspClientPool {
    fn project_path(&self) -> &Path {
        &self.project_path
    }

    async fn open_file(&self, relative_path: &str) -> Result<(), LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        let result = guard.client().open_file(relative_path).await;
        if result.is_ok() {
            self.set_affinity(relative_path, &guard.instance).await;
        }
        result
    }

    async fn open_file_force(&self, relative_path: &str) -> Result<(), LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        let result = guard.client().open_file_force(relative_path).await;
        if result.is_ok() {
            self.set_affinity(relative_path, &guard.instance).await;
        }
        result
    }

    async fn get_file_content(&self, relative_path: &str) -> Result<String, LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        guard.client().get_file_content(relative_path).await
    }

    async fn update_file(
        &self,
        relative_path: &str,
        changes: Vec<Value>,
    ) -> Result<(), LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        guard.client().update_file(relative_path, changes).await
    }

    async fn update_file_content(
        &self,
        relative_path: &str,
        content: &str,
    ) -> Result<(), LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        guard
            .client()
            .update_file_content(relative_path, content)
            .await
    }

    async fn close_files(&self, paths: &[String]) -> Result<(), LspClientError> {
        // Group paths by affinity to minimize cross-instance calls.
        // For simplicity, route to the first path's affinity or least-loaded.
        let first_path = paths.first().map(|s| s.as_str());
        let guard = self.acquire(first_path).await?;
        let result = guard.client().close_files(paths).await;
        if result.is_ok() {
            let mut affinity = self.file_affinity.write().await;
            for path in paths {
                affinity.remove(path);
            }
        }
        result
    }

    async fn get_diagnostics(
        &self,
        relative_path: &str,
        start_line: Option<u32>,
        end_line: Option<u32>,
        inactivity_timeout: Option<f64>,
    ) -> Result<Value, LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        guard
            .client()
            .get_diagnostics(relative_path, start_line, end_line, inactivity_timeout)
            .await
    }

    async fn get_interactive_diagnostics(
        &self,
        relative_path: &str,
        start_line: Option<u32>,
        end_line: Option<u32>,
    ) -> Result<Vec<Value>, LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        guard
            .client()
            .get_interactive_diagnostics(relative_path, start_line, end_line)
            .await
    }

    async fn get_goal(
        &self,
        relative_path: &str,
        line: u32,
        column: u32,
    ) -> Result<Option<Value>, LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        guard.client().get_goal(relative_path, line, column).await
    }

    async fn get_term_goal(
        &self,
        relative_path: &str,
        line: u32,
        column: u32,
    ) -> Result<Option<Value>, LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        guard
            .client()
            .get_term_goal(relative_path, line, column)
            .await
    }

    async fn get_hover(
        &self,
        relative_path: &str,
        line: u32,
        column: u32,
    ) -> Result<Option<Value>, LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        guard.client().get_hover(relative_path, line, column).await
    }

    async fn get_completions(
        &self,
        relative_path: &str,
        line: u32,
        column: u32,
    ) -> Result<Vec<Value>, LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        guard
            .client()
            .get_completions(relative_path, line, column)
            .await
    }

    async fn get_declarations(
        &self,
        relative_path: &str,
        line: u32,
        column: u32,
    ) -> Result<Vec<Value>, LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        guard
            .client()
            .get_declarations(relative_path, line, column)
            .await
    }

    async fn get_references(
        &self,
        relative_path: &str,
        line: u32,
        column: u32,
        include_declaration: bool,
    ) -> Result<Vec<Value>, LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        guard
            .client()
            .get_references(relative_path, line, column, include_declaration)
            .await
    }

    async fn get_document_symbols(
        &self,
        relative_path: &str,
    ) -> Result<Vec<Value>, LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        guard.client().get_document_symbols(relative_path).await
    }

    async fn get_code_actions(
        &self,
        relative_path: &str,
        start_line: u32,
        start_col: u32,
        end_line: u32,
        end_col: u32,
    ) -> Result<Vec<Value>, LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        guard
            .client()
            .get_code_actions(relative_path, start_line, start_col, end_line, end_col)
            .await
    }

    async fn get_code_action_resolve(&self, action: Value) -> Result<Value, LspClientError> {
        // Code action resolve has no file path context — use least-loaded.
        let guard = self.acquire(None).await?;
        guard.client().get_code_action_resolve(action).await
    }

    async fn get_widgets(
        &self,
        relative_path: &str,
        line: u32,
        column: u32,
    ) -> Result<Vec<Value>, LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        guard
            .client()
            .get_widgets(relative_path, line, column)
            .await
    }

    async fn get_widget_source(
        &self,
        relative_path: &str,
        line: u32,
        column: u32,
        javascript_hash: &str,
    ) -> Result<Value, LspClientError> {
        let guard = self.acquire(Some(relative_path)).await?;
        guard
            .client()
            .get_widget_source(relative_path, line, column, javascript_hash)
            .await
    }

    async fn shutdown(&self) -> Result<(), LspClientError> {
        let instances = self.instances.read().await;
        info!("Shutting down pool with {} instances", instances.len());
        for (i, inst) in instances.iter().enumerate() {
            if let Err(e) = inst.client.shutdown().await {
                warn!("Failed to shut down pool instance {i}: {e}");
            }
        }
        // Clear affinity
        self.file_affinity.write().await.clear();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::MockLspClient;

    /// Helper: create a mock client with a project path and open_file expectation.
    fn mock_client(project: &str) -> Arc<dyn LspClient> {
        let mut mock = MockLspClient::new();
        let p = PathBuf::from(project);
        mock.expect_project_path().return_const(p);
        Arc::new(mock) as Arc<dyn LspClient>
    }

    #[tokio::test]
    async fn pool_starts_with_one_instance() {
        let project = PathBuf::from("/test");
        let client = mock_client("/test");
        let spawner: ClientSpawner = Box::new(|| Box::pin(async { Ok(mock_client("/test")) }));
        let pool = LspClientPool::new(project, client, 4, spawner);

        assert_eq!(pool.instance_count().await, 1);
        assert_eq!(pool.max_instances(), 4);
    }

    #[tokio::test]
    async fn acquire_returns_guard_and_tracks_in_flight() {
        let project = PathBuf::from("/test");
        let client = mock_client("/test");
        let spawner: ClientSpawner = Box::new(|| Box::pin(async { Ok(mock_client("/test")) }));
        let pool = LspClientPool::new(project, client, 1, spawner);

        let guard = pool.acquire(None).await.unwrap();
        assert_eq!(pool.in_flight_counts().await, vec![1]);

        drop(guard);
        assert_eq!(pool.in_flight_counts().await, vec![0]);
    }

    #[tokio::test]
    async fn affinity_routes_to_preferred_instance() {
        let project = PathBuf::from("/test");
        let client = mock_client("/test");
        let spawner: ClientSpawner = Box::new(|| Box::pin(async { Ok(mock_client("/test")) }));
        let pool = LspClientPool::new(project, client, 4, spawner);

        // Manually add a second instance
        {
            let mut instances = pool.instances.write().await;
            instances.push(Arc::new(PooledInstance {
                client: mock_client("/test"),
                in_flight: AtomicUsize::new(0),
            }));
        }

        // Set affinity for "Foo.lean" → instance 1
        {
            let mut affinity = pool.file_affinity.write().await;
            affinity.insert("Foo.lean".to_string(), 1);
        }

        // Acquire for "Foo.lean" — should go to instance 1
        let guard = pool.acquire(Some("Foo.lean")).await.unwrap();
        let counts = pool.in_flight_counts().await;
        assert_eq!(counts[0], 0, "instance 0 should be idle");
        assert_eq!(counts[1], 1, "instance 1 should have the request");
        drop(guard);
    }

    #[tokio::test]
    async fn saturated_affinity_falls_back_to_least_loaded() {
        let project = PathBuf::from("/test");
        let client = mock_client("/test");
        let spawner: ClientSpawner = Box::new(|| Box::pin(async { Ok(mock_client("/test")) }));
        let pool = LspClientPool::new(project, client, 4, spawner);

        // Add second instance
        {
            let mut instances = pool.instances.write().await;
            instances.push(Arc::new(PooledInstance {
                client: mock_client("/test"),
                in_flight: AtomicUsize::new(0),
            }));
        }

        // Set affinity for "Foo.lean" → instance 0
        {
            let mut affinity = pool.file_affinity.write().await;
            affinity.insert("Foo.lean".to_string(), 0);
        }

        // Saturate instance 0 beyond threshold
        {
            let instances = pool.instances.read().await;
            for _ in 0..SATURATION_THRESHOLD {
                instances[0].in_flight.fetch_add(1, Ordering::Relaxed);
            }
        }

        // Acquire for "Foo.lean" — should fall back to instance 1 (least loaded)
        let guard = pool.acquire(Some("Foo.lean")).await.unwrap();
        let counts = pool.in_flight_counts().await;
        assert_eq!(
            counts[1], 1,
            "should fall back to instance 1 when 0 is saturated"
        );
        drop(guard);

        // Clean up
        {
            let instances = pool.instances.read().await;
            instances[0].in_flight.store(0, Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn auto_scales_when_all_busy() {
        let project = PathBuf::from("/test");
        let client = mock_client("/test");
        let spawn_count = Arc::new(AtomicUsize::new(0));
        let sc = spawn_count.clone();
        let spawner: ClientSpawner = Box::new(move || {
            sc.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(mock_client("/test")) })
        });
        let pool = LspClientPool::new(project, client, 3, spawner);

        assert_eq!(pool.instance_count().await, 1);

        // Acquire one request — instance 0 is now busy
        let _guard1 = pool.acquire(None).await.unwrap();

        // Acquire another — should trigger scale-up since all instances busy
        let _guard2 = pool.acquire(None).await.unwrap();

        // Give the spawn a moment to complete
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        assert_eq!(
            spawn_count.load(Ordering::Relaxed),
            1,
            "should have spawned once"
        );
        assert_eq!(pool.instance_count().await, 2);
    }

    #[tokio::test]
    async fn does_not_exceed_max_instances() {
        let project = PathBuf::from("/test");
        let client = mock_client("/test");
        let spawner: ClientSpawner = Box::new(|| Box::pin(async { Ok(mock_client("/test")) }));
        let pool = LspClientPool::new(project, client, 2, spawner);

        // Manually add instance to reach max
        {
            let mut instances = pool.instances.write().await;
            instances.push(Arc::new(PooledInstance {
                client: mock_client("/test"),
                in_flight: AtomicUsize::new(0),
            }));
        }
        assert_eq!(pool.instance_count().await, 2);

        // Make all busy and try to acquire
        {
            let instances = pool.instances.read().await;
            for inst in instances.iter() {
                inst.in_flight.fetch_add(1, Ordering::Relaxed);
            }
        }

        let _guard = pool.acquire(None).await.unwrap();

        // Should NOT have grown past max
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert_eq!(pool.instance_count().await, 2, "should not exceed max");
    }

    #[tokio::test]
    async fn close_files_clears_affinity() {
        let project = PathBuf::from("/test");
        let mut client = MockLspClient::new();
        client
            .expect_project_path()
            .return_const(PathBuf::from("/test"));
        client.expect_close_files().returning(|_| Ok(()));
        let client: Arc<dyn LspClient> = Arc::new(client);
        let spawner: ClientSpawner = Box::new(|| Box::pin(async { Ok(mock_client("/test")) }));
        let pool = LspClientPool::new(project, client, 4, spawner);

        // Set affinity
        {
            let mut affinity = pool.file_affinity.write().await;
            affinity.insert("A.lean".to_string(), 0);
            affinity.insert("B.lean".to_string(), 0);
        }

        pool.close_files(&["A.lean".to_string()]).await.unwrap();

        let affinity = pool.file_affinity.read().await;
        assert!(
            !affinity.contains_key("A.lean"),
            "A.lean affinity should be cleared"
        );
        assert!(
            affinity.contains_key("B.lean"),
            "B.lean affinity should remain"
        );
    }

    #[tokio::test]
    async fn shutdown_shuts_down_all_instances() {
        let project = PathBuf::from("/test");
        let shutdown_count = Arc::new(AtomicUsize::new(0));

        let sc = shutdown_count.clone();
        let mut client = MockLspClient::new();
        client
            .expect_project_path()
            .return_const(PathBuf::from("/test"));
        let sc2 = sc.clone();
        client.expect_shutdown().returning(move || {
            sc2.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        let client: Arc<dyn LspClient> = Arc::new(client);

        let sc3 = sc.clone();
        let spawner: ClientSpawner = Box::new(move || {
            let sc4 = sc3.clone();
            Box::pin(async move {
                let mut mock = MockLspClient::new();
                mock.expect_project_path()
                    .return_const(PathBuf::from("/test"));
                let sc5 = sc4.clone();
                mock.expect_shutdown().returning(move || {
                    sc5.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                });
                Ok(Arc::new(mock) as Arc<dyn LspClient>)
            })
        });

        let pool = LspClientPool::new(project, client, 4, spawner);

        // Add a second instance via spawn
        LspClientPool::maybe_spawn_instance(
            Arc::clone(&pool.instances),
            Arc::clone(&pool.spawn_gate),
            Arc::clone(&pool.spawner),
            pool.max_instances,
        )
        .await;
        assert_eq!(pool.instance_count().await, 2);

        pool.shutdown().await.unwrap();
        assert_eq!(shutdown_count.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn affinity_entry_count_tracks_correctly() {
        let project = PathBuf::from("/test");
        let client = mock_client("/test");
        let spawner: ClientSpawner = Box::new(|| Box::pin(async { Ok(mock_client("/test")) }));
        let pool = LspClientPool::new(project, client, 4, spawner);

        assert_eq!(pool.affinity_entry_count().await, 0);

        {
            let mut affinity = pool.file_affinity.write().await;
            affinity.insert("A.lean".to_string(), 0);
            affinity.insert("B.lean".to_string(), 0);
        }

        assert_eq!(pool.affinity_entry_count().await, 2);
    }

    #[tokio::test]
    async fn spawn_failure_does_not_crash_pool() {
        let project = PathBuf::from("/test");
        let client = mock_client("/test");
        let spawner: ClientSpawner =
            Box::new(|| Box::pin(async { Err("lake serve failed to start".to_string()) }));
        let pool = LspClientPool::new(project, client, 4, spawner);

        // Make instance busy to trigger scale-up attempt
        {
            let instances = pool.instances.read().await;
            instances[0].in_flight.fetch_add(1, Ordering::Relaxed);
        }

        // This triggers a scale-up that will fail — should not panic
        let _guard = pool.acquire(None).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Pool should still have just 1 instance
        assert_eq!(pool.instance_count().await, 1);
    }

    #[tokio::test]
    async fn least_loaded_distributes_evenly() {
        let project = PathBuf::from("/test");
        let client = mock_client("/test");
        let spawner: ClientSpawner = Box::new(|| Box::pin(async { Ok(mock_client("/test")) }));
        let pool = LspClientPool::new(project, client, 4, spawner);

        // Add two more instances (3 total)
        {
            let mut instances = pool.instances.write().await;
            for _ in 0..2 {
                instances.push(Arc::new(PooledInstance {
                    client: mock_client("/test"),
                    in_flight: AtomicUsize::new(0),
                }));
            }
        }

        // Acquire 3 requests without file affinity — should spread across instances
        let g1 = pool.acquire(None).await.unwrap();
        let g2 = pool.acquire(None).await.unwrap();
        let g3 = pool.acquire(None).await.unwrap();

        let counts = pool.in_flight_counts().await;
        assert_eq!(counts.iter().sum::<usize>(), 3);
        // Each should have exactly 1 (least-loaded picks idle ones)
        assert!(
            counts.iter().all(|&c| c == 1),
            "expected even distribution, got {:?}",
            counts
        );

        drop(g1);
        drop(g2);
        drop(g3);
    }
}
