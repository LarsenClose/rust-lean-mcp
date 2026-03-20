//! Local project search using ripgrep.
//!
//! Ports the Python `search_utils.py` (358 lines). Searches Lean declaration
//! keywords (`theorem`, `lemma`, `def`, `class`, etc.) in `.lean` files using
//! ripgrep's JSON output, then resolves enclosing namespaces and ranks results
//! by relevance.
//!
//! Uses `std::process::Command` (blocking) since ripgrep is inherently
//! synchronous. Wrap calls in `tokio::task::spawn_blocking` when used from
//! async contexts.

use std::path::Path;

use regex::Regex;
use serde_json::Value;
use tracing;

use crate::models::LocalSearchResult;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Lean declaration keywords to search for.
const DECL_KEYWORDS: &[&str] = &[
    "theorem",
    "lemma",
    "def",
    "class",
    "structure",
    "inductive",
    "instance",
    "abbrev",
    "axiom",
    "opaque",
    "noncomputable def",
    "noncomputable instance",
    "protected def",
    "protected theorem",
    "protected lemma",
    "private def",
    "private theorem",
    "private lemma",
];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Check if the `rg` (ripgrep) binary is available on the system.
///
/// Returns `(available, message)` where `message` is either the version
/// string or an explanation of why ripgrep was not found.
pub fn check_ripgrep_status() -> (bool, String) {
    match std::process::Command::new("rg").arg("--version").output() {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .unwrap_or("ripgrep (unknown version)")
                .to_string();
            (true, version)
        }
        Ok(_) => (false, "ripgrep returned non-zero exit code".to_string()),
        Err(e) => (false, format!("ripgrep not found: {e}")),
    }
}

