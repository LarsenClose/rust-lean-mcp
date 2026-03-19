//! Integration smoke tests.
//!
//! These tests spawn the actual binary and verify end-to-end behavior
//! that unit tests with mocks cannot catch.

use crate::helpers::McpTestClient;
use serde_json::json;

// ---------------------------------------------------------------------------
// Binary smoke tests
// ---------------------------------------------------------------------------

/// Spawn the actual binary, initialize, call lean_local_search (no LSP needed),
/// and verify we get a valid (non-error) JSON-RPC response.
#[tokio::test]
async fn binary_smoke_local_search() {
    let mut client = McpTestClient::spawn().await;
    let init_resp = client.initialize().await;

    // Verify server initialized successfully
    assert!(
        init_resp.get("result").is_some(),
        "initialize should return a result"
    );
    let server_info = &init_resp["result"]["serverInfo"];
    assert_eq!(server_info["name"].as_str().unwrap(), "Lean LSP");

    // Call lean_local_search — doesn't require an LSP connection.
    // With no project path, should return an error (not crash).
    let response = client
        .call_tool(
            "lean_local_search",
            json!({
                "query": "add_comm",
            }),
        )
        .await;

    // Should get a valid response (either results or a graceful error)
    assert!(
        response.get("result").is_some() || response.get("error").is_some(),
        "lean_local_search should return result or error, got: {:?}",
        response
    );

    // If it returns content, verify the structure
    if let Some(result) = response.get("result") {
        let content = result.get("content");
        assert!(content.is_some(), "tool result should have content field");
    }

    client.shutdown().await;
}

/// Spawn the binary with a fixture project dir and verify lean_local_search runs
/// against actual files on disk.
#[tokio::test]
async fn binary_local_search_with_project_path() {
    let fixture_dir = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/lean_project"
    );

    let mut client = McpTestClient::spawn_with_args(&["--lean-project-path", fixture_dir]).await;
    client.initialize().await;

    let response = client
        .call_tool(
            "lean_local_search",
            json!({
                "query": "test_theorem",
            }),
        )
        .await;

    // Should return a result (possibly empty if rg doesn't recognize .lean type)
    assert!(
        response.get("result").is_some(),
        "should get a result, got: {:?}",
        response
    );

    // The result should be valid JSON content
    let content = &response["result"]["content"];
    assert!(content.is_array(), "content should be an array");
    let text = content[0]["text"].as_str().unwrap_or("");
    // Should be parseable JSON
    assert!(
        serde_json::from_str::<serde_json::Value>(text).is_ok(),
        "content text should be valid JSON: {text}"
    );

    client.shutdown().await;
}

// ---------------------------------------------------------------------------
// AppContext initialization flow
// ---------------------------------------------------------------------------

/// Verify AppContext is created with a project path and LSP-requiring tools
/// fail gracefully (no crash).
#[tokio::test]
async fn lsp_tool_without_client_returns_error() {
    let mut client = McpTestClient::spawn_with_args(&["--lean-project-path", "/tmp/fake"]).await;
    let init_resp = client.initialize().await;

    assert!(init_resp.get("result").is_some());

    // An LSP-requiring tool should fail gracefully (not crash)
    let response = client
        .call_tool(
            "lean_goal",
            json!({
                "file_path": "Test.lean",
                "line": 1,
            }),
        )
        .await;

    // Should get a response (not timeout/crash)
    let result = &response["result"];
    if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
        if let Some(text) = content
            .first()
            .and_then(|c| c.get("text"))
            .and_then(|t| t.as_str())
        {
            // Should mention LSP/client not connected
            assert!(
                text.contains("LSP") || text.contains("client") || text.contains("error"),
                "expected LSP-related error, got: {text}"
            );
        }
    }

    client.shutdown().await;
}

/// Verify the server lists all expected tools after initialization.
#[tokio::test]
async fn initialized_server_lists_search_tools() {
    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    let tools_resp = client.list_tools().await;
    let tools = tools_resp["result"]["tools"]
        .as_array()
        .expect("tools should be array");

    let tool_names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    // All four search tools that correspond to our fixtures must exist
    assert!(
        tool_names.contains(&"lean_leansearch"),
        "missing lean_leansearch"
    );
    assert!(tool_names.contains(&"lean_loogle"), "missing lean_loogle");
    assert!(
        tool_names.contains(&"lean_leanfinder"),
        "missing lean_leanfinder"
    );
    assert!(
        tool_names.contains(&"lean_local_search"),
        "missing lean_local_search"
    );

    client.shutdown().await;
}

/// Verify server handles multiple sequential tool calls without crashing.
#[tokio::test]
async fn multiple_tool_calls_dont_crash() {
    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    // Call several different tools in sequence
    let tools = [
        ("lean_local_search", json!({"query": "test"})),
        ("lean_local_search", json!({"query": "add_comm"})),
        (
            "lean_loogle",
            json!({"query": "Nat -> Nat", "num_results": 3}),
        ),
    ];

    for (name, args) in &tools {
        let response = client.call_tool(name, args.clone()).await;
        // Each call should return a valid response (result or error, not crash)
        assert!(
            response.get("result").is_some() || response.get("error").is_some(),
            "{name} should return result or error, got: {response:?}"
        );
    }

    client.shutdown().await;
}
