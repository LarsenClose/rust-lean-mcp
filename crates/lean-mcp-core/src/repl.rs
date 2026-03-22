//! REPL manager for the Lean 4 REPL.
//!
//! Ports the Python `repl.py` (264 lines). The REPL enables ~5x faster tactic
//! testing by keeping a persistent Lean REPL process and communicating via
//! JSON-over-stdin/stdout.
//!
//! # Protocol
//!
//! - **Code command**: `{"cmd": "<code>", "env": <null|int>}` returns
//!   `{"env": int, "sorries": [...]}` or error fields.
//! - **Tactic command**: `{"tactic": "<tactic>", "proofState": int}` returns
//!   `{"goals": [...], "proofStatus": "..."}` or error fields.
//!
//! # Workflow
//!
//! 1. Load a header (import block) once and cache its environment id.
//! 2. Send body code with a trailing `sorry` to obtain a `proofState`.
//! 3. Run each tactic snippet against that proof state.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tracing;

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

/// The result of running a single tactic snippet against the REPL.
#[derive(Debug, Clone)]
pub struct SnippetResult {
    /// Goal strings remaining after the tactic.
    pub goals: Vec<String>,
    /// Raw JSON messages from the REPL response.
    pub messages: Vec<Value>,
    /// Proof status string (e.g. `"completed"`, `"open"`).
    pub proof_status: Option<String>,
    /// Error message if the tactic failed.
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Repl
// ---------------------------------------------------------------------------

/// Manages a persistent Lean REPL child process.
///
/// The REPL binary is discovered via [`Repl::find_repl_binary`] and communicated
/// with through JSON lines on stdin/stdout.
pub struct Repl {
    project_dir: PathBuf,
    repl_path: String,
    timeout: u64,
    proc: Option<Child>,
    /// Cached header text (imports).
    header: Option<String>,
    /// Cached environment id from loading the header.
    header_env: Option<u64>,
}

impl Repl {
    /// Create a new REPL manager for the given project directory.
    ///
    /// `repl_path` is the path/name of the REPL binary. Use
    /// [`Repl::find_repl_binary`] to locate it automatically.
    pub fn new(project_dir: &Path, repl_path: &str) -> Self {
        Self {
            project_dir: project_dir.to_path_buf(),
            repl_path: repl_path.to_string(),
            timeout: 60,
            proc: None,
            header: None,
            header_env: None,
        }
    }

    /// Set the timeout in seconds for REPL commands.
    pub fn set_timeout(&mut self, timeout: u64) {
        self.timeout = timeout;
    }

    /// Find the REPL binary using the following precedence:
    ///
    /// 1. `LEAN_REPL_PATH` environment variable
    /// 2. `.lake/packages/repl/.lake/build/bin/repl` inside the project
    /// 3. `repl` on `PATH`
    pub fn find_repl_binary(project_dir: &Path) -> Option<String> {
        // 1. Environment variable
        if let Ok(val) = std::env::var("LEAN_REPL_PATH") {
            if !val.is_empty() {
                return Some(val);
            }
        }

        // 2. .lake/packages/repl/.lake/build/bin/repl
        let lake_repl = project_dir
            .join(".lake")
            .join("packages")
            .join("repl")
            .join(".lake")
            .join("build")
            .join("bin")
            .join("repl");
        if lake_repl.exists() {
            return lake_repl.to_str().map(|s| s.to_string());
        }

        // 3. Check PATH via `which repl`
        let output = std::process::Command::new("which")
            .arg("repl")
            .output()
            .ok()?;
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(path);
            }
        }

