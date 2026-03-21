//! Real LSP client implementation connecting the [`LspClient`] trait to a
//! [`Multiplexer`] for communication with a Lean 4 LSP server.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::sync::{mpsc, Mutex, Notify};
use tracing::debug;

use crate::client::{path_to_uri, LspClient, LspClientError};
use crate::error::TransportError;
use crate::multiplexer::{Multiplexer, MultiplexerError};

/// Default inactivity timeout for waiting on diagnostics (seconds).
const DEFAULT_DIAG_TIMEOUT_SECS: f64 = 5.0;

/// Grace period after signals agree, for Lean 4.22+ compatibility (ms).
const ELABORATION_GRACE_MS: u64 = 500;

/// Polling interval for the triple-signal loop (ms).
const POLL_INTERVAL_MS: u64 = 50;

/// Tracks the state of a file opened in the LSP server.
struct FileState {
    version: i32,
    content: String,
}

/// Tracks per-file elaboration progress from `$/lean/fileProgress` notifications
/// and `textDocument/waitForDiagnostics` RPC results.
#[derive(Debug, Clone)]
pub(crate) struct ElaborationState {
    /// `false` when the `fileProgress.processing` array is empty (elaboration done).
    pub processing: bool,
    /// Line ranges currently being processed: `(start_line, end_line)`.
    pub current_processing: Vec<(u32, u32)>,
    /// `true` if any processing item has `kind == 2` (fatal error).
    pub fatal_error: bool,
    /// `true` once `textDocument/waitForDiagnostics` RPC has returned.
    pub wait_for_diag_done: bool,
    /// Reset on any notification for this file.
    pub last_activity: Instant,
}

impl ElaborationState {
    fn new() -> Self {
        Self {
            processing: true,
            current_processing: Vec::new(),
            fatal_error: false,
            wait_for_diag_done: false,
            last_activity: Instant::now(),
        }
    }

    /// Returns `true` if no `current_processing` ranges intersect `[start, end]`.
    pub fn is_line_range_complete(&self, start: u32, end: u32) -> bool {
        !self
            .current_processing
            .iter()
            .any(|(s, e)| *s <= end && *e >= start)
    }
}

/// Concrete [`LspClient`] implementation backed by a [`Multiplexer`].
///
/// Communicates with a Lean 4 LSP server (e.g., `lake serve`) via JSON-RPC
/// over the provided async streams. Tracks open files and their version
/// numbers for incremental text synchronization.
pub struct LeanLspClient {
    project_path: PathBuf,
    multiplexer: Arc<Multiplexer>,
    open_files: Arc<Mutex<HashMap<String, FileState>>>,
    /// Diagnostics stored per file URI, updated by notification handler.
    diagnostics: Arc<std::sync::Mutex<HashMap<String, Vec<Value>>>>,
    /// Keeps the diagnostic channel open while the client is alive.
    #[allow(dead_code)]
    diag_tx: mpsc::UnboundedSender<String>,
    /// Receives URI updates when new diagnostics arrive.
    diag_rx: Arc<Mutex<mpsc::UnboundedReceiver<String>>>,
    /// Per-file elaboration state from `$/lean/fileProgress` notifications.
    elaboration_states: Arc<std::sync::Mutex<HashMap<String, ElaborationState>>>,
    /// Signaled when elaboration state changes (fileProgress or diagnostics).
    elaboration_notify: Arc<Notify>,
}

/// Convert a [`MultiplexerError`] to an [`LspClientError`].
fn mux_err(err: MultiplexerError) -> LspClientError {
    match err {
        MultiplexerError::Transport(t) => LspClientError::Transport(t),
        MultiplexerError::Timeout(d) => LspClientError::Timeout {
            operation: format!("request ({}ms)", d.as_millis()),
        },
        MultiplexerError::Shutdown | MultiplexerError::ChannelClosed => {
            LspClientError::Transport(TransportError::Closed)
        }
    }
}

