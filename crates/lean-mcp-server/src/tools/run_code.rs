//! Tool handler for `lean_run_code`.
//!
//! Runs standalone Lean code by writing it to a temporary file, collecting
//! diagnostics from the LSP, and cleaning up the file afterwards.

use lean_lsp_client::client::LspClient;
use lean_lsp_client::types::severity;
use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::models::{DiagnosticMessage, RunResult};
use serde_json::Value;
use std::path::Path;
use uuid::Uuid;

/// Convert raw LSP diagnostics into [`DiagnosticMessage`] items.
///
/// Mirrors the Python `_to_diagnostic_messages` helper.
fn to_diagnostic_messages(diagnostics: &[Value]) -> Vec<DiagnosticMessage> {
    let mut items = Vec::new();
    for diag in diagnostics {
        let range = diag.get("fullRange").or_else(|| diag.get("range"));
        let Some(r) = range else { continue };

        let severity_int = diag.get("severity").and_then(Value::as_i64).unwrap_or(1);
        let sev_name = match severity_int as i32 {
            severity::ERROR => "error",
            severity::WARNING => "warning",
            severity::INFO => "info",
            severity::HINT => "hint",
            _ => "unknown",
        };

        let message = diag.get("message").and_then(Value::as_str).unwrap_or("");
        let line = r
            .pointer("/start/line")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            + 1;
        let column = r
            .pointer("/start/character")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            + 1;

        items.push(DiagnosticMessage {
            severity: sev_name.to_string(),
            message: message.to_string(),
            line,
            column,
        });
    }
    items
}

