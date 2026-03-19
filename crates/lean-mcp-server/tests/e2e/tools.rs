//! Tests for tool listing and tool call behaviour.

use crate::helpers::McpTestClient;
use serde_json::json;

/// All 25 tool names the server must advertise.
const EXPECTED_TOOLS: &[&str] = &[
    "lean_build",
    "lean_file_outline",
    "lean_diagnostic_messages",
    "lean_goal",
    "lean_proof_diff",
    "lean_term_goal",
    "lean_hover_info",
    "lean_completions",
    "lean_declaration_file",
    "lean_references",
    "lean_multi_attempt",
    "lean_run_code",
    "lean_verify",
    "lean_local_search",
    "lean_leansearch",
    "lean_loogle",
    "lean_leanfinder",
    "lean_state_search",
    "lean_hammer_premise",
    "lean_code_actions",
    "lean_get_widgets",
    "lean_get_widget_source",
    "lean_profile_proof",
    "lean_goals_batch",
    "lean_batch",
];

#[tokio::test]
async fn tools_list_returns_all_25_tools() {
    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    let response = client.list_tools().await;
    let tools = response["result"]["tools"]
        .as_array()
        .expect("tools should be an array");

    assert_eq!(
        tools.len(),
        EXPECTED_TOOLS.len(),
        "expected {} tools, got {}",
        EXPECTED_TOOLS.len(),
        tools.len()
    );

    let tool_names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("tool name should be a string"))
        .collect();

    for expected in EXPECTED_TOOLS {
        assert!(tool_names.contains(expected), "missing tool: {expected}");
    }

    client.shutdown().await;
}

#[tokio::test]
async fn every_tool_has_description_and_input_schema() {
    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    let response = client.list_tools().await;
    let tools = response["result"]["tools"]
        .as_array()
        .expect("tools should be an array");

    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        assert!(
            tool.get("description").is_some()
                && tool["description"].as_str().is_some_and(|d| !d.is_empty()),
            "tool {name} should have a non-empty description"
        );
        assert!(
            tool.get("inputSchema").is_some(),
            "tool {name} should have an inputSchema"
        );
        assert_eq!(
            tool["inputSchema"]["type"], "object",
            "tool {name} inputSchema type should be 'object'"
        );
    }

    client.shutdown().await;
}

#[tokio::test]
async fn lean_goal_returns_error_without_lsp_client() {
    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    let response = client
        .call_tool(
            "lean_goal",
            json!({
                "file_path": "/tmp/test.lean",
                "line": 1
            }),
        )
        .await;

    // The tool should return an error because no LSP client is connected.
    let result = &response["result"];
    assert!(
        result["isError"].as_bool().unwrap_or(false),
        "lean_goal without LSP should return isError=true"
    );

    // The error content should mention the missing LSP client or project path.
    let content = result["content"]
        .as_array()
        .expect("content should be an array");
    assert!(!content.is_empty());
    let text = content[0]["text"].as_str().unwrap_or("");
    assert!(
        text.to_lowercase().contains("lsp")
            || text.to_lowercase().contains("client")
            || text.to_lowercase().contains("project path")
            || text.to_lowercase().contains("not configured"),
        "error should mention LSP client or project path: got {text}"
    );

    client.shutdown().await;
}

#[tokio::test]
async fn lean_build_returns_error_without_project_path() {
    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    let response = client.call_tool("lean_build", json!({})).await;

    let result = &response["result"];
    assert!(
        result["isError"].as_bool().unwrap_or(false),
        "lean_build without project path should return isError=true"
    );

    let text = result["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.to_lowercase().contains("project") || text.to_lowercase().contains("path"),
        "error should mention project path: got {text}"
    );

    client.shutdown().await;
}

#[tokio::test]
async fn lean_profile_proof_returns_error_without_project_path() {
    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    let response = client
        .call_tool(
            "lean_profile_proof",
            json!({
                "file_path": "/tmp/test.lean",
                "theorem_name": "foo",
                "line": 1,
                "column": 1
            }),
        )
        .await;

    let result = &response["result"];
    assert!(
        result["isError"].as_bool().unwrap_or(false),
        "lean_profile_proof without project path should return isError=true"
    );

    client.shutdown().await;
}

/// Verify that all LSP-dependent tools return appropriate errors when no LSP client is connected.
#[tokio::test]
async fn lsp_tools_return_error_without_client() {
    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    // All tools that require LSP client
    let lsp_tools = &[
        ("lean_file_outline", json!({"file_path": "/tmp/test.lean"})),
        (
            "lean_diagnostic_messages",
            json!({"file_path": "/tmp/test.lean"}),
        ),
        (
            "lean_goal",
            json!({"file_path": "/tmp/test.lean", "line": 1}),
        ),
        (
            "lean_term_goal",
            json!({"file_path": "/tmp/test.lean", "line": 1}),
        ),
        (
            "lean_hover_info",
            json!({"file_path": "/tmp/test.lean", "line": 1, "column": 1}),
        ),
        (
            "lean_completions",
            json!({"file_path": "/tmp/test.lean", "line": 1, "column": 1}),
        ),
        (
            "lean_declaration_file",
            json!({"file_path": "/tmp/test.lean", "symbol": "foo"}),
        ),
        (
            "lean_references",
            json!({"file_path": "/tmp/test.lean", "line": 1, "column": 1}),
        ),
        (
            "lean_multi_attempt",
            json!({"file_path": "/tmp/test.lean", "line": 1, "snippets": ["simp"]}),
        ),
        (
            "lean_code_actions",
            json!({"file_path": "/tmp/test.lean", "line": 1, "column": 1}),
        ),
        (
            "lean_get_widgets",
            json!({"file_path": "/tmp/test.lean", "line": 1, "column": 1}),
        ),
        (
            "lean_get_widget_source",
            json!({"file_path": "/tmp/test.lean", "javascript_hash": "abc123"}),
        ),
    ];

    for (tool_name, args) in lsp_tools {
        let response = client.call_tool(tool_name, args.clone()).await;
        let result = &response["result"];
        assert!(
            result["isError"].as_bool().unwrap_or(false),
            "{tool_name} without LSP should return isError=true"
        );
    }

    client.shutdown().await;
}

/// Tools that provide required parameters should have them in their schema.
#[tokio::test]
async fn tool_schemas_have_required_params() {
    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    let response = client.list_tools().await;
    let tools = response["result"]["tools"]
        .as_array()
        .expect("tools should be an array");

    // lean_goal requires file_path and line
    let goal_tool = tools
        .iter()
        .find(|t| t["name"] == "lean_goal")
        .expect("lean_goal tool not found");

    let required = goal_tool["inputSchema"]["required"]
        .as_array()
        .expect("lean_goal should have required params");

    let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();

    assert!(
        required_names.contains(&"file_path"),
        "lean_goal should require file_path"
    );
    assert!(
        required_names.contains(&"line"),
        "lean_goal should require line"
    );

    client.shutdown().await;
}
