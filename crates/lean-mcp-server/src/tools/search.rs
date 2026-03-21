//! HTTP-based search tool handlers.
//!
//! Five external search tools that call remote APIs:
//! - `lean_leansearch` -- natural language to Mathlib via leansearch.net
//! - `lean_loogle` (remote) -- type-pattern search via loogle.lean-lang.org
//! - `lean_leanfinder` -- semantic/conceptual search via HuggingFace endpoint
//! - `lean_state_search` -- goal-based lemma search via premise-search.com
//! - `lean_hammer_premise` -- goal-based premise retrieval via leanpremise.net
//!
//! All handlers accept configurable base URLs (for wiremock testing) and use
//! `reqwest` for HTTP.

use lean_lsp_client::client::LspClient;
use lean_mcp_core::error::LeanToolError;
use lean_mcp_core::models::{
    LeanFinderResult, LeanFinderResults, LeanSearchResult, LeanSearchResults, LoogleResult,
    LoogleResults, PremiseResult, PremiseResults, StateSearchResult, StateSearchResults,
};
use lean_mcp_core::utils::extract_goals_list;
use serde_json::Value;

/// Default base URL for LeanSearch.
const LEANSEARCH_DEFAULT_URL: &str = "https://leansearch.net";

/// Default base URL for Loogle.
const LOOGLE_DEFAULT_URL: &str = "https://loogle.lean-lang.org";

/// Default base URL for LeanFinder (HuggingFace endpoint).
const LEANFINDER_DEFAULT_URL: &str =
    "https://bxrituxuhpc70w8w.us-east-1.aws.endpoints.huggingface.cloud";

/// Default base URL for state search (premise-search.com).
const STATE_SEARCH_DEFAULT_URL: &str = "https://premise-search.com";

/// Default base URL for hammer premise (leanpremise.net).
const HAMMER_PREMISE_DEFAULT_URL: &str = "http://leanpremise.net";

/// Default HTTP timeout in seconds.
const HTTP_TIMEOUT_SECS: u64 = 30;

/// Configuration for search tool base URLs.
///
/// All URLs should be without a trailing slash. For production use, the
/// defaults point to the real external services. For tests, set the URL
/// to a `wiremock::MockServer` address.
#[derive(Debug, Clone)]
pub struct SearchConfig {
    /// Base URL for the LeanSearch API.
    pub leansearch_url: String,
    /// Base URL for the Loogle API.
    pub loogle_url: String,
    /// Base URL for the LeanFinder API.
    pub leanfinder_url: String,
    /// Base URL for the state search API.
    pub state_search_url: String,
    /// Base URL for the hammer premise API.
    pub hammer_premise_url: String,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            leansearch_url: std::env::var("LEANSEARCH_URL")
                .unwrap_or_else(|_| LEANSEARCH_DEFAULT_URL.to_string()),
            loogle_url: std::env::var("LOOGLE_URL")
                .unwrap_or_else(|_| LOOGLE_DEFAULT_URL.to_string()),
            leanfinder_url: std::env::var("LEANFINDER_URL")
                .unwrap_or_else(|_| LEANFINDER_DEFAULT_URL.to_string()),
            state_search_url: std::env::var("STATE_SEARCH_URL")
                .unwrap_or_else(|_| STATE_SEARCH_DEFAULT_URL.to_string()),
            hammer_premise_url: std::env::var("HAMMER_PREMISE_URL")
                .unwrap_or_else(|_| HAMMER_PREMISE_DEFAULT_URL.to_string()),
        }
    }
}

/// Build a shared `reqwest::Client` with a reasonable timeout and User-Agent.
///
/// A User-Agent header is required by several search APIs (e.g. loogle.lean-lang.org
/// returns 502 without one).
fn http_client() -> Result<reqwest::Client, LeanToolError> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(HTTP_TIMEOUT_SECS))
        .user_agent("lean-lsp-mcp/0.1")
        .build()
        .map_err(|e| LeanToolError::Other(format!("Failed to build HTTP client: {e}")))
}

// ---------------------------------------------------------------------------
// lean_leansearch
// ---------------------------------------------------------------------------

/// Handle `lean_leansearch`: POST to LeanSearch API.
///
/// Sends a JSON body `{"num_results": "<n>", "query": ["<query>"]}` and
/// parses the nested response structure.
///
/// Rate limit category: `leansearch` (3/30s).
pub async fn handle_leansearch(
    query: &str,
    num_results: usize,
    config: &SearchConfig,
) -> Result<LeanSearchResults, LeanToolError> {
    let client = http_client()?;
    let url = format!("{}/search", config.leansearch_url);

    let body = serde_json::json!({
        "num_results": num_results.to_string(),
        "query": [query],
    });

    let response = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| LeanToolError::Other(format!("LeanSearch HTTP error: {e}")))?;

    if !response.status().is_success() {
        return Err(LeanToolError::Other(format!(
            "LeanSearch returned status {}",
            response.status()
        )));
    }

    let resp_json: Value = response
        .json()
        .await
        .map_err(|e| LeanToolError::Other(format!("LeanSearch JSON parse error: {e}")))?;

    // Response format: {"results": [[{"result": {...}}, ...]]}
    // results[0] is the array of hits for the first query.
    let items = parse_leansearch_results(&resp_json);

    Ok(LeanSearchResults { items })
}

