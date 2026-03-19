//! MCP protocol handshake tests.

use crate::helpers::McpTestClient;
use serde_json::json;

#[tokio::test]
async fn initialize_returns_server_info() {
    let mut client = McpTestClient::spawn().await;
    let response = client.initialize().await;

    let result = response.get("result").expect("missing result");

    // Server info
    let server_info = result.get("serverInfo").expect("missing serverInfo");
    assert_eq!(server_info["name"], "Lean LSP");
    assert!(!server_info["version"].as_str().unwrap().is_empty());

    // Protocol version
    assert!(result.get("protocolVersion").is_some());

    // Capabilities should advertise tools
    let capabilities = result.get("capabilities").expect("missing capabilities");
    assert!(
        capabilities.get("tools").is_some(),
        "server should advertise tools capability"
    );

    // Instructions should be present
    assert!(result.get("instructions").is_some());

    client.shutdown().await;
}

#[tokio::test]
async fn initialize_response_includes_instructions_with_key_sections() {
    let mut client = McpTestClient::spawn().await;
    let response = client.initialize().await;

    let instructions = response["result"]["instructions"]
        .as_str()
        .expect("instructions should be a string");

    assert!(
        instructions.contains("## General Rules"),
        "instructions should contain General Rules"
    );
    assert!(
        instructions.contains("## Key Tools"),
        "instructions should contain Key Tools"
    );
    assert!(
        instructions.contains("## Search Tools"),
        "instructions should contain Search Tools"
    );

    client.shutdown().await;
}

#[tokio::test]
async fn ping_responds() {
    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    let response = client
        .request("ping", json!({}))
        .await
        .expect("server should respond to ping");
    // ping should return a result (empty object)
    assert!(
        response.get("result").is_some(),
        "ping should return a result"
    );

    client.shutdown().await;
}
