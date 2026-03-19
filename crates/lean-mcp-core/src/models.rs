//! Output models matching the Python Pydantic models from `lean_lsp_mcp/models.py`.
//!
//! Every struct derives `Debug, Clone, Serialize, Deserialize` and, where practical,
//! `schemars::JsonSchema` for automatic JSON-Schema generation.

use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Diagnostic severity levels, serialised as lowercase strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

// ---------------------------------------------------------------------------
// Search results
// ---------------------------------------------------------------------------

/// A single result from local (project-level) declaration search.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LocalSearchResult {
    /// Declaration name.
    pub name: String,
    /// Declaration kind (theorem, def, class, etc.).
    pub kind: String,
    /// Relative file path.
    pub file: String,
}

/// A single result from the LeanSearch API.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LeanSearchResult {
    /// Full qualified name.
    pub name: String,
    /// Module where declared.
    pub module_name: String,
    /// Declaration kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Type signature.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
}

/// A single result from the Loogle search engine.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LoogleResult {
    /// Declaration name.
    pub name: String,
    /// Type signature.
    #[serde(rename = "type")]
    pub r#type: String,
    /// Module where declared.
    pub module: String,
}

/// A single result from LeanFinder (semantic/conceptual search).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LeanFinderResult {
    /// Full qualified name.
    pub full_name: String,
    /// Lean type signature.
    pub formal_statement: String,
    /// Natural language description.
    pub informal_statement: String,
}

/// A single result from state (goal) search.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StateSearchResult {
    /// Theorem/lemma name.
    pub name: String,
}

/// A single premise result for simp/omega/aesop.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PremiseResult {
    /// Premise name for simp/omega/aesop.
    pub name: String,
}

// ---------------------------------------------------------------------------
// Core
// ---------------------------------------------------------------------------

/// A compiler diagnostic message.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticMessage {
    /// Severity: error, warning, info, or hint.
    pub severity: String,
    /// Diagnostic message text.
    pub message: String,
    /// Line (1-indexed).
    pub line: i64,
    /// Column (1-indexed).
    pub column: i64,
}

/// Proof state at a given source position.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GoalState {
    /// Source line where goals were queried.
    pub line_context: String,
    /// Goal list at specified column position.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goals: Option<Vec<String>>,
    /// Goals at line start (when column omitted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goals_before: Option<Vec<String>>,
    /// Goals at line end (when column omitted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goals_after: Option<Vec<String>>,
}

/// A single completion item from IDE autocomplete.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompletionItem {
    /// Completion text to insert.
    pub label: String,
    /// Completion kind (function, variable, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Additional detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Hover information for a symbol.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HoverInfo {
    /// The symbol being hovered.
    pub symbol: String,
    /// Type signature and documentation.
    pub info: String,
    /// Diagnostics at this position.
    #[serde(default)]
    pub diagnostics: Vec<DiagnosticMessage>,
}

/// Term-level goal state.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TermGoalState {
    /// Source line where term goal was queried.
    pub line_context: String,
    /// Expected type at this position.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_type: Option<String>,
}

// ---------------------------------------------------------------------------
// Outline
// ---------------------------------------------------------------------------

/// A single entry in a file outline (recursive via `children`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OutlineEntry {
    /// Declaration name.
    pub name: String,
    /// Declaration kind (Thm, Def, Class, Struct, Ns, Ex).
    pub kind: String,
    /// Start line (1-indexed).
    pub start_line: i64,
    /// End line (1-indexed).
    pub end_line: i64,
    /// Type signature if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_signature: Option<String>,
    /// Nested declarations.
    #[serde(default)]
    pub children: Vec<OutlineEntry>,
}

/// Token-efficient file skeleton.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FileOutline {
    /// Import statements.
    #[serde(default)]
    pub imports: Vec<String>,
    /// Top-level declarations.
    #[serde(default)]
    pub declarations: Vec<OutlineEntry>,
    /// Total count (set when truncated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_declarations: Option<i64>,
}

