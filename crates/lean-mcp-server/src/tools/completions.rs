//! Handler for the `lean_completions` tool.
//!
//! Retrieves IDE autocomplete suggestions at a given source position,
//! sorts them by prefix relevance, and truncates to a configurable limit.

use lean_lsp_client::client::LspClient;
use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::models::{CompletionItem, CompletionsResult};
use lean_mcp_core::utils::completion_kind_name;
use serde_json::Value;

/// Handle `lean_completions`: get IDE autocomplete suggestions.
///
/// Sorts by prefix relevance and truncates to `max_completions`.
///
/// `line` and `column` are **1-indexed** (user-facing). They are converted
/// to 0-indexed before being forwarded to the LSP client.
pub async fn handle_lean_completions(
    client: &dyn LspClient,
    file_path: &str,
    line: u32,
    column: u32,
    max_completions: usize,
) -> Result<CompletionsResult, LeanToolError> {
    // 1. Open file, get content + completions from LSP.
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

    let raw_completions = client
        .get_completions(file_path, line - 1, column - 1)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "get_completions".into(),
            message: e.to_string(),
        })?;

    // 2. Map kind integers to strings via completion_kind_name().
    let mut items: Vec<CompletionItem> = raw_completions
        .iter()
        .filter_map(|c| {
            let label = c.get("label")?.as_str()?.to_string();
            let kind = c
                .get("kind")
                .and_then(Value::as_i64)
                .and_then(|k| completion_kind_name(k as i32))
                .map(String::from);
            let detail = c.get("detail").and_then(Value::as_str).map(String::from);
            Some(CompletionItem {
                label,
                kind,
                detail,
            })
        })
        .collect();

    if items.is_empty() {
        return Ok(CompletionsResult { items: vec![] });
    }

    // 3. Extract prefix from text before cursor.
    let prefix = extract_prefix(&content, line, column);

    // 4. Sort: prefix matches first, then contains, then alphabetical.
    sort_completions(&mut items, &prefix);

    // 5. Truncate to max_completions.
    items.truncate(max_completions);

    Ok(CompletionsResult { items })
}

/// Extract the completion prefix from the text before the cursor.
///
/// The prefix is the last identifier-like token before the cursor position.
/// If the text before the cursor ends with `.`, the prefix is empty
/// (dot-triggered completion).
fn extract_prefix(content: &str, line: u32, column: u32) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let line_idx = (line as usize).wrapping_sub(1);
    if line_idx >= lines.len() {
        return String::new();
    }

    let line_text = lines[line_idx];
    let col_idx = (column as usize).wrapping_sub(1);
    let text_before = if col_idx <= line_text.len() {
        &line_text[..col_idx]
    } else {
        line_text
    };

    // Dot-triggered completion: no prefix filtering needed.
    if text_before.ends_with('.') {
        return String::new();
    }

    // Split on delimiter characters and take the last token.
    let last_token = text_before
        .split(|c: char| c.is_whitespace() || "()[]{},:;.".contains(c))
        .next_back()
        .unwrap_or("");

    last_token.to_lowercase()
}

/// Sort completions by prefix relevance.
///
/// Priority:
/// 0. Label starts with prefix
/// 1. Label contains prefix
/// 2. Everything else
///
/// Within each group, sort alphabetically (case-insensitive).
fn sort_completions(items: &mut [CompletionItem], prefix: &str) {
    if prefix.is_empty() {
        // No prefix: sort alphabetically.
        items.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
    } else {
        items.sort_by(|a, b| {
            let a_lower = a.label.to_lowercase();
            let b_lower = b.label.to_lowercase();
            let a_key = sort_key(&a_lower, prefix);
            let b_key = sort_key(&b_lower, prefix);
            a_key.cmp(&b_key)
        });
    }
}