/// Parse the nested LeanSearch response.
///
/// The API returns `results[0][i].result` where each result has:
/// - `name`: array of strings -> joined with `"."`
/// - `module_name`: array of strings -> joined with `"."`
/// - `kind`: string
/// - `type`: raw value (string or stringified)
///
/// Name and module_name arrays contain plain segments that must be
/// dot-joined, matching the Python reference: `".".join(r["name"])`.
/// The type field is taken as a raw value (Python: `r.get("type")`).
fn parse_leansearch_results(json: &Value) -> Vec<LeanSearchResult> {
    let Some(results) = json.get("results").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let Some(first_query) = results.first().and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    first_query
        .iter()
        .filter_map(|entry| {
            let result = entry.get("result")?;

            let name = join_dot_segments(result.get("name")?);
            let module_name = join_dot_segments(result.get("module_name")?);
            let kind = result
                .get("kind")
                .and_then(|v| v.as_str())
                .map(String::from);
            // Type is taken as a raw value: string directly, or stringified for
            // non-string types (matching Python's `r.get("type")`).
            let r#type = result.get("type").map(value_as_string);

            Some(LeanSearchResult {
                name,
                module_name,
                kind,
                r#type,
            })
        })
        .collect()
}

/// Join a JSON value that is either a string or an array of string segments
/// with dot separators.
///
/// Matches the Python reference: `".".join(r["name"])` and `".".join(r["module_name"])`.
/// Segments are plain identifiers (e.g. `["Nat", "add", "comm"]` → `"Nat.add.comm"`).
fn join_dot_segments(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|x| x.as_str())
            .collect::<Vec<_>>()
            .join("."),
        _ => String::new(),
    }
}

/// Convert a JSON value to a string representation.
///
/// - String values are returned as-is.
/// - Other types (arrays, numbers, etc.) are JSON-serialized.
///
/// This matches the Python behavior of passing `r.get("type")` directly
/// to a Pydantic `Optional[str]` field.
fn value_as_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// lean_loogle (remote)
// ---------------------------------------------------------------------------

/// Handle `lean_loogle` (remote): GET to Loogle API.
///
/// Sends `GET /json?q={url_encoded_query}` and parses the `hits` array.
///
/// Rate limit category: `loogle` (3/30s).
pub async fn handle_loogle_remote(
    query: &str,
    num_results: usize,
    config: &SearchConfig,
) -> Result<LoogleResults, LeanToolError> {
    let client = http_client()?;
    let url = format!("{}/json", config.loogle_url);

    let response = client
        .get(&url)
        .query(&[("q", query)])
        .send()
        .await
        .map_err(|e| LeanToolError::Other(format!("Loogle HTTP error: {e}")))?;

    if !response.status().is_success() {
        return Err(LeanToolError::Other(format!(
            "Loogle returned status {}",
            response.status()
        )));
    }

    let resp_json: Value = response
        .json()
        .await
        .map_err(|e| LeanToolError::Other(format!("Loogle JSON parse error: {e}")))?;

    // Check for error/suggestion messages
    if let Some(error) = resp_json.get("error").and_then(|v| v.as_str()) {
        return Err(LeanToolError::Other(format!("Loogle error: {error}")));
    }

    let items = parse_loogle_results(&resp_json, num_results);

    Ok(LoogleResults { items })
}

