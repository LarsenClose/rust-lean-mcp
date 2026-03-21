//! MCP server setup and tool routing.
//!
//! Defines [`AppContext`] for shared server state and implements the rmcp
//! `ServerHandler` trait with all 28 tool handlers wired to the MCP protocol.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use lean_lsp_client::client::LspClient;
use lean_lsp_client::lean_client::LeanLspClient;
use lean_lsp_client::pool::LspClientPool;
use lean_mcp_core::instructions::INSTRUCTIONS;
use lean_mcp_core::models::AttemptResult;
use lean_mcp_core::task_manager::TaskManager;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, InitializeResult, ServerCapabilities};
use rmcp::schemars;
use rmcp::schemars::JsonSchema;
use rmcp::{tool, tool_handler, tool_router};
use serde::Deserialize;
use tokio::io::BufReader;
use tokio::process::Command;

use crate::tools;
use tools::search::SearchConfig;

// ---------------------------------------------------------------------------
// Tool parameter structs
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct BuildParams {
    #[schemars(description = "Run `lake clean` first (slow)")]
    pub clean: Option<bool>,
    #[schemars(description = "Return last N lines of build log (0=none)")]
    pub output_lines: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct FileOutlineParams {
    #[schemars(description = "Absolute or project-root-relative path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Max declarations to return")]
    pub max_declarations: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct DiagnosticParams {
    #[schemars(description = "Absolute or project-root-relative path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Filter from line (1-indexed)")]
    pub start_line: Option<u32>,
    #[schemars(description = "Filter to line (1-indexed)")]
    pub end_line: Option<u32>,
    #[schemars(description = "Filter by severity: error, warning, information, hint")]
    pub severity: Option<String>,
    #[schemars(description = "Filter to a specific declaration (slow)")]
    pub declaration_name: Option<String>,
    #[schemars(description = "Return verbose nested TaggedText with embedded widgets")]
    pub interactive: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GoalParams {
    #[schemars(description = "Absolute or project-root-relative path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Line number (1-indexed)")]
    pub line: u32,
    #[schemars(description = "Column (1-indexed). Omit for before/after")]
    pub column: Option<u32>,
    #[schemars(description = "Declaration name to resolve line from (overrides line)")]
    pub declaration_name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TermGoalParams {
    #[schemars(description = "Absolute or project-root-relative path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Line number (1-indexed)")]
    pub line: u32,
    #[schemars(description = "Column (1-indexed, defaults to end of line)")]
    pub column: Option<u32>,
    #[schemars(description = "Declaration name to resolve line from (overrides line)")]
    pub declaration_name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct HoverParams {
    #[schemars(description = "Absolute or project-root-relative path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Line number (1-indexed)")]
    pub line: u32,
    #[schemars(description = "Column at START of identifier (1-indexed)")]
    pub column: u32,
    #[schemars(description = "Declaration name to resolve line from (overrides line)")]
    pub declaration_name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CompletionsParams {
    #[schemars(description = "Absolute or project-root-relative path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Line number (1-indexed)")]
    pub line: u32,
    #[schemars(description = "Column number (1-indexed)")]
    pub column: u32,
    #[schemars(description = "Max completions to return")]
    pub max_completions: Option<usize>,
    #[schemars(description = "Declaration name to resolve line from (overrides line)")]
    pub declaration_name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct DeclarationParams {
    #[schemars(description = "Absolute or project-root-relative path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Symbol name (case sensitive, must be in file)")]
    pub symbol: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReferencesParams {
    #[schemars(description = "Absolute path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Line number (1-indexed)")]
    pub line: u32,
    #[schemars(description = "Column at START of identifier (1-indexed)")]
    pub column: u32,
    #[schemars(description = "Declaration name to resolve line from (overrides line)")]
    pub declaration_name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MultiAttemptParams {
    #[schemars(description = "Absolute or project-root-relative path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Line number (1-indexed)")]
    pub line: u32,
    #[schemars(description = "Tactics to try (3+ recommended)")]
    pub snippets: Vec<String>,
    #[schemars(description = "Column (1-indexed). Omit to target the tactic line")]
    pub column: Option<u32>,
    #[schemars(
        description = "When true, test each snippet via independent temp files (no file mutation, concurrent execution). Omit or false for default REPL/LSP path"
    )]
    pub parallel: Option<bool>,
    #[schemars(
        description = "Max seconds per snippet (returns 'timeout' for slow tactics). Only applies to parallel mode"
    )]
    pub timeout_per_snippet: Option<f64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MultiAttemptAsyncParams {
    #[schemars(description = "Absolute or project-root-relative path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Line number (1-indexed)")]
    pub line: u32,
    #[schemars(description = "Tactics to try (3+ recommended)")]
    pub snippets: Vec<String>,
    #[schemars(description = "Column (1-indexed). Omit to target the tactic line")]
    pub column: Option<u32>,
    #[schemars(description = "Max seconds per snippet (returns 'timeout' for slow tactics)")]
    pub timeout_per_snippet: Option<f64>,
    #[schemars(
        description = "Use independent temp files per snippet (slow but isolated). Default: false (uses warm LSP)"
    )]
    pub isolated: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct RunCodeParams {
    #[schemars(description = "Self-contained Lean code with imports")]
    pub code: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct VerifyParams {
    #[schemars(description = "Absolute path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Fully qualified name (e.g. `Namespace.theorem`)")]
    pub theorem_name: String,
    #[schemars(description = "Scan source file for suspicious patterns")]
    pub scan_source: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct LocalSearchParams {
    #[schemars(description = "Declaration name or prefix")]
    pub query: String,
    #[schemars(description = "Max matches")]
    pub limit: Option<usize>,
    #[schemars(description = "Project root (inferred if omitted)")]
    pub project_root: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct LeanSearchParams {
    #[schemars(description = "Natural language or Lean term query")]
    pub query: String,
    #[schemars(description = "Max results")]
    pub num_results: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct LoogleParams {
    #[schemars(description = "Type pattern, constant, or name substring")]
    pub query: String,
    #[schemars(description = "Max results")]
    pub num_results: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct LeanFinderParams {
    #[schemars(description = "Mathematical concept or proof state")]
    pub query: String,
    #[schemars(description = "Max results")]
    pub num_results: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct StateSearchParams {
    #[schemars(description = "Absolute or project-root-relative path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Line number (1-indexed)")]
    pub line: u32,
    #[schemars(description = "Column number (1-indexed)")]
    pub column: u32,
    #[schemars(description = "Max results")]
    pub num_results: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct HammerPremiseParams {
    #[schemars(description = "Absolute or project-root-relative path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Line number (1-indexed)")]
    pub line: u32,
    #[schemars(description = "Column number (1-indexed)")]
    pub column: u32,
    #[schemars(description = "Max results")]
    pub num_results: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct CodeActionsParams {
    #[schemars(description = "Absolute path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Line number (1-indexed)")]
    pub line: u32,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetWidgetsParams {
    #[schemars(description = "Absolute path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Line number (1-indexed)")]
    pub line: u32,
    #[schemars(description = "Column number (1-indexed)")]
    pub column: u32,
}

#[derive(Deserialize, JsonSchema)]
pub struct WidgetSourceParams {
    #[schemars(description = "Absolute path to Lean file")]
    pub file_path: String,
    #[schemars(description = "javascriptHash from a widget instance")]
    pub javascript_hash: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProfileProofParams {
    #[schemars(description = "Absolute or project-root-relative path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Line where theorem starts (1-indexed)")]
    pub line: u32,
    #[schemars(description = "Number of slowest lines to return")]
    pub top_n: Option<usize>,
    #[schemars(description = "Max seconds to wait")]
    pub timeout: Option<f64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct BatchGoalParams {
    #[schemars(description = "Array of {file_path, line, column?} positions to query")]
    pub positions: Vec<lean_mcp_core::models::BatchGoalPosition>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProofDiffParams {
    #[schemars(description = "Absolute or project-root-relative path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Line before the tactic (1-indexed)")]
    pub before_line: u32,
    #[schemars(description = "Column on before_line (1-indexed, defaults to end of line)")]
    pub before_column: Option<u32>,
    #[schemars(description = "Line after the tactic (1-indexed)")]
    pub after_line: u32,
    #[schemars(description = "Column on after_line (1-indexed, defaults to end of line)")]
    pub after_column: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct BatchParams {
    #[schemars(description = "Array of {tool_name, arguments} calls to execute concurrently")]
    pub calls: Vec<lean_mcp_core::models::BatchCall>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectHealthParams {
    #[schemars(description = "Fetch goal states at sorry positions (slow, requires LSP)")]
    pub include_goals: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ServerHealthParams {}

#[derive(Deserialize, JsonSchema)]
pub struct TaskResultParams {
    #[schemars(description = "Task ID returned by lean_multi_attempt_async")]
    pub task_id: String,
    #[schemars(description = "Set true to cancel the task and abort remaining work")]
    pub cancel: Option<bool>,
}

// ---------------------------------------------------------------------------
// AppContext
// ---------------------------------------------------------------------------

/// Shared application state for the MCP server.
///
/// Holds configuration and runtime handles that tools need: LSP clients
/// for interacting with `lean --server`, search endpoint configuration, and
/// the Lean project path. Supports per-project LSP clients and auto-detection
/// of Lean project roots from file paths or CWD.
#[derive(Clone)]
pub struct AppContext {
    /// Explicit project path from CLI/env (takes precedence over detection).
    explicit_project_path: Option<PathBuf>,
    /// Per-project LSP client pools, keyed by canonicalized project root.
    clients: Arc<tokio::sync::RwLock<HashMap<PathBuf, Arc<LspClientPool>>>>,
    /// Cached CWD-based project detection result.
    cwd_project: Arc<OnceLock<Option<PathBuf>>>,
    /// Search endpoint configuration (URLs for leansearch, loogle, etc.).
    pub search_config: SearchConfig,
    /// Background task manager for async multi-attempt polling.
    pub task_manager: Arc<TaskManager<AttemptResult>>,
    /// Tool router for rmcp tool dispatch.
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for AppContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppContext")
            .field("explicit_project_path", &self.explicit_project_path)
            .field(
                "client_count",
                &self.clients.try_read().map(|c| c.len()).unwrap_or(0),
            )
            .field("task_manager", &"TaskManager<AttemptResult>")
            .finish()
    }
}

impl AppContext {
    /// Create an [`AppContext`] with no Lean project path or LSP client.
    pub fn new() -> Self {
        Self {
            explicit_project_path: None,
            clients: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            cwd_project: Arc::new(OnceLock::new()),
            search_config: SearchConfig::default(),
            task_manager: Arc::new(TaskManager::new(Duration::from_secs(300))),
            tool_router: Self::tool_router(),
        }
    }

    /// Create an [`AppContext`] with the given project path and search config.
    pub fn with_options(lean_project_path: Option<PathBuf>, search_config: SearchConfig) -> Self {
        Self {
            explicit_project_path: lean_project_path,
            clients: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            cwd_project: Arc::new(OnceLock::new()),
            search_config,
            task_manager: Arc::new(TaskManager::new(Duration::from_secs(300))),
            tool_router: Self::tool_router(),
        }
    }

    /// Resolve the Lean project path using the fallback chain:
    /// 1. Explicit CLI path (always wins)
    /// 2. Auto-detect from file_path (walk up looking for project markers)
    /// 3. Auto-detect from CWD (cached)
    /// 4. Error
    pub fn resolve_project_path(&self, file_path: Option<&str>) -> Result<PathBuf, String> {
        // 1. Explicit path
        if let Some(ref pp) = self.explicit_project_path {
            return Ok(pp.clone());
        }
        // 2. Detect from file_path
        if let Some(fp) = file_path {
            let path = Path::new(fp);
            let abs_path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir().unwrap_or_default().join(path)
            };
            if let Some(project) = lean_mcp_core::file_utils::detect_lean_project(&abs_path) {
                return Ok(project);
            }
        }
        // 3. Detect from CWD (cached)
        let cwd_result = self.cwd_project.get_or_init(|| {
            std::env::current_dir()
                .ok()
                .and_then(|cwd| lean_mcp_core::file_utils::detect_lean_project(&cwd))
        });
        if let Some(ref project) = cwd_result {
            return Ok(project.clone());
        }
        // 4. Error
        Err(
            "No Lean project path configured and auto-detection failed. \
             Pass --lean-project-path or run from within a Lean project directory."
                .to_string(),
        )
    }

    /// Get or create an LSP client pool for a specific project.
    async fn ensure_client_for(&self, project_path: &Path) -> Result<Arc<LspClientPool>, String> {
        // Fast path: read lock
        {
            let clients = self.clients.read().await;
            if let Some(pool) = clients.get(project_path) {
                return Ok(pool.clone());
            }
        }
        // Slow path: write lock with double-check
        let mut clients = self.clients.write().await;
        if let Some(pool) = clients.get(project_path) {
            return Ok(pool.clone());
        }
        let pool = spawn_lsp_pool(project_path.to_path_buf()).await?;
        clients.insert(project_path.to_path_buf(), pool.clone());
        Ok(pool)
    }

    /// Evict and shut down the LSP client for a project.
    ///
    /// Called after `lean_build` so the next tool call spawns a fresh
    /// `lake serve` that loads the newly-built oleans.
    async fn evict_client(&self, project_path: &Path) {
        let old = {
            let mut clients = self.clients.write().await;
            clients.remove(project_path)
        };
        if let Some(client) = old {
            let _ = client.shutdown().await;
        }
    }

    /// Convenience: resolve project from file_path, then get client.
    async fn client_for_file(&self, file_path: &str) -> Result<Arc<LspClientPool>, String> {
        let project_path = self.resolve_project_path(Some(file_path))?;
        self.ensure_client_for(&project_path).await
    }

    /// Convenience: resolve default project, then get client (for tools without file_path).
    async fn client_default(&self) -> Result<Arc<LspClientPool>, String> {
        let project_path = self.resolve_project_path(None)?;
        self.ensure_client_for(&project_path).await
    }

    /// Serialize a result to JSON, falling back to the Debug representation.
    fn to_json<T: serde::Serialize>(result: &T) -> String {
        serde_json::to_string(result).unwrap_or_else(|e| format!("Serialization error: {e}"))
    }
}

impl Default for AppContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn `lake serve` and create a connected [`LeanLspClient`].
async fn spawn_lsp_client(project_path: PathBuf) -> Result<Arc<dyn LspClient>, String> {
    tracing::info!("Spawning `lake serve` in {}", project_path.display());

    let mut child = Command::new("lake")
        .arg("serve")
        .current_dir(&project_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn `lake serve`: {e}"))?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Failed to capture stdin of `lake serve`".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture stdout of `lake serve`".to_string())?;

    let reader = BufReader::new(stdout);
    let client = LeanLspClient::new(project_path, reader, stdin)
        .await
        .map_err(|e| format!("Failed to initialize LSP client: {e}"))?;

    tracing::info!("LSP client connected");
    Ok(Arc::new(client) as Arc<dyn LspClient>)
}

/// Spawn an auto-scaling pool of `lake serve` instances for a project.
///
/// Starts with a single instance and grows on demand up to [`compute_max_instances`].
async fn spawn_lsp_pool(project_path: PathBuf) -> Result<Arc<LspClientPool>, String> {
    let max = compute_max_instances();
    tracing::info!(
        "Creating LSP pool for {} (max {max} instances)",
        project_path.display()
    );

    let initial = spawn_lsp_client(project_path.clone()).await?;

    let pp = project_path.clone();
    let spawner: lean_lsp_client::pool::ClientSpawner = Box::new(move || {
        let pp = pp.clone();
        Box::pin(async move { spawn_lsp_client(pp).await })
    });

    let pool = LspClientPool::new(project_path, initial, max, spawner);
    Ok(Arc::new(pool))
}

/// Compute the maximum number of LSP instances based on system resources.
///
/// Uses CPU count and available memory. Can be overridden via
/// `LEAN_MCP_MAX_INSTANCES` environment variable.
fn compute_max_instances() -> usize {
    // Check env override first
    if let Ok(val) = std::env::var("LEAN_MCP_MAX_INSTANCES") {
        if let Ok(n) = val.parse::<usize>() {
            if n >= 1 {
                tracing::info!("LEAN_MCP_MAX_INSTANCES override: {n}");
                return n;
            }
        }
    }

    // CPU-based cap: half of available parallelism
    let cpu_cap = std::thread::available_parallelism()
        .map(|p| p.get() / 2)
        .unwrap_or(2)
        .max(1);

    // Memory-based cap: ~2 GB per lake serve instance
    let mem_cap = available_memory_gb()
        .map(|gb| (gb / 2.0) as usize)
        .unwrap_or(4)
        .max(1);

    let cap = cpu_cap.min(mem_cap).clamp(1, 8);
    tracing::info!("Auto-computed max LSP instances: {cap} (cpu_cap={cpu_cap}, mem_cap={mem_cap})");
    cap
}

/// Best-effort detection of available system memory in GB.
#[cfg(target_os = "macos")]
fn available_memory_gb() -> Option<f64> {
    let output = std::process::Command::new("sysctl")
        .arg("-n")
        .arg("hw.memsize")
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&output.stdout);
    let bytes: u64 = s.trim().parse().ok()?;
    Some(bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

#[cfg(target_os = "linux")]
fn available_memory_gb() -> Option<f64> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb_str = rest.trim().trim_end_matches("kB").trim();
            let kb: u64 = kb_str.parse().ok()?;
            return Some(kb as f64 / (1024.0 * 1024.0));
        }
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn available_memory_gb() -> Option<f64> {
    None
}

// ---------------------------------------------------------------------------
// Server metadata helpers
// ---------------------------------------------------------------------------

/// The server name advertised to MCP clients.
pub fn server_name() -> &'static str {
    "Lean LSP"
}

/// The server version, pulled from this crate's Cargo.toml at compile time.
pub fn server_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The instructions string sent to MCP clients.
pub fn server_instructions() -> &'static str {
    INSTRUCTIONS
}

// ---------------------------------------------------------------------------
// Declaration-name resolution helper
// ---------------------------------------------------------------------------

/// Resolve `declaration_name` to its start line via document symbols, falling
/// back to the provided `line` when no declaration name is given.
async fn resolve_line(
    client: &dyn LspClient,
    file_path: &str,
    line: u32,
    declaration_name: Option<&str>,
) -> Result<u32, String> {
    match declaration_name {
        Some(name) => tools::symbol_resolve::resolve_declaration_line(client, file_path, name)
            .await
            .map_err(|e| e.to_string()),
        None => Ok(line),
    }
}

// ---------------------------------------------------------------------------
// Tool routing (24 tools)
// ---------------------------------------------------------------------------

#[tool_router]
impl AppContext {
    // ---- Build / Project Management ----

    #[tool(
        name = "lean_build",
        description = "Build the Lean project and restart LSP. Use only if needed (e.g. new imports). SLOW."
    )]
    async fn lean_build(
        &self,
        Parameters(params): Parameters<BuildParams>,
    ) -> Result<String, String> {
        let project_path = self.resolve_project_path(None)?;

        // Shut down the old LSP client before building so it doesn't hold
        // file locks or stale state.
        self.evict_client(&project_path).await;

        let result = tools::build::handle_build(
            &project_path,
            params.clean.unwrap_or(false),
            params.output_lines.unwrap_or(20),
        )
        .await
        .map_err(|e| e.to_string())?;

        // The old client was evicted above. The next tool call will spawn a
        // fresh `lake serve` that loads the newly-built oleans.
        // (No need to eagerly respawn — ensure_client_for handles it lazily.)

        Ok(Self::to_json(&result))
    }

    // ---- Project Health ----

    #[tool(
        name = "lean_project_health",
        description = "Scan project for sorry occurrences, error patterns, and file count. Fast ripgrep scan by default; set include_goals=true for slow LSP goal queries at sorry positions."
    )]
    async fn lean_project_health(
        &self,
        Parameters(params): Parameters<ProjectHealthParams>,
    ) -> Result<String, String> {
        let project_path = self.resolve_project_path(None)?;
        let include_goals = params.include_goals.unwrap_or(false);
        let client = if include_goals {
            Some(self.ensure_client_for(&project_path).await?)
        } else {
            None
        };
        tools::project_health::handle_project_health(
            &project_path,
            client.as_ref().map(|c| c.as_ref() as &dyn LspClient),
            include_goals,
        )
        .await
        .map(|r| Self::to_json(&r))
    }

    // ---- File Outline ----

    #[tool(
        name = "lean_file_outline",
        description = "Get imports and declarations with type signatures. Token-efficient."
    )]
    async fn lean_file_outline(
        &self,
        Parameters(params): Parameters<FileOutlineParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;
        tools::outline::handle_file_outline(
            client.as_ref(),
            &params.file_path,
            params.max_declarations,
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    // ---- Diagnostics ----

    #[tool(
        name = "lean_diagnostic_messages",
        description = "Get compiler diagnostics (errors, warnings, infos) for a Lean file."
    )]
    async fn lean_diagnostic_messages(
        &self,
        Parameters(params): Parameters<DiagnosticParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;
        let project_path = self.resolve_project_path(Some(&params.file_path))?;
        tools::diagnostics::handle_diagnostics(
            client.as_ref(),
            &project_path,
            &params.file_path,
            params.start_line,
            params.end_line,
            params.declaration_name.as_deref(),
            params.interactive.unwrap_or(false),
            params.severity.as_deref(),
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    // ---- Proof Goals ----

    #[tool(
        name = "lean_goal",
        description = "Get proof goals at a position. MOST IMPORTANT tool - use often! Omit column for before/after view. \"no goals\" = proof complete."
    )]
    async fn lean_goal(
        &self,
        Parameters(params): Parameters<GoalParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;
        let line = resolve_line(
            client.as_ref(),
            &params.file_path,
            params.line,
            params.declaration_name.as_deref(),
        )
        .await?;
        tools::goal::handle_lean_goal(client.as_ref(), &params.file_path, line, params.column)
            .await
            .map(|r| Self::to_json(&r))
            .map_err(|e| e.to_string())
    }

    // ---- Proof Diff ----

    #[tool(
        name = "lean_proof_diff",
        description = "Compare proof state before/after a tactic. Returns goals and hypotheses added/removed."
    )]
    async fn lean_proof_diff(
        &self,
        Parameters(params): Parameters<ProofDiffParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;
        tools::proof_diff::handle_lean_proof_diff(
            client.as_ref(),
            &params.file_path,
            params.before_line,
            params.before_column,
            params.after_line,
            params.after_column,
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    // ---- Generic Batch ----

    #[tool(
        name = "lean_batch",
        description = "Execute multiple tool calls concurrently. Returns partial results on individual failures. Cannot call lean_batch recursively."
    )]
    async fn lean_batch(
        &self,
        Parameters(params): Parameters<BatchParams>,
    ) -> Result<String, String> {
        let project_path = self.resolve_project_path(None).ok();
        let client = match &project_path {
            Some(pp) => self.ensure_client_for(pp).await.ok(),
            None => None,
        };
        let result = tools::batch::handle_batch(
            params.calls,
            client.as_ref().map(|c| c.as_ref() as &dyn LspClient),
            project_path.as_deref(),
            &self.search_config,
        )
        .await;
        Ok(Self::to_json(&result))
    }

    // ---- Term Goal ----

    #[tool(
        name = "lean_term_goal",
        description = "Get the expected type at a position."
    )]
    async fn lean_term_goal(
        &self,
        Parameters(params): Parameters<TermGoalParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;
        let line = resolve_line(
            client.as_ref(),
            &params.file_path,
            params.line,
            params.declaration_name.as_deref(),
        )
        .await?;
        tools::goal::handle_lean_term_goal(client.as_ref(), &params.file_path, line, params.column)
            .await
            .map(|r| Self::to_json(&r))
            .map_err(|e| e.to_string())
    }

    // ---- Hover Info ----

    #[tool(
        name = "lean_hover_info",
        description = "Get type signature and docs for a symbol. Essential for understanding APIs."
    )]
    async fn lean_hover_info(
        &self,
        Parameters(params): Parameters<HoverParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;
        let line = resolve_line(
            client.as_ref(),
            &params.file_path,
            params.line,
            params.declaration_name.as_deref(),
        )
        .await?;
        tools::hover::handle_lean_hover(client.as_ref(), &params.file_path, line, params.column)
            .await
            .map(|r| Self::to_json(&r))
            .map_err(|e| e.to_string())
    }

    // ---- Completions ----

    #[tool(
        name = "lean_completions",
        description = "Get IDE autocompletions. Use on INCOMPLETE code (after `.` or partial name)."
    )]
    async fn lean_completions(
        &self,
        Parameters(params): Parameters<CompletionsParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;
        let line = resolve_line(
            client.as_ref(),
            &params.file_path,
            params.line,
            params.declaration_name.as_deref(),
        )
        .await?;
        tools::completions::handle_lean_completions(
            client.as_ref(),
            &params.file_path,
            line,
            params.column,
            params.max_completions.unwrap_or(32),
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    // ---- Declaration Source ----

    #[tool(
        name = "lean_declaration_file",
        description = "Get file where a symbol is declared. Symbol must be present in file first."
    )]
    async fn lean_declaration_file(
        &self,
        Parameters(params): Parameters<DeclarationParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;
        tools::declarations::handle_declaration_file(
            client.as_ref(),
            &params.file_path,
            &params.symbol,
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    // ---- References ----

    #[tool(
        name = "lean_references",
        description = "Find all references to a symbol (including the declaration). Position cursor at the symbol."
    )]
    async fn lean_references(
        &self,
        Parameters(params): Parameters<ReferencesParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;
        let line = resolve_line(
            client.as_ref(),
            &params.file_path,
            params.line,
            params.declaration_name.as_deref(),
        )
        .await?;
        tools::references::handle_references(
            client.as_ref(),
            &params.file_path,
            line,
            params.column,
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    // ---- Multi-Attempt ----

    #[tool(
        name = "lean_multi_attempt",
        description = "Try multiple tactics without modifying file. Returns goal state for each. Set timeout_per_snippet (seconds) in parallel mode to cap slow tactics."
    )]
    async fn lean_multi_attempt(
        &self,
        Parameters(params): Parameters<MultiAttemptParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;
        tools::multi_attempt::handle_multi_attempt(
            client.as_ref(),
            None,
            &params.file_path,
            params.line,
            &params.snippets,
            params.column,
            params.parallel,
            params.timeout_per_snippet,
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    // ---- Multi-Attempt Async ----

    #[tool(
        name = "lean_multi_attempt_async",
        description = "Submit background tactic attempts. Returns task_id immediately. Poll with lean_task_result. Uses warm LSP by default (fast). Set isolated=true for independent temp files."
    )]
    async fn lean_multi_attempt_async(
        &self,
        Parameters(params): Parameters<MultiAttemptAsyncParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;

        // Ensure file is open before reading content (#90)
        client
            .open_file(&params.file_path)
            .await
            .map_err(|e| e.to_string())?;

        let content = client
            .get_file_content(&params.file_path)
            .await
            .map_err(|e| e.to_string())?;

        let lines: Vec<&str> = content.lines().collect();
        if params.line == 0 || params.line as usize > lines.len() {
            return Err(format!(
                "Line {} out of range (file has {} lines)",
                params.line,
                lines.len()
            ));
        }

        // Create task
        let (task_id, cancel_token) = self.task_manager.create_task(params.snippets.len()).await;

        let isolated = params.isolated.unwrap_or(false);
        let timeout = params.timeout_per_snippet;

        if isolated {
            // Isolated path: spawn each snippet as an independent temp-file task
            let project_path = client.project_path().to_path_buf();
            let base_code = lines[..params.line as usize - 1].join("\n");
            let target_line = lines[(params.line - 1) as usize];
            let indent_len = target_line.find(|c: char| !c.is_whitespace()).unwrap_or(0);
            let indent = target_line[..indent_len].to_string();

            for (i, snippet) in params.snippets.iter().enumerate() {
                let client = client.clone();
                let project_path = project_path.clone();
                let base_code = base_code.clone();
                let indent = indent.clone();
                let task_manager = self.task_manager.clone();
                let task_id = task_id.clone();
                let snippet = snippet.clone();
                let cancel_token = cancel_token.clone();

                tokio::spawn(async move {
                    use lean_mcp_core::task_manager::ItemStatus;

                    if cancel_token.is_cancelled() {
                        return;
                    }

                    let result = crate::tools::multi_attempt::run_snippet_isolated(
                        client.as_ref(),
                        &project_path,
                        &snippet,
                        &base_code,
                        &indent,
                        timeout,
                    )
                    .await;

                    task_manager
                        .update_item(&task_id, i, ItemStatus::Completed { result })
                        .await;
                });
            }
        } else {
            // Default warm LSP path: single background task, sequential edit-and-restore
            let file_path = params.file_path.clone();
            let original_content = content.clone();
            let line = params.line;
            let snippets = params.snippets.clone();
            let task_manager = self.task_manager.clone();
            let task_id_clone = task_id.clone();

            let line_text = lines[(line - 1) as usize];
            let target_col = match params.column {
                Some(c) if c > 0 => c - 1, // 1-indexed to 0-indexed
                _ => line_text.find(|c: char| !c.is_whitespace()).unwrap_or(0) as u32,
            };

            tokio::spawn(async move {
                use lean_mcp_core::task_manager::ItemStatus;

                for (i, snippet) in snippets.iter().enumerate() {
                    if cancel_token.is_cancelled() {
                        break;
                    }

                    let result = crate::tools::multi_attempt::run_one_snippet_warm(
                        client.as_ref(),
                        &file_path,
                        &original_content,
                        line,
                        target_col,
                        snippet,
                        timeout,
                    )
                    .await;

                    task_manager
                        .update_item(&task_id_clone, i, ItemStatus::Completed { result })
                        .await;
                }

                // Final restore + force reopen
                let _ = client
                    .update_file_content(&file_path, &original_content)
                    .await;
                let _ = client.open_file_force(&file_path).await;
            });
        }

        // Return immediately with task ID
        Ok(serde_json::to_string(&serde_json::json!({
            "task_id": task_id,
            "status": "submitted",
            "total": params.snippets.len()
        }))
        .unwrap())
    }

    // ---- Run Code ----

    #[tool(
        name = "lean_run_code",
        description = "Run a code snippet and return diagnostics. Must include all imports."
    )]
    async fn lean_run_code(
        &self,
        Parameters(params): Parameters<RunCodeParams>,
    ) -> Result<String, String> {
        let project_path = self.resolve_project_path(None)?;
        let client = self.ensure_client_for(&project_path).await?;
        tools::run_code::handle_run_code(client.as_ref(), &project_path, &params.code)
            .await
            .map(|r| Self::to_json(&r))
            .map_err(|e| e.to_string())
    }

    // ---- Verify Theorem ----

    #[tool(
        name = "lean_verify",
        description = "Check theorem axioms + optional source scan. Only scans the given file, not imports."
    )]
    async fn lean_verify(
        &self,
        Parameters(params): Parameters<VerifyParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;
        tools::verify::handle_verify(
            client.as_ref(),
            &params.file_path,
            &params.theorem_name,
            params.scan_source.unwrap_or(true),
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    // ---- Local Search ----

    #[tool(
        name = "lean_local_search",
        description = "Fast local search to verify declarations exist. Use BEFORE trying a lemma name."
    )]
    async fn lean_local_search(
        &self,
        Parameters(params): Parameters<LocalSearchParams>,
    ) -> Result<String, String> {
        let root = match params.project_root {
            Some(r) => PathBuf::from(r),
            None => self.resolve_project_path(None)?,
        };
        lean_mcp_core::search_utils::lean_local_search(
            &params.query,
            params.limit.unwrap_or(10),
            &root,
        )
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    // ---- LeanSearch (remote) ----

    #[tool(
        name = "lean_leansearch",
        description = "Search Mathlib via leansearch.net using natural language. Rate limited: 3 req/30s."
    )]
    async fn lean_leansearch(
        &self,
        Parameters(params): Parameters<LeanSearchParams>,
    ) -> Result<String, String> {
        tools::search::handle_leansearch(
            &params.query,
            params.num_results.unwrap_or(5),
            &self.search_config,
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    // ---- Loogle (remote) ----

    #[tool(
        name = "lean_loogle",
        description = "Search Mathlib by type signature via loogle.lean-lang.org. Rate limited."
    )]
    async fn lean_loogle(
        &self,
        Parameters(params): Parameters<LoogleParams>,
    ) -> Result<String, String> {
        tools::search::handle_loogle_remote(
            &params.query,
            params.num_results.unwrap_or(8),
            &self.search_config,
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    // ---- Lean Finder (remote) ----

    #[tool(
        name = "lean_leanfinder",
        description = "Semantic search by mathematical meaning via Lean Finder. Rate limited: 10 req/30s."
    )]
    async fn lean_leanfinder(
        &self,
        Parameters(params): Parameters<LeanFinderParams>,
    ) -> Result<String, String> {
        tools::search::handle_leanfinder(
            &params.query,
            params.num_results.unwrap_or(5),
            &self.search_config,
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    // ---- State Search (remote, needs LSP) ----

    #[tool(
        name = "lean_state_search",
        description = "Find lemmas to close the goal at a position. Rate limited: 6 req/30s."
    )]
    async fn lean_state_search(
        &self,
        Parameters(params): Parameters<StateSearchParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;
        tools::search::handle_state_search(
            client.as_ref(),
            &params.file_path,
            params.line,
            params.column,
            params.num_results.unwrap_or(5),
            &self.search_config,
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    // ---- Hammer Premises (remote, needs LSP) ----

    #[tool(
        name = "lean_hammer_premise",
        description = "Get premise suggestions for automation tactics at a goal position. Rate limited: 6 req/30s."
    )]
    async fn lean_hammer_premise(
        &self,
        Parameters(params): Parameters<HammerPremiseParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;
        tools::search::handle_hammer_premise(
            client.as_ref(),
            &params.file_path,
            params.line,
            params.column,
            params.num_results.unwrap_or(32),
            &self.search_config,
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    // ---- Code Actions ----

    #[tool(
        name = "lean_code_actions",
        description = "Get LSP code actions for a line. Returns resolved edits for TryThis suggestions and quick fixes."
    )]
    async fn lean_code_actions(
        &self,
        Parameters(params): Parameters<CodeActionsParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;
        tools::code_actions::handle_code_actions(client.as_ref(), &params.file_path, params.line)
            .await
            .map(|r| Self::to_json(&r))
            .map_err(|e| e.to_string())
    }

    // ---- Widgets ----

    #[tool(
        name = "lean_get_widgets",
        description = "Get panel widgets at a position (proof visualizations, custom widgets). May be large."
    )]
    async fn lean_get_widgets(
        &self,
        Parameters(params): Parameters<GetWidgetsParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;
        tools::widgets::handle_get_widgets(
            client.as_ref(),
            &params.file_path,
            params.line,
            params.column,
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    #[tool(
        name = "lean_get_widget_source",
        description = "Get JavaScript source of a widget by hash. May be large."
    )]
    async fn lean_get_widget_source(
        &self,
        Parameters(params): Parameters<WidgetSourceParams>,
    ) -> Result<String, String> {
        let client = self.client_for_file(&params.file_path).await?;
        tools::widgets::handle_get_widget_source(
            client.as_ref(),
            &params.file_path,
            &params.javascript_hash,
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    // ---- Profile Proof ----

    #[tool(
        name = "lean_profile_proof",
        description = "Run `lean --profile` on a theorem. Returns per-line timing and categories. SLOW."
    )]
    async fn lean_profile_proof(
        &self,
        Parameters(params): Parameters<ProfileProofParams>,
    ) -> Result<String, String> {
        let project_path = self.resolve_project_path(Some(&params.file_path))?;
        let file = PathBuf::from(&params.file_path);
        tools::profile::handle_profile_proof(
            &file,
            params.line,
            &project_path,
            params.timeout.unwrap_or(30.0),
            params.top_n.unwrap_or(5),
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
    }

    // ---- Batch Goals ----

    #[tool(
        name = "lean_goals_batch",
        description = "Get proof goals at multiple positions concurrently. Returns partial results on individual failures."
    )]
    async fn lean_goals_batch(
        &self,
        Parameters(params): Parameters<BatchGoalParams>,
    ) -> Result<String, String> {
        let client = self.client_default().await?;
        tools::batch_goals::handle_lean_goals_batch(client.as_ref(), params.positions)
            .await
            .map(|r| Self::to_json(&r))
            .map_err(|e| e.to_string())
    }

    // ---- Task Result (background task polling) ----

    #[tool(
        name = "lean_task_result",
        description = "Poll background task status. Returns partial results as snippets complete. Set cancel=true to abort."
    )]
    async fn lean_task_result(
        &self,
        Parameters(params): Parameters<TaskResultParams>,
    ) -> Result<String, String> {
        // Handle cancellation
        if params.cancel == Some(true) {
            let cancelled = self.task_manager.cancel_task(&params.task_id).await;
            if !cancelled {
                return Err(format!("Task '{}' not found", params.task_id));
            }
            // Get final snapshot after cancellation
            let snapshot = self
                .task_manager
                .get_task(&params.task_id)
                .await
                .ok_or_else(|| format!("Task '{}' not found", params.task_id))?;
            return Ok(Self::to_json(&snapshot));
        }

        // Run cleanup on each poll (cheap, keeps memory bounded)
        self.task_manager.cleanup_expired().await;

        // Get task snapshot
        let snapshot = self
            .task_manager
            .get_task(&params.task_id)
            .await
            .ok_or_else(|| format!("Task '{}' not found or expired", params.task_id))?;

        Ok(Self::to_json(&snapshot))
    }

    // ---- Server Health ----

    #[tool(
        name = "lean_server_health",
        description = "Get server status: active LSP sessions, pending tasks, and uptime."
    )]
    async fn lean_server_health(
        &self,
        Parameters(_params): Parameters<ServerHealthParams>,
    ) -> Result<String, String> {
        let clients = self.clients.read().await;
        let mut sessions = Vec::new();

        for (path, pool) in clients.iter() {
            sessions.push(serde_json::json!({
                "project_path": path.to_string_lossy(),
                "status": "active",
                "pool_size": pool.instance_count().await,
                "max_pool_size": pool.max_instances(),
                "in_flight": pool.in_flight_counts().await,
                "affinity_entries": pool.affinity_entry_count().await,
            }));
        }
        drop(clients);

        let result = serde_json::json!({
            "active_sessions": sessions.len(),
            "sessions": sessions,
            "server_version": server_version(),
        });

        Ok(Self::to_json(&result))
    }
}

