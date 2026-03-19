//! LSP transport layer with JSON-RPC over stdio and Content-Length framing.
//!
//! The Lean LSP server (`lake serve`) communicates via JSON-RPC 2.0 over stdio
//! with Content-Length framing:
//!
//! ```text
//! Content-Length: 123\r\n
//! \r\n
//! {"jsonrpc":"2.0","id":1,"method":"initialize","params":{...}}
//! ```

use std::path::Path;

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::error::TransportError;

/// Writes a JSON-RPC message with Content-Length framing to the given writer.
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    message: &Value,
) -> Result<(), TransportError> {
    let body = serde_json::to_string(message)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(body.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads a JSON-RPC message with Content-Length framing from the given reader.
pub async fn read_message<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Value, TransportError> {
    let content_length = read_content_length(reader).await?;
    let mut body_buf = vec![0u8; content_length];
    reader.read_exact(&mut body_buf).await?;
    let value: Value = serde_json::from_slice(&body_buf)?;
    Ok(value)
}

/// Parses the Content-Length header from the stream, consuming headers up to
/// the blank line separator.
async fn read_content_length<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<usize, TransportError> {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            return Err(TransportError::Closed);
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            // Blank line signals end of headers.
            break;
        }

        if let Some(value_str) = trimmed.strip_prefix("Content-Length:") {
            let value_str = value_str.trim();
            content_length = Some(value_str.parse::<usize>().map_err(|_| {
                TransportError::InvalidHeader(format!("invalid Content-Length value: {value_str}"))
            })?);
        }
        // Ignore other headers (e.g., Content-Type).
    }

    content_length
        .ok_or_else(|| TransportError::InvalidHeader("missing Content-Length header".to_string()))
}

/// An LSP transport that communicates with a subprocess via stdio.
///
/// Spawns `lake serve` and provides methods to send and receive JSON-RPC
/// messages with Content-Length framing.
pub struct StdioTransport {
    child: Child,
}

impl StdioTransport {
    /// Spawn `lake serve` as a subprocess in the given project directory.
    pub async fn spawn(project_path: &Path) -> Result<Self, TransportError> {
        let child = Command::new("lake")
            .arg("serve")
            .current_dir(project_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        Ok(Self { child })
    }

    /// Send a JSON-RPC message with Content-Length framing.
    pub async fn send(&mut self, message: &Value) -> Result<(), TransportError> {
        let stdin = self
            .child
            .stdin
            .as_mut()
            .ok_or(TransportError::StdinClosed)?;
        write_message(stdin, message).await
    }

    /// Read a JSON-RPC message with Content-Length framing.
    pub async fn receive(&mut self) -> Result<Value, TransportError> {
        let stdout = self
            .child
            .stdout
            .as_mut()
            .ok_or(TransportError::StdoutClosed)?;
        let mut reader = tokio::io::BufReader::new(stdout);
        read_message(&mut reader).await
    }

    /// Shutdown the transport gracefully.
    ///
    /// Drops stdin to signal EOF, then waits for the child process to exit.
    /// If the process does not exit, it is killed.
    pub async fn shutdown(&mut self) -> Result<(), TransportError> {
        // Drop stdin to signal EOF to the server.
        drop(self.child.stdin.take());

        // Try to wait for the child to exit, then kill if needed.
        match tokio::time::timeout(std::time::Duration::from_secs(5), self.child.wait()).await {
            Ok(Ok(status)) => {
                if status.success() {
                    Ok(())
                } else {
                    Err(TransportError::ProcessExited(status.code()))
                }
            }
            Ok(Err(e)) => Err(TransportError::Io(e)),
            Err(_) => {
                // Timeout: force kill.
                self.child.kill().await?;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[tokio::test]
    async fn test_write_message_formats_correctly() {
        let mut buf = Vec::new();
        let msg = json!({"jsonrpc":"2.0","id":1,"method":"initialize"});
        write_message(&mut buf, &msg).await.unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.starts_with("Content-Length: "));
        assert!(output.contains("\r\n\r\n"));

        // Split at the blank line to verify structure.
        let parts: Vec<&str> = output.splitn(2, "\r\n\r\n").collect();
        assert_eq!(parts.len(), 2);

        let header = parts[0];
        let body = parts[1];

        // Verify Content-Length matches actual body length.
        let declared_len: usize = header
            .strip_prefix("Content-Length: ")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(declared_len, body.len());

        // Verify body is valid JSON matching the original.
        let parsed: Value = serde_json::from_str(body).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
    }

    #[tokio::test]
    async fn test_read_message_parses_correctly() {
        let msg = json!({"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}});
        let body = serde_json::to_string(&msg).unwrap();
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);

        let mut reader = tokio::io::BufReader::new(frame.as_bytes());
        let result = read_message(&mut reader).await.unwrap();

        assert_eq!(result["jsonrpc"], "2.0");
        assert_eq!(result["id"], 1);
        assert!(result.get("result").is_some());
    }

    #[tokio::test]
    async fn test_roundtrip_write_then_read() {
        let original = json!({"jsonrpc":"2.0","id":42,"method":"textDocument/hover","params":{"position":{"line":0,"character":5}}});

        // Write to buffer.
        let mut buf = Vec::new();
        write_message(&mut buf, &original).await.unwrap();

        // Read back from buffer.
        let mut reader = tokio::io::BufReader::new(buf.as_slice());
        let result = read_message(&mut reader).await.unwrap();

        assert_eq!(result, original);
    }

    #[tokio::test]
    async fn test_read_message_missing_content_length() {
        let frame = "\r\n{\"jsonrpc\":\"2.0\"}";
        let mut reader = tokio::io::BufReader::new(frame.as_bytes());
        let result = read_message(&mut reader).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, TransportError::InvalidHeader(_)),
            "Expected InvalidHeader, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn test_read_message_malformed_content_length() {
        let frame = "Content-Length: abc\r\n\r\n{}";
        let mut reader = tokio::io::BufReader::new(frame.as_bytes());
        let result = read_message(&mut reader).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, TransportError::InvalidHeader(_)),
            "Expected InvalidHeader, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn test_read_message_closed_stream() {
        let frame = b"";
        let mut reader = tokio::io::BufReader::new(frame.as_slice());
        let result = read_message(&mut reader).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, TransportError::Closed),
            "Expected Closed, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn test_read_message_ignores_extra_headers() {
        let msg = json!({"jsonrpc":"2.0","id":1,"result":null});
        let body = serde_json::to_string(&msg).unwrap();
        let frame = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );

        let mut reader = tokio::io::BufReader::new(frame.as_bytes());
        let result = read_message(&mut reader).await.unwrap();
        assert_eq!(result["id"], 1);
    }

