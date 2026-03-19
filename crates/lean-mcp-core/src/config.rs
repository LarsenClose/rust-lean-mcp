//! Configuration and CLI argument parsing for the Lean MCP server.
//!
//! Provides [`CliArgs`] (clap derive) for command-line parsing and [`Config`] for
//! the fully resolved runtime configuration. Environment variables act as fallbacks
//! when CLI flags are not provided; CLI flags always take precedence.

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;

use clap::Parser;
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Transport mode for the MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    /// Standard I/O (stdin/stdout).
    Stdio,
    /// Streamable HTTP transport.
    StreamableHttp,
    /// Server-Sent Events transport.
    Sse,
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Transport::Stdio => write!(f, "stdio"),
            Transport::StreamableHttp => write!(f, "streamable-http"),
            Transport::Sse => write!(f, "sse"),
        }
    }
}

impl FromStr for Transport {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "stdio" => Ok(Transport::Stdio),
            "streamable-http" => Ok(Transport::StreamableHttp),
            "sse" => Ok(Transport::Sse),
            other => Err(format!(
                "invalid transport '{other}': expected stdio, streamable-http, or sse"
            )),
        }
    }
}

/// Build concurrency mode controlling how concurrent build requests are handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildConcurrencyMode {
    /// Allow concurrent builds.
    Allow,
    /// Cancel the previous build when a new one starts.
    Cancel,
    /// Share the result of the in-progress build.
    Share,
}

impl std::fmt::Display for BuildConcurrencyMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildConcurrencyMode::Allow => write!(f, "allow"),
            BuildConcurrencyMode::Cancel => write!(f, "cancel"),
            BuildConcurrencyMode::Share => write!(f, "share"),
        }
    }
}

