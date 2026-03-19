//! Tool handler for `lean_project_health`.
//!
//! Scans a Lean project using ripgrep (fast) and optionally queries goal
//! states at sorry positions via LSP (slow). Returns aggregated project
//! health: sorry locations, error patterns, and file count.

use std::path::Path;

use lean_lsp_client::client::LspClient;
use lean_mcp_core::models::{ProjectHealthResult, SorryLocation};

/// Handle a `lean_project_health` tool call.
///
/// Scans the project directory for `.lean` files, finds sorry occurrences
/// and error-like patterns using ripgrep, and optionally fetches goal
/// states at sorry positions via the LSP client.
pub async fn handle_project_health(
    project_path: &Path,
    client: Option<&dyn LspClient>,
    include_goals: bool,
) -> Result<ProjectHealthResult, String> {
    // 1. Count .lean files
    let file_count = count_lean_files(project_path)?;

    // 2. Find sorry occurrences (skip if no lean files)
    let mut sorries = if file_count > 0 {
        find_sorries(project_path)?
    } else {
        vec![]
    };

    // 3. Find error patterns (skip if no lean files)
    let errors = if file_count > 0 {
        find_error_patterns(project_path)?
    } else {
        vec![]
    };

    // 4. Optionally fetch goal states at sorry positions
    if include_goals {
        if let Some(client) = client {
            enrich_with_goals(client, &mut sorries).await;
        }
    }

    Ok(ProjectHealthResult {
        file_count,
        sorries,
        errors,
        success: true,
    })
}

/// Count .lean files in the project using ripgrep.
fn count_lean_files(project_path: &Path) -> Result<u32, String> {
    let output = std::process::Command::new("rg")
        .args(["--type", "lean", "--files"])
        .current_dir(project_path)
        .output()
        .map_err(|e| format!("ripgrep not found or failed to run: {e}"))?;

    // Exit code 1 means no files found
    if !output.status.success() && output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ripgrep error counting files: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let count = stdout.lines().filter(|l| !l.trim().is_empty()).count() as u32;
    Ok(count)
}

/// Find all sorry occurrences across .lean files using ripgrep.
fn find_sorries(project_path: &Path) -> Result<Vec<SorryLocation>, String> {
    let output = std::process::Command::new("rg")
        .args(["--json", "--type", "lean", "--no-heading", r"\bsorry\b"])
        .current_dir(project_path)
        .output()
        .map_err(|e| format!("ripgrep not found or failed to run: {e}"))?;

    if !output.status.success() && output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ripgrep error finding sorries: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Ok(vec![]);
    }

    let mut sorries = Vec::new();

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if parsed["type"] != "match" {
            continue;
        }

        let data = &parsed["data"];
        let file = data["path"]["text"].as_str().unwrap_or("").to_string();
        let line_number = data["line_number"].as_u64().unwrap_or(0) as u32;
        let text = data["lines"]["text"]
            .as_str()
            .unwrap_or("")
            .trim()
            .to_string();

        if file.is_empty() || line_number == 0 {
            continue;
        }

        // Skip sorry in comments (simple heuristic: line starts with --)
        let trimmed = text.trim_start();
        if trimmed.starts_with("--") || trimmed.starts_with("/-") {
            continue;
        }

        sorries.push(SorryLocation {
            file,
            line: line_number,
            text,
            decl: None, // Could be enriched later
            goal: None,
        });
    }

    Ok(sorries)
}

/// Find error-like patterns in .lean files using ripgrep.
///
/// Searches for `#check_failure`, `error`, and similar patterns that
/// indicate problems. Returns formatted strings with file:line: message.
fn find_error_patterns(project_path: &Path) -> Result<Vec<String>, String> {
    let output = std::process::Command::new("rg")
        .args([
            "--json",
            "--type",
            "lean",
            "--no-heading",
            r"#check_failure",
        ])
        .current_dir(project_path)
        .output()
        .map_err(|e| format!("ripgrep not found or failed to run: {e}"))?;

    if !output.status.success() && output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ripgrep error finding errors: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Ok(vec![]);
    }

    let mut errors = Vec::new();

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let parsed: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if parsed["type"] != "match" {
            continue;
        }

        let data = &parsed["data"];
        let file = data["path"]["text"].as_str().unwrap_or("");
        let line_number = data["line_number"].as_u64().unwrap_or(0);
        let text = data["lines"]["text"].as_str().unwrap_or("").trim();

        if !file.is_empty() && line_number > 0 {
            errors.push(format!("{file}:{line_number}: {text}"));
        }
    }

    Ok(errors)
}