/// Search for Lean declarations matching `query` in the given project root.
///
/// # Algorithm
///
/// 1. Build a regex pattern matching declaration keywords followed by the query.
/// 2. Spawn `rg --json` with the pattern, searching `.lean` files.
/// 3. Parse the JSON output lines to extract declaration names and kinds.
/// 4. Resolve enclosing namespaces from `namespace` blocks in the source files.
/// 5. Sort by relevance: exact > prefix > contains, project files > packages.
/// 6. Deduplicate and truncate to `limit`.
pub fn lean_local_search(
    query: &str,
    limit: usize,
    project_root: &Path,
) -> Result<Vec<LocalSearchResult>, String> {
    if query.trim().is_empty() {
        return Err("Search query is empty".to_string());
    }

    if !project_root.is_dir() {
        return Err(format!(
            "Project root does not exist: {}",
            project_root.display()
        ));
    }

    // Build regex: match declaration keywords followed by the query.
    // The pattern matches optional attributes and modifiers before the keyword,
    // then captures the declaration name containing the query.
    let escaped_query = regex::escape(query);
    let keywords_pattern = DECL_KEYWORDS
        .iter()
        .map(|kw| regex::escape(kw))
        .collect::<Vec<_>>()
        .join("|");
    let pattern = format!(
        r"^\s*(?:@\[.*?\]\s+)?(?:(?:noncomputable|protected|private)\s+)?(?:{keywords_pattern})\s+(\S*{escaped_query}\S*)"
    );

    tracing::debug!("Local search pattern: {}", pattern);

    // Resolve to absolute path so ripgrep results have consistent paths
    let abs_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let max_results = (limit * 10).to_string();

    // Build ripgrep command with flags that search dependency sources.
    // Key: --no-ignore bypasses .gitignore so .lake/packages/ is included.
    // Glob patterns include only .lean files and exclude build artifacts.
    let mut args = vec![
        "--json",
        "--no-ignore",
        "--hidden",
        "--smart-case",
        "--color",
        "never",
        "--no-messages",
        "--no-heading",
        "--max-count",
        &max_results,
        "-g",
        "*.lean",
        "-g",
        "!.git/**",
        "-g",
        "!.lake/build/**",
        &pattern,
    ];

    // Primary search path: the project root (includes .lake/packages/)
    let root_str = abs_root.to_string_lossy().to_string();
    args.push(&root_str);

    // Optionally include Lean stdlib source directory
    let lean_src = get_lean_src_search_path(project_root);
    if let Some(ref src_path) = lean_src {
        args.push(src_path);
    }

    // Run ripgrep
    let output = std::process::Command::new("rg")
        .args(&args)
        .current_dir(project_root)
        .output()
        .map_err(|e| format!("Failed to run ripgrep: {e}"))?;

    // rg returns exit code 1 for "no matches" — that's not an error
    if !output.status.success() && output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ripgrep error: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Ok(vec![]);
    }

    // Parse JSON output lines
    let name_re = build_name_regex();
    let mut raw_results: Vec<RawMatch> = Vec::new();

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let json: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if json.get("type").and_then(|t| t.as_str()) != Some("match") {
            continue;
        }

        let data = match json.get("data") {
            Some(d) => d,
            None => continue,
        };

        let raw_file_path = data
            .get("path")
            .and_then(|p| p.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");

        // Make path relative to project root for display
        let file_path = make_relative_path(&abs_root, raw_file_path);

        let line_text = data
            .get("lines")
            .and_then(|l| l.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");

        let line_number = data
            .get("line_number")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);

        if let Some((name, kind)) = extract_declaration(&name_re, line_text) {
            raw_results.push(RawMatch {
                name,
                kind,
                file: file_path,
                line_number,
            });
        }
    }

    // Resolve namespaces and build final results
    let mut results: Vec<ScoredResult> = Vec::new();
    for raw in &raw_results {
        let full_name = resolve_namespace(project_root, &raw.file, raw.line_number, &raw.name);
        let score = relevance_score(query, &full_name, &raw.file);
        results.push(ScoredResult {
            result: LocalSearchResult {
                name: full_name,
                kind: raw.kind.clone(),
                file: raw.file.clone(),
            },
            score,
        });
    }

    // Sort by score (higher is better)
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Deduplicate by name
    let mut seen = std::collections::HashSet::new();
    let deduped: Vec<LocalSearchResult> = results
        .into_iter()
        .filter(|sr| seen.insert(sr.result.name.clone()))
        .map(|sr| sr.result)
        .take(limit)
        .collect();

    Ok(deduped)
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/// Raw match from ripgrep before namespace resolution.
struct RawMatch {
    name: String,
    kind: String,
    file: String,
    line_number: u64,
}

