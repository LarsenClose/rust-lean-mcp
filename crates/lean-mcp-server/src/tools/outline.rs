//! Tool handler for `lean_file_outline`.
//!
//! Retrieves a token-efficient file skeleton: imports and top-level
//! declarations with optional type signatures, tags, and nested children.
//!
//! This is a simplified v1 that uses document symbol information directly
//! from the LSP rather than injecting `#info_trees in` commands.

use lean_lsp_client::client::LspClient;
use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::models::{FileOutline, OutlineEntry};
use serde_json::Value;

// ---------------------------------------------------------------------------
// LSP SymbolKind constants (from the LSP specification)
// ---------------------------------------------------------------------------

/// LSP SymbolKind: Module (used for Lean namespaces).
const SYMBOL_KIND_MODULE: i64 = 2;
/// LSP SymbolKind: Namespace.
const SYMBOL_KIND_NAMESPACE: i64 = 3;
/// LSP SymbolKind: Class.
const SYMBOL_KIND_CLASS: i64 = 5;
/// LSP SymbolKind: Method.
const SYMBOL_KIND_METHOD: i64 = 6;
/// LSP SymbolKind: Constructor.
const SYMBOL_KIND_CONSTRUCTOR: i64 = 9;
/// LSP SymbolKind: Enum (used for inductive types).
const SYMBOL_KIND_ENUM: i64 = 10;
/// LSP SymbolKind: Function.
const SYMBOL_KIND_FUNCTION: i64 = 12;
/// LSP SymbolKind: Struct.
const SYMBOL_KIND_STRUCT: i64 = 23;

// ---------------------------------------------------------------------------
// Import extraction
// ---------------------------------------------------------------------------

/// Extract import statements from file content.
///
/// Recognises lines starting with `import ` or `public import `.
/// Stops scanning once a non-import, non-blank, non-comment line is found
/// (imports must appear at the top of a Lean file).
fn extract_imports(content: &str) -> Vec<String> {
    let mut imports = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("--") {
            continue;
        }
        if trimmed.starts_with("import ") || trimmed.starts_with("public import ") {
            imports.push(trimmed.to_string());
        } else {
            // Once we hit a non-import line, stop scanning.
            break;
        }
    }
    imports
}

// ---------------------------------------------------------------------------
// Tag detection
// ---------------------------------------------------------------------------

/// Lean declaration keywords that map to the `Thm` tag.
const THEOREM_KEYWORDS: &[&str] = &["theorem", "lemma"];

/// Detect a tag for an outline entry based on its LSP symbol kind, name,
/// and the optional detail/type information.
///
/// Tags: Thm, Def, Class, Struct, Ns, Ex.
fn detect_tag(kind: i64, name: &str, detail: Option<&str>) -> String {
    // Namespace / module
    if kind == SYMBOL_KIND_MODULE || kind == SYMBOL_KIND_NAMESPACE {
        return "Ns".to_string();
    }

    // Class
    if kind == SYMBOL_KIND_CLASS {
        return "Class".to_string();
    }

    // Struct
    if kind == SYMBOL_KIND_STRUCT || kind == SYMBOL_KIND_ENUM {
        return "Struct".to_string();
    }

    // Check if the name itself hints at the kind (Lean LSP often uses
    // Function kind for theorems/defs/examples alike).
    let lower_name = name.to_lowercase();

    // Example detection
    if lower_name.starts_with("example")
        || lower_name == "example"
        || lower_name.starts_with("example_")
    {
        return "Ex".to_string();
    }

    // Check detail/type for theorem indicators
    if let Some(detail_str) = detail {
        // If detail contains theorem keywords
        for kw in THEOREM_KEYWORDS {
            if detail_str.starts_with(kw) {
                return "Thm".to_string();
            }
        }
        // If detail contains universal quantifier or equality (common in theorems)
        if detail_str.contains('\u{2200}') || detail_str.contains('=') {
            return "Thm".to_string();
        }
    }

    // Constructor
    if kind == SYMBOL_KIND_CONSTRUCTOR {
        return "Def".to_string();
    }

    // Default: Function / Method → Def
    if kind == SYMBOL_KIND_FUNCTION || kind == SYMBOL_KIND_METHOD {
        return "Def".to_string();
    }

    // Fallback
    "Def".to_string()
}

