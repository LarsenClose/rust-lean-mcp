//! Tool handler for `lean_goals_batch`.
//!
//! Queries goal states at multiple positions concurrently, returning partial
//! results when individual positions fail. Opens each unique file once before
//! dispatching concurrent goal queries.

use std::collections::HashSet;

use lean_lsp_client::client::LspClient;
use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::models::{BatchGoalEntry, BatchGoalPosition, BatchGoalResult, GoalState};
use lean_mcp_core::utils::extract_goals_list;

/// Query a single position's goal state.
///
/// Returns `Ok(GoalState)` on success or `Err(message)` on failure.
/// This never propagates errors — failures are captured as strings for
/// partial-result semantics.
async fn query_single_goal(
    client: &dyn LspClient,
    file_path: &str,
    line: u32,
    column: Option<u32>,
    content: &str,
) -> Result<GoalState, String> {
    let lines: Vec<&str> = content.lines().collect();

    // Validate line range (1-indexed).
    if line == 0 || line as usize > lines.len() {
        return Err(format!(
            "Line {line} out of range (file has {} lines)",
            lines.len()
        ));
    }

    let line_text = lines[(line - 1) as usize];
    let lsp_line = line - 1; // convert to 0-indexed

    match column {
        Some(col) => {
            let line_len = line_text.len();
            if col == 0 || col as usize > line_len + 1 {
                return Err(format!(
                    "Column {col} out of range (line has {line_len} characters)"
                ));
            }

            let lsp_col = col - 1;
            let goal_response = client
                .get_goal(file_path, lsp_line, lsp_col)
                .await
                .map_err(|e| format!("LSP error: {e}"))?;

            let goals = extract_goals_list(goal_response.as_ref());

            Ok(GoalState {
                line_context: line_text.to_string(),
                goals: Some(goals),
                goals_before: None,
                goals_after: None,
            })
        }
        None => {
            let first_non_ws = line_text.find(|c: char| !c.is_whitespace()).unwrap_or(0) as u32;
            let end_col = line_text.trim_end().len() as u32;

            let (before_response, after_response) = tokio::join!(
                client.get_goal(file_path, lsp_line, first_non_ws),
                client.get_goal(file_path, lsp_line, end_col),
            );

            let goals_before = extract_goals_list(
                before_response
                    .map_err(|e| format!("LSP error: {e}"))?
                    .as_ref(),
            );
            let goals_after = extract_goals_list(
                after_response
                    .map_err(|e| format!("LSP error: {e}"))?
                    .as_ref(),
            );

            Ok(GoalState {
                line_context: line_text.to_string(),
                goals: None,
                goals_before: Some(goals_before),
                goals_after: Some(goals_after),
            })
        }
    }
}

