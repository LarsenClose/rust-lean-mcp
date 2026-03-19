//! Proof state diff logic.
//!
//! Compares two goal states (before/after a tactic) and reports what changed:
//! goals added/removed and hypotheses added/removed.

use std::collections::HashSet;

/// The result of comparing two proof states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalDiff {
    /// Goal conclusions that appeared in after but not before.
    pub goals_added: Vec<String>,
    /// Goal conclusions that were in before but not after.
    pub goals_removed: Vec<String>,
    /// Hypotheses that appeared in after but not before.
    pub hypotheses_added: Vec<String>,
    /// Hypotheses that were in before but not after.
    pub hypotheses_removed: Vec<String>,
    /// Whether anything changed between the two states.
    pub changed: bool,
}

/// A parsed goal: hypotheses above the turnstile and a conclusion below it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ParsedGoal {
    hypotheses: Vec<String>,
    conclusion: String,
}

/// Parse a single goal string into hypotheses and conclusion.
///
/// A Lean goal looks like:
/// ```text
/// h₁ : P
/// h₂ : Q
/// ⊢ R
/// ```
///
/// Everything before the `⊢` line is hypotheses; the `⊢` line (without the
/// turnstile prefix) is the conclusion. If there is no `⊢`, the entire string
/// is treated as the conclusion.
fn parse_goal(goal: &str) -> ParsedGoal {
    let mut pre_turnstile = Vec::new();
    let mut conclusion_parts = Vec::new();
    let mut found_turnstile = false;

    for line in goal.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('⊢') {
            found_turnstile = true;
            conclusion_parts.push(rest.trim().to_string());
        } else if found_turnstile {
            // Lines after ⊢ are part of a multi-line conclusion
            conclusion_parts.push(trimmed.to_string());
        } else if !trimmed.is_empty() {
            pre_turnstile.push(trimmed.to_string());
        }
    }

    if found_turnstile {
        ParsedGoal {
            hypotheses: pre_turnstile,
            conclusion: conclusion_parts.join("\n"),
        }
    } else {
        // No turnstile found — everything is the conclusion, no hypotheses
        ParsedGoal {
            hypotheses: Vec::new(),
            conclusion: goal.trim().to_string(),
        }
    }
}

