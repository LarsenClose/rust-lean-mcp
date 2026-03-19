//! Tool handler for `lean_code_actions`.
//!
//! Retrieves available code actions (quick fixes, refactorings) for a given
//! line in a Lean file. Diagnostics on the target line are used to determine
//! the ranges to query. Actions are deduplicated by title and resolved to
//! extract concrete text edits.

use std::collections::HashSet;

use lean_lsp_client::client::LspClient;
use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::models::{CodeAction, CodeActionEdit, CodeActionsResult};
use serde_json::Value;

/// Handle a `lean_code_actions` tool call.
///
/// Retrieves code actions for the given line by:
/// 1. Opening the file and fetching diagnostics for the target line.
/// 2. Querying code actions for each diagnostic range on that line.
/// 3. Deduplicating actions by title.
/// 4. Resolving unresolved actions via `get_code_action_resolve`.
/// 5. Extracting text edits, converting LSP 0-indexed positions to 1-indexed.
///
/// `line` is **1-indexed** (matching the MCP tool interface).
/// It is converted to 0-indexed for LSP calls internally.
pub async fn handle_code_actions(
    client: &dyn LspClient,
    file_path: &str,
    line: u32,
) -> Result<CodeActionsResult, LeanToolError> {
    // 1. Open the file in the LSP server.
    client
        .open_file(file_path)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "open_file".into(),
            message: e.to_string(),
        })?;

    // 2. Convert 1-indexed line to 0-indexed for LSP.
    let lsp_line = line.saturating_sub(1);

    // 3. Get diagnostics for the target line.
    let diags_response = client
        .get_diagnostics(file_path, Some(lsp_line), Some(lsp_line), Some(15.0))
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "get_diagnostics".into(),
            message: e.to_string(),
        })?;

    let diagnostics = diags_response
        .get("diagnostics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // 4. Collect diagnostic ranges on the target line.
    let ranges = extract_diagnostic_ranges(&diagnostics, lsp_line);

    // 5. If no diagnostics on this line, return empty actions.
    if ranges.is_empty() {
        return Ok(CodeActionsResult { actions: vec![] });
    }

    // 6. Query code actions for each diagnostic range and deduplicate by title.
    let mut seen_titles = HashSet::new();
    let mut raw_actions: Vec<Value> = Vec::new();

    for (start_line, start_col, end_line, end_col) in &ranges {
        let actions = client
            .get_code_actions(file_path, *start_line, *start_col, *end_line, *end_col)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_code_actions".into(),
                message: e.to_string(),
            })?;

        for action in actions {
            let title = action
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !title.is_empty() && seen_titles.insert(title) {
                raw_actions.push(action);
            }
        }
    }

    // 7. Resolve unresolved actions and extract edits.
    let mut result_actions: Vec<CodeAction> = Vec::new();

    for action in raw_actions {
        let resolved = resolve_action(client, action).await?;
        let title = resolved
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let is_preferred = resolved
            .get("isPreferred")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let edits = extract_edits(&resolved);

        result_actions.push(CodeAction {
            title,
            is_preferred,
            edits,
        });
    }

    Ok(CodeActionsResult {
        actions: result_actions,
    })
}

/// Extract diagnostic ranges that fall on the target line (0-indexed).
///
/// Returns a vec of `(start_line, start_col, end_line, end_col)` tuples,
/// all 0-indexed.
fn extract_diagnostic_ranges(diagnostics: &[Value], target_line: u32) -> Vec<(u32, u32, u32, u32)> {
    let mut ranges = Vec::new();

    for diag in diagnostics {
        let range = diag.get("fullRange").or_else(|| diag.get("range"));
        let Some(r) = range else { continue };

        let start_line = r
            .pointer("/start/line")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let start_col = r
            .pointer("/start/character")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let end_line = r.pointer("/end/line").and_then(Value::as_u64).unwrap_or(0) as u32;
        let end_col = r
            .pointer("/end/character")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;

        // Include diagnostics whose range overlaps the target line.
        if start_line <= target_line && end_line >= target_line {
            ranges.push((start_line, start_col, end_line, end_col));
        }
    }

    ranges
}

/// Resolve a code action if it lacks edits (i.e., is unresolved).
///
/// An action is considered unresolved if it has no `edit` field.
/// In that case, `get_code_action_resolve` is called to obtain the full action.
async fn resolve_action(client: &dyn LspClient, action: Value) -> Result<Value, LeanToolError> {
    if action.get("edit").is_some() {
        return Ok(action);
    }

    client
        .get_code_action_resolve(action)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "get_code_action_resolve".into(),
            message: e.to_string(),
        })
}

