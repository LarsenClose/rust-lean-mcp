//! End-to-end tests for the MCP server binary.
//!
//! These tests spawn the `rust-lean-mcp` binary and communicate with it
//! over stdio using the MCP JSON-RPC protocol (newline-delimited JSON).

mod helpers;

mod errors;
mod protocol;
mod shutdown;
mod tools;