/// Result with a relevance score for sorting.
struct ScoredResult {
    result: LocalSearchResult,
    score: f64,
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Convert an absolute file path to a relative path from the project root.
///
/// If the path is already relative or cannot be made relative, returns it as-is.
fn make_relative_path(project_root: &Path, file_path: &str) -> String {
    let p = Path::new(file_path);
    if p.is_absolute() {
        match p.strip_prefix(project_root) {
            Ok(relative) => relative.to_string_lossy().to_string(),
            Err(_) => file_path.to_string(),
        }
    } else {
        file_path.to_string()
    }
}

/// Get the Lean stdlib source directory, if available.
///
/// Runs `lean --print-prefix` from the project root so that elan resolves
/// the correct toolchain. Returns the path to the `src/` directory if it exists.
fn get_lean_src_search_path(project_root: &Path) -> Option<String> {
    let output = std::process::Command::new("lean")
        .arg("--print-prefix")
        .current_dir(project_root)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let prefix = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if prefix.is_empty() {
        return None;
    }

    let src_dir = Path::new(&prefix).join("src");
    if src_dir.is_dir() {
        Some(src_dir.to_string_lossy().to_string())
    } else {
        None
    }
}

/// Build a regex that captures the declaration name from a Lean source line.
fn build_name_regex() -> Regex {
    let keywords = DECL_KEYWORDS
        .iter()
        .map(|kw| regex::escape(kw))
        .collect::<Vec<_>>()
        .join("|");
    Regex::new(&format!(
        r"(?:^|\s)(?:@\[.*?\]\s+)?(?:{keywords})\s+([^\s:({{]+)"
    ))
    .expect("Invalid name regex")
}

/// Extract the declaration name and kind from a source line.
fn extract_declaration(name_re: &Regex, line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();

    // Skip comment lines
    if trimmed.starts_with("--") || trimmed.starts_with("/-") {
        return None;
    }

    let caps = name_re.captures(trimmed)?;
    let name = caps.get(1)?.as_str().to_string();

    // Determine the kind from the line
    let kind = determine_kind(trimmed);

    // Skip empty names
    if name.is_empty() {
        return None;
    }

    Some((name, kind))
}

/// Determine the declaration kind from the source line.
fn determine_kind(line: &str) -> String {
    // Strip leading attributes
    let stripped = if let Some(idx) = line.find(']') {
        line[idx + 1..].trim_start()
    } else {
        line
    };

    // Strip modifiers
    let stripped = stripped
        .strip_prefix("noncomputable ")
        .or_else(|| stripped.strip_prefix("protected "))
        .or_else(|| stripped.strip_prefix("private "))
        .unwrap_or(stripped)
        .trim_start();

    if stripped.starts_with("theorem ") {
        "theorem".to_string()
    } else if stripped.starts_with("lemma ") {
        "lemma".to_string()
    } else if stripped.starts_with("def ") {
        "def".to_string()
    } else if stripped.starts_with("class ") {
        "class".to_string()
    } else if stripped.starts_with("structure ") {
        "structure".to_string()
    } else if stripped.starts_with("inductive ") {
        "inductive".to_string()
    } else if stripped.starts_with("instance ") {
        "instance".to_string()
    } else if stripped.starts_with("abbrev ") {
        "abbrev".to_string()
    } else if stripped.starts_with("axiom ") {
        "axiom".to_string()
    } else if stripped.starts_with("opaque ") {
        "opaque".to_string()
    } else {
        "unknown".to_string()
    }
}

/// Resolve the enclosing namespace for a declaration by scanning backwards in
/// the file for `namespace` and `section` blocks.
fn resolve_namespace(project_root: &Path, file_path: &str, line_number: u64, name: &str) -> String {
    let full_path = project_root.join(file_path);
    let content = match std::fs::read_to_string(&full_path) {
        Ok(c) => c,
        Err(_) => return name.to_string(),
    };

    let mut namespace_stack: Vec<String> = Vec::new();

    for (i, line) in content.lines().enumerate() {
        if (i as u64 + 1) >= line_number {
            break;
        }

        let trimmed = line.trim();

        if let Some(ns) = trimmed.strip_prefix("namespace ") {
            let ns = ns.trim();
            if !ns.is_empty() && !ns.starts_with("--") {
                namespace_stack.push(ns.to_string());
            }
        } else if trimmed.starts_with("end ") || trimmed == "end" {
            namespace_stack.pop();
        } else if let Some(sect) = trimmed.strip_prefix("section ") {
            let sect = sect.trim();
            if !sect.is_empty() && !sect.starts_with("--") {
                // Named sections create a scope but don't prefix names
                namespace_stack.push(String::new());
            }
        }
    }

    // Filter out empty entries (from named sections)
    let ns_parts: Vec<&str> = namespace_stack
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.as_str())
        .collect();

    if ns_parts.is_empty() {
        name.to_string()
    } else {
        format!("{}.{}", ns_parts.join("."), name)
    }
}