/// Extract text edits from a resolved code action.
///
/// Looks for edits in `edit.documentChanges[].edits[]` and
/// `edit.changes.*[]`, converting LSP 0-indexed positions to 1-indexed.
fn extract_edits(action: &Value) -> Vec<CodeActionEdit> {
    let mut edits = Vec::new();

    // Try documentChanges first.
    if let Some(doc_changes) = action
        .pointer("/edit/documentChanges")
        .and_then(Value::as_array)
    {
        for doc_change in doc_changes {
            if let Some(text_edits) = doc_change.get("edits").and_then(Value::as_array) {
                for edit in text_edits {
                    if let Some(e) = convert_text_edit(edit) {
                        edits.push(e);
                    }
                }
            }
        }
    }

    // Also try changes (flat map of uri -> edits[]).
    if let Some(changes) = action.pointer("/edit/changes").and_then(Value::as_object) {
        for (_uri, uri_edits) in changes {
            if let Some(edit_array) = uri_edits.as_array() {
                for edit in edit_array {
                    if let Some(e) = convert_text_edit(edit) {
                        edits.push(e);
                    }
                }
            }
        }
    }

    edits
}

/// Convert a single LSP TextEdit to a `CodeActionEdit` with 1-indexed positions.
fn convert_text_edit(edit: &Value) -> Option<CodeActionEdit> {
    let range = edit.get("range")?;
    let new_text = edit.get("newText").and_then(Value::as_str)?;

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

    Some(CodeActionEdit {
        new_text: new_text.to_string(),
        start_line,
        start_column: start_col,
        end_line,
        end_column: end_col,
    })
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

    /// Mock LSP client for code actions handler tests.
    struct MockCodeActionClient {
        project: PathBuf,
        /// Canned response for `get_diagnostics`.
        diagnostics_response: Value,
        /// Canned response for `get_code_actions`.
        code_actions_response: Vec<Value>,
        /// Canned response for `get_code_action_resolve`.
        resolve_response: Value,
    }

    impl MockCodeActionClient {
        fn new() -> Self {
            Self {
                project: PathBuf::from("/test/project"),
                diagnostics_response: json!({
                    "diagnostics": [],
                    "success": true
                }),
                code_actions_response: Vec::new(),
                resolve_response: json!({}),
            }
        }

        fn with_diagnostics(mut self, diags: Vec<Value>) -> Self {
            self.diagnostics_response = json!({
                "diagnostics": diags,
                "success": true
            });
            self
        }

        fn with_code_actions(mut self, actions: Vec<Value>) -> Self {
            self.code_actions_response = actions;
            self
        }

        fn with_resolve(mut self, resolved: Value) -> Self {
            self.resolve_response = resolved;
            self
        }
    }

    #[async_trait]
    impl LspClient for MockCodeActionClient {
        fn project_path(&self) -> &Path {
            &self.project
        }
        async fn open_file(&self, _p: &str) -> Result<(), LspClientError> {
            Ok(())
        }
        async fn open_file_force(&self, _p: &str) -> Result<(), LspClientError> {
            Ok(())
        }
        async fn get_file_content(&self, _p: &str) -> Result<String, LspClientError> {
            Ok(String::new())
        }
        async fn update_file(&self, _p: &str, _c: Vec<Value>) -> Result<(), LspClientError> {
            Ok(())
        }
        async fn update_file_content(&self, _p: &str, _c: &str) -> Result<(), LspClientError> {
            Ok(())
        }
        async fn close_files(&self, _p: &[String]) -> Result<(), LspClientError> {
            Ok(())
        }
        async fn get_diagnostics(
            &self,
            _p: &str,
            _sl: Option<u32>,
            _el: Option<u32>,
            _t: Option<f64>,
        ) -> Result<Value, LspClientError> {
            Ok(self.diagnostics_response.clone())
        }
        async fn get_interactive_diagnostics(
            &self,
            _p: &str,
            _sl: Option<u32>,
            _el: Option<u32>,
        ) -> Result<Vec<Value>, LspClientError> {
            Ok(vec![])
        }
        async fn get_goal(
            &self,
            _p: &str,
            _l: u32,
            _c: u32,
        ) -> Result<Option<Value>, LspClientError> {
            Ok(None)
        }
        async fn get_term_goal(
            &self,
            _p: &str,
            _l: u32,
            _c: u32,
        ) -> Result<Option<Value>, LspClientError> {
            Ok(None)
        }
        async fn get_hover(
            &self,
            _p: &str,
            _l: u32,
            _c: u32,
        ) -> Result<Option<Value>, LspClientError> {
            Ok(None)
        }
        async fn get_completions(
            &self,
            _p: &str,
            _l: u32,
            _c: u32,
        ) -> Result<Vec<Value>, LspClientError> {
            Ok(vec![])
        }
        async fn get_declarations(
            &self,
            _p: &str,
            _l: u32,
            _c: u32,
        ) -> Result<Vec<Value>, LspClientError> {
            Ok(vec![])
        }
        async fn get_references(
            &self,
            _p: &str,
            _l: u32,
            _c: u32,
            _d: bool,
        ) -> Result<Vec<Value>, LspClientError> {
            Ok(vec![])
        }
        async fn get_document_symbols(&self, _p: &str) -> Result<Vec<Value>, LspClientError> {
            Ok(vec![])
        }
        async fn get_code_actions(
            &self,
            _p: &str,
            _sl: u32,
            _sc: u32,
            _el: u32,
            _ec: u32,
        ) -> Result<Vec<Value>, LspClientError> {
            Ok(self.code_actions_response.clone())
        }
        async fn get_code_action_resolve(&self, _a: Value) -> Result<Value, LspClientError> {
            Ok(self.resolve_response.clone())
        }
        async fn get_widgets(
            &self,
            _p: &str,
            _l: u32,
            _c: u32,
        ) -> Result<Vec<Value>, LspClientError> {
            Ok(vec![])
        }
        async fn get_widget_source(
            &self,
            _p: &str,
            _l: u32,
            _c: u32,
            _h: &str,
        ) -> Result<Value, LspClientError> {
            Ok(json!({}))
        }
        async fn shutdown(&self) -> Result<(), LspClientError> {
            Ok(())
        }
    }

    // ---- returns actions with edits ----

    #[tokio::test]
    async fn code_actions_returns_actions_with_edits() {
        let client = MockCodeActionClient::new()
            .with_diagnostics(vec![json!({
                "range": {
                    "start": {"line": 4, "character": 2},
                    "end": {"line": 4, "character": 10}
                },
                "severity": 1,
                "message": "unknown tactic"
            })])
            .with_code_actions(vec![json!({
                "title": "Try this: simp only [Nat.add_comm]",
                "isPreferred": true,
                "edit": {
                    "documentChanges": [{
                        "edits": [{
                            "range": {
                                "start": {"line": 4, "character": 2},
                                "end": {"line": 4, "character": 10}
                            },
                            "newText": "simp only [Nat.add_comm]"
                        }]
                    }]
                }
            })]);

        let result = handle_code_actions(&client, "Main.lean", 5).await.unwrap();

        assert_eq!(result.actions.len(), 1);
        assert_eq!(
            result.actions[0].title,
            "Try this: simp only [Nat.add_comm]"
        );
        assert!(result.actions[0].is_preferred);
        assert_eq!(result.actions[0].edits.len(), 1);
        assert_eq!(
            result.actions[0].edits[0].new_text,
            "simp only [Nat.add_comm]"
        );
        // Positions should be 1-indexed: line 4 -> 5, char 2 -> 3
        assert_eq!(result.actions[0].edits[0].start_line, 5);
        assert_eq!(result.actions[0].edits[0].start_column, 3);
        assert_eq!(result.actions[0].edits[0].end_line, 5);
        assert_eq!(result.actions[0].edits[0].end_column, 11);
    }

    // ---- deduplicates by title ----

    #[tokio::test]
    async fn code_actions_deduplicates_by_title() {
        let client = MockCodeActionClient::new()
            .with_diagnostics(vec![
                json!({
                    "range": {
                        "start": {"line": 2, "character": 0},
                        "end": {"line": 2, "character": 5}
                    },
                    "severity": 1,
                    "message": "error 1"
                }),
                json!({
                    "range": {
                        "start": {"line": 2, "character": 3},
                        "end": {"line": 2, "character": 8}
                    },
                    "severity": 1,
                    "message": "error 2"
                }),
            ])
            .with_code_actions(vec![
                json!({
                    "title": "Try this: exact rfl",
                    "edit": {
                        "documentChanges": [{
                            "edits": [{
                                "range": {
                                    "start": {"line": 2, "character": 0},
                                    "end": {"line": 2, "character": 5}
                                },
                                "newText": "exact rfl"
                            }]
                        }]
                    }
                }),
                json!({
                    "title": "Try this: exact rfl",
                    "edit": {
                        "documentChanges": [{
                            "edits": [{
                                "range": {
                                    "start": {"line": 2, "character": 0},
                                    "end": {"line": 2, "character": 5}
                                },
                                "newText": "exact rfl"
                            }]
                        }]
                    }
                }),
            ]);

        let result = handle_code_actions(&client, "Main.lean", 3).await.unwrap();

        // Despite two diagnostics producing the same action title, only one should appear.
        assert_eq!(result.actions.len(), 1);
        assert_eq!(result.actions[0].title, "Try this: exact rfl");
    }

    // ---- resolves unresolved actions ----

    #[tokio::test]
    async fn code_actions_resolves_unresolved_actions() {
        let client = MockCodeActionClient::new()
            .with_diagnostics(vec![json!({
                "range": {
                    "start": {"line": 5, "character": 0},
                    "end": {"line": 5, "character": 10}
                },
                "severity": 2,
                "message": "unused variable"
            })])
            .with_code_actions(vec![json!({
                "title": "Remove unused variable",
                "kind": "quickfix"
                // No "edit" field - this action needs to be resolved.
            })])
            .with_resolve(json!({
                "title": "Remove unused variable",
                "isPreferred": false,
                "edit": {
                    "changes": {
                        "file:///test/Main.lean": [{
                            "range": {
                                "start": {"line": 5, "character": 0},
                                "end": {"line": 5, "character": 10}
                            },
                            "newText": ""
                        }]
                    }
                }
            }));

        let result = handle_code_actions(&client, "Main.lean", 6).await.unwrap();

        assert_eq!(result.actions.len(), 1);
        assert_eq!(result.actions[0].title, "Remove unused variable");
        assert!(!result.actions[0].is_preferred);
        assert_eq!(result.actions[0].edits.len(), 1);
        assert_eq!(result.actions[0].edits[0].new_text, "");
    }

    // ---- empty line returns empty actions ----

    #[tokio::test]
    async fn code_actions_empty_line_returns_empty_actions() {
        let client = MockCodeActionClient::new();

        let result = handle_code_actions(&client, "Main.lean", 1).await.unwrap();

        assert!(result.actions.is_empty());
    }

    // ---- positions are 1-indexed in output ----

    #[tokio::test]
    async fn code_actions_positions_are_1_indexed_in_output() {
        let client = MockCodeActionClient::new()
            .with_diagnostics(vec![json!({
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 5}
                },
                "severity": 1,
                "message": "error"
            })])
            .with_code_actions(vec![json!({
                "title": "Fix it",
                "edit": {
                    "documentChanges": [{
                        "edits": [{
                            "range": {
                                "start": {"line": 0, "character": 0},
                                "end": {"line": 0, "character": 5}
                            },
                            "newText": "fixed"
                        }]
                    }]
                }
            })]);

        let result = handle_code_actions(&client, "Main.lean", 1).await.unwrap();

        assert_eq!(result.actions.len(), 1);
        let edit = &result.actions[0].edits[0];
        // LSP (0,0)-(0,5) should become 1-indexed (1,1)-(1,6)
        assert_eq!(edit.start_line, 1);
        assert_eq!(edit.start_column, 1);
        assert_eq!(edit.end_line, 1);
        assert_eq!(edit.end_column, 6);
    }

    // ---- multiple actions with different titles are all returned ----

    #[tokio::test]
    async fn code_actions_returns_multiple_distinct_actions() {
        let client = MockCodeActionClient::new()
            .with_diagnostics(vec![json!({
                "range": {
                    "start": {"line": 3, "character": 0},
                    "end": {"line": 3, "character": 15}
                },
                "severity": 1,
                "message": "type mismatch"
            })])
            .with_code_actions(vec![
                json!({
                    "title": "Try this: simp",
                    "edit": {
                        "documentChanges": [{
                            "edits": [{
                                "range": {
                                    "start": {"line": 3, "character": 0},
                                    "end": {"line": 3, "character": 15}
                                },
                                "newText": "simp"
                            }]
                        }]
                    }
                }),
                json!({
                    "title": "Try this: ring",
                    "edit": {
                        "documentChanges": [{
                            "edits": [{
                                "range": {
                                    "start": {"line": 3, "character": 0},
                                    "end": {"line": 3, "character": 15}
                                },
                                "newText": "ring"
                            }]
                        }]
                    }
                }),
            ]);

        let result = handle_code_actions(&client, "Main.lean", 4).await.unwrap();

        assert_eq!(result.actions.len(), 2);
        assert_eq!(result.actions[0].title, "Try this: simp");
        assert_eq!(result.actions[1].title, "Try this: ring");
    }

    // ---- extract_diagnostic_ranges unit tests ----

    #[test]
    fn extract_ranges_filters_by_target_line() {
        let diags = vec![
            json!({
                "range": {
                    "start": {"line": 3, "character": 0},
                    "end": {"line": 3, "character": 10}
                }
            }),
            json!({
                "range": {
                    "start": {"line": 5, "character": 2},
                    "end": {"line": 5, "character": 8}
                }
            }),
        ];
        let ranges = extract_diagnostic_ranges(&diags, 3);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], (3, 0, 3, 10));
    }

    #[test]
    fn extract_ranges_includes_multiline_diagnostic_spanning_target() {
        let diags = vec![json!({
            "range": {
                "start": {"line": 2, "character": 0},
                "end": {"line": 6, "character": 5}
            }
        })];
        let ranges = extract_diagnostic_ranges(&diags, 4);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], (2, 0, 6, 5));
    }

    #[test]
    fn extract_ranges_empty_when_no_match() {
        let diags = vec![json!({
            "range": {
                "start": {"line": 10, "character": 0},
                "end": {"line": 10, "character": 5}
            }
        })];
        let ranges = extract_diagnostic_ranges(&diags, 0);
        assert!(ranges.is_empty());
    }

    // ---- convert_text_edit unit tests ----

    #[test]
    fn convert_text_edit_converts_0_to_1_indexed() {
        let edit = json!({
            "range": {
                "start": {"line": 9, "character": 4},
                "end": {"line": 9, "character": 12}
            },
            "newText": "replacement"
        });
        let result = convert_text_edit(&edit).unwrap();
        assert_eq!(result.new_text, "replacement");
        assert_eq!(result.start_line, 10);
        assert_eq!(result.start_column, 5);
        assert_eq!(result.end_line, 10);
        assert_eq!(result.end_column, 13);
    }

    #[test]
    fn convert_text_edit_returns_none_without_range() {
        let edit = json!({"newText": "foo"});
        assert!(convert_text_edit(&edit).is_none());
    }

    #[test]
    fn convert_text_edit_returns_none_without_new_text() {
        let edit = json!({
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 3}
            }
        });
        assert!(convert_text_edit(&edit).is_none());
    }

    // ---- extract_edits unit tests ----

    #[test]
    fn extract_edits_from_document_changes() {
        let action = json!({
            "title": "Fix",
            "edit": {
                "documentChanges": [{
                    "edits": [
                        {
                            "range": {
                                "start": {"line": 1, "character": 0},
                                "end": {"line": 1, "character": 5}
                            },
                            "newText": "hello"
                        },
                        {
                            "range": {
                                "start": {"line": 3, "character": 2},
                                "end": {"line": 3, "character": 7}
                            },
                            "newText": "world"
                        }
                    ]
                }]
            }
        });
        let edits = extract_edits(&action);
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].new_text, "hello");
        assert_eq!(edits[0].start_line, 2); // 1-indexed
        assert_eq!(edits[1].new_text, "world");
        assert_eq!(edits[1].start_line, 4); // 1-indexed
    }

    #[test]
    fn extract_edits_from_changes_map() {
        let action = json!({
            "title": "Fix",
            "edit": {
                "changes": {
                    "file:///test/Main.lean": [{
                        "range": {
                            "start": {"line": 0, "character": 0},
                            "end": {"line": 0, "character": 3}
                        },
                        "newText": "new"
                    }]
                }
            }
        });
        let edits = extract_edits(&action);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "new");
        assert_eq!(edits[0].start_line, 1);
        assert_eq!(edits[0].start_column, 1);
    }

    #[test]
    fn extract_edits_empty_when_no_edit_field() {
        let action = json!({"title": "No edits"});
        let edits = extract_edits(&action);
        assert!(edits.is_empty());
    }

    // ---- extract_ranges with fullRange ----

    #[test]
    fn extract_ranges_prefers_full_range() {
        let diags = vec![json!({
            "range": {
                "start": {"line": 10, "character": 0},
                "end": {"line": 10, "character": 5}
            },
            "fullRange": {
                "start": {"line": 3, "character": 0},
                "end": {"line": 7, "character": 10}
            }
        })];
        // Target line 5 is within fullRange (3-7) but not within range (10-10).
        let ranges = extract_diagnostic_ranges(&diags, 5);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], (3, 0, 7, 10));
    }
}
