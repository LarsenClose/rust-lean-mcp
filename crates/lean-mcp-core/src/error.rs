//! Error types for the Lean MCP core crate.
//!
//! Three top-level error enums cover distinct failure domains:
//! - [`LeanToolError`] — user-facing tool errors (invalid paths, LSP failures, etc.)
//! - [`SearchError`] — local project search errors
//! - [`ConfigError`] — configuration parsing/validation errors

use thiserror::Error;

/// User-facing errors returned by MCP tool handlers.
#[derive(Debug, Error)]
pub enum LeanToolError {
    /// The given path does not belong to any Lean project.
    #[error("Invalid Lean file path: '{0}' not found in any Lean project (no lean-toolchain ancestor or file does not exist)")]
    InvalidPath(String),

    /// An LSP request timed out.
    #[error("LSP timeout during {0}")]
    LspTimeout(String),

    /// An LSP request returned an error.
    #[error("LSP error during {operation}: {message}")]
    LspError {
        /// The LSP operation that failed.
        operation: String,
        /// The error message from the LSP server.
        message: String,
    },

    /// No hover information at the given position.
    #[error("No hover information at line {line}, column {column}")]
    NoHoverInfo {
        /// 1-indexed line number.
        line: u32,
        /// 1-indexed column number.
        column: u32,
    },

    /// The requested symbol was not found in the file.
    #[error("Symbol `{0}` (case sensitive) not found in file. Add it first.")]
    SymbolNotFound(String),

    /// No go-to-declaration target for the given symbol.
    #[error("No declaration available for `{0}`.")]
    NoDeclaration(String),

    /// The specified line exceeds the file length.
    #[error("Line {line} out of range (file has {total} lines)")]
    LineOutOfRange {
        /// The requested line number.
        line: u32,
        /// Total number of lines in the file.
        total: usize,
    },

    /// The specified column exceeds the line length.
    #[error("Column {column} out of range (line has {length} characters)")]
    ColumnOutOfRange {
        /// The requested column number.
        column: u32,
        /// Length of the line in characters.
        length: usize,
    },

    /// No proof goals at the given position.
    #[error("No goals at line {line}, column {column}")]
    NoGoals {
        /// 1-indexed line number.
        line: u32,
        /// 1-indexed column number.
        column: u32,
    },

    /// A named declaration was not found in the file.
    #[error("Declaration '{0}' not found in file.")]
    DeclarationNotFound(String),

    /// The project path could not be determined.
    #[error("Project path unknown")]
    ProjectPathUnknown,

    /// No project path was configured.
    #[error("No project path")]
    NoProjectPath,

    /// The LSP client failed to start.
    #[error("Client start failed for '{path}': {reason}")]
    ClientStartFailed {
        /// The project path that failed.
        path: String,
        /// Why the client could not start.
        reason: String,
    },

    /// An axiom check did not pass.
    #[error("Axiom check failed: {0}")]
    AxiomCheckFailed(String),

    /// The caller exceeded the rate limit.
    #[error("Rate limit exceeded: max {max_requests} requests per {per_seconds}s")]
    RateLimitExceeded {
        /// Maximum allowed requests in the window.
        max_requests: u32,
        /// Window duration in seconds.
        per_seconds: u32,
    },

    /// Catch-all for other errors.
    #[error("{0}")]
    Other(String),
}

/// Errors arising from local project search operations.
#[derive(Debug, Error)]
pub enum SearchError {
    /// No project path has been configured for search.
    #[error("Project path not set")]
    ProjectPathNotSet,

    /// The configured project root is invalid.
    #[error("Invalid project root '{path}': {reason}")]
    InvalidProjectRoot {
        /// The path that was checked.
        path: String,
        /// Why it is invalid.
        reason: String,
    },

    /// Could not locate the project root.
    #[error("Project root not found: {0}")]
    ProjectRootNotFound(String),

    /// The search operation itself failed.
    #[error("Search failed: {0}")]
    SearchFailed(String),

    /// The `rg` (ripgrep) binary was not found.
    #[error("ripgrep not found: {0}")]
    RipgrepNotFound(String),
}