// ---------------------------------------------------------------------------
// ServerHandler implementation
// ---------------------------------------------------------------------------

#[tool_handler]
impl rmcp::ServerHandler for AppContext {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(Implementation::new(server_name(), server_version()))
        .with_instructions(server_instructions())
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

    #[test]
    fn app_context_new_has_no_project_path() {
        let ctx = AppContext::new();
        assert!(ctx.explicit_project_path.is_none());
    }

    #[test]
    fn app_context_default_matches_new() {
        let ctx = AppContext::default();
        assert!(ctx.explicit_project_path.is_none());
    }

    #[test]
    fn app_context_with_project_path() {
        let ctx = AppContext::with_options(
            Some(PathBuf::from("/tmp/lean-project")),
            SearchConfig::default(),
        );
        assert_eq!(
            ctx.explicit_project_path.as_deref(),
            Some(std::path::Path::new("/tmp/lean-project"))
        );
    }

    #[test]
    fn server_name_returns_lean_lsp() {
        assert_eq!(server_name(), "Lean LSP");
    }

    #[test]
    fn server_version_is_not_empty() {
        let version = server_version();
        assert!(!version.is_empty());
    }

    #[test]
    fn server_instructions_contains_key_sections() {
        let instructions = server_instructions();
        assert!(instructions.contains("## General Rules"));
        assert!(instructions.contains("## Key Tools"));
        assert!(instructions.contains("## Search Tools"));
        assert!(instructions.contains("## Search Decision Tree"));
        assert!(instructions.contains("## Return Formats"));
        assert!(instructions.contains("## Error Handling"));
    }

