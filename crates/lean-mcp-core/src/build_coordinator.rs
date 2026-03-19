//! Build coordinator managing concurrent build requests.
//!
//! Ports the Python `BuildCoordinator` class from `server.py:227-265`.
//! Three concurrency modes control how overlapping build requests interact:
//!
//! | Mode     | Behaviour                                                       |
//! |----------|-----------------------------------------------------------------|
//! | `Allow`  | Concurrent builds proceed independently.                        |
//! | `Cancel` | New build cancels in-progress build; cancelled returns error.   |
//! | `Share`  | New build cancels old; both callers receive the new result.     |

use std::sync::Arc;

use tokio::sync::{watch, Mutex};

use crate::config::BuildConcurrencyMode;
use crate::models::BuildResult;

/// Manages concurrent build requests according to a [`BuildConcurrencyMode`].
pub struct BuildCoordinator {
    mode: BuildConcurrencyMode,
    /// Holds a sender for the current in-flight build result (Cancel/Share modes).
    /// When a new build arrives, the old sender is used to signal supersession.
    inner: Arc<Mutex<Option<CoordinatorState>>>,
}

/// Internal state tracking the current in-flight build.
struct CoordinatorState {
    /// Sender that the in-flight build will use to broadcast its result.
    result_tx: watch::Sender<Option<BuildResult>>,
    /// Handle to the spawned build task, used for cancellation.
    task_handle: tokio::task::JoinHandle<()>,
}

/// The superseded-build error returned when a build is cancelled by a newer request.
fn superseded_result() -> BuildResult {
    BuildResult {
        success: false,
        output: String::new(),
        errors: vec!["Build superseded by newer request.".to_string()],
    }
}

impl BuildCoordinator {
    /// Create a new coordinator with the given concurrency mode.
    pub fn new(mode: BuildConcurrencyMode) -> Self {
        Self {
            mode,
            inner: Arc::new(Mutex::new(None)),
        }
    }

    /// Run a build. The behaviour depends on the configured mode:
    ///
    /// - **Allow**: runs the factory directly, no coordination.
    /// - **Cancel**: if a build is in-progress, aborts it (it returns a
    ///   "superseded" error) and starts the new build.
    /// - **Share**: like Cancel, but the old caller waits for and receives
    ///   the new build's result instead of an error.
    pub async fn run<F, Fut>(&self, build_factory: F) -> BuildResult
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = BuildResult> + Send + 'static,
    {
        match self.mode {
            BuildConcurrencyMode::Allow => build_factory().await,
            BuildConcurrencyMode::Cancel => self.run_cancel(build_factory).await,
            BuildConcurrencyMode::Share => self.run_share(build_factory).await,
        }
    }

    /// Cancel mode: abort any in-progress build, then run the new one.
    async fn run_cancel<F, Fut>(&self, build_factory: F) -> BuildResult
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = BuildResult> + Send + 'static,
    {
        let (result_tx, result_rx) = watch::channel(None);

        // Abort previous build if any.
        {
            let mut guard = self.inner.lock().await;
            if let Some(prev) = guard.take() {
                // Signal the superseded error to any waiters on the old channel,
                // then abort the task.
                let _ = prev.result_tx.send(Some(superseded_result()));
                prev.task_handle.abort();
            }

            let fut = build_factory();
            let tx = result_tx.clone();
            let task_handle = tokio::spawn(async move {
                let result = fut.await;
                let _ = tx.send(Some(result));
            });

            *guard = Some(CoordinatorState {
                result_tx,
                task_handle,
            });
        }

        // Wait for this build's result.
        let mut rx = result_rx;
        // Wait until the value becomes Some.
        loop {
            if let Some(result) = rx.borrow_and_update().clone() {
                return result;
            }
            if rx.changed().await.is_err() {
                // Sender dropped without sending a result — shouldn't happen,
                // but return an error to be safe.
                return BuildResult {
                    success: false,
                    output: String::new(),
                    errors: vec!["Build coordinator internal error.".to_string()],
                };
            }
        }
    }