/// Extract the `result` from a JSON-RPC response, or return an error if the
/// response contains an `error` field.
fn extract_result(response: &Value) -> Result<Value, LspClientError> {
    if let Some(error) = response.get("error") {
        return Err(LspClientError::LspError {
            code: error["code"].as_i64().unwrap_or(-1),
            message: error["message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_string(),
        });
    }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

impl LeanLspClient {
    /// Create a new client from async read/write streams.
    ///
    /// Immediately sends the LSP `initialize` request and `initialized`
    /// notification. The caller provides streams connected to a Lean 4
    /// LSP server (or a simulated server for testing).
    pub async fn new<R, W>(
        project_path: PathBuf,
        reader: R,
        writer: W,
    ) -> Result<Self, LspClientError>
    where
        R: AsyncBufRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let multiplexer = Arc::new(Multiplexer::new(reader, writer));
        let diagnostics: Arc<std::sync::Mutex<HashMap<String, Vec<Value>>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let (diag_tx, diag_rx) = mpsc::unbounded_channel();
        let elaboration_states: Arc<std::sync::Mutex<HashMap<String, ElaborationState>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let elaboration_notify = Arc::new(Notify::new());

        // Wire up the notification handler for diagnostics and fileProgress.
        let diag_store = Arc::clone(&diagnostics);
        let elab_states = Arc::clone(&elaboration_states);
        let elab_notify = Arc::clone(&elaboration_notify);
        let tx = diag_tx.clone();
        multiplexer
            .set_notification_handler(move |method, params| {
                if method == "textDocument/publishDiagnostics" {
                    if let Some(uri) = params.get("uri").and_then(|u| u.as_str()) {
                        let diag_list = match params.get("diagnostics") {
                            Some(Value::Array(arr)) => arr.clone(),
                            _ => vec![],
                        };
                        if let Ok(mut map) = diag_store.lock() {
                            map.insert(uri.to_string(), diag_list);
                        }
                        // Update last_activity for elaboration tracking.
                        if let Ok(mut states) = elab_states.lock() {
                            let state = states
                                .entry(uri.to_string())
                                .or_insert_with(ElaborationState::new);
                            state.last_activity = Instant::now();
                        }
                        elab_notify.notify_waiters();
                        let _ = tx.send(uri.to_string());
                    }
                } else if method == "$/lean/fileProgress" {
                    if let Some(uri) = params
                        .get("textDocument")
                        .and_then(|td| td.get("uri"))
                        .and_then(|u| u.as_str())
                    {
                        let processing_arr = params
                            .get("processing")
                            .and_then(|p| p.as_array())
                            .cloned()
                            .unwrap_or_default();

                        let mut ranges = Vec::new();
                        let mut has_fatal = false;

                        for item in &processing_arr {
                            // Extract line range from processing item.
                            let start_line = item
                                .get("range")
                                .and_then(|r| r.get("start"))
                                .and_then(|s| s.get("line"))
                                .and_then(|l| l.as_u64())
                                .unwrap_or(0) as u32;
                            let end_line = item
                                .get("range")
                                .and_then(|r| r.get("end"))
                                .and_then(|e| e.get("line"))
                                .and_then(|l| l.as_u64())
                                .unwrap_or(0) as u32;
                            ranges.push((start_line, end_line));

                            // kind == 2 indicates a fatal error.
                            if item.get("kind").and_then(|k| k.as_u64()) == Some(2) {
                                has_fatal = true;
                            }
                        }

                        if let Ok(mut states) = elab_states.lock() {
                            let state = states
                                .entry(uri.to_string())
                                .or_insert_with(ElaborationState::new);
                            state.processing = !processing_arr.is_empty();
                            state.current_processing = ranges;
                            if has_fatal {
                                state.fatal_error = true;
                            }
                            state.last_activity = Instant::now();
                        }
                        elab_notify.notify_waiters();
                    }
                }
            })
            .await;

        // Send `initialize`.
        let init_params = json!({
            "processId": std::process::id() as i64,
            "capabilities": {},
            "rootUri": format!("file://{}", project_path.display()),
            "rootPath": project_path.display().to_string(),
        });
        let resp = multiplexer
            .request("initialize", Some(init_params))
            .await
            .map_err(mux_err)?;
        extract_result(&resp)?;

        // Send `initialized`.
        multiplexer
            .notify("initialized", Some(json!({})))
            .await
            .map_err(mux_err)?;

        debug!("LSP client initialized for {}", project_path.display());

        Ok(Self {
            project_path,
            multiplexer,
            open_files: Arc::new(Mutex::new(HashMap::new())),
            diagnostics,
            diag_tx,
            diag_rx: Arc::new(Mutex::new(diag_rx)),
            elaboration_states,
            elaboration_notify,
        })
    }

    /// Send an LSP request and extract the result.
    async fn lsp_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, LspClientError> {
        let resp = self
            .multiplexer
            .request(method, params)
            .await
            .map_err(mux_err)?;
        extract_result(&resp)
    }

    /// Build `TextDocumentPositionParams` JSON.
    fn text_doc_pos(&self, relative_path: &str, line: u32, column: u32) -> Value {
        json!({
            "textDocument": {"uri": path_to_uri(&self.project_path, relative_path)},
            "position": {"line": line, "character": column}
        })
    }

    /// Send `textDocument/waitForDiagnostics` RPC and mark state when it returns.
    ///
    /// This is a blocking RPC that returns once the Lean server has finished
    /// computing diagnostics for the given file version. Can be called directly
    /// or the same logic is inlined in the `get_diagnostics` spawned task.
    #[allow(dead_code)]
    async fn send_wait_for_diagnostics(
        &self,
        uri: &str,
        version: i32,
    ) -> Result<(), LspClientError> {
        let params = json!({"uri": uri, "version": version});
        // Use a longer timeout for waitForDiagnostics since elaboration can take a while.
        let resp = self
            .multiplexer
            .request_with_timeout(
                "textDocument/waitForDiagnostics",
                Some(params),
                Duration::from_secs(300),
            )
            .await;
        // Mark state regardless of whether the RPC succeeded — if it errors
        // (e.g., method not found on older Lean), we still want the fallback
        // path to work.
        if let Ok(mut states) = self.elaboration_states.lock() {
            if let Some(state) = states.get_mut(uri) {
                state.wait_for_diag_done = true;
                state.last_activity = Instant::now();
            }
        }
        self.elaboration_notify.notify_waiters();
        match resp {
            Ok(_) => Ok(()),
            Err(MultiplexerError::Timeout(_)) => Err(LspClientError::Timeout {
                operation: "waitForDiagnostics".to_string(),
            }),
            // Method not found is expected on older Lean versions — treat as success.
            Err(_) => Ok(()),
        }
    }

    /// Reset elaboration state for a URI (called on open/update).
    fn reset_elaboration_state(&self, uri: &str) {
        if let Ok(mut states) = self.elaboration_states.lock() {
            states.insert(uri.to_string(), ElaborationState::new());
        }
    }
}

#[async_trait]
impl LspClient for LeanLspClient {
    fn project_path(&self) -> &Path {
        &self.project_path
    }

    async fn open_file(&self, relative_path: &str) -> Result<(), LspClientError> {
        let abs = self.project_path.join(relative_path);
        let disk_content = tokio::fs::read_to_string(&abs)
            .await
            .map_err(|e| LspClientError::Transport(TransportError::Io(e)))?;

        // Prepare notification under lock, then release before sending.
        let notification = {
            let mut files = self.open_files.lock().await;

            if let Some(state) = files.get_mut(relative_path) {
                if disk_content != state.content {
                    state.version += 1;
                    state.content = disk_content.clone();
                    Some((
                        "textDocument/didChange",
                        json!({
                            "textDocument": {
                                "uri": path_to_uri(&self.project_path, relative_path),
                                "version": state.version,
                            },
                            "contentChanges": [{"text": disk_content}],
                        }),
                    ))
                } else {
                    None // no change needed
                }
            } else {
                // First open
                files.insert(
                    relative_path.to_string(),
                    FileState {
                        version: 1,
                        content: disk_content.clone(),
                    },
                );
                Some((
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri": path_to_uri(&self.project_path, relative_path),
                            "languageId": "lean4",
                            "version": 1,
                            "text": disk_content,
                        }
                    }),
                ))
            }
        }; // lock released

        if let Some((method, params)) = notification {
            // Reset elaboration state when the file content changes.
            let uri = path_to_uri(&self.project_path, relative_path);
            self.reset_elaboration_state(&uri);
            self.multiplexer
                .notify(method, Some(params))
                .await
                .map_err(mux_err)?;
        }
        Ok(())
    }

    async fn open_file_force(&self, relative_path: &str) -> Result<(), LspClientError> {
        let close_params = {
            let mut files = self.open_files.lock().await;
            if files.remove(relative_path).is_some() {
                Some(json!({
                    "textDocument": {"uri": path_to_uri(&self.project_path, relative_path)}
                }))
            } else {
                None
            }
        }; // lock released

        if let Some(params) = close_params {
            self.multiplexer
                .notify("textDocument/didClose", Some(params))
                .await
                .map_err(mux_err)?;
        }
        self.open_file(relative_path).await
    }

    async fn get_file_content(&self, relative_path: &str) -> Result<String, LspClientError> {
        let files = self.open_files.lock().await;
        files
            .get(relative_path)
            .map(|f| f.content.clone())
            .ok_or_else(|| LspClientError::FileNotOpen(relative_path.to_string()))
    }

    async fn update_file(
        &self,
        relative_path: &str,
        changes: Vec<Value>,
    ) -> Result<(), LspClientError> {
        let params = {
            let mut files = self.open_files.lock().await;
            let state = files
                .get_mut(relative_path)
                .ok_or_else(|| LspClientError::FileNotOpen(relative_path.to_string()))?;
            state.version += 1;
            json!({
                "textDocument": {
                    "uri": path_to_uri(&self.project_path, relative_path),
                    "version": state.version,
                },
                "contentChanges": changes,
            })
        }; // lock released

        let uri = path_to_uri(&self.project_path, relative_path);
        self.reset_elaboration_state(&uri);
        self.multiplexer
            .notify("textDocument/didChange", Some(params))
            .await
            .map_err(mux_err)?;
        Ok(())
    }

    async fn update_file_content(
        &self,
        relative_path: &str,
        content: &str,
    ) -> Result<(), LspClientError> {
        let params = {
            let mut files = self.open_files.lock().await;
            let state = files
                .get_mut(relative_path)
                .ok_or_else(|| LspClientError::FileNotOpen(relative_path.to_string()))?;
            state.version += 1;
            state.content = content.to_string();
            json!({
                "textDocument": {
                    "uri": path_to_uri(&self.project_path, relative_path),
                    "version": state.version,
                },
                "contentChanges": [{"text": content}],
            })
        }; // lock released

        let uri = path_to_uri(&self.project_path, relative_path);
        self.reset_elaboration_state(&uri);
        self.multiplexer
            .notify("textDocument/didChange", Some(params))
            .await
            .map_err(mux_err)?;
        Ok(())
    }

    async fn close_files(&self, paths: &[String]) -> Result<(), LspClientError> {
        // Collect notifications under lock, then send after releasing.
        let notifications = {
            let mut files = self.open_files.lock().await;
            let mut notifs = Vec::new();
            for path in paths {
                if files.remove(path).is_some() {
                    notifs.push(json!({
                        "textDocument": {"uri": path_to_uri(&self.project_path, path)}
                    }));
                }
            }
            notifs
        }; // lock released

        for params in notifications {
            self.multiplexer
                .notify("textDocument/didClose", Some(params))
                .await
                .map_err(mux_err)?;
        }
        Ok(())
    }

    async fn get_diagnostics(
        &self,
        relative_path: &str,
        start_line: Option<u32>,
        end_line: Option<u32>,
        inactivity_timeout: Option<f64>,
    ) -> Result<Value, LspClientError> {
        let uri = path_to_uri(&self.project_path, relative_path);
        let inactivity_dur =
            Duration::from_secs_f64(inactivity_timeout.unwrap_or(DEFAULT_DIAG_TIMEOUT_SECS));
        // Max overall timeout: 10x inactivity or 60s, whichever is larger.
        let max_timeout = std::cmp::max(inactivity_dur * 10, Duration::from_secs(60));
        let grace_period = Duration::from_millis(ELABORATION_GRACE_MS);
        let poll_interval = Duration::from_millis(POLL_INTERVAL_MS);

        // Get the file version for waitForDiagnostics.
        let file_version = {
            let files = self.open_files.lock().await;
            files.get(relative_path).map(|f| f.version).unwrap_or(1)
        };

        // Spawn waitForDiagnostics RPC in background.
        let wfd_uri = uri.clone();
        let elab_states = Arc::clone(&self.elaboration_states);
        let elab_notify = Arc::clone(&self.elaboration_notify);
        let multiplexer = Arc::clone(&self.multiplexer);
        tokio::spawn(async move {
            let params = json!({"uri": wfd_uri, "version": file_version});
            let resp = multiplexer
                .request_with_timeout(
                    "textDocument/waitForDiagnostics",
                    Some(params),
                    Duration::from_secs(300),
                )
                .await;
            // Mark done regardless of success/failure.
            if let Ok(mut states) = elab_states.lock() {
                if let Some(state) = states.get_mut(&wfd_uri) {
                    state.wait_for_diag_done = true;
                    state.last_activity = Instant::now();
                }
            }
            elab_notify.notify_waiters();
            drop(resp);
        });

        // Triple-signal polling loop.
        let loop_start = Instant::now();
        let mut grace_start: Option<Instant> = None;

        loop {
            // Check max timeout.
            if loop_start.elapsed() >= max_timeout {
                debug!(uri = %uri, "get_diagnostics: max timeout reached");
                break;
            }

            // Read elaboration state.
            let (processing, fatal_error, wait_done, last_activity, range_complete) = {
                let states = self.elaboration_states.lock().unwrap();
                if let Some(state) = states.get(&uri) {
                    let range_done = if let (Some(s), Some(e)) = (start_line, end_line) {
                        state.is_line_range_complete(s, e)
                    } else {
                        false
                    };
                    (
                        state.processing,
                        state.fatal_error,
                        state.wait_for_diag_done,
                        state.last_activity,
                        range_done,
                    )
                } else {
                    // No elaboration state yet — keep waiting.
                    (true, false, false, Instant::now(), false)
                }
            };

            // Signal (a): both fileProgress and waitForDiagnostics agree elaboration is done.
            if !processing && wait_done {
                match grace_start {
                    None => {
                        grace_start = Some(Instant::now());
                    }
                    Some(gs) if gs.elapsed() >= grace_period => {
                        debug!(uri = %uri, "get_diagnostics: triple-signal complete");
                        break;
                    }
                    _ => {}
                }
            }
            // Signal (b): fatal error — done immediately.
            else if fatal_error {
                debug!(uri = %uri, "get_diagnostics: fatal error detected");
                break;
            }
            // Signal (c): partial range complete (when start_line/end_line specified).
            else if range_complete && (wait_done || !processing) {
                match grace_start {
                    None => {
                        grace_start = Some(Instant::now());
                    }
                    Some(gs) if gs.elapsed() >= grace_period => {
                        debug!(uri = %uri, "get_diagnostics: line range complete");
                        break;
                    }
                    _ => {}
                }
            }
            // Signal (d): inactivity timeout — pure fallback, fires when no
            // notifications have arrived for the full inactivity duration.
            // This preserves backward compatibility with the old timeout-only
            // approach and handles servers that don't support fileProgress.
            else if last_activity.elapsed() >= inactivity_dur {
                debug!(uri = %uri, "get_diagnostics: inactivity timeout fallback");
                break;
            }
            // No signals ready yet — reset grace if conditions no longer hold.
            else {
                grace_start = None;
            }

            // Wait for a signal or poll interval, whichever comes first.
            tokio::select! {
                _ = self.elaboration_notify.notified() => {}
                _ = tokio::time::sleep(poll_interval) => {}
            }
        }

        // Also drain any pending diagnostic notifications to get latest state.
        {
            let mut rx = self.diag_rx.lock().await;
            while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(10), rx.recv()).await
            {
            }
        }

        let all_diags = {
            let map = self.diagnostics.lock().unwrap();
            map.get(&uri).cloned().unwrap_or_default()
        };

        let filtered: Vec<Value> = if start_line.is_some() || end_line.is_some() {
            let start = start_line.unwrap_or(0);
            let end = end_line.unwrap_or(u32::MAX);
            all_diags
                .into_iter()
                .filter(|d| {
                    d.get("range")
                        .and_then(|r| r.get("start"))
                        .and_then(|s| s.get("line"))
                        .and_then(|l| l.as_u64())
                        .map(|l| (l as u32) >= start && (l as u32) <= end)
                        .unwrap_or(true)
                })
                .collect()
        } else {
            all_diags
        };
        Ok(json!(filtered))
    }

    async fn get_interactive_diagnostics(
        &self,
        relative_path: &str,
        start_line: Option<u32>,
        end_line: Option<u32>,
    ) -> Result<Vec<Value>, LspClientError> {
        let uri = path_to_uri(&self.project_path, relative_path);
        let params = json!({"textDocument": {"uri": uri}});
        let result = self
            .lsp_request("$/lean/interactiveDiagnostics", Some(params))
            .await?;

        let items = match result {
            Value::Array(arr) => arr,
            _ => vec![],
        };

        if start_line.is_some() || end_line.is_some() {
            let start = start_line.unwrap_or(0);
            let end = end_line.unwrap_or(u32::MAX);
            Ok(items
                .into_iter()
                .filter(|d| {
                    d.get("range")
                        .and_then(|r| r.get("start"))
                        .and_then(|s| s.get("line"))
                        .and_then(|l| l.as_u64())
                        .map(|l| (l as u32) >= start && (l as u32) <= end)
                        .unwrap_or(true)
                })
                .collect())
        } else {
            Ok(items)
        }
    }

    async fn get_goal(
        &self,
        relative_path: &str,
        line: u32,
        column: u32,
    ) -> Result<Option<Value>, LspClientError> {
        let params = self.text_doc_pos(relative_path, line, column);
        let result = self.lsp_request("$/lean/plainGoal", Some(params)).await?;
        Ok(if result.is_null() { None } else { Some(result) })
    }

    async fn get_term_goal(
        &self,
        relative_path: &str,
        line: u32,
        column: u32,
    ) -> Result<Option<Value>, LspClientError> {
        let params = self.text_doc_pos(relative_path, line, column);
        let result = self
            .lsp_request("$/lean/plainTermGoal", Some(params))
            .await?;
        Ok(if result.is_null() { None } else { Some(result) })
    }

    async fn get_hover(
        &self,
        relative_path: &str,
        line: u32,
        column: u32,
    ) -> Result<Option<Value>, LspClientError> {
        let params = self.text_doc_pos(relative_path, line, column);
        let result = self.lsp_request("textDocument/hover", Some(params)).await?;
        Ok(if result.is_null() { None } else { Some(result) })
    }

    async fn get_completions(
        &self,
        relative_path: &str,
        line: u32,
        column: u32,
    ) -> Result<Vec<Value>, LspClientError> {
        let params = self.text_doc_pos(relative_path, line, column);
        let result = self
            .lsp_request("textDocument/completion", Some(params))
            .await?;
        match result {
            Value::Array(items) => Ok(items),
            Value::Object(ref map) => Ok(map
                .get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()),
            _ => Ok(vec![]),
        }
    }

    async fn get_declarations(
        &self,
        relative_path: &str,
        line: u32,
        column: u32,
    ) -> Result<Vec<Value>, LspClientError> {
        let params = self.text_doc_pos(relative_path, line, column);
        let result = self
            .lsp_request("textDocument/definition", Some(params))
            .await?;
        match result {
            Value::Array(items) => Ok(items),
            Value::Null => Ok(vec![]),
            single => Ok(vec![single]),
        }
    }

    async fn get_references(
        &self,
        relative_path: &str,
        line: u32,
        column: u32,
        include_declaration: bool,
    ) -> Result<Vec<Value>, LspClientError> {
        let uri = path_to_uri(&self.project_path, relative_path);
        let params = json!({
            "textDocument": {"uri": uri},
            "position": {"line": line, "character": column},
            "context": {"includeDeclaration": include_declaration}
        });
        let result = self
            .lsp_request("textDocument/references", Some(params))
            .await?;
        match result {
            Value::Array(items) => Ok(items),
            _ => Ok(vec![]),
        }
    }

    async fn get_document_symbols(
        &self,
        relative_path: &str,
    ) -> Result<Vec<Value>, LspClientError> {
        let uri = path_to_uri(&self.project_path, relative_path);
        let params = json!({"textDocument": {"uri": uri}});
        let result = self
            .lsp_request("textDocument/documentSymbol", Some(params))
            .await?;
        match result {
            Value::Array(items) => Ok(items),
            _ => Ok(vec![]),
        }
    }

    async fn get_code_actions(
        &self,
        relative_path: &str,
        start_line: u32,
        start_col: u32,
        end_line: u32,
        end_col: u32,
    ) -> Result<Vec<Value>, LspClientError> {
        let uri = path_to_uri(&self.project_path, relative_path);
        let params = json!({
            "textDocument": {"uri": uri},
            "range": {
                "start": {"line": start_line, "character": start_col},
                "end": {"line": end_line, "character": end_col}
            },
            "context": {"diagnostics": []}
        });
        let result = self
            .lsp_request("textDocument/codeAction", Some(params))
            .await?;
        match result {
            Value::Array(items) => Ok(items),
            _ => Ok(vec![]),
        }
    }

    async fn get_code_action_resolve(&self, action: Value) -> Result<Value, LspClientError> {
        self.lsp_request("codeAction/resolve", Some(action)).await
    }

    async fn get_widgets(
        &self,
        relative_path: &str,
        line: u32,
        column: u32,
    ) -> Result<Vec<Value>, LspClientError> {
        let uri = path_to_uri(&self.project_path, relative_path);
        let params = json!({
            "method": "Lean.Widget.getWidgets",
            "params": {
                "textDocument": {"uri": uri},
                "position": {"line": line, "character": column}
            }
        });
        let result = self.lsp_request("$/lean/rpc/call", Some(params)).await?;
        match result {
            Value::Array(items) => Ok(items),
            Value::Object(ref map) if map.contains_key("widgets") => {
                Ok(map["widgets"].as_array().cloned().unwrap_or_default())
            }
            _ => Ok(vec![]),
        }
    }

    async fn get_widget_source(
        &self,
        relative_path: &str,
        line: u32,
        column: u32,
        javascript_hash: &str,
    ) -> Result<Value, LspClientError> {
        let uri = path_to_uri(&self.project_path, relative_path);
        let params = json!({
            "method": "Lean.Widget.getWidgetSource",
            "params": {
                "position": {"line": line, "character": column},
                "textDocument": {"uri": uri},
                "javascriptHash": javascript_hash,
            }
        });
        self.lsp_request("$/lean/rpc/call", Some(params)).await
    }

    async fn shutdown(&self) -> Result<(), LspClientError> {
        // Send LSP shutdown request.
        let resp = self
            .multiplexer
            .request("shutdown", None)
            .await
            .map_err(mux_err)?;
        extract_result(&resp)?;

        // Send exit notification.
        self.multiplexer
            .notify("exit", None)
            .await
            .map_err(mux_err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{read_message, write_message};
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::TempDir;
    use tokio::io::BufReader;

    type ServerReader = BufReader<tokio::io::DuplexStream>;
    type ServerWriter = tokio::io::DuplexStream;

    /// Set up a `LeanLspClient` with a simulated LSP server that handles init.
    async fn setup() -> (LeanLspClient, ServerReader, ServerWriter, TempDir) {
        let dir = TempDir::new().unwrap();
        let project = dir.path().to_path_buf();

        let (mut sw, cr) = tokio::io::duplex(16384);
        let (cw, sr) = tokio::io::duplex(16384);
        let mut sr = BufReader::new(sr);
        let cr = BufReader::new(cr);

        let (client, (sr, sw)) = tokio::join!(
            async { LeanLspClient::new(project, cr, cw).await.unwrap() },
            async move {
                let req = read_message(&mut sr).await.unwrap();
                assert_eq!(req["method"], "initialize");
                let id = req["id"].as_i64().unwrap();
                write_message(
                    &mut sw,
                    &json!({"jsonrpc":"2.0","id":id,"result":{"capabilities":{}}}),
                )
                .await
                .unwrap();
                let notif = read_message(&mut sr).await.unwrap();
                assert_eq!(notif["method"], "initialized");
                (sr, sw)
            }
        );

        (client, sr, sw, dir)
    }

    /// Create a file on disk and open it, draining the didOpen notification.
    async fn open_test_file(
        client: &LeanLspClient,
        sr: &mut ServerReader,
        dir: &TempDir,
        name: &str,
        content: &str,
    ) {
        std::fs::write(dir.path().join(name), content).unwrap();
        client.open_file(name).await.unwrap();
        let _ = read_message(sr).await.unwrap();
    }

    /// Read the next request from the server side and respond with the given result.
    async fn respond_next(sr: &mut ServerReader, sw: &mut ServerWriter, result: Value) -> Value {
        let req = read_message(sr).await.unwrap();
        let id = req["id"].as_i64().unwrap();
        write_message(sw, &json!({"jsonrpc":"2.0","id":id,"result":result}))
            .await
            .unwrap();
        req
    }

    // ── Basic ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_project_path() {
        let (client, _sr, _sw, dir) = setup().await;
        assert_eq!(client.project_path(), dir.path());
    }

    #[tokio::test]
    async fn test_lean_lsp_client_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LeanLspClient>();
    }

    // ── File operations ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_open_file_sends_did_open() {
        let (client, mut sr, _sw, dir) = setup().await;
        std::fs::write(dir.path().join("Test.lean"), "-- hello").unwrap();

        client.open_file("Test.lean").await.unwrap();

        let msg = read_message(&mut sr).await.unwrap();
        assert_eq!(msg["method"], "textDocument/didOpen");
        assert_eq!(msg["params"]["textDocument"]["languageId"], "lean4");
        assert_eq!(msg["params"]["textDocument"]["version"], 1);
        assert_eq!(msg["params"]["textDocument"]["text"], "-- hello");
    }

    #[tokio::test]
    async fn test_open_file_is_idempotent() {
        let (client, mut sr, _sw, dir) = setup().await;
        std::fs::write(dir.path().join("Test.lean"), "content").unwrap();

        client.open_file("Test.lean").await.unwrap();
        let _ = read_message(&mut sr).await.unwrap();

        // Second open should be a no-op.
        client.open_file("Test.lean").await.unwrap();
        assert_eq!(
            client.get_file_content("Test.lean").await.unwrap(),
            "content"
        );
    }

    #[tokio::test]
    async fn test_open_file_force_reopens() {
        let (client, mut sr, _sw, dir) = setup().await;
        std::fs::write(dir.path().join("Test.lean"), "v1").unwrap();

        client.open_file("Test.lean").await.unwrap();
        let _ = read_message(&mut sr).await.unwrap();

        std::fs::write(dir.path().join("Test.lean"), "v2").unwrap();
        client.open_file_force("Test.lean").await.unwrap();

        let close_msg = read_message(&mut sr).await.unwrap();
        assert_eq!(close_msg["method"], "textDocument/didClose");

        let open_msg = read_message(&mut sr).await.unwrap();
        assert_eq!(open_msg["method"], "textDocument/didOpen");
        assert_eq!(open_msg["params"]["textDocument"]["text"], "v2");
        assert_eq!(open_msg["params"]["textDocument"]["version"], 1);
    }

    #[tokio::test]
    async fn test_get_file_content_returns_cached() {
        let (client, mut sr, _sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "A.lean", "cached text").await;

        assert_eq!(
            client.get_file_content("A.lean").await.unwrap(),
            "cached text"
        );
    }

    #[tokio::test]
    async fn test_get_file_content_not_open() {
        let (client, _sr, _sw, _dir) = setup().await;
        let err = client.get_file_content("Nope.lean").await.unwrap_err();
        assert!(matches!(err, LspClientError::FileNotOpen(_)));
    }

    #[tokio::test]
    async fn test_update_file_sends_did_change() {
        let (client, mut sr, _sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "U.lean", "old").await;

        let changes = vec![json!({"text": "new", "range": {
            "start": {"line": 0, "character": 0},
            "end": {"line": 0, "character": 3}
        }})];
        client.update_file("U.lean", changes).await.unwrap();

        let msg = read_message(&mut sr).await.unwrap();
        assert_eq!(msg["method"], "textDocument/didChange");
        assert_eq!(msg["params"]["textDocument"]["version"], 2);
        assert_eq!(msg["params"]["contentChanges"][0]["text"], "new");
    }

    #[tokio::test]
    async fn test_update_file_not_open() {
        let (client, _sr, _sw, _dir) = setup().await;
        let err = client
            .update_file("X.lean", vec![json!({"text": "x"})])
            .await
            .unwrap_err();
        assert!(matches!(err, LspClientError::FileNotOpen(_)));
    }

    #[tokio::test]
    async fn test_update_file_content_replaces_content() {
        let (client, mut sr, _sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "R.lean", "before").await;

        client.update_file_content("R.lean", "after").await.unwrap();

        let msg = read_message(&mut sr).await.unwrap();
        assert_eq!(msg["params"]["contentChanges"][0]["text"], "after");
        assert_eq!(msg["params"]["textDocument"]["version"], 2);

        assert_eq!(client.get_file_content("R.lean").await.unwrap(), "after");
    }

    #[tokio::test]
    async fn test_close_files_sends_did_close() {
        let (client, mut sr, _sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "C.lean", "x").await;

        client.close_files(&["C.lean".to_string()]).await.unwrap();

        let msg = read_message(&mut sr).await.unwrap();
        assert_eq!(msg["method"], "textDocument/didClose");

        assert!(client.get_file_content("C.lean").await.is_err());
    }

    #[tokio::test]
    async fn test_version_increments_on_updates() {
        let (client, mut sr, _sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "V.lean", "v1").await;

        client.update_file_content("V.lean", "v2").await.unwrap();
        let msg1 = read_message(&mut sr).await.unwrap();
        assert_eq!(msg1["params"]["textDocument"]["version"], 2);

        client.update_file_content("V.lean", "v3").await.unwrap();
        let msg2 = read_message(&mut sr).await.unwrap();
        assert_eq!(msg2["params"]["textDocument"]["version"], 3);
    }

    // ── LSP requests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_get_goal_returns_result() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "G.lean", "by trivial").await;

        let expected = json!({"goals": ["⊢ True"]});
        let (result, req) = tokio::join!(
            client.get_goal("G.lean", 0, 3),
            respond_next(&mut sr, &mut sw, expected.clone())
        );
        assert_eq!(req["method"], "$/lean/plainGoal");
        assert_eq!(result.unwrap(), Some(expected));
    }

    #[tokio::test]
    async fn test_get_goal_returns_none_for_null() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "N.lean", "-- x").await;

        let (result, _) = tokio::join!(
            client.get_goal("N.lean", 0, 0),
            respond_next(&mut sr, &mut sw, Value::Null)
        );
        assert_eq!(result.unwrap(), None);
    }

    #[tokio::test]
    async fn test_get_term_goal_returns_result() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "T.lean", "x").await;

        let expected = json!({"goal": "Nat"});
        let (result, req) = tokio::join!(
            client.get_term_goal("T.lean", 0, 0),
            respond_next(&mut sr, &mut sw, expected.clone())
        );
        assert_eq!(req["method"], "$/lean/plainTermGoal");
        assert_eq!(result.unwrap(), Some(expected));
    }

    #[tokio::test]
    async fn test_get_hover_returns_result() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "H.lean", "def x := 1").await;

        let expected = json!({"contents": {"kind": "markdown", "value": "Nat"}});
        let (result, req) = tokio::join!(
            client.get_hover("H.lean", 0, 4),
            respond_next(&mut sr, &mut sw, expected.clone())
        );
        assert_eq!(req["method"], "textDocument/hover");
        assert_eq!(result.unwrap(), Some(expected));
    }

    #[tokio::test]
    async fn test_get_completions_returns_items() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "Co.lean", "Na").await;

        let resp =
            json!({"isIncomplete": false, "items": [{"label": "Nat"}, {"label": "Nat.add"}]});
        let (result, req) = tokio::join!(
            client.get_completions("Co.lean", 0, 2),
            respond_next(&mut sr, &mut sw, resp)
        );
        assert_eq!(req["method"], "textDocument/completion");
        let items = result.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["label"], "Nat");
    }

    #[tokio::test]
    async fn test_get_declarations_returns_locations() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "D.lean", "#check Nat").await;

        let locs = json!([{"uri": "file:///D.lean", "range": {
            "start": {"line": 0, "character": 7},
            "end": {"line": 0, "character": 10}
        }}]);
        let (result, req) = tokio::join!(
            client.get_declarations("D.lean", 0, 7),
            respond_next(&mut sr, &mut sw, locs)
        );
        assert_eq!(req["method"], "textDocument/definition");
        assert_eq!(result.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_get_references_includes_context() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "Ref.lean", "def x := 1").await;

        let refs = json!([{"uri":"file:///Ref.lean","range":{"start":{"line":0,"character":4},"end":{"line":0,"character":5}}}]);
        let (result, req) = tokio::join!(
            client.get_references("Ref.lean", 0, 4, true),
            respond_next(&mut sr, &mut sw, refs)
        );
        assert_eq!(req["method"], "textDocument/references");
        assert_eq!(req["params"]["context"]["includeDeclaration"], true);
        assert_eq!(result.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_get_document_symbols() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "Sym.lean", "def foo := 1").await;

        let syms = json!([{"name":"foo","kind":12,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":12}}}]);
        let (result, req) = tokio::join!(
            client.get_document_symbols("Sym.lean"),
            respond_next(&mut sr, &mut sw, syms)
        );
        assert_eq!(req["method"], "textDocument/documentSymbol");
        assert_eq!(result.unwrap()[0]["name"], "foo");
    }

    #[tokio::test]
    async fn test_get_code_actions() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "CA.lean", "x").await;

        let actions = json!([{"title":"Add import","kind":"quickfix"}]);
        let (result, req) = tokio::join!(
            client.get_code_actions("CA.lean", 0, 0, 0, 1),
            respond_next(&mut sr, &mut sw, actions)
        );
        assert_eq!(req["method"], "textDocument/codeAction");
        assert_eq!(result.unwrap()[0]["title"], "Add import");
    }

    #[tokio::test]
    async fn test_get_code_action_resolve() {
        let (client, mut sr, mut sw, _dir) = setup().await;

        let action = json!({"title":"Import","kind":"quickfix"});
        let resolved = json!({"title":"Import","kind":"quickfix","edit":{"changes":{}}});
        let (result, req) = tokio::join!(
            client.get_code_action_resolve(action),
            respond_next(&mut sr, &mut sw, resolved.clone())
        );
        assert_eq!(req["method"], "codeAction/resolve");
        assert_eq!(result.unwrap(), resolved);
    }

    // ── Diagnostics ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_get_diagnostics_waits_for_notifications() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "Diag.lean", "bad").await;

        let uri = path_to_uri(client.project_path(), "Diag.lean");
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "diagnostics": [{
                    "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
                    "message": "unknown identifier",
                    "severity": 1
                }]
            }
        });
        write_message(&mut sw, &notif).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let result = client
            .get_diagnostics("Diag.lean", None, None, Some(0.1))
            .await
            .unwrap();
        let diags = result.as_array().unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0]["message"], "unknown identifier");
    }

    #[tokio::test]
    async fn test_get_diagnostics_filters_by_line() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "F.lean", "a\nb\nc").await;

        let uri = path_to_uri(client.project_path(), "F.lean");
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "diagnostics": [
                    {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"message":"err0"},
                    {"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":1}},"message":"err1"},
                    {"range":{"start":{"line":2,"character":0},"end":{"line":2,"character":1}},"message":"err2"},
                ]
            }
        });
        write_message(&mut sw, &notif).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let result = client
            .get_diagnostics("F.lean", Some(1), Some(1), Some(0.1))
            .await
            .unwrap();
        let diags = result.as_array().unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0]["message"], "err1");
    }

    // ── Shutdown ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_shutdown_sends_protocol_messages() {
        let (client, mut sr, mut sw, _dir) = setup().await;

        let (result, _) = tokio::join!(client.shutdown(), async {
            let req = read_message(&mut sr).await.unwrap();
            assert_eq!(req["method"], "shutdown");
            let id = req["id"].as_i64().unwrap();
            write_message(&mut sw, &json!({"jsonrpc":"2.0","id":id,"result":null}))
                .await
                .unwrap();
            let notif = read_message(&mut sr).await.unwrap();
            assert_eq!(notif["method"], "exit");
        });
        assert!(result.is_ok());
    }

    // ── Error handling ────────────────────────────────────────────────

    // ── open_file stale content detection (#106) ──────────────────

    #[tokio::test]
    async fn regression_open_file_detects_disk_change() {
        let (client, mut sr, _sw, dir) = setup().await;

        // First open: sends didOpen with "version 1"
        std::fs::write(dir.path().join("S.lean"), "version 1").unwrap();
        client.open_file("S.lean").await.unwrap();
        let did_open = read_message(&mut sr).await.unwrap();
        assert_eq!(did_open["method"], "textDocument/didOpen");
        assert_eq!(did_open["params"]["textDocument"]["text"], "version 1");

        // External change on disk
        std::fs::write(dir.path().join("S.lean"), "version 2").unwrap();

        // Second open: should detect change and send didChange
        client.open_file("S.lean").await.unwrap();
        let did_change = read_message(&mut sr).await.unwrap();
        assert_eq!(did_change["method"], "textDocument/didChange");
        assert_eq!(
            did_change["params"]["contentChanges"][0]["text"],
            "version 2"
        );
        assert_eq!(did_change["params"]["textDocument"]["version"], 2);
    }

    #[tokio::test]
    async fn open_file_unchanged_content_no_notification() {
        let (client, mut sr, _sw, dir) = setup().await;

        std::fs::write(dir.path().join("NoChange.lean"), "same").unwrap();
        client.open_file("NoChange.lean").await.unwrap();
        let _ = read_message(&mut sr).await.unwrap(); // drain didOpen

        // Re-open without changing disk content — no notification expected
        client.open_file("NoChange.lean").await.unwrap();

        // Verify cached content is still correct (no stale state)
        assert_eq!(
            client.get_file_content("NoChange.lean").await.unwrap(),
            "same"
        );

        // If a didChange was sent, the server side would have a message
        // waiting. We verify there's nothing by checking via a timeout.
        let read_result =
            tokio::time::timeout(Duration::from_millis(100), read_message(&mut sr)).await;
        assert!(
            read_result.is_err(),
            "Expected no message but got one: {:?}",
            read_result
        );
    }

    #[tokio::test]
    async fn open_file_disk_change_updates_cached_content() {
        let (client, mut sr, _sw, dir) = setup().await;

        std::fs::write(dir.path().join("Cache.lean"), "old").unwrap();
        client.open_file("Cache.lean").await.unwrap();
        let _ = read_message(&mut sr).await.unwrap(); // drain didOpen

        // Change disk content
        std::fs::write(dir.path().join("Cache.lean"), "new").unwrap();

        // Re-open should update the cached content
        client.open_file("Cache.lean").await.unwrap();
        let _ = read_message(&mut sr).await.unwrap(); // drain didChange

        // get_file_content reads from cache — should return "new"
        assert_eq!(client.get_file_content("Cache.lean").await.unwrap(), "new");
    }

    #[tokio::test]
    async fn open_file_disk_change_increments_version() {
        let (client, mut sr, _sw, dir) = setup().await;

        std::fs::write(dir.path().join("Ver.lean"), "v1").unwrap();
        client.open_file("Ver.lean").await.unwrap();
        let did_open = read_message(&mut sr).await.unwrap();
        assert_eq!(did_open["params"]["textDocument"]["version"], 1);

        // First disk change → version 2
        std::fs::write(dir.path().join("Ver.lean"), "v2").unwrap();
        client.open_file("Ver.lean").await.unwrap();
        let msg1 = read_message(&mut sr).await.unwrap();
        assert_eq!(msg1["method"], "textDocument/didChange");
        assert_eq!(msg1["params"]["textDocument"]["version"], 2);

        // Second disk change → version 3
        std::fs::write(dir.path().join("Ver.lean"), "v3").unwrap();
        client.open_file("Ver.lean").await.unwrap();
        let msg2 = read_message(&mut sr).await.unwrap();
        assert_eq!(msg2["method"], "textDocument/didChange");
        assert_eq!(msg2["params"]["textDocument"]["version"], 3);
    }

    // ── Error handling ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_lsp_error_response() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "E.lean", "x").await;

        let (result, _) = tokio::join!(client.get_hover("E.lean", 0, 0), async {
            let req = read_message(&mut sr).await.unwrap();
            let id = req["id"].as_i64().unwrap();
            write_message(
                &mut sw,
                &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}}),
            )
            .await
            .unwrap();
        });

        let err = result.unwrap_err();
        match err {
            LspClientError::LspError { code, message } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "Method not found");
            }
            other => panic!("Expected LspError, got: {other:?}"),
        }
    }

    // ── ElaborationState unit tests ────────────────────────────────

    #[test]
    fn elaboration_state_new_defaults() {
        let state = ElaborationState::new();
        assert!(state.processing);
        assert!(state.current_processing.is_empty());
        assert!(!state.fatal_error);
        assert!(!state.wait_for_diag_done);
    }

    #[test]
    fn is_line_range_complete_no_processing() {
        let state = ElaborationState {
            processing: false,
            current_processing: vec![],
            fatal_error: false,
            wait_for_diag_done: false,
            last_activity: Instant::now(),
        };
        assert!(state.is_line_range_complete(0, 100));
        assert!(state.is_line_range_complete(50, 50));
    }

    #[test]
    fn is_line_range_complete_no_overlap() {
        let state = ElaborationState {
            processing: true,
            current_processing: vec![(10, 20), (50, 60)],
            fatal_error: false,
            wait_for_diag_done: false,
            last_activity: Instant::now(),
        };
        // Range 0-9 does not overlap with (10,20) or (50,60).
        assert!(state.is_line_range_complete(0, 9));
        // Range 25-45 does not overlap.
        assert!(state.is_line_range_complete(25, 45));
        // Range 65-100 does not overlap.
        assert!(state.is_line_range_complete(65, 100));
    }

    #[test]
    fn is_line_range_complete_with_overlap() {
        let state = ElaborationState {
            processing: true,
            current_processing: vec![(10, 20), (50, 60)],
            fatal_error: false,
            wait_for_diag_done: false,
            last_activity: Instant::now(),
        };
        // Range 5-15 overlaps with (10,20).
        assert!(!state.is_line_range_complete(5, 15));
        // Range 15-55 overlaps with both.
        assert!(!state.is_line_range_complete(15, 55));
        // Range 55-65 overlaps with (50,60).
        assert!(!state.is_line_range_complete(55, 65));
        // Exact match.
        assert!(!state.is_line_range_complete(10, 20));
        // Single line inside range.
        assert!(!state.is_line_range_complete(15, 15));
    }

    #[test]
    fn is_line_range_complete_boundary_cases() {
        let state = ElaborationState {
            processing: true,
            current_processing: vec![(10, 20)],
            fatal_error: false,
            wait_for_diag_done: false,
            last_activity: Instant::now(),
        };
        // Touching at boundary: range ends at 10, processing starts at 10.
        assert!(!state.is_line_range_complete(5, 10));
        // Touching at other boundary.
        assert!(!state.is_line_range_complete(20, 25));
        // Just outside.
        assert!(state.is_line_range_complete(21, 25));
        assert!(state.is_line_range_complete(0, 9));
    }

    // ── fileProgress notification handler tests ─────────────────────

    #[tokio::test]
    async fn file_progress_updates_elaboration_state() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "FP.lean", "x").await;

        let uri = path_to_uri(client.project_path(), "FP.lean");

        // Send a fileProgress notification with processing ranges.
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "$/lean/fileProgress",
            "params": {
                "textDocument": {"uri": uri},
                "processing": [
                    {
                        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 5, "character": 0}},
                        "kind": 1
                    },
                    {
                        "range": {"start": {"line": 10, "character": 0}, "end": {"line": 15, "character": 0}},
                        "kind": 1
                    }
                ]
            }
        });
        write_message(&mut sw, &notif).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let states = client.elaboration_states.lock().unwrap();
        let state = states.get(&uri).unwrap();
        assert!(state.processing);
        assert_eq!(state.current_processing.len(), 2);
        assert_eq!(state.current_processing[0], (0, 5));
        assert_eq!(state.current_processing[1], (10, 15));
        assert!(!state.fatal_error);
    }

    #[tokio::test]
    async fn file_progress_empty_processing_sets_not_processing() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "EP.lean", "x").await;

        let uri = path_to_uri(client.project_path(), "EP.lean");

        // Send fileProgress with empty processing array.
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "$/lean/fileProgress",
            "params": {
                "textDocument": {"uri": uri},
                "processing": []
            }
        });
        write_message(&mut sw, &notif).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let states = client.elaboration_states.lock().unwrap();
        let state = states.get(&uri).unwrap();
        assert!(!state.processing);
        assert!(state.current_processing.is_empty());
    }

    #[tokio::test]
    async fn file_progress_kind_2_sets_fatal_error() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "FE.lean", "x").await;

        let uri = path_to_uri(client.project_path(), "FE.lean");

        // Send fileProgress with kind == 2 (fatal error).
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "$/lean/fileProgress",
            "params": {
                "textDocument": {"uri": uri},
                "processing": [
                    {
                        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 5, "character": 0}},
                        "kind": 2
                    }
                ]
            }
        });
        write_message(&mut sw, &notif).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let states = client.elaboration_states.lock().unwrap();
        let state = states.get(&uri).unwrap();
        assert!(state.fatal_error);
    }

    // ── Triple-signal get_diagnostics tests ─────────────────────────

    #[tokio::test]
    async fn get_diagnostics_triple_signal_fast_completion() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "TS.lean", "x").await;

        let uri = path_to_uri(client.project_path(), "TS.lean");

        // Spawn a server task that:
        // 1. Sends fileProgress with empty processing (elaboration done)
        // 2. Responds to waitForDiagnostics
        // 3. Sends publishDiagnostics
        let server = tokio::spawn(async move {
            // Wait for the waitForDiagnostics request.
            let req = read_message(&mut sr).await.unwrap();
            assert_eq!(req["method"], "textDocument/waitForDiagnostics");
            let id = req["id"].as_i64().unwrap();

            // Send fileProgress with empty processing (done).
            let fp_notif = json!({
                "jsonrpc": "2.0",
                "method": "$/lean/fileProgress",
                "params": {
                    "textDocument": {"uri": uri.clone()},
                    "processing": []
                }
            });
            write_message(&mut sw, &fp_notif).await.unwrap();

            // Send diagnostics.
            let diag_notif = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": uri.clone(),
                    "diagnostics": [{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"message":"err","severity":1}]
                }
            });
            write_message(&mut sw, &diag_notif).await.unwrap();

            // Respond to waitForDiagnostics.
            write_message(&mut sw, &json!({"jsonrpc":"2.0","id":id,"result":null}))
                .await
                .unwrap();
        });

        let start = Instant::now();
        let result = client
            .get_diagnostics("TS.lean", None, None, Some(5.0))
            .await
            .unwrap();
        let elapsed = start.elapsed();

        // Should complete much faster than the 5s inactivity timeout.
        // The grace period is 500ms, so it should be around 500-700ms.
        assert!(
            elapsed < Duration::from_secs(3),
            "Triple-signal should complete in ~500ms, took {elapsed:?}"
        );

        let diags = result.as_array().unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0]["message"], "err");

        server.await.unwrap();
    }

    #[tokio::test]
    async fn get_diagnostics_fatal_error_returns_immediately() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "Fatal.lean", "x").await;

        let uri = path_to_uri(client.project_path(), "Fatal.lean");

        // Spawn server that sends fatal fileProgress then a diagnostic.
        let server = tokio::spawn(async move {
            // Read the waitForDiagnostics request but don't respond immediately.
            let _req = read_message(&mut sr).await.unwrap();

            // Send fileProgress with kind == 2 (fatal).
            let fp_notif = json!({
                "jsonrpc": "2.0",
                "method": "$/lean/fileProgress",
                "params": {
                    "textDocument": {"uri": uri.clone()},
                    "processing": [{
                        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 0}},
                        "kind": 2
                    }]
                }
            });
            write_message(&mut sw, &fp_notif).await.unwrap();

            // Send a diagnostic too.
            let diag_notif = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": uri,
                    "diagnostics": [{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"message":"fatal error","severity":1}]
                }
            });
            write_message(&mut sw, &diag_notif).await.unwrap();
        });

        let start = Instant::now();
        let result = client
            .get_diagnostics("Fatal.lean", None, None, Some(5.0))
            .await
            .unwrap();
        let elapsed = start.elapsed();

        // Fatal error should cause immediate return (no grace period).
        assert!(
            elapsed < Duration::from_secs(2),
            "Fatal error should return quickly, took {elapsed:?}"
        );

        let diags = result.as_array().unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0]["message"], "fatal error");

        server.await.unwrap();
    }

    #[tokio::test]
    async fn get_diagnostics_inactivity_fallback() {
        // Tests that the inactivity timeout works as fallback when no
        // fileProgress or waitForDiagnostics signals arrive.
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "Inact.lean", "x").await;

        let uri = path_to_uri(client.project_path(), "Inact.lean");

        // Send diagnostics but no fileProgress.
        let diag_notif = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "diagnostics": [{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"message":"timeout err","severity":1}]
            }
        });
        write_message(&mut sw, &diag_notif).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let start = Instant::now();
        let result = client
            .get_diagnostics("Inact.lean", None, None, Some(0.15))
            .await
            .unwrap();
        let elapsed = start.elapsed();

        // Should fire after the 150ms inactivity timeout.
        assert!(
            elapsed < Duration::from_secs(2),
            "Inactivity fallback should fire quickly, took {elapsed:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(100),
            "Should have waited at least ~150ms, took {elapsed:?}"
        );

        let diags = result.as_array().unwrap();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0]["message"], "timeout err");
    }

    #[tokio::test]
    async fn elaboration_state_resets_on_open_file() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "Reset.lean", "x").await;

        let uri = path_to_uri(client.project_path(), "Reset.lean");

        // Send fileProgress to set some state.
        let fp = json!({
            "jsonrpc": "2.0",
            "method": "$/lean/fileProgress",
            "params": {
                "textDocument": {"uri": uri.clone()},
                "processing": []
            }
        });
        write_message(&mut sw, &fp).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        {
            let states = client.elaboration_states.lock().unwrap();
            let state = states.get(&uri).unwrap();
            assert!(!state.processing);
        }

        // Re-open the file with changed content — should reset state.
        std::fs::write(dir.path().join("Reset.lean"), "y").unwrap();
        client.open_file("Reset.lean").await.unwrap();
        let _ = read_message(&mut sr).await.unwrap(); // drain didChange

        {
            let states = client.elaboration_states.lock().unwrap();
            let state = states.get(&uri).unwrap();
            // After reset, processing should be true (default).
            assert!(state.processing);
            assert!(!state.wait_for_diag_done);
            assert!(!state.fatal_error);
        }
    }

    #[tokio::test]
    async fn elaboration_state_resets_on_update_file_content() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "ResetU.lean", "x").await;

        let uri = path_to_uri(client.project_path(), "ResetU.lean");

        // Set some elaboration state.
        let fp = json!({
            "jsonrpc": "2.0",
            "method": "$/lean/fileProgress",
            "params": {
                "textDocument": {"uri": uri.clone()},
                "processing": []
            }
        });
        write_message(&mut sw, &fp).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        {
            let states = client.elaboration_states.lock().unwrap();
            assert!(!states.get(&uri).unwrap().processing);
        }

        // Update file content — should reset state.
        client
            .update_file_content("ResetU.lean", "new content")
            .await
            .unwrap();
        let _ = read_message(&mut sr).await.unwrap(); // drain didChange

        {
            let states = client.elaboration_states.lock().unwrap();
            let state = states.get(&uri).unwrap();
            assert!(state.processing);
            assert!(!state.wait_for_diag_done);
        }
    }

    #[tokio::test]
    async fn publish_diagnostics_updates_last_activity() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "LA.lean", "x").await;

        let uri = path_to_uri(client.project_path(), "LA.lean");

        // Record the current time.
        let before = Instant::now();

        // Send publishDiagnostics.
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri.clone(),
                "diagnostics": []
            }
        });
        write_message(&mut sw, &notif).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let states = client.elaboration_states.lock().unwrap();
        let state = states.get(&uri).unwrap();
        // last_activity should be after our `before` timestamp.
        assert!(state.last_activity >= before);
    }

    #[tokio::test]
    async fn get_diagnostics_line_range_early_return() {
        let (client, mut sr, mut sw, dir) = setup().await;
        open_test_file(&client, &mut sr, &dir, "LR.lean", "a\nb\nc\nd\ne").await;

        let uri = path_to_uri(client.project_path(), "LR.lean");

        // Spawn server that:
        // 1. Receives waitForDiagnostics but doesn't respond
        // 2. Sends fileProgress showing lines 3-4 still processing (but 0-2 done)
        // 3. Sends diagnostics for all lines
        let server = tokio::spawn(async move {
            // Read waitForDiagnostics, hold it.
            let _req = read_message(&mut sr).await.unwrap();

            // Send fileProgress: only lines 3-4 still processing.
            let fp = json!({
                "jsonrpc": "2.0",
                "method": "$/lean/fileProgress",
                "params": {
                    "textDocument": {"uri": uri.clone()},
                    "processing": [{
                        "range": {"start": {"line": 3, "character": 0}, "end": {"line": 4, "character": 0}},
                        "kind": 1
                    }]
                }
            });
            write_message(&mut sw, &fp).await.unwrap();

            // Send diagnostics for lines 0-1.
            let diag = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/publishDiagnostics",
                "params": {
                    "uri": uri,
                    "diagnostics": [
                        {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"message":"err0"},
                        {"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":1}},"message":"err1"}
                    ]
                }
            });
            write_message(&mut sw, &diag).await.unwrap();
        });

        let start = Instant::now();
        // Request diagnostics for lines 0-1 only — should return early since
        // that range is not being processed.
        let result = client
            .get_diagnostics("LR.lean", Some(0), Some(1), Some(5.0))
            .await
            .unwrap();
        let elapsed = start.elapsed();

        // Should complete much faster than the 5s timeout (grace period ~500ms).
        assert!(
            elapsed < Duration::from_secs(3),
            "Line range should complete early, took {elapsed:?}"
        );

        let diags = result.as_array().unwrap();
        assert_eq!(diags.len(), 2);

        server.await.unwrap();
    }
}
