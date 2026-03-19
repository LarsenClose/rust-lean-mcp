//! Tool handler for `lean_diagnostic_messages`.
//!
//! Retrieves compiler diagnostics (errors, warnings, infos, hints) for a Lean
//! file. Supports line-range filtering, declaration-name filtering, severity
//! filtering, interactive mode, and build-error extraction.

use lean_lsp_client::client::LspClient;
use lean_lsp_client::types::severity;
use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::models::{DiagnosticMessage, DiagnosticsResult, InteractiveDiagnosticsResult};
use regex::Regex;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Build-stderr helpers
// ---------------------------------------------------------------------------

/// Regex matching `error: path/file.lean:line:col:` or `warning: ...` lines
/// in `lake build` stderr output.
fn build_error_file_pattern() -> Regex {
    Regex::new(r"(?im)^(?:error|warning):\s*([^\s:]+\.lean):\d+:\d+:").unwrap()
}

/// Return `true` when `message` looks like `lake build` stderr output.
fn is_build_stderr(message: &str) -> bool {
    message.contains("lake setup-file") || build_error_file_pattern().is_match(message)
}

/// Extract unique, sorted `.lean` file paths from `lake build` stderr.
fn extract_failed_dependency_paths(message: &str) -> Vec<String> {
    let re = build_error_file_pattern();
    let mut paths: Vec<String> = re
        .captures_iter(message)
        .filter_map(|cap| cap.get(1).map(|m| m.as_str().to_string()))
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

// ---------------------------------------------------------------------------
// Declaration range via document symbols
// ---------------------------------------------------------------------------

/// Recursively search document symbols for `target_name` (case-sensitive).
fn search_symbols<'a>(symbols: &'a [Value], target_name: &str) -> Option<&'a Value> {
    for symbol in symbols {
        if symbol.get("name").and_then(Value::as_str) == Some(target_name) {
            return Some(symbol);
        }
        if let Some(children) = symbol.get("children").and_then(Value::as_array) {
            if let Some(found) = search_symbols(children, target_name) {
                return Some(found);
            }
        }
    }
    None
}

/// Resolve a declaration name to its 1-indexed `(start_line, end_line)` range.
async fn get_declaration_range(
    client: &dyn LspClient,
    file_path: &str,
    declaration_name: &str,
) -> Result<Option<(u32, u32)>, LeanToolError> {
    let symbols =
        client
            .get_document_symbols(file_path)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_document_symbols".into(),
                message: e.to_string(),
            })?;

    let Some(sym) = search_symbols(&symbols, declaration_name) else {
        return Ok(None);
    };

    let Some(range) = sym.get("range") else {
        return Ok(None);
    };

    let start_line = range
        .pointer("/start/line")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32
        + 1;
    let end_line = range
        .pointer("/end/line")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32
        + 1;

    Ok(Some((start_line, end_line)))
}

// ---------------------------------------------------------------------------
// Severity name mapping
// ---------------------------------------------------------------------------

/// Map an LSP severity integer to a lowercase string.
fn severity_name(sev: i64) -> String {
    match sev as i32 {
        severity::ERROR => "error".into(),
        severity::WARNING => "warning".into(),
        severity::INFO => "info".into(),
        severity::HINT => "hint".into(),
        other => format!("unknown({other})"),
    }
}

// ---------------------------------------------------------------------------
// Process raw diagnostics
// ---------------------------------------------------------------------------

/// Convert raw LSP diagnostics into the MCP result model.
///
/// Detects build stderr at position (1,1), extracts failed dependency paths,
/// and optionally filters by severity.
fn process_diagnostics(
    diagnostics: &[Value],
    build_success: bool,
    severity_filter: Option<&str>,
) -> DiagnosticsResult {
    let mut items: Vec<DiagnosticMessage> = Vec::new();
    let mut failed_deps: Vec<String> = Vec::new();

    for diag in diagnostics {
        let range = diag.get("fullRange").or_else(|| diag.get("range"));
        let Some(r) = range else { continue };

        let severity_int = diag.get("severity").and_then(Value::as_i64).unwrap_or(1);
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

        // Build stderr at (1,1) — extract dependency paths, skip the item.
        if line == 1 && column == 1 && is_build_stderr(message) {
            failed_deps = extract_failed_dependency_paths(message);
            continue;
        }

        let sev_str = severity_name(severity_int);
        if let Some(filter) = severity_filter {
            if sev_str != filter {
                continue;
            }
        }

        items.push(DiagnosticMessage {
            severity: sev_str,
            message: message.to_string(),
            line,
            column,
        });
    }

    DiagnosticsResult {
        success: build_success,
        items,
        failed_dependencies: failed_deps,
    }
}

