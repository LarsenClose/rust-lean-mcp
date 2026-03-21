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
///
/// When `context_lines` is provided (> 0), diagnostics from lines before
/// `context_lines` (0-indexed) are filtered out, and line numbers in the
/// remaining diagnostics are adjusted so they are relative to the snippet.
fn to_diagnostic_messages(diagnostics: &[Value], context_lines: usize) -> Vec<DiagnosticMessage> {
    let mut items = Vec::new();
    for diag in diagnostics {
        let range = diag.get("fullRange").or_else(|| diag.get("range"));
        let Some(r) = range else { continue };

        let raw_line = r
            .pointer("/start/line")
            .and_then(Value::as_i64)
            .unwrap_or(0);

        // Filter out diagnostics from the context region.
        if context_lines > 0 && (raw_line as usize) < context_lines {
            continue;
        }

        let severity_int = diag.get("severity").and_then(Value::as_i64).unwrap_or(1);
        let sev_name = match severity_int as i32 {
            severity::ERROR => "error",
            severity::WARNING => "warning",
            severity::INFO => "info",
            severity::HINT => "hint",
            _ => "unknown",
        };

        let message = diag.get("message").and_then(Value::as_str).unwrap_or("");
        // Adjust line number relative to snippet (subtract context lines).
        let line = raw_line - context_lines as i64 + 1;
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
/// When `file_context` is provided, the content of the given file is read
/// and prepended to the snippet so it inherits the file's imports,
/// namespaces, and section variables. Diagnostics from the context region
/// are filtered out and line numbers are adjusted to be relative to the
/// snippet.
///
/// Returns a [`RunResult`] with `success = true` when there are no error
/// diagnostics.
pub async fn handle_run_code(
    client: &dyn LspClient,
    project_path: &Path,
    code: &str,
    file_context: Option<&str>,
) -> Result<RunResult, LeanToolError> {
    // 1. Generate UUID-based temp filename inside .lake/_mcp/ to avoid git pollution.
    let mcp_dir = project_path.join(".lake").join("_mcp");
    std::fs::create_dir_all(&mcp_dir)
        .map_err(|e| LeanToolError::Other(format!("Error creating .lake/_mcp dir: {e}")))?;
    let filename = format!("_mcp_snippet_{}.lean", Uuid::new_v4().as_simple());
    let abs_path = mcp_dir.join(&filename);
    let rel_path = format!(".lake/_mcp/{filename}");

    // 2. Build the file content, optionally prepending context from an existing file.
    let (file_content, context_lines) = if let Some(ctx_path) = file_context {
        // Read the context file. Try the absolute path first, then relative to project.
        let abs_ctx = if Path::new(ctx_path).is_absolute() {
            Path::new(ctx_path).to_path_buf()
        } else {
            project_path.join(ctx_path)
        };
        let context_content = std::fs::read_to_string(&abs_ctx).map_err(|e| {
            LeanToolError::Other(format!(
                "Error reading file_context '{}': {e}",
                abs_ctx.display()
            ))
        })?;
        let ctx_lines = context_content.lines().count();
        // When using file_context, skip maxHeartbeats injection since
        // the context file may have its own options.
        let combined = format!("{}\n{}", context_content, code);
        (combined, ctx_lines)
    } else {
        // No context: inject maxHeartbeats as before.
        let code_with_heartbeats = super::prepend_max_heartbeats(code);
        (code_with_heartbeats, 0)
    };

    std::fs::write(&abs_path, &file_content)
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

        let diagnostics = to_diagnostic_messages(&diagnostics_arr, context_lines);
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

        let result = handle_run_code(&client, dir.path(), "def foo := 42", None)
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

        let result = handle_run_code(&client, dir.path(), "def x := bad", None)
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

        let _ = handle_run_code(&client, dir.path(), "#check Nat", None)
            .await
            .unwrap();

        // No _mcp_snippet_*.lean files should remain in .lake/_mcp/.
        let mcp_dir = dir.path().join(".lake").join("_mcp");
        if mcp_dir.exists() {
            let remaining: Vec<_> = std::fs::read_dir(&mcp_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("_mcp_snippet_"))
                .collect();
            assert!(remaining.is_empty(), "temp file was not cleaned up");
        }
        // Also verify no temp files leaked to project root.
        let root_remaining: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("_mcp_snippet_"))
            .collect();
        assert!(
            root_remaining.is_empty(),
            "temp file should not be in project root"
        );
    }

    // ---- temp files are created in .lake/_mcp/ ----

    #[tokio::test]
    async fn temp_files_in_lake_mcp_dir() {
        let dir = TempDir::new().unwrap();

        // Use a mock that tracks opened file paths.
        struct PathTrackingClient {
            project: PathBuf,
            opened: std::sync::Mutex<Vec<String>>,
        }

        #[async_trait]
        impl LspClient for PathTrackingClient {
            fn project_path(&self) -> &Path {
                &self.project
            }
            async fn open_file(
                &self,
                p: &str,
            ) -> Result<(), lean_lsp_client::client::LspClientError> {
                self.opened.lock().unwrap().push(p.to_string());
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
                Ok(())
            }
            async fn get_diagnostics(
                &self,
                _p: &str,
                _sl: Option<u32>,
                _el: Option<u32>,
                _t: Option<f64>,
            ) -> Result<Value, lean_lsp_client::client::LspClientError> {
                Ok(json!({"diagnostics": [], "success": true}))
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

        let client = PathTrackingClient {
            project: dir.path().to_path_buf(),
            opened: std::sync::Mutex::new(Vec::new()),
        };

        let _ = handle_run_code(&client, dir.path(), "#check Nat", None)
            .await
            .unwrap();

        let opened = client.opened.lock().unwrap();
        assert_eq!(opened.len(), 1);
        assert!(
            opened[0].starts_with(".lake/_mcp/_mcp_snippet_"),
            "temp file should be in .lake/_mcp/, got: {}",
            opened[0]
        );
    }

    // ---- close_files is always called ----

    #[tokio::test]
    async fn run_code_calls_close_files() {
        let dir = TempDir::new().unwrap();
        let client = MockRunClient::new(dir.path().to_path_buf());
        let close_flag = client.close_called.clone();

        let _ = handle_run_code(&client, dir.path(), "#check Nat", None).await;

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

        let result = handle_run_code(&client, dir.path(), "def x := 42", None)
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

        let items = to_diagnostic_messages(&diags, 0);
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
        let items = to_diagnostic_messages(&diags, 0);
        assert!(items.is_empty());
    }

    // ---- file_context tests ----

    #[test]
    fn to_diagnostic_messages_filters_context_lines() {
        // Simulate context file with 10 lines; diagnostics from context (line 3)
        // and snippet (line 12) regions.
        let diags = vec![
            json!({
                "range": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 5}},
                "severity": 1,
                "message": "context error"
            }),
            json!({
                "range": {"start": {"line": 12, "character": 4}, "end": {"line": 12, "character": 10}},
                "severity": 1,
                "message": "snippet error"
            }),
        ];

        let items = to_diagnostic_messages(&diags, 10);
        // Only the snippet diagnostic should remain.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].message, "snippet error");
        // Line 12 (0-indexed) - 10 context lines + 1 (1-indexed) = 3
        assert_eq!(items[0].line, 3);
        assert_eq!(items[0].column, 5);
    }

    #[test]
    fn to_diagnostic_messages_adjusts_line_numbers_with_context() {
        // Context has 5 lines; diagnostic at 0-indexed line 5 => snippet line 1.
        let diags = vec![json!({
            "range": {"start": {"line": 5, "character": 0}, "end": {"line": 5, "character": 5}},
            "severity": 2,
            "message": "warning in snippet"
        })];

        let items = to_diagnostic_messages(&diags, 5);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].line, 1);
        assert_eq!(items[0].severity, "warning");
    }

    #[tokio::test]
    async fn run_code_with_file_context_prepends_content() {
        let dir = TempDir::new().unwrap();
        // Create a context file with some imports.
        let ctx_file = dir.path().join("MyModule.lean");
        std::fs::write(&ctx_file, "import Init\n\ndef helper := 42\n").unwrap();

        // Create .lake/_mcp so the handler can write temp files.
        std::fs::create_dir_all(dir.path().join(".lake").join("_mcp")).unwrap();

        // Mock client that captures the written file content.
        struct ContentCapturingClient {
            project: PathBuf,
        }

        #[async_trait]
        impl LspClient for ContentCapturingClient {
            fn project_path(&self) -> &Path {
                &self.project
            }
            async fn open_file(
                &self,
                p: &str,
            ) -> Result<(), lean_lsp_client::client::LspClientError> {
                // Verify the temp file contains the context content.
                let abs = self.project.join(p);
                let content = std::fs::read_to_string(&abs).unwrap();
                assert!(
                    content.starts_with("import Init"),
                    "temp file should start with context content, got: {content}"
                );
                assert!(
                    content.contains("def helper := 42"),
                    "temp file should contain context content"
                );
                assert!(
                    content.contains("#check helper"),
                    "temp file should contain snippet code"
                );
                // Verify maxHeartbeats is NOT injected when file_context is used.
                assert!(
                    !content.contains("maxHeartbeats"),
                    "maxHeartbeats should not be injected with file_context"
                );
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
                Ok(())
            }
            async fn get_diagnostics(
                &self,
                _p: &str,
                _sl: Option<u32>,
                _el: Option<u32>,
                _t: Option<f64>,
            ) -> Result<Value, lean_lsp_client::client::LspClientError> {
                // Return a diagnostic in the context region (should be filtered)
                // and one in the snippet region (should be kept).
                // Context is 3 lines ("import Init\n\ndef helper := 42\n"),
                // so snippet starts at 0-indexed line 3.
                Ok(json!({
                    "diagnostics": [
                        {
                            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}},
                            "severity": 3,
                            "message": "context info"
                        },
                        {
                            "range": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 10}},
                            "severity": 3,
                            "message": "snippet info"
                        }
                    ],
                    "success": true
                }))
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

        let client = ContentCapturingClient {
            project: dir.path().to_path_buf(),
        };

        let result = handle_run_code(
            &client,
            dir.path(),
            "#check helper",
            Some(ctx_file.to_str().unwrap()),
        )
        .await
        .unwrap();

        // Only the snippet diagnostic should be returned.
        assert!(result.success);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].message, "snippet info");
        // 0-indexed line 3 - 3 context lines + 1 = 1
        assert_eq!(result.diagnostics[0].line, 1);
    }

    #[tokio::test]
    async fn run_code_without_file_context_unchanged() {
        // Verify that calling without file_context behaves exactly as before.
        let dir = TempDir::new().unwrap();
        let client = MockRunClient::new(dir.path().to_path_buf()).with_diagnostics(vec![json!({
            "range": {
                "start": {"line": 2, "character": 0},
                "end": {"line": 2, "character": 5}
            },
            "severity": 1,
            "message": "some error"
        })]);

        let result = handle_run_code(&client, dir.path(), "def x := bad", None)
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(result.diagnostics.len(), 1);
        // Line should be 2 (0-indexed) + 1 = 3 with no context adjustment.
        assert_eq!(result.diagnostics[0].line, 3);
    }

    #[tokio::test]
    async fn run_code_file_context_missing_file_returns_error() {
        let dir = TempDir::new().unwrap();
        let client = MockRunClient::new(dir.path().to_path_buf());

        let result = handle_run_code(
            &client,
            dir.path(),
            "#check Nat",
            Some("/nonexistent/path/File.lean"),
        )
        .await;

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Error reading file_context"),
            "expected file read error, got: {err_msg}"
        );
    }
}
