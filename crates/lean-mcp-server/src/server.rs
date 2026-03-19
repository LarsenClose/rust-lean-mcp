//! MCP server setup and tool routing.
//!
//! Defines [`AppContext`] for shared server state and implements the rmcp
//! [`ServerHandler`] trait so the server can respond to MCP initialize requests
//! and advertise its capabilities.

use std::path::PathBuf;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{Implementation, InitializeResult, ProtocolVersion, ServerCapabilities};

use lean_mcp_core::instructions::INSTRUCTIONS;

// ---------------------------------------------------------------------------
// AppContext
// ---------------------------------------------------------------------------

/// Shared application state for the MCP server.
///
/// Holds configuration that tools need at runtime, such as the path to the
/// Lean project. Additional fields (LSP client handle, rate limiter, build
/// coordinator) will be added in follow-up issues.
#[derive(Debug, Clone)]
pub struct AppContext {
    /// Path to the Lean project root, if configured.
    pub lean_project_path: Option<PathBuf>,
}

impl AppContext {
    /// Create an [`AppContext`] with no Lean project path set.
    pub fn new() -> Self {
        Self {
            lean_project_path: None,
        }
    }
}

impl Default for AppContext {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Server metadata helpers
// ---------------------------------------------------------------------------

/// The server name advertised to MCP clients.
pub fn server_name() -> &'static str {
    "Lean LSP"
}

/// The server version, pulled from this crate's Cargo.toml at compile time.
pub fn server_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The instructions string sent to MCP clients.
pub fn server_instructions() -> &'static str {
    INSTRUCTIONS
}

// ---------------------------------------------------------------------------
// ServerHandler implementation
// ---------------------------------------------------------------------------

impl ServerHandler for AppContext {
    fn get_info(&self) -> InitializeResult {
        InitializeResult {
            protocol_version: ProtocolVersion::default(),
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
            server_info: Implementation {
                name: server_name().to_owned(),
                version: server_version().to_owned(),
            },
            instructions: Some(server_instructions().to_owned()),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_context_new_has_no_project_path() {
        let ctx = AppContext::new();
        assert!(ctx.lean_project_path.is_none());
    }

    #[test]
    fn app_context_default_matches_new() {
        let ctx = AppContext::default();
        assert!(ctx.lean_project_path.is_none());
    }

    #[test]
    fn app_context_with_project_path() {
        let ctx = AppContext {
            lean_project_path: Some(PathBuf::from("/tmp/lean-project")),
        };
        assert_eq!(
            ctx.lean_project_path.as_deref(),
            Some(std::path::Path::new("/tmp/lean-project"))
        );
    }

    #[test]
    fn server_name_returns_lean_lsp() {
        assert_eq!(server_name(), "Lean LSP");
    }

    #[test]
    fn server_version_is_not_empty() {
        let version = server_version();
        assert!(!version.is_empty());
    }

    #[test]
    fn server_instructions_contains_key_sections() {
        let instructions = server_instructions();
        assert!(instructions.contains("## General Rules"));
        assert!(instructions.contains("## Key Tools"));
        assert!(instructions.contains("## Search Tools"));
        assert!(instructions.contains("## Search Decision Tree"));
        assert!(instructions.contains("## Return Formats"));
        assert!(instructions.contains("## Error Handling"));
    }

    #[test]
    fn get_info_returns_correct_server_metadata() {
        let ctx = AppContext::new();
        let info = ctx.get_info();
        assert_eq!(info.server_info.name, "Lean LSP");
        assert!(!info.server_info.version.is_empty());
        assert!(info.instructions.is_some());
        assert!(info.instructions.as_ref().unwrap().contains("## Key Tools"));
    }

    #[test]
    fn get_info_advertises_tools_capability() {
        let ctx = AppContext::new();
        let info = ctx.get_info();
        assert!(
            info.capabilities.tools.is_some(),
            "server should advertise tools capability"
        );
        assert_eq!(
            info.capabilities.tools.as_ref().unwrap().list_changed,
            Some(true),
            "tools capability should have list_changed = true"
        );
    }

    #[test]
    fn app_context_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AppContext>();
    }
}
