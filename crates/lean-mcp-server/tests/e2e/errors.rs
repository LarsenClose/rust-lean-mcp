//! Error handling tests.

use crate::helpers::McpTestClient;
use serde_json::json;

#[tokio::test]
async fn call_nonexistent_tool_returns_error() {
    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    let response = client
        .call_tool("nonexistent_tool", json!({"foo": "bar"}))
        .await;

    // Should return a JSON-RPC error or an MCP error result.
    let has_error = response.get("error").is_some()
        || response
            .get("result")
            .and_then(|r| r["isError"].as_bool())
            .unwrap_or(false);

    assert!(
        has_error,
        "calling a nonexistent tool should return an error"
    );

    client.shutdown().await;
}

#[tokio::test]
async fn call_tool_with_missing_required_params_returns_error() {
    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    // lean_goal requires file_path and line but we omit them.
    let response = client.call_tool("lean_goal", json!({})).await;

    let has_error = response.get("error").is_some()
        || response
            .get("result")
            .and_then(|r| r["isError"].as_bool())
            .unwrap_or(false);

    assert!(
        has_error,
        "calling lean_goal with no params should return an error"
    );

    client.shutdown().await;
}

#[tokio::test]
async fn call_tool_with_wrong_param_types_returns_error() {
    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    // lean_goal expects line as integer, not string.
    let response = client
        .call_tool(
            "lean_goal",
            json!({
                "file_path": "/tmp/test.lean",
                "line": "not_a_number"
            }),
        )
        .await;

    let has_error = response.get("error").is_some()
        || response
            .get("result")
            .and_then(|r| r["isError"].as_bool())
            .unwrap_or(false);

    assert!(
        has_error,
        "calling lean_goal with string line should return an error"
    );

    client.shutdown().await;
}

#[tokio::test]
async fn unknown_json_rpc_method_returns_error_or_disconnects() {
    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    // Send a request with an unknown method.
    let response = client.request("unknown/method", json!({})).await;

    // The server may either return a JSON-RPC error or close the connection.
    match response {
        Some(resp) => {
            assert!(
                resp.get("error").is_some(),
                "unknown method should return a JSON-RPC error, got: {resp}"
            );
        }
        None => {
            // Server closed connection - also acceptable for unknown methods.
        }
    }
}
