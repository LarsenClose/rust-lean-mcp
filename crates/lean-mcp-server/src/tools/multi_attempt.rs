//! Tool handler for `lean_multi_attempt`.
//!
//! Tries multiple tactic snippets at a given file position without permanently
//! modifying the file. Returns goal state and diagnostics for each snippet.
//!
//! Three paths:
//! - **REPL fast path**: when REPL is available, no column is specified, and
//!   no snippet contains newlines. Uses `Repl::run_snippets()`.
//! - **LSP fallback**: for each snippet, temporarily inserts the tactic text
//!   via incremental file edits, collects diagnostics + goals, then restores
//!   the original file content.
//! - **Parallel path** (`parallel=true`): each snippet is tested via an
//!   independent `run_code`-style temp file. No file mutation, naturally
//!   concurrent via `futures::future::join_all`.

use lean_lsp_client::client::LspClient;
use lean_lsp_client::types::severity;
use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::models::{AttemptResult, DiagnosticMessage, MultiAttemptResult};
use lean_mcp_core::repl::Repl;
use lean_mcp_core::utils::extract_goals_list;
use serde_json::{json, Value};
use std::path::Path;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert raw LSP diagnostics into [`DiagnosticMessage`] items.
///
/// When `context_lines` is provided (> 0), diagnostics from lines before
/// `context_lines` (0-indexed) are filtered out, and line numbers in the
/// remaining diagnostics are adjusted so they are relative to the snippet.
/// This mirrors the `to_diagnostic_messages` helper in `run_code.rs` and
/// ensures that diagnostics from a context/base-code region are excluded.
pub fn to_diagnostic_messages(
    diagnostics: &[Value],
    context_lines: usize,
) -> Vec<DiagnosticMessage> {
    let mut items = Vec::new();
    for diag in diagnostics {
        let range = diag.get("fullRange").or_else(|| diag.get("range"));
        let Some(r) = range else { continue };

        let raw_line = r
            .pointer("/start/line")
            .and_then(Value::as_i64)
            .unwrap_or(0);

        // Filter out diagnostics from the context region.
        if context_lines > 0 && (raw_line as usize) < context_lines {
            continue;
        }

        let severity_int = diag.get("severity").and_then(Value::as_i64).unwrap_or(1);
        let sev_name = match severity_int as i32 {
            severity::ERROR => "error",
            severity::WARNING => "warning",
            severity::INFO => "info",
            severity::HINT => "hint",
            _ => "unknown",
        };

        let message = diag.get("message").and_then(Value::as_str).unwrap_or("");
        // Adjust line number relative to snippet (subtract context lines).
        let line = raw_line - context_lines as i64 + 1;
        let column = r
            .pointer("/start/character")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            + 1;

        items.push(DiagnosticMessage {
            severity: sev_name.to_string(),
            message: message.to_string(),
            line,
            column,
        });
    }
    items
}

/// Filter diagnostics whose range intersects `[start_line, end_line]` (0-indexed).
pub fn filter_diagnostics_by_line_range(
    diagnostics: &[Value],
    start_line: u32,
    end_line: u32,
) -> Vec<Value> {
    diagnostics
        .iter()
        .filter(|diag| {
            let range = diag.get("range").or_else(|| diag.get("fullRange"));
            let Some(r) = range else { return false };
            let Some(ds) = r.pointer("/start/line").and_then(Value::as_u64) else {
                return false;
            };
            let Some(de) = r.pointer("/end/line").and_then(Value::as_u64) else {
                return false;
            };
            let ds = ds as u32;
            let de = de as u32;
            // Intersects if not fully before or fully after
            !(de < start_line || ds > end_line)
        })
        .cloned()
        .collect()
}

/// Resolve the 0-indexed insertion column.
///
/// When `column` is `None`, returns the index of the first non-whitespace
/// character on the line (or 0 if the line is all whitespace).
/// When `column` is `Some(c)` (1-indexed), validates the range and returns `c - 1`.
pub fn resolve_column(line_text: &str, column: Option<u32>) -> Result<u32, LeanToolError> {
    match column {
        None => Ok(line_text.find(|c: char| !c.is_whitespace()).unwrap_or(0) as u32),
        Some(col) => {
            if col == 0 || col as usize > line_text.len() + 1 {
                return Err(LeanToolError::ColumnOutOfRange {
                    column: col,
                    length: line_text.len(),
                });
            }
            Ok(col - 1)
        }
    }
}

/// Build the temporary LSP edit and return the goal cursor position.
///
/// Returns `(snippet_str, change_json, goal_line_0, goal_col_0)`.
pub fn prepare_edit(
    line_text: &str,
    target_col: u32,
    snippet: &str,
    total_lines: usize,
    line_1: u32,
) -> (String, Value, u32, u32) {
    let snippet_str = snippet.trim_end_matches('\n');
    let snippet_lines: Vec<&str> = if snippet_str.is_empty() {
        vec![""]
    } else {
        snippet_str.split('\n').collect()
    };
    let indent = &line_text[..target_col as usize];

    let mut payload_lines = vec![snippet_lines[0].to_string()];
    for part in &snippet_lines[1..] {
        payload_lines.push(format!("{indent}{part}"));
    }
    let payload = payload_lines.join("\n") + "\n";

    let replaced_line_count = snippet_lines.len().max(1);
    let end_line_0 = ((line_1 - 1) as usize + replaced_line_count).min(total_lines) as u32;

    let change = json!({
        "text": payload,
        "range": {
            "start": {"line": line_1 - 1, "character": target_col},
            "end": {"line": end_line_0, "character": 0}
        }
    });

    let goal_line = (line_1 - 1) + (payload_lines.len() as u32) - 1;
    let goal_column = if payload_lines.len() == 1 {
        target_col + payload_lines[0].len() as u32
    } else {
        payload_lines.last().map(|l| l.len() as u32).unwrap_or(0)
    };

    (snippet_str.to_string(), change, goal_line, goal_column)
}

/// Compute the incremental LSP change that restores the original text after
/// a forward edit produced by [`prepare_edit`].
///
/// Instead of replacing the entire file with `update_file_content`, this builds
/// a targeted `textDocument/didChange` entry that only touches the edited
/// region. Lean 4.9+ can then reuse tactic-level snapshots above the edit,
/// re-elaborating only from the changed line down.
///
/// # Arguments
///
/// * `original_content` – the full original file content (before any edits)
/// * `lines`            – lines of the **original** file (from `.lines()`)
/// * `line_1`           – 1-indexed target line
/// * `target_col`       – 0-indexed column where the edit starts
/// * `snippet`          – the snippet that was inserted (same value passed to `prepare_edit`)
/// * `total_lines`      – number of lines in the original file
///
/// # Returns
///
/// A JSON value suitable for passing to `client.update_file(path, vec![restore])`.
pub fn prepare_restore_edit(
    original_content: &str,
    lines: &[&str],
    line_1: u32,
    target_col: u32,
    snippet: &str,
    total_lines: usize,
) -> Value {
    let snippet_str = snippet.trim_end_matches('\n');
    let snippet_lines: Vec<&str> = if snippet_str.is_empty() {
        vec![""]
    } else {
        snippet_str.split('\n').collect()
    };

    // Number of payload lines produced by prepare_edit (mirrors its logic).
    let payload_lines_count = snippet_lines.len();

    // After the forward edit the payload occupies payload_lines_count lines
    // starting at (line_1-1, target_col). The payload always ends with '\n',
    // so the restore end is at (line_1-1 + payload_lines_count, 0).
    let restore_end_line_0 = (line_1 - 1) as usize + payload_lines_count;

    // The original edit range: (line_1-1, target_col) to (end_line_0_orig, 0).
    let replaced_line_count = snippet_lines.len().max(1);
    let end_line_0_orig = ((line_1 - 1) as usize + replaced_line_count).min(total_lines);

    // Extract the original text by byte offsets from original_content.
    // Start offset: sum of lengths of lines before line_1-1, plus newlines,
    // plus target_col.
    let start_idx = (line_1 - 1) as usize;
    let start_byte: usize = lines[..start_idx]
        .iter()
        .map(|l| l.len() + 1) // +1 for '\n'
        .sum::<usize>()
        + target_col as usize;

    // End offset: position (end_line_0_orig, 0).
    // If end_line_0_orig >= total_lines, this is past the last line, so
    // use the full content length.
    let end_byte: usize = if end_line_0_orig >= total_lines {
        original_content.len()
    } else {
        lines[..end_line_0_orig]
            .iter()
            .map(|l| l.len() + 1)
            .sum::<usize>()
    };

    let original_text = &original_content[start_byte..end_byte];

    json!({
        "text": original_text,
        "range": {
            "start": {"line": line_1 - 1, "character": target_col},
            "end": {"line": restore_end_line_0, "character": 0}
        }
    })
}

// ---------------------------------------------------------------------------
// REPL fast path
// ---------------------------------------------------------------------------

/// Try the REPL fast path for multi-attempt.
///
/// Returns `None` when the REPL path is not applicable (column specified,
/// multiline snippets, or no REPL available).
async fn try_repl_path(
    client: &dyn LspClient,
    repl: Option<&mut Repl>,
    file_path: &str,
    line: u32,
    snippets: &[String],
    column: Option<u32>,
) -> Result<Option<MultiAttemptResult>, LeanToolError> {
    // REPL not usable when column specified, multiline snippets, or no repl
    if column.is_some() || snippets.iter().any(|s| s.contains('\n')) {
        return Ok(None);
    }
    let Some(repl) = repl else {
        return Ok(None);
    };

    // Read file content to extract base code up to the target line
    let content =
        client
            .get_file_content(file_path)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_file_content".into(),
                message: e.to_string(),
            })?;

    let lines: Vec<&str> = content.lines().collect();
    if line == 0 || line as usize > lines.len() {
        return Err(LeanToolError::LineOutOfRange {
            line,
            total: lines.len(),
        });
    }

    let base_code = lines[..line as usize - 1].join("\n");
    let repl_results = repl.run_snippets(&base_code, snippets).await;

    let mut items = Vec::with_capacity(snippets.len());
    for (snippet, pr) in snippets.iter().zip(repl_results.iter()) {
        let mut diagnostics: Vec<DiagnosticMessage> = pr
            .messages
            .iter()
            .map(|m| DiagnosticMessage {
                severity: m
                    .get("severity")
                    .and_then(Value::as_str)
                    .unwrap_or("info")
                    .to_string(),
                message: m
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                line: m.pointer("/pos/line").and_then(Value::as_i64).unwrap_or(0),
                column: m
                    .pointer("/pos/column")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
            })
            .collect();

        if let Some(ref err) = pr.error {
            diagnostics.push(DiagnosticMessage {
                severity: "error".to_string(),
                message: err.clone(),
                line: 0,
                column: 0,
            });
        }

        items.push(AttemptResult {
            snippet: snippet.trim_end_matches('\n').to_string(),
            goals: pr.goals.clone(),
            diagnostics,
            timed_out: false,
        });
    }

    Ok(Some(MultiAttemptResult { items }))
}

