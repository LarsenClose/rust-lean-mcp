//! Tool handler for `lean_build`.
//!
//! Runs `lake build` (with optional clean + cache fetch), captures output,
//! and parses progress patterns and errors from the build log.

use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::models::BuildResult;
use regex::Regex;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;

/// Default number of trailing output lines to include in the result.
const DEFAULT_OUTPUT_LINES: usize = 20;

/// Parse `[N/M]` progress patterns from build output.
///
/// Returns a vec of `(completed, total)` pairs extracted from lines matching
/// the `[N/M]` pattern emitted by `lake build --verbose`.
fn parse_progress(output: &str) -> Vec<(u64, u64)> {
    let re = Regex::new(r"\[(\d+)/(\d+)\]").expect("valid regex");
    re.captures_iter(output)
        .filter_map(|cap| {
            let n: u64 = cap[1].parse().ok()?;
            let m: u64 = cap[2].parse().ok()?;
            Some((n, m))
        })
        .collect()
}

/// Collect lines containing "error" (case-insensitive) from build output.
fn collect_errors(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| line.to_lowercase().contains("error"))
        .map(|line| line.trim().to_string())
        .collect()
}

/// Truncate output to the last `n` lines. If `n == 0`, returns an empty string.
fn truncate_output(output: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= n {
        output.to_string()
    } else {
        lines[lines.len() - n..].join("\n")
    }
}

/// Run a subprocess command and return `(exit_success, combined_output)`.
async fn run_command(program: &str, args: &[&str], cwd: &Path) -> Result<(bool, String), String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to run `{program}`: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = if stderr.is_empty() {
        stdout.to_string()
    } else if stdout.is_empty() {
        stderr.to_string()
    } else {
        format!("{stdout}\n{stderr}")
    };

    Ok((output.status.success(), combined))
}

