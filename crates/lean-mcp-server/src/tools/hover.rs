//! Handler for the `lean_hover_info` tool.
//!
//! Retrieves type signature and documentation for a symbol at a given
//! source position, along with any diagnostics at that position.

use lean_lsp_client::client::LspClient;
use lean_lsp_client::types::severity;
use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::models::{DiagnosticMessage, HoverInfo};
use serde_json::Value;

/// Handle `lean_hover_info`: get type signature + docs at position.
///
/// Also includes diagnostics that overlap the hover position.
///
/// `line` and `column` are **1-indexed** (user-facing). They are converted
/// to 0-indexed before being forwarded to the LSP client.
pub async fn handle_lean_hover(
    client: &dyn LspClient,
    file_path: &str,
    line: u32,
    column: u32,
) -> Result<HoverInfo, LeanToolError> {
    // 1. Open the file and get its content.
    client
        .open_file(file_path)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "open_file".into(),
            message: e.to_string(),
        })?;

    let content =
        client
            .get_file_content(file_path)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_file_content".into(),
                message: e.to_string(),
            })?;

    // 2. Request hover at (line-1, column-1) — LSP uses 0-indexed positions.
    let hover_response = client
        .get_hover(file_path, line - 1, column - 1)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "get_hover".into(),
            message: e.to_string(),
        })?;

    let hover_value = hover_response.ok_or(LeanToolError::NoHoverInfo { line, column })?;

    // 3. Extract the symbol from the hover range.
    let symbol = extract_symbol_from_range(&content, hover_value.get("range"));

    // 4. Extract hover content and strip markdown fences.
    let info = extract_hover_content(&hover_value);

    // 5. Get diagnostics and filter to position.
    let diagnostics = match client.get_diagnostics(file_path, None, None, None).await {
        Ok(diag_value) => {
            let raw = extract_raw_diagnostics(&diag_value);
            let filtered = filter_diagnostics_by_position(&raw, line - 1, column - 1);
            to_diagnostic_messages(&filtered)
        }
        Err(_) => Vec::new(),
    };

    // 6. Return HoverInfo.
    Ok(HoverInfo {
        symbol,
        info,
        diagnostics,
    })
}

/// Extract the symbol text from a hover range within the file content.
///
/// The range is an LSP `Range` object with `start` and `end` positions
/// (0-indexed line/character). Returns an empty string if the range is
/// absent or out of bounds.
fn extract_symbol_from_range(content: &str, range: Option<&Value>) -> String {
    let Some(range) = range else {
        return String::new();
    };

    let start = range.get("start");
    let end = range.get("end");

    let (Some(start), Some(end)) = (start, end) else {
        return String::new();
    };

    let start_line = start.get("line").and_then(Value::as_u64).unwrap_or(0) as usize;
    let start_char = start.get("character").and_then(Value::as_u64).unwrap_or(0) as usize;
    let end_line = end.get("line").and_then(Value::as_u64).unwrap_or(0) as usize;
    let end_char = end.get("character").and_then(Value::as_u64).unwrap_or(0) as usize;

    let lines: Vec<&str> = content.split('\n').collect();
    if start_line >= lines.len() || end_line >= lines.len() {
        return String::new();
    }

    if start_line == end_line {
        let line = lines[start_line];
        let start_byte = char_offset_to_byte(line, start_char);
        let end_byte = char_offset_to_byte(line, end_char);
        if start_byte <= end_byte && end_byte <= line.len() {
            return line[start_byte..end_byte].to_string();
        }
        return String::new();
    }

    // Multi-line range
    let mut result = String::new();
    let first_line = lines[start_line];
    let start_byte = char_offset_to_byte(first_line, start_char);
    if start_byte <= first_line.len() {
        result.push_str(&first_line[start_byte..]);
    }
    for line in &lines[start_line + 1..end_line] {
        result.push('\n');
        result.push_str(line);
    }
    let last_line = lines[end_line];
    let end_byte = char_offset_to_byte(last_line, end_char);
    if end_byte <= last_line.len() {
        result.push('\n');
        result.push_str(&last_line[..end_byte]);
    }
    result
}

