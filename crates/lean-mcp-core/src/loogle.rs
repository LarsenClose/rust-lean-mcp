//! Loogle search -- local subprocess and remote API.
//!
//! Ports the Python `loogle.py` (499 lines). Two modes:
//!
//! - **Remote**: HTTP GET to `https://loogle.lean-lang.org/json?q={query}` (or
//!   `LOOGLE_URL` override). Stateless, no installation needed.
//! - **Local**: Interactive subprocess (`loogle --json --interactive`) for
//!   offline/low-latency queries. Requires cloning and building the loogle repo.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;
use tracing;

use crate::models::LoogleResult;

// ---------------------------------------------------------------------------
// Cache directory
// ---------------------------------------------------------------------------

/// Return the cache directory for loogle data.
///
/// Precedence:
/// 1. `LEAN_LOOGLE_CACHE_DIR` environment variable
/// 2. `$XDG_CACHE_HOME/lean-lsp-mcp/loogle`
/// 3. `~/.cache/lean-lsp-mcp/loogle`
pub fn get_cache_dir() -> PathBuf {
    if let Ok(d) = std::env::var("LEAN_LOOGLE_CACHE_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    let base = if cfg!(windows) {
        std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs_fallback_home().join("AppData").join("Local"))
    } else {
        std::env::var("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs_fallback_home().join(".cache"))
    };
    base.join("lean-lsp-mcp").join("loogle")
}

/// Fallback home directory (avoids pulling in the `dirs` crate).
fn dirs_fallback_home() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

// ---------------------------------------------------------------------------
// Remote API
// ---------------------------------------------------------------------------

/// Query the remote Loogle API.
///
/// Set `LOOGLE_URL` to override the default endpoint.
/// Set `LOOGLE_HEADERS` to a JSON object of extra headers
/// (e.g. `{"X-API-Key": "..."}`).
pub async fn loogle_remote(query: &str, num_results: usize) -> Result<Vec<LoogleResult>, String> {
    let base =
        std::env::var("LOOGLE_URL").unwrap_or_else(|_| "https://loogle.lean-lang.org".to_string());
    let url = format!(
        "{}/json?q={}",
        base.trim_end_matches('/'),
        urlencoding::encode(query)
    );

    let mut builder = reqwest::Client::new()
        .get(&url)
        .header("User-Agent", "lean-lsp-mcp/0.1")
        .timeout(std::time::Duration::from_secs(10));

    // Extra headers from environment
    if let Ok(extra) = std::env::var("LOOGLE_HEADERS") {
        if let Ok(map) = serde_json::from_str::<serde_json::Map<String, Value>>(&extra) {
            for (k, v) in map {
                if let Some(val) = v.as_str() {
                    builder = builder.header(&k, val);
                }
            }
        }
    }

    let response = builder
        .send()
        .await
        .map_err(|e| format!("loogle error:\n{e}"))?;

    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("loogle error:\n{e}"))?;

    let hits = match body.get("hits").and_then(|h| h.as_array()) {
        Some(arr) => arr,
        None => return Err("No results found.".to_string()),
    };

    let results: Vec<LoogleResult> = hits
        .iter()
        .take(num_results)
        .map(|h| LoogleResult {
            name: h
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            r#type: h
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            module: h
                .get("module")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        })
        .collect();

    Ok(results)
}

// ---------------------------------------------------------------------------
// Local subprocess manager
// ---------------------------------------------------------------------------

/// Manages a local loogle installation and interactive subprocess.
///
/// # Lifecycle
///
/// 1. [`LoogleManager::ensure_installed`] -- clone repo, build binary, create index
/// 2. [`LoogleManager::start`] -- launch interactive subprocess
/// 3. [`LoogleManager::query`] -- send queries via stdin, read JSON from stdout
/// 4. [`LoogleManager::stop`] -- terminate subprocess
pub struct LoogleManager {
    cache_dir: PathBuf,
    repo_dir: PathBuf,
    index_dir: PathBuf,
    project_path: Option<PathBuf>,
    process: Option<Child>,
    ready: bool,
    extra_paths: Vec<PathBuf>,
}

const REPO_URL: &str = "https://github.com/nomeata/loogle.git";
const READY_SIGNAL: &str = "Loogle is ready.";

