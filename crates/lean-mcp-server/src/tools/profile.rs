//! Tool handler for `lean_profile_proof`.
//!
//! Runs `lean --profile` on a single theorem and parses trace output
//! into per-line timing data and cumulative category breakdowns.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::models::{LineProfile, ProofProfileResult};
use regex::Regex;
use tokio::process::Command;

/// Categories to exclude from the cumulative timing report.
const SKIP_CATEGORIES: &[&str] = &[
    "import",
    "initialization",
    "parsing",
    "interpretation",
    "linting",
];

/// A single parsed trace entry.
#[derive(Debug, Clone)]
struct TraceEntry {
    depth: usize,
    class: String,
    ms: f64,
    message: String,
}

/// A proof source line with metadata for matching.
#[derive(Debug, Clone)]
struct ProofItem {
    line_no: usize,
    content: String,
    is_bullet: bool,
}

// ---------------------------------------------------------------------------
// Header / theorem extraction
// ---------------------------------------------------------------------------

/// Regex for header lines (import, open, set_option, universe, variable).
fn is_header_line(line: &str) -> bool {
    let re = Regex::new(r"^(import|open|set_option|universe|variable)\s").expect("valid regex");
    re.is_match(line)
}

/// Regex for declaration starts.
fn declaration_re() -> Regex {
    Regex::new(r"^\s*(?:private\s+)?(theorem|lemma|def)\s+(\S+)").expect("valid regex")
}

/// Find where imports/header ends and declarations begin.
fn find_header_end(lines: &[&str]) -> usize {
    let decl_re = declaration_re();
    let mut header_end = 0;
    let mut in_block = false;

    for (i, line) in lines.iter().enumerate() {
        let s = line.trim();
        if line.contains("/-") {
            in_block = true;
        }
        if line.contains("-/") {
            in_block = false;
        }
        if in_block
            || s.is_empty()
            || s.starts_with("--")
            || is_header_line(line)
            || s.starts_with("namespace")
            || s.starts_with("section")
        {
            header_end = i + 1;
        } else if decl_re.is_match(line)
            || s.starts_with("@[")
            || s.starts_with("private ")
            || s.starts_with("protected ")
        {
            break;
        } else {
            header_end = i + 1;
        }
    }
    header_end
}

/// Find where theorem ends (next declaration or EOF).
fn find_theorem_end(lines: &[&str], start: usize) -> usize {
    let decl_re = declaration_re();
    for (i, line) in lines.iter().enumerate().skip(start + 1) {
        if decl_re.is_match(line) {
            return i;
        }
    }
    lines.len()
}

/// Extract imports/header + single theorem.
/// Returns `(source, theorem_name, theorem_start_line_in_source)`.
fn extract_theorem_source(
    lines: &[&str],
    target_line: usize,
) -> Result<(String, String, usize), LeanToolError> {
    let decl_re = declaration_re();
    let m = decl_re.captures(lines[target_line - 1]).ok_or_else(|| {
        LeanToolError::Other(format!("No theorem/lemma/def at line {target_line}"))
    })?;
    let name = m[2].to_string();

    let header_end = find_header_end(lines);
    let theorem_end = find_theorem_end(lines, target_line - 1);

    let header = lines[..header_end].join("\n");
    let theorem = lines[target_line - 1..theorem_end].join("\n");
    let source = format!("{header}\n\n{theorem}\n");
    // +2: one for the blank line between header and theorem, one for 1-indexing
    let src_start = header_end + 2;

    Ok((source, name, src_start))
}

// ---------------------------------------------------------------------------
// Trace output parsing
// ---------------------------------------------------------------------------

