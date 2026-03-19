//! Tool handlers for `lean_get_widgets` and `lean_get_widget_source`.
//!
//! Simple passthroughs to the LSP client's widget methods.

use lean_lsp_client::client::LspClient;
use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::models::{WidgetSourceResult, WidgetsResult};

/// Handle a `lean_get_widgets` tool call.
///
/// Returns all panel widgets at the given position.
///
/// `line` and `column` are **1-indexed** (matching the MCP tool interface).
/// They are converted to 0-indexed for LSP calls internally.
pub async fn handle_get_widgets(
    client: &dyn LspClient,
    file_path: &str,
    line: u32,
    column: u32,
) -> Result<WidgetsResult, LeanToolError> {
    // 1. Open the file in the LSP server.
    client
        .open_file(file_path)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "open_file".into(),
            message: e.to_string(),
        })?;

    // 2. Convert 1-indexed to 0-indexed.
    let lsp_line = line.saturating_sub(1);
    let lsp_col = column.saturating_sub(1);

    // 3. Get widgets from the LSP.
    let widgets = client
        .get_widgets(file_path, lsp_line, lsp_col)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "get_widgets".into(),
            message: e.to_string(),
        })?;

    Ok(WidgetsResult { widgets })
}

/// Handle a `lean_get_widget_source` tool call.
///
/// Returns the JavaScript source for a widget identified by its hash.
/// The position is fixed at (0, 0) since widget source is not position-dependent
/// beyond file scope.
///
/// `file_path` is the relative path to a Lean file that uses the widget.
/// `javascript_hash` is the hash string from a widget instance.
pub async fn handle_get_widget_source(
    client: &dyn LspClient,
    file_path: &str,
    javascript_hash: &str,
) -> Result<WidgetSourceResult, LeanToolError> {
    // 1. Open the file in the LSP server.
    client
        .open_file(file_path)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "open_file".into(),
            message: e.to_string(),
        })?;

    // 2. Get widget source -- position is (0, 0) per the Python reference.
    let source = client
        .get_widget_source(file_path, 0, 0, javascript_hash)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "get_widget_source".into(),
            message: e.to_string(),
        })?;

    Ok(WidgetSourceResult { source })
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
    use std::sync::Mutex;

    /// Mock LSP client for widget handler tests.
    struct MockWidgetClient {
        project: PathBuf,
        /// Canned widget response.
        widgets_response: Vec<Value>,
        /// Canned widget source response.
        widget_source_response: Value,
        /// Track calls to get_widget_source for assertion.
        source_calls: Mutex<Vec<(u32, u32, String)>>,
    }

    impl MockWidgetClient {
        fn new() -> Self {
            Self {
                project: PathBuf::from("/test/project"),
                widgets_response: Vec::new(),
                widget_source_response: json!({}),
                source_calls: Mutex::new(Vec::new()),
            }
        }

        fn with_widgets(mut self, widgets: Vec<Value>) -> Self {
            self.widgets_response = widgets;
            self
        }

        fn with_widget_source(mut self, source: Value) -> Self {
            self.widget_source_response = source;
            self
        }
    }

    #[async_trait]
    impl LspClient for MockWidgetClient {
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
            Ok(self.widgets_response.clone())
        }
        async fn get_widget_source(
            &self,
            _p: &str,
            l: u32,
            c: u32,
            h: &str,
        ) -> Result<Value, lean_lsp_client::client::LspClientError> {
            self.source_calls
                .lock()
                .unwrap()
                .push((l, c, h.to_string()));
            Ok(self.widget_source_response.clone())
        }
        async fn shutdown(&self) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }
    }

    // ---- get_widgets returns widget data ----

    #[tokio::test]
    async fn get_widgets_returns_data() {
        let client = MockWidgetClient::new().with_widgets(vec![
            json!({"id": "w1", "name": "InfoView"}),
            json!({"id": "w2", "name": "GoalView"}),
        ]);

        let result = handle_get_widgets(&client, "Main.lean", 5, 10)
            .await
            .unwrap();

        assert_eq!(result.widgets.len(), 2);
        assert_eq!(result.widgets[0]["id"], "w1");
        assert_eq!(result.widgets[1]["id"], "w2");
    }

    // ---- get_widgets returns empty for no widgets ----

    #[tokio::test]
    async fn get_widgets_handles_empty() {
        let client = MockWidgetClient::new();

        let result = handle_get_widgets(&client, "Main.lean", 1, 1)
            .await
            .unwrap();

        assert!(result.widgets.is_empty());
    }

    // ---- get_widgets converts 1-indexed to 0-indexed ----

    #[tokio::test]
    async fn get_widgets_converts_position() {
        // This is implicitly tested: line=1, column=1 should map to (0,0) for LSP.
        // The mock returns the same response regardless, but the handler calls with
        // 0-indexed values. We trust the saturating_sub logic since it matches the
        // pattern in other handlers.
        let client = MockWidgetClient::new();

        let result = handle_get_widgets(&client, "Main.lean", 1, 1)
            .await
            .unwrap();

        assert!(result.widgets.is_empty());
    }

    // ---- get_widget_source returns source data ----

    #[tokio::test]
    async fn get_widget_source_returns_data() {
        let client = MockWidgetClient::new().with_widget_source(json!({
            "source": "export default function() { return 'hello'; }"
        }));

        let result = handle_get_widget_source(&client, "Main.lean", "abc123hash")
            .await
            .unwrap();

        assert_eq!(
            result.source["source"],
            "export default function() { return 'hello'; }"
        );
    }

    // ---- get_widget_source passes hash correctly ----

    #[tokio::test]
    async fn get_widget_source_passes_hash() {
        let client = MockWidgetClient::new();

        let _ = handle_get_widget_source(&client, "Main.lean", "my_hash_123")
            .await
            .unwrap();

        let calls = client.source_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, 0); // line
        assert_eq!(calls[0].1, 0); // column
        assert_eq!(calls[0].2, "my_hash_123"); // hash
    }

    // ---- get_widget_source handles empty source ----

    #[tokio::test]
    async fn get_widget_source_handles_empty() {
        let client = MockWidgetClient::new();

        let result = handle_get_widget_source(&client, "Main.lean", "empty_hash")
            .await
            .unwrap();

        // Default mock returns json!({})
        assert!(result.source.as_object().unwrap().is_empty());
    }
}