        None
    }

    /// Start (or restart) the REPL child process.
    pub async fn start(&mut self) -> Result<(), String> {
        // Kill any existing process.
        self.close().await;

        // Launch via `lake env <repl>` so that LEAN_PATH and other environment
        // variables are set correctly, matching the Python implementation.
        let child = Command::new("lake")
            .arg("env")
            .arg(&self.repl_path)
            .current_dir(&self.project_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                format!(
                    "Failed to start REPL process via 'lake env {}': {e}",
                    self.repl_path
                )
            })?;

        self.proc = Some(child);
        self.header = None;
        self.header_env = None;
        Ok(())
    }

    /// Send a JSON command to the REPL and read the JSON response line.
    async fn send_command(&mut self, cmd: &Value) -> Result<Value, String> {
        let proc = self
            .proc
            .as_mut()
            .ok_or_else(|| "REPL process not running".to_string())?;

        let stdin = proc
            .stdin
            .as_mut()
            .ok_or_else(|| "REPL stdin not available".to_string())?;

        let stdout = proc
            .stdout
            .as_mut()
            .ok_or_else(|| "REPL stdout not available".to_string())?;

        let cmd_str = serde_json::to_string(cmd)
            .map_err(|e| format!("Failed to serialize REPL command: {e}"))?;

        tracing::debug!("REPL send: {}", cmd_str);

        // Write command + newline
        stdin
            .write_all(cmd_str.as_bytes())
            .await
            .map_err(|e| format!("Failed to write to REPL stdin: {e}"))?;
        stdin
            .write_all(b"\n\n")
            .await
            .map_err(|e| format!("Failed to write newline to REPL stdin: {e}"))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("Failed to flush REPL stdin: {e}"))?;

        // Read response with timeout
        let timeout_duration = std::time::Duration::from_secs(self.timeout);
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();

        let read_result = tokio::time::timeout(timeout_duration, async {
            // The REPL may print multiple lines; we read until we get a valid JSON line.
            loop {
                line.clear();
                let bytes_read = reader
                    .read_line(&mut line)
                    .await
                    .map_err(|e| format!("Failed to read from REPL stdout: {e}"))?;
                if bytes_read == 0 {
                    return Err("REPL process closed stdout unexpectedly".to_string());
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(trimmed) {
                    Ok(val) => {
                        tracing::debug!("REPL recv: {}", trimmed);
                        return Ok(val);
                    }
                    Err(_) => {
                        // Not valid JSON yet, continue reading
                        tracing::trace!("REPL non-JSON line: {}", trimmed);
                        continue;
                    }
                }
            }
        })
        .await;

        match read_result {
            Ok(inner) => inner,
            Err(_) => Err(format!("REPL command timed out after {}s", self.timeout)),
        }
    }

    /// Load the header (import block) and cache the resulting environment id.
    ///
    /// If the header text is unchanged from a previous call, the cached
    /// environment is reused.
    pub async fn load_header(&mut self, header_text: &str) -> Result<u64, String> {
        // Check cache
        if let (Some(ref cached_header), Some(env)) = (&self.header, self.header_env) {
            if cached_header == header_text {
                return Ok(env);
            }
        }

        // Need to (re)start the REPL for a new header
        self.start().await?;

        let cmd = serde_json::json!({
            "cmd": header_text,
            "env": 0
        });

        let resp = self.send_command(&cmd).await?;

        if let Some(err) = resp.get("message").and_then(|m| m.as_str()) {
            if !err.is_empty() {
                return Err(format!("REPL header error: {err}"));
            }
        }

        let env = resp.get("env").and_then(|e| e.as_u64()).ok_or_else(|| {
            format!(
                "REPL header response missing 'env' field: {}",
                serde_json::to_string(&resp).unwrap_or_default()
            )
        })?;

        self.header = Some(header_text.to_string());
        self.header_env = Some(env);
        Ok(env)
    }

    /// Extract the header (everything before the first declaration keyword) and
    /// body from the base code.
    pub fn split_header_body(base_code: &str) -> (String, String) {
        // Declaration keywords that mark the start of the body.
        //
        // Context-establishing commands (`open`, `namespace`, `section`,
        // `set_option`, `variable`) are deliberately **excluded** so they
        // remain in the header. This ensures the REPL header environment
        // includes the same namespace openings, options, and variables that
        // `lake build` sees, preventing name-resolution divergence (#149).
        let decl_keywords = [
            "theorem ",
            "lemma ",
            "def ",
            "example ",
            "instance ",
            "class ",
            "structure ",
            "inductive ",
            "noncomputable ",
            "private ",
            "protected ",
            "#check ",
            "#eval ",
        ];

        for (i, line) in base_code.lines().enumerate() {
            let trimmed = line.trim_start();
            let is_decl = decl_keywords.iter().any(|kw| trimmed.starts_with(kw));
            if is_decl {
                // Found the first declaration line
                let header_end: usize = base_code
                    .lines()
                    .take(i)
                    .map(|l| l.len() + 1) // +1 for newline
                    .sum();
                let header = base_code[..header_end].to_string();
                let body = base_code[header_end..].to_string();
                return (header, body);
            }
        }

        // No declaration found — everything is header
        (base_code.to_string(), String::new())
    }

    /// Run multiple tactic snippets against the given base code.
    ///
    /// # Workflow
    ///
    /// 1. Split `base_code` into header (imports) and body.
    /// 2. Load the header to get its environment (cached across calls).
    /// 3. Send the body with a trailing `sorry` to get the proof state.
    /// 4. Run each snippet as a tactic against that proof state.
    ///
    /// Returns one [`SnippetResult`] per snippet. If the setup steps fail,
    /// all results will carry the error.
    pub async fn run_snippets(
        &mut self,
        base_code: &str,
        snippets: &[String],
    ) -> Vec<SnippetResult> {
        let error_result = |msg: &str| SnippetResult {
            goals: vec![],
            messages: vec![],
            proof_status: None,
            error: Some(msg.to_string()),
        };

        if snippets.is_empty() {
            return vec![];
        }

        // 1. Split header / body
        let (header, body) = Self::split_header_body(base_code);

        // 2. Load header
        let header_env = match self.load_header(&header).await {
            Ok(env) => env,
            Err(e) => {
                return snippets.iter().map(|_| error_result(&e)).collect();
            }
        };

        // 3. Send body + sorry to get proof state
        let body_with_sorry = if body.trim().is_empty() {
            return snippets
                .iter()
                .map(|_| error_result("No body code to evaluate"))
                .collect();
        } else {
            format!("{}\nsorry", body.trim_end())
        };

        let body_cmd = serde_json::json!({
            "cmd": body_with_sorry,
            "env": header_env
        });

        let body_resp = match self.send_command(&body_cmd).await {
            Ok(resp) => resp,
            Err(e) => {
                return snippets
                    .iter()
                    .map(|_| error_result(&format!("REPL body error: {e}")))
                    .collect();
            }
        };

        // Extract proof state from the last sorry (ours is always appended at the end;
        // earlier entries may be pre-existing sorries in the user code).
        let proof_state = Self::extract_proof_state(&body_resp);

        let proof_state = match proof_state {
            Some(ps) => ps,
            None => {
                // Check for error messages in the response
                let err_msg = body_resp
                    .get("messages")
                    .and_then(|m| m.as_array())
                    .map(|msgs| {
                        msgs.iter()
                            .filter_map(|m| {
                                let sev = m.get("severity")?.as_str()?;
                                let data = m.get("data")?.as_str()?;
                                if sev == "error" {
                                    Some(data.to_string())
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .unwrap_or_default();

                let msg = if err_msg.is_empty() {
                    format!(
                        "No proof state found in REPL response: {}",
                        serde_json::to_string(&body_resp).unwrap_or_default()
                    )
                } else {
                    format!("REPL body errors: {err_msg}")
                };

                return snippets.iter().map(|_| error_result(&msg)).collect();
            }
        };

        // 4. Run each tactic snippet
        let mut results = Vec::with_capacity(snippets.len());
        for snippet in snippets {
            let tactic_cmd = serde_json::json!({
                "tactic": snippet,
                "proofState": proof_state
            });

            match self.send_command(&tactic_cmd).await {
                Ok(resp) => {
                    let goals = resp
                        .get("goals")
                        .and_then(|g| g.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();

                    let proof_status = resp
                        .get("proofStatus")
                        .and_then(|s| s.as_str())
                        .map(String::from);

                    let messages = resp
                        .get("messages")
                        .and_then(|m| m.as_array())
                        .cloned()
                        .unwrap_or_default();

                    let error = resp
                        .get("message")
                        .and_then(|m| m.as_str())
                        .map(String::from);

                    results.push(SnippetResult {
                        goals,
                        messages,
                        proof_status,
                        error,
                    });
                }
                Err(e) => {
                    results.push(error_result(&format!("REPL tactic error: {e}")));
                }
            }
        }

        results
    }

    /// Extract the proof state id from the *last* sorry in the REPL response.
    ///
    /// The appended `sorry` is always the last entry in the `"sorries"` array;
    /// earlier entries may be pre-existing sorries in the user code. This
    /// mirrors the Python implementation which uses `sorries[-1]`.
    fn extract_proof_state(body_resp: &Value) -> Option<u64> {
        body_resp
            .get("sorries")
            .and_then(|s| s.as_array())
            .and_then(|arr| arr.last())
            .and_then(|s| s.get("proofState"))
            .and_then(|ps| ps.as_u64())
    }

    /// Returns the cached header text, if any.
    pub fn cached_header(&self) -> Option<&str> {
        self.header.as_deref()
    }

    /// Returns the cached header environment id, if any.
    pub fn cached_header_env(&self) -> Option<u64> {
        self.header_env
    }

    /// Check whether the REPL child process is still alive.
    ///
    /// Returns `true` if a process is running and has not exited,
    /// `false` if no process exists or it has already exited.
    pub fn is_alive(&mut self) -> bool {
        match self.proc.as_mut() {
            None => false,
            Some(child) => {
                // try_wait returns Ok(Some(status)) if exited, Ok(None) if still running
                matches!(child.try_wait(), Ok(None))
            }
        }
    }

    /// Close the REPL child process if running.
    pub async fn close(&mut self) {
        if let Some(mut child) = self.proc.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.header = None;
        self.header_env = None;
    }
}

impl Drop for Repl {
    fn drop(&mut self) {
        if let Some(mut child) = self.proc.take() {
            let _ = child.start_kill();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    /// Helper: temporarily set env vars under a global lock.
    fn with_env_vars<F, R>(vars: &[(&str, &str)], f: F) -> R
    where
        F: FnOnce() -> R + std::panic::UnwindSafe,
    {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        for (k, v) in vars {
            unsafe { std::env::set_var(k, v) };
        }
        let result = std::panic::catch_unwind(f);
        for (k, _) in vars {
            unsafe { std::env::remove_var(k) };
        }
        result.unwrap_or_else(|e| std::panic::resume_unwind(e))
    }

    // ---- find_repl_binary --------------------------------------------------

    #[test]
    fn find_repl_binary_from_env_var() {
        with_env_vars(&[("LEAN_REPL_PATH", "/custom/repl")], || {
            let result = Repl::find_repl_binary(Path::new("/nonexistent"));
            assert_eq!(result, Some("/custom/repl".to_string()));
        });
    }

    #[test]
    fn find_repl_binary_empty_env_var_skipped() {
        with_env_vars(&[("LEAN_REPL_PATH", "")], || {
            // With empty env and nonexistent dir, should fall through
            let result = Repl::find_repl_binary(Path::new("/nonexistent/project"));
            // May or may not find `repl` on PATH; just verify no panic
            // and that it didn't return the empty string
            if let Some(ref path) = result {
                assert!(!path.is_empty());
            }
        });
    }

    #[test]
    fn find_repl_binary_in_lake_packages() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::remove_var("LEAN_REPL_PATH") };

        let tmp = tempfile::tempdir().unwrap();
        let repl_bin = tmp
            .path()
            .join(".lake")
            .join("packages")
            .join("repl")
            .join(".lake")
            .join("build")
            .join("bin");
        std::fs::create_dir_all(&repl_bin).unwrap();
        let repl_path = repl_bin.join("repl");
        std::fs::write(&repl_path, "#!/bin/sh\n").unwrap();

        let result = Repl::find_repl_binary(tmp.path());
        assert!(result.is_some());
        assert!(result.unwrap().contains(".lake/packages/repl"));
    }

    #[test]
    fn find_repl_binary_nonexistent_project() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::remove_var("LEAN_REPL_PATH") };

        // With a nonexistent directory, should not find the lake path.
        // May or may not find `repl` on PATH.
        let result = Repl::find_repl_binary(Path::new("/absolutely/nonexistent/path"));
        // Just verify no panic
        let _ = result;
    }

    // ---- Repl construction -------------------------------------------------

    #[test]
    fn new_repl_has_correct_fields() {
        let repl = Repl::new(Path::new("/my/project"), "/usr/bin/repl");
        assert_eq!(repl.project_dir, PathBuf::from("/my/project"));
        assert_eq!(repl.repl_path, "/usr/bin/repl");
        assert_eq!(repl.timeout, 60);
        assert!(repl.proc.is_none());
        assert!(repl.header.is_none());
        assert!(repl.header_env.is_none());
    }

    #[test]
    fn set_timeout_changes_value() {
        let mut repl = Repl::new(Path::new("/my/project"), "repl");
        repl.set_timeout(120);
        assert_eq!(repl.timeout, 120);
    }

    // ---- split_header_body -------------------------------------------------

    #[test]
    fn split_header_body_with_imports_and_theorem() {
        let code = "import Mathlib\nimport Lean\n\ntheorem foo : True := by\n  trivial";
        let (header, body) = Repl::split_header_body(code);
        assert_eq!(header, "import Mathlib\nimport Lean\n\n");
        assert_eq!(body, "theorem foo : True := by\n  trivial");
    }

    #[test]
    fn split_header_body_with_def() {
        let code = "import Lean\n\ndef myFunc : Nat := 42";
        let (header, body) = Repl::split_header_body(code);
        assert_eq!(header, "import Lean\n\n");
        assert_eq!(body, "def myFunc : Nat := 42");
    }

    #[test]
    fn split_header_body_no_declaration() {
        let code = "import Mathlib\nimport Lean\n\n-- just comments";
        let (header, body) = Repl::split_header_body(code);
        assert_eq!(header, code);
        assert_eq!(body, "");
    }

    #[test]
    fn split_header_body_immediate_declaration() {
        let code = "theorem foo : True := trivial";
        let (header, body) = Repl::split_header_body(code);
        assert_eq!(header, "");
        assert_eq!(body, "theorem foo : True := trivial");
    }

    #[test]
    fn split_header_body_with_lemma() {
        let code = "import Lean\n\nlemma bar : 1 = 1 := rfl";
        let (header, body) = Repl::split_header_body(code);
        assert_eq!(header, "import Lean\n\n");
        assert_eq!(body, "lemma bar : 1 = 1 := rfl");
    }

    #[test]
    fn split_header_body_private_declaration() {
        let code = "import Lean\n\nprivate def secret := 42";
        let (header, body) = Repl::split_header_body(code);
        assert_eq!(header, "import Lean\n\n");
        assert_eq!(body, "private def secret := 42");
    }

    #[test]
    fn split_header_body_open_namespace() {
        // `open Nat in` is a scoped modifier for the theorem — the split
        // happens at `theorem` (after `open Nat in`), not at `open`.
        // But since `open` is no longer a decl keyword, the first
        // decl keyword is `theorem` on a later line. The `open Nat in`
        // line is part of the header.
        let code = "import Lean\n\nopen Nat in\ntheorem foo := rfl";
        let (header, body) = Repl::split_header_body(code);
        assert_eq!(header, "import Lean\n\nopen Nat in\n");
        assert_eq!(body, "theorem foo := rfl");
    }

    #[test]
    fn split_header_body_open_in_header() {
        // `open MeasureTheory` should be included in the header, not
        // treated as a body separator. This ensures REPL name resolution
        // matches lake build (#149).
        let code =
            "import Mathlib.MeasureTheory\n\nopen MeasureTheory\n\ntheorem foo : True := trivial";
        let (header, body) = Repl::split_header_body(code);
        assert_eq!(
            header,
            "import Mathlib.MeasureTheory\n\nopen MeasureTheory\n\n"
        );
        assert_eq!(body, "theorem foo : True := trivial");
    }

    #[test]
    fn split_header_body_namespace_in_header() {
        // `namespace` should be included in the header so the REPL
        // elaborates in the correct namespace context.
        let code = "import Lean\n\nnamespace MyModule\n\ndef foo := 42";
        let (header, body) = Repl::split_header_body(code);
        assert_eq!(header, "import Lean\n\nnamespace MyModule\n\n");
        assert_eq!(body, "def foo := 42");
    }

    #[test]
    fn split_header_body_set_option_in_header() {
        // `set_option` should be included in the header so elaboration
        // options match lake build context.
        let code = "import Lean\n\nset_option autoImplicit false\n\ndef foo := 42";
        let (header, body) = Repl::split_header_body(code);
        assert_eq!(header, "import Lean\n\nset_option autoImplicit false\n\n");
        assert_eq!(body, "def foo := 42");
    }

    #[test]
    fn split_header_body_variable_in_header() {
        // `variable` should be in the header so the REPL has the same
        // section variables as lake build.
        let code = "import Lean\n\nvariable (n : Nat)\n\ndef foo := n";
        let (header, body) = Repl::split_header_body(code);
        assert_eq!(header, "import Lean\n\nvariable (n : Nat)\n\n");
        assert_eq!(body, "def foo := n");
    }

    #[test]
    fn split_header_body_section_in_header() {
        // `section` should be in the header for correct scoping context.
        let code = "import Lean\n\nsection MySection\n\ndef foo := 42";
        let (header, body) = Repl::split_header_body(code);
        assert_eq!(header, "import Lean\n\nsection MySection\n\n");
        assert_eq!(body, "def foo := 42");
    }

    #[test]
    fn split_header_body_multiple_context_lines() {
        // Multiple context-establishing lines should all be in header.
        let code = "import Mathlib\n\nopen Nat\nnamespace Foo\nset_option autoImplicit false\nvariable (n : Nat)\n\ntheorem bar : n = n := rfl";
        let (header, body) = Repl::split_header_body(code);
        assert_eq!(
            header,
            "import Mathlib\n\nopen Nat\nnamespace Foo\nset_option autoImplicit false\nvariable (n : Nat)\n\n"
        );
        assert_eq!(body, "theorem bar : n = n := rfl");
    }

    // ---- SnippetResult construction ----------------------------------------

    #[test]
    fn snippet_result_default_fields() {
        let sr = SnippetResult {
            goals: vec!["a = b".to_string()],
            messages: vec![],
            proof_status: Some("open".to_string()),
            error: None,
        };
        assert_eq!(sr.goals, vec!["a = b"]);
        assert!(sr.error.is_none());
        assert_eq!(sr.proof_status, Some("open".to_string()));
    }

    #[test]
    fn snippet_result_with_error() {
        let sr = SnippetResult {
            goals: vec![],
            messages: vec![],
            proof_status: None,
            error: Some("unknown tactic".to_string()),
        };
        assert!(sr.goals.is_empty());
        assert_eq!(sr.error, Some("unknown tactic".to_string()));
    }

    // ---- run_snippets edge cases (no process needed) -----------------------

    #[tokio::test]
    async fn run_snippets_empty_snippets_returns_empty() {
        let mut repl = Repl::new(Path::new("/tmp"), "repl");
        let results = repl
            .run_snippets("import Lean\ntheorem foo := by", &[])
            .await;
        assert!(results.is_empty());
    }

    // ---- close / Drop ------------------------------------------------------

    #[tokio::test]
    async fn close_when_no_process_is_noop() {
        let mut repl = Repl::new(Path::new("/tmp"), "repl");
        // Should not panic
        repl.close().await;
        assert!(repl.proc.is_none());
    }

    #[test]
    fn drop_when_no_process_is_noop() {
        let repl = Repl::new(Path::new("/tmp"), "repl");
        drop(repl);
        // Should not panic
    }

    // ---- extract_proof_state -----------------------------------------------

    #[test]
    fn extract_proof_state_single_sorry() {
        let resp = serde_json::json!({
            "sorries": [{"proofState": 42, "pos": {"line": 5, "column": 2}}],
            "env": 1
        });
        assert_eq!(Repl::extract_proof_state(&resp), Some(42));
    }

    #[test]
    fn extract_proof_state_multiple_sorries_returns_last() {
        // When the user code already contains sorries, the appended sorry
        // is the last entry.  We must pick the *last* one, not the first.
        let resp = serde_json::json!({
            "sorries": [
                {"proofState": 10, "pos": {"line": 3, "column": 0}},
                {"proofState": 20, "pos": {"line": 7, "column": 0}},
                {"proofState": 30, "pos": {"line": 12, "column": 0}}
            ],
            "env": 2
        });
        assert_eq!(Repl::extract_proof_state(&resp), Some(30));
    }

    #[test]
    fn extract_proof_state_empty_sorries_array() {
        let resp = serde_json::json!({
            "sorries": [],
            "env": 1
        });
        assert_eq!(Repl::extract_proof_state(&resp), None);
    }

    #[test]
    fn extract_proof_state_missing_sorries_field() {
        let resp = serde_json::json!({"env": 1});
        assert_eq!(Repl::extract_proof_state(&resp), None);
    }

    #[test]
    fn extract_proof_state_sorry_without_proof_state() {
        let resp = serde_json::json!({
            "sorries": [{"pos": {"line": 1, "column": 0}}],
            "env": 1
        });
        assert_eq!(Repl::extract_proof_state(&resp), None);
    }

    // ---- start (lake env wrapping) -----------------------------------------

    #[tokio::test]
    async fn start_uses_lake_env_wrapping() {
        // Verify that a start failure mentions 'lake env' in the error message,
        // confirming we launch via `lake env <repl_path>`.
        let mut repl = Repl::new(Path::new("/nonexistent/project"), "/fake/repl");
        let err = repl.start().await.unwrap_err();
        assert!(
            err.contains("lake env"),
            "Expected error to mention 'lake env', got: {err}"
        );
    }

    // ---- Send + Sync assertions -------------------------------------------

    #[test]
    fn snippet_result_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SnippetResult>();
    }
}
