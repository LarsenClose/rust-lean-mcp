//! Generic batch dispatch: run multiple tool calls concurrently.

use std::path::{Path, PathBuf};

use lean_lsp_client::client::LspClient;
use lean_mcp_core::models::{BatchCall, BatchCallResult, BatchResult};
use serde::Deserialize;

use super::search::SearchConfig;

/// Execute a batch of tool calls concurrently, returning partial results.
///
/// Self-recursion (`lean_batch`) is blocked to prevent unbounded fan-out.
pub async fn handle_batch(
    calls: Vec<BatchCall>,
    client: Option<&dyn LspClient>,
    project_path: Option<&Path>,
    search_config: &SearchConfig,
) -> BatchResult {
    let futs: Vec<_> = calls
        .into_iter()
        .map(|call| {
            let sc = search_config.clone();
            let pp = project_path.map(|p| p.to_path_buf());
            async move { dispatch_one(call, client, pp.as_deref(), &sc).await }
        })
        .collect();

    let items = futures::future::join_all(futs).await;
    BatchResult { items }
}

async fn dispatch_one(
    call: BatchCall,
    client: Option<&dyn LspClient>,
    project_path: Option<&Path>,
    search_config: &SearchConfig,
) -> BatchCallResult {
    match dispatch_inner(
        &call.tool_name,
        &call.arguments,
        client,
        project_path,
        search_config,
    )
    .await
    {
        Ok(value) => BatchCallResult {
            tool_name: call.tool_name,
            result: Some(value),
            is_error: false,
            error: None,
        },
        Err(e) => BatchCallResult {
            tool_name: call.tool_name,
            result: None,
            is_error: true,
            error: Some(e),
        },
    }
}

// ---------------------------------------------------------------------------
// Per-tool argument structs (kept private to this module)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GoalArgs {
    file_path: String,
    line: u32,
    column: Option<u32>,
}

#[derive(Deserialize)]
struct TermGoalArgs {
    file_path: String,
    line: u32,
    column: Option<u32>,
}

#[derive(Deserialize)]
struct HoverArgs {
    file_path: String,
    line: u32,
    column: u32,
}

#[derive(Deserialize)]
struct CompletionsArgs {
    file_path: String,
    line: u32,
    column: u32,
    max_completions: Option<usize>,
}

#[derive(Deserialize)]
struct DiagnosticsArgs {
    file_path: String,
    start_line: Option<u32>,
    end_line: Option<u32>,
    severity: Option<String>,
    declaration_name: Option<String>,
    interactive: Option<bool>,
}

#[derive(Deserialize)]
struct OutlineArgs {
    file_path: String,
    max_declarations: Option<usize>,
}

#[derive(Deserialize)]
struct DeclarationArgs {
    file_path: String,
    symbol: String,
}

#[derive(Deserialize)]
struct ReferencesArgs {
    file_path: String,
    line: u32,
    column: u32,
}

#[derive(Deserialize)]
struct MultiAttemptArgs {
    file_path: String,
    line: u32,
    snippets: Vec<String>,
    column: Option<u32>,
    parallel: Option<bool>,
}

#[derive(Deserialize)]
struct RunCodeArgs {
    code: String,
}

#[derive(Deserialize)]
struct VerifyArgs {
    file_path: String,
    theorem_name: String,
    scan_source: Option<bool>,
}

#[derive(Deserialize)]
struct BuildArgs {
    clean: Option<bool>,
    output_lines: Option<usize>,
}

#[derive(Deserialize)]
struct ProfileArgs {
    file_path: String,
    line: u32,
    top_n: Option<usize>,
    timeout: Option<f64>,
}

#[derive(Deserialize)]
struct LocalSearchArgs {
    query: String,
    limit: Option<usize>,
    project_root: Option<String>,
}

#[derive(Deserialize)]
struct LeanSearchArgs {
    query: String,
    num_results: Option<usize>,
}

#[derive(Deserialize)]
struct LoogleArgs {
    query: String,
    num_results: Option<usize>,
}

#[derive(Deserialize)]
struct LeanFinderArgs {
    query: String,
    num_results: Option<usize>,
}

#[derive(Deserialize)]
struct StateSearchArgs {
    file_path: String,
    line: u32,
    column: u32,
    num_results: Option<usize>,
}

#[derive(Deserialize)]
struct HammerPremiseArgs {
    file_path: String,
    line: u32,
    column: u32,
    num_results: Option<usize>,
}

#[derive(Deserialize)]
struct CodeActionsArgs {
    file_path: String,
    line: u32,
}

#[derive(Deserialize)]
struct GetWidgetsArgs {
    file_path: String,
    line: u32,
    column: u32,
}

#[derive(Deserialize)]
struct WidgetSourceArgs {
    file_path: String,
    javascript_hash: String,
}

#[derive(Deserialize)]
struct BatchGoalArgs {
    positions: Vec<lean_mcp_core::models::BatchGoalPosition>,
}