// ---------------------------------------------------------------------------
// Symbol flattening / conversion
// ---------------------------------------------------------------------------

/// Convert an LSP document symbol JSON value to an `OutlineEntry`.
///
/// Recursively processes children. The `kind` field from LSP is an integer
/// (SymbolKind enum); we map it to a human-readable tag.
fn symbol_to_entry(symbol: &Value) -> Option<OutlineEntry> {
    let name = symbol.get("name").and_then(Value::as_str)?;
    let kind_int = symbol.get("kind").and_then(Value::as_i64).unwrap_or(0);
    let detail = symbol.get("detail").and_then(Value::as_str);

    let range = symbol.get("range")?;
    let start_line = range
        .pointer("/start/line")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        + 1; // Convert 0-indexed to 1-indexed
    let end_line = range
        .pointer("/end/line")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        + 1;

    let tag = detect_tag(kind_int, name, detail);

    // Use the detail field as the type signature if available.
    // TODO: In v2, implement full type extraction using `#info_trees in`
    // commands inserted before each declaration, similar to the Python
    // implementation in outline_utils.py. This would provide accurate
    // type signatures for all declarations, not just those where the LSP
    // populates the detail field.
    let type_signature = detail.map(|d| d.to_string());

    // Recursively process children
    let children = symbol
        .get("children")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(symbol_to_entry).collect())
        .unwrap_or_default();

    Some(OutlineEntry {
        name: name.to_string(),
        kind: tag,
        start_line,
        end_line,
        type_signature,
        children,
    })
}

// ---------------------------------------------------------------------------
// Public handler
// ---------------------------------------------------------------------------