/// Parse lean --profile trace output into trace entries and cumulative times.
fn parse_output(output: &str) -> (Vec<TraceEntry>, HashMap<String, f64>) {
    let trace_re = Regex::new(r"^(\s*)\[([^\]]+)\]\s+\[([\d.]+)\]\s+(.+)$").expect("valid regex");
    let cumulative_re = Regex::new(r"^\s+(\S+(?:\s+\S+)*)\s+([\d.]+)(ms|s)$").expect("valid regex");

    let mut traces = Vec::new();
    let mut cumulative = HashMap::new();
    let mut in_cumulative = false;

    for line in output.lines() {
        if line.contains("cumulative profiling times:") {
            in_cumulative = true;
        } else if in_cumulative {
            if let Some(m) = cumulative_re.captures(line) {
                let cat = m[1].to_string();
                let val: f64 = m[2].parse().unwrap_or(0.0);
                let unit = &m[3];
                let ms = if unit == "s" { val * 1000.0 } else { val };
                cumulative.insert(cat, ms);
            }
        } else if let Some(m) = trace_re.captures(line) {
            let indent = m[1].len();
            let class = m[2].to_string();
            let time_s: f64 = m[3].parse().unwrap_or(0.0);
            let msg = m[4].to_string();
            traces.push(TraceEntry {
                depth: indent / 2,
                class,
                ms: time_s * 1000.0,
                message: msg,
            });
        }
    }

    (traces, cumulative)
}

// ---------------------------------------------------------------------------
// Line matching / timing extraction
// ---------------------------------------------------------------------------

/// Build list of proof items from source lines starting at `proof_start`.
fn build_proof_items(source_lines: &[&str], proof_start: usize) -> Vec<ProofItem> {
    let mut items = Vec::new();
    for (i, source_line) in source_lines.iter().enumerate().skip(proof_start) {
        let s = source_line.trim();
        if !s.is_empty() && !s.starts_with("--") {
            let first = s.chars().next().unwrap_or(' ');
            let is_bullet = matches!(first, '\u{00b7}' | '*' | '-');
            let content = s
                .trim_start_matches(|c: char| "\u{00b7}*- \t".contains(c))
                .to_string();
            items.push(ProofItem {
                line_no: i + 1, // 1-indexed
                content,
                is_bullet,
            });
        }
    }
    items
}

/// Find matching source line for a tactic trace.
fn match_line(
    tactic: &str,
    is_bullet: bool,
    items: &[ProofItem],
    used: &HashSet<usize>,
) -> Option<usize> {
    for item in items {
        if used.contains(&item.line_no) {
            continue;
        }
        if is_bullet && item.is_bullet {
            return Some(item.line_no);
        }
        if !is_bullet && !item.content.is_empty() {
            let tactic_prefix: String = tactic.chars().take(25).collect();
            let content_prefix: String = item.content.chars().take(25).collect();
            if tactic.starts_with(&content_prefix) || item.content.starts_with(&tactic_prefix) {
                return Some(item.line_no);
            }
        }
    }
    None
}

/// Extract per-line timing from traces.
fn extract_line_times(
    traces: &[TraceEntry],
    name: &str,
    proof_items: &[ProofItem],
) -> (HashMap<usize, f64>, f64) {
    let mut line_times: HashMap<usize, f64> = HashMap::new();
    let mut total = 0.0_f64;
    let mut value_depth = 0;
    let mut in_value = false;
    let mut tactic_depth: Option<usize> = None;
    let name_re = Regex::new(&format!(r"\b{}\b", regex::escape(name))).expect("valid regex");
    let mut used = HashSet::new();

    for entry in traces {
        if entry.class == "Elab.definition.value" && name_re.is_match(&entry.message) {
            in_value = true;
            value_depth = entry.depth;
            total = entry.ms;
        } else if entry.class == "Elab.async" && entry.message.contains(&format!("proof of {name}"))
        {
            total = total.max(entry.ms);
        } else if in_value {
            if entry.depth <= value_depth {
                break;
            }
            if entry.class == "Elab.step" && !entry.message.starts_with("expected type:") {
                if tactic_depth.is_none() {
                    tactic_depth = Some(entry.depth);
                }
                if Some(entry.depth) == tactic_depth {
                    let tactic = entry
                        .message
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim()
                        .trim_start_matches(|c: char| "\u{00b7}*- \t".contains(c));
                    let is_bullet = !tactic.is_empty()
                        && entry
                            .message
                            .trim()
                            .starts_with(|c: char| "\u{00b7}*-".contains(c));
                    if let Some(ln) = match_line(tactic, is_bullet, proof_items, &used) {
                        *line_times.entry(ln).or_insert(0.0) += entry.ms;
                        used.insert(ln);
                    }
                }
            }
        }
    }

    (line_times, total)
}

