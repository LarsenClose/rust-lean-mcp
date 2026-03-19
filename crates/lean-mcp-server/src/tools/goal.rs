//! Tool handlers for `lean_goal` and `lean_term_goal`.
//!
//! Each handler opens the file via the LSP client, extracts the relevant
//! line text, converts from 1-indexed user coordinates to 0-indexed LSP
//! coordinates, and queries the Lean server for proof state.

use lean_lsp_client::client::LspClient;
use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::models::{GoalState, TermGoalState};
use lean_mcp_core::utils::extract_goals_list;

/// Handle a `lean_goal` tool call.
///
/// When `column` is `None`, queries goals at two positions on the line:
///   - **goals_before**: first non-whitespace character
///   - **goals_after**: end of line (length of trimmed-right text)
///
/// When `column` is `Some(c)`, queries goals at that exact column.
///
/// `line` and `column` are **1-indexed** (matching the MCP tool interface).
/// They are converted to 0-indexed for LSP calls internally.
pub async fn handle_lean_goal(
    client: &dyn LspClient,
    file_path: &str,
    line: u32,
    column: Option<u32>,
) -> Result<GoalState, LeanToolError> {
    // 1. Open the file in the LSP server.
    client
        .open_file(file_path)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "open_file".into(),
            message: e.to_string(),
        })?;

    // 2. Get file content and extract the requested line.
    let content =
        client
            .get_file_content(file_path)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_file_content".into(),
                message: e.to_string(),
            })?;

    let lines: Vec<&str> = content.lines().collect();

    // 3. Validate line range (1-indexed).
    if line == 0 || line as usize > lines.len() {
        return Err(LeanToolError::LineOutOfRange {
            line,
            total: lines.len(),
        });
    }

    let line_text = lines[(line - 1) as usize];
    let lsp_line = line - 1; // convert to 0-indexed

    match column {
        Some(col) => {
            // Validate column range (1-indexed).
            let line_len = line_text.len();
            if col == 0 || col as usize > line_len + 1 {
                return Err(LeanToolError::ColumnOutOfRange {
                    column: col,
                    length: line_len,
                });
            }

            let lsp_col = col - 1; // convert to 0-indexed
            let goal_response = client
                .get_goal(file_path, lsp_line, lsp_col)
                .await
                .map_err(|e| LeanToolError::LspError {
                    operation: "get_goal".into(),
                    message: e.to_string(),
                })?;

            let goals = extract_goals_list(goal_response.as_ref());

            Ok(GoalState {
                line_context: line_text.to_string(),
                goals: Some(goals),
                goals_before: None,
                goals_after: None,
            })
        }
        None => {
            // Find first non-whitespace column (0-indexed for LSP).
            let first_non_ws = line_text.find(|c: char| !c.is_whitespace()).unwrap_or(0) as u32;

            // End of line (0-indexed for LSP) = length of right-trimmed text.
            let end_col = line_text.trim_end().len() as u32;

            let before_response = client
                .get_goal(file_path, lsp_line, first_non_ws)
                .await
                .map_err(|e| LeanToolError::LspError {
                    operation: "get_goal".into(),
                    message: e.to_string(),
                })?;

            let after_response = client
                .get_goal(file_path, lsp_line, end_col)
                .await
                .map_err(|e| LeanToolError::LspError {
                    operation: "get_goal".into(),
                    message: e.to_string(),
                })?;

            let goals_before = extract_goals_list(before_response.as_ref());
            let goals_after = extract_goals_list(after_response.as_ref());

            Ok(GoalState {
                line_context: line_text.to_string(),
                goals: None,
                goals_before: Some(goals_before),
                goals_after: Some(goals_after),
            })
        }
    }
}

/// Strip markdown code fences from a Lean term-goal response.
///
/// Removes a leading `` ```lean\n `` and trailing `` \n``` `` if present.
fn strip_markdown_fences(s: &str) -> String {
    let s = s
        .strip_prefix("```lean\n")
        .or_else(|| s.strip_prefix("```lean\r\n"))
        .unwrap_or(s);
    let s = s.strip_suffix("\n```").unwrap_or(s);
    let s = s.strip_suffix("\r\n```").unwrap_or(s);
    s.to_string()
}

