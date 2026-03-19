//! General-purpose utility functions.
//!
//! Provides helpers for LSP completion kinds, goal extraction, and
//! source-text searching.

use serde_json::Value;

/// Map an LSP `CompletionItemKind` integer to its human-readable name.
///
/// See <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#completionItemKind>.
pub fn completion_kind_name(kind: i32) -> Option<&'static str> {
    match kind {
        1 => Some("Text"),
        2 => Some("Method"),
        3 => Some("Function"),
        4 => Some("Constructor"),
        5 => Some("Field"),
        6 => Some("Variable"),
        7 => Some("Class"),
        8 => Some("Interface"),
        9 => Some("Module"),
        10 => Some("Property"),
        11 => Some("Unit"),
        12 => Some("Value"),
        13 => Some("Enum"),
        14 => Some("Keyword"),
        15 => Some("Snippet"),
        16 => Some("Color"),
        17 => Some("File"),
        18 => Some("Reference"),
        19 => Some("Folder"),
        20 => Some("EnumMember"),
        21 => Some("Constant"),
        22 => Some("Struct"),
        23 => Some("Event"),
        24 => Some("Operator"),
        25 => Some("TypeParameter"),
        _ => None,
    }
}

/// Extract the list of goal strings from an LSP plainTermGoal / plainGoal response.
///
/// Expects the response to contain a `"goals"` key with an array of strings.
/// Returns an empty `Vec` when the response is `None`, missing the key,
/// or the key is not an array of strings.
pub fn extract_goals_list(goal_response: Option<&Value>) -> Vec<String> {
    let Some(resp) = goal_response else {
        return Vec::new();
    };
    let Some(goals) = resp.get("goals") else {
        return Vec::new();
    };
    let Some(arr) = goals.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect()
}

/// Find the first occurrence of `query` in `content`.
///
/// Returns a 0-indexed `(line, column)` tuple, or `None` if not found.
pub fn find_start_position(content: &str, query: &str) -> Option<(usize, usize)> {
    let byte_offset = content.find(query)?;
    let before = &content[..byte_offset];
    let line = before.matches('\n').count();
    let col = match before.rfind('\n') {
        Some(nl) => byte_offset - nl - 1,
        None => byte_offset,
    };
    Some((line, col))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- completion_kind_name ----

    #[test]
    fn completion_kind_known_values() {
        assert_eq!(completion_kind_name(1), Some("Text"));
        assert_eq!(completion_kind_name(2), Some("Method"));
        assert_eq!(completion_kind_name(3), Some("Function"));
        assert_eq!(completion_kind_name(6), Some("Variable"));
        assert_eq!(completion_kind_name(7), Some("Class"));
        assert_eq!(completion_kind_name(14), Some("Keyword"));
        assert_eq!(completion_kind_name(21), Some("Constant"));
        assert_eq!(completion_kind_name(22), Some("Struct"));
        assert_eq!(completion_kind_name(25), Some("TypeParameter"));
    }

    #[test]
    fn completion_kind_unknown() {
        assert_eq!(completion_kind_name(0), None);
        assert_eq!(completion_kind_name(26), None);
        assert_eq!(completion_kind_name(-1), None);
        assert_eq!(completion_kind_name(999), None);
    }

    // ---- extract_goals_list ----

    #[test]
    fn extract_goals_with_goals() {
        let v = json!({"goals": ["a = b", "c = d"]});
        let goals = extract_goals_list(Some(&v));
        assert_eq!(goals, vec!["a = b", "c = d"]);
    }

    #[test]
    fn extract_goals_empty_array() {
        let v = json!({"goals": []});
        let goals = extract_goals_list(Some(&v));
        assert!(goals.is_empty());
    }

    #[test]
    fn extract_goals_none_response() {
        let goals = extract_goals_list(None);
        assert!(goals.is_empty());
    }

    #[test]
    fn extract_goals_missing_key() {
        let v = json!({"result": "something"});
        let goals = extract_goals_list(Some(&v));
        assert!(goals.is_empty());
    }

    #[test]
    fn extract_goals_non_array() {
        let v = json!({"goals": "not an array"});
        let goals = extract_goals_list(Some(&v));
        assert!(goals.is_empty());
    }

    #[test]
    fn extract_goals_filters_non_strings() {
        let v = json!({"goals": ["ok", 42, null, "also ok"]});
        let goals = extract_goals_list(Some(&v));
        assert_eq!(goals, vec!["ok", "also ok"]);
    }

    // ---- find_start_position ----

    #[test]
    fn find_position_single_line() {
        let content = "def foo := 42";
        let pos = find_start_position(content, "foo");
        assert_eq!(pos, Some((0, 4)));
    }

    #[test]
    fn find_position_multi_line() {
        let content = "line0\nline1\ndef foo := 42";
        let pos = find_start_position(content, "foo");
        assert_eq!(pos, Some((2, 4)));
    }

    #[test]
    fn find_position_at_start() {
        let content = "hello world";
        let pos = find_start_position(content, "hello");
        assert_eq!(pos, Some((0, 0)));
    }

    #[test]
    fn find_position_not_found() {
        let content = "def bar := 0";
        let pos = find_start_position(content, "foo");
        assert!(pos.is_none());
    }

    #[test]
    fn find_position_empty_content() {
        let pos = find_start_position("", "foo");
        assert!(pos.is_none());
    }

    #[test]
    fn find_position_at_line_boundary() {
        let content = "aaa\nbbb\nccc";
        let pos = find_start_position(content, "bbb");
        assert_eq!(pos, Some((1, 0)));
    }
}