    #[test]
    fn get_info_returns_correct_server_metadata() {
        let ctx = AppContext::new();
        let info = rmcp::ServerHandler::get_info(&ctx);
        assert_eq!(info.server_info.name, "Lean LSP");
        assert!(!info.server_info.version.is_empty());
        assert!(info.instructions.is_some());
        assert!(info.instructions.as_ref().unwrap().contains("## Key Tools"));
    }

    #[test]
    fn get_info_advertises_tools_capability() {
        let ctx = AppContext::new();
        let info = rmcp::ServerHandler::get_info(&ctx);
        assert!(
            info.capabilities.tools.is_some(),
            "server should advertise tools capability"
        );
        assert_eq!(
            info.capabilities.tools.as_ref().unwrap().list_changed,
            Some(true),
            "tools capability should have list_changed = true"
        );
    }

    #[test]
    fn to_json_serializes_simple_value() {
        let val = serde_json::json!({"key": "value"});
        let json = AppContext::to_json(&val);
        assert!(json.contains("key"));
        assert!(json.contains("value"));
    }

    #[test]
    fn app_context_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AppContext>();
    }

    #[test]
    fn app_context_debug_format() {
        let ctx = AppContext::new();
        let debug = format!("{:?}", ctx);
        assert!(debug.contains("AppContext"));
        assert!(debug.contains("explicit_project_path"));
        assert!(debug.contains("client_count"));
    }

    // ---- resolve_project_path tests ----

    #[test]
    fn resolve_project_path_explicit_takes_precedence() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("lakefile.lean"), "-- lakefile").unwrap();
        let explicit = PathBuf::from("/explicit/path");
        let ctx = AppContext::with_options(Some(explicit.clone()), SearchConfig::default());
        // Even with a file_path in a real project, explicit wins
        let file_in_project = dir.path().join("Foo.lean");
        fs::write(&file_in_project, "-- code").unwrap();
        let result = ctx
            .resolve_project_path(Some(file_in_project.to_str().unwrap()))
            .unwrap();
        assert_eq!(result, explicit);
    }

    #[test]
    fn resolve_project_path_detects_from_file_path() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("src");
        fs::create_dir_all(&sub).unwrap();
        fs::write(dir.path().join("lakefile.lean"), "-- lakefile").unwrap();
        let file = sub.join("Foo.lean");
        fs::write(&file, "-- code").unwrap();
        let ctx = AppContext::new();
        let result = ctx
            .resolve_project_path(Some(file.to_str().unwrap()))
            .unwrap();
        assert_eq!(
            result.canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn resolve_project_path_detects_from_cwd() {
        // When run from the repo root, CWD has lean-toolchain, so detection succeeds.
        // This test validates that the CWD fallback path works.
        let ctx = AppContext::new();
        // The rust-lsp-mcp repo itself has a lean-toolchain file,
        // so running from the repo root should detect it.
        // If it doesn't (different CI env), we just check the method doesn't panic.
        let result = ctx.resolve_project_path(None);
        // We can't guarantee CWD is in a Lean project, so just check it returns something
        // or returns the expected error message.
        match result {
            Ok(path) => assert!(path.exists()),
            Err(e) => assert!(e.contains("auto-detection failed")),
        }
    }

    #[test]
    fn resolve_project_path_error_when_nothing_found() {
        // Use a tempdir that definitely has no Lean project markers above it.
        let dir = TempDir::new().unwrap();
        let ctx = AppContext {
            explicit_project_path: None,
            clients: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            // Pre-populate the CWD cache with None to prevent real CWD detection
            cwd_project: Arc::new(OnceLock::new()),
            search_config: SearchConfig::default(),
            task_manager: Arc::new(TaskManager::new(Duration::from_secs(300))),
            tool_router: AppContext::tool_router(),
        };
        // Force the CWD cache to None
        let _ = ctx.cwd_project.set(None);
        // Use a file path in the tempdir (no markers)
        let file = dir.path().join("Foo.lean");
        fs::write(&file, "-- code").unwrap();
        // The temp dir (/tmp/...) should have no Lean project markers
        let result = ctx.resolve_project_path(Some(file.to_str().unwrap()));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("auto-detection failed"));
    }

    #[test]
    fn resolve_project_path_cwd_result_is_cached() {
        let ctx = AppContext::new();
        // Pre-populate the CWD cache
        let test_path = PathBuf::from("/test/cached/path");
        let _ = ctx.cwd_project.set(Some(test_path.clone()));
        // Without explicit path or file_path, should return the cached value
        let result = ctx.resolve_project_path(None).unwrap();
        assert_eq!(result, test_path);
    }

    // ---- lean_task_result tests ----

    use lean_mcp_core::task_manager::{ItemStatus, TaskStatus};

    #[tokio::test]
    async fn task_result_not_found() {
        let ctx = AppContext::new();
        let result = ctx.task_manager.get_task("nonexistent-id").await;
        assert!(result.is_none(), "Nonexistent task should return None");
    }

    #[tokio::test]
    async fn task_result_returns_snapshot() {
        let ctx = AppContext::new();
        let (task_id, _token) = ctx.task_manager.create_task(3).await;

        ctx.task_manager
            .update_item(
                &task_id,
                0,
                ItemStatus::Completed {
                    result: AttemptResult {
                        snippet: "simp".into(),
                        goals: vec!["|- True".into()],
                        diagnostics: vec![],
                        timed_out: false,
                    },
                },
            )
            .await;
        ctx.task_manager
            .update_item(
                &task_id,
                1,
                ItemStatus::Failed {
                    error: "timeout".into(),
                },
            )
            .await;

        let snapshot = ctx.task_manager.get_task(&task_id).await.unwrap();
        assert_eq!(snapshot.task_id, task_id);
        assert_eq!(snapshot.status, TaskStatus::Running);
        assert_eq!(snapshot.total, 3);
        assert_eq!(snapshot.completed_count, 2);
        assert!(
            matches!(&snapshot.items[0], ItemStatus::Completed { result } if result.snippet == "simp")
        );
        assert!(matches!(&snapshot.items[1], ItemStatus::Failed { error } if error == "timeout"));
        assert!(matches!(&snapshot.items[2], ItemStatus::Pending));
    }

    #[tokio::test]
    async fn task_result_cancel() {
        let ctx = AppContext::new();
        let (task_id, token) = ctx.task_manager.create_task(2).await;

        assert!(!token.is_cancelled());
        let cancelled = ctx.task_manager.cancel_task(&task_id).await;
        assert!(cancelled);
        assert!(token.is_cancelled());

        let snapshot = ctx.task_manager.get_task(&task_id).await.unwrap();
        assert_eq!(snapshot.status, TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn task_result_cancel_unknown_task() {
        let ctx = AppContext::new();
        let cancelled = ctx.task_manager.cancel_task("does-not-exist").await;
        assert!(!cancelled, "Cancelling unknown task should return false");
    }

    #[tokio::test]
    async fn task_result_expired_task_not_found() {
        let tm: TaskManager<AttemptResult> = TaskManager::new(Duration::from_millis(0));
        let (task_id, _token) = tm.create_task(1).await;

        tm.update_item(
            &task_id,
            0,
            ItemStatus::Completed {
                result: AttemptResult {
                    snippet: "ring".into(),
                    goals: vec![],
                    diagnostics: vec![],
                    timed_out: false,
                },
            },
        )
        .await;

        tokio::time::sleep(Duration::from_millis(5)).await;

        tm.cleanup_expired().await;
        assert!(
            tm.get_task(&task_id).await.is_none(),
            "Expired task should not be found after cleanup"
        );
    }

    #[tokio::test]
    async fn task_result_serializes_attempt_results() {
        let ctx = AppContext::new();
        let (task_id, _token) = ctx.task_manager.create_task(2).await;

        ctx.task_manager
            .update_item(
                &task_id,
                0,
                ItemStatus::Completed {
                    result: AttemptResult {
                        snippet: "simp".into(),
                        goals: vec!["|- True".into()],
                        diagnostics: vec![],
                        timed_out: false,
                    },
                },
            )
            .await;
        ctx.task_manager
            .update_item(
                &task_id,
                1,
                ItemStatus::Completed {
                    result: AttemptResult {
                        snippet: "ring".into(),
                        goals: vec![],
                        diagnostics: vec![],
                        timed_out: false,
                    },
                },
            )
            .await;

        let snapshot = ctx.task_manager.get_task(&task_id).await.unwrap();
        let json = AppContext::to_json(&snapshot);

        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["task_id"], task_id);
        assert_eq!(v["status"], "completed");
        assert_eq!(v["total"], 2);
        assert_eq!(v["completed_count"], 2);

        let items = v["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["status"], "Completed");
        assert_eq!(items[0]["result"]["snippet"], "simp");
        assert_eq!(items[1]["status"], "Completed");
        assert_eq!(items[1]["result"]["snippet"], "ring");
    }

    #[test]
    fn app_context_debug_includes_task_manager() {
        let ctx = AppContext::new();
        let debug = format!("{:?}", ctx);
        assert!(
            debug.contains("task_manager"),
            "Debug output should include task_manager"
        );
    }

    // ---- lean_multi_attempt_async integration tests ----

    #[tokio::test]
    async fn task_manager_in_app_context() {
        let ctx = AppContext::new();
        let (task_id, _token) = ctx.task_manager.create_task(3).await;
        let snap = ctx.task_manager.get_task(&task_id).await.unwrap();
        assert_eq!(snap.total, 3);
        assert_eq!(snap.completed_count, 0);
    }

    #[tokio::test]
    async fn task_manager_update_and_poll() {
        use lean_mcp_core::models::DiagnosticMessage;

        let ctx = AppContext::new();
        let (task_id, _token) = ctx.task_manager.create_task(2).await;

        ctx.task_manager
            .update_item(
                &task_id,
                0,
                ItemStatus::Completed {
                    result: AttemptResult {
                        snippet: "simp".to_string(),
                        goals: vec!["no goals".to_string()],
                        diagnostics: Vec::new(),
                        timed_out: false,
                    },
                },
            )
            .await;

        let snap = ctx.task_manager.get_task(&task_id).await.unwrap();
        assert_eq!(snap.completed_count, 1);
        assert_eq!(snap.status, TaskStatus::Running);

        ctx.task_manager
            .update_item(
                &task_id,
                1,
                ItemStatus::Completed {
                    result: AttemptResult {
                        snippet: "ring".to_string(),
                        goals: Vec::new(),
                        diagnostics: vec![DiagnosticMessage {
                            severity: "error".to_string(),
                            message: "tactic 'ring' failed".to_string(),
                            line: 1,
                            column: 1,
                        }],
                        timed_out: false,
                    },
                },
            )
            .await;

        let snap = ctx.task_manager.get_task(&task_id).await.unwrap();
        assert_eq!(snap.completed_count, 2);
        assert_eq!(snap.status, TaskStatus::Completed);
    }

    // ---- lean_server_health tests ----

    #[tokio::test]
    async fn server_health_returns_valid_json() {
        let ctx = AppContext::new();
        let result = ctx
            .lean_server_health(Parameters(ServerHealthParams {}))
            .await
            .expect("lean_server_health should succeed");

        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("result should be valid JSON");
        assert!(
            parsed.get("active_sessions").is_some(),
            "response should contain active_sessions"
        );
        assert!(
            parsed.get("sessions").is_some(),
            "response should contain sessions"
        );
        assert!(parsed["sessions"].is_array(), "sessions should be an array");
        assert!(
            parsed.get("server_version").is_some(),
            "response should contain server_version"
        );
    }

    #[tokio::test]
    async fn server_health_no_sessions_initially() {
        let ctx = AppContext::new();
        let result = ctx
            .lean_server_health(Parameters(ServerHealthParams {}))
            .await
            .expect("lean_server_health should succeed");

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            parsed["active_sessions"], 0,
            "fresh server should have 0 active sessions"
        );
        assert_eq!(
            parsed["sessions"].as_array().unwrap().len(),
            0,
            "fresh server should have empty sessions array"
        );
    }

    #[tokio::test]
    async fn server_health_shows_version() {
        let ctx = AppContext::new();
        let result = ctx
            .lean_server_health(Parameters(ServerHealthParams {}))
            .await
            .expect("lean_server_health should succeed");

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let version = parsed["server_version"]
            .as_str()
            .expect("server_version should be a string");
        assert!(!version.is_empty(), "server_version should be non-empty");
        // Should match the compile-time version
        assert_eq!(version, server_version());
    }
}