/// Compute a relevance score for a search result.
///
/// Higher is better. Factors:
/// - Exact match > prefix match > contains match
/// - Project files rank higher than `.lake/packages/` files
fn relevance_score(query: &str, full_name: &str, file_path: &str) -> f64 {
    let mut score = 0.0;

    let name_lower = full_name.to_lowercase();
    let query_lower = query.to_lowercase();

    // Match type scoring
    if name_lower == query_lower {
        score += 100.0;
    } else if name_lower.ends_with(&format!(".{query_lower}")) {
        // Exact match on the last component (e.g., Nat.add_comm matches add_comm)
        score += 90.0;
    } else if full_name
        .rsplit('.')
        .next()
        .is_some_and(|last| last.to_lowercase().starts_with(&query_lower))
    {
        score += 70.0;
    } else if full_name
        .rsplit('.')
        .next()
        .is_some_and(|last| last.to_lowercase().contains(&query_lower))
    {
        score += 50.0;
    } else if name_lower.contains(&query_lower) {
        score += 30.0;
    }

    // Source location scoring: project files > lake packages
    if file_path.contains(".lake/packages") || file_path.contains(".lake\\packages") {
        // Package file — lower priority
        score += 0.0;
    } else {
        // Project file — higher priority
        score += 10.0;
    }

    // Shorter names are generally more relevant
    let name_len = full_name.len() as f64;
    score += (1.0 / (1.0 + name_len * 0.01)).min(5.0);

    score
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ---- check_ripgrep_status ----------------------------------------------

    #[test]
    fn check_ripgrep_status_returns_tuple() {
        let (available, msg) = check_ripgrep_status();
        // In CI/dev environments, rg is typically available.
        // We just verify the function doesn't panic and returns sensible data.
        if available {
            assert!(msg.contains("ripgrep") || msg.contains("rg"));
        } else {
            assert!(!msg.is_empty());
        }
    }

    // ---- build_name_regex --------------------------------------------------

    #[test]
    fn name_regex_matches_theorem() {
        let re = build_name_regex();
        let caps = re.captures("theorem add_comm (a b : Nat) : a + b = b + a");
        assert!(caps.is_some());
        assert_eq!(caps.unwrap().get(1).unwrap().as_str(), "add_comm");
    }

    #[test]
    fn name_regex_matches_def() {
        let re = build_name_regex();
        let caps = re.captures("def myFunc : Nat := 42");
        assert!(caps.is_some());
        assert_eq!(caps.unwrap().get(1).unwrap().as_str(), "myFunc");
    }

    #[test]
    fn name_regex_matches_class() {
        let re = build_name_regex();
        let caps = re.captures("class MyClass where");
        assert!(caps.is_some());
        assert_eq!(caps.unwrap().get(1).unwrap().as_str(), "MyClass");
    }

    #[test]
    fn name_regex_matches_lemma() {
        let re = build_name_regex();
        let caps = re.captures("lemma foo_bar : True := trivial");
        assert!(caps.is_some());
        assert_eq!(caps.unwrap().get(1).unwrap().as_str(), "foo_bar");
    }

    #[test]
    fn name_regex_matches_noncomputable_def() {
        let re = build_name_regex();
        let caps = re.captures("noncomputable def myDef : Real := 1.0");
        assert!(caps.is_some());
        assert_eq!(caps.unwrap().get(1).unwrap().as_str(), "myDef");
    }

    #[test]
    fn name_regex_matches_with_attribute() {
        let re = build_name_regex();
        let caps = re.captures("@[simp] theorem simp_lemma : True := trivial");
        assert!(caps.is_some());
        assert_eq!(caps.unwrap().get(1).unwrap().as_str(), "simp_lemma");
    }

    // ---- extract_declaration -----------------------------------------------

    #[test]
    fn extract_declaration_theorem() {
        let re = build_name_regex();
        let result = extract_declaration(&re, "theorem add_comm : a + b = b + a := by ring");
        assert!(result.is_some());
        let (name, kind) = result.unwrap();
        assert_eq!(name, "add_comm");
        assert_eq!(kind, "theorem");
    }

    #[test]
    fn extract_declaration_private_def() {
        let re = build_name_regex();
        let result = extract_declaration(&re, "private def helper := 42");
        assert!(result.is_some());
        let (name, kind) = result.unwrap();
        assert_eq!(name, "helper");
        assert_eq!(kind, "def");
    }

    #[test]
    fn extract_declaration_skips_comments() {
        let re = build_name_regex();
        let result = extract_declaration(&re, "-- theorem not_a_theorem");
        assert!(result.is_none());
    }

    // ---- determine_kind ----------------------------------------------------

    #[test]
    fn determine_kind_all_variants() {
        assert_eq!(determine_kind("theorem foo"), "theorem");
        assert_eq!(determine_kind("lemma bar"), "lemma");
        assert_eq!(determine_kind("def baz"), "def");
        assert_eq!(determine_kind("class Qux where"), "class");
        assert_eq!(determine_kind("structure Point where"), "structure");
        assert_eq!(determine_kind("inductive Tree where"), "inductive");
        assert_eq!(determine_kind("instance : Foo where"), "instance");
        assert_eq!(determine_kind("abbrev MyType"), "abbrev");
        assert_eq!(determine_kind("axiom ax"), "axiom");
        assert_eq!(determine_kind("opaque hidden"), "opaque");
    }

    #[test]
    fn determine_kind_with_modifier() {
        assert_eq!(determine_kind("noncomputable def foo"), "def");
        assert_eq!(determine_kind("protected theorem bar"), "theorem");
        assert_eq!(determine_kind("private lemma baz"), "lemma");
    }

    // ---- relevance_score ---------------------------------------------------

    #[test]
    fn relevance_exact_match_highest() {
        let exact = relevance_score("add_comm", "add_comm", "src/main.lean");
        let prefix = relevance_score("add_comm", "add_comm_left", "src/main.lean");
        let contains = relevance_score("add_comm", "Nat.add_comm", "src/main.lean");
        assert!(exact > prefix);
        assert!(exact > contains);
    }

    #[test]
    fn relevance_project_over_package() {
        let project = relevance_score("foo", "foo", "src/main.lean");
        let package = relevance_score("foo", "foo", ".lake/packages/mathlib/src/main.lean");
        assert!(project > package);
    }

    #[test]
    fn relevance_last_component_exact_match() {
        let score = relevance_score("add_comm", "Nat.add_comm", "src/main.lean");
        let partial = relevance_score("add_comm", "Nat.add_comm_left", "src/main.lean");
        assert!(score > partial);
    }

    #[test]
    fn relevance_case_insensitive() {
        let score = relevance_score("AddComm", "addcomm", "src/main.lean");
        assert!(score > 50.0);
    }

    // ---- resolve_namespace -------------------------------------------------

    #[test]
    fn resolve_namespace_no_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let lean_file = tmp.path().join("Test.lean");
        fs::write(&lean_file, "import Lean\n\ntheorem foo : True := trivial\n").unwrap();

        let result = resolve_namespace(tmp.path(), "Test.lean", 3, "foo");
        assert_eq!(result, "foo");
    }

    #[test]
    fn resolve_namespace_single_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let lean_file = tmp.path().join("Test.lean");
        fs::write(
            &lean_file,
            "import Lean\n\nnamespace MyNs\n\ntheorem foo : True := trivial\n\nend MyNs\n",
        )
        .unwrap();

        let result = resolve_namespace(tmp.path(), "Test.lean", 5, "foo");
        assert_eq!(result, "MyNs.foo");
    }

    #[test]
    fn resolve_namespace_nested() {
        let tmp = tempfile::tempdir().unwrap();
        let lean_file = tmp.path().join("Test.lean");
        fs::write(
            &lean_file,
            "namespace Outer\nnamespace Inner\ntheorem bar := rfl\nend Inner\nend Outer\n",
        )
        .unwrap();

        let result = resolve_namespace(tmp.path(), "Test.lean", 3, "bar");
        assert_eq!(result, "Outer.Inner.bar");
    }

    #[test]
    fn resolve_namespace_after_end() {
        let tmp = tempfile::tempdir().unwrap();
        let lean_file = tmp.path().join("Test.lean");
        fs::write(&lean_file, "namespace Ns\nend Ns\ntheorem baz := rfl\n").unwrap();

        let result = resolve_namespace(tmp.path(), "Test.lean", 3, "baz");
        assert_eq!(result, "baz");
    }

    #[test]
    fn resolve_namespace_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let result = resolve_namespace(tmp.path(), "NoSuchFile.lean", 1, "foo");
        assert_eq!(result, "foo");
    }

    // ---- lean_local_search -------------------------------------------------

    #[test]
    fn lean_local_search_empty_query_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let result = lean_local_search("", 10, tmp.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty"));
    }

    #[test]
    fn lean_local_search_nonexistent_root_errors() {
        let result = lean_local_search("foo", 10, Path::new("/absolutely/nonexistent/path"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("does not exist"));
    }

    #[test]
    fn lean_local_search_no_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let lean_file = tmp.path().join("Test.lean");
        fs::write(&lean_file, "-- just a comment\n").unwrap();

        // This should return empty (no matches) rather than an error,
        // assuming rg is available.
        let (rg_available, _) = check_ripgrep_status();
        if rg_available {
            let result = lean_local_search("zzz_nonexistent_symbol_zzz", 10, tmp.path());
            // rg may not recognize .lean without config; either empty result or error is fine
            if let Ok(results) = result {
                assert!(results.is_empty());
            }
        }
    }

    // ---- deduplication logic -----------------------------------------------

    #[test]
    fn deduplication_preserves_first_occurrence() {
        // Test the dedup logic in isolation
        let results = vec![
            ScoredResult {
                result: LocalSearchResult {
                    name: "Foo.bar".to_string(),
                    kind: "theorem".to_string(),
                    file: "A.lean".to_string(),
                },
                score: 100.0,
            },
            ScoredResult {
                result: LocalSearchResult {
                    name: "Foo.bar".to_string(),
                    kind: "theorem".to_string(),
                    file: "B.lean".to_string(),
                },
                score: 50.0,
            },
            ScoredResult {
                result: LocalSearchResult {
                    name: "Baz.qux".to_string(),
                    kind: "def".to_string(),
                    file: "C.lean".to_string(),
                },
                score: 80.0,
            },
        ];

        let mut seen = std::collections::HashSet::new();
        let deduped: Vec<LocalSearchResult> = results
            .into_iter()
            .filter(|sr| seen.insert(sr.result.name.clone()))
            .map(|sr| sr.result)
            .take(10)
            .collect();

        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].name, "Foo.bar");
        assert_eq!(deduped[0].file, "A.lean"); // first occurrence preserved
        assert_eq!(deduped[1].name, "Baz.qux");
    }

    // ---- DECL_KEYWORDS constant -------------------------------------------

    #[test]
    fn decl_keywords_contains_essential_keywords() {
        assert!(DECL_KEYWORDS.contains(&"theorem"));
        assert!(DECL_KEYWORDS.contains(&"lemma"));
        assert!(DECL_KEYWORDS.contains(&"def"));
        assert!(DECL_KEYWORDS.contains(&"class"));
        assert!(DECL_KEYWORDS.contains(&"structure"));
        assert!(DECL_KEYWORDS.contains(&"inductive"));
        assert!(DECL_KEYWORDS.contains(&"instance"));
        assert!(DECL_KEYWORDS.contains(&"abbrev"));
    }

    // ---- make_relative_path -------------------------------------------------

    #[test]
    fn make_relative_path_absolute_inside_root() {
        let root = Path::new("/home/user/project");
        let result = make_relative_path(root, "/home/user/project/.lake/packages/mathlib/Foo.lean");
        assert_eq!(result, ".lake/packages/mathlib/Foo.lean");
    }

    #[test]
    fn make_relative_path_already_relative() {
        let root = Path::new("/home/user/project");
        let result = make_relative_path(root, ".lake/packages/mathlib/Foo.lean");
        assert_eq!(result, ".lake/packages/mathlib/Foo.lean");
    }

    #[test]
    fn make_relative_path_absolute_outside_root() {
        let root = Path::new("/home/user/project");
        let result = make_relative_path(root, "/opt/lean/src/Init.lean");
        assert_eq!(result, "/opt/lean/src/Init.lean");
    }

    // ---- lean_local_search with .lake/packages/ ----------------------------

    #[test]
    fn lean_local_search_finds_declarations_in_lake_packages() {
        let (rg_available, _) = check_ripgrep_status();
        if !rg_available {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();

        // Create a project-level lean file
        let project_file = tmp.path().join("Main.lean");
        fs::write(&project_file, "def myProjectDef : Nat := 42\n").unwrap();

        // Create a simulated .lake/packages/ dependency with a declaration
        let pkg_dir = tmp.path().join(".lake/packages/mathlib/Mathlib/Data");
        fs::create_dir_all(&pkg_dir).unwrap();
        let pkg_file = pkg_dir.join("Nat.lean");
        fs::write(
            &pkg_file,
            "namespace Polynomial\n\ntheorem card_roots (p : Polynomial) : True := trivial\n\nend Polynomial\n",
        )
        .unwrap();

        let results = lean_local_search("card_roots", 10, tmp.path()).unwrap();
        assert!(
            !results.is_empty(),
            "Expected to find card_roots in .lake/packages/ but got empty results"
        );
        assert_eq!(results[0].name, "Polynomial.card_roots");
        assert!(results[0].file.contains(".lake/packages/"));
    }

    #[test]
    fn lean_local_search_excludes_lake_build_artifacts() {
        let (rg_available, _) = check_ripgrep_status();
        if !rg_available {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();

        // Create a build artifact (should be excluded)
        let build_dir = tmp.path().join(".lake/build/lib");
        fs::create_dir_all(&build_dir).unwrap();
        let build_file = build_dir.join("BuildArtifact.lean");
        fs::write(&build_file, "theorem buildOnlyDecl : True := trivial\n").unwrap();

        // Create a legit project file
        let project_file = tmp.path().join("Main.lean");
        fs::write(&project_file, "-- no declarations\n").unwrap();

        let results = lean_local_search("buildOnlyDecl", 10, tmp.path()).unwrap();
        assert!(
            results.is_empty(),
            "Expected .lake/build/ declarations to be excluded, but found: {:?}",
            results
        );
    }

    #[test]
    fn lean_local_search_ranks_project_files_over_packages() {
        let (rg_available, _) = check_ripgrep_status();
        if !rg_available {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();

        // Project-local file with namespace to avoid dedup
        let project_file = tmp.path().join("MyProject.lean");
        fs::write(
            &project_file,
            "namespace Local\ndef sharedName : Nat := 1\nend Local\n",
        )
        .unwrap();

        // Package file with same base declaration name but different namespace
        let pkg_dir = tmp.path().join(".lake/packages/dep/Dep");
        fs::create_dir_all(&pkg_dir).unwrap();
        let pkg_file = pkg_dir.join("Lib.lean");
        fs::write(
            &pkg_file,
            "namespace Dep\ndef sharedName : Nat := 2\nend Dep\n",
        )
        .unwrap();

        let results = lean_local_search("sharedName", 10, tmp.path()).unwrap();
        assert!(
            results.len() >= 2,
            "Expected at least 2 results, got {}",
            results.len()
        );
        // Project file should rank first (Local.sharedName from project vs Dep.sharedName from package)
        assert!(
            !results[0].file.contains(".lake/packages/"),
            "Expected project file to rank first, but got: {}",
            results[0].file
        );
    }

    // ---- get_lean_src_search_path ------------------------------------------

    #[test]
    fn get_lean_src_search_path_returns_option() {
        // This test just checks the function doesn't panic.
        // In environments without lean installed, it returns None.
        let tmp = tempfile::tempdir().unwrap();
        let result = get_lean_src_search_path(tmp.path());
        // Either Some(valid_path) or None; both are fine
        if let Some(ref path) = result {
            assert!(Path::new(path).exists(), "Path should exist: {}", path);
        }
    }

    // ---- Send + Sync assertions -------------------------------------------

    #[test]
    fn local_search_result_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LocalSearchResult>();
    }
}
