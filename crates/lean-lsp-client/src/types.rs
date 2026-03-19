//! Lean-specific LSP extension types beyond what `lsp-types` provides.

use serde::{Deserialize, Serialize};

/// Response from `$/lean/plainGoal` request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlainGoalResponse {
    /// List of goal strings (e.g., `["h : n < m\n⊢ n + 1 < m + 1"]`).
    #[serde(default)]
    pub goals: Vec<String>,
}

/// Response from `$/lean/plainTermGoal` request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlainTermGoalResponse {
    /// Expected type, may contain markdown fences.
    pub goal: Option<String>,
}

/// A content change for `textDocument/didChange`, matching the LSP
/// `TextDocumentContentChangeEvent` shape. Uses `(line, character)` pairs
/// for the optional range.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContentChange {
    /// New text to insert.
    pub text: String,
    /// Range to replace. If `None`, the entire document is replaced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<LspRange>,
}

/// LSP Range with start and end positions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

/// LSP Position (0-indexed line and character).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

impl ContentChange {
    /// Create a range replacement change.
    /// `start` and `end` are `[line, character]` pairs, 0-indexed.
    pub fn new(text: &str, start: [u32; 2], end: [u32; 2]) -> Self {
        Self {
            text: text.to_string(),
            range: Some(LspRange {
                start: LspPosition {
                    line: start[0],
                    character: start[1],
                },
                end: LspPosition {
                    line: end[0],
                    character: end[1],
                },
            }),
        }
    }

    /// Create a full-document replacement.
    pub fn full(text: &str) -> Self {
        Self {
            text: text.to_string(),
            range: None,
        }
    }
}

/// Parameters for Lean's widget RPC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WidgetRequest {
    #[serde(rename = "textDocument")]
    pub text_document: TextDocumentIdentifier,
    pub position: LspPosition,
}

/// Simplified `TextDocumentIdentifier`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TextDocumentIdentifier {
    pub uri: String,
}

/// Widget source request parameter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WidgetSourceParams {
    pub position: LspPosition,
    #[serde(rename = "textDocument")]
    pub text_document: TextDocumentIdentifier,
    #[serde(rename = "javascriptHash")]
    pub javascript_hash: String,
}

/// LSP Diagnostic severity values.
pub mod severity {
    /// Error severity.
    pub const ERROR: i32 = 1;
    /// Warning severity.
    pub const WARNING: i32 = 2;
    /// Information severity.
    pub const INFO: i32 = 3;
    /// Hint severity.
    pub const HINT: i32 = 4;

    /// Return a human-readable name for the given severity value.
    pub fn name(severity: i32) -> &'static str {
        match severity {
            ERROR => "error",
            WARNING => "warning",
            INFO => "info",
            HINT => "hint",
            _ => "unknown",
        }
    }
}

/// Lean-specific LSP method names.
pub mod methods {
    /// Request the plain-text proof goal at a position.
    pub const PLAIN_GOAL: &str = "$/lean/plainGoal";
    /// Request the plain-text term goal at a position.
    pub const PLAIN_TERM_GOAL: &str = "$/lean/plainTermGoal";
    /// Request interactive diagnostics.
    pub const INTERACTIVE_DIAGNOSTICS: &str = "$/lean/interactiveDiagnostics";
    /// Connect to the Lean RPC server.
    pub const RPC_CONNECT: &str = "$/lean/rpc/connect";
    /// Invoke a Lean RPC method.
    pub const RPC_CALL: &str = "$/lean/rpc/call";
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    // ── ContentChange ──────────────────────────────────────────────

    #[test]
    fn content_change_new_creates_correct_range() {
        let change = ContentChange::new("hello", [1, 0], [1, 5]);
        assert_eq!(change.text, "hello");
        let range = change.range.unwrap();
        assert_eq!(range.start.line, 1);
        assert_eq!(range.start.character, 0);
        assert_eq!(range.end.line, 1);
        assert_eq!(range.end.character, 5);
    }

    #[test]
    fn content_change_full_has_no_range() {
        let change = ContentChange::full("-- entire document");
        assert_eq!(change.text, "-- entire document");
        assert!(change.range.is_none());
    }

    #[test]
    fn content_change_serialization_omits_none_range() {
        let change = ContentChange::full("abc");
        let serialized = serde_json::to_string(&change).unwrap();
        assert!(!serialized.contains("range"));
    }