// ---------------------------------------------------------------------------
// Attempt
// ---------------------------------------------------------------------------

/// Result of a single tactic attempt.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AttemptResult {
    /// Code snippet that was tried.
    pub snippet: String,
    /// Goal list after applying snippet.
    #[serde(default)]
    pub goals: Vec<String>,
    /// Diagnostics for this attempt.
    #[serde(default)]
    pub diagnostics: Vec<DiagnosticMessage>,
}

// ---------------------------------------------------------------------------
// Build / Run
// ---------------------------------------------------------------------------

/// Result of a Lean project build.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BuildResult {
    /// Whether build succeeded.
    pub success: bool,
    /// Build output.
    pub output: String,
    /// Build errors if any.
    #[serde(default)]
    pub errors: Vec<String>,
}

/// Result of running a standalone Lean snippet.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RunResult {
    /// Whether code compiled successfully.
    pub success: bool,
    /// Compiler diagnostics.
    #[serde(default)]
    pub diagnostics: Vec<DiagnosticMessage>,
}

/// Information about a declaration's source file.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DeclarationInfo {
    /// Path to declaration file.
    pub file_path: String,
    /// File content.
    pub content: String,
}

// ---------------------------------------------------------------------------
// Wrappers
// ---------------------------------------------------------------------------

/// Helper: deserialise a missing bool field as `true`.
fn default_true() -> bool {
    true
}

/// Wrapper for diagnostic messages list with build status.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticsResult {
    /// True if the queried file/range has no errors.
    #[serde(default = "default_true")]
    pub success: bool,
    /// List of diagnostic messages.
    #[serde(default)]
    pub items: Vec<DiagnosticMessage>,
    /// File paths of dependencies that failed to build.
    #[serde(default)]
    pub failed_dependencies: Vec<String>,
}

/// Wrapper for completions list.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompletionsResult {
    /// List of completion items.
    #[serde(default)]
    pub items: Vec<CompletionItem>,
}

/// Wrapper for multi-attempt results list.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MultiAttemptResult {
    /// List of attempt results.
    #[serde(default)]
    pub items: Vec<AttemptResult>,
}

/// Wrapper for local search results list.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LocalSearchResults {
    /// List of local search results.
    #[serde(default)]
    pub items: Vec<LocalSearchResult>,
}

/// Wrapper for LeanSearch results list.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LeanSearchResults {
    /// List of LeanSearch results.
    #[serde(default)]
    pub items: Vec<LeanSearchResult>,
}

/// Wrapper for Loogle results list.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LoogleResults {
    /// List of Loogle results.
    #[serde(default)]
    pub items: Vec<LoogleResult>,
}

/// Wrapper for Lean Finder results list.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LeanFinderResults {
    /// List of Lean Finder results.
    #[serde(default)]
    pub items: Vec<LeanFinderResult>,
}

/// Wrapper for state search results list.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StateSearchResults {
    /// List of state search results.
    #[serde(default)]
    pub items: Vec<StateSearchResult>,
}

/// Wrapper for premise results list.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PremiseResults {
    /// List of premise results.
    #[serde(default)]
    pub items: Vec<PremiseResult>,
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

/// Wrapper for widget instances at a position.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WidgetsResult {
    /// Widget instances (id, name, range, props).
    #[serde(default)]
    pub widgets: Vec<serde_json::Value>,
}

/// Wrapper for interactive diagnostics with embedded widgets.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InteractiveDiagnosticsResult {
    /// Interactive diagnostic objects with TaggedText messages.
    #[serde(default)]
    pub diagnostics: Vec<serde_json::Value>,
}

/// Widget JavaScript source for a given hash.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WidgetSourceResult {
    /// Widget source data including JavaScript module.
    pub source: serde_json::Value,
}

// ---------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------

