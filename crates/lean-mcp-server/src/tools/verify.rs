//! Tool handler for `lean_verify`.
//!
//! Verifies a Lean theorem by appending `#print axioms` to the file,
//! collecting diagnostics, parsing axiom names, and optionally scanning
//! the source for suspicious patterns.

use lean_lsp_client::client::LspClient;
use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::models::{SourceWarning, VerifyResult};
use regex::Regex;
use serde_json::Value;
use std::path::Path;

/// Suspicious source patterns that may affect soundness.
///
/// Matches the 13 patterns from the Python `verify.py` `_WARNING_PATTERNS`.
/// Each entry is a regex pattern string.
pub const WARNING_PATTERNS: &[&str] = &[
    r"set_option\s+debug\.",
    r"\bunsafe\b",
    r"@\[implemented_by\b",
    r"@\[extern\b",
    r"\bopaque\b",
    r"local\s+instance\b",
    r"local\s+notation\b",
    r"local\s+macro_rules\b",
    r"scoped\s+notation\b",
    r"scoped\s+instance\b",
    r"@\[csimp\b",
    r"import\s+Lean\.Elab\b",
    r"import\s+Lean\.Meta\b",
];

/// Parse axiom names from `#print axioms` diagnostic output.
///
/// Looks for severity=3 (info) diagnostics containing
/// `"depends on axioms: [axiom1, axiom2, ...]"` and extracts the
/// comma-separated axiom names from within the brackets.
pub fn parse_axioms(diagnostics: &[Value]) -> Vec<String> {
    let re = Regex::new(r"depends on axioms:\s*\[(.+?)\]").expect("valid regex");
    let mut axioms = Vec::new();

    for diag in diagnostics {
        // severity == 3 means "info" in LSP.
        if diag.get("severity").and_then(Value::as_i64) != Some(3) {
            continue;
        }
        let message = diag
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .replace('\n', " ");

        if let Some(cap) = re.captures(&message) {
            for axiom in cap[1].split(',') {
                let trimmed = axiom.trim();
                if !trimmed.is_empty() {
                    axioms.push(trimmed.to_string());
                }
            }
        }
    }

    axioms
}

/// Check for error diagnostics (severity=1).
///
/// Returns a joined error message string if any error diagnostics exist,
/// or `None` if there are no errors.
pub fn check_axiom_errors(diagnostics: &[Value]) -> Option<String> {
    let errors: Vec<&str> = diagnostics
        .iter()
        .filter(|d| d.get("severity").and_then(Value::as_i64) == Some(1))
        .filter_map(|d| d.get("message").and_then(Value::as_str))
        .collect();

    if errors.is_empty() {
        None
    } else {
        Some(errors.join("; "))
    }
}

/// Scan a file for suspicious source patterns using regex.
///
/// Returns a list of [`SourceWarning`] entries with 1-indexed line numbers
/// and the matched pattern text. Falls back gracefully if the file cannot
/// be read.
pub fn scan_warnings(file_path: &Path) -> Vec<SourceWarning> {
    let content = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let compiled: Vec<Regex> = WARNING_PATTERNS
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect();

    let mut warnings = Vec::new();

    for (line_idx, line_text) in content.lines().enumerate() {
        for re in &compiled {
            if let Some(m) = re.find(line_text) {
                warnings.push(SourceWarning {
                    line: (line_idx + 1) as i64,
                    pattern: m.as_str().to_string(),
                });
                // Only report the first matching pattern per line.
                break;
            }
        }
    }

    warnings
}