/// Parse Loogle hits.
///
/// Response: `{"hits": [{"name": "...", "type": "...", "module": "..."}, ...]}`
fn parse_loogle_results(json: &Value, num_results: usize) -> Vec<LoogleResult> {
    let Some(hits) = json.get("hits").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    hits.iter()
        .take(num_results)
        .filter_map(|hit| {
            let name = hit.get("name")?.as_str()?.to_string();
            let r#type = hit.get("type")?.as_str()?.to_string();
            let module = hit.get("module")?.as_str()?.to_string();
            Some(LoogleResult {
                name,
                r#type,
                module,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// lean_leanfinder
// ---------------------------------------------------------------------------

/// Handle `lean_leanfinder`: POST to HuggingFace endpoint.
///
/// Sends `{"inputs": "<query>", "top_k": <n>}` and filters results to
/// Mathlib4 entries, extracting `full_name` from the URL pattern.
///
/// Rate limit category: `leanfinder` (10/30s).
pub async fn handle_leanfinder(
    query: &str,
    num_results: usize,
    config: &SearchConfig,
) -> Result<LeanFinderResults, LeanToolError> {
    let client = http_client()?;
    let url = config.leanfinder_url.clone();

    let body = serde_json::json!({
        "inputs": query,
        "top_k": num_results,
    });

    let response = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| LeanToolError::Other(format!("LeanFinder HTTP error: {e}")))?;

    if !response.status().is_success() {
        return Err(LeanToolError::Other(format!(
            "LeanFinder returned status {}",
            response.status()
        )));
    }

    let resp_json: Value = response
        .json()
        .await
        .map_err(|e| LeanToolError::Other(format!("LeanFinder JSON parse error: {e}")))?;

    let items = parse_leanfinder_results(&resp_json);

    Ok(LeanFinderResults { items })
}

/// Parse LeanFinder results, filtering to mathlib4 entries.
///
/// Response is `{"results": [...]}` where each entry has:
/// - `url`: must contain the mathlib4_docs base URL; name extracted via
///   regex `pattern=(.*?)#doc`
/// - `formal_statement`: the Lean type signature
/// - `informal_statement`: natural language description
///
/// Matches the Python reference which accesses `data["results"]` and
/// filters by checking `"mathlib4_docs" not in result["url"]`.
fn parse_leanfinder_results(json: &Value) -> Vec<LeanFinderResult> {
    // The API returns {"results": [...]}, not a top-level array.
    let arr = json
        .get("results")
        .and_then(|v| v.as_array())
        .or_else(|| json.as_array());

    let Some(arr) = arr else {
        return Vec::new();
    };

    arr.iter()
        .filter_map(|entry| {
            let url = entry.get("url")?.as_str()?;

            // Filter to mathlib4 results by checking URL contains mathlib4_docs
            // (matches Python: "mathlib4_docs" not in result["url"])
            if !url.contains("mathlib4_docs") {
                return None;
            }

            // Extract name using pattern= URL matching (matches Python:
            // re.search(r"pattern=(.*?)#doc", result["url"]))
            let full_name = extract_name_from_url(url)?;

            let formal_statement = entry
                .get("formal_statement")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let informal_statement = entry
                .get("informal_statement")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            Some(LeanFinderResult {
                full_name,
                formal_statement,
                informal_statement,
            })
        })
        .collect()
}

/// Extract the declaration name from a Mathlib URL using the
/// `?pattern=NAME#doc` format.
///
/// Returns `None` if the URL does not contain `pattern=...#doc`,
/// matching the Python reference: `re.search(r"pattern=(.*?)#doc", url)`.
fn extract_name_from_url(url: &str) -> Option<String> {
    let idx = url.find("pattern=")?;
    let after = &url[idx + "pattern=".len()..];
    let name = after.split('#').next().unwrap_or(after);
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

// ---------------------------------------------------------------------------
// lean_state_search
// ---------------------------------------------------------------------------

/// Handle `lean_state_search`: get goal from LSP, then search premise-search.com.
///
/// 1. Opens file and gets proof goal at position via LSP
/// 2. URL-encodes the first goal string
/// 3. GET `https://premise-search.com/api/search?query={goal}&results={n}&rev=v4.22.0`
///
/// Rate limit category: `lean_state_search` (6/30s).
pub async fn handle_state_search(
    lsp_client: &dyn LspClient,
    file_path: &str,
    line: u32,
    column: u32,
    num_results: usize,
    config: &SearchConfig,
) -> Result<StateSearchResults, LeanToolError> {
    // 1. Get goal at position
    let goal = get_first_goal(lsp_client, file_path, line, column).await?;

    // 2. Search premise-search.com
    let client = http_client()?;
    let url = format!("{}/api/search", config.state_search_url);

    let response = client
        .get(&url)
        .query(&[
            ("query", goal.as_str()),
            ("results", &num_results.to_string()),
            ("rev", "v4.22.0"),
        ])
        .send()
        .await
        .map_err(|e| LeanToolError::Other(format!("StateSearch HTTP error: {e}")))?;

    if !response.status().is_success() {
        return Err(LeanToolError::Other(format!(
            "StateSearch returned status {}",
            response.status()
        )));
    }

    let resp_json: Value = response
        .json()
        .await
        .map_err(|e| LeanToolError::Other(format!("StateSearch JSON parse error: {e}")))?;

    let items = parse_state_search_results(&resp_json);

    Ok(StateSearchResults { items })
}

/// Parse state search results.
///
/// Response is an array of objects with a `"name"` field.
fn parse_state_search_results(json: &Value) -> Vec<StateSearchResult> {
    let Some(arr) = json.as_array() else {
        return Vec::new();
    };

    arr.iter()
        .filter_map(|entry| {
            let name = entry.get("name")?.as_str()?.to_string();
            Some(StateSearchResult { name })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// lean_hammer_premise
// ---------------------------------------------------------------------------

/// Handle `lean_hammer_premise`: get goal from LSP, then POST to leanpremise.net.
///
/// 1. Opens file and gets proof goal at position via LSP
/// 2. POST to `http://leanpremise.net/retrieve` with
///    `{"state": "<goal>", "new_premises": [], "k": <n>}`
///
/// Rate limit category: `hammer_premise` (6/30s).
pub async fn handle_hammer_premise(
    lsp_client: &dyn LspClient,
    file_path: &str,
    line: u32,
    column: u32,
    num_results: usize,
    config: &SearchConfig,
) -> Result<PremiseResults, LeanToolError> {
    // 1. Get goal at position
    let goal = get_first_goal(lsp_client, file_path, line, column).await?;

    // 2. Post to leanpremise.net
    let client = http_client()?;
    let url = format!("{}/retrieve", config.hammer_premise_url);

    let body = serde_json::json!({
        "state": goal,
        "new_premises": [],
        "k": num_results,
    });

    let response = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| LeanToolError::Other(format!("HammerPremise HTTP error: {e}")))?;

    if !response.status().is_success() {
        return Err(LeanToolError::Other(format!(
            "HammerPremise returned status {}",
            response.status()
        )));
    }

    let resp_json: Value = response
        .json()
        .await
        .map_err(|e| LeanToolError::Other(format!("HammerPremise JSON parse error: {e}")))?;

    let items = parse_premise_results(&resp_json);

    Ok(PremiseResults { items })
}

/// Parse premise results.
///
/// Response is an array of strings (premise names).
fn parse_premise_results(json: &Value) -> Vec<PremiseResult> {
    let Some(arr) = json.as_array() else {
        return Vec::new();
    };

    arr.iter()
        .filter_map(|entry| {
            let name = entry.as_str()?.to_string();
            Some(PremiseResult { name })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Open a file via LSP and get the first goal at the given position.
///
/// Coordinates are **1-indexed** (user-facing). They are converted to
/// 0-indexed before calling the LSP client.
async fn get_first_goal(
    client: &dyn LspClient,
    file_path: &str,
    line: u32,
    column: u32,
) -> Result<String, LeanToolError> {
    // Open the file
    client
        .open_file(file_path)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "open_file".into(),
            message: e.to_string(),
        })?;

    // Convert to 0-indexed
    let lsp_line = line.saturating_sub(1);
    let lsp_col = column.saturating_sub(1);

    // Get goal
    let goal_response = client
        .get_goal(file_path, lsp_line, lsp_col)
        .await
        .map_err(|e| LeanToolError::LspError {
            operation: "get_goal".into(),
            message: e.to_string(),
        })?;

    let goals = extract_goals_list(goal_response.as_ref());

    goals
        .into_iter()
        .next()
        .ok_or(LeanToolError::NoGoals { line, column })
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
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // -- Mock LSP client for state_search / hammer_premise ----------------

    struct MockSearchLspClient {
        project: PathBuf,
        content: String,
        goal_responses: Vec<((u32, u32), Option<Value>)>,
    }

    impl MockSearchLspClient {
        fn new(content: &str) -> Self {
            Self {
                project: PathBuf::from("/test/project"),
                content: content.to_string(),
                goal_responses: Vec::new(),
            }
        }

        fn with_goal(mut self, line: u32, col: u32, response: Option<Value>) -> Self {
            self.goal_responses.push(((line, col), response));
            self
        }
    }

    #[async_trait]
    impl LspClient for MockSearchLspClient {
        fn project_path(&self) -> &Path {
            &self.project
        }

        async fn open_file(
            &self,
            _relative_path: &str,
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }

        async fn open_file_force(
            &self,
            _relative_path: &str,
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }

        async fn get_file_content(
            &self,
            _relative_path: &str,
        ) -> Result<String, lean_lsp_client::client::LspClientError> {
            Ok(self.content.clone())
        }

        async fn update_file(
            &self,
            _relative_path: &str,
            _changes: Vec<Value>,
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }

        async fn update_file_content(
            &self,
            _relative_path: &str,
            _content: &str,
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }

        async fn close_files(
            &self,
            _paths: &[String],
        ) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }

        async fn get_diagnostics(
            &self,
            _relative_path: &str,
            _start_line: Option<u32>,
            _end_line: Option<u32>,
            _inactivity_timeout: Option<f64>,
        ) -> Result<Value, lean_lsp_client::client::LspClientError> {
            Ok(json!({}))
        }

        async fn get_interactive_diagnostics(
            &self,
            _relative_path: &str,
            _start_line: Option<u32>,
            _end_line: Option<u32>,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }

        async fn get_goal(
            &self,
            _relative_path: &str,
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
            _relative_path: &str,
            _line: u32,
            _column: u32,
        ) -> Result<Option<Value>, lean_lsp_client::client::LspClientError> {
            Ok(None)
        }

        async fn get_hover(
            &self,
            _relative_path: &str,
            _line: u32,
            _column: u32,
        ) -> Result<Option<Value>, lean_lsp_client::client::LspClientError> {
            Ok(None)
        }

        async fn get_completions(
            &self,
            _relative_path: &str,
            _line: u32,
            _column: u32,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }

        async fn get_declarations(
            &self,
            _relative_path: &str,
            _line: u32,
            _column: u32,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }

        async fn get_references(
            &self,
            _relative_path: &str,
            _line: u32,
            _column: u32,
            _include_declaration: bool,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }

        async fn get_document_symbols(
            &self,
            _relative_path: &str,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }

        async fn get_code_actions(
            &self,
            _relative_path: &str,
            _start_line: u32,
            _start_col: u32,
            _end_line: u32,
            _end_col: u32,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }

        async fn get_code_action_resolve(
            &self,
            _action: Value,
        ) -> Result<Value, lean_lsp_client::client::LspClientError> {
            Ok(json!({}))
        }

        async fn get_widgets(
            &self,
            _relative_path: &str,
            _line: u32,
            _column: u32,
        ) -> Result<Vec<Value>, lean_lsp_client::client::LspClientError> {
            Ok(vec![])
        }

        async fn get_widget_source(
            &self,
            _relative_path: &str,
            _line: u32,
            _column: u32,
            _javascript_hash: &str,
        ) -> Result<Value, lean_lsp_client::client::LspClientError> {
            Ok(json!({}))
        }

        async fn shutdown(&self) -> Result<(), lean_lsp_client::client::LspClientError> {
            Ok(())
        }
    }

    /// Build a `SearchConfig` pointing at the given mock server URL.
    fn mock_config(server_url: &str) -> SearchConfig {
        SearchConfig {
            leansearch_url: server_url.to_string(),
            loogle_url: server_url.to_string(),
            leanfinder_url: server_url.to_string(),
            state_search_url: server_url.to_string(),
            hammer_premise_url: server_url.to_string(),
        }
    }

    // =====================================================================
    // LeanSearch tests
    // =====================================================================

    #[tokio::test]
    async fn leansearch_returns_results() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [[
                    {
                        "result": {
                            "name": ["Nat", "add_comm"],
                            "module_name": ["Init", "Data", "Nat"],
                            "kind": "theorem",
                            "type": "forall (n m : Nat), n + m = m + n"
                        }
                    },
                    {
                        "result": {
                            "name": ["Nat", "mul_comm"],
                            "module_name": ["Init", "Data", "Nat"],
                            "kind": "theorem",
                            "type": "forall (n m : Nat), n * m = m * n"
                        }
                    }
                ]]
            })))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let result = handle_leansearch("commutativity of addition", 5, &config)
            .await
            .unwrap();

        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].name, "Nat.add_comm");
        assert_eq!(result.items[0].module_name, "Init.Data.Nat");
        assert_eq!(result.items[0].kind, Some("theorem".to_string()));
        assert_eq!(
            result.items[0].r#type,
            Some("forall (n m : Nat), n + m = m + n".to_string())
        );
        assert_eq!(result.items[1].name, "Nat.mul_comm");
    }

    #[tokio::test]
    async fn leansearch_handles_empty_response() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [[]]
            })))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let result = handle_leansearch("nonexistent query", 5, &config)
            .await
            .unwrap();

        assert!(result.items.is_empty());
    }

    #[tokio::test]
    async fn leansearch_handles_missing_results_key() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let result = handle_leansearch("query", 5, &config).await.unwrap();

        assert!(result.items.is_empty());
    }

    // =====================================================================
    // Loogle tests
    // =====================================================================

    #[tokio::test]
    async fn loogle_returns_results() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/json"))
            .and(query_param("q", "Nat.add_comm"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "hits": [
                    {
                        "name": "Nat.add_comm",
                        "type": "forall (n m : Nat), n + m = m + n",
                        "module": "Init.Data.Nat.Lemmas"
                    },
                    {
                        "name": "Nat.add_comm'",
                        "type": "forall (n m : Nat), n + m = m + n",
                        "module": "Init.Data.Nat.Lemmas"
                    }
                ]
            })))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let result = handle_loogle_remote("Nat.add_comm", 8, &config)
            .await
            .unwrap();

        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].name, "Nat.add_comm");
        assert_eq!(result.items[0].r#type, "forall (n m : Nat), n + m = m + n");
        assert_eq!(result.items[0].module, "Init.Data.Nat.Lemmas");
    }

    #[tokio::test]
    async fn loogle_handles_no_hits() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "hits": []
            })))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let result = handle_loogle_remote("nonexistent", 8, &config)
            .await
            .unwrap();

        assert!(result.items.is_empty());
    }

    #[tokio::test]
    async fn loogle_handles_error_response() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": "Invalid query syntax"
            })))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let err = handle_loogle_remote("bad query ??? !!!", 8, &config)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("Loogle error"));
        assert!(err.to_string().contains("Invalid query syntax"));
    }

    #[tokio::test]
    async fn loogle_respects_num_results() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "hits": [
                    {"name": "A", "type": "T", "module": "M"},
                    {"name": "B", "type": "T", "module": "M"},
                    {"name": "C", "type": "T", "module": "M"}
                ]
            })))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let result = handle_loogle_remote("query", 2, &config).await.unwrap();

        assert_eq!(result.items.len(), 2);
    }

    // =====================================================================
    // LeanFinder tests
    // =====================================================================

    #[tokio::test]
    async fn leanfinder_filters_to_mathlib4() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [
                    {
                        "url": "https://leanprover-community.github.io/mathlib4_docs/find/?pattern=Nat.add_comm#doc",
                        "formal_statement": "theorem Nat.add_comm : forall n m, n + m = m + n",
                        "informal_statement": "Commutativity of natural number addition"
                    },
                    {
                        "url": "https://example.com/Other.html",
                        "formal_statement": "def other",
                        "informal_statement": "Not mathlib4"
                    },
                    {
                        "url": "https://leanprover-community.github.io/mathlib4_docs/find/?pattern=Nat.mul_comm#doc",
                        "formal_statement": "theorem Nat.mul_comm : forall n m, n * m = m * n",
                        "informal_statement": "Commutativity of natural number multiplication"
                    }
                ]
            })))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let result = handle_leanfinder("commutativity", 5, &config)
            .await
            .unwrap();

        // Only mathlib4_docs URLs should be included
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].full_name, "Nat.add_comm");
        assert_eq!(result.items[1].full_name, "Nat.mul_comm");
    }

    #[tokio::test]
    async fn leanfinder_extracts_names_from_urls() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [
                    {
                        "url": "https://leanprover-community.github.io/mathlib4_docs/find/?pattern=IsOpen#doc",
                        "formal_statement": "def IsOpen",
                        "informal_statement": "An open set"
                    }
                ]
            })))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let result = handle_leanfinder("open set", 5, &config).await.unwrap();

        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].full_name, "IsOpen");
        assert_eq!(result.items[0].formal_statement, "def IsOpen");
        assert_eq!(result.items[0].informal_statement, "An open set");
    }

    #[tokio::test]
    async fn leanfinder_handles_empty_response() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let result = handle_leanfinder("nothing", 5, &config).await.unwrap();

        assert!(result.items.is_empty());
    }

    // =====================================================================
    // State Search tests
    // =====================================================================

    #[tokio::test]
    async fn state_search_gets_goal_then_searches() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/search"))
            .and(query_param("query", "a : Nat |- a = a"))
            .and(query_param("rev", "v4.22.0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"name": "rfl"},
                {"name": "Eq.refl"}
            ])))
            .mount(&server)
            .await;

        // Mock LSP: line 2, col 3 -> 0-indexed (1, 2) -> goal response
        let lsp = MockSearchLspClient::new("theorem foo : Nat := by\n  exact h").with_goal(
            1,
            2,
            Some(json!({"goals": ["a : Nat |- a = a"]})),
        );

        let config = mock_config(&server.uri());
        let result = handle_state_search(&lsp, "Main.lean", 2, 3, 5, &config)
            .await
            .unwrap();

        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].name, "rfl");
        assert_eq!(result.items[1].name, "Eq.refl");
    }

    #[tokio::test]
    async fn state_search_no_goals_returns_error() {
        let server = MockServer::start().await;
        let lsp = MockSearchLspClient::new("import Mathlib");

        let config = mock_config(&server.uri());
        let err = handle_state_search(&lsp, "Main.lean", 1, 1, 5, &config)
            .await
            .unwrap_err();

        match err {
            LeanToolError::NoGoals { line, column } => {
                assert_eq!(line, 1);
                assert_eq!(column, 1);
            }
            other => panic!("expected NoGoals, got: {other}"),
        }
    }

    // =====================================================================
    // Hammer Premise tests
    // =====================================================================

    #[tokio::test]
    async fn hammer_premise_gets_goal_then_retrieves() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/retrieve"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                "Nat.add_comm",
                "Nat.add_assoc",
                "Nat.zero_add"
            ])))
            .mount(&server)
            .await;

        let lsp = MockSearchLspClient::new("theorem foo : Nat := by\n  simp").with_goal(
            1,
            2,
            Some(json!({"goals": ["n m : Nat |- n + m = m + n"]})),
        );

        let config = mock_config(&server.uri());
        let result = handle_hammer_premise(&lsp, "Main.lean", 2, 3, 32, &config)
            .await
            .unwrap();

        assert_eq!(result.items.len(), 3);
        assert_eq!(result.items[0].name, "Nat.add_comm");
        assert_eq!(result.items[1].name, "Nat.add_assoc");
        assert_eq!(result.items[2].name, "Nat.zero_add");
    }

    #[tokio::test]
    async fn hammer_premise_no_goals_returns_error() {
        let server = MockServer::start().await;
        let lsp = MockSearchLspClient::new("import Mathlib");

        let config = mock_config(&server.uri());
        let err = handle_hammer_premise(&lsp, "Main.lean", 1, 1, 32, &config)
            .await
            .unwrap_err();

        match err {
            LeanToolError::NoGoals { line, column } => {
                assert_eq!(line, 1);
                assert_eq!(column, 1);
            }
            other => panic!("expected NoGoals, got: {other}"),
        }
    }

    // =====================================================================
    // Error cases
    // =====================================================================

    #[tokio::test]
    async fn leansearch_http_error_status() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let err = handle_leansearch("query", 5, &config).await.unwrap_err();

        assert!(err.to_string().contains("status"));
    }

    #[tokio::test]
    async fn loogle_http_error_status() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/json"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let err = handle_loogle_remote("query", 8, &config).await.unwrap_err();

        assert!(err.to_string().contains("status"));
    }

    #[tokio::test]
    async fn leanfinder_http_error_status() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let err = handle_leanfinder("query", 5, &config).await.unwrap_err();

        assert!(err.to_string().contains("status"));
    }

    #[tokio::test]
    async fn leansearch_invalid_json_response() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let err = handle_leansearch("query", 5, &config).await.unwrap_err();

        assert!(err.to_string().contains("JSON parse error"));
    }

    #[tokio::test]
    async fn state_search_http_error_status() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/search"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let lsp = MockSearchLspClient::new("theorem foo := by\n  simp").with_goal(
            1,
            2,
            Some(json!({"goals": ["some goal"]})),
        );

        let config = mock_config(&server.uri());
        let err = handle_state_search(&lsp, "Main.lean", 2, 3, 5, &config)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("status"));
    }

    #[tokio::test]
    async fn hammer_premise_http_error_status() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/retrieve"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let lsp = MockSearchLspClient::new("theorem foo := by\n  simp").with_goal(
            1,
            2,
            Some(json!({"goals": ["some goal"]})),
        );

        let config = mock_config(&server.uri());
        let err = handle_hammer_premise(&lsp, "Main.lean", 2, 3, 32, &config)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("status"));
    }

    // =====================================================================
    // Unit tests for parsing helpers
    // =====================================================================

    #[test]
    fn join_dot_segments_from_array() {
        let v = json!(["Nat", "add_comm"]);
        assert_eq!(join_dot_segments(&v), "Nat.add_comm");
    }

    #[test]
    fn join_dot_segments_from_string() {
        let v = json!("Nat.add_comm");
        assert_eq!(join_dot_segments(&v), "Nat.add_comm");
    }

    #[test]
    fn join_dot_segments_from_non_string() {
        let v = json!(42);
        assert_eq!(join_dot_segments(&v), "");
    }

    #[test]
    fn join_dot_segments_single_element() {
        let v = json!(["continuous_id"]);
        assert_eq!(join_dot_segments(&v), "continuous_id");
    }

    #[test]
    fn value_as_string_from_string() {
        let v = json!("Continuous id");
        assert_eq!(value_as_string(&v), "Continuous id");
    }

    #[test]
    fn value_as_string_from_non_string() {
        let v = json!(42);
        assert_eq!(value_as_string(&v), "42");
    }

    #[test]
    fn value_as_string_from_array() {
        let v = json!(["Continuous ", "g"]);
        // Non-string values are JSON-serialized
        assert_eq!(value_as_string(&v), "[\"Continuous \",\"g\"]");
    }

    #[test]
    fn extract_name_from_url_with_pattern_query() {
        assert_eq!(
            extract_name_from_url(
                "https://leanprover-community.github.io/mathlib4_docs/find/?pattern=mul_comm#doc"
            ),
            Some("mul_comm".to_string())
        );
    }

    #[test]
    fn extract_name_from_url_without_pattern() {
        // URLs without pattern= should return None (matching Python regex behavior)
        assert_eq!(
            extract_name_from_url(
                "https://leanprover-community.github.io/mathlib4_docs/Mathlib/Algebra/Nat.add_comm.html"
            ),
            None
        );
    }

    #[test]
    fn extract_name_from_url_no_match() {
        assert_eq!(extract_name_from_url("https://example.com/SomeName"), None);
    }

    #[test]
    fn parse_leansearch_empty_results() {
        let json = json!({"results": [[]]});
        assert!(parse_leansearch_results(&json).is_empty());
    }

    #[test]
    fn parse_loogle_empty_hits() {
        let json = json!({"hits": []});
        assert!(parse_loogle_results(&json, 10).is_empty());
    }

    #[test]
    fn parse_state_search_non_array() {
        let json = json!({"error": "something"});
        assert!(parse_state_search_results(&json).is_empty());
    }

    #[test]
    fn parse_premise_results_with_strings() {
        let json = json!(["foo", "bar", "baz"]);
        let results = parse_premise_results(&json);
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].name, "foo");
        assert_eq!(results[2].name, "baz");
    }

    #[test]
    fn parse_premise_results_empty_array() {
        let json = json!([]);
        assert!(parse_premise_results(&json).is_empty());
    }

    // =====================================================================
    // Fixture-based parsing tests (real API response formats)
    // =====================================================================

    /// Path to the fixture directory (relative to workspace root).
    const FIXTURES_DIR: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/api_responses"
    );

    #[test]
    fn fixture_leansearch_parses_real_response() {
        let fixture =
            std::fs::read_to_string(format!("{FIXTURES_DIR}/leansearch_continuous.json")).unwrap();
        let fixture_json: serde_json::Value = serde_json::from_str(&fixture).unwrap();

        let results = parse_leansearch_results(&fixture_json);
        assert!(
            !results.is_empty(),
            "LeanSearch fixture should produce non-empty results"
        );
        assert_eq!(results.len(), 4, "Expected 4 results from fixture");

        // Name segments dot-joined: ".".join(["Continuous", "comp"]) = "Continuous.comp"
        assert_eq!(results[0].name, "Continuous.comp");
        assert_eq!(
            results[0].module_name,
            "Mathlib.Topology.ContinuousOn.Basic"
        );
        assert_eq!(results[0].kind, Some("theorem".to_string()));
        // Type is taken as raw string value
        assert!(results[0]
            .r#type
            .as_ref()
            .is_some_and(|t| t.contains("Continuous")),);

        // Single-element name array: ".".join(["continuous_id"]) = "continuous_id"
        assert_eq!(results[2].name, "continuous_id");
        // Scalar string type passes through unchanged
        assert_eq!(results[2].r#type, Some("Continuous id".to_string()));
    }

    #[test]
    fn fixture_loogle_parses_real_response() {
        let fixture =
            std::fs::read_to_string(format!("{FIXTURES_DIR}/loogle_nat_add_comm.json")).unwrap();
        let fixture_json: serde_json::Value = serde_json::from_str(&fixture).unwrap();

        let results = parse_loogle_results(&fixture_json, 10);
        assert!(!results.is_empty());
        assert_eq!(results.len(), 5);

        assert_eq!(results[0].name, "Nat.add_comm");
        assert!(results[0].r#type.contains("n + m = m + n"));
        assert_eq!(results[0].module, "Init.Data.Nat.Lemmas");

        // Test num_results truncation
        let truncated = parse_loogle_results(&fixture_json, 2);
        assert_eq!(truncated.len(), 2);
    }

    #[test]
    fn fixture_leanfinder_parses_real_response() {
        let fixture =
            std::fs::read_to_string(format!("{FIXTURES_DIR}/leanfinder_commutativity.json"))
                .unwrap();
        let fixture_json: serde_json::Value = serde_json::from_str(&fixture).unwrap();

        let results = parse_leanfinder_results(&fixture_json);

        // Should filter out the non-mathlib4_docs URL entry
        assert_eq!(results.len(), 3, "Expected 3 mathlib4 results");

        // Name extracted from ?pattern=NAME#doc URL format
        assert_eq!(results[0].full_name, "mul_comm");
        assert!(results[0].formal_statement.contains("a * b = b * a"));
        assert!(results[0].informal_statement.contains("commutative"));

        assert_eq!(results[1].full_name, "add_comm");
        assert_eq!(results[2].full_name, "Commute");
    }

    #[test]
    fn fixture_ripgrep_jsonl_parses_match_lines() {
        let fixture =
            std::fs::read_to_string(format!("{FIXTURES_DIR}/ripgrep_declarations.jsonl")).unwrap();

        let mut matches = Vec::new();
        for line in fixture.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let json: serde_json::Value = serde_json::from_str(line).unwrap();
            if json.get("type").and_then(|t| t.as_str()) == Some("match") {
                let data = json.get("data").unwrap();
                let file_path = data
                    .get("path")
                    .and_then(|p| p.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                let line_text = data
                    .get("lines")
                    .and_then(|l| l.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                matches.push((file_path.to_string(), line_text.to_string()));
            }
        }

        assert_eq!(matches.len(), 4, "Expected 4 match lines in fixture");
        assert!(matches[0].1.contains("theorem add_assoc"));
        assert!(matches[1].1.contains("def myHelper"));
        assert!(matches[2].1.contains("class MyGroup"));
        assert!(matches[3].0.contains(".lake/packages/mathlib"));
    }

    // =====================================================================
    // Bug fix regression tests (#126, #127, #128)
    // =====================================================================

    /// #126: leansearch name segments must be dot-joined.
    #[test]
    fn leansearch_name_dot_joined() {
        let json = json!({
            "results": [[
                {
                    "result": {
                        "name": ["Nat", "add", "comm"],
                        "module_name": ["Init", "Data", "Nat"],
                        "kind": "theorem",
                        "type": "forall (n m : Nat), n + m = m + n"
                    }
                }
            ]]
        });
        let results = parse_leansearch_results(&json);
        assert_eq!(results.len(), 1);
        // Segments dot-joined: "Nat" + "." + "add" + "." + "comm"
        assert_eq!(results[0].name, "Nat.add.comm");
    }

    /// #126: leansearch type field taken as raw string, not joined from array.
    #[test]
    fn leansearch_type_as_raw_value() {
        let json = json!({
            "results": [[
                {
                    "result": {
                        "name": ["Foo"],
                        "module_name": ["Bar"],
                        "kind": "def",
                        "type": "Nat → Nat"
                    }
                }
            ]]
        });
        let results = parse_leansearch_results(&json);
        assert_eq!(results[0].r#type, Some("Nat → Nat".to_string()));
    }

    /// #126: leansearch type field handles non-string values (serialized).
    #[test]
    fn leansearch_type_non_string_serialized() {
        let json = json!({
            "results": [[
                {
                    "result": {
                        "name": ["Foo"],
                        "module_name": ["Bar"],
                        "kind": "def",
                        "type": ["Continuous ", "f"]
                    }
                }
            ]]
        });
        let results = parse_leansearch_results(&json);
        // Non-string type values are JSON-serialized
        assert!(results[0].r#type.is_some());
    }

    /// #126: leanfinder parses from {"results": [...]} wrapper.
    #[test]
    fn leanfinder_results_key_extraction() {
        let json = json!({
            "results": [
                {
                    "url": "https://leanprover-community.github.io/mathlib4_docs/find/?pattern=Nat.succ#doc",
                    "formal_statement": "def Nat.succ",
                    "informal_statement": "Successor"
                }
            ]
        });
        let results = parse_leanfinder_results(&json);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].full_name, "Nat.succ");
    }

    /// #126: leanfinder skips URLs without pattern= (matching Python regex).
    #[test]
    fn leanfinder_skips_non_pattern_urls() {
        let json = json!({
            "results": [
                {
                    "url": "https://leanprover-community.github.io/mathlib4_docs/Mathlib/Foo.html",
                    "formal_statement": "def Foo",
                    "informal_statement": "A definition"
                }
            ]
        });
        let results = parse_leanfinder_results(&json);
        assert!(
            results.is_empty(),
            "URLs without pattern= should be skipped"
        );
    }

    /// #126: leanfinder filters by mathlib4_docs in URL.
    #[test]
    fn leanfinder_filters_by_url_not_corpus() {
        let json = json!({
            "results": [
                {
                    "url": "https://leanprover-community.github.io/lean4_docs/find/?pattern=True#doc",
                    "formal_statement": "def True",
                    "informal_statement": "The true proposition"
                },
                {
                    "url": "https://leanprover-community.github.io/mathlib4_docs/find/?pattern=mul_comm#doc",
                    "formal_statement": "theorem mul_comm",
                    "informal_statement": "Commutativity"
                }
            ]
        });
        let results = parse_leanfinder_results(&json);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].full_name, "mul_comm");
    }

    /// #128: loogle requests include User-Agent header.
    /// The http_client() function sets User-Agent, verified via wiremock.
    #[tokio::test]
    async fn loogle_sends_user_agent_header() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/json"))
            .and(wiremock::matchers::header("User-Agent", "lean-lsp-mcp/0.1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "hits": [{"name": "A", "type": "T", "module": "M"}]
            })))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let result = handle_loogle_remote("test", 8, &config).await.unwrap();
        assert_eq!(result.items.len(), 1);
    }

    /// #128: leansearch requests include User-Agent header.
    #[tokio::test]
    async fn leansearch_sends_user_agent_header() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/search"))
            .and(wiremock::matchers::header("User-Agent", "lean-lsp-mcp/0.1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [[]]
            })))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let result = handle_leansearch("query", 5, &config).await.unwrap();
        assert!(result.items.is_empty());
    }

    /// #128: leanfinder requests include User-Agent header.
    #[tokio::test]
    async fn leanfinder_sends_user_agent_header() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(wiremock::matchers::header("User-Agent", "lean-lsp-mcp/0.1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
            .mount(&server)
            .await;

        let config = mock_config(&server.uri());
        let result = handle_leanfinder("query", 5, &config).await.unwrap();
        assert!(result.items.is_empty());
    }

    /// #126: leanfinder with top-level array (backward compatibility fallback).
    #[test]
    fn leanfinder_fallback_top_level_array() {
        let json = json!([
            {
                "url": "https://leanprover-community.github.io/mathlib4_docs/find/?pattern=add_comm#doc",
                "formal_statement": "theorem add_comm",
                "informal_statement": "Addition commutes"
            }
        ]);
        let results = parse_leanfinder_results(&json);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].full_name, "add_comm");
    }
}