impl LoogleManager {
    /// Create a new manager.
    ///
    /// `cache_dir` defaults to [`get_cache_dir()`]. `project_path` is an optional
    /// Lean project whose `.lake/packages` will be indexed.
    pub fn new(cache_dir: Option<PathBuf>, project_path: Option<PathBuf>) -> Self {
        let cache_dir = cache_dir.unwrap_or_else(get_cache_dir);
        let repo_dir = cache_dir.join("repo");
        let index_dir = cache_dir.join("index");
        Self {
            cache_dir,
            repo_dir,
            index_dir,
            project_path,
            process: None,
            ready: false,
            extra_paths: Vec::new(),
        }
    }

    /// Path to the compiled loogle binary inside the repo.
    pub fn binary_path(&self) -> PathBuf {
        self.repo_dir
            .join(".lake")
            .join("build")
            .join("bin")
            .join("loogle")
    }

    /// Whether the loogle binary exists.
    pub fn is_installed(&self) -> bool {
        self.binary_path().exists()
    }

    /// Whether the subprocess is running and ready for queries.
    pub fn is_running(&self) -> bool {
        self.ready && self.process.as_ref().is_some_and(|p| p.id().is_some())
    }

    /// The cache directory.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// The repo directory.
    pub fn repo_dir(&self) -> &Path {
        &self.repo_dir
    }

    /// The index directory.
    pub fn index_dir(&self) -> &Path {
        &self.index_dir
    }

    /// The project path, if set.
    pub fn project_path(&self) -> Option<&Path> {
        self.project_path.as_deref()
    }

    // -- Prerequisites -------------------------------------------------------

