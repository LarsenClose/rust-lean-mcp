//! Shared utility for resolving declaration names to line ranges via document symbols.
//!
//! Extracted from `diagnostics.rs` so that position-based tools (`lean_goal`,
//! `lean_term_goal`, `lean_hover_info`, `lean_completions`, `lean_references`)
//! can also resolve a `declaration_name` to its current line.

use lean_lsp_client::client::LspClient;
use lean_mcp_core::error::LeanToolError;
use serde_json::Value;

/// Recursively search document symbols for `target_name` (case-sensitive).
pub fn search_symbols<'a>(symbols: &'a [Value], target_name: &str) -> Option<&'a Value> {
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

/// Resolve a declaration name to its 1-indexed `(start_line, end_line)` range
/// via `textDocument/documentSymbol`.
pub async fn get_declaration_range(
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

/// Resolve a declaration name to its 1-indexed start line, returning
/// [`LeanToolError::DeclarationNotFound`] when the symbol is absent.
pub async fn resolve_declaration_line(
    client: &dyn LspClient,
    file_path: &str,
    declaration_name: &str,
) -> Result<u32, LeanToolError> {
    match get_declaration_range(client, file_path, declaration_name).await? {
        Some((start_line, _)) => Ok(start_line),
        None => Err(LeanToolError::DeclarationNotFound(
            declaration_name.to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    #[test]
    fn search_symbols_empty_list() {
        assert!(search_symbols(&[], "anything").is_none());
    }

    #[test]
    fn search_symbols_deeply_nested() {
        let symbols = vec![json!({
            "name": "A",
            "kind": 2,
            "children": [{
                "name": "B",
                "kind": 2,
                "children": [{
                    "name": "deep",
                    "kind": 12
                }]
            }]
        })];
        let found = search_symbols(&symbols, "deep");
        assert!(found.is_some());
        assert_eq!(found.unwrap()["name"], "deep");
    }
}