/// Handle a `lean_goals_batch` tool call.
///
/// Opens each unique file once, then fires concurrent goal queries for all
/// positions. Returns partial results — individual position failures are
/// captured in the `error` field rather than failing the whole batch.
///
/// `positions` must contain at least one entry; coordinates are 1-indexed.
pub async fn handle_lean_goals_batch(
    client: &dyn LspClient,
    positions: Vec<BatchGoalPosition>,
) -> Result<BatchGoalResult, LeanToolError> {
    if positions.is_empty() {
        return Ok(BatchGoalResult { items: vec![] });
    }

    // 1. Collect unique file paths and open each once.
    let unique_files: HashSet<&str> = positions.iter().map(|p| p.file_path.as_str()).collect();

    for file_path in &unique_files {
        client
            .open_file(file_path)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "open_file".into(),
                message: e.to_string(),
            })?;
    }

    // 2. Pre-fetch file contents for all unique files.
    let mut file_contents: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for file_path in &unique_files {
        let content =
            client
                .get_file_content(file_path)
                .await
                .map_err(|e| LeanToolError::LspError {
                    operation: "get_file_content".into(),
                    message: e.to_string(),
                })?;
        file_contents.insert(file_path.to_string(), content);
    }

    // 3. Fire concurrent goal queries for all positions.
    let futures: Vec<_> = positions
        .iter()
        .map(|pos| {
            let content = file_contents
                .get(&pos.file_path)
                .expect("file content pre-fetched");
            query_single_goal(client, &pos.file_path, pos.line, pos.column, content)
        })
        .collect();

    let results = futures::future::join_all(futures).await;

    // 4. Assemble batch results with partial-failure semantics.
    let items: Vec<BatchGoalEntry> = positions
        .into_iter()
        .zip(results)
        .map(|(position, result)| match result {
            Ok(goal_state) => BatchGoalEntry {
                position,
                result: Some(goal_state),
                error: None,
            },
            Err(msg) => BatchGoalEntry {
                position,
                result: None,
                error: Some(msg),
            },
        })
        .collect();

    Ok(BatchGoalResult { items })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A mock LSP client for batch goal tests.
    struct MockBatchClient {
        project: PathBuf,
        /// Map of file path -> content.
        files: std::collections::HashMap<String, String>,
        /// Canned goal responses keyed by (file, 0-indexed line, 0-indexed col).
        goal_responses: Vec<(String, u32, u32, Option<Value>)>,
        /// Counter for open_file calls (to verify dedup).
        open_count: AtomicU32,
    }

    impl MockBatchClient {
        fn new() -> Self {
            Self {
                project: PathBuf::from("/test/project"),
                files: std::collections::HashMap::new(),
                goal_responses: Vec::new(),
                open_count: AtomicU32::new(0),
            }
        }

        fn with_file(mut self, path: &str, content: &str) -> Self {
            self.files.insert(path.to_string(), content.to_string());
            self
        }

        fn with_goal(mut self, file: &str, line: u32, col: u32, response: Option<Value>) -> Self {
            self.goal_responses
                .push((file.to_string(), line, col, response));
            self
        }
    }

    #[async_trait]
    impl LspClient for MockBatchClient {
        fn project_path(&self) -> &Path {
            &self.project
        }

        async fn open_file(
            &self,
            _relative_path: &str,
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            self.open_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn open_file_force(
            &self,
            _relative_path: &str,
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }

        async fn get_file_content(
            &self,
            relative_path: &str,
        ) -> Result<String, lean_lsp_client::client::LspClientError> {
            self.files.get(relative_path).cloned().ok_or(
                lean_lsp_client::client::LspClientError::FileNotOpen(relative_path.to_string()),
            )
        }

        async fn update_file(
            &self,
            _relative_path: &str,
            _changes: Vec<Value>,
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }

        async fn update_file_content(
            &self,
            _relative_path: &str,
            _content: &str,
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }

        async fn close_files(
            &self,
            _paths: &[String],
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }

        async fn get_diagnostics(
            &self,
            _relative_path: &str,
            _start_line: Option<u32>,
            _end_line: Option<u32>,
            _inactivity_timeout: Option<f64>,
        ) -> Result<Value, lean_lsp_client::client::LspClientError> {
            Ok(json!({}))
        }

        async fn get_interactive_diagnostics(
            &self,
            _relative_path: &str,
            _start_line: Option<u32>,
            _end_line: Option<u32>,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }

        async fn get_goal(
            &self,
            relative_path: &str,
            line: u32,
            column: u32,
        ) -> Result<Option<Value>, lean_lsp_client::client::LspClientError> {
            for (f, l, c, resp) in &self.goal_responses {
                if f == relative_path && *l == line && *c == column {
                    return Ok(resp.clone());
                }
            }
            Ok(None)
        }

        async fn get_term_goal(
            &self,
            _relative_path: &str,
            _line: u32,
            _column: u32,
        ) -> Result<Option<Value>, lean_lsp_client::client::LspClientError> {
            Ok(None)
        }

        async fn get_hover(
            &self,
            _relative_path: &str,
            _line: u32,
            _column: u32,
        ) -> Result<Option<Value>, lean_lsp_client::client::LspClientError> {
            Ok(None)
        }

        async fn get_completions(
            &self,
            _relative_path: &str,
            _line: u32,
            _column: u32,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }

        async fn get_declarations(
            &self,
            _relative_path: &str,
            _line: u32,
            _column: u32,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }

        async fn get_references(
            &self,
            _relative_path: &str,
            _line: u32,
            _column: u32,
            _include_declaration: bool,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }

        async fn get_document_symbols(
            &self,
            _relative_path: &str,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }

        async fn get_code_actions(
            &self,
            _relative_path: &str,
            _start_line: u32,
            _start_col: u32,
            _end_line: u32,
            _end_col: u32,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }

        async fn get_code_action_resolve(
            &self,
            _action: Value,
        ) -> Result<Value, lean_lsp_client::client::LspClientError> {
            Ok(json!({}))
        }

        async fn get_widgets(
            &self,
            _relative_path: &str,
            _line: u32,
            _column: u32,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }

        async fn get_widget_source(
            &self,
            _relative_path: &str,
            _line: u32,
            _column: u32,
            _javascript_hash: &str,
        ) -> Result<Value, lean_lsp_client::client::LspClientError> {
            Ok(json!({}))
        }

        async fn shutdown(&self) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }
    }

    // ---- Basic batch with multiple positions in one file ----

    #[tokio::test]
    async fn batch_single_file_multiple_positions() {
        let client = MockBatchClient::new()
            .with_file(
                "Main.lean",
                "import Mathlib\ntheorem foo : True := by\n  trivial",
            )
            .with_goal("Main.lean", 1, 0, Some(json!({"goals": ["⊢ True"]})))
            .with_goal("Main.lean", 2, 2, Some(json!({"goals": []})));

        let positions = vec![
            BatchGoalPosition {
                file_path: "Main.lean".into(),
                line: 2,
                column: Some(1),
            },
            BatchGoalPosition {
                file_path: "Main.lean".into(),
                line: 3,
                column: Some(3),
            },
        ];

        let result = handle_lean_goals_batch(&client, positions).await.unwrap();

        assert_eq!(result.items.len(), 2);
        // Both should succeed (possibly with empty goals).
        assert!(result.items[0].result.is_some());
        assert!(result.items[0].error.is_none());
        assert!(result.items[1].result.is_some());
        assert!(result.items[1].error.is_none());
    }

    // ---- Opens each unique file only once ----

    #[tokio::test]
    async fn batch_deduplicates_file_opens() {
        let client = MockBatchClient::new()
            .with_file("A.lean", "theorem a : True := by trivial")
            .with_file("B.lean", "theorem b : True := by trivial");

        let positions = vec![
            BatchGoalPosition {
                file_path: "A.lean".into(),
                line: 1,
                column: Some(1),
            },
            BatchGoalPosition {
                file_path: "A.lean".into(),
                line: 1,
                column: Some(5),
            },
            BatchGoalPosition {
                file_path: "B.lean".into(),
                line: 1,
                column: Some(1),
            },
        ];

        let result = handle_lean_goals_batch(&client, positions).await.unwrap();

        assert_eq!(result.items.len(), 3);
        // Should open only 2 unique files (A.lean and B.lean).
        assert_eq!(client.open_count.load(Ordering::SeqCst), 2);
    }

    // ---- Partial failure: bad position doesn't fail the batch ----

    #[tokio::test]
    async fn batch_partial_failure_on_bad_line() {
        let client = MockBatchClient::new()
            .with_file("Main.lean", "line one\nline two")
            .with_goal("Main.lean", 0, 0, Some(json!({"goals": ["⊢ True"]})));

        let positions = vec![
            BatchGoalPosition {
                file_path: "Main.lean".into(),
                line: 1,
                column: Some(1),
            },
            BatchGoalPosition {
                file_path: "Main.lean".into(),
                line: 99,
                column: Some(1),
            },
        ];

        let result = handle_lean_goals_batch(&client, positions).await.unwrap();

        assert_eq!(result.items.len(), 2);
        // First should succeed.
        assert!(result.items[0].result.is_some());
        assert!(result.items[0].error.is_none());
        // Second should fail with line out of range.
        assert!(result.items[1].result.is_none());
        assert!(result.items[1].error.is_some());
        assert!(result.items[1]
            .error
            .as_ref()
            .unwrap()
            .contains("out of range"));
    }

    // ---- Partial failure: bad column ----

    #[tokio::test]
    async fn batch_partial_failure_on_bad_column() {
        let client = MockBatchClient::new().with_file("Main.lean", "short");

        let positions = vec![BatchGoalPosition {
            file_path: "Main.lean".into(),
            line: 1,
            column: Some(100),
        }];

        let result = handle_lean_goals_batch(&client, positions).await.unwrap();

        assert_eq!(result.items.len(), 1);
        assert!(result.items[0].result.is_none());
        assert!(result.items[0]
            .error
            .as_ref()
            .unwrap()
            .contains("Column 100 out of range"));
    }

    // ---- Empty positions returns empty result ----

    #[tokio::test]
    async fn batch_empty_positions() {
        let client = MockBatchClient::new();

        let result = handle_lean_goals_batch(&client, vec![]).await.unwrap();

        assert!(result.items.is_empty());
    }

    // ---- Without column returns before/after goals ----

    #[tokio::test]
    async fn batch_without_column_returns_before_after() {
        // "  simp" has first non-ws at col 2 (0-indexed), trimmed end at col 6.
        let client = MockBatchClient::new()
            .with_file("Main.lean", "theorem foo := by\n  simp\n  done")
            .with_goal("Main.lean", 1, 2, Some(json!({"goals": ["⊢ 0 = 0"]})))
            .with_goal("Main.lean", 1, 6, Some(json!({"goals": []})));

        let positions = vec![BatchGoalPosition {
            file_path: "Main.lean".into(),
            line: 2,
            column: None,
        }];

        let result = handle_lean_goals_batch(&client, positions).await.unwrap();

        assert_eq!(result.items.len(), 1);
        let entry = &result.items[0];
        assert!(entry.error.is_none());
        let gs = entry.result.as_ref().unwrap();
        assert_eq!(gs.line_context, "  simp");
        assert!(gs.goals.is_none());
        assert_eq!(gs.goals_before, Some(vec!["⊢ 0 = 0".to_string()]));
        assert_eq!(gs.goals_after, Some(vec![]));
    }

    // ---- With column returns exact goals ----

    #[tokio::test]
    async fn batch_with_column_returns_exact_goals() {
        let client = MockBatchClient::new()
            .with_file("Main.lean", "import Mathlib\n  exact h")
            .with_goal(
                "Main.lean",
                1,
                2,
                Some(json!({"goals": ["a : Nat\n⊢ a = a"]})),
            );

        let positions = vec![BatchGoalPosition {
            file_path: "Main.lean".into(),
            line: 2,
            column: Some(3),
        }];

        let result = handle_lean_goals_batch(&client, positions).await.unwrap();

        assert_eq!(result.items.len(), 1);
        let gs = result.items[0].result.as_ref().unwrap();
        assert_eq!(gs.goals, Some(vec!["a : Nat\n⊢ a = a".to_string()]));
        assert!(gs.goals_before.is_none());
    }

    // ---- Multiple files in one batch ----

    #[tokio::test]
    async fn batch_multiple_files() {
        let client = MockBatchClient::new()
            .with_file("A.lean", "theorem a := by trivial")
            .with_file("B.lean", "theorem b := by trivial")
            .with_goal("A.lean", 0, 0, Some(json!({"goals": ["⊢ True"]})))
            .with_goal("B.lean", 0, 0, Some(json!({"goals": ["⊢ False"]})));

        let positions = vec![
            BatchGoalPosition {
                file_path: "A.lean".into(),
                line: 1,
                column: Some(1),
            },
            BatchGoalPosition {
                file_path: "B.lean".into(),
                line: 1,
                column: Some(1),
            },
        ];

        let result = handle_lean_goals_batch(&client, positions).await.unwrap();

        assert_eq!(result.items.len(), 2);
        assert!(result.items[0].result.is_some());
        assert!(result.items[1].result.is_some());
        assert_eq!(result.items[0].position.file_path, "A.lean");
        assert_eq!(result.items[1].position.file_path, "B.lean");
    }

    // ---- Preserves input order ----

    #[tokio::test]
    async fn batch_preserves_order() {
        let client = MockBatchClient::new().with_file("Main.lean", "line1\nline2\nline3");

        let positions = vec![
            BatchGoalPosition {
                file_path: "Main.lean".into(),
                line: 3,
                column: Some(1),
            },
            BatchGoalPosition {
                file_path: "Main.lean".into(),
                line: 1,
                column: Some(1),
            },
            BatchGoalPosition {
                file_path: "Main.lean".into(),
                line: 2,
                column: Some(1),
            },
        ];

        let result = handle_lean_goals_batch(&client, positions).await.unwrap();

        assert_eq!(result.items[0].position.line, 3);
        assert_eq!(result.items[1].position.line, 1);
        assert_eq!(result.items[2].position.line, 2);
    }

    // ---- Line zero returns error ----

    #[tokio::test]
    async fn batch_line_zero_returns_error() {
        let client = MockBatchClient::new().with_file("Main.lean", "line one");

        let positions = vec![BatchGoalPosition {
            file_path: "Main.lean".into(),
            line: 0,
            column: Some(1),
        }];

        let result = handle_lean_goals_batch(&client, positions).await.unwrap();

        assert_eq!(result.items.len(), 1);
        assert!(result.items[0].error.is_some());
        assert!(result.items[0]
            .error
            .as_ref()
            .unwrap()
            .contains("Line 0 out of range"));
    }

    // ---- Model types are Send + Sync ----

    #[test]
    fn batch_types_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BatchGoalPosition>();
        assert_send_sync::<BatchGoalEntry>();
        assert_send_sync::<BatchGoalResult>();
    }
}