    #[test]
    fn content_change_roundtrip() {
        let original = ContentChange::new("x", [0, 0], [0, 1]);
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: ContentChange = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    // ── PlainGoalResponse ──────────────────────────────────────────

    #[test]
    fn plain_goal_response_empty_goals() {
        let resp: PlainGoalResponse = serde_json::from_value(json!({})).unwrap();
        assert!(resp.goals.is_empty());
    }

    #[test]
    fn plain_goal_response_multiple_goals() {
        let resp: PlainGoalResponse =
            serde_json::from_value(json!({"goals": ["goal1", "goal2", "goal3"]})).unwrap();
        assert_eq!(resp.goals, vec!["goal1", "goal2", "goal3"]);
    }

    #[test]
    fn plain_goal_response_roundtrip() {
        let original = PlainGoalResponse {
            goals: vec!["h : n < m\n⊢ n + 1 < m + 1".to_string()],
        };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: PlainGoalResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    // ── PlainTermGoalResponse ──────────────────────────────────────

    #[test]
    fn plain_term_goal_response_with_goal() {
        let resp: PlainTermGoalResponse =
            serde_json::from_value(json!({"goal": "Nat -> Nat"})).unwrap();
        assert_eq!(resp.goal, Some("Nat -> Nat".to_string()));
    }

    #[test]
    fn plain_term_goal_response_without_goal() {
        let resp: PlainTermGoalResponse = serde_json::from_value(json!({"goal": null})).unwrap();
        assert_eq!(resp.goal, None);
    }

    // ── LspPosition / LspRange ─────────────────────────────────────

    #[test]
    fn lsp_position_roundtrip() {
        let pos = LspPosition {
            line: 42,
            character: 7,
        };
        let json = serde_json::to_string(&pos).unwrap();
        let deserialized: LspPosition = serde_json::from_str(&json).unwrap();
        assert_eq!(pos, deserialized);
    }

    #[test]
    fn lsp_range_roundtrip() {
        let range = LspRange {
            start: LspPosition {
                line: 0,
                character: 0,
            },
            end: LspPosition {
                line: 10,
                character: 20,
            },
        };
        let json = serde_json::to_string(&range).unwrap();
        let deserialized: LspRange = serde_json::from_str(&json).unwrap();
        assert_eq!(range, deserialized);
    }

    // ── severity ───────────────────────────────────────────────────

    #[test]
    fn severity_name_for_all_values() {
        assert_eq!(severity::name(severity::ERROR), "error");
        assert_eq!(severity::name(severity::WARNING), "warning");
        assert_eq!(severity::name(severity::INFO), "info");
        assert_eq!(severity::name(severity::HINT), "hint");
        assert_eq!(severity::name(99), "unknown");
    }

    // ── methods ────────────────────────────────────────────────────

    #[test]
    fn methods_constants_are_correct() {
        assert_eq!(methods::PLAIN_GOAL, "$/lean/plainGoal");
        assert_eq!(methods::PLAIN_TERM_GOAL, "$/lean/plainTermGoal");
        assert_eq!(
            methods::INTERACTIVE_DIAGNOSTICS,
            "$/lean/interactiveDiagnostics"
        );
        assert_eq!(methods::RPC_CONNECT, "$/lean/rpc/connect");
        assert_eq!(methods::RPC_CALL, "$/lean/rpc/call");
    }

    // ── WidgetSourceParams ─────────────────────────────────────────

    #[test]
    fn widget_source_params_serialization() {
        let params = WidgetSourceParams {
            position: LspPosition {
                line: 5,
                character: 10,
            },
            text_document: TextDocumentIdentifier {
                uri: "file:///tmp/test.lean".to_string(),
            },
            javascript_hash: "abc123".to_string(),
        };
        let value = serde_json::to_value(&params).unwrap();
        assert_eq!(value["position"]["line"], 5);
        assert_eq!(value["position"]["character"], 10);
        assert_eq!(value["textDocument"]["uri"], "file:///tmp/test.lean");
        assert_eq!(value["javascriptHash"], "abc123");
    }

    #[test]
    fn widget_source_params_roundtrip() {
        let original = WidgetSourceParams {
            position: LspPosition {
                line: 0,
                character: 0,
            },
            text_document: TextDocumentIdentifier {
                uri: "file:///a.lean".to_string(),
            },
            javascript_hash: "hash".to_string(),
        };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: WidgetSourceParams = serde_json::from_str(&json).unwrap();
        assert_eq!(original, deserialized);
    }

    // ── Send + Sync ────────────────────────────────────────────────

    #[test]
    fn types_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PlainGoalResponse>();
        assert_send_sync::<PlainTermGoalResponse>();
        assert_send_sync::<ContentChange>();
        assert_send_sync::<LspRange>();
        assert_send_sync::<LspPosition>();
        assert_send_sync::<WidgetRequest>();
        assert_send_sync::<TextDocumentIdentifier>();
        assert_send_sync::<WidgetSourceParams>();
    }
}
