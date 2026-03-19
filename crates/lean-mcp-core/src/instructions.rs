//! Server instructions sent to MCP clients.
//!
//! The [`INSTRUCTIONS`] constant is included verbatim in the MCP server
//! capabilities so that connected LLM agents know how to use the available
//! Lean tools effectively.

/// Server instructions sent to MCP clients.
///
/// Covers general rules, key tools, search tools, the search decision tree,
/// return formats, and error handling guidance.
pub const INSTRUCTIONS: &str = "\
## General Rules\n\
- All line and column numbers are 1-indexed.\n\
- This MCP does NOT edit files. Use other tools for editing.\n\
\n\
## Key Tools\n\
- **lean_goal**: Proof state at position. Omit `column` for before/after. \"no goals\" = done!\n\
- **lean_diagnostic_messages**: Compiler errors/warnings. \"no goals to be solved\" = remove tactics.\n\
- **lean_hover_info**: Type signature + docs. Column at START of identifier.\n\
- **lean_completions**: IDE autocomplete on incomplete code.\n\
- **lean_local_search**: Fast local declaration search. Use BEFORE trying a lemma name.\n\
- **lean_file_outline**: Token-efficient file skeleton (slow-ish).\n\
- **lean_multi_attempt**: Test tactics without editing at a proof position. Use `column` for an \
exact source position; omit it for fast line-based REPL attempts: `[\"simp\", \"ring\", \"omega\"]`\n\
- **lean_declaration_file**: Get declaration source. Use sparingly (large output).\n\
- **lean_run_code**: Run standalone snippet. Use rarely.\n\
- **lean_verify**: Axiom check + source scan. Use fully qualified name (e.g. `Ns.thm`).\n\
- **lean_build**: Rebuild + restart LSP. Only if needed (new imports). SLOW!\n\
- **lean_profile_proof**: Profile a theorem for performance. Shows tactic hotspots. SLOW!\n\
\n\
## Search Tools (rate limited)\n\
- **lean_leansearch** (3/30s): Natural language -> mathlib\n\
- **lean_loogle** (3/30s): Type pattern -> mathlib\n\
- **lean_leanfinder** (10/30s): Semantic/conceptual search\n\
- **lean_state_search** (3/30s): Goal -> closing lemmas\n\
- **lean_hammer_premise** (3/30s): Goal -> premises for simp/aesop\n\
\n\
## Search Decision Tree\n\
1. \"Does X exist locally?\" -> lean_local_search\n\
2. \"I need a lemma that says X\" -> lean_leansearch\n\
3. \"Find lemma with type pattern\" -> lean_loogle\n\
4. \"What's the Lean name for concept X?\" -> lean_leanfinder\n\
5. \"What closes this goal?\" -> lean_state_search\n\
6. \"What to feed simp?\" -> lean_hammer_premise\n\
\n\
After finding a name: lean_local_search to verify, lean_hover_info for signature.\n\
\n\
## Return Formats\n\
List tools return JSON arrays. Empty = `[]`.\n\
\n\
## Error Handling\n\
Check `isError` in responses: `true` means failure (timeout/LSP error), while `[]` with \
`isError: false` means no results found.\n";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_is_not_empty() {
        assert!(!INSTRUCTIONS.is_empty());
    }

    #[test]
    fn instructions_contains_key_sections() {
        assert!(INSTRUCTIONS.contains("## General Rules"));
        assert!(INSTRUCTIONS.contains("## Key Tools"));
        assert!(INSTRUCTIONS.contains("## Search Tools"));
        assert!(INSTRUCTIONS.contains("## Search Decision Tree"));
        assert!(INSTRUCTIONS.contains("## Return Formats"));
        assert!(INSTRUCTIONS.contains("## Error Handling"));
    }

    #[test]
    fn instructions_mentions_all_key_tools() {
        let tools = [
            "lean_goal",
            "lean_diagnostic_messages",
            "lean_hover_info",
            "lean_completions",
            "lean_local_search",
            "lean_file_outline",
            "lean_multi_attempt",
            "lean_declaration_file",
            "lean_run_code",
            "lean_verify",
            "lean_build",
            "lean_profile_proof",
        ];
        for tool in &tools {
            assert!(
                INSTRUCTIONS.contains(tool),
                "INSTRUCTIONS should mention {tool}"
            );
        }
    }

    #[test]
    fn instructions_mentions_all_search_tools() {
        let search_tools = [
            "lean_leansearch",
            "lean_loogle",
            "lean_leanfinder",
            "lean_state_search",
            "lean_hammer_premise",
        ];
        for tool in &search_tools {
            assert!(
                INSTRUCTIONS.contains(tool),
                "INSTRUCTIONS should mention {tool}"
            );
        }
    }
}
