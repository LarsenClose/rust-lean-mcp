//! Tool handler for `lean_proof_diff`.
//!
//! Compares proof state at two positions (before/after a tactic) and returns
//! what changed: goals added/removed and hypotheses added/removed.

use lean_lsp_client::client::LspClient;
use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::goal_diff::diff_goals;
use lean_mcp_core::models::GoalDiffResult;
use lean_mcp_core::utils::extract_goals_list;

/// Handle a `lean_proof_diff` tool call.
///
/// Opens the file, queries goals at two positions (before_line and after_line),
/// and returns a structured diff of what changed.
///
/// Both `before_line` and `after_line` are **1-indexed**.
/// Optional columns default to end-of-line (goals_after position).
pub async fn handle_lean_proof_diff(
    client: &dyn LspClient,
    file_path: &str,
    before_line: u32,
    before_column: Option<u32>,
    after_line: u32,
    after_column: Option<u32>,
) -> Result<GoalDiffResult, LeanToolError> {
    // 1. Open the file
    client
        .open_file(file_path)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "open_file".into(),
            message: e.to_string(),
        })?;

    // 2. Get file content and validate lines
    let content =
        client
            .get_file_content(file_path)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_file_content".into(),
                message: e.to_string(),
            })?;

    let lines: Vec<&str> = content.lines().collect();

    // Validate before_line
    if before_line == 0 || before_line as usize > lines.len() {
        return Err(LeanToolError::LineOutOfRange {
            line: before_line,
            total: lines.len(),
        });
    }
    // Validate after_line
    if after_line == 0 || after_line as usize > lines.len() {
        return Err(LeanToolError::LineOutOfRange {
            line: after_line,
            total: lines.len(),
        });
    }

    // 3. Compute LSP columns (0-indexed)
    let before_lsp_line = before_line - 1;
    let before_lsp_col = match before_column {
        Some(col) => col.saturating_sub(1),
        None => lines[before_lsp_line as usize].trim_end().len() as u32,
    };

    let after_lsp_line = after_line - 1;
    let after_lsp_col = match after_column {
        Some(col) => col.saturating_sub(1),
        None => lines[after_lsp_line as usize].trim_end().len() as u32,
    };

    // 4. Query goals at both positions
    let before_response = client
        .get_goal(file_path, before_lsp_line, before_lsp_col)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "get_goal".into(),
            message: e.to_string(),
        })?;

    let after_response = client
        .get_goal(file_path, after_lsp_line, after_lsp_col)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "get_goal".into(),
            message: e.to_string(),
        })?;

    let before_goals = extract_goals_list(before_response.as_ref());
    let after_goals = extract_goals_list(after_response.as_ref());

    // 5. Diff the two states
    let diff = diff_goals(&before_goals, &after_goals);

    Ok(GoalDiffResult {
        goals_added: diff.goals_added,
        goals_removed: diff.goals_removed,
        hypotheses_added: diff.hypotheses_added,
        hypotheses_removed: diff.hypotheses_removed,
        changed: diff.changed,
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

    struct MockDiffClient {
        project: PathBuf,
        content: String,
        goal_responses: Vec<((u32, u32), Option<Value>)>,
    }

    impl MockDiffClient {
        fn new(content: &str) -> Self {
            Self {
                project: PathBuf::from("/test/project"),
                content: content.to_string(),
                goal_responses: Vec::new(),
            }
        }

        fn with_goal(mut self, line: u32, col: u32, response: Option<Value>) -> Self {
            self.goal_responses.push(((line, col), response));
            self
        }
    }

    #[async_trait]
    impl LspClient for MockDiffClient {
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

    #[tokio::test]
    async fn proof_diff_detects_intro() {
        // Before intro: ⊢ P -> P
        // After intro:  h : P ⊢ P
        let content = "theorem foo : P -> P := by\n  intro h\n  exact h";
        let before_col = "theorem foo : P -> P := by".len() as u32;
        let after_col = "  intro h".len() as u32;

        let client = MockDiffClient::new(content)
            .with_goal(0, before_col, Some(json!({"goals": ["⊢ P -> P"]})))
            .with_goal(1, after_col, Some(json!({"goals": ["h : P\n⊢ P"]})));

        let result = handle_lean_proof_diff(&client, "Main.lean", 1, None, 2, None)
            .await
            .unwrap();

        assert!(result.changed);
        assert!(result.goals_removed.contains(&"P -> P".to_string()));
        assert!(result.goals_added.contains(&"P".to_string()));
        assert!(result.hypotheses_added.contains(&"h : P".to_string()));
    }

    #[tokio::test]
    async fn proof_diff_goal_solved() {
        let client = MockDiffClient::new("theorem foo : True := by\n  trivial")
            .with_goal(0, 24, Some(json!({"goals": ["⊢ True"]})))
            .with_goal(1, 9, Some(json!({"goals": []})));

        let result = handle_lean_proof_diff(&client, "Main.lean", 1, None, 2, None)
            .await
            .unwrap();

        assert!(result.changed);
        assert_eq!(result.goals_removed, vec!["True"]);
        assert!(result.goals_added.is_empty());
    }

    #[tokio::test]
    async fn proof_diff_no_change() {
        let client = MockDiffClient::new("theorem foo : True := by\n  skip")
            .with_goal(0, 24, Some(json!({"goals": ["⊢ True"]})))
            .with_goal(1, 6, Some(json!({"goals": ["⊢ True"]})));

        let result = handle_lean_proof_diff(&client, "Main.lean", 1, None, 2, None)
            .await
            .unwrap();

        assert!(!result.changed);
    }

    #[tokio::test]
    async fn proof_diff_with_explicit_columns() {
        let client = MockDiffClient::new("theorem foo : True := by\n  trivial")
            .with_goal(0, 2, Some(json!({"goals": ["⊢ True"]})))
            .with_goal(1, 4, Some(json!({"goals": []})));

        let result = handle_lean_proof_diff(&client, "Main.lean", 1, Some(3), 2, Some(5))
            .await
            .unwrap();

        assert!(result.changed);
    }

    #[tokio::test]
    async fn proof_diff_line_out_of_range() {
        let client = MockDiffClient::new("one line");

        let err = handle_lean_proof_diff(&client, "Main.lean", 1, None, 5, None)
            .await
            .unwrap_err();

        match err {
            LeanToolError::LineOutOfRange { line, total } => {
                assert_eq!(line, 5);
                assert_eq!(total, 1);
            }
            other => panic!("expected LineOutOfRange, got: {other}"),
        }
    }

    #[tokio::test]
    async fn proof_diff_before_line_out_of_range() {
        let client = MockDiffClient::new("one line");

        let err = handle_lean_proof_diff(&client, "Main.lean", 0, None, 1, None)
            .await
            .unwrap_err();

        match err {
            LeanToolError::LineOutOfRange { line, .. } => {
                assert_eq!(line, 0);
            }
            other => panic!("expected LineOutOfRange, got: {other}"),
        }
    }
}
