//! Tool handler for `lean_references`.
//!
//! Finds all references to a symbol at a given position, including the
//! declaration itself. Returns locations with 1-indexed positions and
//! absolute file paths.

use lean_lsp_client::client::{uri_to_path, LspClient};
use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::models::{ReferenceLocation, ReferencesResult};
use serde_json::Value;

/// Handle a `lean_references` tool call.
///
/// Finds all references to the symbol at `(line, column)`, including the
/// declaration site.
///
/// `line` and `column` are **1-indexed** (matching the MCP tool interface).
/// They are converted to 0-indexed for LSP calls internally.
pub async fn handle_references(
    client: &dyn LspClient,
    file_path: &str,
    line: u32,
    column: u32,
) -> Result<ReferencesResult, LeanToolError> {
    // 1. Open the file in the LSP server.
    client
        .open_file(file_path)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "open_file".into(),
            message: e.to_string(),
        })?;

    // 2. Ensure elaboration by fetching diagnostics first.
    client
        .get_diagnostics(file_path, None, None, None)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "get_diagnostics".into(),
            message: e.to_string(),
        })?;

    // 3. Convert 1-indexed to 0-indexed.
    let lsp_line = line.saturating_sub(1);
    let lsp_col = column.saturating_sub(1);

    // 4. Get references with include_declaration=true.
    let raw_refs = client
        .get_references(file_path, lsp_line, lsp_col, true)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "get_references".into(),
            message: e.to_string(),
        })?;

    // 5. Convert raw LSP locations to the MCP model.
    let items: Vec<ReferenceLocation> = raw_refs
        .iter()
        .filter_map(|loc| {
            let uri = loc.get("uri")?.as_str()?;
            let range = loc.get("range")?;

            let abs_path = uri_to_path(uri)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();

            let start_line = range
                .pointer("/start/line")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                + 1;
            let start_col = range
                .pointer("/start/character")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                + 1;
            let end_line = range
                .pointer("/end/line")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                + 1;
            let end_col = range
                .pointer("/end/character")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                + 1;

            Some(ReferenceLocation {
                file_path: abs_path,
                line: start_line,
                column: start_col,
                end_line,
                end_column: end_col,
            })
        })
        .collect();

    Ok(ReferencesResult { items })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Mock LSP client for references handler tests.
    struct MockRefClient {
        project: PathBuf,
        /// Canned response for `get_references`.
        references_response: Vec<Value>,
        /// Track whether `get_diagnostics` was called (for elaboration check).
        diagnostics_called: AtomicBool,
    }

    impl MockRefClient {
        fn new() -> Self {
            Self {
                project: PathBuf::from("/test/project"),
                references_response: Vec::new(),
                diagnostics_called: AtomicBool::new(false),
            }
        }

        fn with_references(mut self, refs: Vec<Value>) -> Self {
            self.references_response = refs;
            self
        }
    }

    #[async_trait]
    impl LspClient for MockRefClient {
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
            Ok(())
        }
        async fn get_diagnostics(
            &self,
            _p: &str,
            _sl: Option<u32>,
            _el: Option<u32>,
            _t: Option<f64>,
        ) -> Result<Value, lean_lsp_client::client::LspClientError> {
            self.diagnostics_called.store(true, Ordering::SeqCst);
            Ok(json!({}))
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
            Ok(self.references_response.clone())
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

    // ---- references returns locations with 1-indexed positions ----

    #[tokio::test]
    async fn references_returns_locations_with_1_indexed_positions() {
        let client = MockRefClient::new().with_references(vec![
            json!({
                "uri": "file:///test/project/Main.lean",
                "range": {
                    "start": {"line": 4, "character": 10},
                    "end": {"line": 4, "character": 13}
                }
            }),
            json!({
                "uri": "file:///test/project/Util.lean",
                "range": {
                    "start": {"line": 9, "character": 0},
                    "end": {"line": 9, "character": 3}
                }
            }),
        ]);

        let result = handle_references(&client, "Main.lean", 5, 11)
            .await
            .unwrap();

        assert_eq!(result.items.len(), 2);

        assert_eq!(result.items[0].file_path, "/test/project/Main.lean");
        assert_eq!(result.items[0].line, 5);
        assert_eq!(result.items[0].column, 11);
        assert_eq!(result.items[0].end_line, 5);
        assert_eq!(result.items[0].end_column, 14);

        assert_eq!(result.items[1].file_path, "/test/project/Util.lean");
        assert_eq!(result.items[1].line, 10);
        assert_eq!(result.items[1].column, 1);
    }

    // ---- references with no results returns empty list ----

    #[tokio::test]
    async fn references_with_no_results_returns_empty_list() {
        let client = MockRefClient::new();

        let result = handle_references(&client, "Main.lean", 1, 1).await.unwrap();

        assert!(result.items.is_empty());
    }

    // ---- references ensures elaboration before querying ----

    #[tokio::test]
    async fn references_ensures_elaboration_before_querying() {
        let client = MockRefClient::new();

        let _ = handle_references(&client, "Main.lean", 1, 1).await.unwrap();

        assert!(
            client.diagnostics_called.load(Ordering::SeqCst),
            "get_diagnostics should be called to ensure elaboration"
        );
    }

    // ---- references skips entries with missing uri or range ----

    #[tokio::test]
    async fn references_skips_malformed_entries() {
        let client = MockRefClient::new().with_references(vec![
            json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}}}), // no uri
            json!({"uri": "file:///a.lean"}), // no range
            json!({
                "uri": "file:///good.lean",
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 3}
                }
            }),
        ]);

        let result = handle_references(&client, "Main.lean", 1, 1).await.unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].file_path, "/good.lean");
    }

    // ---- references handles multiple files ----

    #[tokio::test]
    async fn references_handles_multiple_files() {
        let client = MockRefClient::new().with_references(vec![
            json!({
                "uri": "file:///project/A.lean",
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}}
            }),
            json!({
                "uri": "file:///project/B.lean",
                "range": {"start": {"line": 10, "character": 3}, "end": {"line": 10, "character": 8}}
            }),
            json!({
                "uri": "file:///project/C.lean",
                "range": {"start": {"line": 20, "character": 7}, "end": {"line": 22, "character": 0}}
            }),
        ]);

        let result = handle_references(&client, "A.lean", 1, 1).await.unwrap();

        assert_eq!(result.items.len(), 3);
        assert_eq!(result.items[0].file_path, "/project/A.lean");
        assert_eq!(result.items[1].file_path, "/project/B.lean");
        assert_eq!(result.items[2].file_path, "/project/C.lean");
        assert_eq!(result.items[2].line, 21);
        assert_eq!(result.items[2].end_line, 23);
    }
}