/// Compare two goal states and return what changed.
///
/// Each input is a slice of goal strings (as returned by `extract_goals_list`).
/// The diff computes:
/// - Goals whose conclusions were added or removed
/// - Hypotheses (across all goals) that were added or removed
pub fn diff_goals(before: &[String], after: &[String]) -> GoalDiff {
    let before_parsed: Vec<ParsedGoal> = before.iter().map(|g| parse_goal(g)).collect();
    let after_parsed: Vec<ParsedGoal> = after.iter().map(|g| parse_goal(g)).collect();

    // Compare conclusions
    let before_conclusions: HashSet<&str> = before_parsed
        .iter()
        .map(|g| g.conclusion.as_str())
        .collect();
    let after_conclusions: HashSet<&str> =
        after_parsed.iter().map(|g| g.conclusion.as_str()).collect();

    let goals_added: Vec<String> = after_conclusions
        .difference(&before_conclusions)
        .map(|s| s.to_string())
        .collect();
    let goals_removed: Vec<String> = before_conclusions
        .difference(&after_conclusions)
        .map(|s| s.to_string())
        .collect();

    // Compare hypotheses across all goals
    let before_hyps: HashSet<&str> = before_parsed
        .iter()
        .flat_map(|g| g.hypotheses.iter().map(String::as_str))
        .collect();
    let after_hyps: HashSet<&str> = after_parsed
        .iter()
        .flat_map(|g| g.hypotheses.iter().map(String::as_str))
        .collect();

    let hypotheses_added: Vec<String> = after_hyps
        .difference(&before_hyps)
        .map(|s| s.to_string())
        .collect();
    let hypotheses_removed: Vec<String> = before_hyps
        .difference(&after_hyps)
        .map(|s| s.to_string())
        .collect();

    let changed = !goals_added.is_empty()
        || !goals_removed.is_empty()
        || !hypotheses_added.is_empty()
        || !hypotheses_removed.is_empty();

    GoalDiff {
        goals_added,
        goals_removed,
        hypotheses_added,
        hypotheses_removed,
        changed,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_goal ----

    #[test]
    fn parse_goal_standard_format() {
        let goal = "h : Nat\n⊢ h = h";
        let parsed = parse_goal(goal);
        assert_eq!(parsed.hypotheses, vec!["h : Nat"]);
        assert_eq!(parsed.conclusion, "h = h");
    }

    #[test]
    fn parse_goal_multiple_hypotheses() {
        let goal = "a : Nat\nb : Nat\nh : a = b\n⊢ b = a";
        let parsed = parse_goal(goal);
        assert_eq!(parsed.hypotheses, vec!["a : Nat", "b : Nat", "h : a = b"]);
        assert_eq!(parsed.conclusion, "b = a");
    }

    #[test]
    fn parse_goal_no_turnstile() {
        let goal = "True";
        let parsed = parse_goal(goal);
        assert!(parsed.hypotheses.is_empty());
        assert_eq!(parsed.conclusion, "True");
    }

    #[test]
    fn parse_goal_empty_string() {
        let parsed = parse_goal("");
        assert!(parsed.hypotheses.is_empty());
        assert_eq!(parsed.conclusion, "");
    }

    #[test]
    fn parse_goal_only_turnstile() {
        let goal = "⊢ True";
        let parsed = parse_goal(goal);
        assert!(parsed.hypotheses.is_empty());
        assert_eq!(parsed.conclusion, "True");
    }

    #[test]
    fn parse_goal_multiline_conclusion() {
        let goal = "h : P\n⊢ ∀ x,\n  f x = g x";
        let parsed = parse_goal(goal);
        assert_eq!(parsed.hypotheses, vec!["h : P"]);
        assert_eq!(parsed.conclusion, "∀ x,\nf x = g x");
    }

    // ---- diff_goals ----

    #[test]
    fn diff_no_change() {
        let before = vec!["h : Nat\n⊢ h = h".to_string()];
        let after = vec!["h : Nat\n⊢ h = h".to_string()];
        let diff = diff_goals(&before, &after);
        assert!(!diff.changed);
        assert!(diff.goals_added.is_empty());
        assert!(diff.goals_removed.is_empty());
        assert!(diff.hypotheses_added.is_empty());
        assert!(diff.hypotheses_removed.is_empty());
    }

    #[test]
    fn diff_goal_solved() {
        let before = vec!["h : Nat\n⊢ h = h".to_string()];
        let after: Vec<String> = vec![];
        let diff = diff_goals(&before, &after);
        assert!(diff.changed);
        assert_eq!(diff.goals_removed, vec!["h = h"]);
        assert!(diff.goals_added.is_empty());
        assert_eq!(diff.hypotheses_removed, vec!["h : Nat"]);
    }

    #[test]
    fn diff_hypothesis_added_by_intro() {
        let before = vec!["⊢ ∀ (n : Nat), n = n".to_string()];
        let after = vec!["n : Nat\n⊢ n = n".to_string()];
        let diff = diff_goals(&before, &after);
        assert!(diff.changed);
        assert_eq!(diff.hypotheses_added, vec!["n : Nat"]);
        assert_eq!(diff.goals_added, vec!["n = n"]);
        assert_eq!(diff.goals_removed, vec!["∀ (n : Nat), n = n"]);
    }

    #[test]
    fn diff_goal_split_into_two() {
        let before = vec!["⊢ P ∧ Q".to_string()];
        let after = vec!["⊢ P".to_string(), "⊢ Q".to_string()];
        let diff = diff_goals(&before, &after);
        assert!(diff.changed);
        assert_eq!(diff.goals_removed.len(), 1);
        assert!(diff.goals_removed.contains(&"P ∧ Q".to_string()));
        assert_eq!(diff.goals_added.len(), 2);
        assert!(diff.goals_added.contains(&"P".to_string()));
        assert!(diff.goals_added.contains(&"Q".to_string()));
    }

    #[test]
    fn diff_both_empty() {
        let diff = diff_goals(&[], &[]);
        assert!(!diff.changed);
    }

    #[test]
    fn diff_new_goal_from_empty() {
        let after = vec!["⊢ True".to_string()];
        let diff = diff_goals(&[], &after);
        assert!(diff.changed);
        assert_eq!(diff.goals_added, vec!["True"]);
    }

    #[test]
    fn diff_hypothesis_removed() {
        let before = vec!["h1 : P\nh2 : Q\n⊢ R".to_string()];
        let after = vec!["h1 : P\n⊢ R".to_string()];
        let diff = diff_goals(&before, &after);
        assert!(diff.changed);
        // Conclusion unchanged
        assert!(diff.goals_added.is_empty());
        assert!(diff.goals_removed.is_empty());
        // h2 was removed
        assert_eq!(diff.hypotheses_removed, vec!["h2 : Q"]);
        assert!(diff.hypotheses_added.is_empty());
    }

    #[test]
    fn diff_multiple_goals_one_solved() {
        let before = vec!["⊢ P".to_string(), "⊢ Q".to_string()];
        let after = vec!["⊢ Q".to_string()];
        let diff = diff_goals(&before, &after);
        assert!(diff.changed);
        assert_eq!(diff.goals_removed, vec!["P"]);
        assert!(diff.goals_added.is_empty());
    }
}