/// Filter cumulative categories to relevant ones (>= 1ms, excluding skip set).
fn filter_categories(cumulative: &HashMap<String, f64>) -> HashMap<String, f64> {
    let mut entries: Vec<_> = cumulative
        .iter()
        .filter(|(k, v)| !SKIP_CATEGORIES.contains(&k.as_str()) && **v >= 1.0)
        .map(|(k, v)| (k.clone(), (*v * 10.0).round() / 10.0))
        .collect();
    entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    entries.into_iter().collect()
}

/// Find line index (0-based) after `:= by` in source.
fn find_proof_start(source_lines: &[&str]) -> Result<usize, LeanToolError> {
    for (i, line) in source_lines.iter().enumerate() {
        if line.contains(":= by") || line.trim_end().ends_with(" by") {
            return Ok(i + 1);
        }
    }
    Err(LeanToolError::Other(
        "No 'by' proof found in theorem".to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Subprocess execution
// ---------------------------------------------------------------------------

/// Run `lake env lean --profile` and return the combined output.
///
/// Uses `kill_on_drop(true)` so that the child process is cleaned up
/// on timeout or cancellation.
async fn run_lean_profile(
    file_path: &Path,
    project_path: &Path,
    timeout: f64,
) -> Result<String, LeanToolError> {
    let child = Command::new("lake")
        .args([
            "env",
            "lean",
            "--profile",
            "-Dtrace.profiler=true",
            "-Dtrace.profiler.threshold=0",
        ])
        .arg(file_path)
        .current_dir(project_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| LeanToolError::Other(format!("Failed to run `lake env lean`: {e}")))?;

    let timeout_duration = std::time::Duration::from_secs_f64(timeout);
    match tokio::time::timeout(timeout_duration, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.is_empty() {
                Ok(stdout.to_string())
            } else if stdout.is_empty() {
                Ok(stderr.to_string())
            } else {
                Ok(format!("{stdout}\n{stderr}"))
            }
        }
        Ok(Err(e)) => Err(LeanToolError::Other(format!("lean --profile failed: {e}"))),
        Err(_) => Err(LeanToolError::LspTimeout(format!(
            "Profiling timed out after {timeout}s"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Public handler
// ---------------------------------------------------------------------------

/// Handle a `lean_profile_proof` tool call.
///
/// Reads the file, extracts the theorem at `theorem_line`, runs
/// `lean --profile` on it, and returns per-line timing data.
pub async fn handle_profile_proof(
    file_path: &Path,
    theorem_line: u32,
    project_path: &Path,
    timeout: f64,
    top_n: usize,
) -> Result<ProofProfileResult, LeanToolError> {
    let content = tokio::fs::read_to_string(file_path)
        .await
        .map_err(|e| LeanToolError::InvalidPath(format!("{}: {e}", file_path.display())))?;

    let lines: Vec<&str> = content.lines().collect();
    let theorem_line = theorem_line as usize;

    if theorem_line == 0 || theorem_line > lines.len() {
        return Err(LeanToolError::LineOutOfRange {
            line: theorem_line as u32,
            total: lines.len(),
        });
    }

    let (source, name, src_start) = extract_theorem_source(&lines, theorem_line)?;
    let source_lines: Vec<&str> = source.lines().collect();
    let line_offset = theorem_line as i64 - src_start as i64;
    let proof_start = find_proof_start(&source_lines)?;
    let proof_items = build_proof_items(&source_lines, proof_start);

    // Write source to temp file in project_path
    let temp_path = project_path.join(format!(".lean_profile_{}.lean", std::process::id()));
    tokio::fs::write(&temp_path, &source)
        .await
        .map_err(|e| LeanToolError::Other(format!("Failed to write temp file: {e}")))?;

    let result = run_lean_profile(&temp_path, project_path, timeout).await;

    // Clean up temp file regardless of result
    let _ = tokio::fs::remove_file(&temp_path).await;

    let output = result?;
    let (traces, cumulative) = parse_output(&output);
    let (line_times, total) = extract_line_times(&traces, &name, &proof_items);

    // Build top lines sorted by time, filtered to >= 1% of total
    let mut top_lines: Vec<(usize, f64)> = line_times
        .into_iter()
        .filter(|(_, ms)| *ms >= total * 0.01)
        .collect();
    top_lines.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    top_lines.truncate(top_n);

    let profile_lines: Vec<LineProfile> = top_lines
        .into_iter()
        .map(|(ln, ms)| {
            let text = if ln <= source_lines.len() {
                let t = source_lines[ln - 1].trim();
                if t.len() > 60 {
                    t[..60].to_string()
                } else {
                    t.to_string()
                }
            } else {
                String::new()
            };
            LineProfile {
                line: (ln as i64 + line_offset),
                ms: (ms * 10.0).round() / 10.0,
                text,
            }
        })
        .collect();

    Ok(ProofProfileResult {
        ms: (total * 10.0).round() / 10.0,
        lines: profile_lines,
        categories: filter_categories(&cumulative),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- find_header_end ----

    #[test]
    fn find_header_end_simple_imports() {
        let lines = vec![
            "import Mathlib.Tactic",
            "import Mathlib.Data.Nat.Basic",
            "",
            "theorem foo : True := trivial",
        ];
        assert_eq!(find_header_end(&lines), 3);
    }

    #[test]
    fn find_header_end_with_open_and_namespace() {
        let lines = vec![
            "import Mathlib.Tactic",
            "open Nat",
            "namespace MyNs",
            "",
            "theorem foo : True := trivial",
        ];
        assert_eq!(find_header_end(&lines), 4);
    }

    #[test]
    fn find_header_end_with_block_comment() {
        let lines = vec![
            "import Mathlib.Tactic",
            "/-",
            "  A block comment",
            "-/",
            "",
            "theorem foo : True := trivial",
        ];
        assert_eq!(find_header_end(&lines), 5);
    }

    #[test]
    fn find_header_end_no_header() {
        let lines = vec!["theorem foo : True := trivial"];
        assert_eq!(find_header_end(&lines), 0);
    }

    // ---- find_theorem_end ----

    #[test]
    fn find_theorem_end_single_theorem() {
        let lines = vec!["theorem foo : True := by", "  trivial"];
        assert_eq!(find_theorem_end(&lines, 0), 2);
    }

    #[test]
    fn find_theorem_end_multiple_theorems() {
        let lines = vec![
            "theorem foo : True := by",
            "  trivial",
            "theorem bar : True := by",
            "  trivial",
        ];
        assert_eq!(find_theorem_end(&lines, 0), 2);
    }

    // ---- extract_theorem_source ----

    #[test]
    fn extract_theorem_source_basic() {
        let lines = vec![
            "import Mathlib.Tactic",
            "",
            "theorem foo : True := by",
            "  trivial",
        ];
        let (source, name, _src_start) = extract_theorem_source(&lines, 3).unwrap();
        assert_eq!(name, "foo");
        assert!(source.contains("import Mathlib.Tactic"));
        assert!(source.contains("theorem foo : True := by"));
        assert!(source.contains("trivial"));
    }

    #[test]
    fn extract_theorem_source_not_a_theorem() {
        let lines = vec!["import Mathlib.Tactic", "", "-- just a comment"];
        let result = extract_theorem_source(&lines, 3);
        assert!(result.is_err());
    }

    #[test]
    fn extract_theorem_source_lemma() {
        let lines = vec![
            "import Mathlib.Tactic",
            "",
            "lemma bar (n : Nat) : n = n := by",
            "  rfl",
        ];
        let (source, name, _) = extract_theorem_source(&lines, 3).unwrap();
        assert_eq!(name, "bar");
        assert!(source.contains("lemma bar"));
    }

    #[test]
    fn extract_theorem_source_private_def() {
        let lines = vec![
            "import Mathlib.Tactic",
            "",
            "private def helper : Nat := 42",
        ];
        let (_, name, _) = extract_theorem_source(&lines, 3).unwrap();
        assert_eq!(name, "helper");
    }

    // ---- parse_output ----

    #[test]
    fn parse_output_trace_lines() {
        let output = "\
[Elab.definition.value] [0.050] foo
  [Elab.step] [0.030] intro h
    [Elab.step] [0.010] exact h";

        let (traces, cumulative) = parse_output(output);
        assert_eq!(traces.len(), 3);
        assert_eq!(traces[0].depth, 0);
        assert_eq!(traces[0].class, "Elab.definition.value");
        assert!((traces[0].ms - 50.0).abs() < 0.1);
        assert_eq!(traces[0].message, "foo");

        assert_eq!(traces[1].depth, 1);
        assert_eq!(traces[1].class, "Elab.step");
        assert!((traces[1].ms - 30.0).abs() < 0.1);

        assert_eq!(traces[2].depth, 2);
        assert!((traces[2].ms - 10.0).abs() < 0.1);

        assert!(cumulative.is_empty());
    }

    #[test]
    fn parse_output_cumulative_section() {
        let output = "\
[Elab.step] [0.010] foo
cumulative profiling times:
    elaboration 42.5ms
    type checking 13.2ms
    simp 1.5s";

        let (traces, cumulative) = parse_output(output);
        assert_eq!(traces.len(), 1);
        assert_eq!(cumulative.len(), 3);
        assert!((cumulative["elaboration"] - 42.5).abs() < 0.1);
        assert!((cumulative["type checking"] - 13.2).abs() < 0.1);
        assert!((cumulative["simp"] - 1500.0).abs() < 0.1);
    }

    #[test]
    fn parse_output_empty() {
        let (traces, cumulative) = parse_output("");
        assert!(traces.is_empty());
        assert!(cumulative.is_empty());
    }

    #[test]
    fn parse_output_no_traces_only_cumulative() {
        let output = "\
Some random output
cumulative profiling times:
    tactic 100.0ms";

        let (traces, cumulative) = parse_output(output);
        assert!(traces.is_empty());
        assert_eq!(cumulative.len(), 1);
        assert!((cumulative["tactic"] - 100.0).abs() < 0.1);
    }

    // ---- build_proof_items ----

    #[test]
    fn build_proof_items_basic() {
        let lines = vec!["theorem foo : P := by", "  intro h", "  exact h"];
        let items = build_proof_items(&lines, 1);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].line_no, 2);
        assert_eq!(items[0].content, "intro h");
        assert!(!items[0].is_bullet);
        assert_eq!(items[1].line_no, 3);
        assert_eq!(items[1].content, "exact h");
    }

    #[test]
    fn build_proof_items_with_bullets() {
        let lines = vec![
            "theorem foo : P := by",
            "  constructor",
            "  \u{00b7} intro h",
            "  \u{00b7} exact h",
        ];
        let items = build_proof_items(&lines, 1);
        assert_eq!(items.len(), 3);
        assert!(!items[0].is_bullet);
        assert!(items[1].is_bullet);
        assert!(items[2].is_bullet);
    }

    #[test]
    fn build_proof_items_skips_comments_and_blanks() {
        let lines = vec!["theorem foo : P := by", "  -- a comment", "", "  intro h"];
        let items = build_proof_items(&lines, 1);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].content, "intro h");
    }

    // ---- match_line ----

    #[test]
    fn match_line_exact_prefix() {
        let items = vec![
            ProofItem {
                line_no: 2,
                content: "intro h".into(),
                is_bullet: false,
            },
            ProofItem {
                line_no: 3,
                content: "exact h".into(),
                is_bullet: false,
            },
        ];
        let used = HashSet::new();
        assert_eq!(match_line("exact h", false, &items, &used), Some(3));
    }

    #[test]
    fn match_line_bullet_match() {
        let items = vec![
            ProofItem {
                line_no: 2,
                content: "intro h".into(),
                is_bullet: true,
            },
            ProofItem {
                line_no: 3,
                content: "exact h".into(),
                is_bullet: false,
            },
        ];
        let used = HashSet::new();
        assert_eq!(match_line("anything", true, &items, &used), Some(2));
    }

    #[test]
    fn match_line_skips_used() {
        let items = vec![
            ProofItem {
                line_no: 2,
                content: "intro h".into(),
                is_bullet: false,
            },
            ProofItem {
                line_no: 3,
                content: "intro h".into(),
                is_bullet: false,
            },
        ];
        let mut used = HashSet::new();
        used.insert(2);
        assert_eq!(match_line("intro h", false, &items, &used), Some(3));
    }

    #[test]
    fn match_line_no_match() {
        let items = vec![ProofItem {
            line_no: 2,
            content: "intro h".into(),
            is_bullet: false,
        }];
        let used = HashSet::new();
        assert_eq!(match_line("simp", false, &items, &used), None);
    }

    // ---- extract_line_times ----

    #[test]
    fn extract_line_times_basic() {
        let traces = vec![
            TraceEntry {
                depth: 0,
                class: "Elab.definition.value".into(),
                ms: 100.0,
                message: "myThm".into(),
            },
            TraceEntry {
                depth: 1,
                class: "Elab.step".into(),
                ms: 50.0,
                message: "intro h".into(),
            },
            TraceEntry {
                depth: 1,
                class: "Elab.step".into(),
                ms: 30.0,
                message: "exact h".into(),
            },
        ];
        let items = vec![
            ProofItem {
                line_no: 3,
                content: "intro h".into(),
                is_bullet: false,
            },
            ProofItem {
                line_no: 4,
                content: "exact h".into(),
                is_bullet: false,
            },
        ];
        let (times, total) = extract_line_times(&traces, "myThm", &items);
        assert!((total - 100.0).abs() < 0.1);
        assert_eq!(times.len(), 2);
        assert!((times[&3] - 50.0).abs() < 0.1);
        assert!((times[&4] - 30.0).abs() < 0.1);
    }

    #[test]
    fn extract_line_times_stops_at_value_depth() {
        let traces = vec![
            TraceEntry {
                depth: 0,
                class: "Elab.definition.value".into(),
                ms: 100.0,
                message: "myThm".into(),
            },
            TraceEntry {
                depth: 1,
                class: "Elab.step".into(),
                ms: 50.0,
                message: "intro h".into(),
            },
            TraceEntry {
                depth: 0,
                class: "Elab.definition.value".into(),
                ms: 200.0,
                message: "otherThm".into(),
            },
        ];
        let items = vec![ProofItem {
            line_no: 3,
            content: "intro h".into(),
            is_bullet: false,
        }];
        let (times, total) = extract_line_times(&traces, "myThm", &items);
        assert!((total - 100.0).abs() < 0.1);
        assert_eq!(times.len(), 1);
    }

    #[test]
    fn extract_line_times_async_proof() {
        let traces = vec![
            TraceEntry {
                depth: 0,
                class: "Elab.definition.value".into(),
                ms: 50.0,
                message: "myThm".into(),
            },
            TraceEntry {
                depth: 0,
                class: "Elab.async".into(),
                ms: 200.0,
                message: "proof of myThm".into(),
            },
        ];
        let items = vec![];
        let (_, total) = extract_line_times(&traces, "myThm", &items);
        assert!((total - 200.0).abs() < 0.1);
    }

    #[test]
    fn extract_line_times_skips_expected_type() {
        let traces = vec![
            TraceEntry {
                depth: 0,
                class: "Elab.definition.value".into(),
                ms: 100.0,
                message: "myThm".into(),
            },
            TraceEntry {
                depth: 1,
                class: "Elab.step".into(),
                ms: 10.0,
                message: "expected type: Nat".into(),
            },
            TraceEntry {
                depth: 1,
                class: "Elab.step".into(),
                ms: 50.0,
                message: "intro h".into(),
            },
        ];
        let items = vec![ProofItem {
            line_no: 3,
            content: "intro h".into(),
            is_bullet: false,
        }];
        let (times, _) = extract_line_times(&traces, "myThm", &items);
        assert_eq!(times.len(), 1);
        assert!(times.contains_key(&3));
    }

    // ---- filter_categories ----

    #[test]
    fn filter_categories_basic() {
        let mut cumulative = HashMap::new();
        cumulative.insert("elaboration".into(), 42.5);
        cumulative.insert("type checking".into(), 13.2);
        cumulative.insert("import".into(), 500.0);
        cumulative.insert("parsing".into(), 100.0);
        cumulative.insert("tiny".into(), 0.5);

        let filtered = filter_categories(&cumulative);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.contains_key("elaboration"));
        assert!(filtered.contains_key("type checking"));
        assert!(!filtered.contains_key("import"));
        assert!(!filtered.contains_key("parsing"));
        assert!(!filtered.contains_key("tiny"));
    }

    #[test]
    fn filter_categories_rounds_values() {
        let mut cumulative = HashMap::new();
        cumulative.insert("elaboration".into(), 42.55);
        let filtered = filter_categories(&cumulative);
        assert!((filtered["elaboration"] - 42.6).abs() < 0.01);
    }

    #[test]
    fn filter_categories_empty() {
        let cumulative = HashMap::new();
        let filtered = filter_categories(&cumulative);
        assert!(filtered.is_empty());
    }

    // ---- find_proof_start ----

    #[test]
    fn find_proof_start_inline() {
        let lines = vec!["theorem foo : P := by", "  intro h"];
        assert_eq!(find_proof_start(&lines).unwrap(), 1);
    }

    #[test]
    fn find_proof_start_ends_with_by() {
        let lines = vec!["theorem foo : P :=", "  by", "  intro h"];
        // The second line ends with "by"
        assert_eq!(find_proof_start(&lines).unwrap(), 2);
    }

    #[test]
    fn find_proof_start_not_found() {
        let lines = vec!["theorem foo : P := trivial"];
        assert!(find_proof_start(&lines).is_err());
    }

    // ---- ProofProfileResult construction ----

    #[test]
    fn profile_result_construction() {
        let result = ProofProfileResult {
            ms: 100.5,
            lines: vec![
                LineProfile {
                    line: 5,
                    ms: 50.0,
                    text: "intro h".into(),
                },
                LineProfile {
                    line: 6,
                    ms: 30.0,
                    text: "exact h".into(),
                },
            ],
            categories: {
                let mut m = HashMap::new();
                m.insert("elaboration".into(), 80.0);
                m
            },
        };
        assert!((result.ms - 100.5).abs() < f64::EPSILON);
        assert_eq!(result.lines.len(), 2);
        assert_eq!(result.lines[0].line, 5);
        assert_eq!(result.categories.len(), 1);
    }

    // ---- Declaration regex ----

    #[test]
    fn declaration_re_matches_theorem() {
        let re = declaration_re();
        let m = re.captures("theorem foo : True := trivial").unwrap();
        assert_eq!(&m[1], "theorem");
        assert_eq!(&m[2], "foo");
    }

    #[test]
    fn declaration_re_matches_private_lemma() {
        let re = declaration_re();
        let m = re
            .captures("private lemma bar (n : Nat) : n = n := rfl")
            .unwrap();
        assert_eq!(&m[1], "lemma");
        assert_eq!(&m[2], "bar");
    }

    #[test]
    fn declaration_re_matches_def() {
        let re = declaration_re();
        let m = re.captures("def helper : Nat := 42").unwrap();
        assert_eq!(&m[1], "def");
        assert_eq!(&m[2], "helper");
    }

    #[test]
    fn declaration_re_no_match_on_comment() {
        let re = declaration_re();
        assert!(re.captures("-- theorem foo").is_none());
    }

    // ---- Trace parsing integration ----

    #[test]
    fn parse_output_full_trace() {
        let output = "\
[Elab.definition.value] [0.150] myThm
  [Elab.step] [0.080] intro h
  [Elab.step] [0.050] exact h
  [Elab.step] [0.005] expected type: Bool
cumulative profiling times:
    elaboration 150.0ms
    initialization 5.0ms
    type checking 30.5ms
    import 200.0ms";

        let (traces, cumulative) = parse_output(output);
        assert_eq!(traces.len(), 4);
        assert_eq!(cumulative.len(), 4);

        // Verify filtering
        let filtered = filter_categories(&cumulative);
        assert_eq!(filtered.len(), 2); // elaboration + type checking
        assert!(!filtered.contains_key("initialization"));
        assert!(!filtered.contains_key("import"));
    }

    // ---- End-to-end pipeline (no subprocess) ----

    #[test]
    fn end_to_end_parse_and_extract() {
        let source = "\
import Mathlib.Tactic

theorem myThm (P Q : Prop) (hp : P) (hq : Q) : P := by
  intro
  exact hp";

        let source_lines: Vec<&str> = source.lines().collect();
        let proof_start = find_proof_start(&source_lines).unwrap();
        let proof_items = build_proof_items(&source_lines, proof_start);
        assert_eq!(proof_items.len(), 2);

        let trace_output = "\
[Elab.definition.value] [0.100] myThm
  [Elab.step] [0.060] intro
  [Elab.step] [0.030] exact hp
cumulative profiling times:
    elaboration 100.0ms";

        let (traces, cumulative) = parse_output(trace_output);
        let (line_times, total) = extract_line_times(&traces, "myThm", &proof_items);

        assert!((total - 100.0).abs() < 0.1);
        assert_eq!(line_times.len(), 2);

        let filtered = filter_categories(&cumulative);
        assert!(filtered.contains_key("elaboration"));
    }
}