impl FromStr for BuildConcurrencyMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "allow" => Ok(BuildConcurrencyMode::Allow),
            "cancel" => Ok(BuildConcurrencyMode::Cancel),
            "share" => Ok(BuildConcurrencyMode::Share),
            other => Err(format!(
                "invalid build concurrency mode '{other}': expected allow, cancel, or share"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// CLI arguments (clap derive)
// ---------------------------------------------------------------------------

/// Command-line arguments for the Lean MCP server.
#[derive(Debug, Clone, Parser)]
#[command(name = "lean-mcp", about = "Lean 4 MCP server")]
pub struct CliArgs {
    /// Transport mode: stdio, streamable-http, or sse.
    #[arg(long, default_value = "stdio")]
    pub transport: String,

    /// Host address to bind (for HTTP transports).
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Port to bind (for HTTP transports).
    #[arg(long, default_value_t = 8000)]
    pub port: u16,

    /// Path to the Lean project.
    #[arg(long)]
    pub lean_project_path: Option<String>,

    /// Comma-separated list of tool names to disable.
    #[arg(long)]
    pub disable_tools: Option<String>,

    /// JSON object mapping tool names to custom descriptions.
    #[arg(long)]
    pub tool_descriptions: Option<String>,

    /// Override the server instructions string.
    #[arg(long)]
    pub instructions: Option<String>,

    /// Enable local Loogle instance.
    #[arg(long)]
    pub loogle_local: bool,

    /// Path to the Loogle cache directory.
    #[arg(long)]
    pub loogle_cache_dir: Option<String>,

    /// Enable the Lean REPL.
    #[arg(long)]
    pub repl: bool,

    /// REPL timeout in seconds.
    #[arg(long)]
    pub repl_timeout: Option<u64>,
}

// ---------------------------------------------------------------------------
// Resolved configuration
// ---------------------------------------------------------------------------

/// Fully resolved runtime configuration for the Lean MCP server.
///
/// Built by merging CLI arguments with environment variable fallbacks.
/// CLI flags always take precedence over environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    /// Transport mode.
    pub transport: Transport,
    /// Host address for HTTP transports.
    pub host: String,
    /// Port for HTTP transports.
    pub port: u16,
    /// Path to the Lean project root.
    pub lean_project_path: Option<PathBuf>,
    /// Set of disabled tool names.
    pub disabled_tools: Vec<String>,
    /// Custom tool descriptions (tool name -> description).
    pub tool_descriptions: HashMap<String, String>,
    /// Server instructions override.
    pub instructions: Option<String>,
    /// Whether local Loogle is enabled.
    pub loogle_local: bool,
    /// Path to the Loogle cache directory.
    pub loogle_cache_dir: Option<PathBuf>,
    /// Whether the Lean REPL is enabled.
    pub repl: bool,
    /// REPL timeout in seconds.
    pub repl_timeout: u64,
    /// Build concurrency mode (env-only: LEAN_BUILD_CONCURRENCY).
    pub build_concurrency: BuildConcurrencyMode,
    /// Log level string (env-only: LEAN_LOG_LEVEL).
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            transport: Transport::Stdio,
            host: "127.0.0.1".to_string(),
            port: 8000,
            lean_project_path: None,
            disabled_tools: Vec::new(),
            tool_descriptions: HashMap::new(),
            instructions: None,
            loogle_local: false,
            loogle_cache_dir: None,
            repl: false,
            repl_timeout: 60,
            build_concurrency: BuildConcurrencyMode::Allow,
            log_level: "INFO".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Env-var helpers
// ---------------------------------------------------------------------------

/// Read an environment variable, returning `None` for missing or empty values.
fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Parse disabled tools from a comma-separated string.
fn parse_disabled_tools(s: &str) -> Vec<String> {
    s.split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Parse a JSON string into a `HashMap<String, String>` for tool descriptions.
fn parse_tool_descriptions(s: &str) -> Result<HashMap<String, String>, ConfigError> {
    serde_json::from_str(s).map_err(|e| ConfigError::JsonParseError {
        name: "tool_descriptions".to_string(),
        reason: e.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Config construction
// ---------------------------------------------------------------------------

impl Config {
    /// Build a [`Config`] by merging CLI arguments with environment variable fallbacks.
    ///
    /// CLI flags take precedence. Environment variables are used only when the
    /// corresponding CLI flag was not provided.
    pub fn from_cli_and_env(cli: &CliArgs) -> Result<Self, ConfigError> {
        // Transport: CLI value (which has a default of "stdio"), but env can override
        // only if CLI was left at the default.
        let transport_str = if cli.transport != "stdio" {
            cli.transport.clone()
        } else {
            env_opt("LEAN_TRANSPORT").unwrap_or_else(|| cli.transport.clone())
        };
        let transport =
            Transport::from_str(&transport_str).map_err(|_| ConfigError::InvalidValue {
                name: "transport".to_string(),
                value: transport_str.clone(),
                reason: "expected stdio, streamable-http, or sse".to_string(),
            })?;

        // Host / port: CLI defaults are fine, no env override specified in Python.
        let host = cli.host.clone();
        let port = cli.port;

        // Lean project path: CLI > env
        let lean_project_path = cli
            .lean_project_path
            .clone()
            .or_else(|| env_opt("LEAN_PROJECT_PATH"))
            .map(PathBuf::from);

        // Disabled tools: CLI > env
        let disabled_tools = if let Some(ref dt) = cli.disable_tools {
            parse_disabled_tools(dt)
        } else {
            env_opt("LEAN_MCP_DISABLED_TOOLS")
                .map(|s| parse_disabled_tools(&s))
                .unwrap_or_default()
        };

        // Tool descriptions: CLI > env
        let tool_descriptions = if let Some(ref td) = cli.tool_descriptions {
            parse_tool_descriptions(td)?
        } else if let Some(td_env) = env_opt("LEAN_MCP_TOOL_DESCRIPTIONS") {
            parse_tool_descriptions(&td_env)?
        } else {
            HashMap::new()
        };

        // Instructions: CLI > env
        let instructions = cli
            .instructions
            .clone()
            .or_else(|| env_opt("LEAN_MCP_INSTRUCTIONS"));

        // Loogle local: CLI flag > env
        let loogle_local = if cli.loogle_local {
            true
        } else {
            env_opt("LEAN_LOOGLE_LOCAL")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        };

        // Loogle cache dir: CLI > env
        let loogle_cache_dir = cli
            .loogle_cache_dir
            .clone()
            .or_else(|| env_opt("LEAN_LOOGLE_CACHE_DIR"))
            .map(PathBuf::from);

        // REPL: CLI flag > env
        let repl = if cli.repl {
            true
        } else {
            env_opt("LEAN_REPL")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false)
        };

        // REPL timeout: CLI > env > default 60
        let repl_timeout = if let Some(t) = cli.repl_timeout {
            t
        } else if let Some(t_str) = env_opt("LEAN_REPL_TIMEOUT") {
            t_str
                .parse::<u64>()
                .map_err(|_| ConfigError::InvalidValue {
                    name: "LEAN_REPL_TIMEOUT".to_string(),
                    value: t_str,
                    reason: "must be a non-negative integer (seconds)".to_string(),
                })?
        } else {
            60
        };

        // Build concurrency: env only
        let build_concurrency = if let Some(bc_str) = env_opt("LEAN_BUILD_CONCURRENCY") {
            BuildConcurrencyMode::from_str(&bc_str).map_err(|_| ConfigError::InvalidValue {
                name: "LEAN_BUILD_CONCURRENCY".to_string(),
                value: bc_str,
                reason: "expected allow, cancel, or share".to_string(),
            })?
        } else {
            BuildConcurrencyMode::Allow
        };

        // Log level: env only
        let log_level = env_opt("LEAN_LOG_LEVEL").unwrap_or_else(|| "INFO".to_string());

        Ok(Config {
            transport,
            host,
            port,
            lean_project_path,
            disabled_tools,
            tool_descriptions,
            instructions,
            loogle_local,
            loogle_cache_dir,
            repl,
            repl_timeout,
            build_concurrency,
            log_level,
        })
    }

    /// Convenience: parse CLI arguments from the process argv and merge with env.
    pub fn from_args() -> Result<Self, ConfigError> {
        let cli = CliArgs::parse();
        Self::from_cli_and_env(&cli)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a `CliArgs` with all defaults (mirrors `--transport stdio`).
    fn default_cli() -> CliArgs {
        CliArgs {
            transport: "stdio".to_string(),
            host: "127.0.0.1".to_string(),
            port: 8000,
            lean_project_path: None,
            disable_tools: None,
            tool_descriptions: None,
            instructions: None,
            loogle_local: false,
            loogle_cache_dir: None,
            repl: false,
            repl_timeout: None,
        }
    }

    // Because env vars are process-global, tests that mutate them must run
    // sequentially. We use unique env var names where possible, but for the
    // real env vars we rely on `serial_test` not being available — instead
    // we clean up after ourselves in each test.

    /// Helper: temporarily set env vars, run a closure, then unset them.
    fn with_env_vars<F, R>(vars: &[(&str, &str)], f: F) -> R
    where
        F: FnOnce() -> R,
    {
        for (k, v) in vars {
            std::env::set_var(k, v);
        }
        let result = f();
        for (k, _) in vars {
            std::env::remove_var(k);
        }
        result
    }

    // ---- Defaults ----

    #[test]
    fn config_defaults() {
        // Ensure no relevant env vars leak in.
        let keys = [
            "LEAN_PROJECT_PATH",
            "LEAN_MCP_DISABLED_TOOLS",
            "LEAN_MCP_TOOL_DESCRIPTIONS",
            "LEAN_MCP_INSTRUCTIONS",
            "LEAN_LOOGLE_LOCAL",
            "LEAN_LOOGLE_CACHE_DIR",
            "LEAN_REPL",
            "LEAN_REPL_TIMEOUT",
            "LEAN_BUILD_CONCURRENCY",
            "LEAN_LOG_LEVEL",
            "LEAN_TRANSPORT",
        ];
        for k in &keys {
            std::env::remove_var(k);
        }

        let cfg = Config::from_cli_and_env(&default_cli()).unwrap();
        assert_eq!(cfg.transport, Transport::Stdio);
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 8000);
        assert!(cfg.lean_project_path.is_none());
        assert!(cfg.disabled_tools.is_empty());
        assert!(cfg.tool_descriptions.is_empty());
        assert!(cfg.instructions.is_none());
        assert!(!cfg.loogle_local);
        assert!(cfg.loogle_cache_dir.is_none());
        assert!(!cfg.repl);
        assert_eq!(cfg.repl_timeout, 60);
        assert_eq!(cfg.build_concurrency, BuildConcurrencyMode::Allow);
        assert_eq!(cfg.log_level, "INFO");
    }

    // ---- CLI overrides ----

    #[test]
    fn cli_transport_override() {
        let mut cli = default_cli();
        cli.transport = "sse".to_string();
        let cfg = Config::from_cli_and_env(&cli).unwrap();
        assert_eq!(cfg.transport, Transport::Sse);
    }

    #[test]
    fn cli_host_and_port_override() {
        let mut cli = default_cli();
        cli.host = "0.0.0.0".to_string();
        cli.port = 9090;
        let cfg = Config::from_cli_and_env(&cli).unwrap();
        assert_eq!(cfg.host, "0.0.0.0");
        assert_eq!(cfg.port, 9090);
    }

    #[test]
    fn cli_lean_project_path_override() {
        let mut cli = default_cli();
        cli.lean_project_path = Some("/home/user/lean-proj".to_string());
        let cfg = Config::from_cli_and_env(&cli).unwrap();
        assert_eq!(
            cfg.lean_project_path,
            Some(PathBuf::from("/home/user/lean-proj"))
        );
    }

    #[test]
    fn cli_disable_tools() {
        let mut cli = default_cli();
        cli.disable_tools = Some("lean_build, lean_run_code".to_string());
        let cfg = Config::from_cli_and_env(&cli).unwrap();
        assert_eq!(cfg.disabled_tools, vec!["lean_build", "lean_run_code"]);
    }

    #[test]
    fn cli_tool_descriptions_json() {
        let mut cli = default_cli();
        cli.tool_descriptions =
            Some(r#"{"lean_goal":"Get proof state","lean_hover_info":"Hover info"}"#.to_string());
        let cfg = Config::from_cli_and_env(&cli).unwrap();
        assert_eq!(cfg.tool_descriptions.len(), 2);
        assert_eq!(
            cfg.tool_descriptions.get("lean_goal").unwrap(),
            "Get proof state"
        );
    }

    #[test]
    fn cli_instructions_override() {
        let mut cli = default_cli();
        cli.instructions = Some("Custom instructions".to_string());
        let cfg = Config::from_cli_and_env(&cli).unwrap();
        assert_eq!(cfg.instructions.as_deref(), Some("Custom instructions"));
    }

    #[test]
    fn cli_repl_flags() {
        let mut cli = default_cli();
        cli.repl = true;
        cli.repl_timeout = Some(120);
        let cfg = Config::from_cli_and_env(&cli).unwrap();
        assert!(cfg.repl);
        assert_eq!(cfg.repl_timeout, 120);
    }

    #[test]
    fn cli_loogle_flags() {
        let mut cli = default_cli();
        cli.loogle_local = true;
        cli.loogle_cache_dir = Some("/tmp/loogle".to_string());
        let cfg = Config::from_cli_and_env(&cli).unwrap();
        assert!(cfg.loogle_local);
        assert_eq!(cfg.loogle_cache_dir, Some(PathBuf::from("/tmp/loogle")));
    }

    // ---- Env var fallbacks ----

    #[test]
    fn env_lean_project_path_fallback() {
        with_env_vars(&[("LEAN_PROJECT_PATH", "/env/lean")], || {
            let cfg = Config::from_cli_and_env(&default_cli()).unwrap();
            assert_eq!(cfg.lean_project_path, Some(PathBuf::from("/env/lean")));
        });
    }

    #[test]
    fn env_disabled_tools_fallback() {
        with_env_vars(
            &[("LEAN_MCP_DISABLED_TOOLS", "lean_build,lean_verify")],
            || {
                let cfg = Config::from_cli_and_env(&default_cli()).unwrap();
                assert_eq!(cfg.disabled_tools, vec!["lean_build", "lean_verify"]);
            },
        );
    }

    #[test]
    fn env_tool_descriptions_fallback() {
        with_env_vars(
            &[(
                "LEAN_MCP_TOOL_DESCRIPTIONS",
                r#"{"lean_goal":"Custom desc"}"#,
            )],
            || {
                let cfg = Config::from_cli_and_env(&default_cli()).unwrap();
                assert_eq!(
                    cfg.tool_descriptions.get("lean_goal").unwrap(),
                    "Custom desc"
                );
            },
        );
    }

    #[test]
    fn env_instructions_fallback() {
        with_env_vars(&[("LEAN_MCP_INSTRUCTIONS", "Env instructions")], || {
            let cfg = Config::from_cli_and_env(&default_cli()).unwrap();
            assert_eq!(cfg.instructions.as_deref(), Some("Env instructions"));
        });
    }

    #[test]
    fn env_loogle_local_true() {
        with_env_vars(&[("LEAN_LOOGLE_LOCAL", "true")], || {
            let cfg = Config::from_cli_and_env(&default_cli()).unwrap();
            assert!(cfg.loogle_local);
        });
    }

    #[test]
    fn env_loogle_local_one() {
        with_env_vars(&[("LEAN_LOOGLE_LOCAL", "1")], || {
            let cfg = Config::from_cli_and_env(&default_cli()).unwrap();
            assert!(cfg.loogle_local);
        });
    }

    #[test]
    fn env_repl_and_timeout() {
        with_env_vars(&[("LEAN_REPL", "1"), ("LEAN_REPL_TIMEOUT", "300")], || {
            let cfg = Config::from_cli_and_env(&default_cli()).unwrap();
            assert!(cfg.repl);
            assert_eq!(cfg.repl_timeout, 300);
        });
    }

    #[test]
    fn env_build_concurrency() {
        with_env_vars(&[("LEAN_BUILD_CONCURRENCY", "cancel")], || {
            let cfg = Config::from_cli_and_env(&default_cli()).unwrap();
            assert_eq!(cfg.build_concurrency, BuildConcurrencyMode::Cancel);
        });
    }

    #[test]
    fn env_log_level() {
        with_env_vars(&[("LEAN_LOG_LEVEL", "DEBUG")], || {
            let cfg = Config::from_cli_and_env(&default_cli()).unwrap();
            assert_eq!(cfg.log_level, "DEBUG");
        });
    }

    // ---- CLI takes precedence over env ----

    #[test]
    fn cli_overrides_env_project_path() {
        with_env_vars(&[("LEAN_PROJECT_PATH", "/env/path")], || {
            let mut cli = default_cli();
            cli.lean_project_path = Some("/cli/path".to_string());
            let cfg = Config::from_cli_and_env(&cli).unwrap();
            assert_eq!(cfg.lean_project_path, Some(PathBuf::from("/cli/path")));
        });
    }

    #[test]
    fn cli_overrides_env_disabled_tools() {
        with_env_vars(&[("LEAN_MCP_DISABLED_TOOLS", "env_tool")], || {
            let mut cli = default_cli();
            cli.disable_tools = Some("cli_tool".to_string());
            let cfg = Config::from_cli_and_env(&cli).unwrap();
            assert_eq!(cfg.disabled_tools, vec!["cli_tool"]);
        });
    }

    #[test]
    fn cli_overrides_env_instructions() {
        with_env_vars(&[("LEAN_MCP_INSTRUCTIONS", "env inst")], || {
            let mut cli = default_cli();
            cli.instructions = Some("cli inst".to_string());
            let cfg = Config::from_cli_and_env(&cli).unwrap();
            assert_eq!(cfg.instructions.as_deref(), Some("cli inst"));
        });
    }

    // ---- Invalid values ----

    #[test]
    fn invalid_transport_rejected() {
        let mut cli = default_cli();
        cli.transport = "websocket".to_string();
        let result = Config::from_cli_and_env(&cli);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("transport"));
    }

    #[test]
    fn invalid_tool_descriptions_json() {
        let mut cli = default_cli();
        cli.tool_descriptions = Some("not json".to_string());
        let result = Config::from_cli_and_env(&cli);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("tool_descriptions"));
    }

    #[test]
    fn invalid_repl_timeout_env() {
        with_env_vars(&[("LEAN_REPL_TIMEOUT", "not_a_number")], || {
            let result = Config::from_cli_and_env(&default_cli());
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert!(err.to_string().contains("LEAN_REPL_TIMEOUT"));
        });
    }

    #[test]
    fn invalid_build_concurrency_env() {
        with_env_vars(&[("LEAN_BUILD_CONCURRENCY", "invalid")], || {
            let result = Config::from_cli_and_env(&default_cli());
            assert!(result.is_err());
            let err = result.unwrap_err();
            assert!(err.to_string().contains("LEAN_BUILD_CONCURRENCY"));
        });
    }

    // ---- Enum Display / FromStr ----

    #[test]
    fn transport_display_and_parse() {
        for (variant, s) in [
            (Transport::Stdio, "stdio"),
            (Transport::StreamableHttp, "streamable-http"),
            (Transport::Sse, "sse"),
        ] {
            assert_eq!(variant.to_string(), s);
            assert_eq!(Transport::from_str(s).unwrap(), variant);
        }
        assert!(Transport::from_str("grpc").is_err());
    }

    #[test]
    fn build_concurrency_display_and_parse() {
        for (variant, s) in [
            (BuildConcurrencyMode::Allow, "allow"),
            (BuildConcurrencyMode::Cancel, "cancel"),
            (BuildConcurrencyMode::Share, "share"),
        ] {
            assert_eq!(variant.to_string(), s);
            assert_eq!(BuildConcurrencyMode::from_str(s).unwrap(), variant);
        }
        assert!(BuildConcurrencyMode::from_str("queue").is_err());
    }

    // ---- Edge cases ----

    #[test]
    fn empty_disable_tools_string_yields_empty_vec() {
        let mut cli = default_cli();
        cli.disable_tools = Some("".to_string());
        let cfg = Config::from_cli_and_env(&cli).unwrap();
        assert!(cfg.disabled_tools.is_empty());
    }

    #[test]
    fn config_default_trait() {
        let cfg = Config::default();
        assert_eq!(cfg.transport, Transport::Stdio);
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 8000);
        assert_eq!(cfg.repl_timeout, 60);
        assert_eq!(cfg.build_concurrency, BuildConcurrencyMode::Allow);
        assert_eq!(cfg.log_level, "INFO");
    }

    #[test]
    fn loogle_cache_dir_from_env() {
        with_env_vars(&[("LEAN_LOOGLE_CACHE_DIR", "/tmp/cache")], || {
            let cfg = Config::from_cli_and_env(&default_cli()).unwrap();
            assert_eq!(cfg.loogle_cache_dir, Some(PathBuf::from("/tmp/cache")));
        });
    }

    // ---- Send + Sync ----

    #[test]
    fn config_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Config>();
        assert_send_sync::<Transport>();
        assert_send_sync::<BuildConcurrencyMode>();
        assert_send_sync::<CliArgs>();
    }
}
