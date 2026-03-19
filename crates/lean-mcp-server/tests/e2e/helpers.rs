//! Shared test helpers for spawning and communicating with the MCP server.

use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::{timeout, Duration};

/// Default timeout for reading a response from the server.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// MCP test client that spawns and communicates with the server binary.
pub struct McpTestClient {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: AtomicU64,
}

impl McpTestClient {
    /// Spawn the server binary with default args (no Lean project).
    pub async fn spawn() -> Self {
        Self::spawn_with_args(&[]).await
    }

    /// Spawn the server binary with custom CLI arguments.
    pub async fn spawn_with_args(args: &[&str]) -> Self {
        let bin_path = assert_cmd::cargo::cargo_bin("rust-lean-mcp");
        let mut child = Command::new(bin_path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn rust-lean-mcp");

        let stdin = child.stdin.take().expect("stdin not captured");
        let stdout = child.stdout.take().expect("stdout not captured");
        let reader = BufReader::new(stdout);

        Self {
            child,
            stdin,
            reader,
            next_id: AtomicU64::new(1),
        }
    }

    /// Allocate the next request ID.
    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Send a JSON-RPC request and return the response.
    /// Returns `None` if the server closed the connection before responding.
    pub async fn request(&mut self, method: &str, params: Value) -> Option<Value> {
        let id = self.next_id();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let mut line = serde_json::to_string(&msg).unwrap();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();

        // Read responses until we find the one matching our ID.
        // Skip notifications (messages without an "id" field).
        loop {
            let response = self.read_message().await?;
            if response.get("id") == Some(&json!(id)) {
                return Some(response);
            }
            // If it's a notification, loop and read the next message.
        }
    }

    /// Send a JSON-RPC notification (no response expected).
    pub async fn notify(&mut self, method: &str, params: Value) {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        let mut line = serde_json::to_string(&msg).unwrap();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    /// Read a single newline-delimited JSON message from stdout.
    /// Returns `None` if the server closed the connection.
    async fn read_message(&mut self) -> Option<Value> {
        let mut buf = String::new();
        let bytes = timeout(READ_TIMEOUT, self.reader.read_line(&mut buf))
            .await
            .expect("timeout reading from server")
            .expect("IO error reading from server");
        if bytes == 0 || buf.trim().is_empty() {
            return None; // Server closed connection
        }
        Some(serde_json::from_str(buf.trim()).expect("invalid JSON from server"))
    }

    /// Perform the MCP initialize handshake and return the server info.
    pub async fn initialize(&mut self) -> Value {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "test-client",
                        "version": "0.1.0"
                    }
                }),
            )
            .await
            .expect("server closed connection during initialize");

        // Send initialized notification to complete the handshake.
        self.notify("notifications/initialized", json!({})).await;

        result
    }

    /// Request the list of available tools.
    pub async fn list_tools(&mut self) -> Value {
        self.request("tools/list", json!({}))
            .await
            .expect("server closed connection during tools/list")
    }

    /// Call a tool by name with the given arguments.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments,
            }),
        )
        .await
        .expect("server closed connection during tools/call")
    }

    /// Close stdin to signal the server to shut down, then wait for exit.
    pub async fn shutdown(mut self) -> std::process::ExitStatus {
        drop(self.stdin);
        timeout(Duration::from_secs(5), self.child.wait())
            .await
            .expect("timeout waiting for server exit")
            .expect("error waiting for server exit")
    }
}
