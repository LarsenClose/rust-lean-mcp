//! MCP server setup and tool routing.
//!
//! Defines [`AppContext`] for shared server state and implements the rmcp
//! `ServerHandler` trait with all 23 tool handlers wired to the MCP protocol.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lean_lsp_client::client::LspClient;
use lean_mcp_core::instructions::INSTRUCTIONS;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, InitializeResult, ServerCapabilities};
use rmcp::schemars;
use rmcp::schemars::JsonSchema;
use rmcp::{tool, tool_handler, tool_router};
use serde::Deserialize;

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
}

#[derive(Deserialize, JsonSchema)]
pub struct TermGoalParams {
    #[schemars(description = "Absolute or project-root-relative path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Line number (1-indexed)")]
    pub line: u32,
    #[schemars(description = "Column (1-indexed, defaults to end of line)")]
    pub column: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct HoverParams {
    #[schemars(description = "Absolute or project-root-relative path to Lean file")]
    pub file_path: String,
    #[schemars(description = "Line number (1-indexed)")]
    pub line: u32,
    #[schemars(description = "Column at START of identifier (1-indexed)")]
    pub column: u32,
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

// ---------------------------------------------------------------------------
// AppContext
// ---------------------------------------------------------------------------

/// Shared application state for the MCP server.
///
/// Holds configuration and runtime handles that tools need: the LSP client
/// for interacting with `lean --server`, search endpoint configuration, and
/// the Lean project path.
#[derive(Clone)]
pub struct AppContext {
    /// Path to the Lean project root, if configured.
    pub lean_project_path: Option<PathBuf>,
    /// LSP client for communicating with the Lean server.
    pub lsp_client: Option<Arc<dyn LspClient>>,
    /// Search endpoint configuration (URLs for leansearch, loogle, etc.).
    pub search_config: SearchConfig,
    /// Tool router for rmcp tool dispatch.
    tool_router: ToolRouter<Self>,
}

impl std::fmt::Debug for AppContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppContext")
            .field("lean_project_path", &self.lean_project_path)
            .field("lsp_client", &self.lsp_client.is_some())
            .finish()
    }
}

impl AppContext {
    /// Create an [`AppContext`] with no Lean project path or LSP client.
    pub fn new() -> Self {
        Self {
            lean_project_path: None,
            lsp_client: None,
            search_config: SearchConfig::default(),
            tool_router: Self::tool_router(),
        }
    }

    /// Create an [`AppContext`] with the given project path and search config.
    pub fn with_options(lean_project_path: Option<PathBuf>, search_config: SearchConfig) -> Self {
        Self {
            lean_project_path,
            lsp_client: None,
            search_config,
            tool_router: Self::tool_router(),
        }
    }

    /// Get the LSP client, returning an error string if not connected.
    fn require_client(&self) -> Result<&dyn LspClient, String> {
        self.lsp_client
            .as_deref()
            .ok_or_else(|| "LSP client not connected. Run lean_build first.".to_string())
    }

