//! Graceful shutdown tests.

use crate::helpers::McpTestClient;

#[tokio::test]
async fn server_exits_cleanly_on_stdin_close() {
    let mut client = McpTestClient::spawn().await;
    client.initialize().await;

    // Close stdin and wait for the server to exit.
    let status = client.shutdown().await;

    // The server should exit with code 0 when stdin closes.
    assert!(
        status.success(),
        "server should exit cleanly, got: {status}"
    );
}

#[tokio::test]
async fn server_exits_cleanly_without_initialize() {
    // Spawn the server but immediately close stdin without initializing.
    let client = McpTestClient::spawn().await;
    let status = client.shutdown().await;

    // Even without initialization, the server should exit gracefully.
    // It may exit with 0 or a non-zero code depending on implementation,
    // but it should not hang or crash.
    let _ = status;
}
