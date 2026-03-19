//! Transport error types for the LSP client.

use thiserror::Error;

/// Errors that can occur during LSP transport operations.
#[derive(Debug, Error)]
pub enum TransportError {
    /// An I/O error occurred during communication.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON serialization/deserialization error occurred.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    /// The Content-Length header was missing or malformed.
    #[error("Invalid Content-Length header: {0}")]
    InvalidHeader(String),

    /// The subprocess stdin handle was closed or unavailable.
    #[error("stdin closed")]
    StdinClosed,

    /// The subprocess stdout handle was closed or unavailable.
    #[error("stdout closed")]
    StdoutClosed,

    /// The transport has been closed and can no longer be used.
    #[error("Transport closed")]
    Closed,

    /// The LSP server process exited unexpectedly.
    #[error("LSP server process exited with code {0:?}")]
    ProcessExited(Option<i32>),
}
