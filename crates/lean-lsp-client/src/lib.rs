//! Async Lean 4 Language Server Protocol client.
//!
//! Provides a custom LSP client tailored to Lean's protocol extensions
//! including proof goals, widgets, and interactive diagnostics.

pub mod client;
pub mod error;
pub mod jsonrpc;
pub mod lean_client;
pub mod multiplexer;
pub mod transport;
pub mod types;
