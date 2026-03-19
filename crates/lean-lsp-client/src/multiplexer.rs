//! Async LSP message multiplexer for concurrent request dispatch.
//!
//! Reads all incoming messages from the LSP server in a background task,
//! dispatches responses to waiting callers by matching request ID, and
//! routes notifications (e.g., `textDocument/publishDiagnostics`) to handlers.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, error, warn};

use crate::error::TransportError;
use crate::jsonrpc::{Message, Notification, Request, RequestId};
use crate::transport::{read_message, write_message};

/// Default timeout for requests (30 seconds).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// A pending request waiting for its response.
type PendingRequest = oneshot::Sender<Value>;

/// Notification handler callback type.
type NotificationHandler = Arc<dyn Fn(String, Value) + Send + Sync>;

/// Errors that can occur during multiplexer operations.
#[derive(Debug, thiserror::Error)]
pub enum MultiplexerError {
    /// A transport-level error occurred.
    #[error("Transport error: {0}")]
    Transport(#[from] TransportError),

    /// A request timed out waiting for its response.
    #[error("Request timed out after {0:?}")]
    Timeout(Duration),

    /// The multiplexer has been shut down.
    #[error("Multiplexer shut down")]
    Shutdown,

    /// Sending on a channel failed because the receiver was dropped.
    #[error("Send failed: channel closed")]
    ChannelClosed,
}

/// Multiplexes LSP JSON-RPC messages: dispatches responses by request ID,
/// routes notifications to handlers.
///
/// Spawns background reader and writer tasks that communicate over channels.
/// The reader task reads all incoming messages and either resolves a pending
/// request or invokes the notification handler. The writer task serializes
/// outgoing messages with Content-Length framing.
pub struct Multiplexer {
    /// Channel to send outgoing messages to the writer task.
    outgoing_tx: mpsc::Sender<Value>,
    /// Pending requests awaiting responses, keyed by request ID.
    pending: Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
    /// Next request ID.
    next_id: Arc<Mutex<i64>>,
    /// Notification handler.
    notification_handler: Arc<Mutex<Option<NotificationHandler>>>,
    /// Background reader task handle.
    reader_handle: Option<tokio::task::JoinHandle<()>>,
    /// Background writer task handle.
    writer_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Multiplexer {
    /// Create a new multiplexer from async read/write streams.
    ///
    /// Spawns background reader and writer tasks immediately. The reader
    /// continuously reads messages from the LSP server and dispatches them.
    /// The writer serializes outgoing messages with Content-Length framing.
    pub fn new<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncBufRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending: Arc<Mutex<HashMap<RequestId, PendingRequest>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let notification_handler: Arc<Mutex<Option<NotificationHandler>>> =
            Arc::new(Mutex::new(None));

        // Channel for outgoing messages (writer task consumes these).
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Value>(64);

        // Spawn the writer task.
        let writer_handle = tokio::spawn(Self::writer_loop(writer, outgoing_rx));

        // Spawn the reader task.
        let reader_pending = Arc::clone(&pending);
        let reader_notification_handler = Arc::clone(&notification_handler);
        let reader_handle = tokio::spawn(Self::reader_loop(
            reader,
            reader_pending,
            reader_notification_handler,
        ));

        Self {
            outgoing_tx,
            pending,
            next_id: Arc::new(Mutex::new(1)),
            notification_handler,
            reader_handle: Some(reader_handle),
            writer_handle: Some(writer_handle),
        }
    }

    /// Set a handler for notifications (e.g., `textDocument/publishDiagnostics`).
    ///
    /// The handler receives the notification method name and params value.
    /// Only one handler is supported; calling this again replaces the previous one.
    pub async fn set_notification_handler<F>(&self, handler: F)
    where
        F: Fn(String, Value) + Send + Sync + 'static,
    {
        let mut guard = self.notification_handler.lock().await;
        *guard = Some(Arc::new(handler));
    }

    /// Send a request and wait for the response.
    ///
    /// Allocates a unique request ID, registers a pending response channel,
    /// sends the request through the writer task, and waits for the matching
    /// response (with a default timeout).
    pub async fn request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, MultiplexerError> {
        self.request_with_timeout(method, params, DEFAULT_TIMEOUT)
            .await
    }