/// Handle a `lean_build` tool call.
///
/// Steps:
/// 1. If `clean` is true, run `lake clean`.
/// 2. Run `lake exe cache get` to fetch cached artifacts.
/// 3. Run `lake build --verbose`, capturing stdout/stderr.
/// 4. Parse progress from `[N/M]` patterns (stored for future progress reporting).
/// 5. Collect error lines from build output.
/// 6. Return [`BuildResult`] with the last `output_lines` of the log.
pub async fn handle_build(
    project_path: &Path,
    clean: bool,
    output_lines: usize,
) -> Result<BuildResult, LeanToolError> {
    let mut full_log = String::new();

    // 1. Optionally run `lake clean`.
    if clean {
        let (ok, out) = run_command("lake", &["clean"], project_path)
            .await
            .map_err(LeanToolError::Other)?;

        full_log.push_str(&out);
        if !ok {
            let errors = vec![format!("lake clean failed: {out}")];
            return Ok(BuildResult {
                success: false,
                output: truncate_output(&full_log, output_lines),
                errors,
            });
        }
    }

    // 2. Run `lake exe cache get`.
    match run_command("lake", &["exe", "cache", "get"], project_path).await {
        Ok((_, out)) => {
            if !full_log.is_empty() {
                full_log.push('\n');
            }
            full_log.push_str(&out);
        }
        Err(e) => {
            // Cache fetch failure is non-fatal; log it and continue.
            if !full_log.is_empty() {
                full_log.push('\n');
            }
            full_log.push_str(&format!("cache get warning: {e}"));
        }
    }

    // 3. Run `lake build --verbose`.
    let (build_ok, build_out) = run_command("lake", &["build", "--verbose"], project_path)
        .await
        .map_err(|e| LeanToolError::Other(e.to_string()))?;

    if !full_log.is_empty() {
        full_log.push('\n');
    }
    full_log.push_str(&build_out);

    // 4. Parse progress (for future progress reporting).
    let _progress = parse_progress(&build_out);

    // 5. Collect errors from build output.
    let errors = if build_ok {
        vec![]
    } else {
        collect_errors(&full_log)
    };

    // 6. Return result with truncated output.
    Ok(BuildResult {
        success: build_ok,
        output: truncate_output(&full_log, output_lines),
        errors,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_progress ----

    #[test]
    fn parse_progress_extracts_patterns() {
        let output = "[1/5] Building Module.A\n[2/5] Building Module.B\n[5/5] Done";
        let progress = parse_progress(output);
        assert_eq!(progress, vec![(1, 5), (2, 5), (5, 5)]);
    }

    #[test]
    fn parse_progress_empty_on_no_patterns() {
        let output = "Building project...\nDone.";
        let progress = parse_progress(output);
        assert!(progress.is_empty());
    }

    #[test]
    fn parse_progress_handles_large_numbers() {
        let output = "[150/300] Compiling Mathlib.Tactic.Ring";
        let progress = parse_progress(output);
        assert_eq!(progress, vec![(150, 300)]);
    }

    // ---- collect_errors ----

    #[test]
    fn collect_errors_finds_error_lines() {
        let output = "Building Module.A\nerror: unknown identifier 'foo'\n\
                      Building Module.B\nerror: type mismatch\nDone.";
        let errors = collect_errors(output);
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0], "error: unknown identifier 'foo'");
        assert_eq!(errors[1], "error: type mismatch");
    }

    #[test]
    fn collect_errors_case_insensitive() {
        let output = "ERROR: something failed\nError: another failure";
        let errors = collect_errors(output);
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn collect_errors_empty_on_success() {
        let output = "Building Module.A\n[1/1] Done";
        let errors = collect_errors(output);
        assert!(errors.is_empty());
    }

    // ---- truncate_output ----

    #[test]
    fn truncate_output_returns_last_n_lines() {
        let output = "line1\nline2\nline3\nline4\nline5";
        let result = truncate_output(output, 3);
        assert_eq!(result, "line3\nline4\nline5");
    }

    #[test]
    fn truncate_output_returns_all_when_fewer_lines() {
        let output = "line1\nline2";
        let result = truncate_output(output, 10);
        assert_eq!(result, "line1\nline2");
    }

    #[test]
    fn truncate_output_returns_empty_when_zero() {
        let output = "line1\nline2\nline3";
        let result = truncate_output(output, 0);
        assert_eq!(result, "");
    }

    #[test]
    fn truncate_output_handles_empty_input() {
        let result = truncate_output("", 5);
        assert_eq!(result, "");
    }

    // ---- BuildResult construction (simulated) ----

    #[test]
    fn build_result_success() {
        let result = BuildResult {
            success: true,
            output: "Build complete".into(),
            errors: vec![],
        };
        assert!(result.success);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn build_result_failure_with_errors() {
        let log = "Building...\nerror: unknown id\nerror: type mismatch\nDone.";
        let errors = collect_errors(log);
        let result = BuildResult {
            success: false,
            output: truncate_output(log, DEFAULT_OUTPUT_LINES),
            errors,
        };
        assert!(!result.success);
        assert_eq!(result.errors.len(), 2);
        assert!(result.output.contains("error: unknown id"));
    }

    #[test]
    fn build_result_output_truncation() {
        let lines: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
        let full_output = lines.join("\n");
        let truncated = truncate_output(&full_output, 5);
        let truncated_lines: Vec<&str> = truncated.lines().collect();
        assert_eq!(truncated_lines.len(), 5);
        assert_eq!(truncated_lines[0], "line 45");
        assert_eq!(truncated_lines[4], "line 49");
    }

    #[test]
    fn build_result_clean_failure() {
        let result = BuildResult {
            success: false,
            output: "lake clean failed".into(),
            errors: vec!["lake clean failed: permission denied".into()],
        };
        assert!(!result.success);
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("permission denied"));
    }

    // ---- parse_progress edge cases ----

    #[test]
    fn parse_progress_mixed_content() {
        let output = "Fetching cache...\n[1/10] Building A\nsome text [2/10] more text\n[10/10]";
        let progress = parse_progress(output);
        assert_eq!(progress, vec![(1, 10), (2, 10), (10, 10)]);
    }

    #[test]
    fn parse_progress_ignores_malformed() {
        let output = "[/5] bad\n[abc/def] bad\n[3/5] good";
        let progress = parse_progress(output);
        assert_eq!(progress, vec![(3, 5)]);
    }
}
