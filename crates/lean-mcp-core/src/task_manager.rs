//! Background task manager with per-item progress tracking.
//!
//! [`TaskManager`] coordinates background task execution where each task
//! consists of multiple independent items (e.g., tactic attempts). Callers
//! can poll for partial results as individual items complete.
//!
//! # Design
//!
//! * **Per-item tracking** — Each task has `total` items, each with its own
//!   [`ItemStatus`]. Items transition from `Pending` to `Completed` or
//!   `Failed` independently.
//! * **Cancellation** — Tasks can be cancelled via a [`CancellationToken`],
//!   allowing background workers to cooperatively abort.
//! * **TTL cleanup** — Completed/cancelled tasks are retained for a
//!   configurable TTL, then garbage-collected by [`TaskManager::cleanup_expired`].
//! * **Concurrency** — `tokio::sync::RwLock` allows concurrent snapshot reads
//!   while writes (create, update, cancel) take an exclusive lock.
//!
//! [`CancellationToken`]: tokio_util::sync::CancellationToken

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Status of an individual item within a task.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status")]
pub enum ItemStatus<T: Clone + Send + Sync> {
    /// Item is still being processed.
    Pending,
    /// Item completed successfully.
    Completed {
        /// The result value.
        result: T,
    },
    /// Item failed with an error.
    Failed {
        /// Description of the failure.
        error: String,
    },
}

/// Status of an overall task.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Task is still running (some items pending).
    Running,
    /// All items have completed (successfully or with failure).
    Completed,
    /// Task was explicitly cancelled.
    Cancelled,
}

/// Snapshot of a task's state, returned by [`TaskManager::get_task`].
#[derive(Debug, Clone, Serialize)]
pub struct TaskSnapshot<T: Clone + Send + Sync> {
    /// Unique task identifier.
    pub task_id: String,
    /// Overall task status.
    pub status: TaskStatus,
    /// Total number of items in the task.
    pub total: usize,
    /// Number of items that have finished (completed or failed).
    pub completed_count: usize,
    /// Per-item status.
    pub items: Vec<ItemStatus<T>>,
}

/// Internal task state.
struct TaskState<T: Clone + Send + Sync + 'static> {
    total: usize,
    items: Vec<ItemStatus<T>>,
    created_at: Instant,
    cancelled: bool,
    cancel_token: CancellationToken,
}

/// Manages background tasks with per-item progress tracking.
///
/// Generic over the result type `T` (e.g., `AttemptResult`).
pub struct TaskManager<T: Clone + Send + Sync + 'static> {
    tasks: Arc<RwLock<HashMap<String, TaskState<T>>>>,
    ttl: Duration,
}