    /// Get the project path, returning an error string if not configured.
    fn require_project_path(&self) -> Result<&Path, String> {
        self.lean_project_path
            .as_deref()
            .ok_or_else(|| "No Lean project path configured.".to_string())
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
// Tool routing (23 tools)
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
        let project_path = self.require_project_path()?;
        tools::build::handle_build(
            project_path,
            params.clean.unwrap_or(false),
            params.output_lines.unwrap_or(20),
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
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
        let client = self.require_client()?;
        tools::outline::handle_file_outline(client, &params.file_path, params.max_declarations)
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
        let client = self.require_client()?;
        tools::diagnostics::handle_diagnostics(
            client,
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
        let client = self.require_client()?;
        tools::goal::handle_lean_goal(client, &params.file_path, params.line, params.column)
            .await
            .map(|r| Self::to_json(&r))
            .map_err(|e| e.to_string())
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
        let client = self.require_client()?;
        tools::goal::handle_lean_term_goal(client, &params.file_path, params.line, params.column)
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
        let client = self.require_client()?;
        tools::hover::handle_lean_hover(client, &params.file_path, params.line, params.column)
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
        let client = self.require_client()?;
        tools::completions::handle_lean_completions(
            client,
            &params.file_path,
            params.line,
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
        let client = self.require_client()?;
        tools::declarations::handle_declaration_file(client, &params.file_path, &params.symbol)
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
        let client = self.require_client()?;
        tools::references::handle_references(client, &params.file_path, params.line, params.column)
            .await
            .map(|r| Self::to_json(&r))
            .map_err(|e| e.to_string())
    }

    // ---- Multi-Attempt ----

    #[tool(
        name = "lean_multi_attempt",
        description = "Try multiple tactics without modifying file. Returns goal state for each."
    )]
    async fn lean_multi_attempt(
        &self,
        Parameters(params): Parameters<MultiAttemptParams>,
    ) -> Result<String, String> {
        let client = self.require_client()?;
        tools::multi_attempt::handle_multi_attempt(
            client,
            None,
            &params.file_path,
            params.line,
            &params.snippets,
            params.column,
        )
        .await
        .map(|r| Self::to_json(&r))
        .map_err(|e| e.to_string())
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
        let client = self.require_client()?;
        let project_path = self.require_project_path()?;
        tools::run_code::handle_run_code(client, project_path, &params.code)
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
        let client = self.require_client()?;
        tools::verify::handle_verify(
            client,
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
        let root = params
            .project_root
            .map(PathBuf::from)
            .or_else(|| self.lean_project_path.clone());
        let root = root
            .as_deref()
            .ok_or_else(|| "No project path available for local search.".to_string())?;
        lean_mcp_core::search_utils::lean_local_search(
            &params.query,
            params.limit.unwrap_or(10),
            root,
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
        let client = self.require_client()?;
        tools::search::handle_state_search(
            client,
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
        let client = self.require_client()?;
        tools::search::handle_hammer_premise(
            client,
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
        let client = self.require_client()?;
        tools::code_actions::handle_code_actions(client, &params.file_path, params.line)
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
        let client = self.require_client()?;
        tools::widgets::handle_get_widgets(client, &params.file_path, params.line, params.column)
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
        let client = self.require_client()?;
        tools::widgets::handle_get_widget_source(client, &params.file_path, &params.javascript_hash)
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
        let project_path = self.require_project_path()?;
        let file = PathBuf::from(&params.file_path);
        tools::profile::handle_profile_proof(
            &file,
            params.line,
            project_path,
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
        let client = self.require_client()?;
        tools::batch_goals::handle_lean_goals_batch(client, params.positions)
            .await
            .map(|r| Self::to_json(&r))
            .map_err(|e| e.to_string())
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

    #[test]
    fn app_context_new_has_no_project_path() {
        let ctx = AppContext::new();
        assert!(ctx.lean_project_path.is_none());
    }

    #[test]
    fn app_context_default_matches_new() {
        let ctx = AppContext::default();
        assert!(ctx.lean_project_path.is_none());
    }

    #[test]
    fn app_context_with_project_path() {
        let ctx = AppContext {
            lean_project_path: Some(PathBuf::from("/tmp/lean-project")),
            lsp_client: None,
            search_config: SearchConfig::default(),
            tool_router: AppContext::tool_router(),
        };
        assert_eq!(
            ctx.lean_project_path.as_deref(),
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
    fn require_client_returns_error_when_none() {
        let ctx = AppContext::new();
        assert!(ctx.require_client().is_err());
    }

    #[test]
    fn require_project_path_returns_error_when_none() {
        let ctx = AppContext::new();
        assert!(ctx.require_project_path().is_err());
    }

    #[test]
    fn require_project_path_returns_ok_when_set() {
        let ctx = AppContext {
            lean_project_path: Some(PathBuf::from("/tmp/test")),
            lsp_client: None,
            search_config: SearchConfig::default(),
            tool_router: AppContext::tool_router(),
        };
        assert!(ctx.require_project_path().is_ok());
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
        assert!(debug.contains("lsp_client"));
    }
}