/// Handle a `lean_verify` tool call.
///
/// 1. Gets current file content from the LSP client.
/// 2. Appends `#print axioms _root_.{theorem_name}` to the file.
/// 3. Collects diagnostics from the appended region.
/// 4. Parses axiom names from info-severity diagnostics.
/// 5. Reverts file changes.
/// 6. Optionally scans source for suspicious patterns.
pub async fn handle_verify(
    client: &dyn LspClient,
    file_path: &str,
    theorem_name: &str,
    scan_source: bool,
) -> Result<VerifyResult, LeanToolError> {
    let project_path = client.project_path();

    // 1. Open file and get its current content.
    client
        .open_file(file_path)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "open_file".into(),
            message: e.to_string(),
        })?;

    let original_content =
        client
            .get_file_content(file_path)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_file_content".into(),
                message: e.to_string(),
            })?;

    let line_count = original_content.lines().count();

    // 2. Append `#print axioms` command.
    let axiom_line = format!("\n#print axioms _root_.{theorem_name}\n");
    let new_content = format!("{original_content}{axiom_line}");

    client
        .update_file_content(file_path, &new_content)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "update_file_content".into(),
            message: e.to_string(),
        })?;

    // 3. Get diagnostics from the appended region.
    let appended_start = line_count as u32;
    let raw = client
        .get_diagnostics(file_path, Some(appended_start), None, Some(15.0))
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

    // 4. Parse axioms and check for errors.
    let axioms = parse_axioms(&diagnostics_arr);
    let error_msg = check_axiom_errors(&diagnostics_arr);

    // 5. Revert file changes.
    let revert_result = client
        .update_file_content(file_path, &original_content)
        .await;
    if let Err(e) = revert_result {
        // Log but don't fail — the axiom result is still valid.
        tracing::warn!("Failed to revert file after axiom check: {e}");
    }

    // Check for axiom errors.
    if let Some(err) = error_msg {
        return Err(LeanToolError::AxiomCheckFailed(err));
    }

    // 6. Optionally scan source for suspicious patterns.
    let warnings = if scan_source {
        let abs_path = project_path.join(file_path);
        scan_warnings(&abs_path)
    } else {
        vec![]
    };

    Ok(VerifyResult { axioms, warnings })
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
    use tempfile::TempDir;

    // ---- parse_axioms ----

    #[test]
    fn parse_axioms_extracts_from_info_diag() {
        let diags = vec![json!({
            "severity": 3,
            "message": "'myThm' depends on axioms: [propext, Classical.choice, Quot.sound]"
        })];
        let axioms = parse_axioms(&diags);
        assert_eq!(axioms, vec!["propext", "Classical.choice", "Quot.sound"]);
    }

    #[test]
    fn parse_axioms_ignores_non_info() {
        let diags = vec![
            json!({
                "severity": 1,
                "message": "depends on axioms: [propext]"
            }),
            json!({
                "severity": 2,
                "message": "depends on axioms: [Quot.sound]"
            }),
        ];
        let axioms = parse_axioms(&diags);
        assert!(axioms.is_empty());
    }

    #[test]
    fn parse_axioms_handles_multiline_message() {
        let diags = vec![json!({
            "severity": 3,
            "message": "'thm' depends on axioms:\n[propext,\nClassical.choice]"
        })];
        let axioms = parse_axioms(&diags);
        assert_eq!(axioms, vec!["propext", "Classical.choice"]);
    }

    #[test]
    fn parse_axioms_empty_on_no_match() {
        let diags = vec![json!({
            "severity": 3,
            "message": "'thm' does not depend on any axioms"
        })];
        let axioms = parse_axioms(&diags);
        assert!(axioms.is_empty());
    }

    #[test]
    fn parse_axioms_empty_on_empty_input() {
        let axioms = parse_axioms(&[]);
        assert!(axioms.is_empty());
    }

    // ---- check_axiom_errors ----

    #[test]
    fn check_axiom_errors_returns_joined_errors() {
        let diags = vec![
            json!({"severity": 1, "message": "unknown identifier 'foo'"}),
            json!({"severity": 1, "message": "type mismatch"}),
        ];
        let result = check_axiom_errors(&diags);
        assert_eq!(
            result,
            Some("unknown identifier 'foo'; type mismatch".to_string())
        );
    }

    #[test]
    fn check_axiom_errors_returns_none_on_no_errors() {
        let diags = vec![
            json!({"severity": 2, "message": "warning"}),
            json!({"severity": 3, "message": "info"}),
        ];
        let result = check_axiom_errors(&diags);
        assert!(result.is_none());
    }

    #[test]
    fn check_axiom_errors_empty_input() {
        assert!(check_axiom_errors(&[]).is_none());
    }

    // ---- scan_warnings ----

    #[test]
    fn scan_warnings_detects_patterns() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("Test.lean");
        std::fs::write(
            &file,
            "import Mathlib\nset_option debug.foo true\nunsafe def x := 42\n\
             @[implemented_by bar] def baz := 1\ndef normal := 0\n",
        )
        .unwrap();

        let warnings = scan_warnings(&file);
        assert_eq!(warnings.len(), 3);

        assert_eq!(warnings[0].line, 2);
        assert_eq!(warnings[0].pattern, "set_option debug.");

        assert_eq!(warnings[1].line, 3);
        assert_eq!(warnings[1].pattern, "unsafe");

        assert_eq!(warnings[2].line, 4);
        assert!(warnings[2].pattern.contains("implemented_by"));
    }

    #[test]
    fn scan_warnings_detects_all_13_patterns() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("AllPatterns.lean");
        std::fs::write(
            &file,
            "set_option debug.x true\n\
             unsafe def a := 1\n\
             @[implemented_by b] def c := 1\n\
             @[extern \"x\"] def d := 1\n\
             opaque e : Nat\n\
             local instance f : Decidable True := .isTrue trivial\n\
             local notation \"x\" => 1\n\
             local macro_rules | _ => `(1)\n\
             scoped notation \"y\" => 2\n\
             scoped instance g : Decidable True := .isTrue trivial\n\
             @[csimp] theorem h : True := trivial\n\
             import Lean.Elab\n\
             import Lean.Meta\n",
        )
        .unwrap();

        let warnings = scan_warnings(&file);
        assert_eq!(warnings.len(), 13);
        for (i, w) in warnings.iter().enumerate() {
            assert_eq!(w.line, (i + 1) as i64);
        }
    }

    #[test]
    fn scan_warnings_empty_on_clean_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("Clean.lean");
        std::fs::write(&file, "theorem foo : True := trivial\n").unwrap();

        let warnings = scan_warnings(&file);
        assert!(warnings.is_empty());
    }

    #[test]
    fn scan_warnings_returns_empty_on_missing_file() {
        let warnings = scan_warnings(Path::new("/nonexistent/path/Test.lean"));
        assert!(warnings.is_empty());
    }

    // ---- WARNING_PATTERNS constant ----

    #[test]
    fn warning_patterns_has_13_entries() {
        assert_eq!(WARNING_PATTERNS.len(), 13);
    }

    #[test]
    fn warning_patterns_all_compile() {
        for pattern in WARNING_PATTERNS {
            assert!(
                Regex::new(pattern).is_ok(),
                "Pattern failed to compile: {pattern}"
            );
        }
    }

    // ---- handle_verify (mock LSP) ----

    /// Mock LSP client for verify handler tests.
    struct MockVerifyClient {
        project: PathBuf,
        file_content: String,
        diagnostics_response: Value,
    }

    impl MockVerifyClient {
        fn new(project: PathBuf, file_content: &str) -> Self {
            Self {
                project,
                file_content: file_content.to_string(),
                diagnostics_response: json!({
                    "diagnostics": [],
                    "success": true
                }),
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
    impl LspClient for MockVerifyClient {
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
            Ok(self.file_content.clone())
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

    #[tokio::test]
    async fn handle_verify_returns_axioms() {
        let dir = TempDir::new().unwrap();
        let client =
            MockVerifyClient::new(dir.path().to_path_buf(), "theorem foo : True := trivial\n")
                .with_diagnostics(vec![json!({
                    "severity": 3,
                    "message": "'foo' depends on axioms: [propext, Classical.choice, Quot.sound]"
                })]);

        let result = handle_verify(&client, "Foo.lean", "foo", false)
            .await
            .unwrap();

        assert_eq!(
            result.axioms,
            vec!["propext", "Classical.choice", "Quot.sound"]
        );
        assert!(result.warnings.is_empty());
    }

    #[tokio::test]
    async fn handle_verify_with_errors_returns_axiom_check_failed() {
        let dir = TempDir::new().unwrap();
        let client = MockVerifyClient::new(dir.path().to_path_buf(), "-- content\n")
            .with_diagnostics(vec![json!({
                "severity": 1,
                "message": "unknown identifier 'nonexistent'"
            })]);

        let err = handle_verify(&client, "Bad.lean", "nonexistent", false)
            .await
            .unwrap_err();

        match err {
            LeanToolError::AxiomCheckFailed(msg) => {
                assert!(msg.contains("unknown identifier"));
            }
            other => panic!("expected AxiomCheckFailed, got: {other}"),
        }
    }

    #[tokio::test]
    async fn handle_verify_with_source_scan() {
        let dir = TempDir::new().unwrap();
        // Create a file with a suspicious pattern.
        let lean_file = dir.path().join("Suspicious.lean");
        std::fs::write(
            &lean_file,
            "unsafe def x := 42\ntheorem bar : True := trivial\n",
        )
        .unwrap();

        let client = MockVerifyClient::new(
            dir.path().to_path_buf(),
            "unsafe def x := 42\ntheorem bar : True := trivial\n",
        )
        .with_diagnostics(vec![json!({
            "severity": 3,
            "message": "'bar' depends on axioms: [propext]"
        })]);

        let result = handle_verify(&client, "Suspicious.lean", "bar", true)
            .await
            .unwrap();

        assert_eq!(result.axioms, vec!["propext"]);
        assert_eq!(result.warnings.len(), 1);
        assert_eq!(result.warnings[0].line, 1);
        assert_eq!(result.warnings[0].pattern, "unsafe");
    }

    #[tokio::test]
    async fn handle_verify_no_axioms() {
        let dir = TempDir::new().unwrap();
        let client = MockVerifyClient::new(
            dir.path().to_path_buf(),
            "theorem trivialThm : True := trivial\n",
        )
        .with_diagnostics(vec![json!({
            "severity": 3,
            "message": "'trivialThm' does not depend on any axioms"
        })]);

        let result = handle_verify(&client, "Trivial.lean", "trivialThm", false)
            .await
            .unwrap();

        assert!(result.axioms.is_empty());
        assert!(result.warnings.is_empty());
    }
}