/// A single reference location.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReferenceLocation {
    /// Absolute file path.
    pub file_path: String,
    /// Line (1-indexed).
    pub line: i64,
    /// Column (1-indexed).
    pub column: i64,
    /// End line (1-indexed).
    pub end_line: i64,
    /// End column (1-indexed).
    pub end_column: i64,
}

/// Wrapper for find references results.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReferencesResult {
    /// List of reference locations.
    #[serde(default)]
    pub items: Vec<ReferenceLocation>,
}

// ---------------------------------------------------------------------------
// Profiling
// ---------------------------------------------------------------------------

/// Timing for a single source line.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LineProfile {
    /// Source line number (1-indexed).
    pub line: i64,
    /// Time in milliseconds.
    pub ms: f64,
    /// Source line content (truncated).
    pub text: String,
}

/// Profiling result for a theorem.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProofProfileResult {
    /// Total elaboration time in ms.
    pub ms: f64,
    /// Time per source line (>1% of total).
    #[serde(default)]
    pub lines: Vec<LineProfile>,
    /// Cumulative time by category in ms.
    #[serde(default)]
    pub categories: HashMap<String, f64>,
}

// ---------------------------------------------------------------------------
// Code Actions
// ---------------------------------------------------------------------------

/// A text edit produced by a code action.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CodeActionEdit {
    /// Replacement text.
    pub new_text: String,
    /// Start line (1-indexed).
    pub start_line: i64,
    /// Start column (1-indexed).
    pub start_column: i64,
    /// End line (1-indexed).
    pub end_line: i64,
    /// End column (1-indexed).
    pub end_column: i64,
}

/// A single available code action.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CodeAction {
    /// Code action title (e.g. "Try this: simp only [...]").
    pub title: String,
    /// Whether this is the preferred action.
    pub is_preferred: bool,
    /// Text edits to apply.
    #[serde(default)]
    pub edits: Vec<CodeActionEdit>,
}

/// Wrapper for code actions at a position.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CodeActionsResult {
    /// List of available code actions.
    #[serde(default)]
    pub actions: Vec<CodeAction>,
}

// ---------------------------------------------------------------------------
// Batch goals
// ---------------------------------------------------------------------------

/// A single position for a batch goal query.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BatchGoalPosition {
    /// Relative path to the Lean file.
    pub file_path: String,
    /// Line number (1-indexed).
    pub line: u32,
    /// Column number (1-indexed). If omitted, returns goals before/after.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

/// Result for a single position in a batch goal query.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BatchGoalEntry {
    /// The position that was queried.
    pub position: BatchGoalPosition,
    /// The goal state if the query succeeded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<GoalState>,
    /// Error message if the query failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Result of querying goals at multiple positions concurrently.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BatchGoalResult {
    /// Results for each queried position (same order as input).
    #[serde(default)]
    pub items: Vec<BatchGoalEntry>,
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

/// A suspicious source pattern detected during verification.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SourceWarning {
    /// Line number (1-indexed).
    pub line: i64,
    /// Matched pattern text.
    pub pattern: String,
}

/// Result of axiom verification for a declaration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct VerifyResult {
    /// Axioms used. Standard 3: propext, Classical.choice, Quot.sound.
    #[serde(default)]
    pub axioms: Vec<String>,
    /// Suspicious source patterns (if enabled).
    #[serde(default)]
    pub warnings: Vec<SourceWarning>,
}

// ---------------------------------------------------------------------------
// Proof diff
// ---------------------------------------------------------------------------