// ---------------------------------------------------------------------------
// LSP fallback path
// ---------------------------------------------------------------------------

/// LSP-based multi-attempt: edit file, get diagnostics + goals, restore.
async fn lsp_path(
    client: &dyn LspClient,
    file_path: &str,
    line: u32,
    snippets: &[String],
    column: Option<u32>,
) -> Result<MultiAttemptResult, LeanToolError> {
    // 1. Open file
    client
        .open_file(file_path)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "open_file".into(),
            message: e.to_string(),
        })?;

    // 2. Save original content
    let original_content =
        client
            .get_file_content(file_path)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_file_content".into(),
                message: e.to_string(),
            })?;

    let lines: Vec<&str> = original_content.lines().collect();
    if line == 0 || line as usize > lines.len() {
        return Err(LeanToolError::LineOutOfRange {
            line,
            total: lines.len(),
        });
    }

    let line_text = lines[(line - 1) as usize];
    let target_col = resolve_column(line_text, column)?;

    // 3. Try each snippet, always restoring afterwards
    let result = async {
        let mut items = Vec::with_capacity(snippets.len());
        for snippet in snippets {
            let (snippet_str, change, goal_line, goal_column) =
                prepare_edit(line_text, target_col, snippet, lines.len(), line);

            // Apply the edit
            client
                .update_file(file_path, vec![change])
                .await
                .map_err(|e| LeanToolError::LspError {
                    operation: "update_file".into(),
                    message: e.to_string(),
                })?;

            // Get diagnostics
            let raw_diags = client
                .get_diagnostics(file_path, None, None, Some(15.0))
                .await
                .map_err(|e| LeanToolError::LspError {
                    operation: "get_diagnostics".into(),
                    message: e.to_string(),
                })?;

            let all_diags = raw_diags
                .get("diagnostics")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            let filtered = filter_diagnostics_by_line_range(&all_diags, line - 1, goal_line);
            let diagnostics = to_diagnostic_messages(&filtered, 0);

            // Get goals
            let goal_result = client
                .get_goal(file_path, goal_line, goal_column)
                .await
                .map_err(|e| LeanToolError::LspError {
                    operation: "get_goal".into(),
                    message: e.to_string(),
                })?;

            let goals = extract_goals_list(goal_result.as_ref());

            items.push(AttemptResult {
                snippet: snippet_str,
                goals,
                diagnostics,
                timed_out: false,
            });

            // Restore original content before next snippet
            client
                .update_file_content(file_path, &original_content)
                .await
                .map_err(|e| LeanToolError::LspError {
                    operation: "update_file_content".into(),
                    message: e.to_string(),
                })?;
        }
        Ok(MultiAttemptResult { items })
    }
    .await;

    // 4. Always restore original and force reopen
    let _ = client
        .update_file_content(file_path, &original_content)
        .await;
    let _ = client.open_file_force(file_path).await;

    result
}

// ---------------------------------------------------------------------------
// Warm LSP single-snippet helper
// ---------------------------------------------------------------------------

/// Run a single snippet against the warm LSP using edit-and-restore.
///
/// This is the per-snippet body extracted from `lsp_path`. It:
/// 1. Applies the snippet edit at `target_col` on `line`
/// 2. Gets diagnostics and goals
/// 3. Restores the original content
///
/// `line` is 1-indexed. `target_col` is 0-indexed.
///
/// On any error, returns an [`AttemptResult`] with the error captured as a
/// diagnostic rather than propagating. This ensures the caller always gets
/// a result to post to TaskManager.
pub async fn run_one_snippet_warm(
    client: &dyn LspClient,
    file_path: &str,
    original_content: &str,
    line: u32,
    target_col: u32,
    snippet: &str,
    timeout_secs: Option<f64>,
) -> AttemptResult {
    let lines: Vec<&str> = original_content.lines().collect();
    let line_text = if (line as usize) <= lines.len() && line > 0 {
        lines[(line - 1) as usize]
    } else {
        ""
    };

    let lsp_work = async {
        let (snippet_str, change, goal_line, goal_column) =
            prepare_edit(line_text, target_col, snippet, lines.len(), line);

        // Apply the edit
        client
            .update_file(file_path, vec![change])
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "update_file".into(),
                message: e.to_string(),
            })?;

        // Get diagnostics
        let raw_diags = client
            .get_diagnostics(file_path, None, None, Some(15.0))
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_diagnostics".into(),
                message: e.to_string(),
            })?;

        let all_diags = raw_diags
            .get("diagnostics")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let filtered = filter_diagnostics_by_line_range(&all_diags, line - 1, goal_line);
        let diagnostics = to_diagnostic_messages(&filtered, 0);

        // Get goals
        let goal_result = client
            .get_goal(file_path, goal_line, goal_column)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_goal".into(),
                message: e.to_string(),
            })?;

        let goals = extract_goals_list(goal_result.as_ref());

        Ok::<AttemptResult, LeanToolError>(AttemptResult {
            snippet: snippet_str,
            goals,
            diagnostics,
            timed_out: false,
        })
    };

    // Apply per-snippet timeout if configured
    let result: Result<AttemptResult, LeanToolError> = if let Some(secs) = timeout_secs {
        let snippet_str = snippet.trim_end_matches('\n');
        match tokio::time::timeout(std::time::Duration::from_secs_f64(secs), lsp_work).await {
            Ok(r) => r,
            Err(_) => Ok(AttemptResult {
                snippet: snippet_str.to_string(),
                goals: Vec::new(),
                diagnostics: vec![DiagnosticMessage {
                    severity: "warning".to_string(),
                    message: format!("Tactic timed out after {secs}s"),
                    line: 0,
                    column: 0,
                }],
                timed_out: true,
            }),
        }
    } else {
        lsp_work.await
    };

    // Restore via targeted incremental edit so Lean 4.9+ can reuse
    // tactic-level snapshots above the edited line. Falls back to full
    // content replacement if the incremental restore fails.
    let restore_change = prepare_restore_edit(
        original_content,
        &lines,
        line,
        target_col,
        snippet,
        lines.len(),
    );
    if client
        .update_file(file_path, vec![restore_change])
        .await
        .is_err()
    {
        let _ = client
            .update_file_content(file_path, original_content)
            .await;
    }

    match result {
        Ok(r) => r,
        Err(e) => {
            let snippet_str = snippet.trim_end_matches('\n');
            AttemptResult {
                snippet: snippet_str.to_string(),
                goals: Vec::new(),
                diagnostics: vec![DiagnosticMessage {
                    severity: "error".to_string(),
                    message: e.to_string(),
                    line: 0,
                    column: 0,
                }],
                timed_out: false,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Parallel path (run_code semantics)
// ---------------------------------------------------------------------------

/// Run a single snippet via an independent temp file, returning its result.
///
/// The temp file contains `base_code + indented_snippet + indented_sorry`,
/// which gives the LSP enough context to check the tactic. Diagnostics are
/// collected and the file is always cleaned up.
///
/// `indent` is the leading whitespace from the target line, applied to each
/// line of the snippet and to the trailing `sorry` so that Lean's
/// whitespace-sensitive tactic blocks are preserved.
///
/// When `timeout_secs` is `Some(t)`, the entire LSP interaction (open, get
/// diagnostics, get goal) is capped at `t` seconds. On timeout the result
/// has `timed_out: true` with a warning diagnostic.
pub async fn run_snippet_isolated(
    client: &dyn LspClient,
    project_path: &Path,
    snippet: &str,
    base_code: &str,
    indent: &str,
    timeout_secs: Option<f64>,
) -> AttemptResult {
    let snippet_str = snippet.trim_end_matches('\n');

    // Indent each line of the snippet to match the target line's indentation
    let indented_snippet: String = snippet_str
        .lines()
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n");

    let code = if base_code.is_empty() {
        format!("{indented_snippet}\n{indent}sorry")
    } else {
        format!("{base_code}\n{indented_snippet}\n{indent}sorry")
    };

    let base_line_count = base_code.lines().count();

    let mcp_dir = project_path.join(".lake").join("_mcp");
    std::fs::create_dir_all(&mcp_dir).ok(); // Ensure directory exists
    let filename = format!("_mcp_attempt_{}.lean", Uuid::new_v4().as_simple());
    let abs_path = mcp_dir.join(&filename);
    let rel_path = format!(".lake/_mcp/{filename}");

    // Write temp file
    if let Err(e) = std::fs::write(&abs_path, &code) {
        return AttemptResult {
            snippet: snippet_str.to_string(),
            goals: Vec::new(),
            diagnostics: vec![DiagnosticMessage {
                severity: "error".to_string(),
                message: format!("Failed to write temp file: {e}"),
                line: 0,
                column: 0,
            }],
            timed_out: false,
        };
    }

    let lsp_work = async {
        // Open in LSP
        client
            .open_file(&rel_path)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "open_file".into(),
                message: e.to_string(),
            })?;

        // Get diagnostics
        let raw = client
            .get_diagnostics(&rel_path, None, None, Some(15.0))
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_diagnostics".into(),
                message: e.to_string(),
            })?;

        let all_diags = raw
            .get("diagnostics")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        // Filter diagnostics: use context_lines to exclude base_code diagnostics
        // and adjust line numbers to be snippet-relative (matching run_code.rs's
        // file_context approach). This prevents false positives from context-region
        // diagnostics bleeding into snippet results — e.g., an error diagnostic
        // from a tactic like `omega` on a `Field` goal may have a line number in
        // the theorem header (context region) rather than the snippet region, so
        // the old filter_diagnostics_by_line_range approach would miss it.
        let context_lines = base_line_count;
        let diagnostics = to_diagnostic_messages(&all_diags, context_lines);

        // Get goal state at the end of the snippet (before sorry)
        // The snippet's last line is at base_line_count + snippet_line_count - 1
        let snippet_line_count = snippet_str.lines().count().max(1) as u32;
        let goal_line = base_line_count as u32 + snippet_line_count - 1;
        let last_snippet_line = snippet_str.lines().last().unwrap_or("");
        let goal_column = indent.len() as u32 + last_snippet_line.len() as u32;

        let goal_result = client
            .get_goal(&rel_path, goal_line, goal_column)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_goal".into(),
                message: e.to_string(),
            })?;

        let goals = extract_goals_list(goal_result.as_ref());

        Ok(AttemptResult {
            snippet: snippet_str.to_string(),
            goals,
            diagnostics,
            timed_out: false,
        })
    };

    // Apply per-snippet timeout if configured
    let result: Result<AttemptResult, LeanToolError> = if let Some(secs) = timeout_secs {
        match tokio::time::timeout(std::time::Duration::from_secs_f64(secs), lsp_work).await {
            Ok(r) => r,
            Err(_) => Ok(AttemptResult {
                snippet: snippet_str.to_string(),
                goals: Vec::new(),
                diagnostics: vec![DiagnosticMessage {
                    severity: "warning".to_string(),
                    message: format!("Tactic timed out after {secs}s"),
                    line: 0,
                    column: 0,
                }],
                timed_out: true,
            }),
        }
    } else {
        lsp_work.await
    };

    // Always clean up (even on timeout)
    let _ = client.close_files(&[rel_path]).await;
    let _ = std::fs::remove_file(&abs_path);

    match result {
        Ok(r) => r,
        Err(e) => AttemptResult {
            snippet: snippet_str.to_string(),
            goals: Vec::new(),
            diagnostics: vec![DiagnosticMessage {
                severity: "error".to_string(),
                message: e.to_string(),
                line: 0,
                column: 0,
            }],
            timed_out: false,
        },
    }
}