    /// Share mode: abort any in-progress build; both old and new callers
    /// receive the new build's result.
    async fn run_share<F, Fut>(&self, build_factory: F) -> BuildResult
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = BuildResult> + Send + 'static,
    {
        let (result_tx, result_rx) = watch::channel(None);

        {
            let mut guard = self.inner.lock().await;
            if let Some(prev) = guard.take() {
                // In share mode, redirect old waiters to the new channel
                // by sending a sentinel. We don't send a superseded error;
                // instead the old sender gets a "redirect" marker and old
                // callers will receive the new result.
                //
                // We achieve sharing by sending the new result through
                // the *old* sender too. The spawned task will broadcast
                // to both.
                prev.task_handle.abort();

                let fut = build_factory();
                let new_tx = result_tx.clone();
                let old_tx = prev.result_tx;
                let task_handle = tokio::spawn(async move {
                    let result = fut.await;
                    let _ = new_tx.send(Some(result.clone()));
                    let _ = old_tx.send(Some(result));
                });

                *guard = Some(CoordinatorState {
                    result_tx,
                    task_handle,
                });
            } else {
                // No previous build — just start a new one.
                let fut = build_factory();
                let tx = result_tx.clone();
                let task_handle = tokio::spawn(async move {
                    let result = fut.await;
                    let _ = tx.send(Some(result));
                });

                *guard = Some(CoordinatorState {
                    result_tx,
                    task_handle,
                });
            }
        }

        // Wait for the result.
        let mut rx = result_rx;
        loop {
            if let Some(result) = rx.borrow_and_update().clone() {
                return result;
            }
            if rx.changed().await.is_err() {
                return BuildResult {
                    success: false,
                    output: String::new(),
                    errors: vec!["Build coordinator internal error.".to_string()],
                };
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;
    use tokio::sync::Barrier;

    /// Helper: a build factory that completes immediately with a success result.
    fn immediate_success(output: &str) -> BuildResult {
        BuildResult {
            success: true,
            output: output.to_string(),
            errors: vec![],
        }
    }

    // ---- Allow mode --------------------------------------------------------

    #[tokio::test]
    async fn allow_mode_runs_build_directly() {
        let coord = BuildCoordinator::new(BuildConcurrencyMode::Allow);
        let result = coord.run(|| async { immediate_success("build-1") }).await;
        assert!(result.success);
        assert_eq!(result.output, "build-1");
    }

    #[tokio::test]
    async fn allow_mode_runs_builds_independently() {
        let coord = Arc::new(BuildCoordinator::new(BuildConcurrencyMode::Allow));
        let counter = Arc::new(AtomicU32::new(0));

        let mut handles = vec![];
        for i in 0..3 {
            let coord = Arc::clone(&coord);
            let counter = Arc::clone(&counter);
            handles.push(tokio::spawn(async move {
                coord
                    .run(|| {
                        let counter = Arc::clone(&counter);
                        async move {
                            counter.fetch_add(1, Ordering::SeqCst);
                            immediate_success(&format!("build-{i}"))
                        }
                    })
                    .await
            }));
        }

        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.unwrap());
        }

        // All three builds should have run.
        assert_eq!(counter.load(Ordering::SeqCst), 3);
        assert!(results.iter().all(|r| r.success));
    }

    // ---- Cancel mode -------------------------------------------------------

    #[tokio::test]
    async fn cancel_mode_supersedes_in_progress_build() {
        let coord = Arc::new(BuildCoordinator::new(BuildConcurrencyMode::Cancel));
        let barrier = Arc::new(Barrier::new(2));

        // Start a slow build.
        let coord1 = Arc::clone(&coord);
        let barrier1 = Arc::clone(&barrier);
        let first_handle = tokio::spawn(async move {
            coord1
                .run(|| {
                    let barrier1 = barrier1;
                    async move {
                        // Signal that the first build has started.
                        barrier1.wait().await;
                        // Sleep long enough to be superseded.
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        immediate_success("first-build")
                    }
                })
                .await
        });

        // Wait until the first build has started.
        barrier.wait().await;

        // Give a tiny moment for the first build to register its state.
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Start a second build that should supersede the first.
        let coord2 = Arc::clone(&coord);
        let second_handle = tokio::spawn(async move {
            coord2
                .run(|| async { immediate_success("second-build") })
                .await
        });

        let second_result = second_handle.await.unwrap();
        assert!(second_result.success);
        assert_eq!(second_result.output, "second-build");

        let first_result = first_handle.await.unwrap();
        assert!(!first_result.success);
        assert_eq!(
            first_result.errors,
            vec!["Build superseded by newer request."]
        );
    }

    #[tokio::test]
    async fn cancel_mode_single_build_succeeds() {
        let coord = BuildCoordinator::new(BuildConcurrencyMode::Cancel);
        let result = coord
            .run(|| async { immediate_success("only-build") })
            .await;
        assert!(result.success);
        assert_eq!(result.output, "only-build");
    }

    // ---- Share mode --------------------------------------------------------

    #[tokio::test]
    async fn share_mode_both_callers_get_new_result() {
        let coord = Arc::new(BuildCoordinator::new(BuildConcurrencyMode::Share));
        let barrier = Arc::new(Barrier::new(2));

        // Start a slow build.
        let coord1 = Arc::clone(&coord);
        let barrier1 = Arc::clone(&barrier);
        let first_handle = tokio::spawn(async move {
            coord1
                .run(|| {
                    let barrier1 = barrier1;
                    async move {
                        barrier1.wait().await;
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        immediate_success("old-result")
                    }
                })
                .await
        });

        // Wait until the first build has started.
        barrier.wait().await;

        // Give a tiny moment for the first build to register its state.
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Start a second build.
        let coord2 = Arc::clone(&coord);
        let second_handle = tokio::spawn(async move {
            coord2
                .run(|| async { immediate_success("new-result") })
                .await
        });

        let second_result = second_handle.await.unwrap();
        assert!(second_result.success);
        assert_eq!(second_result.output, "new-result");

        let first_result = first_handle.await.unwrap();
        // In share mode, the first caller should also get the new result.
        assert!(first_result.success);
        assert_eq!(first_result.output, "new-result");
    }

    #[tokio::test]
    async fn share_mode_single_build_succeeds() {
        let coord = BuildCoordinator::new(BuildConcurrencyMode::Share);
        let result = coord
            .run(|| async { immediate_success("only-build") })
            .await;
        assert!(result.success);
        assert_eq!(result.output, "only-build");
    }

    // ---- Mode from config --------------------------------------------------

    #[tokio::test]
    async fn mode_from_string_parsing() {
        use std::str::FromStr;

        let allow = BuildConcurrencyMode::from_str("allow").unwrap();
        assert_eq!(allow, BuildConcurrencyMode::Allow);

        let cancel = BuildConcurrencyMode::from_str("cancel").unwrap();
        assert_eq!(cancel, BuildConcurrencyMode::Cancel);

        let share = BuildConcurrencyMode::from_str("share").unwrap();
        assert_eq!(share, BuildConcurrencyMode::Share);

        assert!(BuildConcurrencyMode::from_str("invalid").is_err());

        // Verify the coordinator respects the parsed mode.
        let coord = BuildCoordinator::new(cancel);
        assert_eq!(coord.mode, BuildConcurrencyMode::Cancel);
    }

    // ---- Default mode is Allow ---------------------------------------------

    #[tokio::test]
    async fn default_mode_is_allow() {
        use crate::config::Config;
        let cfg = Config::default();
        assert_eq!(cfg.build_concurrency, BuildConcurrencyMode::Allow);

        // A coordinator built from the default config should use Allow.
        let coord = BuildCoordinator::new(cfg.build_concurrency);
        let result = coord
            .run(|| async { immediate_success("default-build") })
            .await;
        assert!(result.success);
        assert_eq!(result.output, "default-build");
    }

    // ---- Send + Sync assertions -------------------------------------------

    #[test]
    fn build_coordinator_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BuildCoordinator>();
    }
}