/// Handle a `lean_term_goal` tool call.
///
/// Queries the Lean server for the expected type at a given position.
/// If `column` is `None`, defaults to the end of the line (right-trimmed).
///
/// `line` and `column` are **1-indexed**; converted to 0-indexed for LSP.
pub async fn handle_lean_term_goal(
    client: &dyn LspClient,
    file_path: &str,
    line: u32,
    column: Option<u32>,
) -> Result<TermGoalState, LeanToolError> {
    // 1. Open the file in the LSP server.
    client
        .open_file(file_path)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "open_file".into(),
            message: e.to_string(),
        })?;

    // 2. Get file content and extract the requested line.
    let content =
        client
            .get_file_content(file_path)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_file_content".into(),
                message: e.to_string(),
            })?;

    let lines: Vec<&str> = content.lines().collect();

    // 3. Validate line range (1-indexed).
    if line == 0 || line as usize > lines.len() {
        return Err(LeanToolError::LineOutOfRange {
            line,
            total: lines.len(),
        });
    }

    let line_text = lines[(line - 1) as usize];
    let lsp_line = line - 1; // convert to 0-indexed

    // 4. Default column to end of (trimmed) line if not given.
    let lsp_col = match column {
        Some(col) => {
            let line_len = line_text.len();
            if col == 0 || col as usize > line_len + 1 {
                return Err(LeanToolError::ColumnOutOfRange {
                    column: col,
                    length: line_len,
                });
            }
            col - 1
        }
        None => line_text.trim_end().len() as u32,
    };

    // 5. Query term goal from the LSP.
    let term_goal_response = client
        .get_term_goal(file_path, lsp_line, lsp_col)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "get_term_goal".into(),
            message: e.to_string(),
        })?;

    // 6. Extract the expected type, stripping markdown fences.
    let expected_type = term_goal_response.and_then(|v| {
        v.get("goal")
            .and_then(|g| g.as_str())
            .map(strip_markdown_fences)
    });

    Ok(TermGoalState {
        line_context: line_text.to_string(),
        expected_type,
    })
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

    /// A minimal mock for [`LspClient`] used in goal handler tests.
    ///
    /// Pre-loaded with file content and canned responses for `get_goal`
    /// and `get_term_goal` keyed by `(line, column)`.
    struct MockGoalClient {
        project: PathBuf,
        content: String,
        /// Canned goal responses keyed by (0-indexed line, 0-indexed col).
        goal_responses: Vec<((u32, u32), Option<Value>)>,
        /// Canned term-goal responses keyed by (0-indexed line, 0-indexed col).
        term_goal_responses: Vec<((u32, u32), Option<Value>)>,
    }

    impl MockGoalClient {
        fn new(content: &str) -> Self {
            Self {
                project: PathBuf::from("/test/project"),
                content: content.to_string(),
                goal_responses: Vec::new(),
                term_goal_responses: Vec::new(),
            }
        }

        fn with_goal(mut self, line: u32, col: u32, response: Option<Value>) -> Self {
            self.goal_responses.push(((line, col), response));
            self
        }

        fn with_term_goal(mut self, line: u32, col: u32, response: Option<Value>) -> Self {
            self.term_goal_responses.push(((line, col), response));
            self
        }
    }

    #[async_trait]
    impl LspClient for MockGoalClient {
        fn project_path(&self) -> &Path {
            &self.project
        }

        async fn open_file(
            &self,
            _relative_path: &str,
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
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
            _relative_path: &str,
        ) -> Result<String, lean_lsp_client::client::LspClientError> {
            Ok(self.content.clone())
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
            _relative_path: &str,
            line: u32,
            column: u32,
        ) -> Result<Option<Value>, lean_lsp_client::client::LspClientError> {
            for ((l, c), resp) in &self.goal_responses {
                if *l == line && *c == column {
                    return Ok(resp.clone());
                }
            }
            Ok(None)
        }

        async fn get_term_goal(
            &self,
            _relative_path: &str,
            line: u32,
            column: u32,
        ) -> Result<Option<Value>, lean_lsp_client::client::LspClientError> {
            for ((l, c), resp) in &self.term_goal_responses {
                if *l == line && *c == column {
                    return Ok(resp.clone());
                }
            }
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

    // ---- handle_lean_goal with explicit column ----

    #[tokio::test]
    async fn goal_with_column_returns_goals() {
        let client = MockGoalClient::new("import Mathlib\n  exact h\ntheorem foo : True := by")
            .with_goal(1, 2, Some(json!({"goals": ["a : Nat\n|- a = a"]})));

        let result = handle_lean_goal(&client, "Main.lean", 2, Some(3))
            .await
            .unwrap();

        assert_eq!(result.line_context, "  exact h");
        assert_eq!(result.goals, Some(vec!["a : Nat\n|- a = a".to_string()]));
        assert!(result.goals_before.is_none());
        assert!(result.goals_after.is_none());
    }

    // ---- handle_lean_goal without column (before/after) ----

    #[tokio::test]
    async fn goal_without_column_returns_before_and_after() {
        // Line "  simp" has first non-ws at column 2 (0-indexed) and trimmed end at column 6.
        let client = MockGoalClient::new("theorem foo : Nat := by\n  simp\n  done")
            .with_goal(1, 2, Some(json!({"goals": ["|- 0 = 0"]})))
            .with_goal(1, 6, Some(json!({"goals": []})));

        let result = handle_lean_goal(&client, "Main.lean", 2, None)
            .await
            .unwrap();

        assert_eq!(result.line_context, "  simp");
        assert!(result.goals.is_none());
        assert_eq!(result.goals_before, Some(vec!["|- 0 = 0".to_string()]));
        assert_eq!(result.goals_after, Some(vec![]));
    }

    // ---- handle_lean_goal line out of range ----

    #[tokio::test]
    async fn goal_line_out_of_range_returns_error() {
        let client = MockGoalClient::new("line one\nline two");

        let err = handle_lean_goal(&client, "Main.lean", 5, Some(1))
            .await
            .unwrap_err();

        match err {
            LeanToolError::LineOutOfRange { line, total } => {
                assert_eq!(line, 5);
                assert_eq!(total, 2);
            }
            other => panic!("expected LineOutOfRange, got: {other}"),
        }
    }

    // ---- handle_lean_goal line zero returns error ----

    #[tokio::test]
    async fn goal_line_zero_returns_error() {
        let client = MockGoalClient::new("line one");

        let err = handle_lean_goal(&client, "Main.lean", 0, Some(1))
            .await
            .unwrap_err();

        match err {
            LeanToolError::LineOutOfRange { line, total } => {
                assert_eq!(line, 0);
                assert_eq!(total, 1);
            }
            other => panic!("expected LineOutOfRange, got: {other}"),
        }
    }

    // ---- handle_lean_goal column out of range ----

    #[tokio::test]
    async fn goal_column_out_of_range_returns_error() {
        let client = MockGoalClient::new("short");

        let err = handle_lean_goal(&client, "Main.lean", 1, Some(100))
            .await
            .unwrap_err();

        match err {
            LeanToolError::ColumnOutOfRange { column, length } => {
                assert_eq!(column, 100);
                assert_eq!(length, 5);
            }
            other => panic!("expected ColumnOutOfRange, got: {other}"),
        }
    }

    // ---- handle_lean_goal with empty response returns empty goals ----

    #[tokio::test]
    async fn goal_with_column_empty_response_returns_empty_goals() {
        let client = MockGoalClient::new("  exact h").with_goal(0, 2, None);

        let result = handle_lean_goal(&client, "Main.lean", 1, Some(3))
            .await
            .unwrap();

        assert_eq!(result.goals, Some(vec![]));
    }

    // ---- handle_lean_term_goal returns expected_type ----

    #[tokio::test]
    async fn term_goal_returns_expected_type() {
        let client = MockGoalClient::new("def foo := Nat.succ 0").with_term_goal(
            0,
            21,
            Some(json!({"goal": "Nat"})),
        );

        let result = handle_lean_term_goal(&client, "Main.lean", 1, None)
            .await
            .unwrap();

        assert_eq!(result.line_context, "def foo := Nat.succ 0");
        assert_eq!(result.expected_type, Some("Nat".to_string()));
    }

    // ---- handle_lean_term_goal strips markdown fences ----

    #[tokio::test]
    async fn term_goal_strips_markdown_fences() {
        let client = MockGoalClient::new("def foo := Nat.succ 0").with_term_goal(
            0,
            21,
            Some(json!({"goal": "```lean\nNat -> Nat\n```"})),
        );

        let result = handle_lean_term_goal(&client, "Main.lean", 1, None)
            .await
            .unwrap();

        assert_eq!(result.expected_type, Some("Nat -> Nat".to_string()));
    }

    // ---- handle_lean_term_goal with None response ----

    #[tokio::test]
    async fn term_goal_none_response_returns_none_expected_type() {
        let client = MockGoalClient::new("import Mathlib").with_term_goal(0, 14, None);

        let result = handle_lean_term_goal(&client, "Main.lean", 1, None)
            .await
            .unwrap();

        assert_eq!(result.line_context, "import Mathlib");
        assert!(result.expected_type.is_none());
    }

    // ---- handle_lean_term_goal with explicit column ----

    #[tokio::test]
    async fn term_goal_with_explicit_column() {
        let client = MockGoalClient::new("  foo bar baz").with_term_goal(
            0,
            5,
            Some(json!({"goal": "Prop"})),
        );

        let result = handle_lean_term_goal(&client, "Main.lean", 1, Some(6))
            .await
            .unwrap();

        assert_eq!(result.expected_type, Some("Prop".to_string()));
    }

    // ---- handle_lean_term_goal line out of range ----

    #[tokio::test]
    async fn term_goal_line_out_of_range() {
        let client = MockGoalClient::new("one line");

        let err = handle_lean_term_goal(&client, "Main.lean", 10, None)
            .await
            .unwrap_err();

        match err {
            LeanToolError::LineOutOfRange { line, total } => {
                assert_eq!(line, 10);
                assert_eq!(total, 1);
            }
            other => panic!("expected LineOutOfRange, got: {other}"),
        }
    }

    // ---- strip_markdown_fences unit tests ----

    #[test]
    fn strip_fences_with_lean_block() {
        assert_eq!(strip_markdown_fences("```lean\nfoo\n```"), "foo");
    }

    #[test]
    fn strip_fences_no_fences() {
        assert_eq!(strip_markdown_fences("plain text"), "plain text");
    }

    #[test]
    fn strip_fences_partial_prefix_only() {
        assert_eq!(strip_markdown_fences("```lean\nfoo"), "foo");
    }
}