#[derive(Deserialize)]
struct ProofDiffArgs {
    file_path: String,
    before_line: u32,
    before_column: Option<u32>,
    after_line: u32,
    after_column: Option<u32>,
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

fn require_client(client: Option<&dyn LspClient>) -> Result<&dyn LspClient, String> {
    client.ok_or_else(|| "No LSP client available. Set a Lean project path first.".to_string())
}

fn require_project(project_path: Option<&Path>) -> Result<&Path, String> {
    project_path.ok_or_else(|| "No Lean project path configured.".to_string())
}

fn deser<T: serde::de::DeserializeOwned>(args: &serde_json::Value) -> Result<T, String> {
    serde_json::from_value(args.clone()).map_err(|e| format!("Invalid arguments: {e}"))
}

fn to_json<T: serde::Serialize>(val: &T) -> Result<serde_json::Value, String> {
    serde_json::to_value(val).map_err(|e| format!("Serialization error: {e}"))
}

async fn dispatch_inner(
    tool_name: &str,
    args: &serde_json::Value,
    client: Option<&dyn LspClient>,
    project_path: Option<&Path>,
    search_config: &SearchConfig,
) -> Result<serde_json::Value, String> {
    match tool_name {
        // Block self-recursion
        "lean_batch" => Err("lean_batch cannot call itself recursively.".to_string()),

        "lean_goal" => {
            let a: GoalArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::goal::handle_lean_goal(c, &a.file_path, a.line, a.column)
                .await
                .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_term_goal" => {
            let a: TermGoalArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::goal::handle_lean_term_goal(c, &a.file_path, a.line, a.column)
                .await
                .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_hover_info" => {
            let a: HoverArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::hover::handle_lean_hover(c, &a.file_path, a.line, a.column)
                .await
                .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_completions" => {
            let a: CompletionsArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::completions::handle_lean_completions(
                c,
                &a.file_path,
                a.line,
                a.column,
                a.max_completions.unwrap_or(32),
            )
            .await
            .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_diagnostic_messages" => {
            let a: DiagnosticsArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::diagnostics::handle_diagnostics(
                c,
                &a.file_path,
                a.start_line,
                a.end_line,
                a.declaration_name.as_deref(),
                a.interactive.unwrap_or(false),
                a.severity.as_deref(),
            )
            .await
            .map_err(|e| e.to_string())?;
            Ok(r)
        }

        "lean_file_outline" => {
            let a: OutlineArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::outline::handle_file_outline(c, &a.file_path, a.max_declarations)
                .await
                .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_declaration_file" => {
            let a: DeclarationArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::declarations::handle_declaration_file(c, &a.file_path, &a.symbol)
                .await
                .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_references" => {
            let a: ReferencesArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::references::handle_references(c, &a.file_path, a.line, a.column)
                .await
                .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_multi_attempt" => {
            let a: MultiAttemptArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::multi_attempt::handle_multi_attempt(
                c,
                None,
                &a.file_path,
                a.line,
                &a.snippets,
                a.column,
                a.parallel,
            )
            .await
            .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_run_code" => {
            let a: RunCodeArgs = deser(args)?;
            let c = require_client(client)?;
            let pp = require_project(project_path)?;
            let r = super::run_code::handle_run_code(c, pp, &a.code)
                .await
                .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_verify" => {
            let a: VerifyArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::verify::handle_verify(
                c,
                &a.file_path,
                &a.theorem_name,
                a.scan_source.unwrap_or(true),
            )
            .await
            .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_build" => {
            let a: BuildArgs = deser(args)?;
            let pp = require_project(project_path)?;
            let r = super::build::handle_build(
                pp,
                a.clean.unwrap_or(false),
                a.output_lines.unwrap_or(20),
            )
            .await
            .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_profile_proof" => {
            let a: ProfileArgs = deser(args)?;
            let pp = require_project(project_path)?;
            let file = PathBuf::from(&a.file_path);
            let r = super::profile::handle_profile_proof(
                &file,
                a.line,
                pp,
                a.timeout.unwrap_or(30.0),
                a.top_n.unwrap_or(5),
            )
            .await
            .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_local_search" => {
            let a: LocalSearchArgs = deser(args)?;
            let root = a
                .project_root
                .as_ref()
                .map(PathBuf::from)
                .or_else(|| project_path.map(|p| p.to_path_buf()));
            let root = root
                .as_deref()
                .ok_or_else(|| "No project path available for local search.".to_string())?;
            let r = lean_mcp_core::search_utils::lean_local_search(
                &a.query,
                a.limit.unwrap_or(10),
                root,
            )
            .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_leansearch" => {
            let a: LeanSearchArgs = deser(args)?;
            let r = super::search::handle_leansearch(
                &a.query,
                a.num_results.unwrap_or(5),
                search_config,
            )
            .await
            .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_loogle" => {
            let a: LoogleArgs = deser(args)?;
            let r = super::search::handle_loogle_remote(
                &a.query,
                a.num_results.unwrap_or(8),
                search_config,
            )
            .await
            .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_leanfinder" => {
            let a: LeanFinderArgs = deser(args)?;
            let r = super::search::handle_leanfinder(
                &a.query,
                a.num_results.unwrap_or(5),
                search_config,
            )
            .await
            .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_state_search" => {
            let a: StateSearchArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::search::handle_state_search(
                c,
                &a.file_path,
                a.line,
                a.column,
                a.num_results.unwrap_or(5),
                search_config,
            )
            .await
            .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_hammer_premise" => {
            let a: HammerPremiseArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::search::handle_hammer_premise(
                c,
                &a.file_path,
                a.line,
                a.column,
                a.num_results.unwrap_or(32),
                search_config,
            )
            .await
            .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_code_actions" => {
            let a: CodeActionsArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::code_actions::handle_code_actions(c, &a.file_path, a.line)
                .await
                .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_get_widgets" => {
            let a: GetWidgetsArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::widgets::handle_get_widgets(c, &a.file_path, a.line, a.column)
                .await
                .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_get_widget_source" => {
            let a: WidgetSourceArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::widgets::handle_get_widget_source(c, &a.file_path, &a.javascript_hash)
                .await
                .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_goals_batch" => {
            let a: BatchGoalArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::batch_goals::handle_lean_goals_batch(c, a.positions)
                .await
                .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        "lean_proof_diff" => {
            let a: ProofDiffArgs = deser(args)?;
            let c = require_client(client)?;
            let r = super::proof_diff::handle_lean_proof_diff(
                c,
                &a.file_path,
                a.before_line,
                a.before_column,
                a.after_line,
                a.after_column,
            )
            .await
            .map_err(|e| e.to_string())?;
            to_json(&r)
        }

        other => Err(format!("Unknown tool: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn default_config() -> SearchConfig {
        SearchConfig::default()
    }

    #[tokio::test]
    async fn empty_batch_returns_empty_items() {
        let result = handle_batch(vec![], None, None, &default_config()).await;
        assert!(result.items.is_empty());
    }

    #[tokio::test]
    async fn unknown_tool_returns_error() {
        let calls = vec![BatchCall {
            tool_name: "nonexistent_tool".into(),
            arguments: json!({}),
        }];
        let result = handle_batch(calls, None, None, &default_config()).await;
        assert_eq!(result.items.len(), 1);
        assert!(result.items[0].is_error);
        assert!(result.items[0]
            .error
            .as_ref()
            .unwrap()
            .contains("Unknown tool"));
    }

    #[tokio::test]
    async fn self_recursion_blocked() {
        let calls = vec![BatchCall {
            tool_name: "lean_batch".into(),
            arguments: json!({"calls": []}),
        }];
        let result = handle_batch(calls, None, None, &default_config()).await;
        assert_eq!(result.items.len(), 1);
        assert!(result.items[0].is_error);
        assert!(result.items[0]
            .error
            .as_ref()
            .unwrap()
            .contains("recursively"));
    }

    #[tokio::test]
    async fn lsp_tool_without_client_returns_error() {
        let calls = vec![BatchCall {
            tool_name: "lean_goal".into(),
            arguments: json!({"file_path": "/tmp/test.lean", "line": 1}),
        }];
        let result = handle_batch(calls, None, None, &default_config()).await;
        assert_eq!(result.items.len(), 1);
        assert!(result.items[0].is_error);
        assert!(result.items[0]
            .error
            .as_ref()
            .unwrap()
            .to_lowercase()
            .contains("lsp"));
    }

    #[tokio::test]
    async fn build_without_project_path_returns_error() {
        let calls = vec![BatchCall {
            tool_name: "lean_build".into(),
            arguments: json!({}),
        }];
        let result = handle_batch(calls, None, None, &default_config()).await;
        assert_eq!(result.items.len(), 1);
        assert!(result.items[0].is_error);
        assert!(result.items[0]
            .error
            .as_ref()
            .unwrap()
            .to_lowercase()
            .contains("project"));
    }

    #[tokio::test]
    async fn order_preserved() {
        let calls = vec![
            BatchCall {
                tool_name: "lean_goal".into(),
                arguments: json!({"file_path": "/tmp/a.lean", "line": 1}),
            },
            BatchCall {
                tool_name: "lean_build".into(),
                arguments: json!({}),
            },
            BatchCall {
                tool_name: "nonexistent".into(),
                arguments: json!({}),
            },
        ];
        let result = handle_batch(calls, None, None, &default_config()).await;
        assert_eq!(result.items.len(), 3);
        assert_eq!(result.items[0].tool_name, "lean_goal");
        assert_eq!(result.items[1].tool_name, "lean_build");
        assert_eq!(result.items[2].tool_name, "nonexistent");
    }
}
