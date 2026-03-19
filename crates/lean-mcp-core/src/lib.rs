//! Core business logic for the Lean LSP MCP server.
//!
//! Contains domain models, rate limiting, build coordination, search utilities,
//! file path resolution, and all logic independent of the MCP and LSP protocols.

pub mod build_coordinator;
pub mod config;
pub mod error;
pub mod file_utils;
pub mod goal_diff;
pub mod instructions;
pub mod loogle;
pub mod models;
pub mod rate_limit;
pub mod repl;
pub mod search_utils;
pub mod utils;