/// Convert a UTF-16 character offset to a byte offset within a line.
///
/// LSP uses UTF-16 offsets; for ASCII text this equals the character index.
/// We fall back to character index for simplicity (correct for the vast
/// majority of Lean source files).
fn char_offset_to_byte(line: &str, char_offset: usize) -> usize {
    line.char_indices()
        .nth(char_offset)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(line.len())
}

/// Extract the hover content string from a hover response value.
///
/// Strips Lean markdown fences (`\`\`\`lean\n` ... `\n\`\`\``) from the content.
fn extract_hover_content(hover: &Value) -> String {
    let contents = hover.get("contents");
    let raw = contents
        .and_then(|c| c.get("value"))
        .and_then(Value::as_str)
        .unwrap_or("No hover information available.");

    strip_markdown_fences(raw)
}

/// Strip Lean markdown code fences from hover content.
pub(crate) fn strip_markdown_fences(s: &str) -> String {
    s.replace("```lean\n", "")
        .replace("\n```", "")
        .replace("```lean", "")
        .replace("```", "")
        .trim()
        .to_string()
}

/// Extract the raw diagnostics array from a diagnostics response value.
///
/// The diagnostics response may be an array directly, or have a
/// `diagnostics` key containing the array.
fn extract_raw_diagnostics(value: &Value) -> Vec<Value> {
    if let Some(arr) = value.as_array() {
        return arr.clone();
    }
    if let Some(arr) = value.get("diagnostics").and_then(Value::as_array) {
        return arr.clone();
    }
    Vec::new()
}

/// Filter diagnostics that intersect a given 0-indexed position.
fn filter_diagnostics_by_position(diagnostics: &[Value], line: u32, column: u32) -> Vec<Value> {
    diagnostics
        .iter()
        .filter(|diag| diagnostic_intersects(diag, line, column))
        .cloned()
        .collect()
}

/// Check whether a diagnostic range intersects a 0-indexed (line, column).
fn diagnostic_intersects(diag: &Value, line: u32, column: u32) -> bool {
    let range = diag.get("fullRange").or_else(|| diag.get("range")).cloned();

    let Some(range) = range else {
        return false;
    };

    let start = &range["start"];
    let end = &range["end"];

    let start_line = start.get("line").and_then(Value::as_u64).map(|v| v as u32);
    let end_line = end.get("line").and_then(Value::as_u64).map(|v| v as u32);

    let (Some(sl), Some(el)) = (start_line, end_line) else {
        return false;
    };

    if line < sl || line > el {
        return false;
    }

    let start_char = start.get("character").and_then(Value::as_u64).unwrap_or(0) as u32;
    let end_char = end
        .get("character")
        .and_then(Value::as_u64)
        .unwrap_or(u64::from(column) + 1) as u32;

    // Zero-length range: match only exact position.
    if sl == el && start_char == end_char {
        return column == start_char;
    }

    if line == sl && column < start_char {
        return false;
    }
    if line == el && column >= end_char {
        return false;
    }

    true
}