/// Enrich sorry locations with goal states from the LSP client.
async fn enrich_with_goals(client: &dyn LspClient, sorries: &mut [SorryLocation]) {
    for sorry in sorries.iter_mut() {
        // Open the file first
        if client.open_file(&sorry.file).await.is_err() {
            continue;
        }

        // Get file content to compute column
        let content = match client.get_file_content(&sorry.file).await {
            Ok(c) => c,
            Err(_) => continue,
        };

        let lines: Vec<&str> = content.lines().collect();
        let lsp_line = sorry.line.saturating_sub(1);
        if lsp_line as usize >= lines.len() {
            continue;
        }

        // Find the column of "sorry" in the line
        let line_text = lines[lsp_line as usize];
        let col = match line_text.find("sorry") {
            Some(c) => c as u32,
            None => continue,
        };

        // Query goal at the sorry position (0-indexed line and column for LSP)
        let goal_response = match client.get_goal(&sorry.file, lsp_line, col).await {
            Ok(resp) => resp,
            Err(_) => continue,
        };

        if let Some(resp) = goal_response {
            let goals = lean_mcp_core::utils::extract_goals_list(Some(&resp));
            if !goals.is_empty() {
                sorry.goal = Some(goals.join("\n---\n"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Skip test if ripgrep is not installed (e.g. CI runners).
    fn require_ripgrep() -> bool {
        if std::process::Command::new("rg")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: ripgrep not installed");
            return false;
        }
        true
    }

    fn setup_project(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (name, content) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
        }
        dir
    }

    #[test]
    fn count_lean_files_finds_lean_files() {
        if !require_ripgrep() {
            return;
        }
        let dir = setup_project(&[
            ("Main.lean", "theorem foo : True := by trivial"),
            ("Lib.lean", "def x := 1"),
            ("README.md", "not a lean file"),
        ]);
        let count = count_lean_files(dir.path()).unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn count_lean_files_empty_project() {
        if !require_ripgrep() {
            return;
        }
        let dir = setup_project(&[("README.md", "no lean files")]);
        let count = count_lean_files(dir.path()).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn find_sorries_detects_sorry() {
        if !require_ripgrep() {
            return;
        }
        let dir = setup_project(&[
            ("Main.lean", "theorem foo : True := by\n  sorry"),
            ("Lib.lean", "def x := 1"),
        ]);
        let sorries = find_sorries(dir.path()).unwrap();
        assert_eq!(sorries.len(), 1);
        assert_eq!(sorries[0].file, "Main.lean");
        assert_eq!(sorries[0].line, 2);
        assert_eq!(sorries[0].text, "sorry");
    }

    #[test]
    fn find_sorries_skips_comments() {
        if !require_ripgrep() {
            return;
        }
        let dir = setup_project(&[(
            "Main.lean",
            "-- sorry this is a comment\ntheorem foo : True := by\n  sorry",
        )]);
        let sorries = find_sorries(dir.path()).unwrap();
        assert_eq!(sorries.len(), 1);
        assert_eq!(sorries[0].line, 3);
    }

    #[test]
    fn find_sorries_no_matches() {
        if !require_ripgrep() {
            return;
        }
        let dir = setup_project(&[("Main.lean", "theorem foo : True := by trivial")]);
        let sorries = find_sorries(dir.path()).unwrap();
        assert!(sorries.is_empty());
    }

    #[test]
    fn find_sorries_multiple_files() {
        if !require_ripgrep() {
            return;
        }
        let dir = setup_project(&[
            ("A.lean", "theorem a : True := sorry"),
            ("B.lean", "theorem b : True := sorry"),
        ]);
        let sorries = find_sorries(dir.path()).unwrap();
        assert_eq!(sorries.len(), 2);
    }

    #[test]
    fn find_error_patterns_detects_check_failure() {
        if !require_ripgrep() {
            return;
        }
        let dir = setup_project(&[("Main.lean", "def x := 1\n#check_failure foo")]);
        let errors = find_error_patterns(dir.path()).unwrap();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("Main.lean"));
        assert!(errors[0].contains("#check_failure"));
    }

    #[test]
    fn find_error_patterns_no_matches() {
        if !require_ripgrep() {
            return;
        }
        let dir = setup_project(&[("Main.lean", "def x := 1")]);
        let errors = find_error_patterns(dir.path()).unwrap();
        assert!(errors.is_empty());
    }

    #[tokio::test]
    async fn handle_project_health_no_lsp() {
        if !require_ripgrep() {
            return;
        }
        let dir = setup_project(&[
            ("Main.lean", "theorem foo : True := by\n  sorry"),
            ("Lib.lean", "def x := 1"),
        ]);
        let result = handle_project_health(dir.path(), None, false)
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.file_count, 2);
        assert_eq!(result.sorries.len(), 1);
        assert!(result.errors.is_empty());
    }

    #[tokio::test]
    async fn handle_project_health_empty_project() {
        if !require_ripgrep() {
            return;
        }
        let dir = setup_project(&[("README.md", "nothing")]);
        let result = handle_project_health(dir.path(), None, false)
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.file_count, 0);
        assert!(result.sorries.is_empty());
    }

    #[test]
    fn find_sorries_word_boundary() {
        if !require_ripgrep() {
            return;
        }
        // "sorry" inside a string like "sorry_tactic" should NOT match
        // because we use \bsorry\b
        let dir = setup_project(&[("Main.lean", "def sorry_helper := 1\ndef x := sorry")]);
        let sorries = find_sorries(dir.path()).unwrap();
        // Only the standalone "sorry" should match, not "sorry_helper"
        assert_eq!(sorries.len(), 1);
        assert_eq!(sorries[0].line, 2);
    }

    #[test]
    fn find_sorries_in_subdirectories() {
        if !require_ripgrep() {
            return;
        }
        let dir = setup_project(&[
            ("src/Main.lean", "theorem foo := sorry"),
            ("src/sub/Lib.lean", "theorem bar := sorry"),
        ]);
        let sorries = find_sorries(dir.path()).unwrap();
        assert_eq!(sorries.len(), 2);
    }
}