// ---------------------------------------------------------------------------
// Public handler
// ---------------------------------------------------------------------------

/// Handle a `lean_diagnostic_messages` tool call.
///
/// Supports line-range filtering, declaration-name filtering, severity
/// filtering, interactive mode, and build-error extraction.
///
/// `start_line` and `end_line` are **1-indexed** (matching the MCP tool
/// interface). They are converted to 0-indexed for LSP calls internally.
pub async fn handle_diagnostics(
    client: &dyn LspClient,
    file_path: &str,
    start_line: Option<u32>,
    end_line: Option<u32>,
    declaration_name: Option<&str>,
    interactive: bool,
    severity_filter: Option<&str>,
) -> Result<serde_json::Value, LeanToolError> {
    // 1. Open file in the LSP server.
    client
        .open_file(file_path)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "open_file".into(),
            message: e.to_string(),
        })?;

    // 2. If declaration_name is given, resolve its range via document symbols.
    let (start_line, end_line) = if let Some(name) = declaration_name {
        let range = get_declaration_range(client, file_path, name).await?;
        match range {
            Some((s, e)) => (Some(s), Some(e)),
            None => return Err(LeanToolError::DeclarationNotFound(name.to_string())),
        }
    } else {
        (start_line, end_line)
    };

    // 3. Convert 1-indexed to 0-indexed for LSP.
    let start_line_0 = start_line.map(|l| l.saturating_sub(1));
    let end_line_0 = end_line.map(|l| l.saturating_sub(1));

    // 4. Interactive mode: return InteractiveDiagnosticsResult.
    if interactive {
        let diags = client
            .get_interactive_diagnostics(file_path, start_line_0, end_line_0)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_interactive_diagnostics".into(),
                message: e.to_string(),
            })?;
        let result = InteractiveDiagnosticsResult { diagnostics: diags };
        return serde_json::to_value(&result).map_err(|e| LeanToolError::Other(e.to_string()));
    }

    // 5. Standard diagnostics.
    let raw = client
        .get_diagnostics(file_path, start_line_0, end_line_0, Some(15.0))
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "get_diagnostics".into(),
            message: e.to_string(),
        })?;

    // The raw response has { diagnostics: [...], success: bool }.
    let diagnostics_arr = raw
        .get("diagnostics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let build_success = raw.get("success").and_then(Value::as_bool).unwrap_or(true);

    let result = process_diagnostics(&diagnostics_arr, build_success, severity_filter);
    serde_json::to_value(&result).map_err(|e| LeanToolError::Other(e.to_string()))
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

    /// Mock LSP client for diagnostics handler tests.
    struct MockDiagClient {
        project: PathBuf,
        /// Canned response for `get_diagnostics`.
        diagnostics_response: Value,
        /// Canned response for `get_interactive_diagnostics`.
        interactive_response: Vec<Value>,
        /// Canned response for `get_document_symbols`.
        symbols_response: Vec<Value>,
    }

    impl MockDiagClient {
        fn new() -> Self {
            Self {
                project: PathBuf::from("/test/project"),
                diagnostics_response: json!({
                    "diagnostics": [],
                    "success": true
                }),
                interactive_response: Vec::new(),
                symbols_response: Vec::new(),
            }
        }

        fn with_diagnostics(mut self, diags: Vec<Value>, success: bool) -> Self {
            self.diagnostics_response = json!({
                "diagnostics": diags,
                "success": success
            });
            self
        }

        fn with_interactive(mut self, diags: Vec<Value>) -> Self {
            self.interactive_response = diags;
            self
        }

        fn with_symbols(mut self, symbols: Vec<Value>) -> Self {
            self.symbols_response = symbols;
            self
        }
    }

    #[async_trait]
    impl LspClient for MockDiagClient {
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
            Ok(self.diagnostics_response.clone())
        }
        async fn get_interactive_diagnostics(
            &self,
            _p: &str,
            _sl: Option<u32>,
            _el: Option<u32>,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(self.interactive_response.clone())
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
            Ok(self.symbols_response.clone())
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

    // ---- is_build_stderr ----

    #[test]
    fn build_stderr_detected_with_lake_setup() {
        assert!(is_build_stderr("lake setup-file: error processing..."));
    }

    #[test]
    fn build_stderr_detected_with_error_pattern() {
        assert!(is_build_stderr(
            "error: Mathlib/Tactic/Ring.lean:42:5: some error"
        ));
    }

    #[test]
    fn build_stderr_not_detected_for_normal_message() {
        assert!(!is_build_stderr("unknown identifier 'foo'"));
    }

    // ---- extract_failed_dependency_paths ----

    #[test]
    fn extract_deps_from_build_stderr() {
        let msg = "error: Mathlib/Tactic/Ring.lean:42:5: foo\n\
                   warning: Mathlib/Data/Nat.lean:10:1: bar\n\
                   error: Mathlib/Tactic/Ring.lean:50:3: baz";
        let deps = extract_failed_dependency_paths(msg);
        assert_eq!(
            deps,
            vec!["Mathlib/Data/Nat.lean", "Mathlib/Tactic/Ring.lean"]
        );
    }

    #[test]
    fn extract_deps_empty_for_normal_message() {
        let deps = extract_failed_dependency_paths("unknown identifier 'foo'");
        assert!(deps.is_empty());
    }

    // ---- severity_name ----

    #[test]
    fn severity_name_maps_all_known_values() {
        assert_eq!(severity_name(1), "error");
        assert_eq!(severity_name(2), "warning");
        assert_eq!(severity_name(3), "info");
        assert_eq!(severity_name(4), "hint");
        assert_eq!(severity_name(99), "unknown(99)");
    }

    // ---- process_diagnostics ----

    #[test]
    fn process_diagnostics_returns_filtered_items() {
        let diags = vec![
            json!({
                "range": {"start": {"line": 4, "character": 2}, "end": {"line": 4, "character": 10}},
                "severity": 1,
                "message": "unknown identifier"
            }),
            json!({
                "range": {"start": {"line": 6, "character": 0}, "end": {"line": 6, "character": 5}},
                "severity": 2,
                "message": "unused variable"
            }),
        ];
        let result = process_diagnostics(&diags, true, None);
        assert!(result.success);
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].severity, "error");
        assert_eq!(result.items[0].line, 5); // 0-indexed + 1
        assert_eq!(result.items[0].column, 3);
        assert_eq!(result.items[1].severity, "warning");
    }

    #[test]
    fn process_diagnostics_handles_build_stderr_extraction() {
        let diags = vec![
            json!({
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                "severity": 1,
                "message": "error: Mathlib/Tactic/Ring.lean:42:5: build failure"
            }),
            json!({
                "range": {"start": {"line": 9, "character": 4}, "end": {"line": 9, "character": 10}},
                "severity": 1,
                "message": "type mismatch"
            }),
        ];
        let result = process_diagnostics(&diags, false, None);
        assert!(!result.success);
        assert_eq!(result.failed_dependencies, vec!["Mathlib/Tactic/Ring.lean"]);
        // Build stderr item at (1,1) should be excluded
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].message, "type mismatch");
    }

    #[test]
    fn process_diagnostics_severity_filtering() {
        let diags = vec![
            json!({
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}},
                "severity": 1,
                "message": "error msg"
            }),
            json!({
                "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 5}},
                "severity": 2,
                "message": "warning msg"
            }),
            json!({
                "range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 5}},
                "severity": 3,
                "message": "info msg"
            }),
        ];
        let result = process_diagnostics(&diags, true, Some("warning"));
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].severity, "warning");
        assert_eq!(result.items[0].message, "warning msg");
    }

    #[test]
    fn process_diagnostics_uses_full_range_if_present() {
        let diags = vec![json!({
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}},
            "fullRange": {"start": {"line": 10, "character": 3}, "end": {"line": 12, "character": 0}},
            "severity": 1,
            "message": "test"
        })];
        let result = process_diagnostics(&diags, true, None);
        assert_eq!(result.items[0].line, 11); // fullRange line 10 + 1
        assert_eq!(result.items[0].column, 4); // fullRange char 3 + 1
    }

    // ---- search_symbols ----

    #[test]
    fn search_symbols_finds_top_level() {
        let symbols = vec![
            json!({"name": "foo", "kind": 12}),
            json!({"name": "bar", "kind": 6}),
        ];
        let found = search_symbols(&symbols, "bar");
        assert!(found.is_some());
        assert_eq!(found.unwrap()["name"], "bar");
    }

    #[test]
    fn search_symbols_finds_nested() {
        let symbols = vec![json!({
            "name": "Namespace",
            "kind": 2,
            "children": [
                {"name": "innerThm", "kind": 12, "range": {"start": {"line": 5}, "end": {"line": 10}}}
            ]
        })];
        let found = search_symbols(&symbols, "innerThm");
        assert!(found.is_some());
        assert_eq!(found.unwrap()["name"], "innerThm");
    }

    #[test]
    fn search_symbols_returns_none_when_missing() {
        let symbols = vec![json!({"name": "foo", "kind": 12})];
        assert!(search_symbols(&symbols, "missing").is_none());
    }

    // ---- handle_diagnostics (async integration with mock) ----

    #[tokio::test]
    async fn diagnostics_returns_items_from_lsp() {
        let client = MockDiagClient::new().with_diagnostics(
            vec![json!({
                "range": {"start": {"line": 2, "character": 5}, "end": {"line": 2, "character": 10}},
                "severity": 1,
                "message": "unknown identifier 'foo'"
            })],
            false,
        );

        let result = handle_diagnostics(&client, "Main.lean", None, None, None, false, None)
            .await
            .unwrap();

        let dr: DiagnosticsResult = serde_json::from_value(result).unwrap();
        assert!(!dr.success);
        assert_eq!(dr.items.len(), 1);
        assert_eq!(dr.items[0].severity, "error");
        assert_eq!(dr.items[0].line, 3);
        assert_eq!(dr.items[0].column, 6);
    }

    #[tokio::test]
    async fn diagnostics_interactive_mode_returns_raw() {
        let interactive_diags =
            vec![json!({"severity": 1, "message": {"tag": "text", "text": "err"}})];
        let client = MockDiagClient::new().with_interactive(interactive_diags.clone());

        let result = handle_diagnostics(&client, "Main.lean", None, None, None, true, None)
            .await
            .unwrap();

        let ir: InteractiveDiagnosticsResult = serde_json::from_value(result).unwrap();
        assert_eq!(ir.diagnostics.len(), 1);
    }

    #[tokio::test]
    async fn diagnostics_with_declaration_name() {
        let symbols = vec![json!({
            "name": "myThm",
            "kind": 12,
            "range": {
                "start": {"line": 4, "character": 0},
                "end": {"line": 8, "character": 0}
            }
        })];
        let client = MockDiagClient::new()
            .with_symbols(symbols)
            .with_diagnostics(
                vec![json!({
                    "range": {
                        "start": {"line": 5, "character": 2},
                        "end": {"line": 5, "character": 10}
                    },
                    "severity": 1,
                    "message": "type mismatch"
                })],
                true,
            );

        let result =
            handle_diagnostics(&client, "Main.lean", None, None, Some("myThm"), false, None)
                .await
                .unwrap();

        let dr: DiagnosticsResult = serde_json::from_value(result).unwrap();
        assert!(dr.success);
        assert_eq!(dr.items.len(), 1);
    }

    #[tokio::test]
    async fn diagnostics_declaration_not_found_returns_error() {
        let client = MockDiagClient::new().with_symbols(vec![]);

        let err = handle_diagnostics(
            &client,
            "Main.lean",
            None,
            None,
            Some("nonexistent"),
            false,
            None,
        )
        .await
        .unwrap_err();

        match err {
            LeanToolError::DeclarationNotFound(name) => {
                assert_eq!(name, "nonexistent");
            }
            other => panic!("expected DeclarationNotFound, got: {other}"),
        }
    }

    #[tokio::test]
    async fn diagnostics_empty_file_returns_success() {
        let client = MockDiagClient::new();

        let result = handle_diagnostics(&client, "Empty.lean", None, None, None, false, None)
            .await
            .unwrap();

        let dr: DiagnosticsResult = serde_json::from_value(result).unwrap();
        assert!(dr.success);
        assert!(dr.items.is_empty());
        assert!(dr.failed_dependencies.is_empty());
    }
}