/// Convert raw LSP diagnostic values into `DiagnosticMessage` models.
fn to_diagnostic_messages(diagnostics: &[Value]) -> Vec<DiagnosticMessage> {
    diagnostics
        .iter()
        .filter_map(|diag| {
            let range = diag
                .get("fullRange")
                .or_else(|| diag.get("range"))
                .cloned()?;

            let start = &range["start"];
            let severity_int = diag.get("severity").and_then(Value::as_i64).unwrap_or(1) as i32;

            Some(DiagnosticMessage {
                severity: severity::name(severity_int).to_string(),
                message: diag
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                line: start.get("line").and_then(Value::as_i64).unwrap_or(0) + 1,
                column: start.get("character").and_then(Value::as_i64).unwrap_or(0) + 1,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use lean_lsp_client::client::{LspClient, LspClientError};
    use serde_json::json;
    use std::path::{Path, PathBuf};

    // -- Minimal mock LSP client for testing --

    struct MockClient {
        project: PathBuf,
        content: String,
        hover: Option<Value>,
        diagnostics: Value,
    }

    impl MockClient {
        fn new() -> Self {
            Self {
                project: PathBuf::from("/test/project"),
                content: String::new(),
                hover: None,
                diagnostics: json!([]),
            }
        }
    }

    #[async_trait]
    impl LspClient for MockClient {
        fn project_path(&self) -> &Path {
            &self.project
        }
        async fn open_file(&self, _path: &str) -> Result<(), LspClientError> {
            Ok(())
        }
        async fn open_file_force(&self, _path: &str) -> Result<(), LspClientError> {
            Ok(())
        }
        async fn get_file_content(&self, _path: &str) -> Result<String, LspClientError> {
            Ok(self.content.clone())
        }
        async fn update_file(
            &self,
            _path: &str,
            _changes: Vec<Value>,
        ) -> Result<(), LspClientError> {
            Ok(())
        }
        async fn update_file_content(
            &self,
            _path: &str,
            _content: &str,
        ) -> Result<(), LspClientError> {
            Ok(())
        }
        async fn close_files(&self, _paths: &[String]) -> Result<(), LspClientError> {
            Ok(())
        }
        async fn get_diagnostics(
            &self,
            _path: &str,
            _start: Option<u32>,
            _end: Option<u32>,
            _timeout: Option<f64>,
        ) -> Result<Value, LspClientError> {
            Ok(self.diagnostics.clone())
        }
        async fn get_interactive_diagnostics(
            &self,
            _path: &str,
            _start: Option<u32>,
            _end: Option<u32>,
        ) -> Result<Vec<Value>, LspClientError> {
            Ok(vec![])
        }
        async fn get_goal(
            &self,
            _path: &str,
            _line: u32,
            _col: u32,
        ) -> Result<Option<Value>, LspClientError> {
            Ok(None)
        }
        async fn get_term_goal(
            &self,
            _path: &str,
            _line: u32,
            _col: u32,
        ) -> Result<Option<Value>, LspClientError> {
            Ok(None)
        }
        async fn get_hover(
            &self,
            _path: &str,
            _line: u32,
            _col: u32,
        ) -> Result<Option<Value>, LspClientError> {
            Ok(self.hover.clone())
        }
        async fn get_completions(
            &self,
            _path: &str,
            _line: u32,
            _col: u32,
        ) -> Result<Vec<Value>, LspClientError> {
            Ok(vec![])
        }
        async fn get_declarations(
            &self,
            _path: &str,
            _line: u32,
            _col: u32,
        ) -> Result<Vec<Value>, LspClientError> {
            Ok(vec![])
        }
        async fn get_references(
            &self,
            _path: &str,
            _line: u32,
            _col: u32,
            _include_decl: bool,
        ) -> Result<Vec<Value>, LspClientError> {
            Ok(vec![])
        }
        async fn get_document_symbols(&self, _path: &str) -> Result<Vec<Value>, LspClientError> {
            Ok(vec![])
        }
        async fn get_code_actions(
            &self,
            _path: &str,
            _sl: u32,
            _sc: u32,
            _el: u32,
            _ec: u32,
        ) -> Result<Vec<Value>, LspClientError> {
            Ok(vec![])
        }
        async fn get_code_action_resolve(&self, _action: Value) -> Result<Value, LspClientError> {
            Ok(json!(null))
        }
        async fn get_widgets(
            &self,
            _path: &str,
            _line: u32,
            _col: u32,
        ) -> Result<Vec<Value>, LspClientError> {
            Ok(vec![])
        }
        async fn get_widget_source(
            &self,
            _path: &str,
            _line: u32,
            _col: u32,
            _hash: &str,
        ) -> Result<Value, LspClientError> {
            Ok(json!(null))
        }
        async fn shutdown(&self) -> Result<(), LspClientError> {
            Ok(())
        }
    }

    // ---- strip_markdown_fences ----

    #[test]
    fn strips_lean_fences() {
        assert_eq!(
            strip_markdown_fences("```lean\nNat -> Nat\n```"),
            "Nat -> Nat"
        );
    }

    #[test]
    fn strips_bare_fences() {
        assert_eq!(strip_markdown_fences("```\nsome text\n```"), "some text");
    }

    #[test]
    fn no_fences_passthrough() {
        assert_eq!(strip_markdown_fences("Nat -> Nat"), "Nat -> Nat");
    }

    #[test]
    fn strips_multiple_fences() {
        let input = "```lean\nfirst\n```\n\n```lean\nsecond\n```";
        let result = strip_markdown_fences(input);
        assert_eq!(result, "first\n\nsecond");
    }

    // ---- extract_symbol_from_range ----

    #[test]
    fn extracts_symbol_single_line() {
        let content = "def foo := 42";
        let range = json!({
            "start": {"line": 0, "character": 4},
            "end": {"line": 0, "character": 7}
        });
        assert_eq!(extract_symbol_from_range(content, Some(&range)), "foo");
    }

    #[test]
    fn extracts_symbol_no_range() {
        assert_eq!(extract_symbol_from_range("def foo", None), "");
    }

    #[test]
    fn extracts_symbol_out_of_bounds() {
        let content = "short";
        let range = json!({
            "start": {"line": 5, "character": 0},
            "end": {"line": 5, "character": 3}
        });
        assert_eq!(extract_symbol_from_range(content, Some(&range)), "");
    }

    // ---- extract_hover_content ----

    #[test]
    fn extracts_hover_content_with_fences() {
        let hover = json!({
            "contents": {
                "value": "```lean\nNat.add : Nat -> Nat -> Nat\n```"
            }
        });
        assert_eq!(extract_hover_content(&hover), "Nat.add : Nat -> Nat -> Nat");
    }

    #[test]
    fn extracts_hover_content_missing() {
        let hover = json!({});
        assert_eq!(
            extract_hover_content(&hover),
            "No hover information available."
        );
    }

    // ---- filter_diagnostics_by_position ----

    #[test]
    fn filters_diagnostics_at_position() {
        let diags = vec![
            json!({
                "range": {
                    "start": {"line": 5, "character": 0},
                    "end": {"line": 5, "character": 10}
                },
                "severity": 1,
                "message": "error here"
            }),
            json!({
                "range": {
                    "start": {"line": 10, "character": 0},
                    "end": {"line": 10, "character": 5}
                },
                "severity": 2,
                "message": "warning elsewhere"
            }),
        ];
        let filtered = filter_diagnostics_by_position(&diags, 5, 3);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["message"], "error here");
    }

    #[test]
    fn filters_diagnostics_none_match() {
        let diags = vec![json!({
            "range": {
                "start": {"line": 10, "character": 0},
                "end": {"line": 10, "character": 5}
            },
            "severity": 1,
            "message": "far away"
        })];
        let filtered = filter_diagnostics_by_position(&diags, 0, 0);
        assert!(filtered.is_empty());
    }

    // ---- to_diagnostic_messages ----

    #[test]
    fn converts_raw_diagnostics_to_models() {
        let diags = vec![json!({
            "range": {
                "start": {"line": 4, "character": 2},
                "end": {"line": 4, "character": 10}
            },
            "severity": 1,
            "message": "unknown identifier"
        })];
        let msgs = to_diagnostic_messages(&diags);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].severity, "error");
        assert_eq!(msgs[0].message, "unknown identifier");
        assert_eq!(msgs[0].line, 5); // 0-indexed -> 1-indexed
        assert_eq!(msgs[0].column, 3);
    }

    #[test]
    fn converts_diagnostics_with_full_range() {
        let diags = vec![json!({
            "fullRange": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 5}
            },
            "severity": 2,
            "message": "warning"
        })];
        let msgs = to_diagnostic_messages(&diags);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].severity, "warning");
        assert_eq!(msgs[0].line, 1);
        assert_eq!(msgs[0].column, 1);
    }

    // ---- handle_lean_hover integration ----

    #[tokio::test]
    async fn hover_returns_symbol_and_info() {
        let client = MockClient {
            content: "def Nat.add := sorry".into(),
            hover: Some(json!({
                "contents": {
                    "kind": "markdown",
                    "value": "```lean\nNat -> Nat -> Nat\n```"
                },
                "range": {
                    "start": {"line": 0, "character": 4},
                    "end": {"line": 0, "character": 11}
                }
            })),
            ..MockClient::new()
        };

        let result = handle_lean_hover(&client, "Main.lean", 1, 5).await.unwrap();
        assert_eq!(result.symbol, "Nat.add");
        assert_eq!(result.info, "Nat -> Nat -> Nat");
        assert!(result.diagnostics.is_empty());
    }

    #[tokio::test]
    async fn hover_strips_markdown_fences() {
        let client = MockClient {
            content: "def foo := 42".into(),
            hover: Some(json!({
                "contents": {
                    "value": "```lean\nNat\n```"
                },
                "range": {
                    "start": {"line": 0, "character": 4},
                    "end": {"line": 0, "character": 7}
                }
            })),
            ..MockClient::new()
        };

        let result = handle_lean_hover(&client, "Main.lean", 1, 5).await.unwrap();
        assert_eq!(result.info, "Nat");
        assert!(!result.info.contains("```"));
    }

    #[tokio::test]
    async fn hover_includes_diagnostics_at_position() {
        let client = MockClient {
            content: "def bad := sorry".into(),
            hover: Some(json!({
                "contents": {"value": "sorry"},
                "range": {
                    "start": {"line": 0, "character": 11},
                    "end": {"line": 0, "character": 16}
                }
            })),
            diagnostics: json!([{
                "range": {
                    "start": {"line": 0, "character": 11},
                    "end": {"line": 0, "character": 16}
                },
                "severity": 2,
                "message": "declaration uses sorry"
            }]),
            ..MockClient::new()
        };

        let result = handle_lean_hover(&client, "Main.lean", 1, 12)
            .await
            .unwrap();
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].severity, "warning");
        assert_eq!(result.diagnostics[0].message, "declaration uses sorry");
    }

    #[tokio::test]
    async fn hover_with_no_info_returns_error() {
        let client = MockClient {
            content: "-- comment".into(),
            hover: None,
            ..MockClient::new()
        };

        let result = handle_lean_hover(&client, "Main.lean", 1, 1).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, LeanToolError::NoHoverInfo { line: 1, column: 1 }),
            "expected NoHoverInfo, got: {err:?}"
        );
    }

    // ---- char_offset_to_byte ----

    #[test]
    fn char_offset_ascii() {
        assert_eq!(char_offset_to_byte("hello", 0), 0);
        assert_eq!(char_offset_to_byte("hello", 3), 3);
        assert_eq!(char_offset_to_byte("hello", 5), 5);
    }

    #[test]
    fn char_offset_beyond_len() {
        assert_eq!(char_offset_to_byte("hi", 10), 2);
    }

    // ---- diagnostic_intersects edge cases ----

    #[test]
    fn diagnostic_no_range_does_not_intersect() {
        let diag = json!({"severity": 1, "message": "oops"});
        assert!(!diagnostic_intersects(&diag, 0, 0));
    }

    #[test]
    fn diagnostic_zero_length_range_exact_match() {
        let diag = json!({
            "range": {
                "start": {"line": 3, "character": 5},
                "end": {"line": 3, "character": 5}
            }
        });
        assert!(diagnostic_intersects(&diag, 3, 5));
        assert!(!diagnostic_intersects(&diag, 3, 4));
        assert!(!diagnostic_intersects(&diag, 3, 6));
    }
}