/// Handle a `lean_file_outline` tool call.
///
/// Opens the file, extracts imports from content, retrieves document symbols
/// from the LSP, converts them to outline entries with tag detection, and
/// optionally truncates the declaration list.
pub async fn handle_file_outline(
    client: &dyn LspClient,
    file_path: &str,
    max_declarations: Option<usize>,
) -> Result<FileOutline, LeanToolError> {
    // 1. Open the file in the LSP server.
    client
        .open_file(file_path)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "open_file".into(),
            message: e.to_string(),
        })?;

    // 2. Get file content for import extraction.
    let content =
        client
            .get_file_content(file_path)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_file_content".into(),
                message: e.to_string(),
            })?;

    // 3. Extract imports from file content.
    let imports = extract_imports(&content);

    // 4. Get document symbols from LSP.
    let symbols =
        client
            .get_document_symbols(file_path)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_document_symbols".into(),
                message: e.to_string(),
            })?;

    // 5. Convert symbols to outline entries.
    let declarations: Vec<OutlineEntry> = symbols.iter().filter_map(symbol_to_entry).collect();

    // 6. Apply max_declarations truncation.
    let (declarations, total_declarations) = match max_declarations {
        Some(max) if max < declarations.len() => {
            let total = declarations.len() as i64;
            let truncated = declarations.into_iter().take(max).collect();
            (truncated, Some(total))
        }
        _ => (declarations, None),
    };

    Ok(FileOutline {
        imports,
        declarations,
        total_declarations,
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

    // -- Mock LSP client for outline tests --

    struct MockOutlineClient {
        project: PathBuf,
        content: String,
        symbols: Vec<Value>,
    }

    impl MockOutlineClient {
        fn new() -> Self {
            Self {
                project: PathBuf::from("/test/project"),
                content: String::new(),
                symbols: Vec::new(),
            }
        }

        fn with_content(mut self, content: &str) -> Self {
            self.content = content.to_string();
            self
        }

        fn with_symbols(mut self, symbols: Vec<Value>) -> Self {
            self.symbols = symbols;
            self
        }
    }

    #[async_trait]
    impl LspClient for MockOutlineClient {
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
            Ok(self.content.clone())
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
            Ok(json!({"diagnostics": [], "success": true}))
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
            Ok(self.symbols.clone())
        }
        async fn get_code_actions(
            &self,
            _p: &str,
            _sl: u32,
            _sc: u32,
            _el: u32,
            _ec: u32,
        ) -> Result<Vec<Value>, LspClientError> {
            Ok(vec![])
        }
        async fn get_code_action_resolve(&self, _a: Value) -> Result<Value, LspClientError> {
            Ok(json!({}))
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

    // ---- extract_imports ----

    #[test]
    fn extract_imports_basic() {
        let content = "import Mathlib.Tactic\nimport Init.Data.Nat\n\ndef foo := 42\n";
        let imports = extract_imports(content);
        assert_eq!(
            imports,
            vec!["import Mathlib.Tactic", "import Init.Data.Nat"]
        );
    }

    #[test]
    fn extract_imports_with_public() {
        let content = "public import Mathlib.Tactic\nimport Init\n\ndef foo := 42\n";
        let imports = extract_imports(content);
        assert_eq!(imports, vec!["public import Mathlib.Tactic", "import Init"]);
    }

    #[test]
    fn extract_imports_with_comments_and_blanks() {
        let content =
            "-- header comment\n\nimport Mathlib.Tactic\n\ntheorem foo : True := trivial\n";
        let imports = extract_imports(content);
        assert_eq!(imports, vec!["import Mathlib.Tactic"]);
    }

    #[test]
    fn extract_imports_empty_content() {
        let imports = extract_imports("");
        assert!(imports.is_empty());
    }

    #[test]
    fn extract_imports_no_imports() {
        let content = "def foo := 42\ntheorem bar : True := trivial\n";
        let imports = extract_imports(content);
        assert!(imports.is_empty());
    }

    // ---- detect_tag ----

    #[test]
    fn tag_namespace_from_module_kind() {
        assert_eq!(detect_tag(SYMBOL_KIND_MODULE, "MyNamespace", None), "Ns");
    }

    #[test]
    fn tag_namespace_from_namespace_kind() {
        assert_eq!(detect_tag(SYMBOL_KIND_NAMESPACE, "MyNamespace", None), "Ns");
    }

    #[test]
    fn tag_class() {
        assert_eq!(detect_tag(SYMBOL_KIND_CLASS, "Monad", None), "Class");
    }

    #[test]
    fn tag_struct_from_struct_kind() {
        assert_eq!(detect_tag(SYMBOL_KIND_STRUCT, "Point", None), "Struct");
    }

    #[test]
    fn tag_struct_from_enum_kind() {
        assert_eq!(detect_tag(SYMBOL_KIND_ENUM, "Color", None), "Struct");
    }

    #[test]
    fn tag_theorem_from_detail_keyword() {
        assert_eq!(
            detect_tag(SYMBOL_KIND_FUNCTION, "add_comm", Some("theorem add_comm")),
            "Thm"
        );
    }

    #[test]
    fn tag_theorem_from_detail_with_forall() {
        assert_eq!(
            detect_tag(
                SYMBOL_KIND_FUNCTION,
                "foo",
                Some("\u{2200} (n : Nat), n = n")
            ),
            "Thm"
        );
    }

    #[test]
    fn tag_theorem_from_detail_with_equality() {
        assert_eq!(
            detect_tag(SYMBOL_KIND_FUNCTION, "foo", Some("n + 0 = n")),
            "Thm"
        );
    }

    #[test]
    fn tag_def_for_function() {
        assert_eq!(detect_tag(SYMBOL_KIND_FUNCTION, "myFunc", None), "Def");
    }

    #[test]
    fn tag_def_for_method() {
        assert_eq!(detect_tag(SYMBOL_KIND_METHOD, "myMethod", None), "Def");
    }

    #[test]
    fn tag_example() {
        assert_eq!(detect_tag(SYMBOL_KIND_FUNCTION, "example", None), "Ex");
    }

    #[test]
    fn tag_example_prefixed() {
        assert_eq!(detect_tag(SYMBOL_KIND_FUNCTION, "example_foo", None), "Ex");
    }

    // ---- symbol_to_entry ----

    #[test]
    fn symbol_to_entry_basic() {
        let sym = json!({
            "name": "myDef",
            "kind": 12,
            "range": {
                "start": {"line": 5, "character": 0},
                "end": {"line": 10, "character": 0}
            }
        });
        let entry = symbol_to_entry(&sym).unwrap();
        assert_eq!(entry.name, "myDef");
        assert_eq!(entry.kind, "Def");
        assert_eq!(entry.start_line, 6); // 0-indexed + 1
        assert_eq!(entry.end_line, 11);
        assert!(entry.type_signature.is_none());
        assert!(entry.children.is_empty());
    }

    #[test]
    fn symbol_to_entry_with_detail() {
        let sym = json!({
            "name": "add_comm",
            "kind": 12,
            "detail": "theorem add_comm : a + b = b + a",
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 3, "character": 0}
            }
        });
        let entry = symbol_to_entry(&sym).unwrap();
        assert_eq!(entry.name, "add_comm");
        assert_eq!(entry.kind, "Thm");
        assert_eq!(
            entry.type_signature,
            Some("theorem add_comm : a + b = b + a".to_string())
        );
    }

    #[test]
    fn symbol_to_entry_with_children() {
        let sym = json!({
            "name": "MyNs",
            "kind": 2,
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 20, "character": 0}
            },
            "children": [
                {
                    "name": "innerDef",
                    "kind": 12,
                    "range": {
                        "start": {"line": 2, "character": 2},
                        "end": {"line": 5, "character": 0}
                    }
                }
            ]
        });
        let entry = symbol_to_entry(&sym).unwrap();
        assert_eq!(entry.name, "MyNs");
        assert_eq!(entry.kind, "Ns");
        assert_eq!(entry.children.len(), 1);
        assert_eq!(entry.children[0].name, "innerDef");
        assert_eq!(entry.children[0].kind, "Def");
    }

    #[test]
    fn symbol_to_entry_missing_name_returns_none() {
        let sym = json!({
            "kind": 12,
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 1, "character": 0}
            }
        });
        assert!(symbol_to_entry(&sym).is_none());
    }

    #[test]
    fn symbol_to_entry_missing_range_returns_none() {
        let sym = json!({
            "name": "foo",
            "kind": 12
        });
        assert!(symbol_to_entry(&sym).is_none());
    }

    // ---- handle_file_outline (async integration tests) ----

    #[tokio::test]
    async fn outline_with_imports() {
        let content = "import Mathlib.Tactic\nimport Init\n\ndef foo := 42\n";
        let symbols = vec![json!({
            "name": "foo",
            "kind": 12,
            "range": {
                "start": {"line": 3, "character": 0},
                "end": {"line": 3, "character": 14}
            }
        })];

        let client = MockOutlineClient::new()
            .with_content(content)
            .with_symbols(symbols);

        let result = handle_file_outline(&client, "Main.lean", None)
            .await
            .unwrap();

        assert_eq!(result.imports.len(), 2);
        assert_eq!(result.imports[0], "import Mathlib.Tactic");
        assert_eq!(result.imports[1], "import Init");
        assert_eq!(result.declarations.len(), 1);
        assert_eq!(result.declarations[0].name, "foo");
        assert_eq!(result.declarations[0].kind, "Def");
        assert!(result.total_declarations.is_none());
    }

    #[tokio::test]
    async fn outline_with_nested_namespaces() {
        let content = "namespace Outer\nnamespace Inner\ndef foo := 42\nend Inner\nend Outer\n";
        let symbols = vec![json!({
            "name": "Outer",
            "kind": 2,
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 4, "character": 10}
            },
            "children": [
                {
                    "name": "Inner",
                    "kind": 2,
                    "range": {
                        "start": {"line": 1, "character": 0},
                        "end": {"line": 3, "character": 9}
                    },
                    "children": [
                        {
                            "name": "foo",
                            "kind": 12,
                            "range": {
                                "start": {"line": 2, "character": 0},
                                "end": {"line": 2, "character": 14}
                            }
                        }
                    ]
                }
            ]
        })];

        let client = MockOutlineClient::new()
            .with_content(content)
            .with_symbols(symbols);

        let result = handle_file_outline(&client, "Main.lean", None)
            .await
            .unwrap();

        assert_eq!(result.declarations.len(), 1);
        let outer = &result.declarations[0];
        assert_eq!(outer.name, "Outer");
        assert_eq!(outer.kind, "Ns");
        assert_eq!(outer.children.len(), 1);

        let inner = &outer.children[0];
        assert_eq!(inner.name, "Inner");
        assert_eq!(inner.kind, "Ns");
        assert_eq!(inner.children.len(), 1);

        let foo = &inner.children[0];
        assert_eq!(foo.name, "foo");
        assert_eq!(foo.kind, "Def");
    }

    #[tokio::test]
    async fn outline_empty_file() {
        let client = MockOutlineClient::new()
            .with_content("")
            .with_symbols(vec![]);

        let result = handle_file_outline(&client, "Empty.lean", None)
            .await
            .unwrap();

        assert!(result.imports.is_empty());
        assert!(result.declarations.is_empty());
        assert!(result.total_declarations.is_none());
    }

    #[tokio::test]
    async fn outline_max_declarations_truncation() {
        let content = "def a := 1\ndef b := 2\ndef c := 3\n";
        let symbols = vec![
            json!({
                "name": "a",
                "kind": 12,
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 10}
                }
            }),
            json!({
                "name": "b",
                "kind": 12,
                "range": {
                    "start": {"line": 1, "character": 0},
                    "end": {"line": 1, "character": 10}
                }
            }),
            json!({
                "name": "c",
                "kind": 12,
                "range": {
                    "start": {"line": 2, "character": 0},
                    "end": {"line": 2, "character": 10}
                }
            }),
        ];

        let client = MockOutlineClient::new()
            .with_content(content)
            .with_symbols(symbols);

        let result = handle_file_outline(&client, "Main.lean", Some(2))
            .await
            .unwrap();

        assert_eq!(result.declarations.len(), 2);
        assert_eq!(result.declarations[0].name, "a");
        assert_eq!(result.declarations[1].name, "b");
        assert_eq!(result.total_declarations, Some(3));
    }

    #[tokio::test]
    async fn outline_max_declarations_no_truncation_when_within_limit() {
        let content = "def a := 1\n";
        let symbols = vec![json!({
            "name": "a",
            "kind": 12,
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 10}
            }
        })];

        let client = MockOutlineClient::new()
            .with_content(content)
            .with_symbols(symbols);

        let result = handle_file_outline(&client, "Main.lean", Some(10))
            .await
            .unwrap();

        assert_eq!(result.declarations.len(), 1);
        assert!(result.total_declarations.is_none());
    }

    #[tokio::test]
    async fn outline_tag_detection_theorem() {
        let content = "theorem add_comm (a b : Nat) : a + b = b + a := sorry\n";
        let symbols = vec![json!({
            "name": "add_comm",
            "kind": 12,
            "detail": "theorem add_comm : a + b = b + a",
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 53}
            }
        })];

        let client = MockOutlineClient::new()
            .with_content(content)
            .with_symbols(symbols);

        let result = handle_file_outline(&client, "Main.lean", None)
            .await
            .unwrap();

        assert_eq!(result.declarations.len(), 1);
        assert_eq!(result.declarations[0].kind, "Thm");
        assert_eq!(result.declarations[0].name, "add_comm");
    }

    #[tokio::test]
    async fn outline_tag_detection_def() {
        let content = "def myFunc (n : Nat) : Nat := n + 1\n";
        let symbols = vec![json!({
            "name": "myFunc",
            "kind": 12,
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 36}
            }
        })];

        let client = MockOutlineClient::new()
            .with_content(content)
            .with_symbols(symbols);

        let result = handle_file_outline(&client, "Main.lean", None)
            .await
            .unwrap();

        assert_eq!(result.declarations[0].kind, "Def");
    }

    #[tokio::test]
    async fn outline_tag_detection_namespace() {
        let content = "namespace Foo\nend Foo\n";
        let symbols = vec![json!({
            "name": "Foo",
            "kind": 2,
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 1, "character": 7}
            }
        })];

        let client = MockOutlineClient::new()
            .with_content(content)
            .with_symbols(symbols);

        let result = handle_file_outline(&client, "Main.lean", None)
            .await
            .unwrap();

        assert_eq!(result.declarations[0].kind, "Ns");
    }

    #[tokio::test]
    async fn outline_tag_detection_example() {
        let content = "example : True := trivial\n";
        let symbols = vec![json!({
            "name": "example",
            "kind": 12,
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 25}
            }
        })];

        let client = MockOutlineClient::new()
            .with_content(content)
            .with_symbols(symbols);

        let result = handle_file_outline(&client, "Main.lean", None)
            .await
            .unwrap();

        assert_eq!(result.declarations[0].kind, "Ex");
    }
}