/// Parallel multi-attempt: tests each tactic via independent run_code calls.
///
/// Each tactic gets the file content up to the target line + the tactic appended
/// to an independent temp file. No file mutation, no need to restore state,
/// naturally parallelizable via `futures::future::join_all`.
///
/// `line` is **1-indexed** (matching the MCP tool interface).
///
/// When `timeout_per_snippet` is `Some(t)`, each snippet's LSP interaction
/// is capped at `t` seconds. Timed-out snippets return `timed_out: true`.
pub async fn handle_multi_attempt_parallel(
    client: &dyn LspClient,
    project_path: &Path,
    file_path: &str,
    line: u32,
    snippets: &[String],
    timeout_per_snippet: Option<f64>,
) -> Result<MultiAttemptResult, LeanToolError> {
    if snippets.is_empty() {
        return Ok(MultiAttemptResult { items: Vec::new() });
    }

    // 0. Ensure file is open before reading content (#90).
    //    On a cold LSP (file never previously opened), get_file_content will
    //    fail with "File not open". This mirrors what the non-parallel lsp_path
    //    does (line 260-266). open_file is idempotent — a no-op if already open.
    client
        .open_file(file_path)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "open_file".into(),
            message: e.to_string(),
        })?;

    // 1. Read file content to extract base code
    let content =
        client
            .get_file_content(file_path)
            .await
            .map_err(|e| LeanToolError::LspError {
                operation: "get_file_content".into(),
                message: e.to_string(),
            })?;

    let lines: Vec<&str> = content.lines().collect();
    if line == 0 || line as usize > lines.len() {
        return Err(LeanToolError::LineOutOfRange {
            line,
            total: lines.len(),
        });
    }

    // 2. Extract code up to target line (imports + context before the tactic).
    //    Do NOT inject maxHeartbeats: the context file may have its own options,
    //    and injecting an extra line shifts line numbers, confusing diagnostic
    //    filtering (matching run_code.rs's file_context approach which also
    //    skips maxHeartbeats when context is provided).
    let base_code = lines[..line as usize - 1].join("\n");

    // 3. Extract target line's indentation so temp files preserve it
    let target_line = lines[(line - 1) as usize];
    let indent_len = target_line.find(|c: char| !c.is_whitespace()).unwrap_or(0);
    let indent = &target_line[..indent_len];

    // 4. Fire all run_code calls concurrently
    let futures: Vec<_> = snippets
        .iter()
        .map(|snippet| {
            run_snippet_isolated(
                client,
                project_path,
                snippet,
                &base_code,
                indent,
                timeout_per_snippet,
            )
        })
        .collect();

    let items = futures::future::join_all(futures).await;

    Ok(MultiAttemptResult { items })
}

// ---------------------------------------------------------------------------
// Public handler
// ---------------------------------------------------------------------------