    #[tokio::test]
    async fn test_multiple_messages_in_sequence() {
        let msg1 = json!({"jsonrpc":"2.0","id":1,"method":"initialize"});
        let msg2 = json!({"jsonrpc":"2.0","id":2,"method":"shutdown"});

        let mut buf = Vec::new();
        write_message(&mut buf, &msg1).await.unwrap();
        write_message(&mut buf, &msg2).await.unwrap();

        let mut reader = tokio::io::BufReader::new(buf.as_slice());
        let result1 = read_message(&mut reader).await.unwrap();
        let result2 = read_message(&mut reader).await.unwrap();

        assert_eq!(result1["id"], 1);
        assert_eq!(result1["method"], "initialize");
        assert_eq!(result2["id"], 2);
        assert_eq!(result2["method"], "shutdown");
    }

    #[tokio::test]
    async fn test_duplex_write_and_read() {
        let (client_writer, server_reader) = tokio::io::duplex(4096);
        let (server_writer, client_reader) = tokio::io::duplex(4096);

        let msg = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}});
        let resp = json!({"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}});

        let msg_clone = msg.clone();
        let resp_clone = resp.clone();

        // Simulate client sending, server receiving, server responding.
        let client_handle = tokio::spawn(async move {
            let mut writer = client_writer;
            let mut reader = tokio::io::BufReader::new(client_reader);

            write_message(&mut writer, &msg_clone).await.unwrap();
            let received_resp = read_message(&mut reader).await.unwrap();
            received_resp
        });

        let server_handle = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(server_reader);
            let mut writer = server_writer;

            let received_msg = read_message(&mut reader).await.unwrap();
            write_message(&mut writer, &resp_clone).await.unwrap();
            received_msg
        });

        let (client_result, server_result) = tokio::join!(client_handle, server_handle);
        let received_resp = client_result.unwrap();
        let received_msg = server_result.unwrap();

        assert_eq!(received_msg, msg);
        assert_eq!(received_resp, resp);
    }

    #[test]
    fn test_transport_error_display() {
        let err = TransportError::StdinClosed;
        assert_eq!(format!("{err}"), "stdin closed");

        let err = TransportError::StdoutClosed;
        assert_eq!(format!("{err}"), "stdout closed");

        let err = TransportError::Closed;
        assert_eq!(format!("{err}"), "Transport closed");

        let err = TransportError::ProcessExited(Some(1));
        assert_eq!(
            format!("{err}"),
            "LSP server process exited with code Some(1)"
        );

        let err = TransportError::ProcessExited(None);
        assert_eq!(format!("{err}"), "LSP server process exited with code None");

        let err = TransportError::InvalidHeader("bad header".to_string());
        assert_eq!(
            format!("{err}"),
            "Invalid Content-Length header: bad header"
        );
    }

    #[test]
    fn test_transport_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken");
        let transport_err: TransportError = io_err.into();
        assert!(matches!(transport_err, TransportError::Io(_)));
    }

    #[test]
    fn test_transport_error_from_json() {
        let json_err = serde_json::from_str::<Value>("not json").unwrap_err();
        let transport_err: TransportError = json_err.into();
        assert!(matches!(transport_err, TransportError::Json(_)));
    }

    /// Assert that TransportError is Send + Sync for use across async tasks.
    #[test]
    fn test_transport_error_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<TransportError>();
        assert_sync::<TransportError>();
    }

    /// Assert that StdioTransport is Send for use across async tasks.
    #[test]
    fn test_stdio_transport_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<StdioTransport>();
    }
}