/// Handle a `lean_run_code` tool call.
///
/// Writes `code` to a temporary `.lean` file in `project_path`, opens it
/// in the LSP server, collects diagnostics, then always closes and deletes
/// the temp file.
///
/// Returns a [`RunResult`] with `success = true` when there are no error
/// diagnostics.
pub async fn handle_run_code(
    client: &dyn LspClient,
    project_path: &Path,
    code: &str,
) -> Result<RunResult, LeanToolError> {
    // 1. Generate UUID-based temp filename.
    let rel_path = format!("_mcp_snippet_{}.lean", Uuid::new_v4().as_simple());
    let abs_path = project_path.join(&rel_path);

    // 2. Write code to the temp file.
    std::fs::write(&abs_path, code)
        .map_err(|e| LeanToolError::Other(format!("Error writing code snippet: {e}")))?;

    // 3. Open in LSP, get diagnostics -- always clean up afterwards.
    let result = async {
        client
            .open_file(&rel_path)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "open_file".into(),
                message: e.to_string(),
            })?;

        let raw = client
            .get_diagnostics(&rel_path, None, None, Some(15.0))
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_diagnostics".into(),
                message: e.to_string(),
            })?;

        let diagnostics_arr = raw
            .get("diagnostics")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let diagnostics = to_diagnostic_messages(&diagnostics_arr);
        let has_errors = diagnostics.iter().any(|d| d.severity == "error");

        Ok(RunResult {
            success: !has_errors,
            diagnostics,
        })
    }
    .await;

    // 4. Always close the file in LSP and delete the temp file.
    let _ = client.close_files(&[rel_path]).await;
    let _ = std::fs::remove_file(&abs_path);

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tempfile::TempDir;

    /// Mock LSP client for run_code handler tests.
    struct MockRunClient {
        project: PathBuf,
        /// Canned diagnostics response.
        diagnostics_response: Value,
        /// Track whether close_files was called.
        close_called: Arc<AtomicBool>,
    }

    impl MockRunClient {
        fn new(project: PathBuf) -> Self {
            Self {
                project,
                diagnostics_response: json!({
                    "diagnostics": [],
                    "success": true
                }),
                close_called: Arc::new(AtomicBool::new(false)),
            }
        }

        fn with_diagnostics(mut self, diags: Vec<Value>) -> Self {
            self.diagnostics_response = json!({
                "diagnostics": diags,
                "success": true
            });
            self
        }
    }

    #[async_trait]
    impl LspClient for MockRunClient {
        fn project_path(&self) -> &Path {
            &self.project
        }
        async fn open_file(&self, _p: &str) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }
        async fn open_file_force(
            &self,
            _p: &str,
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }
        async fn get_file_content(
            &self,
            _p: &str,
        ) -> Result<String, lean_lsp_client::client::LspClientError> {
            Ok(String::new())
        }
        async fn update_file(
            &self,
            _p: &str,
            _c: Vec<Value>,
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }
        async fn update_file_content(
            &self,
            _p: &str,
            _c: &str,
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }
        async fn close_files(
            &self,
            _p: &[String],
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            self.close_called.store(true, Ordering::SeqCst);
            Ok(())
        }
        async fn get_diagnostics(
            &self,
            _p: &str,
            _sl: Option<u32>,
            _el: Option<u32>,
            _t: Option<f64>,
        ) -> Result<Value, lean_lsp_client::client::LspClientError> {
            Ok(self.diagnostics_response.clone())
        }
        async fn get_interactive_diagnostics(
            &self,
            _p: &str,
            _sl: Option<u32>,
            _el: Option<u32>,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }
        async fn get_goal(
            &self,
            _p: &str,
            _l: u32,
            _c: u32,
        ) -> Result<Option<Value>, lean_lsp_client::client::LspClientError> {
            Ok(None)
        }
        async fn get_term_goal(
            &self,
            _p: &str,
            _l: u32,
            _c: u32,
        ) -> Result<Option<Value>, lean_lsp_client::client::LspClientError> {
            Ok(None)
        }
        async fn get_hover(
            &self,
            _p: &str,
            _l: u32,
            _c: u32,
        ) -> Result<Option<Value>, lean_lsp_client::client::LspClientError> {
            Ok(None)
        }
        async fn get_completions(
            &self,
            _p: &str,
            _l: u32,
            _c: u32,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }
        async fn get_declarations(
            &self,
            _p: &str,
            _l: u32,
            _c: u32,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }
        async fn get_references(
            &self,
            _p: &str,
            _l: u32,
            _c: u32,
            _d: bool,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }
        async fn get_document_symbols(
            &self,
            _p: &str,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }
        async fn get_code_actions(
            &self,
            _p: &str,
            _sl: u32,
            _sc: u32,
            _el: u32,
            _ec: u32,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }
        async fn get_code_action_resolve(
            &self,
            _a: Value,
        ) -> Result<Value, lean_lsp_client::client::LspClientError> {
            Ok(json!({}))
        }
        async fn get_widgets(
            &self,
            _p: &str,
            _l: u32,
            _c: u32,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }
        async fn get_widget_source(
            &self,
            _p: &str,
            _l: u32,
            _c: u32,
            _h: &str,
        ) -> Result<Value, lean_lsp_client::client::LspClientError> {
            Ok(json!({}))
        }
        async fn shutdown(&self) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }
    }

    // ---- successful code (no errors) ----

    #[tokio::test]
    async fn run_code_success_no_errors() {
        let dir = TempDir::new().unwrap();
        let client = MockRunClient::new(dir.path().to_path_buf());

        let result = handle_run_code(&client, dir.path(), "def foo := 42")
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.diagnostics.is_empty());
    }

    // ---- code with errors ----

    #[tokio::test]
    async fn run_code_with_errors() {
        let dir = TempDir::new().unwrap();
        let client = MockRunClient::new(dir.path().to_path_buf()).with_diagnostics(vec![json!({
            "range": {
                "start": {"line": 0, "character": 4},
                "end": {"line": 0, "character": 10}
            },
            "severity": 1,
            "message": "unknown identifier 'bad'"
        })]);

        let result = handle_run_code(&client, dir.path(), "def x := bad")
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].severity, "error");
        assert_eq!(result.diagnostics[0].message, "unknown identifier 'bad'");
        assert_eq!(result.diagnostics[0].line, 1);
        assert_eq!(result.diagnostics[0].column, 5);
    }

    // ---- temp file cleanup after success ----

    #[tokio::test]
    async fn run_code_cleans_up_temp_file() {
        let dir = TempDir::new().unwrap();
        let client = MockRunClient::new(dir.path().to_path_buf());

        let _ = handle_run_code(&client, dir.path(), "#check Nat")
            .await
            .unwrap();

        // No _mcp_snippet_*.lean files should remain.
        let remaining: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("_mcp_snippet_"))
            .collect();
        assert!(remaining.is_empty(), "temp file was not cleaned up");
    }

    // ---- close_files is always called ----

    #[tokio::test]
    async fn run_code_calls_close_files() {
        let dir = TempDir::new().unwrap();
        let client = MockRunClient::new(dir.path().to_path_buf());
        let close_flag = client.close_called.clone();

        let _ = handle_run_code(&client, dir.path(), "#check Nat").await;

        assert!(
            close_flag.load(Ordering::SeqCst),
            "close_files should be called"
        );
    }

    // ---- code with warnings only counts as success ----

    #[tokio::test]
    async fn run_code_warnings_only_counts_as_success() {
        let dir = TempDir::new().unwrap();
        let client = MockRunClient::new(dir.path().to_path_buf()).with_diagnostics(vec![json!({
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 5}
            },
            "severity": 2,
            "message": "unused variable"
        })]);

        let result = handle_run_code(&client, dir.path(), "def x := 42")
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].severity, "warning");
    }

    // ---- to_diagnostic_messages unit tests ----

    #[test]
    fn to_diagnostic_messages_converts_correctly() {
        let diags = vec![
            json!({
                "range": {"start": {"line": 4, "character": 2}, "end": {"line": 4, "character": 10}},
                "severity": 1,
                "message": "unknown id"
            }),
            json!({
                "fullRange": {"start": {"line": 10, "character": 3}, "end": {"line": 12, "character": 0}},
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}},
                "severity": 3,
                "message": "info msg"
            }),
        ];

        let items = to_diagnostic_messages(&diags);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].severity, "error");
        assert_eq!(items[0].line, 5);
        assert_eq!(items[0].column, 3);
        // fullRange takes precedence
        assert_eq!(items[1].severity, "info");
        assert_eq!(items[1].line, 11);
        assert_eq!(items[1].column, 4);
    }

    #[test]
    fn to_diagnostic_messages_skips_missing_range() {
        let diags = vec![json!({
            "severity": 1,
            "message": "no range"
        })];
        let items = to_diagnostic_messages(&diags);
        assert!(items.is_empty());
    }
}