impl<T: Clone + Send + Sync + 'static> TaskManager<T> {
    /// Create a new `TaskManager` with the given TTL for completed tasks.
    ///
    /// After a task completes or is cancelled, it will be retained for `ttl`
    /// duration before being eligible for cleanup.
    pub fn new(ttl: Duration) -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            ttl,
        }
    }

    /// Create a new task with `total` items. Returns the task ID and a
    /// [`CancellationToken`] that background workers should monitor.
    ///
    /// All items start in [`ItemStatus::Pending`].
    pub async fn create_task(&self, total: usize) -> (String, CancellationToken) {
        let task_id = Uuid::new_v4().to_string();
        let cancel_token = CancellationToken::new();
        let state = TaskState {
            total,
            items: (0..total).map(|_| ItemStatus::Pending).collect(),
            created_at: Instant::now(),
            cancelled: false,
            cancel_token: cancel_token.clone(),
        };
        self.tasks.write().await.insert(task_id.clone(), state);
        (task_id, cancel_token)
    }

    /// Update a specific item's status.
    ///
    /// Called by background workers as each item completes. If `task_id` is
    /// unknown or `index` is out of bounds, this is a no-op.
    pub async fn update_item(&self, task_id: &str, index: usize, status: ItemStatus<T>) {
        let mut tasks = self.tasks.write().await;
        if let Some(state) = tasks.get_mut(task_id) {
            if index < state.items.len() {
                state.items[index] = status;
            }
        }
    }

    /// Get a snapshot of a task's current state.
    ///
    /// Returns `None` if the task does not exist (never created or already
    /// cleaned up).
    pub async fn get_task(&self, task_id: &str) -> Option<TaskSnapshot<T>> {
        let tasks = self.tasks.read().await;
        let state = tasks.get(task_id)?;

        let completed_count = state
            .items
            .iter()
            .filter(|i| !matches!(i, ItemStatus::Pending))
            .count();

        let status = if state.cancelled {
            TaskStatus::Cancelled
        } else if completed_count == state.total {
            TaskStatus::Completed
        } else {
            TaskStatus::Running
        };

        Some(TaskSnapshot {
            task_id: task_id.to_string(),
            status,
            total: state.total,
            completed_count,
            items: state.items.clone(),
        })
    }

    /// Cancel a task.
    ///
    /// Sets the cancelled flag and triggers the [`CancellationToken`] so
    /// background workers can cooperatively abort. Returns `true` if the
    /// task existed, `false` otherwise.
    pub async fn cancel_task(&self, task_id: &str) -> bool {
        let mut tasks = self.tasks.write().await;
        if let Some(state) = tasks.get_mut(task_id) {
            state.cancelled = true;
            state.cancel_token.cancel();
            true
        } else {
            false
        }
    }

    /// Remove tasks that have been completed/cancelled for longer than the TTL.
    ///
    /// Running tasks are never removed regardless of age.
    pub async fn cleanup_expired(&self) {
        let mut tasks = self.tasks.write().await;
        tasks.retain(|_, state| {
            let completed_count = state
                .items
                .iter()
                .filter(|i| !matches!(i, ItemStatus::Pending))
                .count();
            let is_done = completed_count == state.total || state.cancelled;
            // Keep if not done, or if done but within TTL
            !is_done || state.created_at.elapsed() < self.ttl
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a TaskManager<String> with a generous TTL.
    fn make_manager() -> TaskManager<String> {
        TaskManager::new(Duration::from_secs(60))
    }

    #[tokio::test]
    async fn create_task_returns_unique_ids() {
        let mgr = make_manager();
        let (id1, _) = mgr.create_task(1).await;
        let (id2, _) = mgr.create_task(1).await;
        assert_ne!(id1, id2, "Task IDs must be unique");
    }

    #[tokio::test]
    async fn get_task_returns_none_for_unknown_id() {
        let mgr = make_manager();
        assert!(mgr.get_task("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn new_task_is_all_pending() {
        let mgr = make_manager();
        let (id, _) = mgr.create_task(3).await;
        let snap = mgr.get_task(&id).await.unwrap();

        assert_eq!(snap.total, 3);
        assert_eq!(snap.completed_count, 0);
        assert_eq!(snap.status, TaskStatus::Running);
        assert_eq!(snap.items.len(), 3);
        for item in &snap.items {
            assert!(matches!(item, ItemStatus::Pending));
        }
    }

    #[tokio::test]
    async fn update_item_changes_status() {
        let mgr = make_manager();
        let (id, _) = mgr.create_task(3).await;

        mgr.update_item(
            &id,
            0,
            ItemStatus::Completed {
                result: "done".to_string(),
            },
        )
        .await;

        let snap = mgr.get_task(&id).await.unwrap();
        assert!(matches!(&snap.items[0], ItemStatus::Completed { result } if result == "done"));
        assert!(matches!(&snap.items[1], ItemStatus::Pending));
        assert!(matches!(&snap.items[2], ItemStatus::Pending));
    }

    #[tokio::test]
    async fn task_completes_when_all_items_done() {
        let mgr = make_manager();
        let (id, _) = mgr.create_task(2).await;

        mgr.update_item(
            &id,
            0,
            ItemStatus::Completed {
                result: "a".to_string(),
            },
        )
        .await;
        mgr.update_item(
            &id,
            1,
            ItemStatus::Completed {
                result: "b".to_string(),
            },
        )
        .await;

        let snap = mgr.get_task(&id).await.unwrap();
        assert_eq!(snap.status, TaskStatus::Completed);
        assert_eq!(snap.completed_count, 2);
    }

    #[tokio::test]
    async fn cancel_task_sets_cancelled() {
        let mgr = make_manager();
        let (id, _) = mgr.create_task(2).await;

        assert!(mgr.cancel_task(&id).await);

        let snap = mgr.get_task(&id).await.unwrap();
        assert_eq!(snap.status, TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn cancel_triggers_token() {
        let mgr = make_manager();
        let (id, token) = mgr.create_task(1).await;

        assert!(!token.is_cancelled(), "Token should not be cancelled yet");
        mgr.cancel_task(&id).await;
        assert!(
            token.is_cancelled(),
            "Token should be cancelled after cancel_task"
        );
    }

    #[tokio::test]
    async fn cleanup_removes_expired_completed_tasks() {
        // TTL of 0 means tasks expire immediately once done.
        let mgr: TaskManager<String> = TaskManager::new(Duration::from_millis(0));
        let (id, _) = mgr.create_task(1).await;

        mgr.update_item(
            &id,
            0,
            ItemStatus::Completed {
                result: "x".to_string(),
            },
        )
        .await;

        // Let the TTL elapse.
        tokio::time::sleep(Duration::from_millis(5)).await;

        mgr.cleanup_expired().await;
        assert!(
            mgr.get_task(&id).await.is_none(),
            "Expired completed task should be cleaned up"
        );
    }

    #[tokio::test]
    async fn cleanup_keeps_running_tasks() {
        let mgr: TaskManager<String> = TaskManager::new(Duration::from_millis(0));
        let (id, _) = mgr.create_task(2).await;

        // Only complete 1 of 2 items — task is still running.
        mgr.update_item(
            &id,
            0,
            ItemStatus::Completed {
                result: "x".to_string(),
            },
        )
        .await;

        tokio::time::sleep(Duration::from_millis(5)).await;

        mgr.cleanup_expired().await;
        assert!(
            mgr.get_task(&id).await.is_some(),
            "Running task must not be cleaned up"
        );
    }

    #[tokio::test]
    async fn update_out_of_bounds_is_noop() {
        let mgr = make_manager();
        let (id, _) = mgr.create_task(2).await;

        // Index 5 is out of bounds — should not panic.
        mgr.update_item(
            &id,
            5,
            ItemStatus::Completed {
                result: "oob".to_string(),
            },
        )
        .await;

        let snap = mgr.get_task(&id).await.unwrap();
        assert_eq!(
            snap.completed_count, 0,
            "Out-of-bounds update should be a no-op"
        );
    }

    #[tokio::test]
    async fn partial_completion_snapshot() {
        let mgr = make_manager();
        let (id, _) = mgr.create_task(3).await;

        mgr.update_item(
            &id,
            0,
            ItemStatus::Completed {
                result: "a".to_string(),
            },
        )
        .await;
        mgr.update_item(
            &id,
            2,
            ItemStatus::Completed {
                result: "c".to_string(),
            },
        )
        .await;

        let snap = mgr.get_task(&id).await.unwrap();
        assert_eq!(snap.completed_count, 2);
        assert_eq!(snap.status, TaskStatus::Running);
        assert!(matches!(&snap.items[1], ItemStatus::Pending));
    }

    #[tokio::test]
    async fn failed_item_counts_as_completed() {
        let mgr = make_manager();
        let (id, _) = mgr.create_task(1).await;

        mgr.update_item(
            &id,
            0,
            ItemStatus::Failed {
                error: "timeout".to_string(),
            },
        )
        .await;

        let snap = mgr.get_task(&id).await.unwrap();
        assert_eq!(snap.completed_count, 1);
        assert_eq!(snap.status, TaskStatus::Completed);
        assert!(matches!(&snap.items[0], ItemStatus::Failed { error } if error == "timeout"));
    }
}