/// Compute a sort key for a completion label given a prefix.
fn sort_key<'a>(label_lower: &'a str, prefix: &str) -> (u8, &'a str) {
    if label_lower.starts_with(prefix) {
        (0, label_lower)
    } else if label_lower.contains(prefix) {
        (1, label_lower)
    } else {
        (2, label_lower)
    }
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
        completions: Vec<Value>,
    }

    impl MockClient {
        fn new() -> Self {
            Self {
                project: PathBuf::from("/test/project"),
                content: String::new(),
                completions: vec![],
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
            Ok(json!([]))
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
            Ok(None)
        }
        async fn get_completions(
            &self,
            _path: &str,
            _line: u32,
            _col: u32,
        ) -> Result<Vec<Value>, LspClientError> {
            Ok(self.completions.clone())
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

    // ---- extract_prefix ----

    #[test]
    fn prefix_from_partial_identifier() {
        let content = "def Nat.ad";
        // line=1, column=11 -> text_before = "def Nat.ad"
        assert_eq!(extract_prefix(content, 1, 11), "ad");
    }

    #[test]
    fn prefix_after_dot() {
        let content = "def Nat.";
        // line=1, column=9 -> text_before = "def Nat."
        assert_eq!(extract_prefix(content, 1, 9), "");
    }

    #[test]
    fn prefix_from_empty_content() {
        assert_eq!(extract_prefix("", 1, 1), "");
    }

    #[test]
    fn prefix_line_out_of_range() {
        assert_eq!(extract_prefix("hello", 5, 1), "");
    }

    #[test]
    fn prefix_after_space() {
        let content = "theorem foo : Nat";
        // column=18 -> text_before = "theorem foo : Nat"
        assert_eq!(extract_prefix(content, 1, 18), "nat");
    }

    // ---- sort_completions ----

    #[test]
    fn sort_prefix_then_contains_then_other() {
        let mut items = vec![
            CompletionItem {
                label: "zz_other".into(),
                kind: None,
                detail: None,
            },
            CompletionItem {
                label: "fooBar".into(),
                kind: None,
                detail: None,
            },
            CompletionItem {
                label: "xyzfooabc".into(),
                kind: None,
                detail: None,
            },
            CompletionItem {
                label: "foo".into(),
                kind: None,
                detail: None,
            },
        ];
        sort_completions(&mut items, "foo");
        assert_eq!(items[0].label, "foo"); // prefix match
        assert_eq!(items[1].label, "fooBar"); // prefix match
        assert_eq!(items[2].label, "xyzfooabc"); // contains
        assert_eq!(items[3].label, "zz_other"); // other
    }

    #[test]
    fn sort_empty_prefix_alphabetical() {
        let mut items = vec![
            CompletionItem {
                label: "banana".into(),
                kind: None,
                detail: None,
            },
            CompletionItem {
                label: "Apple".into(),
                kind: None,
                detail: None,
            },
            CompletionItem {
                label: "cherry".into(),
                kind: None,
                detail: None,
            },
        ];
        sort_completions(&mut items, "");
        assert_eq!(items[0].label, "Apple");
        assert_eq!(items[1].label, "banana");
        assert_eq!(items[2].label, "cherry");
    }

    // ---- handle_lean_completions integration ----

    #[tokio::test]
    async fn completions_returns_sorted_items() {
        let client = MockClient {
            content: "def Na".into(),
            completions: vec![
                json!({"label": "Nat.succ", "kind": 3}),
                json!({"label": "Nat.add", "kind": 3}),
                json!({"label": "Nat.zero", "kind": 21}),
            ],
            ..MockClient::new()
        };

        let result = handle_lean_completions(&client, "Main.lean", 1, 7, 32)
            .await
            .unwrap();
        assert_eq!(result.items.len(), 3);
        // All start with "na" so sorted alphabetically among prefix matches.
        assert_eq!(result.items[0].label, "Nat.add");
        assert_eq!(result.items[1].label, "Nat.succ");
        assert_eq!(result.items[2].label, "Nat.zero");
    }

    #[tokio::test]
    async fn completions_maps_kind_integers_to_strings() {
        let client = MockClient {
            content: "x".into(),
            completions: vec![
                json!({"label": "foo", "kind": 3}),
                json!({"label": "bar", "kind": 6}),
                json!({"label": "baz"}),
            ],
            ..MockClient::new()
        };

        let result = handle_lean_completions(&client, "Main.lean", 1, 2, 32)
            .await
            .unwrap();
        let kinds: Vec<Option<&str>> = result.items.iter().map(|i| i.kind.as_deref()).collect();
        assert!(kinds.contains(&Some("Function")));
        assert!(kinds.contains(&Some("Variable")));
        assert!(kinds.contains(&None));
    }

    #[tokio::test]
    async fn completions_respects_max_limit() {
        let client = MockClient {
            content: "x".into(),
            completions: (0..50)
                .map(|i| json!({"label": format!("item{i:02}")}))
                .collect(),
            ..MockClient::new()
        };

        let result = handle_lean_completions(&client, "Main.lean", 1, 2, 5)
            .await
            .unwrap();
        assert_eq!(result.items.len(), 5);
    }

    #[tokio::test]
    async fn completions_prefix_sorting() {
        let client = MockClient {
            content: "def fo".into(),
            completions: vec![
                json!({"label": "zz_unrelated"}),
                json!({"label": "infoof"}),
                json!({"label": "fooBar"}),
                json!({"label": "foo"}),
            ],
            ..MockClient::new()
        };

        let result = handle_lean_completions(&client, "Main.lean", 1, 7, 32)
            .await
            .unwrap();
        // prefix = "fo"
        // "foo" and "fooBar" start with "fo" -> group 0
        // "infoof" contains "fo" -> group 1
        // "zz_unrelated" -> group 2
        assert_eq!(result.items[0].label, "foo");
        assert_eq!(result.items[1].label, "fooBar");
        assert_eq!(result.items[2].label, "infoof");
        assert_eq!(result.items[3].label, "zz_unrelated");
    }

    #[tokio::test]
    async fn completions_with_dot_trigger() {
        let client = MockClient {
            content: "Nat.".into(),
            completions: vec![
                json!({"label": "succ", "kind": 3}),
                json!({"label": "add", "kind": 3}),
            ],
            ..MockClient::new()
        };

        let result = handle_lean_completions(&client, "Main.lean", 1, 5, 32)
            .await
            .unwrap();
        // prefix is empty (dot trigger), so alphabetical order.
        assert_eq!(result.items[0].label, "add");
        assert_eq!(result.items[1].label, "succ");
    }

    #[tokio::test]
    async fn completions_empty_returns_empty() {
        let client = MockClient {
            content: "x".into(),
            completions: vec![],
            ..MockClient::new()
        };

        let result = handle_lean_completions(&client, "Main.lean", 1, 2, 32)
            .await
            .unwrap();
        assert!(result.items.is_empty());
    }

    #[tokio::test]
    async fn completions_skips_items_without_label() {
        let client = MockClient {
            content: "x".into(),
            completions: vec![
                json!({"kind": 3}),                   // no label -> skipped
                json!({"label": "valid", "kind": 3}), // has label -> kept
                json!({"label": 42}),                 // label not a string -> skipped
            ],
            ..MockClient::new()
        };

        let result = handle_lean_completions(&client, "Main.lean", 1, 2, 32)
            .await
            .unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].label, "valid");
    }
}