/// Handle a `lean_multi_attempt` tool call.
///
/// Tries multiple tactic snippets at the given position without permanently
/// modifying the file.
///
/// When `parallel` is `Some(true)`, uses independent temp files for each
/// snippet (run_code semantics), enabling true concurrent execution.
/// Otherwise, uses the REPL fast path when available, falling back to
/// sequential LSP file edits.
///
/// `line` and `column` are **1-indexed** (matching the MCP tool interface).
///
/// `timeout_per_snippet` only applies in parallel mode. Each snippet's LSP
/// interaction is capped at this many seconds; timed-out snippets return
/// `timed_out: true` with a warning diagnostic.
#[allow(clippy::too_many_arguments)]
pub async fn handle_multi_attempt(
    client: &dyn LspClient,
    repl: Option<&mut Repl>,
    file_path: &str,
    line: u32,
    snippets: &[String],
    column: Option<u32>,
    parallel: Option<bool>,
    timeout_per_snippet: Option<f64>,
) -> Result<MultiAttemptResult, LeanToolError> {
    // Parallel path: use run_code semantics with independent temp files
    if parallel == Some(true) {
        let project_path = client.project_path().to_path_buf();
        return handle_multi_attempt_parallel(
            client,
            &project_path,
            file_path,
            line,
            snippets,
            timeout_per_snippet,
        )
        .await;
    }

    // Try REPL fast path first
    if let Some(result) = try_repl_path(client, repl, file_path, line, snippets, column).await? {
        return Ok(result);
    }

    // Fall back to LSP
    lsp_path(client, file_path, line, snippets, column).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// Mock LSP client for multi_attempt handler tests.
    ///
    /// Tracks file content mutations and provides canned diagnostics/goals.
    struct MockMultiAttemptClient {
        project: PathBuf,
        content: String,
        /// Current file content (mutated by update_file / update_file_content).
        current_content: Mutex<String>,
        /// Canned diagnostics response.
        diagnostics_response: Value,
        /// Canned goal responses keyed by (0-indexed line, 0-indexed col).
        goal_responses: Vec<((u32, u32), Option<Value>)>,
        /// Track whether open_file_force was called.
        force_reopen_called: Mutex<bool>,
    }

    impl MockMultiAttemptClient {
        fn new(content: &str) -> Self {
            Self {
                project: PathBuf::from("/test/project"),
                content: content.to_string(),
                current_content: Mutex::new(content.to_string()),
                diagnostics_response: json!({
                    "diagnostics": [],
                    "success": true
                }),
                goal_responses: Vec::new(),
                force_reopen_called: Mutex::new(false),
            }
        }

        fn with_diagnostics(mut self, diags: Vec<Value>) -> Self {
            self.diagnostics_response = json!({
                "diagnostics": diags,
                "success": true
            });
            self
        }

        fn with_goal(mut self, line: u32, col: u32, response: Option<Value>) -> Self {
            self.goal_responses.push(((line, col), response));
            self
        }
    }

    #[async_trait]
    impl LspClient for MockMultiAttemptClient {
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
            *self.force_reopen_called.lock().unwrap() = true;
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
            changes: Vec<Value>,
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            // Apply incremental changes so tests can verify restore correctness.
            let mut content = self.current_content.lock().unwrap().clone();
            for change in &changes {
                if let Some(range) = change.get("range") {
                    let start_line = range
                        .pointer("/start/line")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize;
                    let start_char = range
                        .pointer("/start/character")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize;
                    let end_line = range
                        .pointer("/end/line")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize;
                    let end_char = range
                        .pointer("/end/character")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize;
                    let new_text = change.get("text").and_then(Value::as_str).unwrap_or("");

                    // Convert line/char positions to byte offsets.
                    // When end_line is past the last line, clamp to content length.
                    let lines: Vec<&str> = content.split('\n').collect();
                    let line_offset = |l: usize, c: usize| -> usize {
                        if l >= lines.len() {
                            content.len()
                        } else {
                            lines[..l].iter().map(|s| s.len() + 1).sum::<usize>() + c
                        }
                    };
                    let start_offset = line_offset(start_line, start_char);
                    let end_offset = line_offset(end_line, end_char);

                    content = format!(
                        "{}{}{}",
                        &content[..start_offset],
                        new_text,
                        &content[end_offset..]
                    );
                } else if let Some(text) = change.get("text").and_then(Value::as_str) {
                    content = text.to_string();
                }
            }
            *self.current_content.lock().unwrap() = content;
            Ok(())
        }
        async fn update_file_content(
            &self,
            _p: &str,
            content: &str,
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            *self.current_content.lock().unwrap() = content.to_string();
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
            line: u32,
            column: u32,
        ) -> Result<Option<Value>, lean_lsp_client::client::LspClientError> {
            for ((l, c), resp) in &self.goal_responses {
                if *l == line && *c == column {
                    return Ok(resp.clone());
                }
            }
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

    // ---- LSP path: basic single snippet with goals ----

    #[tokio::test]
    async fn lsp_single_snippet_returns_goals() {
        let client = MockMultiAttemptClient::new("theorem foo : True := by\n  sorry\n  done")
            .with_goal(1, 6, Some(json!({"goals": ["|- True"]})));

        let snippets = vec!["simp".to_string()];
        let result =
            handle_multi_attempt(&client, None, "Main.lean", 2, &snippets, None, None, None)
                .await
                .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].snippet, "simp");
        assert_eq!(result.items[0].goals, vec!["|- True"]);
        assert!(result.items[0].diagnostics.is_empty());
    }

    // ---- LSP path: multiple snippets ----

    #[tokio::test]
    async fn lsp_multiple_snippets() {
        let client = MockMultiAttemptClient::new("theorem foo : True := by\n  sorry").with_goal(
            1,
            6,
            Some(json!({"goals": ["|- True"]})),
        );

        let snippets = vec!["simp".to_string(), "trivial".to_string()];
        let result =
            handle_multi_attempt(&client, None, "Main.lean", 2, &snippets, None, None, None)
                .await
                .unwrap();

        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].snippet, "simp");
        assert_eq!(result.items[1].snippet, "trivial");
    }

    // ---- LSP path: explicit column ----

    #[tokio::test]
    async fn lsp_explicit_column() {
        let client = MockMultiAttemptClient::new("theorem foo : True := by\n  sorry").with_goal(
            1,
            8,
            Some(json!({"goals": []})),
        );

        let snippets = vec!["simp".to_string()];
        let result = handle_multi_attempt(
            &client,
            None,
            "Main.lean",
            2,
            &snippets,
            Some(5),
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].goals, Vec::<String>::new());
    }

    // ---- LSP path: with diagnostics in range ----

    #[tokio::test]
    async fn lsp_with_diagnostics_in_range() {
        let client = MockMultiAttemptClient::new("theorem foo : True := by\n  sorry")
            .with_diagnostics(vec![json!({
                "range": {
                    "start": {"line": 1, "character": 2},
                    "end": {"line": 1, "character": 7}
                },
                "severity": 1,
                "message": "unknown tactic"
            })])
            .with_goal(1, 6, None);

        let snippets = vec!["bad_tactic".to_string()];
        let result =
            handle_multi_attempt(&client, None, "Main.lean", 2, &snippets, None, None, None)
                .await
                .unwrap();

        assert_eq!(result.items[0].diagnostics.len(), 1);
        assert_eq!(result.items[0].diagnostics[0].severity, "error");
        assert_eq!(result.items[0].diagnostics[0].message, "unknown tactic");
    }

    // ---- LSP path: diagnostics outside range are filtered ----

    #[tokio::test]
    async fn lsp_diagnostics_outside_range_filtered() {
        let client = MockMultiAttemptClient::new("import Lean\ntheorem foo : True := by\n  sorry")
            .with_diagnostics(vec![json!({
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 5}
                },
                "severity": 2,
                "message": "import warning"
            })])
            .with_goal(2, 6, None);

        let snippets = vec!["simp".to_string()];
        let result =
            handle_multi_attempt(&client, None, "Main.lean", 3, &snippets, None, None, None)
                .await
                .unwrap();

        assert!(result.items[0].diagnostics.is_empty());
    }

    // ---- LSP path: force reopen called after completion ----

    #[tokio::test]
    async fn lsp_force_reopen_called() {
        let client = MockMultiAttemptClient::new("theorem foo : True := by\n  sorry");

        let snippets = vec!["simp".to_string()];
        let _ =
            handle_multi_attempt(&client, None, "Main.lean", 2, &snippets, None, None, None).await;

        assert!(*client.force_reopen_called.lock().unwrap());
    }

    // ---- LSP path: content restored after snippets ----

    #[tokio::test]
    async fn lsp_content_restored() {
        let original = "theorem foo : True := by\n  sorry";
        let client = MockMultiAttemptClient::new(original);

        let snippets = vec!["simp".to_string()];
        let _ =
            handle_multi_attempt(&client, None, "Main.lean", 2, &snippets, None, None, None).await;

        let restored = client.current_content.lock().unwrap().clone();
        assert_eq!(restored, original);
    }

    // ---- line out of range ----

    #[tokio::test]
    async fn line_out_of_range() {
        let client = MockMultiAttemptClient::new("one line");

        let snippets = vec!["simp".to_string()];
        let err = handle_multi_attempt(&client, None, "Main.lean", 5, &snippets, None, None, None)
            .await
            .unwrap_err();

        match err {
            LeanToolError::LineOutOfRange { line, total } => {
                assert_eq!(line, 5);
                assert_eq!(total, 1);
            }
            other => panic!("expected LineOutOfRange, got: {other}"),
        }
    }

    // ---- column out of range ----

    #[tokio::test]
    async fn column_out_of_range() {
        let client = MockMultiAttemptClient::new("short");

        let snippets = vec!["simp".to_string()];
        let err = handle_multi_attempt(
            &client,
            None,
            "Main.lean",
            1,
            &snippets,
            Some(100),
            None,
            None,
        )
        .await
        .unwrap_err();

        match err {
            LeanToolError::ColumnOutOfRange { column, length } => {
                assert_eq!(column, 100);
                assert_eq!(length, 5);
            }
            other => panic!("expected ColumnOutOfRange, got: {other}"),
        }
    }

    // ---- resolve_column unit tests ----

    #[test]
    fn resolve_column_none_finds_first_non_ws() {
        assert_eq!(resolve_column("  simp", None).unwrap(), 2);
        assert_eq!(resolve_column("simp", None).unwrap(), 0);
        assert_eq!(resolve_column("    ", None).unwrap(), 0);
    }

    #[test]
    fn resolve_column_some_converts_to_0_indexed() {
        assert_eq!(resolve_column("  simp", Some(3)).unwrap(), 2);
        assert_eq!(resolve_column("  simp", Some(1)).unwrap(), 0);
    }

    #[test]
    fn resolve_column_zero_errors() {
        let err = resolve_column("simp", Some(0)).unwrap_err();
        match err {
            LeanToolError::ColumnOutOfRange { column, .. } => assert_eq!(column, 0),
            other => panic!("expected ColumnOutOfRange, got: {other}"),
        }
    }

    // ---- prepare_edit unit tests ----

    #[test]
    fn prepare_edit_single_line_snippet() {
        let (snippet_str, change, goal_line, goal_col) = prepare_edit("  sorry", 2, "simp", 3, 2);

        assert_eq!(snippet_str, "simp");
        assert_eq!(goal_line, 1);
        assert_eq!(goal_col, 6);
        assert_eq!(change["range"]["start"]["line"], 1);
        assert_eq!(change["range"]["start"]["character"], 2);
    }

    #[test]
    fn prepare_edit_multiline_snippet() {
        let (snippet_str, _change, goal_line, goal_col) =
            prepare_edit("  sorry", 2, "simp\nexact h", 3, 2);

        assert_eq!(snippet_str, "simp\nexact h");
        assert_eq!(goal_line, 2);
        assert_eq!(goal_col, 9);
    }

    #[test]
    fn prepare_edit_strips_trailing_newline() {
        let (snippet_str, _, _, _) = prepare_edit("  sorry", 2, "simp\n", 3, 2);
        assert_eq!(snippet_str, "simp");
    }

    // ---- prepare_restore_edit unit tests ----

    #[test]
    fn prepare_restore_edit_single_line_snippet() {
        // File: "theorem foo : True := by\n  sorry" (no trailing newline)
        let content = "theorem foo : True := by\n  sorry";
        let lines: Vec<&str> = content.lines().collect();

        let restore = prepare_restore_edit(content, &lines, 2, 2, "simp", lines.len());

        // The restore should undo replacing "sorry" with "simp" on line 2
        assert_eq!(restore["text"], "sorry");
        assert_eq!(restore["range"]["start"]["line"], 1);
        assert_eq!(restore["range"]["start"]["character"], 2);
        // payload "simp\n" is 1 line, so restore end = 1 + 1 = 2
        assert_eq!(restore["range"]["end"]["line"], 2);
        assert_eq!(restore["range"]["end"]["character"], 0);
    }

    #[test]
    fn prepare_restore_edit_multiline_snippet() {
        // File with trailing newline: 3 lines
        let content = "theorem foo : True := by\n  sorry\n  done\n";
        let lines: Vec<&str> = content.lines().collect();

        let restore = prepare_restore_edit(content, &lines, 2, 2, "simp\nexact h", lines.len());

        // Original text from (1,2) to (end_line_0,0):
        // snippet has 2 lines -> replaced_line_count=2
        // end_line_0 = min(1+2, 3) = 3
        // Original text from (1,2) to (3,0) = "sorry\n  done\n"
        assert_eq!(restore["text"], "sorry\n  done\n");
        assert_eq!(restore["range"]["start"]["line"], 1);
        assert_eq!(restore["range"]["start"]["character"], 2);
        // payload has 2 lines -> restore end = 1 + 2 = 3
        assert_eq!(restore["range"]["end"]["line"], 3);
        assert_eq!(restore["range"]["end"]["character"], 0);
    }

    #[test]
    fn prepare_restore_edit_roundtrip_no_trailing_newline() {
        // Verify that applying edit then restore produces identical content.
        let original = "theorem foo : True := by\n  sorry";
        let lines: Vec<&str> = original.lines().collect();
        let line_text = lines[1]; // "  sorry"
        let target_col = 2u32;
        let snippet = "simp";

        // Forward edit
        let (_snip, change, _gl, _gc) =
            prepare_edit(line_text, target_col, snippet, lines.len(), 2);

        // Apply forward edit to content
        let edited = apply_change(original, &change);

        // Restore edit
        let restore = prepare_restore_edit(original, &lines, 2, target_col, snippet, lines.len());

        // Apply restore to edited content
        let restored = apply_change(&edited, &restore);

        assert_eq!(
            restored, original,
            "roundtrip must produce identical content"
        );
    }

    #[test]
    fn prepare_restore_edit_roundtrip_with_trailing_newline() {
        let original = "theorem foo : True := by\n  sorry\n";
        let lines: Vec<&str> = original.lines().collect();
        let line_text = lines[1];
        let target_col = 2u32;
        let snippet = "simp";

        let (_snip, change, _gl, _gc) =
            prepare_edit(line_text, target_col, snippet, lines.len(), 2);
        let edited = apply_change(original, &change);
        let restore = prepare_restore_edit(original, &lines, 2, target_col, snippet, lines.len());
        let restored = apply_change(&edited, &restore);

        assert_eq!(
            restored, original,
            "roundtrip must produce identical content (trailing newline)"
        );
    }

    #[test]
    fn prepare_restore_edit_roundtrip_multiline_snippet() {
        let original = "theorem foo : True := by\n  sorry\n  done\n";
        let lines: Vec<&str> = original.lines().collect();
        let line_text = lines[1];
        let target_col = 2u32;
        let snippet = "simp\nexact h";

        let (_snip, change, _gl, _gc) =
            prepare_edit(line_text, target_col, snippet, lines.len(), 2);
        let edited = apply_change(original, &change);
        let restore = prepare_restore_edit(original, &lines, 2, target_col, snippet, lines.len());
        let restored = apply_change(&edited, &restore);

        assert_eq!(
            restored, original,
            "roundtrip must produce identical content (multiline snippet)"
        );
    }

    /// Apply a single LSP incremental change to content (test helper).
    fn apply_change(content: &str, change: &Value) -> String {
        let range = change.get("range").expect("change must have range");
        let start_line = range
            .pointer("/start/line")
            .and_then(Value::as_u64)
            .unwrap() as usize;
        let start_char = range
            .pointer("/start/character")
            .and_then(Value::as_u64)
            .unwrap() as usize;
        let end_line = range.pointer("/end/line").and_then(Value::as_u64).unwrap() as usize;
        let end_char = range
            .pointer("/end/character")
            .and_then(Value::as_u64)
            .unwrap() as usize;
        let new_text = change.get("text").and_then(Value::as_str).unwrap();

        let parts: Vec<&str> = content.split('\n').collect();
        let line_offset = |l: usize, c: usize| -> usize {
            if l >= parts.len() {
                content.len()
            } else {
                parts[..l].iter().map(|s| s.len() + 1).sum::<usize>() + c
            }
        };
        let start_offset = line_offset(start_line, start_char);
        let end_offset = line_offset(end_line, end_char);

        format!(
            "{}{}{}",
            &content[..start_offset],
            new_text,
            &content[end_offset..]
        )
    }

    // ---- filter_diagnostics_by_line_range unit tests ----

    #[test]
    fn filter_diagnostics_keeps_intersecting() {
        let diags = vec![
            json!({
                "range": {"start": {"line": 5}, "end": {"line": 7}},
                "severity": 1,
                "message": "in range"
            }),
            json!({
                "range": {"start": {"line": 0}, "end": {"line": 1}},
                "severity": 2,
                "message": "out of range"
            }),
        ];
        let filtered = filter_diagnostics_by_line_range(&diags, 3, 8);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["message"], "in range");
    }

    #[test]
    fn filter_diagnostics_empty_input() {
        let filtered = filter_diagnostics_by_line_range(&[], 0, 10);
        assert!(filtered.is_empty());
    }

    // ---- to_diagnostic_messages unit tests ----

    #[test]
    fn to_diagnostic_messages_converts_correctly() {
        let diags = vec![json!({
            "range": {"start": {"line": 4, "character": 2}, "end": {"line": 4, "character": 10}},
            "severity": 1,
            "message": "unknown id"
        })];
        let items = to_diagnostic_messages(&diags, 0);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].severity, "error");
        assert_eq!(items[0].line, 5);
        assert_eq!(items[0].column, 3);
    }

    #[test]
    fn to_diagnostic_messages_filters_context_lines() {
        // Simulate context file with 10 lines; diagnostics from context (line 3)
        // and snippet (line 12) regions.
        let diags = vec![
            json!({
                "range": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 5}},
                "severity": 1,
                "message": "context error"
            }),
            json!({
                "range": {"start": {"line": 12, "character": 4}, "end": {"line": 12, "character": 10}},
                "severity": 1,
                "message": "snippet error"
            }),
        ];

        let items = to_diagnostic_messages(&diags, 10);
        // Only the snippet diagnostic should remain.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].message, "snippet error");
        // Line 12 (0-indexed) - 10 context lines + 1 (1-indexed) = 3
        assert_eq!(items[0].line, 3);
        assert_eq!(items[0].column, 5);
    }

    #[test]
    fn to_diagnostic_messages_adjusts_line_numbers_with_context() {
        // Context has 5 lines; diagnostic at 0-indexed line 5 => snippet line 1.
        let diags = vec![json!({
            "range": {"start": {"line": 5, "character": 0}, "end": {"line": 5, "character": 5}},
            "severity": 2,
            "message": "warning in snippet"
        })];

        let items = to_diagnostic_messages(&diags, 5);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].line, 1);
        assert_eq!(items[0].severity, "warning");
    }

    // ---- REPL path: falls back to LSP when column specified ----

    #[tokio::test]
    async fn repl_skipped_when_column_specified() {
        let client = MockMultiAttemptClient::new("theorem foo : True := by\n  sorry").with_goal(
            1,
            6,
            Some(json!({"goals": []})),
        );

        let snippets = vec!["simp".to_string()];
        let result = handle_multi_attempt(
            &client,
            None,
            "Main.lean",
            2,
            &snippets,
            Some(3),
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.items.len(), 1);
    }

    // ---- REPL path: falls back when multiline snippet ----

    #[tokio::test]
    async fn repl_skipped_when_multiline_snippet() {
        let client =
            MockMultiAttemptClient::new("theorem foo : True := by\n  sorry").with_goal(1, 6, None);

        let snippets = vec!["simp\nexact h".to_string()];
        let result =
            handle_multi_attempt(&client, None, "Main.lean", 2, &snippets, None, None, None)
                .await
                .unwrap();

        assert_eq!(result.items.len(), 1);
    }

    // ---- empty snippets ----

    #[tokio::test]
    async fn empty_snippets_returns_empty() {
        let client = MockMultiAttemptClient::new("theorem foo : True := by\n  sorry");

        let snippets: Vec<String> = vec![];
        let result =
            handle_multi_attempt(&client, None, "Main.lean", 2, &snippets, None, None, None)
                .await
                .unwrap();

        assert!(result.items.is_empty());
    }

    // ========================================================================
    // Parallel path tests (8 tests)
    // ========================================================================

    /// Mock LSP client for parallel multi-attempt tests.
    ///
    /// Uses a real temp dir for project_path so temp files can be written/cleaned.
    struct MockParallelClient {
        project: PathBuf,
        content: String,
        diagnostics_response: Value,
        goal_responses: Vec<((u32, u32), Option<Value>)>,
        close_called: Mutex<Vec<String>>,
    }

    impl MockParallelClient {
        fn new(project: PathBuf, content: &str) -> Self {
            Self {
                project,
                content: content.to_string(),
                diagnostics_response: json!({
                    "diagnostics": [],
                    "success": true
                }),
                goal_responses: Vec::new(),
                close_called: Mutex::new(Vec::new()),
            }
        }

        fn with_diagnostics(mut self, diags: Vec<Value>) -> Self {
            self.diagnostics_response = json!({
                "diagnostics": diags,
                "success": true
            });
            self
        }

        fn with_goal(mut self, line: u32, col: u32, response: Option<Value>) -> Self {
            self.goal_responses.push(((line, col), response));
            self
        }
    }

    #[async_trait]
    impl LspClient for MockParallelClient {
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
            paths: &[String],
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            self.close_called
                .lock()
                .unwrap()
                .extend(paths.iter().cloned());
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
            line: u32,
            column: u32,
        ) -> Result<Option<Value>, lean_lsp_client::client::LspClientError> {
            for ((l, c), resp) in &self.goal_responses {
                if *l == line && *c == column {
                    return Ok(resp.clone());
                }
            }
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

    // ---- Parallel: single snippet returns results ----

    #[tokio::test]
    async fn parallel_single_snippet_returns_results() {
        let dir = tempfile::TempDir::new().unwrap();
        // goal queried at (1, 6) -- base has 1 line (0-indexed line 0),
        // snippet at line 1, col = indent(2) + len("simp")(4) = 6
        let client = MockParallelClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        )
        .with_goal(1, 6, Some(json!({"goals": ["|- True"]})));

        let snippets = vec!["simp".to_string()];
        let result =
            handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 2, &snippets, None)
                .await
                .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].snippet, "simp");
        assert_eq!(result.items[0].goals, vec!["|- True"]);
        assert!(result.items[0].diagnostics.is_empty());
    }

    // ---- Parallel: multiple snippets all complete ----

    #[tokio::test]
    async fn parallel_multiple_snippets() {
        let dir = tempfile::TempDir::new().unwrap();
        // goal columns: indent(2) + len("simp")(4) = 6, indent(2) + len("trivial")(7) = 9
        // base has 1 line, so snippet at line 1 (0-indexed)
        let client = MockParallelClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        )
        .with_goal(1, 6, Some(json!({"goals": ["|- True"]})))
        .with_goal(1, 9, Some(json!({"goals": []})));

        let snippets = vec!["simp".to_string(), "trivial".to_string()];
        let result =
            handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 2, &snippets, None)
                .await
                .unwrap();

        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].snippet, "simp");
        assert_eq!(result.items[1].snippet, "trivial");
    }

    // ---- Parallel: empty snippets returns empty ----

    #[tokio::test]
    async fn parallel_empty_snippets() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = MockParallelClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        );

        let snippets: Vec<String> = vec![];
        let result =
            handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 2, &snippets, None)
                .await
                .unwrap();

        assert!(result.items.is_empty());
    }

    // ---- Parallel: line out of range ----

    #[tokio::test]
    async fn parallel_line_out_of_range() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = MockParallelClient::new(dir.path().to_path_buf(), "one line");

        let snippets = vec!["simp".to_string()];
        let err =
            handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 5, &snippets, None)
                .await
                .unwrap_err();

        match err {
            LeanToolError::LineOutOfRange { line, total } => {
                assert_eq!(line, 5);
                assert_eq!(total, 1);
            }
            other => panic!("expected LineOutOfRange, got: {other}"),
        }
    }

    // ---- Parallel: temp files are cleaned up ----

    #[tokio::test]
    async fn parallel_temp_files_cleaned_up() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = MockParallelClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        );

        let snippets = vec!["simp".to_string(), "ring".to_string()];
        let _ = handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 2, &snippets, None)
            .await
            .unwrap();

        // Check .lake/_mcp/ directory for leftover temp files.
        let mcp_dir = dir.path().join(".lake").join("_mcp");
        if mcp_dir.exists() {
            let remaining: Vec<_> = std::fs::read_dir(&mcp_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("_mcp_attempt_"))
                .collect();
            assert!(remaining.is_empty(), "temp files were not cleaned up");
        }
        // Also verify no temp files in project root.
        let root_remaining: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("_mcp_attempt_"))
            .collect();
        assert!(
            root_remaining.is_empty(),
            "temp files should not be in project root"
        );
    }

    // ---- Parallel: close_files called for each snippet ----

    #[tokio::test]
    async fn parallel_close_files_called() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = MockParallelClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        );

        let snippets = vec!["simp".to_string(), "ring".to_string()];
        let _ = handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 2, &snippets, None)
            .await
            .unwrap();

        let closed = client.close_called.lock().unwrap();
        assert_eq!(
            closed.len(),
            2,
            "close_files should be called for each snippet"
        );
        for path in closed.iter() {
            assert!(
                path.starts_with(".lake/_mcp/_mcp_attempt_"),
                "closed path should be a temp file in .lake/_mcp/: {path}"
            );
        }
    }

    // ---- Parallel: temp files are in .lake/_mcp/ not project root ----

    #[tokio::test]
    async fn parallel_temp_files_in_lake_mcp_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = MockParallelClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        );

        let snippets = vec!["simp".to_string()];
        let _ = handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 2, &snippets, None)
            .await
            .unwrap();

        // The .lake/_mcp/ directory should have been created (even if files are cleaned up)
        let mcp_dir = dir.path().join(".lake").join("_mcp");
        assert!(
            mcp_dir.exists(),
            ".lake/_mcp/ directory should be created for temp files"
        );

        // Verify no temp files leaked to project root
        let root_remaining: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("_mcp_attempt_"))
            .collect();
        assert!(
            root_remaining.is_empty(),
            "temp files should not be in project root"
        );

        // Verify close_files paths have the new prefix
        let closed = client.close_called.lock().unwrap();
        for path in closed.iter() {
            assert!(
                path.starts_with(".lake/_mcp/"),
                "closed path should be in .lake/_mcp/: {path}"
            );
        }
    }

    // ---- Parallel: diagnostics in snippet range are captured ----

    #[tokio::test]
    async fn parallel_diagnostics_captured() {
        let dir = tempfile::TempDir::new().unwrap();
        // Diagnostic at line 1 (0-indexed): base has 1 line, snippet at line 1
        let client = MockParallelClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        )
        .with_diagnostics(vec![json!({
            "range": {
                "start": {"line": 1, "character": 0},
                "end": {"line": 1, "character": 10}
            },
            "severity": 1,
            "message": "unknown tactic 'bad'"
        })]);

        let snippets = vec!["bad".to_string()];
        let result =
            handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 2, &snippets, None)
                .await
                .unwrap();

        assert_eq!(result.items[0].diagnostics.len(), 1);
        assert_eq!(result.items[0].diagnostics[0].severity, "error");
        assert_eq!(
            result.items[0].diagnostics[0].message,
            "unknown tactic 'bad'"
        );
    }

    // ---- Parallel: diagnostics outside snippet range are filtered ----

    #[tokio::test]
    async fn parallel_diagnostics_outside_range_filtered() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = MockParallelClient::new(
            dir.path().to_path_buf(),
            "import Lean\ntheorem foo : True := by\n  sorry",
        )
        .with_diagnostics(vec![json!({
            "range": {
                "start": {"line": 0, "character": 0},
                "end": {"line": 0, "character": 5}
            },
            "severity": 2,
            "message": "import warning"
        })]);

        let snippets = vec!["simp".to_string()];
        let result =
            handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 3, &snippets, None)
                .await
                .unwrap();

        assert!(
            result.items[0].diagnostics.is_empty(),
            "diagnostics outside snippet range should be filtered"
        );
    }

    // ---- Regression #148: context-region errors must not cause false positives ----

    /// Regression test for #148: a tactic that produces an error diagnostic
    /// with a line number in the context region (e.g., a unification error
    /// referencing the theorem header) must NOT be treated as a success.
    ///
    /// The old code used `filter_diagnostics_by_line_range` which only kept
    /// diagnostics physically within the snippet line range, discarding errors
    /// whose line fell in the context. The fix uses `to_diagnostic_messages`
    /// with `context_lines` which keeps all diagnostics from the snippet
    /// region onward.
    #[tokio::test]
    async fn parallel_context_region_error_not_false_positive() {
        let dir = tempfile::TempDir::new().unwrap();
        // Simulates a scenario like:
        //   variable [Field K]
        //   theorem foo (x : K) : x = x := by
        //     sorry   <-- target line 3
        //
        // When "omega" is tried, the error diagnostic might have its line
        // number at line 1 (the theorem header in the context region), not
        // at the snippet line. The old filter would miss this error.
        //
        // File content: 2 lines of context + sorry at line 3.
        // base_code = lines[0..2] = "variable [Field K]\ntheorem foo (x : K) : x = x := by"
        // context_lines = 2
        //
        // Error diagnostic at line 1 (0-indexed, in context region) would be
        // filtered by context_lines >= 2. But an error at the snippet line
        // (line 2, 0-indexed) should be kept.
        //
        // The critical case: an error at the sorry/snippet boundary line
        // that was previously missed by the tight line range filter.
        let client = MockParallelClient::new(
            dir.path().to_path_buf(),
            "variable [Field K]\ntheorem foo (x : K) : x = x := by\n  sorry",
        )
        .with_diagnostics(vec![
            // Error in context region (should be filtered)
            json!({
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": 0, "character": 10}
                },
                "severity": 2,
                "message": "unused variable warning"
            }),
            // Error at snippet line (should be kept) - the tactic itself fails
            json!({
                "range": {
                    "start": {"line": 2, "character": 2},
                    "end": {"line": 2, "character": 7}
                },
                "severity": 1,
                "message": "omega failed to prove the goal"
            }),
            // Error after snippet (sorry line - should also be kept)
            json!({
                "range": {
                    "start": {"line": 3, "character": 2},
                    "end": {"line": 3, "character": 7}
                },
                "severity": 2,
                "message": "declaration uses sorry"
            }),
        ]);

        let snippets = vec!["omega".to_string()];
        let result =
            handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 3, &snippets, None)
                .await
                .unwrap();

        // The snippet error MUST be captured - it should NOT be a false positive.
        assert!(
            result.items[0]
                .diagnostics
                .iter()
                .any(|d| d.severity == "error"),
            "tactic error must be captured, not filtered as false positive; diagnostics: {:?}",
            result.items[0].diagnostics
        );
        // The context-region warning should be filtered out.
        assert!(
            !result.items[0]
                .diagnostics
                .iter()
                .any(|d| d.message == "unused variable warning"),
            "context-region diagnostics should be filtered out"
        );
        // Snippet-region diagnostics should be present (both the error and the sorry warning).
        assert_eq!(
            result.items[0].diagnostics.len(),
            2,
            "should have snippet error + sorry warning"
        );
    }

    /// Regression test for #148: verify that without maxHeartbeats injection,
    /// line numbers in the temp file match the original file structure (no
    /// +1 offset from a prepended set_option line).
    #[tokio::test]
    async fn parallel_no_heartbeats_line_offset() {
        let dir = tempfile::TempDir::new().unwrap();
        // File: line 0 = "import Mathlib", line 1 = "theorem ...", line 2 = "  sorry"
        // Target line 3 (1-indexed). base_code = "import Mathlib\ntheorem ..." (2 lines).
        // Without maxHeartbeats, snippet is at line 2 (0-indexed).
        // With old maxHeartbeats, it would be at line 3 (0-indexed).
        let client = MockParallelClient::new(
            dir.path().to_path_buf(),
            "import Mathlib\ntheorem foo : True := by\n  sorry",
        )
        // Goal at line 2 (base=2, snippet=1 line, goal_line = 2+1-1 = 2)
        .with_goal(2, 6, Some(json!({"goals": ["|- True"]})));

        let snippets = vec!["simp".to_string()];
        let result =
            handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 3, &snippets, None)
                .await
                .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(
            result.items[0].goals,
            vec!["|- True"],
            "goal should be found at line 2 (no maxHeartbeats offset)"
        );
    }

    // ---- handle_multi_attempt dispatches to parallel when flag set ----

    #[tokio::test]
    async fn handle_multi_attempt_dispatches_parallel() {
        let dir = tempfile::TempDir::new().unwrap();
        // goal column: indent(2) + len("simp")(4) = 6, base has 1 line so snippet at line 1
        let client = MockParallelClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        )
        .with_goal(1, 6, Some(json!({"goals": ["|- True"]})));

        let snippets = vec!["simp".to_string()];
        let result = handle_multi_attempt(
            &client,
            None,
            "Main.lean",
            2,
            &snippets,
            None,
            Some(true),
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].snippet, "simp");
    }

    // ---- parallel=false uses default (LSP) path ----

    #[tokio::test]
    async fn handle_multi_attempt_parallel_false_uses_lsp() {
        let client = MockMultiAttemptClient::new("theorem foo : True := by\n  sorry").with_goal(
            1,
            6,
            Some(json!({"goals": ["|- True"]})),
        );

        let snippets = vec!["simp".to_string()];
        let result = handle_multi_attempt(
            &client,
            None,
            "Main.lean",
            2,
            &snippets,
            None,
            Some(false),
            None,
        )
        .await
        .unwrap();

        assert_eq!(result.items.len(), 1);
        // LSP path used -> force reopen should be called
        assert!(*client.force_reopen_called.lock().unwrap());
    }

    // ---- Parallel: snippet with trailing newline is trimmed ----

    #[tokio::test]
    async fn parallel_trailing_newline_trimmed() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = MockParallelClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        );

        let snippets = vec!["simp\n".to_string()];
        let result =
            handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 2, &snippets, None)
                .await
                .unwrap();

        assert_eq!(result.items[0].snippet, "simp");
    }

    // ========================================================================
    // Parallel indentation tests (Closes #87)
    // ========================================================================

    // ---- Parallel: preserves indentation — goal queried at correct column ----

    #[tokio::test]
    async fn parallel_preserves_indentation_goal_column() {
        // File has 2-space indented sorry at line 3.
        // Prior tactic "intro h" at line 2.
        // The snippet "simp" should be written as "  simp" in the temp file,
        // so goal_column = indent(2) + len("simp")(4) = 6.
        // Base has 2 lines -> goal line 2 (0-indexed).
        let dir = tempfile::TempDir::new().unwrap();
        let client = MockParallelClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  intro h\n  sorry",
        )
        .with_goal(2, 6, Some(json!({"goals": ["h : True\n|- True"]})));

        let snippets = vec!["simp".to_string()];
        let result =
            handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 3, &snippets, None)
                .await
                .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(
            result.items[0].goals,
            vec!["h : True\n|- True"],
            "goal should be found at indented column (2 + 4 = 6)"
        );
    }

    // ---- Parallel: indentation with empty base_code ----

    #[tokio::test]
    async fn parallel_indentation_empty_base_code() {
        // Edge case: sorry is the FIRST line (line 1), so base_code is empty.
        // 4-space indent on the sorry line.
        // No base lines -> snippet at line 0 (0-indexed).
        let dir = tempfile::TempDir::new().unwrap();
        let client = MockParallelClient::new(dir.path().to_path_buf(), "    sorry")
            // goal_column = 4 (indent) + 4 ("simp") = 8
            .with_goal(0, 8, Some(json!({"goals": ["|- Nat"]})));

        let snippets = vec!["simp".to_string()];
        let result =
            handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 1, &snippets, None)
                .await
                .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(
            result.items[0].goals,
            vec!["|- Nat"],
            "goal should be found at indented column even with empty base_code"
        );
    }

    // ---- Parallel: multi-line snippet gets each line indented ----

    #[tokio::test]
    async fn parallel_multiline_snippet_indented() {
        // A multi-line snippet "simp\nexact h" with 2-space indent should become:
        //   "  simp\n  exact h" in the temp file.
        // The goal is queried at the LAST line of the snippet.
        // Last snippet line = "exact h" (len 7), indent = 2
        // goal_column = 2 + 7 = 9
        let dir = tempfile::TempDir::new().unwrap();
        let client = MockParallelClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        )
        // base has 1 line, multi-line snippet has 2 lines.
        // goal_line = 1 + 2 - 1 = 2, goal_column = 2 + 7 = 9
        .with_goal(2, 9, Some(json!({"goals": ["|- False"]})));

        let snippets = vec!["simp\nexact h".to_string()];
        let result =
            handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 2, &snippets, None)
                .await
                .unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(
            result.items[0].goals,
            vec!["|- False"],
            "multi-line snippet should have each line indented, goal at indent + last_line_len"
        );
    }

    // ========================================================================
    // Regression tests for #90: parallel cold file open
    // ========================================================================

    /// Mock LSP client that tracks whether `open_file` is called on the
    /// *source* file (not temp files). Simulates a cold LSP where
    /// `get_file_content` fails unless `open_file` was called first.
    struct MockColdFileClient {
        project: PathBuf,
        content: String,
        /// Tracks all paths passed to `open_file`.
        open_file_calls: Mutex<Vec<String>>,
        diagnostics_response: Value,
        goal_responses: Vec<((u32, u32), Option<Value>)>,
        close_called: Mutex<Vec<String>>,
    }

    impl MockColdFileClient {
        fn new(project: PathBuf, content: &str) -> Self {
            Self {
                project,
                content: content.to_string(),
                open_file_calls: Mutex::new(Vec::new()),
                diagnostics_response: json!({
                    "diagnostics": [],
                    "success": true
                }),
                goal_responses: Vec::new(),
                close_called: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl LspClient for MockColdFileClient {
        fn project_path(&self) -> &Path {
            &self.project
        }
        async fn open_file(&self, p: &str) -> Result<(), lean_lsp_client::client::LspClientError> {
            self.open_file_calls.lock().unwrap().push(p.to_string());
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
            p: &str,
        ) -> Result<String, lean_lsp_client::client::LspClientError> {
            // Simulate cold LSP: if the file hasn't been opened, fail.
            // Temp files (in .lake/_mcp/_mcp_attempt_) are always "open".
            if !p.starts_with(".lake/_mcp/_mcp_attempt_") {
                let calls = self.open_file_calls.lock().unwrap();
                if !calls.contains(&p.to_string()) {
                    return Err(lean_lsp_client::client::LspClientError::FileNotOpen(
                        p.to_string(),
                    ));
                }
            }
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
            paths: &[String],
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            self.close_called
                .lock()
                .unwrap()
                .extend(paths.iter().cloned());
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
            line: u32,
            column: u32,
        ) -> Result<Option<Value>, lean_lsp_client::client::LspClientError> {
            for ((l, c), resp) in &self.goal_responses {
                if *l == line && *c == column {
                    return Ok(resp.clone());
                }
            }
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

    /// Regression test for #90: parallel multi_attempt must call open_file
    /// on the source file before calling get_file_content, so that cold
    /// (never-previously-opened) files work correctly.
    #[tokio::test]
    async fn regression_parallel_cold_file_open() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = MockColdFileClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        );

        let snippets = vec!["simp".to_string()];
        // This would fail with "File not open" before the fix
        let result =
            handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 2, &snippets, None)
                .await;

        // Must succeed (not error with "File not open")
        let result =
            result.expect("parallel multi_attempt should open file before reading content");
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].snippet, "simp");

        // Verify open_file was called with the source file path
        let calls = client.open_file_calls.lock().unwrap();
        assert!(
            calls.contains(&"Main.lean".to_string()),
            "open_file must be called on the source file; calls were: {calls:?}"
        );
    }

    /// Mock that errors on open_file for the source file.
    struct MockOpenFileErrorClient {
        project: PathBuf,
    }

    #[async_trait]
    impl LspClient for MockOpenFileErrorClient {
        fn project_path(&self) -> &Path {
            &self.project
        }
        async fn open_file(&self, p: &str) -> Result<(), lean_lsp_client::client::LspClientError> {
            // Temp files succeed, source file fails
            if !p.starts_with(".lake/_mcp/_mcp_attempt_") {
                return Err(lean_lsp_client::client::LspClientError::FileNotOpen(
                    format!("Cannot open file: {p}"),
                ));
            }
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
            Ok("theorem foo : True := by\n  sorry".to_string())
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
            Ok(json!({"diagnostics": [], "success": true}))
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

    /// Test that when open_file fails, the error propagates correctly
    /// as a LeanToolError::LspError with operation "open_file".
    #[tokio::test]
    async fn parallel_open_file_error_propagates() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = MockOpenFileErrorClient {
            project: dir.path().to_path_buf(),
        };

        let snippets = vec!["simp".to_string()];
        let err =
            handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 2, &snippets, None)
                .await
                .unwrap_err();

        match err {
            LeanToolError::LspError {
                operation, message, ..
            } => {
                assert_eq!(operation, "open_file");
                assert!(
                    message.contains("Cannot open file"),
                    "error message should contain the original error: {message}"
                );
            }
            other => panic!("expected LspError with operation='open_file', got: {other}"),
        }
    }

    // ---- Parallel: sorry line uses target line indent (not hardcoded) ----

    #[tokio::test]
    async fn parallel_sorry_uses_target_indent() {
        // With 4-space indent, the sorry line should be "    sorry" not "  sorry".
        // We verify by checking diagnostics: the sorry line is at
        // base_line_count + snippet_line_count = 1 + 1 = line 2 (0-indexed).
        let dir = tempfile::TempDir::new().unwrap();
        let client = MockParallelClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n    sorry",
        )
        .with_diagnostics(vec![json!({
            "range": {
                "start": {"line": 2, "character": 0},
                "end": {"line": 2, "character": 9}
            },
            "severity": 2,
            "message": "declaration uses sorry"
        })])
        // goal at (1, 8): base has 1 line, indent(4) + len("simp")(4) = 8
        .with_goal(1, 8, Some(json!({"goals": ["|- True"]})));

        let snippets = vec!["simp".to_string()];
        let result =
            handle_multi_attempt_parallel(&client, dir.path(), "Main.lean", 2, &snippets, None)
                .await
                .unwrap();

        // The sorry line diagnostic at line 2 should be captured (it's within range)
        assert_eq!(result.items[0].diagnostics.len(), 1);
        assert_eq!(
            result.items[0].diagnostics[0].message,
            "declaration uses sorry"
        );
        // And the goal should be found at the correct indented position
        assert_eq!(result.items[0].goals, vec!["|- True"]);
    }

    // ========================================================================
    // Timeout tests (Closes #91)
    // ========================================================================

    /// Mock LSP client with configurable delay in get_diagnostics.
    ///
    /// When `diagnostics_delay` is set, `get_diagnostics` will sleep for
    /// that duration before returning, enabling timeout testing.
    struct MockTimeoutClient {
        project: PathBuf,
        content: String,
        diagnostics_response: Value,
        diagnostics_delay: Option<std::time::Duration>,
        goal_responses: Vec<((u32, u32), Option<Value>)>,
        close_called: Mutex<Vec<String>>,
    }

    impl MockTimeoutClient {
        fn new(project: PathBuf, content: &str) -> Self {
            Self {
                project,
                content: content.to_string(),
                diagnostics_response: json!({
                    "diagnostics": [],
                    "success": true
                }),
                diagnostics_delay: None,
                goal_responses: Vec::new(),
                close_called: Mutex::new(Vec::new()),
            }
        }

        fn with_diagnostics_delay(mut self, delay: std::time::Duration) -> Self {
            self.diagnostics_delay = Some(delay);
            self
        }

        fn with_goal(mut self, line: u32, col: u32, response: Option<Value>) -> Self {
            self.goal_responses.push(((line, col), response));
            self
        }
    }

    #[async_trait]
    impl LspClient for MockTimeoutClient {
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
            paths: &[String],
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            self.close_called
                .lock()
                .unwrap()
                .extend(paths.iter().cloned());
            Ok(())
        }
        async fn get_diagnostics(
            &self,
            _p: &str,
            _sl: Option<u32>,
            _el: Option<u32>,
            _t: Option<f64>,
        ) -> Result<Value, lean_lsp_client::client::LspClientError> {
            if let Some(delay) = self.diagnostics_delay {
                tokio::time::sleep(delay).await;
            }
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
            line: u32,
            column: u32,
        ) -> Result<Option<Value>, lean_lsp_client::client::LspClientError> {
            for ((l, c), resp) in &self.goal_responses {
                if *l == line && *c == column {
                    return Ok(resp.clone());
                }
            }
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

    // ---- Timeout: snippet exceeding timeout returns timed_out result ----

    #[tokio::test]
    async fn parallel_timeout_returns_timed_out_result() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = MockTimeoutClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        )
        .with_diagnostics_delay(std::time::Duration::from_secs(5));

        let snippets = vec!["exact?".to_string()];
        let result = handle_multi_attempt_parallel(
            &client,
            dir.path(),
            "Main.lean",
            2,
            &snippets,
            Some(0.1), // 100ms timeout, but diagnostics takes 5s
        )
        .await
        .unwrap();

        assert_eq!(result.items.len(), 1);
        assert!(
            result.items[0].timed_out,
            "snippet should be marked timed_out"
        );
        assert!(
            result.items[0].goals.is_empty(),
            "timed out should have no goals"
        );
        assert_eq!(result.items[0].diagnostics.len(), 1);
        assert_eq!(result.items[0].diagnostics[0].severity, "warning");
        assert!(
            result.items[0].diagnostics[0].message.contains("timed out"),
            "diagnostic should mention timeout"
        );
        assert_eq!(result.items[0].snippet, "exact?");
    }

    // ---- Timeout: temp files cleaned up even on timeout ----

    #[tokio::test]
    async fn parallel_timeout_cleans_up_temp_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = MockTimeoutClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        )
        .with_diagnostics_delay(std::time::Duration::from_secs(5));

        let snippets = vec!["exact?".to_string(), "simp?".to_string()];
        let _ = handle_multi_attempt_parallel(
            &client,
            dir.path(),
            "Main.lean",
            2,
            &snippets,
            Some(0.1),
        )
        .await
        .unwrap();

        // Verify temp files were cleaned up from .lake/_mcp/
        let mcp_dir = dir.path().join(".lake").join("_mcp");
        if mcp_dir.exists() {
            let remaining: Vec<_> = std::fs::read_dir(&mcp_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("_mcp_attempt_"))
                .collect();
            assert!(
                remaining.is_empty(),
                "temp files should be cleaned up even on timeout"
            );
        }

        // Verify close_files was called
        let closed = client.close_called.lock().unwrap();
        assert_eq!(
            closed.len(),
            2,
            "close_files should be called for each snippet even on timeout"
        );
    }

    // ---- Timeout: None timeout works normally (existing behavior) ----

    #[tokio::test]
    async fn parallel_no_timeout_works_normally() {
        let dir = tempfile::TempDir::new().unwrap();
        // base has 1 line, snippet at line 1 (0-indexed)
        let client = MockTimeoutClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        )
        .with_goal(1, 6, Some(json!({"goals": ["|- True"]})));

        let snippets = vec!["simp".to_string()];
        let result = handle_multi_attempt_parallel(
            &client,
            dir.path(),
            "Main.lean",
            2,
            &snippets,
            None, // No timeout
        )
        .await
        .unwrap();

        assert_eq!(result.items.len(), 1);
        assert!(!result.items[0].timed_out, "should not be timed out");
        assert_eq!(result.items[0].goals, vec!["|- True"]);
        assert_eq!(result.items[0].snippet, "simp");
    }

    // ---- Timeout: fast snippet completes, slow snippet times out ----

    #[tokio::test]
    async fn parallel_fast_snippet_completes_slow_times_out() {
        let dir = tempfile::TempDir::new().unwrap();
        // Use MockTimeoutClient with a delay shorter than the timeout for the "fast" case.
        // We cannot make some snippets fast and others slow with this mock,
        // because delay applies to all get_diagnostics calls. Instead, we test
        // the mixed scenario by creating two separate calls and checking results.
        //
        // For the "fast" test, no delay:
        // base has 1 line, snippet at line 1 (0-indexed)
        let fast_client = MockTimeoutClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        )
        .with_goal(1, 6, Some(json!({"goals": ["|- True"]})));

        let fast_snippets = vec!["simp".to_string()];
        let fast_result = handle_multi_attempt_parallel(
            &fast_client,
            dir.path(),
            "Main.lean",
            2,
            &fast_snippets,
            Some(1.0), // 1s timeout, no delay -> should complete
        )
        .await
        .unwrap();

        assert_eq!(fast_result.items.len(), 1);
        assert!(
            !fast_result.items[0].timed_out,
            "fast snippet should complete"
        );
        assert_eq!(fast_result.items[0].goals, vec!["|- True"]);

        // For the "slow" test, 5s delay with 100ms timeout:
        let slow_client = MockTimeoutClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        )
        .with_diagnostics_delay(std::time::Duration::from_secs(5));

        let slow_snippets = vec!["exact?".to_string()];
        let slow_result = handle_multi_attempt_parallel(
            &slow_client,
            dir.path(),
            "Main.lean",
            2,
            &slow_snippets,
            Some(0.1), // 100ms timeout, but takes 5s
        )
        .await
        .unwrap();

        assert_eq!(slow_result.items.len(), 1);
        assert!(
            slow_result.items[0].timed_out,
            "slow snippet should time out"
        );
        assert!(slow_result.items[0].goals.is_empty());
    }

    // ---- Timeout: timed_out field not present in non-timeout results ----

    #[tokio::test]
    async fn parallel_timed_out_not_in_normal_json() {
        let dir = tempfile::TempDir::new().unwrap();
        // base has 1 line, snippet at line 1 (0-indexed)
        let client = MockTimeoutClient::new(
            dir.path().to_path_buf(),
            "theorem foo : True := by\n  sorry",
        )
        .with_goal(1, 6, Some(json!({"goals": ["|- True"]})));

        let snippets = vec!["simp".to_string()];
        let result = handle_multi_attempt_parallel(
            &client,
            dir.path(),
            "Main.lean",
            2,
            &snippets,
            Some(5.0), // generous timeout
        )
        .await
        .unwrap();

        // Serialize to JSON and verify timed_out is omitted for non-timeout result
        let json_val = serde_json::to_value(&result.items[0]).unwrap();
        assert!(
            !json_val.as_object().unwrap().contains_key("timed_out"),
            "timed_out=false should be omitted from serialized JSON"
        );
    }

    // ========================================================================
    // run_one_snippet_warm tests (Closes #100)
    // ========================================================================

    // ---- run_one_snippet_warm: returns goals ----

    #[tokio::test]
    async fn run_one_snippet_warm_returns_goals() {
        let client = MockMultiAttemptClient::new("theorem foo : True := by\n  sorry").with_goal(
            1,
            6,
            Some(json!({"goals": ["|- True"]})),
        );

        let result = run_one_snippet_warm(
            &client,
            "Main.lean",
            "theorem foo : True := by\n  sorry",
            2, // line (1-indexed)
            2, // target_col (0-indexed: first non-ws on "  sorry")
            "simp",
            None,
        )
        .await;

        assert_eq!(result.snippet, "simp");
        assert_eq!(result.goals, vec!["|- True"]);
        assert!(result.diagnostics.is_empty());
        assert!(!result.timed_out);
    }

    // ---- run_one_snippet_warm: restores original content ----

    #[tokio::test]
    async fn run_one_snippet_warm_restores_content() {
        let original = "theorem foo : True := by\n  sorry";
        let client = MockMultiAttemptClient::new(original);

        let _ = run_one_snippet_warm(&client, "Main.lean", original, 2, 2, "simp", None).await;

        let restored = client.current_content.lock().unwrap().clone();
        assert_eq!(restored, original, "original content must be restored");
    }

    // ---- run_one_snippet_warm: timeout produces timed_out result ----

    #[tokio::test]
    async fn run_one_snippet_warm_timeout() {
        let client = MockTimeoutClient::new(
            PathBuf::from("/test/project"),
            "theorem foo : True := by\n  sorry",
        )
        .with_diagnostics_delay(std::time::Duration::from_secs(5));

        let result = run_one_snippet_warm(
            &client,
            "Main.lean",
            "theorem foo : True := by\n  sorry",
            2,
            2,
            "exact?",
            Some(0.1), // 100ms timeout, diagnostics takes 5s
        )
        .await;

        assert!(result.timed_out, "snippet should be marked timed_out");
        assert!(result.goals.is_empty());
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].severity, "warning");
        assert!(result.diagnostics[0].message.contains("timed out"));
        assert_eq!(result.snippet, "exact?");
    }

    // ---- run_one_snippet_warm: error returns diagnostic (not panic) ----

    /// Mock client where update_file fails.
    struct MockUpdateFileErrorClient {
        project: PathBuf,
        current_content: Mutex<String>,
    }

    impl MockUpdateFileErrorClient {
        fn new() -> Self {
            Self {
                project: PathBuf::from("/test/project"),
                current_content: Mutex::new(String::new()),
            }
        }
    }

    #[async_trait]
    impl LspClient for MockUpdateFileErrorClient {
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
            Ok("theorem foo : True := by\n  sorry".to_string())
        }
        async fn update_file(
            &self,
            _p: &str,
            _c: Vec<Value>,
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            Err(lean_lsp_client::client::LspClientError::FileNotOpen(
                "update_file failed".to_string(),
            ))
        }
        async fn update_file_content(
            &self,
            _p: &str,
            content: &str,
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            *self.current_content.lock().unwrap() = content.to_string();
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
            Ok(json!({"diagnostics": [], "success": true}))
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
    async fn run_one_snippet_warm_error_returns_diagnostic() {
        let client = MockUpdateFileErrorClient::new();

        let result = run_one_snippet_warm(
            &client,
            "Main.lean",
            "theorem foo : True := by\n  sorry",
            2,
            2,
            "simp",
            None,
        )
        .await;

        // Should not panic; error captured as diagnostic
        assert_eq!(result.snippet, "simp");
        assert!(result.goals.is_empty());
        assert!(!result.diagnostics.is_empty());
        assert_eq!(result.diagnostics[0].severity, "error");
        assert!(
            result.diagnostics[0].message.contains("update_file"),
            "error message should mention the failing operation: {}",
            result.diagnostics[0].message
        );
        assert!(!result.timed_out);
    }

    // ---- run_one_snippet_warm: content restored even on error ----

    #[tokio::test]
    async fn run_one_snippet_warm_restores_on_error() {
        let original = "theorem foo : True := by\n  sorry";
        let client = MockUpdateFileErrorClient::new();

        let _ = run_one_snippet_warm(&client, "Main.lean", original, 2, 2, "simp", None).await;

        let restored = client.current_content.lock().unwrap().clone();
        assert_eq!(
            restored, original,
            "original content must be restored even on error"
        );
    }
}