/// Result of comparing proof state at two positions.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GoalDiffResult {
    /// Goal conclusions that appeared after but not before.
    #[serde(default)]
    pub goals_added: Vec<String>,
    /// Goal conclusions that were before but not after.
    #[serde(default)]
    pub goals_removed: Vec<String>,
    /// Hypotheses that appeared after but not before.
    #[serde(default)]
    pub hypotheses_added: Vec<String>,
    /// Hypotheses that were before but not after.
    #[serde(default)]
    pub hypotheses_removed: Vec<String>,
    /// Whether anything changed between the two states.
    pub changed: bool,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    // -- Serialization round-trips --

    #[test]
    fn round_trip_goal_state_with_goals() {
        let gs = GoalState {
            line_context: "exact Nat.succ_pos n".into(),
            goals: Some(vec!["0 < Nat.succ n".into()]),
            goals_before: None,
            goals_after: None,
        };
        let json = serde_json::to_string(&gs).unwrap();
        let gs2: GoalState = serde_json::from_str(&json).unwrap();
        assert_eq!(gs.line_context, gs2.line_context);
        assert_eq!(gs.goals, gs2.goals);
        assert!(gs2.goals_before.is_none());
        assert!(gs2.goals_after.is_none());
    }

    #[test]
    fn round_trip_goal_state_with_before_after() {
        let gs = GoalState {
            line_context: "simp".into(),
            goals: None,
            goals_before: Some(vec!["a = b".into()]),
            goals_after: Some(vec![]),
        };
        let json = serde_json::to_string(&gs).unwrap();
        let gs2: GoalState = serde_json::from_str(&json).unwrap();
        assert!(gs2.goals.is_none());
        assert_eq!(gs2.goals_before, Some(vec!["a = b".into()]));
        assert_eq!(gs2.goals_after, Some(vec![]));
    }

    #[test]
    fn round_trip_diagnostics_result() {
        let dr = DiagnosticsResult {
            success: false,
            items: vec![DiagnosticMessage {
                severity: "error".into(),
                message: "unknown identifier 'foo'".into(),
                line: 10,
                column: 5,
            }],
            failed_dependencies: vec!["Mathlib.Tactic".into()],
        };
        let json = serde_json::to_string(&dr).unwrap();
        let dr2: DiagnosticsResult = serde_json::from_str(&json).unwrap();
        assert!(!dr2.success);
        assert_eq!(dr2.items.len(), 1);
        assert_eq!(dr2.failed_dependencies, vec!["Mathlib.Tactic"]);
    }

    #[test]
    fn round_trip_hover_info() {
        let hi = HoverInfo {
            symbol: "Nat.add".into(),
            info: "Nat -> Nat -> Nat".into(),
            diagnostics: vec![],
        };
        let json = serde_json::to_string(&hi).unwrap();
        let hi2: HoverInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(hi2.symbol, "Nat.add");
        assert!(hi2.diagnostics.is_empty());
    }

    #[test]
    fn round_trip_build_result() {
        let br = BuildResult {
            success: true,
            output: "Build complete".into(),
            errors: vec![],
        };
        let json = serde_json::to_string(&br).unwrap();
        let br2: BuildResult = serde_json::from_str(&json).unwrap();
        assert!(br2.success);
        assert!(br2.errors.is_empty());
    }

    #[test]
    fn round_trip_verify_result() {
        let vr = VerifyResult {
            axioms: vec![
                "propext".into(),
                "Classical.choice".into(),
                "Quot.sound".into(),
            ],
            warnings: vec![SourceWarning {
                line: 42,
                pattern: "sorry".into(),
            }],
        };
        let json = serde_json::to_string(&vr).unwrap();
        let vr2: VerifyResult = serde_json::from_str(&json).unwrap();
        assert_eq!(vr2.axioms.len(), 3);
        assert_eq!(vr2.warnings[0].pattern, "sorry");
    }

    #[test]
    fn round_trip_code_actions_result() {
        let car = CodeActionsResult {
            actions: vec![CodeAction {
                title: "Try this: simp only [Nat.add_comm]".into(),
                is_preferred: true,
                edits: vec![CodeActionEdit {
                    new_text: "simp only [Nat.add_comm]".into(),
                    start_line: 5,
                    start_column: 3,
                    end_line: 5,
                    end_column: 8,
                }],
            }],
        };
        let json = serde_json::to_string(&car).unwrap();
        let car2: CodeActionsResult = serde_json::from_str(&json).unwrap();
        assert_eq!(car2.actions.len(), 1);
        assert!(car2.actions[0].is_preferred);
        assert_eq!(car2.actions[0].edits.len(), 1);
    }

    // -- Optional field skipping --

    #[test]
    fn optional_fields_omitted_when_none() {
        let gs = GoalState {
            line_context: "intro h".into(),
            goals: None,
            goals_before: None,
            goals_after: None,
        };
        let v: Value = serde_json::to_value(&gs).unwrap();
        assert!(!v.as_object().unwrap().contains_key("goals"));
        assert!(!v.as_object().unwrap().contains_key("goals_before"));
        assert!(!v.as_object().unwrap().contains_key("goals_after"));
    }

    #[test]
    fn lean_search_type_field_renamed() {
        let r = LeanSearchResult {
            name: "Nat.add_comm".into(),
            module_name: "Init.Data.Nat.Basic".into(),
            kind: None,
            r#type: Some("forall (n m : Nat), n + m = m + n".into()),
        };
        let v: Value = serde_json::to_value(&r).unwrap();
        // Serialised key must be "type", not "r#type"
        assert!(v.as_object().unwrap().contains_key("type"));
        assert!(!v.as_object().unwrap().contains_key("r#type"));
        // "kind" should be absent because it is None
        assert!(!v.as_object().unwrap().contains_key("kind"));
    }

    // -- DiagnosticSeverity enum serialization --

    #[test]
    fn diagnostic_severity_lowercase_serialization() {
        assert_eq!(
            serde_json::to_value(DiagnosticSeverity::Error).unwrap(),
            json!("error")
        );
        assert_eq!(
            serde_json::to_value(DiagnosticSeverity::Warning).unwrap(),
            json!("warning")
        );
        assert_eq!(
            serde_json::to_value(DiagnosticSeverity::Info).unwrap(),
            json!("info")
        );
        assert_eq!(
            serde_json::to_value(DiagnosticSeverity::Hint).unwrap(),
            json!("hint")
        );
    }

    #[test]
    fn diagnostic_severity_round_trip() {
        let s: DiagnosticSeverity = serde_json::from_str("\"warning\"").unwrap();
        assert_eq!(s, DiagnosticSeverity::Warning);
    }

    // -- Default values --

    #[test]
    fn diagnostics_result_success_defaults_to_true() {
        // When "success" is omitted from JSON, it should default to true.
        let dr: DiagnosticsResult =
            serde_json::from_str(r#"{"items":[],"failed_dependencies":[]}"#).unwrap();
        assert!(dr.success);
    }

    #[test]
    fn diagnostics_result_explicit_false() {
        let dr: DiagnosticsResult =
            serde_json::from_str(r#"{"success":false,"items":[],"failed_dependencies":[]}"#)
                .unwrap();
        assert!(!dr.success);
    }

    // -- HashMap serialization for ProofProfileResult --

    #[test]
    fn proof_profile_result_categories_round_trip() {
        let mut categories = HashMap::new();
        categories.insert("elaboration".into(), 42.5);
        categories.insert("type_checking".into(), 13.2);
        let ppr = ProofProfileResult {
            ms: 55.7,
            lines: vec![LineProfile {
                line: 10,
                ms: 42.5,
                text: "  exact h".into(),
            }],
            categories,
        };
        let json = serde_json::to_string(&ppr).unwrap();
        let ppr2: ProofProfileResult = serde_json::from_str(&json).unwrap();
        assert!((ppr2.ms - 55.7).abs() < f64::EPSILON);
        assert_eq!(ppr2.lines.len(), 1);
        assert_eq!(ppr2.categories.len(), 2);
        assert!((ppr2.categories["elaboration"] - 42.5).abs() < f64::EPSILON);
    }

    // -- Empty collections --

    #[test]
    fn empty_vec_fields_deserialize_from_missing_key() {
        // All Vec fields with #[serde(default)] should become empty vec
        // when the key is missing from JSON.
        let hi: HoverInfo = serde_json::from_str(r#"{"symbol":"x","info":"Nat"}"#).unwrap();
        assert!(hi.diagnostics.is_empty());

        let fo: FileOutline = serde_json::from_str(r#"{}"#).unwrap();
        assert!(fo.imports.is_empty());
        assert!(fo.declarations.is_empty());
        assert!(fo.total_declarations.is_none());
    }

    #[test]
    fn empty_hashmap_default() {
        let ppr: ProofProfileResult = serde_json::from_str(r#"{"ms":0.0}"#).unwrap();
        assert!(ppr.categories.is_empty());
        assert!(ppr.lines.is_empty());
    }

    // -- Send + Sync static assertions --

    #[test]
    fn key_types_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<DiagnosticSeverity>();
        assert_send_sync::<DiagnosticMessage>();
        assert_send_sync::<GoalState>();
        assert_send_sync::<CompletionItem>();
        assert_send_sync::<HoverInfo>();
        assert_send_sync::<TermGoalState>();
        assert_send_sync::<OutlineEntry>();
        assert_send_sync::<FileOutline>();
        assert_send_sync::<AttemptResult>();
        assert_send_sync::<BuildResult>();
        assert_send_sync::<RunResult>();
        assert_send_sync::<DeclarationInfo>();
        assert_send_sync::<DiagnosticsResult>();
        assert_send_sync::<CompletionsResult>();
        assert_send_sync::<MultiAttemptResult>();
        assert_send_sync::<LocalSearchResults>();
        assert_send_sync::<LeanSearchResults>();
        assert_send_sync::<LoogleResults>();
        assert_send_sync::<LeanFinderResults>();
        assert_send_sync::<StateSearchResults>();
        assert_send_sync::<PremiseResults>();
        assert_send_sync::<WidgetsResult>();
        assert_send_sync::<InteractiveDiagnosticsResult>();
        assert_send_sync::<WidgetSourceResult>();
        assert_send_sync::<ReferenceLocation>();
        assert_send_sync::<ReferencesResult>();
        assert_send_sync::<LineProfile>();
        assert_send_sync::<ProofProfileResult>();
        assert_send_sync::<CodeActionEdit>();
        assert_send_sync::<CodeAction>();
        assert_send_sync::<CodeActionsResult>();
        assert_send_sync::<SourceWarning>();
        assert_send_sync::<VerifyResult>();
        assert_send_sync::<BatchGoalPosition>();
        assert_send_sync::<BatchGoalEntry>();
        assert_send_sync::<BatchGoalResult>();
        assert_send_sync::<GoalDiffResult>();
    }

    // -- Widget types with serde_json::Value --

    #[test]
    fn widgets_result_round_trip() {
        let wr = WidgetsResult {
            widgets: vec![json!({"id": "w1", "name": "InfoView"})],
        };
        let json = serde_json::to_string(&wr).unwrap();
        let wr2: WidgetsResult = serde_json::from_str(&json).unwrap();
        assert_eq!(wr2.widgets.len(), 1);
        assert_eq!(wr2.widgets[0]["id"], "w1");
    }

    // -- Recursive OutlineEntry --

    #[test]
    fn outline_entry_with_children() {
        let entry = OutlineEntry {
            name: "MyNamespace".into(),
            kind: "Ns".into(),
            start_line: 1,
            end_line: 50,
            type_signature: None,
            children: vec![OutlineEntry {
                name: "myTheorem".into(),
                kind: "Thm".into(),
                start_line: 5,
                end_line: 10,
                type_signature: Some("Nat -> Nat".into()),
                children: vec![],
            }],
        };
        let json = serde_json::to_string(&entry).unwrap();
        let entry2: OutlineEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry2.children.len(), 1);
        assert_eq!(entry2.children[0].name, "myTheorem");
        assert_eq!(entry2.children[0].type_signature, Some("Nat -> Nat".into()));
    }

    // -- BatchGoal types --

    #[test]
    fn round_trip_batch_goal_position() {
        let pos = BatchGoalPosition {
            file_path: "Main.lean".into(),
            line: 5,
            column: Some(3),
        };
        let json = serde_json::to_string(&pos).unwrap();
        let pos2: BatchGoalPosition = serde_json::from_str(&json).unwrap();
        assert_eq!(pos2.file_path, "Main.lean");
        assert_eq!(pos2.line, 5);
        assert_eq!(pos2.column, Some(3));
    }

    #[test]
    fn batch_goal_position_omits_none_column() {
        let pos = BatchGoalPosition {
            file_path: "Main.lean".into(),
            line: 1,
            column: None,
        };
        let v: Value = serde_json::to_value(&pos).unwrap();
        assert!(!v.as_object().unwrap().contains_key("column"));
    }

    #[test]
    fn round_trip_batch_goal_entry_success() {
        let entry = BatchGoalEntry {
            position: BatchGoalPosition {
                file_path: "Main.lean".into(),
                line: 2,
                column: Some(3),
            },
            result: Some(GoalState {
                line_context: "  exact h".into(),
                goals: Some(vec!["⊢ True".into()]),
                goals_before: None,
                goals_after: None,
            }),
            error: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let entry2: BatchGoalEntry = serde_json::from_str(&json).unwrap();
        assert!(entry2.result.is_some());
        assert!(entry2.error.is_none());
    }

    #[test]
    fn round_trip_batch_goal_entry_error() {
        let entry = BatchGoalEntry {
            position: BatchGoalPosition {
                file_path: "Bad.lean".into(),
                line: 99,
                column: Some(1),
            },
            result: None,
            error: Some("Line 99 out of range".into()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let entry2: BatchGoalEntry = serde_json::from_str(&json).unwrap();
        assert!(entry2.result.is_none());
        assert_eq!(entry2.error.as_deref(), Some("Line 99 out of range"));
    }

    #[test]
    fn round_trip_batch_goal_result() {
        let result = BatchGoalResult {
            items: vec![BatchGoalEntry {
                position: BatchGoalPosition {
                    file_path: "Main.lean".into(),
                    line: 1,
                    column: Some(1),
                },
                result: Some(GoalState {
                    line_context: "exact h".into(),
                    goals: Some(vec![]),
                    goals_before: None,
                    goals_after: None,
                }),
                error: None,
            }],
        };
        let json = serde_json::to_string(&result).unwrap();
        let result2: BatchGoalResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result2.items.len(), 1);
    }

    // -- GoalDiffResult --

    #[test]
    fn round_trip_goal_diff_result() {
        let gdr = GoalDiffResult {
            goals_added: vec!["P".into()],
            goals_removed: vec!["P -> P".into()],
            hypotheses_added: vec!["h : P".into()],
            hypotheses_removed: vec![],
            changed: true,
        };
        let json = serde_json::to_string(&gdr).unwrap();
        let gdr2: GoalDiffResult = serde_json::from_str(&json).unwrap();
        assert!(gdr2.changed);
        assert_eq!(gdr2.goals_added, vec!["P"]);
        assert_eq!(gdr2.goals_removed, vec!["P -> P"]);
        assert_eq!(gdr2.hypotheses_added, vec!["h : P"]);
        assert!(gdr2.hypotheses_removed.is_empty());
    }

    #[test]
    fn goal_diff_result_defaults_from_minimal_json() {
        let gdr: GoalDiffResult = serde_json::from_str(r#"{"changed":false}"#).unwrap();
        assert!(!gdr.changed);
        assert!(gdr.goals_added.is_empty());
        assert!(gdr.goals_removed.is_empty());
        assert!(gdr.hypotheses_added.is_empty());
        assert!(gdr.hypotheses_removed.is_empty());
    }
}