    fn check_prerequisites() -> Result<(), String> {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return Err("git not found in PATH".to_string());
        }
        if std::process::Command::new("lake")
            .arg("--version")
            .output()
            .is_err()
        {
            return Err(
                "lake not found (install elan: https://github.com/leanprover/elan)".to_string(),
            );
        }
        Ok(())
    }

    // -- Repo management -----------------------------------------------------

    fn run_cmd(
        cmd: &[&str],
        cwd: &Path,
        _timeout_secs: u64,
    ) -> Result<std::process::Output, String> {
        let output = std::process::Command::new(cmd[0])
            .args(&cmd[1..])
            .current_dir(cwd)
            .env("LAKE_ARTIFACT_CACHE", "false")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("Failed to run `{}`: {e}", cmd.join(" ")))?;
        Ok(output)
    }

    fn clone_repo(&self) -> Result<(), String> {
        if self.repo_dir.exists() {
            return Ok(());
        }
        tracing::info!("Cloning loogle to {:?}...", self.repo_dir);
        std::fs::create_dir_all(&self.cache_dir)
            .map_err(|e| format!("Failed to create cache dir: {e}"))?;

        let output = Self::run_cmd(
            &[
                "git",
                "clone",
                "--depth",
                "1",
                REPO_URL,
                &self.repo_dir.to_string_lossy(),
            ],
            &self.cache_dir,
            300,
        )?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "Clone failed (exit {:?}): {}",
                output.status.code(),
                &stderr[..stderr.len().min(2000)]
            ));
        }
        Ok(())
    }

    fn build_loogle(&self) -> Result<(), String> {
        if self.is_installed() {
            return Ok(());
        }
        if !self.repo_dir.exists() {
            return Err("Repo directory does not exist".to_string());
        }
        tracing::info!("Downloading mathlib cache...");
        let _ = Self::run_cmd(&["lake", "exe", "cache", "get"], &self.repo_dir, 600);

        tracing::info!("Building loogle (this may take a few minutes)...");
        let output = Self::run_cmd(&["lake", "build"], &self.repo_dir, 900)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "Build failed (exit {:?}): {}",
                output.status.code(),
                &stderr[..stderr.len().min(2000)]
            ));
        }
        Ok(())
    }

    // -- Mathlib / toolchain helpers ------------------------------------------

    /// Read the mathlib git revision from `lake-manifest.json`.
    pub fn get_mathlib_version(&self) -> String {
        let manifest_path = self.repo_dir.join("lake-manifest.json");
        let content = match std::fs::read_to_string(&manifest_path) {
            Ok(c) => c,
            Err(_) => return "unknown".to_string(),
        };
        let manifest: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => return "unknown".to_string(),
        };
        manifest
            .get("packages")
            .and_then(|p| p.as_array())
            .and_then(|pkgs| {
                pkgs.iter().find_map(|pkg| {
                    if pkg.get("name")?.as_str()? == "mathlib" {
                        let rev = pkg.get("rev")?.as_str()?;
                        Some(rev[..rev.len().min(12)].to_string())
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_else(|| "unknown".to_string())
    }

    fn get_toolchain_version(&self) -> Option<String> {
        let tc_path = self.repo_dir.join("lean-toolchain");
        std::fs::read_to_string(tc_path)
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn get_project_toolchain(&self) -> Option<String> {
        let project_path = self.project_path.as_ref()?;
        let tc_path = project_path.join("lean-toolchain");
        std::fs::read_to_string(tc_path)
            .ok()
            .map(|s| s.trim().to_string())
    }

    // -- Project path discovery ----------------------------------------------

    /// Find `.lake/packages` lib paths from the user's project.
    ///
    /// Skips paths when the project and loogle toolchains differ (incompatible
    /// `.olean` files).
    pub fn discover_project_paths(&self) -> Vec<PathBuf> {
        let project_path = match &self.project_path {
            Some(p) => p,
            None => return vec![],
        };

        let loogle_tc = self.get_toolchain_version();
        let project_tc = self.get_project_toolchain();
        if let (Some(ref ltc), Some(ref ptc)) = (&loogle_tc, &project_tc) {
            if ltc != ptc {
                tracing::warn!(
                    "Toolchain mismatch: loogle uses {}, project uses {}. \
                     Skipping project paths (incompatible .olean files).",
                    ltc,
                    ptc,
                );
                return vec![];
            }
        }

        let mut paths = Vec::new();
        let lake_packages = project_path.join(".lake").join("packages");
        if lake_packages.exists() {
            if let Ok(entries) = std::fs::read_dir(&lake_packages) {
                for entry in entries.flatten() {
                    if !entry.path().is_dir() {
                        continue;
                    }
                    let lib_path = entry
                        .path()
                        .join(".lake")
                        .join("build")
                        .join("lib")
                        .join("lean");
                    if lib_path.exists() {
                        paths.push(lib_path);
                    }
                }
            }
        }
        let project_lib = project_path
            .join(".lake")
            .join("build")
            .join("lib")
            .join("lean");
        if project_lib.exists() {
            paths.push(project_lib);
        }
        paths.sort();
        paths
    }

    // -- Index management ----------------------------------------------------

    /// Compute the index file path based on mathlib version and extra paths.
    pub fn get_index_path(&self) -> PathBuf {
        let base = format!("mathlib-{}", self.get_mathlib_version());
        if self.extra_paths.is_empty() {
            self.index_dir.join(format!("{base}.idx"))
        } else {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let paths_str: String = self
                .extra_paths
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join(":");
            let mut hasher = DefaultHasher::new();
            paths_str.hash(&mut hasher);
            let hash = format!("{:016x}", hasher.finish());
            self.index_dir.join(format!("{base}-{}.idx", &hash[..8]))
        }
    }

    /// Remove old index files that don't match the current mathlib version.
    pub fn cleanup_old_indices(&self) {
        if !self.index_dir.exists() {
            return;
        }
        let current_prefix = format!("mathlib-{}", self.get_mathlib_version());
        if let Ok(entries) = std::fs::read_dir(&self.index_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.ends_with(".idx") && !name_str.starts_with(&current_prefix) {
                    if let Err(e) = std::fs::remove_file(entry.path()) {
                        tracing::warn!("Failed to remove old index {:?}: {e}", entry.path());
                    } else {
                        tracing::info!("Removed old index: {}", name_str);
                    }
                }
            }
        }
    }

    fn build_index(&self) -> Option<PathBuf> {
        let index_path = self.get_index_path();
        if index_path.exists() {
            return Some(index_path);
        }
        if !self.is_installed() {
            return None;
        }
        if let Err(e) = std::fs::create_dir_all(&self.index_dir) {
            tracing::error!("Failed to create index dir: {e}");
            return None;
        }
        self.cleanup_old_indices();

        let binary = self.binary_path();
        let mut cmd_args = vec![
            binary.to_string_lossy().to_string(),
            "--write-index".to_string(),
            index_path.to_string_lossy().to_string(),
            "--json".to_string(),
        ];
        for path in &self.extra_paths {
            cmd_args.push("--path".to_string());
            cmd_args.push(path.to_string_lossy().to_string());
        }
        cmd_args.push(String::new()); // Empty query for index building

        if !self.extra_paths.is_empty() {
            tracing::info!(
                "Building search index with {} extra paths...",
                self.extra_paths.len()
            );
        } else {
            tracing::info!("Building search index...");
        }

        let cmd_strs: Vec<&str> = cmd_args.iter().map(|s| s.as_str()).collect();
        match Self::run_cmd(&cmd_strs, &self.repo_dir, 600) {
            Ok(_) => {
                if index_path.exists() {
                    Some(index_path)
                } else {
                    None
                }
            }
            Err(e) => {
                tracing::error!("Index build error: {e}");
                None
            }
        }
    }

    // -- Public lifecycle methods --------------------------------------------

    /// Update project path and rediscover extra paths.
    /// Returns `true` if the discovered paths changed.
    pub fn set_project_path(&mut self, project_path: Option<PathBuf>) -> bool {
        self.project_path = project_path;
        let new_paths = self.discover_project_paths();
        if new_paths != self.extra_paths {
            self.extra_paths = new_paths;
            if !self.extra_paths.is_empty() {
                tracing::info!(
                    "Discovered {} project library paths",
                    self.extra_paths.len()
                );
            }
            true
        } else {
            false
        }
    }

    /// Clone repo, build binary, and create search index.
    ///
    /// This is a blocking operation that may take several minutes on first run.
    pub fn ensure_installed(&mut self) -> bool {
        if let Err(e) = Self::check_prerequisites() {
            tracing::warn!("Prerequisites: {e}");
            return false;
        }
        if let Err(e) = self.clone_repo() {
            tracing::error!("Clone failed: {e}");
            return false;
        }
        if let Err(e) = self.build_loogle() {
            tracing::error!("Build failed: {e}");
            return false;
        }
        self.extra_paths = self.discover_project_paths();
        if !self.extra_paths.is_empty() {
            tracing::info!("Indexing {} project library paths", self.extra_paths.len());
        }
        if self.build_index().is_none() {
            tracing::warn!("Index build failed, loogle will build on startup");
        }
        self.is_installed()
    }

    /// Start the interactive loogle subprocess.
    pub async fn start(&mut self) -> bool {
        if self.process.as_ref().is_some_and(|p| p.id().is_some()) {
            return self.ready;
        }

        if !self.is_installed() {
            tracing::error!("Loogle binary not found");
            return false;
        }

        // Check if project paths changed and rebuild index if needed
        if self.project_path.is_some() {
            let new_paths = self.discover_project_paths();
            if new_paths != self.extra_paths {
                self.extra_paths = new_paths;
                self.build_index();
            }
        }

        let binary = self.binary_path();
        let mut cmd = tokio::process::Command::new(&binary);
        cmd.args(["--json", "--interactive"]);

        let idx = self.get_index_path();
        if idx.exists() {
            cmd.args(["--read-index", &idx.to_string_lossy()]);
        }
        for path in &self.extra_paths {
            cmd.args(["--path", &path.to_string_lossy()]);
        }

        cmd.current_dir(&self.repo_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if !self.extra_paths.is_empty() {
            tracing::info!(
                "Starting loogle with {} extra paths...",
                self.extra_paths.len()
            );
        } else {
            tracing::info!("Starting loogle subprocess...");
        }

        match cmd.spawn() {
            Ok(mut child) => {
                let stdout = match child.stdout.take() {
                    Some(s) => s,
                    None => {
                        tracing::error!("No stdout from loogle process");
                        return false;
                    }
                };
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();

                match tokio::time::timeout(
                    std::time::Duration::from_secs(120),
                    reader.read_line(&mut line),
                )
                .await
                {
                    Ok(Ok(_)) if line.contains(READY_SIGNAL) => {
                        child.stdout = Some(reader.into_inner());
                        self.process = Some(child);
                        self.ready = true;
                        tracing::info!("Loogle ready");
                        true
                    }
                    Ok(Ok(_)) => {
                        tracing::error!("Loogle failed to start. stdout: {}", line.trim());
                        let _ = child.kill().await;
                        false
                    }
                    Ok(Err(e)) => {
                        tracing::error!("Failed to read loogle stdout: {e}");
                        let _ = child.kill().await;
                        false
                    }
                    Err(_) => {
                        tracing::error!("Loogle startup timeout");
                        let _ = child.kill().await;
                        false
                    }
                }
            }
            Err(e) => {
                tracing::error!("Failed to spawn loogle: {e}");
                false
            }
        }
    }

    /// Send a query to the running subprocess.
    ///
    /// Automatically restarts the subprocess if it has died (one retry).
    pub async fn query(&mut self, q: &str, num_results: usize) -> Result<Vec<Value>, String> {
        for attempt in 0..2 {
            if !self.ready || !self.process.as_ref().is_some_and(|p| p.id().is_some()) {
                if attempt > 0 {
                    return Err("Loogle subprocess not ready".to_string());
                }
                self.ready = false;
                if !self.start().await {
                    return Err("Failed to start loogle".to_string());
                }
                continue;
            }

            let proc = self.process.as_mut().unwrap();
            let stdin = proc
                .stdin
                .as_mut()
                .ok_or_else(|| "Loogle stdin not available".to_string())?;
            let stdout = proc
                .stdout
                .as_mut()
                .ok_or_else(|| "Loogle stdout not available".to_string())?;

            if let Err(e) = stdin.write_all(format!("{q}\n").as_bytes()).await {
                return Err(format!("Failed to write query: {e}"));
            }
            if let Err(e) = stdin.flush().await {
                return Err(format!("Failed to flush stdin: {e}"));
            }

            let mut reader = BufReader::new(stdout);
            let mut line = String::new();

            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                reader.read_line(&mut line),
            )
            .await
            {
                Ok(Ok(_)) => {
                    let response: Value = serde_json::from_str(line.trim())
                        .map_err(|e| format!("Invalid response: {e}"))?;

                    if let Some(err) = response.get("error").and_then(|e| e.as_str()) {
                        tracing::warn!("Query error: {err}");
                        return Ok(vec![]);
                    }

                    let hits = response
                        .get("hits")
                        .and_then(|h| h.as_array())
                        .cloned()
                        .unwrap_or_default();

                    let results: Vec<Value> = hits
                        .into_iter()
                        .take(num_results)
                        .map(|h| {
                            serde_json::json!({
                                "name": h.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                                "type": h.get("type").and_then(|v| v.as_str()).unwrap_or(""),
                                "module": h.get("module").and_then(|v| v.as_str()).unwrap_or(""),
                                "doc": h.get("doc").cloned(),
                            })
                        })
                        .collect();

                    return Ok(results);
                }
                Ok(Err(e)) => return Err(format!("Read error: {e}")),
                Err(_) => return Err("Query timeout".to_string()),
            }
        }

        Err("Loogle subprocess not ready".to_string())
    }

    /// Stop the subprocess.
    pub async fn stop(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.kill().await;
            match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
                Ok(_) => {}
                Err(_) => {
                    tracing::warn!("Loogle process did not exit after kill");
                }
            }
        }
        self.ready = false;
    }
}

impl Drop for LoogleManager {
    fn drop(&mut self) {
        if let Some(mut child) = self.process.take() {
            let _ = child.start_kill();
        }
    }
}

// ---------------------------------------------------------------------------
// URL encoding helper
// ---------------------------------------------------------------------------

/// Minimal URL-encoding module to avoid adding a crate dependency.
mod urlencoding {
    use std::fmt::Write;

    pub fn encode(input: &str) -> String {
        let mut out = String::with_capacity(input.len() * 3);
        for byte in input.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(byte as char)
                }
                _ => {
                    let _ = write!(out, "%{byte:02X}");
                }
            }
        }
        out
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

    /// Temporarily set env vars under a global lock (sync tests).
    fn with_env_vars<F, R>(vars: &[(&str, &str)], clear: &[&str], f: F) -> R
    where
        F: FnOnce() -> R + std::panic::UnwindSafe,
    {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        for (k, v) in vars {
            unsafe { std::env::set_var(k, v) };
        }
        for k in clear {
            unsafe { std::env::remove_var(k) };
        }
        let result = std::panic::catch_unwind(f);
        for (k, _) in vars {
            unsafe { std::env::remove_var(k) };
        }
        result.unwrap_or_else(|e| std::panic::resume_unwind(e))
    }

    /// RAII guard for env vars in async tests (avoids nested runtime).
    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        keys: Vec<String>,
    }

    impl EnvGuard {
        fn new(vars: &[(&str, &str)], clear: &[&str]) -> Self {
            let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            for (k, v) in vars {
                unsafe { std::env::set_var(k, v) };
            }
            for k in clear {
                unsafe { std::env::remove_var(k) };
            }
            let keys: Vec<String> = vars.iter().map(|(k, _)| k.to_string()).collect();
            Self { _lock: lock, keys }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for k in &self.keys {
                unsafe { std::env::remove_var(k) };
            }
        }
    }

    // ---- get_cache_dir -----------------------------------------------------

    #[test]
    fn cache_dir_from_env_var() {
        with_env_vars(
            &[("LEAN_LOOGLE_CACHE_DIR", "/custom/cache/loogle")],
            &[],
            || {
                let dir = get_cache_dir();
                assert_eq!(dir, PathBuf::from("/custom/cache/loogle"));
            },
        );
    }

    #[test]
    fn cache_dir_from_xdg() {
        with_env_vars(
            &[("XDG_CACHE_HOME", "/xdg/cache")],
            &["LEAN_LOOGLE_CACHE_DIR"],
            || {
                let dir = get_cache_dir();
                assert_eq!(dir, PathBuf::from("/xdg/cache/lean-lsp-mcp/loogle"));
            },
        );
    }

    #[test]
    fn cache_dir_default_fallback() {
        with_env_vars(&[], &["LEAN_LOOGLE_CACHE_DIR", "XDG_CACHE_HOME"], || {
            let dir = get_cache_dir();
            assert!(dir.to_string_lossy().ends_with("lean-lsp-mcp/loogle"));
        });
    }

    #[test]
    fn cache_dir_empty_env_var_ignored() {
        with_env_vars(&[("LEAN_LOOGLE_CACHE_DIR", "")], &[], || {
            let dir = get_cache_dir();
            assert!(dir.to_string_lossy().contains("lean-lsp-mcp"));
        });
    }

    // ---- LoogleManager construction ----------------------------------------

    #[test]
    fn manager_new_default_cache_dir() {
        let mgr = LoogleManager::new(None, None);
        assert!(mgr.cache_dir().to_string_lossy().contains("lean-lsp-mcp"));
        assert_eq!(mgr.repo_dir(), mgr.cache_dir().join("repo"));
        assert_eq!(mgr.index_dir(), mgr.cache_dir().join("index"));
        assert!(mgr.project_path().is_none());
        assert!(!mgr.is_running());
    }

    #[test]
    fn manager_new_custom_cache_dir() {
        let cache = PathBuf::from("/tmp/test-loogle");
        let project = PathBuf::from("/my/lean/project");
        let mgr = LoogleManager::new(Some(cache.clone()), Some(project.clone()));
        assert_eq!(mgr.cache_dir(), cache.as_path());
        assert_eq!(mgr.repo_dir(), cache.join("repo"));
        assert_eq!(mgr.index_dir(), cache.join("index"));
        assert_eq!(mgr.project_path(), Some(project.as_path()));
    }

    #[test]
    fn manager_binary_path() {
        let mgr = LoogleManager::new(Some(PathBuf::from("/cache")), None);
        let expected = PathBuf::from("/cache/repo/.lake/build/bin/loogle");
        assert_eq!(mgr.binary_path(), expected);
    }

    #[test]
    fn manager_is_installed_false_when_no_binary() {
        let mgr = LoogleManager::new(Some(PathBuf::from("/nonexistent")), None);
        assert!(!mgr.is_installed());
    }

    #[test]
    fn manager_is_running_false_initially() {
        let mgr = LoogleManager::new(None, None);
        assert!(!mgr.is_running());
        assert!(!mgr.ready);
    }

    // ---- Index path --------------------------------------------------------

    #[test]
    fn index_path_without_extra_paths() {
        let mgr = LoogleManager::new(Some(PathBuf::from("/cache")), None);
        let idx = mgr.get_index_path();
        assert!(idx.starts_with("/cache/index"));
        assert!(idx.to_string_lossy().contains("mathlib-"));
        assert!(idx.to_string_lossy().ends_with(".idx"));
    }

    #[test]
    fn index_path_with_extra_paths_includes_hash() {
        let mut mgr = LoogleManager::new(Some(PathBuf::from("/cache")), None);
        mgr.extra_paths = vec![PathBuf::from("/some/path")];
        let idx = mgr.get_index_path();
        let name = idx.file_name().unwrap().to_string_lossy();
        assert!(name.contains("mathlib-"));
        let parts: Vec<&str> = name.strip_suffix(".idx").unwrap().split('-').collect();
        assert!(parts.len() >= 3);
    }

    #[test]
    fn index_path_deterministic() {
        let mut mgr1 = LoogleManager::new(Some(PathBuf::from("/cache")), None);
        mgr1.extra_paths = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let mut mgr2 = LoogleManager::new(Some(PathBuf::from("/cache")), None);
        mgr2.extra_paths = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        assert_eq!(mgr1.get_index_path(), mgr2.get_index_path());
    }

    // ---- Cleanup old indices -----------------------------------------------

    #[test]
    fn cleanup_old_indices_nonexistent_dir_is_noop() {
        let mgr = LoogleManager::new(Some(PathBuf::from("/nonexistent")), None);
        mgr.cleanup_old_indices();
    }

    #[test]
    fn cleanup_old_indices_removes_old_files() {
        let tmp = tempfile::tempdir().unwrap();
        let index_dir = tmp.path().join("index");
        std::fs::create_dir_all(&index_dir).unwrap();

        std::fs::write(index_dir.join("mathlib-abc123.idx"), "old").unwrap();
        std::fs::write(index_dir.join("mathlib-def456.idx"), "also old").unwrap();
        std::fs::write(index_dir.join("mathlib-unknown.idx"), "current").unwrap();
        std::fs::write(index_dir.join("not-an-index.txt"), "keep").unwrap();

        let mut mgr = LoogleManager::new(Some(tmp.path().to_path_buf()), None);
        mgr.index_dir = index_dir.clone();
        mgr.cleanup_old_indices();

        assert!(index_dir.join("mathlib-unknown.idx").exists());
        assert!(index_dir.join("not-an-index.txt").exists());
        assert!(!index_dir.join("mathlib-abc123.idx").exists());
        assert!(!index_dir.join("mathlib-def456.idx").exists());
    }

    // ---- set_project_path --------------------------------------------------

    #[test]
    fn set_project_path_returns_false_when_unchanged() {
        let mut mgr = LoogleManager::new(None, None);
        let changed = mgr.set_project_path(None);
        assert!(!changed);
    }

    #[test]
    fn set_project_path_to_nonexistent_returns_no_paths() {
        let mut mgr = LoogleManager::new(None, None);
        let changed = mgr.set_project_path(Some(PathBuf::from("/nonexistent/project")));
        assert!(!changed);
    }

    // ---- mathlib version ---------------------------------------------------

    #[test]
    fn get_mathlib_version_returns_unknown_when_no_manifest() {
        let mgr = LoogleManager::new(Some(PathBuf::from("/nonexistent")), None);
        assert_eq!(mgr.get_mathlib_version(), "unknown");
    }

    #[test]
    fn get_mathlib_version_parses_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let manifest = serde_json::json!({
            "packages": [
                {"name": "mathlib", "rev": "abcdef1234567890"},
                {"name": "other", "rev": "xxx"}
            ]
        });
        std::fs::write(
            repo.join("lake-manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let mgr = LoogleManager::new(Some(tmp.path().to_path_buf()), None);
        assert_eq!(mgr.get_mathlib_version(), "abcdef123456");
    }

    // ---- URL encoding ------------------------------------------------------

    #[test]
    fn urlencoding_basic() {
        assert_eq!(urlencoding::encode("hello"), "hello");
        assert_eq!(urlencoding::encode("a b"), "a%20b");
    }

    #[test]
    fn urlencoding_special_chars() {
        assert_eq!(urlencoding::encode("a+b"), "a%2Bb");
        assert_eq!(urlencoding::encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencoding::encode("100%"), "100%25");
    }

    // ---- discover_project_paths -------------------------------------------

    #[test]
    fn discover_project_paths_no_project() {
        let mgr = LoogleManager::new(None, None);
        assert!(mgr.discover_project_paths().is_empty());
    }

    #[test]
    fn discover_project_paths_with_packages() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let pkg_lib = project
            .join(".lake")
            .join("packages")
            .join("mathlib")
            .join(".lake")
            .join("build")
            .join("lib")
            .join("lean");
        std::fs::create_dir_all(&pkg_lib).unwrap();

        let mgr = LoogleManager::new(None, Some(project));
        let paths = mgr.discover_project_paths();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].to_string_lossy().contains("mathlib"));
    }

    // ---- Remote mode (wiremock) -------------------------------------------

    #[tokio::test]
    async fn loogle_remote_success() {
        let server = wiremock::MockServer::start().await;

        let response_body = serde_json::json!({
            "hits": [
                {"name": "Nat.add_comm", "type": "forall (n m : Nat), n + m = m + n", "module": "Init.Data.Nat.Basic"},
                {"name": "Nat.mul_comm", "type": "forall (n m : Nat), n * m = m * n", "module": "Init.Data.Nat.Basic"},
                {"name": "Int.add_comm", "type": "forall (a b : Int), a + b = b + a", "module": "Init.Data.Int.Basic"}
            ]
        });

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/json"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&response_body))
            .mount(&server)
            .await;

        let _guard = EnvGuard::new(&[("LOOGLE_URL", &server.uri())], &["LOOGLE_HEADERS"]);
        let results = loogle_remote("add_comm", 2).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].name, "Nat.add_comm");
        assert_eq!(results[0].module, "Init.Data.Nat.Basic");
        assert!(results[0].r#type.contains("forall"));
    }

    #[tokio::test]
    async fn loogle_remote_no_hits() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/json"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"error": "parse error"})),
            )
            .mount(&server)
            .await;

        let _guard = EnvGuard::new(&[("LOOGLE_URL", &server.uri())], &["LOOGLE_HEADERS"]);
        let result = loogle_remote("???invalid???", 5).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No results"));
    }

    #[tokio::test]
    async fn loogle_remote_empty_hits() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/json"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"hits": []})),
            )
            .mount(&server)
            .await;

        let _guard = EnvGuard::new(&[("LOOGLE_URL", &server.uri())], &["LOOGLE_HEADERS"]);
        let results = loogle_remote("something", 5).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn loogle_remote_with_extra_headers() {
        let server = wiremock::MockServer::start().await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/json"))
            .and(wiremock::matchers::header("X-Custom", "test-value"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"hits": [{"name": "found", "type": "T", "module": "M"}]}),
            ))
            .mount(&server)
            .await;

        let headers = serde_json::json!({"X-Custom": "test-value"}).to_string();
        let _guard = EnvGuard::new(
            &[("LOOGLE_URL", &server.uri()), ("LOOGLE_HEADERS", &headers)],
            &[],
        );
        let results = loogle_remote("test", 5).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "found");
    }

    #[tokio::test]
    async fn loogle_remote_network_error() {
        let _guard = EnvGuard::new(&[("LOOGLE_URL", "http://127.0.0.1:1")], &["LOOGLE_HEADERS"]);
        let result = loogle_remote("anything", 5).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("loogle error"));
    }

    // ---- Stop when no process is noop -------------------------------------

    #[tokio::test]
    async fn stop_when_no_process_is_noop() {
        let mut mgr = LoogleManager::new(None, None);
        mgr.stop().await;
        assert!(!mgr.is_running());
    }

    // ---- Drop when no process is safe -------------------------------------

    #[test]
    fn drop_when_no_process_is_safe() {
        let mgr = LoogleManager::new(None, None);
        drop(mgr);
    }

    // ---- Send + Sync assertions -------------------------------------------

    #[test]
    fn loogle_result_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LoogleResult>();
    }
}
