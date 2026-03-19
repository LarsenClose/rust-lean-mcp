//! Core business logic for the Lean LSP MCP server.
//!
//! Contains domain models, rate limiting, build coordination, search utilities,
//! file path resolution, and all logic independent of the MCP and LSP protocols.

pub mod error;
pub mod models;
pub mod rate_limit;