/// Errors arising from configuration parsing or validation.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// A configuration value is invalid.
    #[error("Invalid config value for '{name}': '{value}' — {reason}")]
    InvalidValue {
        /// Name of the configuration key.
        name: String,
        /// The invalid value that was provided.
        value: String,
        /// Why the value is invalid.
        reason: String,
    },

    /// A JSON configuration string could not be parsed.
    #[error("Failed to parse JSON config '{name}': {reason}")]
    JsonParseError {
        /// Name of the configuration key.
        name: String,
        /// The parse error description.
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- LeanToolError Display tests ----

    #[test]
    fn display_invalid_path() {
        let err = LeanToolError::InvalidPath("/tmp/bad.lean".into());
        assert_eq!(
            err.to_string(),
            "Invalid Lean file path: '/tmp/bad.lean' not found in any Lean project \
             (no lean-toolchain ancestor or file does not exist)"
        );
    }

    #[test]
    fn display_lsp_timeout() {
        let err = LeanToolError::LspTimeout("hover".into());
        assert_eq!(err.to_string(), "LSP timeout during hover");
    }

    #[test]
    fn display_lsp_error() {
        let err = LeanToolError::LspError {
            operation: "completion".into(),
            message: "server crashed".into(),
        };
        assert_eq!(
            err.to_string(),
            "LSP error during completion: server crashed"
        );
    }

    #[test]
    fn display_no_hover_info() {
        let err = LeanToolError::NoHoverInfo {
            line: 10,
            column: 5,
        };
        assert_eq!(err.to_string(), "No hover information at line 10, column 5");
    }

    #[test]
    fn display_symbol_not_found() {
        let err = LeanToolError::SymbolNotFound("Nat.add_comm".into());
        assert_eq!(
            err.to_string(),
            "Symbol `Nat.add_comm` (case sensitive) not found in file. Add it first."
        );
    }

    #[test]
    fn display_no_declaration() {
        let err = LeanToolError::NoDeclaration("foo".into());
        assert_eq!(err.to_string(), "No declaration available for `foo`.");
    }

    #[test]
    fn display_line_out_of_range() {
        let err = LeanToolError::LineOutOfRange {
            line: 999,
            total: 42,
        };
        assert_eq!(err.to_string(), "Line 999 out of range (file has 42 lines)");
    }

    #[test]
    fn display_column_out_of_range() {
        let err = LeanToolError::ColumnOutOfRange {
            column: 80,
            length: 40,
        };
        assert_eq!(
            err.to_string(),
            "Column 80 out of range (line has 40 characters)"
        );
    }

    #[test]
    fn display_no_goals() {
        let err = LeanToolError::NoGoals { line: 3, column: 1 };
        assert_eq!(err.to_string(), "No goals at line 3, column 1");
    }

    #[test]
    fn display_declaration_not_found() {
        let err = LeanToolError::DeclarationNotFound("myTheorem".into());
        assert_eq!(
            err.to_string(),
            "Declaration 'myTheorem' not found in file."
        );
    }

    #[test]
    fn display_project_path_unknown() {
        let err = LeanToolError::ProjectPathUnknown;
        assert_eq!(err.to_string(), "Project path unknown");
    }

    #[test]
    fn display_no_project_path() {
        let err = LeanToolError::NoProjectPath;
        assert_eq!(err.to_string(), "No project path");
    }

    #[test]
    fn display_client_start_failed() {
        let err = LeanToolError::ClientStartFailed {
            path: "/home/user/proj".into(),
            reason: "binary not found".into(),
        };
        assert_eq!(
            err.to_string(),
            "Client start failed for '/home/user/proj': binary not found"
        );
    }

    #[test]
    fn display_axiom_check_failed() {
        let err = LeanToolError::AxiomCheckFailed("uses sorry".into());
        assert_eq!(err.to_string(), "Axiom check failed: uses sorry");
    }

    #[test]
    fn display_rate_limit_exceeded() {
        let err = LeanToolError::RateLimitExceeded {
            max_requests: 3,
            per_seconds: 30,
        };
        assert_eq!(
            err.to_string(),
            "Rate limit exceeded: max 3 requests per 30s"
        );
    }

    #[test]
    fn display_other() {
        let err = LeanToolError::Other("something went wrong".into());
        assert_eq!(err.to_string(), "something went wrong");
    }

    // ---- SearchError Display tests ----

    #[test]
    fn display_project_path_not_set() {
        let err = SearchError::ProjectPathNotSet;
        assert_eq!(err.to_string(), "Project path not set");
    }

    #[test]
    fn display_invalid_project_root() {
        let err = SearchError::InvalidProjectRoot {
            path: "/nonexistent".into(),
            reason: "directory does not exist".into(),
        };
        assert_eq!(
            err.to_string(),
            "Invalid project root '/nonexistent': directory does not exist"
        );
    }

    #[test]
    fn display_project_root_not_found() {
        let err = SearchError::ProjectRootNotFound("no lakefile".into());
        assert_eq!(err.to_string(), "Project root not found: no lakefile");
    }

    #[test]
    fn display_search_failed() {
        let err = SearchError::SearchFailed("regex invalid".into());
        assert_eq!(err.to_string(), "Search failed: regex invalid");
    }

    #[test]
    fn display_ripgrep_not_found() {
        let err = SearchError::RipgrepNotFound("not in PATH".into());
        assert_eq!(err.to_string(), "ripgrep not found: not in PATH");
    }

    // ---- ConfigError Display tests ----

    #[test]
    fn display_invalid_value() {
        let err = ConfigError::InvalidValue {
            name: "timeout".into(),
            value: "-1".into(),
            reason: "must be non-negative".into(),
        };
        assert_eq!(
            err.to_string(),
            "Invalid config value for 'timeout': '-1' \u{2014} must be non-negative"
        );
    }

    #[test]
    fn display_json_parse_error() {
        let err = ConfigError::JsonParseError {
            name: "settings".into(),
            reason: "unexpected EOF".into(),
        };
        assert_eq!(
            err.to_string(),
            "Failed to parse JSON config 'settings': unexpected EOF"
        );
    }

    // ---- Static trait assertions ----

    #[test]
    fn lean_tool_error_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LeanToolError>();
    }

    #[test]
    fn search_error_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SearchError>();
    }

    #[test]
    fn config_error_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ConfigError>();
    }
}