    /// Send a request and wait for the response with a custom timeout.
    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, MultiplexerError> {
        // 1. Allocate next request ID.
        let numeric_id = {
            let mut next = self.next_id.lock().await;
            let id = *next;
            *next += 1;
            id
        };
        let id = RequestId::Number(numeric_id);

        // 2. Create oneshot channel.
        let (tx, rx) = oneshot::channel();

        // 3. Register in pending map.
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id.clone(), tx);
        }

        // 4. Build and send request via outgoing channel.
        let request = Request::new(numeric_id, method, params);
        let msg_value = serde_json::to_value(&request)
            .map_err(|e| MultiplexerError::Transport(TransportError::Json(e)))?;

        debug!(%id, method, "Sending request");

        self.outgoing_tx
            .send(msg_value)
            .await
            .map_err(|_| MultiplexerError::ChannelClosed)?;

        // 5. Wait on oneshot receiver (with timeout).
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => {
                // Sender was dropped (e.g., multiplexer shut down).
                Err(MultiplexerError::Shutdown)
            }
            Err(_) => {
                // Timeout: remove from pending map.
                let mut pending = self.pending.lock().await;
                pending.remove(&id);
                Err(MultiplexerError::Timeout(timeout))
            }
        }
    }

    /// Send a notification (no response expected).
    pub async fn notify(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), MultiplexerError> {
        let notification = Notification::new(method, params);
        let msg_value = serde_json::to_value(&notification)
            .map_err(|e| MultiplexerError::Transport(TransportError::Json(e)))?;

        debug!(method, "Sending notification");

        self.outgoing_tx
            .send(msg_value)
            .await
            .map_err(|_| MultiplexerError::ChannelClosed)?;

        Ok(())
    }

    /// Shutdown the multiplexer gracefully.
    ///
    /// Drops the outgoing channel to stop the writer task, aborts the reader
    /// task, and cancels all pending requests with a shutdown error.
    pub async fn shutdown(mut self) -> Result<(), MultiplexerError> {
        // Drop the sender to close the writer channel.
        drop(self.outgoing_tx);

        // Cancel all pending requests.
        {
            let mut pending = self.pending.lock().await;
            for (id, sender) in pending.drain() {
                debug!(%id, "Cancelling pending request due to shutdown");
                // Send a null value to indicate cancellation; the receiver
                // will see this as a successful receive but with a sentinel.
                // However, dropping the sender is cleaner since the receiver
                // will get a RecvError, which we map to Shutdown.
                drop(sender);
            }
        }

        // Abort the reader task.
        if let Some(handle) = self.reader_handle.take() {
            handle.abort();
            let _ = handle.await;
        }

        // Wait for the writer task to finish (it should finish when the
        // channel is closed).
        if let Some(handle) = self.writer_handle.take() {
            let _ = handle.await;
        }

        Ok(())
    }

    /// Background reader loop: reads messages and dispatches them.
    async fn reader_loop<R: AsyncBufRead + Unpin>(
        mut reader: R,
        pending: Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
        notification_handler: Arc<Mutex<Option<NotificationHandler>>>,
    ) {
        loop {
            match read_message(&mut reader).await {
                Ok(value) => {
                    Self::dispatch_message(value, &pending, &notification_handler).await;
                }
                Err(TransportError::Closed) => {
                    debug!("Reader stream closed (EOF)");
                    // Close all pending with error by dropping senders.
                    let mut pending = pending.lock().await;
                    let count = pending.len();
                    if count > 0 {
                        warn!(count, "Dropping pending requests due to stream close");
                    }
                    pending.clear();
                    break;
                }
                Err(e) => {
                    error!(?e, "Reader encountered error");
                    // Close all pending with error by dropping senders.
                    let mut pending = pending.lock().await;
                    pending.clear();
                    break;
                }
            }
        }
    }

    /// Dispatch a single parsed message to the correct handler.
    async fn dispatch_message(
        value: Value,
        pending: &Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
        notification_handler: &Arc<Mutex<Option<NotificationHandler>>>,
    ) {
        match Message::from_value(value.clone()) {
            Ok(Message::Response(resp)) => {
                if let Some(id) = resp.id {
                    let mut pending = pending.lock().await;
                    if let Some(sender) = pending.remove(&id) {
                        // Send the full response value so the caller can
                        // inspect result/error.
                        let _ = sender.send(value);
                        debug!(%id, "Dispatched response to pending request");
                    } else {
                        warn!(%id, "Received response for unknown request ID");
                    }
                }
            }
            Ok(Message::Notification(notif)) => {
                debug!(method = %notif.method, "Received notification");
                let handler = notification_handler.lock().await;
                if let Some(ref h) = *handler {
                    let params = notif.params.unwrap_or(Value::Null);
                    h(notif.method, params);
                }
            }
            Ok(Message::Request(req)) => {
                // Server-to-client requests (e.g., window/showMessageRequest).
                // We log but don't handle them for now.
                debug!(id = %req.id, method = %req.method, "Received server request (unhandled)");
            }
            Err(e) => {
                warn!(?e, "Failed to parse incoming message");
            }
        }
    }

    /// Background writer loop: serializes and sends outgoing messages.
    async fn writer_loop<W: AsyncWrite + Unpin>(
        mut writer: W,
        mut outgoing_rx: mpsc::Receiver<Value>,
    ) {
        while let Some(msg) = outgoing_rx.recv().await {
            if let Err(e) = write_message(&mut writer, &msg).await {
                error!(?e, "Writer encountered error");
                break;
            }
        }
        debug!("Writer loop exiting");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::BufReader;

    /// Helper: write a framed JSON-RPC message into a buffer.
    #[allow(dead_code)]
    async fn frame_message(value: &Value) -> Vec<u8> {
        let mut buf = Vec::new();
        write_message(&mut buf, value).await.unwrap();
        buf
    }

    /// Helper: create a duplex pair and wrap the read side in a BufReader.
    fn make_duplex(
        buf_size: usize,
    ) -> (tokio::io::DuplexStream, BufReader<tokio::io::DuplexStream>) {
        let (writer, reader) = tokio::io::duplex(buf_size);
        (writer, BufReader::new(reader))
    }

    // ── Test 1: Send request and receive matching response ─────────

    #[tokio::test]
    async fn test_request_receives_matching_response() {
        let (mut server_writer, client_reader) = make_duplex(4096);
        let (client_writer, server_reader) = make_duplex(4096);

        let mux = Multiplexer::new(client_reader, client_writer);

        // Spawn a "server" that reads the request and sends a response.
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(server_reader);
            let req = read_message(&mut reader).await.unwrap();
            let id = req["id"].as_i64().unwrap();
            let resp = json!({"jsonrpc": "2.0", "id": id, "result": {"status": "ok"}});
            write_message(&mut server_writer, &resp).await.unwrap();
        });

        let result = mux
            .request("test/method", Some(json!({"key": "value"})))
            .await
            .unwrap();
        assert_eq!(result["result"]["status"], "ok");

        server.await.unwrap();
        mux.shutdown().await.unwrap();
    }

    // ── Test 2: Multiple concurrent requests get correct responses ─

    #[tokio::test]
    async fn test_concurrent_requests_dispatched_correctly() {
        let (mut server_writer, client_reader) = make_duplex(8192);
        let (client_writer, server_reader) = make_duplex(8192);

        let mux = Multiplexer::new(client_reader, client_writer);

        // Server: read 3 requests, respond in reverse order.
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(server_reader);
            let mut requests = Vec::new();
            for _ in 0..3 {
                let req = read_message(&mut reader).await.unwrap();
                requests.push(req);
            }
            // Respond in reverse order to test out-of-order dispatch.
            for req in requests.into_iter().rev() {
                let id = req["id"].as_i64().unwrap();
                let method = req["method"].as_str().unwrap().to_string();
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"method_echo": method}
                });
                write_message(&mut server_writer, &resp).await.unwrap();
            }
        });

        // tokio::join! polls all futures on the same task, so &self references work.
        let (r1, r2, r3) = tokio::join!(
            mux.request("method/one", None),
            mux.request("method/two", None),
            mux.request("method/three", None),
        );

        assert_eq!(r1.unwrap()["result"]["method_echo"], "method/one");
        assert_eq!(r2.unwrap()["result"]["method_echo"], "method/two");
        assert_eq!(r3.unwrap()["result"]["method_echo"], "method/three");

        server.await.unwrap();
        mux.shutdown().await.unwrap();
    }

    // ── Test 3: Notification routing to handler ────────────────────

    #[tokio::test]
    async fn test_notification_routed_to_handler() {
        let (mut server_writer, client_reader) = make_duplex(4096);
        let (client_writer, _server_reader) = make_duplex(4096);

        let mux = Multiplexer::new(client_reader, client_writer);

        let received = Arc::new(Mutex::new(Vec::new()));
        let received_clone = Arc::clone(&received);

        mux.set_notification_handler(move |method, params| {
            let received = Arc::clone(&received_clone);
            // Use try_lock since we're in a sync context.
            if let Ok(mut guard) = received.try_lock() {
                guard.push((method, params));
            };
        })
        .await;

        // Server sends a notification.
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": "file:///test.lean", "diagnostics": []}
        });
        write_message(&mut server_writer, &notif).await.unwrap();

        // Give the reader loop time to process.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let guard = received.lock().await;
        assert_eq!(guard.len(), 1);
        assert_eq!(guard[0].0, "textDocument/publishDiagnostics");
        assert_eq!(guard[0].1["uri"], "file:///test.lean");

        mux.shutdown().await.unwrap();
    }

    // ── Test 4: Timeout on unanswered request ──────────────────────

    #[tokio::test]
    async fn test_request_timeout() {
        let (_server_writer, client_reader) = make_duplex(4096);
        let (client_writer, _server_reader) = make_duplex(4096);

        let mux = Multiplexer::new(client_reader, client_writer);

        let timeout = Duration::from_millis(100);
        let result = mux.request_with_timeout("test/slow", None, timeout).await;

        assert!(result.is_err());
        match result.unwrap_err() {
            MultiplexerError::Timeout(d) => assert_eq!(d, timeout),
            e => panic!("Expected Timeout, got: {e:?}"),
        }

        // Verify the pending map is cleaned up after timeout.
        let pending = mux.pending.lock().await;
        assert!(
            pending.is_empty(),
            "Pending map should be empty after timeout"
        );
        drop(pending);

        mux.shutdown().await.unwrap();
    }

    // ── Test 5: Shutdown cancels pending requests ──────────────────

    #[tokio::test]
    async fn test_shutdown_cancels_pending() {
        let (_server_writer, client_reader) = make_duplex(4096);
        let (client_writer, _server_reader) = make_duplex(4096);

        let mux = Multiplexer::new(client_reader, client_writer);

        // Manually register a pending request without cloning outgoing_tx
        // (cloning would keep the writer channel open during shutdown).
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = mux.pending.lock().await;
            pending.insert(RequestId::Number(999), tx);
        }

        // Shutdown should cancel the pending request by dropping all senders.
        mux.shutdown().await.unwrap();

        // The oneshot receiver should get an error (sender dropped).
        assert!(rx.await.is_err());
    }

    // ── Test 6: Request ID auto-increments ─────────────────────────

    #[tokio::test]
    async fn test_request_id_auto_increments() {
        let (mut server_writer, client_reader) = make_duplex(8192);
        let (client_writer, server_reader) = make_duplex(8192);

        let mux = Multiplexer::new(client_reader, client_writer);

        // Server: read 3 requests, respond to each, recording IDs.
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(server_reader);
            let mut ids = Vec::new();
            for _ in 0..3 {
                let req = read_message(&mut reader).await.unwrap();
                let id = req["id"].as_i64().unwrap();
                ids.push(id);
                let resp = json!({"jsonrpc": "2.0", "id": id, "result": null});
                write_message(&mut server_writer, &resp).await.unwrap();
            }
            ids
        });

        // Send 3 sequential requests.
        mux.request("m1", None).await.unwrap();
        mux.request("m2", None).await.unwrap();
        mux.request("m3", None).await.unwrap();

        let ids = server.await.unwrap();
        assert_eq!(ids, vec![1, 2, 3]);

        mux.shutdown().await.unwrap();
    }

    // ── Test 7: Send notification ──────────────────────────────────

    #[tokio::test]
    async fn test_send_notification() {
        let (_server_writer, client_reader) = make_duplex(4096);
        let (client_writer, server_reader) = make_duplex(4096);

        let mux = Multiplexer::new(client_reader, client_writer);

        mux.notify("initialized", Some(json!({}))).await.unwrap();

        // Server reads the notification.
        let mut reader = BufReader::new(server_reader);
        let msg = read_message(&mut reader).await.unwrap();

        assert_eq!(msg["method"], "initialized");
        assert!(msg.get("id").is_none());

        mux.shutdown().await.unwrap();
    }

    // ── Test 8: Notification without params ────────────────────────

    #[tokio::test]
    async fn test_notification_without_params() {
        let (_server_writer, client_reader) = make_duplex(4096);
        let (client_writer, server_reader) = make_duplex(4096);

        let mux = Multiplexer::new(client_reader, client_writer);

        mux.notify("exit", None).await.unwrap();

        let mut reader = BufReader::new(server_reader);
        let msg = read_message(&mut reader).await.unwrap();

        assert_eq!(msg["method"], "exit");
        assert_eq!(msg["jsonrpc"], "2.0");

        mux.shutdown().await.unwrap();
    }

    // ── Test 9: Multiple notifications to handler ──────────────────

    #[tokio::test]
    async fn test_multiple_notifications_to_handler() {
        let (mut server_writer, client_reader) = make_duplex(8192);
        let (client_writer, _server_reader) = make_duplex(4096);

        let mux = Multiplexer::new(client_reader, client_writer);

        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);

        mux.set_notification_handler(move |_method, _params| {
            count_clone.fetch_add(1, Ordering::SeqCst);
        })
        .await;

        // Server sends 5 notifications.
        for i in 0..5 {
            let notif = json!({
                "jsonrpc": "2.0",
                "method": "window/logMessage",
                "params": {"type": 3, "message": format!("msg {i}")}
            });
            write_message(&mut server_writer, &notif).await.unwrap();
        }

        // Give the reader loop time to process all notifications.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(count.load(Ordering::SeqCst), 5);

        mux.shutdown().await.unwrap();
    }

    // ── Test 10: Error response is delivered to caller ─────────────

    #[tokio::test]
    async fn test_error_response_delivered() {
        let (mut server_writer, client_reader) = make_duplex(4096);
        let (client_writer, server_reader) = make_duplex(4096);

        let mux = Multiplexer::new(client_reader, client_writer);

        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(server_reader);
            let req = read_message(&mut reader).await.unwrap();
            let id = req["id"].as_i64().unwrap();
            let resp = json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "Method not found"}
            });
            write_message(&mut server_writer, &resp).await.unwrap();
        });

        let result = mux.request("nonexistent/method", None).await.unwrap();
        assert_eq!(result["error"]["code"], -32601);
        assert_eq!(result["error"]["message"], "Method not found");

        server.await.unwrap();
        mux.shutdown().await.unwrap();
    }

    // ── Test 11: Reader EOF closes pending requests ────────────────

    #[tokio::test]
    async fn test_reader_eof_closes_pending() {
        let (server_writer, client_reader) = make_duplex(4096);
        let (client_writer, _server_reader) = make_duplex(4096);

        let mux = Multiplexer::new(client_reader, client_writer);

        // Manually register a pending request.
        let (_tx_unused, rx) = {
            let (tx, rx) = oneshot::channel::<Value>();
            let mut pending = mux.pending.lock().await;
            pending.insert(RequestId::Number(999), tx);
            ((), rx)
        };

        // Close the server's write end -> reader gets EOF.
        drop(server_writer);

        // Give the reader loop time to process EOF.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The oneshot should have been dropped (sender gone).
        assert!(rx.await.is_err());

        mux.shutdown().await.unwrap();
    }

    // ── Test 12: MultiplexerError Display ──────────────────────────

    #[test]
    fn test_error_display() {
        let err = MultiplexerError::Transport(TransportError::Closed);
        assert_eq!(format!("{err}"), "Transport error: Transport closed");

        let err = MultiplexerError::Timeout(Duration::from_secs(5));
        assert_eq!(format!("{err}"), "Request timed out after 5s");

        let err = MultiplexerError::Shutdown;
        assert_eq!(format!("{err}"), "Multiplexer shut down");

        let err = MultiplexerError::ChannelClosed;
        assert_eq!(format!("{err}"), "Send failed: channel closed");
    }

    // ── Test 13: MultiplexerError is Send + Sync ───────────────────

    #[test]
    fn test_error_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<MultiplexerError>();
        assert_sync::<MultiplexerError>();
    }

    // ── Test 14: Multiplexer is Send ───────────────────────────────

    #[test]
    fn test_multiplexer_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Multiplexer>();
    }

    // ── Test 15: Request with params roundtrip ─────────────────────

    #[tokio::test]
    async fn test_request_params_reach_server() {
        let (mut server_writer, client_reader) = make_duplex(4096);
        let (client_writer, server_reader) = make_duplex(4096);

        let mux = Multiplexer::new(client_reader, client_writer);

        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(server_reader);
            let req = read_message(&mut reader).await.unwrap();
            let id = req["id"].as_i64().unwrap();

            // Verify params made it through.
            assert_eq!(req["params"]["textDocument"]["uri"], "file:///test.lean");
            assert_eq!(req["params"]["position"]["line"], 5);

            let resp = json!({"jsonrpc": "2.0", "id": id, "result": {"contents": "Nat"}});
            write_message(&mut server_writer, &resp).await.unwrap();
        });

        let params = json!({
            "textDocument": {"uri": "file:///test.lean"},
            "position": {"line": 5, "character": 0}
        });
        let result = mux
            .request("textDocument/hover", Some(params))
            .await
            .unwrap();
        assert_eq!(result["result"]["contents"], "Nat");

        server.await.unwrap();
        mux.shutdown().await.unwrap();
    }

    // ── Test 16: Server request with string ID is handled ─────────

    #[tokio::test]
    async fn test_server_request_with_string_id_does_not_crash() {
        let (mut server_writer, client_reader) = make_duplex(4096);
        let (client_writer, server_reader) = make_duplex(4096);

        let mux = Multiplexer::new(client_reader, client_writer);

        // Server sends a request with a string ID (e.g., Lean's register_lean_watcher).
        let server = tokio::spawn(async move {
            let server_req = json!({
                "jsonrpc": "2.0",
                "id": "lean-watcher-1",
                "method": "client/registerCapability",
                "params": {"registrations": []}
            });
            write_message(&mut server_writer, &server_req)
                .await
                .unwrap();

            // Then read and respond to a normal client request.
            let mut reader = BufReader::new(server_reader);
            let req = read_message(&mut reader).await.unwrap();
            let id = req["id"].as_i64().unwrap();
            let resp = json!({"jsonrpc": "2.0", "id": id, "result": "ok"});
            write_message(&mut server_writer, &resp).await.unwrap();
        });

        // Give the reader time to process the string-ID request without crashing.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Normal request should still work after receiving a string-ID server request.
        let result = mux.request("test/method", None).await.unwrap();
        assert_eq!(result["result"], "ok");

        server.await.unwrap();
        mux.shutdown().await.unwrap();
    }

    // ── Test 17: Response with string ID dispatched correctly ─────

    #[tokio::test]
    async fn test_response_with_string_id_dispatched() {
        let (mut server_writer, client_reader) = make_duplex(4096);
        let (client_writer, _server_reader) = make_duplex(4096);

        let mux = Multiplexer::new(client_reader, client_writer);

        // Manually register a pending request with a string ID.
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = mux.pending.lock().await;
            pending.insert(RequestId::String("custom-id".to_string()), tx);
        }

        // Server sends a response with that string ID.
        let resp = json!({"jsonrpc": "2.0", "id": "custom-id", "result": {"data": "found"}});
        write_message(&mut server_writer, &resp).await.unwrap();

        // The pending request should receive the response.
        let result = tokio::time::timeout(Duration::from_secs(1), rx)
            .await
            .expect("should not timeout")
            .expect("channel should not be dropped");
        assert_eq!(result["result"]["data"], "found");

        mux.shutdown().await.unwrap();
    }
}
