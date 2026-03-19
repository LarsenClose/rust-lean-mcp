//! Tool handler for `lean_declaration_file`.
//!
//! Finds where a symbol is declared and returns the declaration file content.

use lean_lsp_client::client::{uri_to_path, LspClient};
use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::file_utils::get_file_contents;
use lean_mcp_core::models::DeclarationInfo;
use lean_mcp_core::utils::find_start_position;

/// Handle a `lean_declaration_file` tool call.
///
/// Opens the file, finds the first occurrence of `symbol` in its content,
/// queries the LSP for go-to-declaration at that position, reads the
/// declaration file, and returns its path and content.
///
/// # Errors
///
/// - [`LeanToolError::SymbolNotFound`] if `symbol` is not in the file content.
/// - [`LeanToolError::NoDeclaration`] if the LSP returns no declarations.
pub async fn handle_declaration_file(
    client: &dyn LspClient,
    file_path: &str,
    symbol: &str,
) -> Result<DeclarationInfo, LeanToolError> {
    // 1. Open file and get its content.
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

    // 2. Find the first occurrence of the symbol (case-sensitive).
    let (line, col) = find_start_position(&content, symbol)
        .ok_or_else(|| LeanToolError::SymbolNotFound(symbol.to_string()))?;

    // 3. Get declarations at that position (already 0-indexed from find_start_position).
    let declarations = client
        .get_declarations(file_path, line as u32, col as u32)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "get_declarations".into(),
            message: e.to_string(),
        })?;

    if declarations.is_empty() {
        return Err(LeanToolError::NoDeclaration(symbol.to_string()));
    }

    // 4. Extract targetUri (or uri) from the first declaration.
    let decl = &declarations[0];
    let uri = decl
        .get("targetUri")
        .or_else(|| decl.get("uri"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| LeanToolError::NoDeclaration(symbol.to_string()))?;

    // 5. Convert URI to absolute path.
    let abs_path = uri_to_path(uri)
        .ok_or_else(|| LeanToolError::Other(format!("Could not convert URI to path: {uri}")))?;

    let abs_path_str = abs_path.to_string_lossy().into_owned();

    // 6. Read declaration file content.
    let file_content = get_file_contents(&abs_path_str).map_err(|e| {
        LeanToolError::Other(format!(
            "Could not open declaration file `{abs_path_str}` for `{symbol}`: {e}"
        ))
    })?;

    Ok(DeclarationInfo {
        file_path: abs_path_str,
        content: file_content,
    })
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
    use tempfile::TempDir;

    /// Mock LSP client for declaration handler tests.
    struct MockDeclClient {
        project: PathBuf,
        content: String,
        /// Canned declarations keyed by (0-indexed line, 0-indexed col).
        declarations_responses: Vec<((u32, u32), Vec<Value>)>,
    }

    impl MockDeclClient {
        fn new(content: &str) -> Self {
            Self {
                project: PathBuf::from("/test/project"),
                content: content.to_string(),
                declarations_responses: Vec::new(),
            }
        }

        fn with_declarations(mut self, line: u32, col: u32, decls: Vec<Value>) -> Self {
            self.declarations_responses.push(((line, col), decls));
            self
        }
    }

    #[async_trait]
    impl LspClient for MockDeclClient {
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
            Ok(self.content.clone())
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
            l: u32,
            c: u32,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            for ((rl, rc), decls) in &self.declarations_responses {
                if *rl == l && *rc == c {
                    return Ok(decls.clone());
                }
            }
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

    // ---- symbol found and declaration returned ----

    #[tokio::test]
    async fn declaration_file_returns_content() {
        let dir = TempDir::new().unwrap();
        let decl_file = dir.path().join("Decl.lean");
        std::fs::write(&decl_file, "-- declaration source\ndef bar := 42\n").unwrap();

        let decl_uri = format!("file://{}", decl_file.display());

        // Content has "Nat.add" starting at line 1, col 9 (0-indexed).
        let client = MockDeclClient::new("import Mathlib\ndef x := Nat.add 1 2").with_declarations(
            1,
            9,
            vec![json!({
                "targetUri": decl_uri,
                "targetRange": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 1, "character": 0}
                }
            })],
        );

        let result = handle_declaration_file(&client, "Main.lean", "Nat.add")
            .await
            .unwrap();

        assert_eq!(result.file_path, decl_file.to_string_lossy());
        assert!(result.content.contains("def bar := 42"));
    }

    // ---- symbol found, declaration uses "uri" key instead of "targetUri" ----

    #[tokio::test]
    async fn declaration_file_uses_uri_fallback() {
        let dir = TempDir::new().unwrap();
        let decl_file = dir.path().join("Other.lean");
        std::fs::write(&decl_file, "-- other file\n").unwrap();

        let decl_uri = format!("file://{}", decl_file.display());

        let client = MockDeclClient::new("def foo := bar").with_declarations(
            0,
            11,
            vec![json!({ "uri": decl_uri })],
        );

        let result = handle_declaration_file(&client, "Main.lean", "bar")
            .await
            .unwrap();

        assert_eq!(result.file_path, decl_file.to_string_lossy());
        assert!(result.content.contains("-- other file"));
    }

    // ---- symbol not found in file ----

    #[tokio::test]
    async fn declaration_file_symbol_not_found() {
        let client = MockDeclClient::new("def foo := 42");

        let err = handle_declaration_file(&client, "Main.lean", "nonexistent")
            .await
            .unwrap_err();

        match err {
            LeanToolError::SymbolNotFound(name) => {
                assert_eq!(name, "nonexistent");
            }
            other => panic!("expected SymbolNotFound, got: {other}"),
        }
    }

    // ---- no declaration available ----

    #[tokio::test]
    async fn declaration_file_no_declaration() {
        // Symbol "foo" is at (0, 4) -- but declarations returns empty.
        let client = MockDeclClient::new("def foo := 42");

        let err = handle_declaration_file(&client, "Main.lean", "foo")
            .await
            .unwrap_err();

        match err {
            LeanToolError::NoDeclaration(name) => {
                assert_eq!(name, "foo");
            }
            other => panic!("expected NoDeclaration, got: {other}"),
        }
    }

    // ---- declaration with missing uri field ----

    #[tokio::test]
    async fn declaration_file_missing_uri_returns_error() {
        let client = MockDeclClient::new("def foo := 42").with_declarations(
            0,
            4,
            vec![json!({"targetRange": {"start": {"line": 0}, "end": {"line": 1}}})],
        );

        let err = handle_declaration_file(&client, "Main.lean", "foo")
            .await
            .unwrap_err();

        match err {
            LeanToolError::NoDeclaration(name) => {
                assert_eq!(name, "foo");
            }
            other => panic!("expected NoDeclaration, got: {other}"),
        }
    }
}
